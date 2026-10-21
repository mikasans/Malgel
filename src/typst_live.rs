//! The live preview a Typst source gets while it is edited, the way
//! tinymist previews one beside the editor.
//!
//! tinymist keeps one compiler world per document, edits its source in
//! place on every change, lets `comemo` reuse whatever the edit did not
//! touch, and ships the result to a webview as incremental vector data. A
//! GPUI view has no webview, so the pages are drawn here as raster sheets
//! (SVG → PNG on this thread) and the page viewer shows them; a page whose
//! frame did not change keeps its sheet (and the GPU texture made from it),
//! so typing on page 7 redraws page 7 only. Rasterizing off the UI thread
//! keeps title-bar buttons responsive while the preview updates.
//!
//! All of it runs on a thread of its own. The editor sends the text after a
//! short pause in typing; requests that queue up while a compile runs are
//! folded into the newest, so the preview never falls behind. A compile that
//! fails reports its diagnostics and keeps the last good pages. The same
//! thread answers where a click on a page comes from in the source, where
//! the caret's text lands on the pages, and where a go-to-definition lands
//! (typst-ide's `jump_from_click`, `jump_from_cursor`, and `definition`),
//! since it holds the compiled document.

use std::{
    collections::HashMap,
    num::NonZeroUsize,
    ops::Range,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use gpui_kit::{Image, ImageFormat};
use typst::{
    WorldExt as _,
    diag::Severity,
    introspection::PagedPosition,
    layout::{Abs, Point},
    syntax::{LinkedNode, Side, Tag},
    utils::hash128,
};

use typst_layout::PagedDocument;

use crate::typst_preview::DocumentWorld;

/// How often the compiler thread checks the files a document named.
const IMPORT_POLL: Duration = Duration::from_secs(1);

/// Device pixels per Typst point when rasterizing a page sheet. 2× keeps
/// pages sharp on HiDPI without handing huge SVGs to the UI paint path.
const SHEET_SCALE: f32 = 2.;

/// Longest edge of a live-preview page raster. Poster-sized pages are
/// downsampled so a single sheet cannot allocate multi‑hundred‑MiB
/// pixmaps on the compiler thread (same bound as Mermaid display).
const MAX_SHEET_EDGE: u32 = 2048;

/// Turn a Typst SVG page into a PNG `Image` on the compiler thread so GPUI
/// never runs resvg on the UI thread for live preview pages.
fn sheet_image(svg: String, width_pt: f32, height_pt: f32) -> Arc<Image> {
    match rasterize_svg_display(&svg, width_pt, height_pt) {
        Some((format, bytes)) => Arc::new(Image::from_bytes(format, bytes)),
        None => {
            log::warn!("typst live: SVG raster failed; falling back to SVG image");
            Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes()))
        }
    }
}

/// Rasterize a Typst SVG page for the live sheet cache. Prefers lossless WebP
/// so cached `Arc<Image>` payloads stay small (see `display_raster`).
fn rasterize_svg_display(
    svg: &str,
    width_pt: f32,
    height_pt: f32,
) -> Option<(ImageFormat, Vec<u8>)> {
    let options = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_str(svg, &options).ok()?;
    let mut width = ((width_pt * SHEET_SCALE).ceil() as u32).max(1);
    let mut height = ((height_pt * SHEET_SCALE).ceil() as u32).max(1);
    let longest = width.max(height);
    if longest > MAX_SHEET_EDGE {
        let scale = MAX_SHEET_EDGE as f32 / longest as f32;
        width = ((width as f32) * scale).round().max(1.) as u32;
        height = ((height as f32) * scale).round().max(1.) as u32;
    }
    let px_bytes = (width as u64) * (height as u64) * 4;
    let _span = crate::perf::Span::new("sheet_raster").detail(format!(
        "{width}x{height} pixmap≈{:.1}MiB",
        px_bytes as f64 / (1024.0 * 1024.0)
    ));
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)?;
    // Fit the SVG's own size into the pixmap. A plain SHEET_SCALE assumept=px
    // clip when typst_svg's user space does not match page points 1:1.
    let sx = width as f32 / tree.size().width().max(1.);
    let sy = height as f32 / tree.size().height().max(1.);
    let transform = resvg::tiny_skia::Transform::from_scale(sx, sy);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let encoded = crate::display_raster::encode_rgba(width, height, pixmap.data());
    drop(_span);
    encoded
}

/// A drawn page: its size in points and its sheet.
#[derive(Clone)]
pub struct LivePage {
    pub width: f32,
    pub height: f32,
    pub image: Arc<Image>,
}

/// A message from the compiler about the source.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveDiagnostic {
    /// Byte range in the source, when the message is about the source
    /// itself rather than a file it named.
    pub range: Option<Range<usize>>,
    pub message: String,
    pub error: bool,
}

/// One compile's outcome.
pub struct Compiled {
    /// The editor revision the text came from.
    pub revision: usize,
    /// The new pages; `None` when the compile failed and the last good pages
    /// stay.
    pub pages: Option<Vec<LivePage>>,
    pub diagnostics: Vec<LiveDiagnostic>,
    /// How long compiling and drawing took.
    pub elapsed: Duration,
    /// How many pages had to be drawn again (the rest were reused).
    pub redrawn: usize,
    /// Syntax highlighting for the text compiled, from the compiler's own
    /// parse: (byte range, tag), outer nodes before the nodes inside them.
    pub highlights: Vec<(Range<usize>, Tag)>,
    /// More sheets are still being rasterized; UI may paint what is here now.
    pub partial: bool,
}

/// The highlighting typst's own syntax tree gives `source`, the same
/// classification tinymist's semantic tokens come from.
pub fn highlights(source: &typst::syntax::Source) -> Vec<(Range<usize>, Tag)> {
    fn walk(node: &LinkedNode, out: &mut Vec<(Range<usize>, Tag)>) {
        if let Some(tag) = typst::syntax::highlight(node)
            && tag != Tag::Error
        {
            out.push((node.range(), tag));
        }
        for child in node.children() {
            walk(&child, out);
        }
    }
    let mut out = Vec::new();
    walk(&LinkedNode::new(source.root()), &mut out);
    out
}

/// The compiler for one document, holding its world, its last good
/// document and the sheets drawn for it.
pub struct LiveCompiler {
    world: DocumentWorld,
    /// The files the last compile read besides the source, and their stamp,
    /// so a change to an import or image recompiles.
    read: Vec<PathBuf>,
    stamp: Option<crate::session::DiskStamp>,
    text: String,
    revision: usize,
    document: Option<PagedDocument>,
    sheets: HashMap<u128, Arc<Image>>,
    /// The sheet key of every page of the last good document, in page order,
    /// so a progressive paint can stand the previous sheet in for a page that
    /// is still being drawn instead of dropping the page.
    page_keys: Vec<u128>,
    /// Diagnostics from the last compile, reused when the source is unchanged.
    diagnostics: Vec<LiveDiagnostic>,
}

impl LiveCompiler {
    pub fn new(path: &Path) -> Self {
        Self {
            world: DocumentWorld::new(path, String::new()),
            read: Vec::new(),
            stamp: None,
            text: String::new(),
            revision: 0,
            document: None,
            sheets: HashMap::new(),
            page_keys: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    /// Forget everything a compile could be reused from: the last text, the
    /// drawn sheets, the bytes of named files and the compiler's memoized
    /// work. The next compile starts from scratch and draws every page again
    /// — what Force Recompile asks for when the preview looks wrong.
    pub fn invalidate(&mut self) {
        self.text.clear();
        self.sheets.clear();
        self.world.clear_caches();
        comemo::evict(0);
    }

    /// Compile `text`, drawing only pages that changed.
    pub fn compile(&mut self, text: &str, revision: usize) -> Compiled {
        let started = Instant::now();
        crate::perf::mark(&format!("typst_compile_begin rev={revision}"));
        // Same source as last success: reuse sheets — no recompile, no redraw.
        if text == self.text && self.document.is_some() && !self.sheets.is_empty() {
            self.revision = revision;
            let pages = self.document.as_ref().map(|document| {
                document
                    .pages()
                    .iter()
                    .map(|page| {
                        let key = hash128(page);
                        let image = self.sheets.get(&key).cloned().unwrap_or_else(|| {
                            let svg = typst_svg::svg(page, &typst_svg::SvgOptions::default());
                            let width = page.frame.width().to_pt() as f32;
                            let height = page.frame.height().to_pt() as f32;
                            sheet_image(svg, width, height)
                        });
                        LivePage {
                            width: page.frame.width().to_pt() as f32,
                            height: page.frame.height().to_pt() as f32,
                            image,
                        }
                    })
                    .collect()
            });
            crate::perf::mark(&format!(
                "typst_compile_reuse_sheets pages={} elapsed_ms={:.1}",
                pages.as_ref().map_or(0, Vec::len),
                started.elapsed().as_secs_f64() * 1000.0
            ));
            return Compiled {
                revision,
                pages,
                diagnostics: self.diagnostics.clone(),
                elapsed: started.elapsed(),
                redrawn: 0,
                highlights: highlights(self.world.main_source()),
                partial: false,
            };
        }
        if self.text != text {
            self.text = text.to_string();
        }
        self.revision = revision;
        self.world.set_text(text);
        let warned = {
            let _span = crate::perf::Span::new("typst_compile_core");
            typst::compile::<PagedDocument>(&self.world)
        };
        let mut diagnostics: Vec<LiveDiagnostic> = warned
            .warnings
            .iter()
            .map(|warning| self.diagnostic(warning))
            .collect();
        let mut redrawn = 0;
        let pages = match warned.output {
            Ok(document) => {
                let mut sheets = HashMap::with_capacity(document.pages().len());
                let pages = document
                    .pages()
                    .iter()
                    .map(|page| {
                        let key = hash128(page);
                        let image = match self.sheets.get(&key) {
                            Some(image) => image.clone(),
                            None => {
                                redrawn += 1;
                                let svg = typst_svg::svg(page, &typst_svg::SvgOptions::default());
                                let width = page.frame.width().to_pt() as f32;
                                let height = page.frame.height().to_pt() as f32;
                                sheet_image(svg, width, height)
                            }
                        };
                        sheets.insert(key, image.clone());
                        LivePage {
                            width: page.frame.width().to_pt() as f32,
                            height: page.frame.height().to_pt() as f32,
                            image,
                        }
                    })
                    .collect();
                // Only the sheets of the current pages are kept.
                self.page_keys = document.pages().iter().map(hash128).collect();
                self.sheets = sheets;
                self.document = Some(document);
                Some(pages)
            }
            Err(errors) => {
                let mut errors: Vec<LiveDiagnostic> =
                    errors.iter().map(|error| self.diagnostic(error)).collect();
                errors.append(&mut diagnostics);
                diagnostics = errors;
                None
            }
        };
        // Drop memoized results nothing has used for a while, as the typst
        // command does between compiles, so memory stays flat while typing.
        comemo::evict(10);
        self.read = self.world.read_files();
        self.stamp = Some(crate::typst_preview::stamp_of(&self.read));
        let elapsed = started.elapsed();
        crate::perf::mark(&format!(
            "typst_compile_done rev={revision} redrawn={redrawn} pages={} elapsed_ms={:.1}",
            pages.as_ref().map_or(0, Vec::len),
            elapsed.as_secs_f64() * 1000.0
        ));
        log::debug!(
            "typst live: compile {:.1} ms ({} page(s) redrawn, revision {revision})",
            elapsed.as_secs_f64() * 1000.,
            redrawn,
        );
        self.diagnostics = diagnostics.clone();
        Compiled {
            revision,
            pages,
            diagnostics,
            elapsed,
            redrawn,
            highlights: highlights(self.world.main_source()),
            partial: false,
        }
    }

    /// Compile with progressive sheet paint: first dirty page is rasterized and
    /// reported via `on_partial` before the remaining dirty pages, so first
    /// paint does not wait on the whole stack.
    ///
    /// `force` throws away every cache first (see [`Self::invalidate`]), so
    /// the pages are compiled and drawn anew even when the text is the same.
    pub fn compile_progressive(
        &mut self,
        text: &str,
        revision: usize,
        force: bool,
        mut on_partial: impl FnMut(Compiled),
    ) -> Compiled {
        let started = Instant::now();
        crate::perf::mark(&format!("typst_compile_begin rev={revision} force={force}"));
        // The sheets on screen now, by page: a progressive paint shows these
        // for pages not drawn yet, so the page count never dips mid-compile.
        let previous: Vec<Option<Arc<Image>>> = self
            .page_keys
            .iter()
            .map(|key| self.sheets.get(key).cloned())
            .collect();
        if force {
            self.invalidate();
        }
        if text == self.text && self.document.is_some() && !self.sheets.is_empty() {
            return self.compile(text, revision);
        }
        if self.text != text {
            self.text = text.to_string();
        }
        self.revision = revision;
        self.world.set_text(text);
        let warned = {
            let _span = crate::perf::Span::new("typst_compile_core");
            typst::compile::<PagedDocument>(&self.world)
        };
        let mut diagnostics: Vec<LiveDiagnostic> = warned
            .warnings
            .iter()
            .map(|warning| self.diagnostic(warning))
            .collect();
        let mut redrawn = 0usize;
        let pages = match warned.output {
            Ok(document) => {
                let mut keys = Vec::with_capacity(document.pages().len());
                let mut meta = Vec::with_capacity(document.pages().len());
                let mut dirty: Vec<usize> = Vec::new();
                for (ix, page) in document.pages().iter().enumerate() {
                    let key = hash128(page);
                    let width = page.frame.width().to_pt() as f32;
                    let height = page.frame.height().to_pt() as f32;
                    keys.push(key);
                    meta.push((width, height));
                    if !self.sheets.contains_key(&key) {
                        dirty.push(ix);
                    }
                }
                let mut sheets = HashMap::with_capacity(document.pages().len());
                for key in &keys {
                    if let Some(image) = self.sheets.get(key) {
                        sheets.insert(*key, image.clone());
                    }
                }
                let build_pages = |sheets: &HashMap<u128, Arc<Image>>| -> Vec<LivePage> {
                    keys.iter()
                        .zip(meta.iter())
                        .enumerate()
                        .filter_map(|(ix, (key, (w, h)))| {
                            sheets
                                .get(key)
                                .or_else(|| previous.get(ix).and_then(Option::as_ref))
                                .map(|image| LivePage {
                                    width: *w,
                                    height: *h,
                                    image: image.clone(),
                                })
                        })
                        .collect()
                };
                for (step, &ix) in dirty.iter().enumerate() {
                    let page = &document.pages()[ix];
                    let key = keys[ix];
                    let (width, height) = meta[ix];
                    let svg = typst_svg::svg(page, &typst_svg::SvgOptions::default());
                    redrawn += 1;
                    let image = sheet_image(svg, width, height);
                    sheets.insert(key, image);
                    if step == 0 && dirty.len() > 1 {
                        let ready = build_pages(&sheets);
                        crate::perf::mark(&format!(
                            "typst_compile_partial rev={revision} ready={} pending={}",
                            ready.len(),
                            dirty.len() - 1
                        ));
                        on_partial(Compiled {
                            revision,
                            pages: Some(ready),
                            diagnostics: diagnostics.clone(),
                            elapsed: started.elapsed(),
                            redrawn,
                            highlights: highlights(self.world.main_source()),
                            partial: true,
                        });
                    }
                }
                let pages: Vec<LivePage> = keys
                    .iter()
                    .zip(meta.iter())
                    .filter_map(|(key, (w, h))| {
                        sheets.get(key).map(|image| LivePage {
                            width: *w,
                            height: *h,
                            image: image.clone(),
                        })
                    })
                    .collect();
                debug_assert_eq!(pages.len(), document.pages().len());
                self.page_keys = keys;
                self.sheets = sheets;
                self.document = Some(document);
                Some(pages)
            }
            Err(errors) => {
                let mut errors: Vec<LiveDiagnostic> =
                    errors.iter().map(|error| self.diagnostic(error)).collect();
                errors.append(&mut diagnostics);
                diagnostics = errors;
                None
            }
        };
        comemo::evict(10);
        self.read = self.world.read_files();
        self.stamp = Some(crate::typst_preview::stamp_of(&self.read));
        let elapsed = started.elapsed();
        crate::perf::mark(&format!(
            "typst_compile_done rev={revision} redrawn={redrawn} pages={} elapsed_ms={:.1}",
            pages.as_ref().map_or(0, Vec::len),
            elapsed.as_secs_f64() * 1000.0
        ));
        log::debug!(
            "typst live: compile {:.1} ms ({} page(s) redrawn, revision {revision})",
            elapsed.as_secs_f64() * 1000.,
            redrawn,
        );
        self.diagnostics = diagnostics.clone();
        Compiled {
            revision,
            pages,
            diagnostics,
            elapsed,
            redrawn,
            highlights: highlights(self.world.main_source()),
            partial: false,
        }
    }

    fn diagnostic(&self, diagnostic: &typst::diag::SourceDiagnostic) -> LiveDiagnostic {
        let main = self.world.main_source().id();
        let range = (diagnostic.span.id() == Some(main))
            .then(|| self.world.range(diagnostic.span))
            .flatten();
        let mut message = diagnostic.message.to_string();
        for hint in &diagnostic.hints {
            message.push_str("\nhint: ");
            message.push_str(&hint.v);
        }
        LiveDiagnostic {
            range,
            message,
            error: diagnostic.severity == Severity::Error,
        }
    }

    /// Compile again when a file the document named changed on disk.
    pub fn recompile_if_imports_changed(&mut self) -> Option<Compiled> {
        if self.read.is_empty() {
            return None;
        }
        let stamp = crate::typst_preview::stamp_of(&self.read);
        if Some(stamp) == self.stamp {
            return None;
        }
        let text = std::mem::take(&mut self.text);
        let compiled = self.compile(&text, self.revision);
        self.text = text;
        Some(compiled)
    }

    /// Where in the source a click at `x`, `y` points on page `page`
    /// (0-based) comes from, as a byte offset.
    pub fn source_at(&self, page: usize, x: f32, y: f32) -> Option<usize> {
        let document = self.document.as_ref()?;
        let position = PagedPosition {
            page: NonZeroUsize::new(page + 1)?,
            point: Point::new(Abs::pt(x as f64), Abs::pt(y as f64)),
        };
        match typst_ide::jump_from_click(&self.world, document, &position)? {
            typst_ide::Jump::File(id, offset) if id == self.world.main_source().id() => {
                Some(offset)
            }
            _ => None,
        }
    }

    /// Where the text at byte `offset` in the source lands on the pages, as
    /// (page, x, y) in points.
    pub fn page_at(&self, offset: usize) -> Option<(usize, f32, f32)> {
        let document = self.document.as_ref()?;
        let source = self.world.main_source();
        let text = source.text();
        let mut at = offset.min(text.len());
        // A caret on markup or an empty line has no text of its own on the
        // pages: the nearest text before it stands in.
        let mut position = None;
        for _ in 0..256 {
            if let Some(found) = typst_ide::jump_from_cursor(document, source, at).first() {
                position = Some(*found);
                break;
            }
            match text[..at].char_indices().next_back() {
                Some((previous, _)) => at = previous,
                None => break,
            }
        }
        let position = position?;
        Some((
            position.page.get() - 1,
            position.point.x.to_pt() as f32,
            position.point.y.to_pt() as f32,
        ))
    }

    /// Where go-to-definition at byte `offset` lands in this source, when the
    /// definition is in the same file (labels, refs, lets, callees).
    pub fn definition_at(&self, offset: usize) -> Option<usize> {
        let source = self.world.main_source();
        let definition = typst_ide::definition(
            &self.world,
            self.document.as_ref(),
            source,
            offset,
            Side::After,
        )
        .or_else(|| {
            typst_ide::definition(
                &self.world,
                self.document.as_ref(),
                source,
                offset,
                Side::Before,
            )
        })?;
        match definition {
            typst_ide::Definition::Span(span) if span.id() == Some(source.id()) => {
                self.world.range(span).map(|range| range.start)
            }
            _ => None,
        }
    }
}

/// What the editor asks the compiler thread.
pub enum Request {
    /// Compile `text`, the editor's text at `revision`. `force` drops every
    /// cache first so the pages are compiled and drawn from scratch.
    Compile {
        text: String,
        revision: usize,
        force: bool,
    },
    SourceAt {
        page: usize,
        x: f32,
        y: f32,
    },
    PageAt {
        offset: usize,
    },
    DefinitionAt {
        offset: usize,
    },
}

/// What the compiler thread answers.
pub enum Response {
    Compiled(Compiled),
    SourceAt(usize),
    PageAt(usize, f32, f32),
    /// Go-to-definition landed at this byte offset in the main source.
    DefinitionAt(usize),
}

/// The requests of one batch worth answering, in the order they arrived:
/// only the newest of each kind, since anything older is already stale.
/// A forced compile anywhere in the batch makes the newest compile forced,
/// so an edit typed right after Force Recompile does not cancel the force.
fn fold_batch(batch: Vec<Request>) -> Vec<Request> {
    let newest = |kind: fn(&Request) -> bool| batch.iter().rposition(kind);
    let newest_compile = newest(|r| matches!(r, Request::Compile { .. }));
    let newest_source = newest(|r| matches!(r, Request::SourceAt { .. }));
    let newest_page = newest(|r| matches!(r, Request::PageAt { .. }));
    let newest_def = newest(|r| matches!(r, Request::DefinitionAt { .. }));
    let forced = batch
        .iter()
        .any(|r| matches!(r, Request::Compile { force: true, .. }));
    batch
        .into_iter()
        .enumerate()
        .filter_map(|(ix, request)| {
            let keep = Some(ix)
                == match request {
                    Request::Compile { .. } => newest_compile,
                    Request::SourceAt { .. } => newest_source,
                    Request::PageAt { .. } => newest_page,
                    Request::DefinitionAt { .. } => newest_def,
                };
            keep.then_some(match request {
                Request::Compile {
                    text,
                    revision,
                    force,
                } => Request::Compile {
                    text,
                    revision,
                    force: force || forced,
                },
                other => other,
            })
        })
        .collect()
}

/// The answer to a compile that panicked: no pages (the last good ones stay
/// on screen) and a message saying so.
fn panicked(revision: usize) -> Compiled {
    Compiled {
        revision,
        pages: None,
        diagnostics: vec![LiveDiagnostic {
            message: "Typst compile panicked — preview stopped. Fix the source and try again."
                .into(),
            range: None,
            error: true,
        }],
        elapsed: Duration::ZERO,
        redrawn: 0,
        highlights: Vec::new(),
        partial: false,
    }
}

/// Answer one request. `None` when there is nothing to say (a click that
/// lands on no source, for one).
fn answer(
    compiler: &mut LiveCompiler,
    request: Request,
    answers: &futures::channel::mpsc::UnboundedSender<Response>,
) -> Option<Response> {
    match request {
        Request::Compile {
            text,
            revision,
            force,
        } => {
            let partial_tx = answers.clone();
            let compiled = compiler.compile_progressive(&text, revision, force, move |partial| {
                let _ = partial_tx.unbounded_send(Response::Compiled(partial));
            });
            Some(Response::Compiled(compiled))
        }
        Request::SourceAt { page, x, y } => compiler.source_at(page, x, y).map(Response::SourceAt),
        Request::PageAt { offset } => compiler
            .page_at(offset)
            .map(|(page, x, y)| Response::PageAt(page, x, y)),
        Request::DefinitionAt { offset } => {
            compiler.definition_at(offset).map(Response::DefinitionAt)
        }
    }
}

/// The compiler thread for one document. Dropping it ends the thread.
pub struct LiveWorker {
    requests: mpsc::Sender<Request>,
}

impl LiveWorker {
    /// Start compiling `path`'s text on a thread of its own; answers arrive
    /// on the returned receiver.
    ///
    /// The thread outlives anything a request does to it: every call into
    /// Typst — a compile, the poll that recompiles when an import changed,
    /// a click or caret lookup — runs under `catch_unwind`, and a panic
    /// rebuilds the compiler instead of ending the thread. A dead thread
    /// would swallow every later edit without a word, leaving the preview
    /// frozen on whatever it showed last.
    pub fn spawn(path: PathBuf) -> (Self, futures::channel::mpsc::UnboundedReceiver<Response>) {
        let (requests, inbox) = mpsc::channel::<Request>();
        let (answers, outbox) = futures::channel::mpsc::unbounded();
        std::thread::Builder::new()
            .name("typst-live".into())
            // Typst's layout recurses deeply on nested content.
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                let mut compiler = LiveCompiler::new(&path);
                loop {
                    let first = match inbox.recv_timeout(IMPORT_POLL) {
                        Ok(request) => request,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let polled = panic::catch_unwind(AssertUnwindSafe(|| {
                                compiler.recompile_if_imports_changed()
                            }));
                            let compiled = match polled {
                                Ok(compiled) => compiled,
                                Err(_) => {
                                    log::error!("typst live: import recompile panicked");
                                    let revision = compiler.revision;
                                    compiler = LiveCompiler::new(&path);
                                    Some(panicked(revision))
                                }
                            };
                            if let Some(compiled) = compiled
                                && answers
                                    .unbounded_send(Response::Compiled(compiled))
                                    .is_err()
                            {
                                return;
                            }
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    };
                    // Fold a queue of edits into the newest one: requests
                    // that arrived while the last compile ran are all here,
                    // so the newest text is always the one compiled next.
                    let mut batch = vec![first];
                    batch.extend(inbox.try_iter());
                    for request in fold_batch(batch) {
                        let compile_revision = match &request {
                            Request::Compile { revision, .. } => Some(*revision),
                            _ => None,
                        };
                        let answered =
                            panic::catch_unwind(AssertUnwindSafe(|| {
                                answer(&mut compiler, request, &answers)
                            }));
                        let response = match answered {
                            Ok(response) => response,
                            Err(_) => {
                                log::error!(
                                    "typst live: request panicked (compile rev {compile_revision:?})"
                                );
                                // Rebuild the world — a panicking compile may
                                // leave memoization / sheets inconsistent.
                                compiler = LiveCompiler::new(&path);
                                compile_revision.map(|revision| Response::Compiled(panicked(revision)))
                            }
                        };
                        if let Some(response) = response
                            && answers.unbounded_send(response).is_err()
                        {
                            return;
                        }
                    }
                }
            })
            .expect("start the Typst compiler thread");
        (Self { requests }, outbox)
    }

    /// Hand the thread a request. `false` when the thread is gone.
    pub fn send(&self, request: Request) -> bool {
        self.requests.send(request).is_ok()
    }
}

/// Bookkeeping that keeps the preview on the editor's newest text.
///
/// Every edit bumps the editor's revision. A compile is sent for it after a
/// short pause; a compile answers with the revision it compiled. When the
/// answer is older than the editor and nothing newer is on its way — an edit
/// whose send was lost, cancelled by the budget gate flipping, or swallowed
/// any other way — the editor sends its text again, so the preview always
/// ends on the text in the editor rather than on a stale page.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LiveSync {
    /// The editor's newest revision: what the preview should show.
    wanted: usize,
    /// The newest revision handed to the compiler thread.
    sent: Option<usize>,
    /// The newest revision a finished (non-partial) compile answered for.
    done: Option<usize>,
    /// A Force Recompile waiting for its answer.
    forcing: Option<usize>,
}

impl LiveSync {
    /// The editor's text changed and is now at `revision`.
    pub fn edited(&mut self, revision: usize) {
        self.wanted = self.wanted.max(revision);
    }

    /// The text at `revision` went to the compiler thread.
    pub fn sent(&mut self, revision: usize, force: bool) {
        self.wanted = self.wanted.max(revision);
        self.sent = Some(self.sent.map_or(revision, |sent| sent.max(revision)));
        if force {
            self.forcing = Some(revision);
        }
    }

    /// A compile answered for `revision`. Returns whether the editor has to
    /// send its text again: it moved on since, and no newer compile is
    /// queued that would catch it up.
    pub fn compiled(&mut self, revision: usize, partial: bool) -> bool {
        if partial {
            return false;
        }
        self.done = Some(self.done.map_or(revision, |done| done.max(revision)));
        if self.forcing.is_some_and(|forcing| revision >= forcing) {
            self.forcing = None;
        }
        self.wanted > revision && self.sent.is_none_or(|sent| sent <= revision)
    }

    /// Whether a Force Recompile is still running.
    pub fn forcing(&self) -> bool {
        self.forcing.is_some()
    }

    /// A Force Recompile will not be answered (its send failed).
    pub fn cancel_force(&mut self) {
        self.forcing = None;
    }

    /// Whether the pages on screen are older than the editor's text.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn behind(&self) -> bool {
        self.done.is_none_or(|done| done < self.wanted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiler(name: &str) -> LiveCompiler {
        let dir = std::env::temp_dir().join(format!("malgel-live-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        LiveCompiler::new(&dir.join("main.typ"))
    }

    fn pages(n: usize) -> String {
        (1..=n)
            .map(|i| {
                format!(
                    "= Section {i}\n\n{}\n\n#pagebreak()\n",
                    "Some words here. ".repeat(40)
                )
            })
            .collect()
    }

    #[test]
    fn headings_strings_and_keywords_are_tagged() {
        let source = typst::syntax::Source::detached("= Title\n#let x = \"s\"\n");
        let tags: Vec<Tag> = highlights(&source)
            .into_iter()
            .map(|(_, tag)| tag)
            .collect();
        assert!(tags.contains(&Tag::Heading));
        assert!(tags.contains(&Tag::Keyword));
        assert!(tags.contains(&Tag::String));
    }

    #[test]
    fn an_edit_redraws_only_the_page_it_changed() {
        let mut live = compiler("redraw");
        let text = pages(6);
        let first = live.compile(&text, 1);
        assert_eq!(first.pages.as_ref().map(Vec::len), Some(7));
        assert_eq!(first.redrawn, 7);
        let edited = text.replacen("Section 3", "Section three", 1);
        let second = live.compile(&edited, 2);
        assert_eq!(second.redrawn, 1, "only page 3 changed");
    }

    #[test]
    fn a_broken_edit_keeps_the_pages_and_points_at_the_mistake() {
        let mut live = compiler("broken");
        live.compile("= Fine\n", 1);
        let text = "= Fine\n#oops(\n";
        let failed = live.compile(text, 2);
        assert!(failed.pages.is_none());
        let error = failed
            .diagnostics
            .iter()
            .find(|d| d.error)
            .expect("an error");
        let range = error.range.clone().expect("in the source");
        assert!(range.start >= 7, "{range:?}");
        // The last good document still answers jumps.
        assert!(live.page_at(2).is_some());
    }

    #[test]
    fn a_changed_import_recompiles() {
        let dir = std::env::temp_dir().join(format!("malgel-live-import-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("part.typ"), "One").unwrap();
        let mut live = LiveCompiler::new(&dir.join("main.typ"));
        live.compile("#include \"part.typ\"\n", 1);
        assert!(live.recompile_if_imports_changed().is_none());
        std::fs::write(dir.join("part.typ"), "Two, longer").unwrap();
        assert!(live.recompile_if_imports_changed().is_some());
    }

    #[test]
    fn click_and_cursor_jumps_round_trip() {
        let mut live = compiler("jump");
        let text = "= Title\n\nHello world, this is a paragraph.\n";
        live.compile(text, 1);
        // An empty line at the end still lands on the page.
        assert!(live.page_at(text.len()).is_some());
        let offset = text.find("paragraph").unwrap();
        let (page, x, y) = live.page_at(offset).expect("on the page");
        assert_eq!(page, 0);
        let back = live
            .source_at(page, x + 1., y - 2.)
            .expect("back in the source");
        assert!(
            text[..back].contains("Hello") || back >= text.find("Hello").unwrap(),
            "{back}"
        );
    }

    #[test]
    fn heading_refs_need_numbering_to_produce_pages() {
        let mut live = compiler("heading-ref");
        // Same class of error as samples/nav-preview.typ without `#set heading`.
        let broken = "= Title <sec>\n\nSee @sec.\n";
        let failed = live.compile(broken, 1);
        assert!(failed.pages.is_none(), "unnumbered heading ref must fail");
        assert!(
            failed.diagnostics.iter().any(|d| d.error),
            "expected a compile error"
        );
        let ok = "#set heading(numbering: \"1.\")\n= Title <sec>\n\nSee @sec.\n";
        let compiled = live.compile(ok, 2);
        assert!(
            compiled.pages.is_some(),
            "numbered heading ref should compile"
        );
        assert!(!compiled.pages.as_ref().unwrap().is_empty());
    }

    #[test]
    fn go_to_definition_finds_a_label_from_its_ref() {
        let mut live = compiler("def");
        let text = "#figure[Hi] <fig>\nSee @fig for details.\n";
        live.compile(text, 1);
        let at = text.find("@fig").expect("@fig");
        let target = live.definition_at(at).expect("label definition");
        assert!(
            target < at,
            "definition {target} should be the label before the ref at {at}"
        );
        assert!(
            text[target..].starts_with("#figure") || text[target..].contains("<fig>"),
            "{target}"
        );
    }

    /// Keystroke-to-pages time on a multi-page document, warm caches.
    #[test]
    fn a_keystroke_on_a_long_document_compiles_quickly() {
        let mut live = compiler("latency");
        let mut text = pages(30);
        live.compile(&text, 0);
        let mut worst = Duration::ZERO;
        let mut total = Duration::ZERO;
        for i in 0..10 {
            let at = text.find("Section 15").unwrap();
            text.insert(at, 'x');
            let result = live.compile(&text, i + 1);
            assert!(result.pages.is_some());
            worst = worst.max(result.elapsed);
            total += result.elapsed;
        }
        eprintln!(
            "typst live: 31 pages, mean {:?}, worst {:?}",
            total / 10,
            worst
        );
    }

    #[test]
    fn large_page_sheet_is_downsampled() {
        let mut live = compiler("poster");
        // 80cm square → uncapped 2× raster would be ~4535² (~78 MiB RGBA).
        let text = "#set page(width: 80cm, height: 80cm, margin: 1cm)\n= Poster\n";
        let compiled = live.compile(text, 1);
        let pages = compiled.pages.expect("poster should compile");
        assert_eq!(pages.len(), 1);
        let img = &pages[0].image;
        assert!(
            img.bytes().len() < 8 * 1024 * 1024,
            "poster sheet too large: {} bytes",
            img.bytes().len()
        );
        let b = img.bytes();
        assert!(
            (b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP")
                || b.starts_with(&[0xFF, 0xD8])
                || (b.len() >= 4 && b[0] == 0x89 && &b[1..4] == b"PNG"),
            "expected WebP, JPEG, or PNG sheet cache encoding"
        );
    }

    fn ids(compiled: &Compiled) -> Vec<u64> {
        compiled
            .pages
            .as_ref()
            .expect("compiled")
            .iter()
            .map(|page| page.image.id())
            .collect()
    }

    /// One character, a deleted character and an undo each reach the pages:
    /// the edited page gets a new sheet, the others keep theirs.
    #[test]
    fn single_character_edits_redraw_the_page() {
        let mut live = compiler("tiny-edits");
        let text = pages(3);
        let first = live.compile_progressive(&text, 1, false, |_| {});
        let before = ids(&first);
        let typed = text.replacen("Section 2", "Section 2x", 1);
        let second = live.compile_progressive(&typed, 2, false, |_| {});
        let after = ids(&second);
        assert_eq!(second.redrawn, 1, "only the edited page is drawn");
        assert_eq!(before[0], after[0]);
        assert_ne!(
            before[1], after[1],
            "a one-character edit must change page 2"
        );
        // Undo: the old text, and the old page, come back.
        let undone = live.compile_progressive(&text, 3, false, |_| {});
        assert_eq!(ids(&undone), before);
        // A single deleted character.
        let deleted = text.replacen("Section 3", "Section ", 1);
        let shown = live.compile_progressive(&deleted, 4, false, |_| {});
        assert_ne!(ids(&shown)[2], before[2]);
    }

    /// Force Recompile skips the "same text, same pages" shortcut and the
    /// sheet cache: every page is compiled and drawn again.
    #[test]
    fn force_recompile_bypasses_every_cache() {
        let mut live = compiler("force");
        let text = pages(3);
        let first = live.compile_progressive(&text, 1, false, |_| {});
        let count = first.pages.as_ref().map_or(0, Vec::len);
        assert!(count >= 3);
        let again = live.compile_progressive(&text, 2, false, |_| {});
        assert_eq!(again.redrawn, 0, "unchanged text reuses every sheet");
        let forced = live.compile_progressive(&text, 3, true, |_| {});
        assert_eq!(forced.redrawn, count, "a forced compile draws every page");
        assert_eq!(forced.revision, 3);
        let old = first.pages.unwrap();
        let new = forced.pages.unwrap();
        assert!(
            old.iter()
                .zip(&new)
                .all(|(a, b)| !Arc::ptr_eq(&a.image, &b.image)),
            "forced sheets are new images, so the viewer reloads them"
        );
    }

    /// A progressive paint never shows fewer pages than the document has:
    /// pages still being drawn keep their previous sheet meanwhile, so the
    /// viewer does not shrink (and lose its scroll position) mid-compile.
    #[test]
    fn progressive_paint_keeps_the_page_count() {
        let mut live = compiler("progressive-count");
        let text = pages(5);
        let first = live.compile_progressive(&text, 1, false, |_| {});
        let count = first.pages.as_ref().map_or(0, Vec::len);
        // A page-wide change dirties every page.
        let restyled = format!("#set text(fill: blue)\n{text}");
        let mut partials = Vec::new();
        let done = live.compile_progressive(&restyled, 2, false, |partial| {
            partials.push(partial.pages.as_ref().map_or(0, Vec::len));
        });
        assert!(
            !partials.is_empty(),
            "several dirty pages paint progressively"
        );
        assert!(
            partials.iter().all(|&n| n == count),
            "partial page counts {partials:?} != {count}"
        );
        assert_eq!(done.pages.as_ref().map_or(0, Vec::len), count);
    }

    /// Edits queued behind a running compile fold into the newest, and that
    /// newest text is compiled next — nothing typed during a compile is lost.
    #[test]
    fn edits_during_a_compile_are_compiled_next() {
        use futures::StreamExt as _;
        let dir = std::env::temp_dir().join(format!("malgel-live-inflight-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (worker, mut answers) = LiveWorker::spawn(dir.join("main.typ"));
        let text = pages(8);
        // The first compile is slow; the edits land while it runs.
        assert!(worker.send(Request::Compile {
            text: text.clone(),
            revision: 1,
            force: false,
        }));
        let mut last = text.clone();
        for revision in 2..=4 {
            last = last.replacen("Section 1\n", "Section 1!\n", 1);
            assert!(worker.send(Request::Compile {
                text: last.clone(),
                revision,
                force: false,
            }));
        }
        let mut finals = Vec::new();
        let newest = futures::executor::block_on(async {
            loop {
                let Some(Response::Compiled(compiled)) = answers.next().await else {
                    continue;
                };
                if compiled.partial {
                    continue;
                }
                finals.push(compiled.revision);
                if compiled.revision == 4 {
                    break compiled;
                }
            }
        });
        assert!(
            finals.len() <= 2,
            "queued edits fold into one compile: {finals:?}"
        );
        assert_eq!(*finals.last().unwrap(), 4);
        // The pages shown are the newest text's.
        let mut check = compiler("inflight-check");
        let expected = check.compile(&last, 9);
        let shown: Vec<u64> = newest.pages.unwrap().iter().map(|p| p.image.id()).collect();
        assert_eq!(shown, ids(&expected));
    }

    /// A Force Recompile followed at once by an edit is still forced.
    #[test]
    fn a_force_folded_into_a_newer_edit_stays_forced() {
        let batch = vec![
            Request::Compile {
                text: "a".into(),
                revision: 1,
                force: true,
            },
            Request::PageAt { offset: 0 },
            Request::Compile {
                text: "ab".into(),
                revision: 2,
                force: false,
            },
        ];
        let folded = fold_batch(batch);
        assert_eq!(folded.len(), 2);
        assert!(matches!(
            folded[1],
            Request::Compile {
                revision: 2,
                force: true,
                ..
            }
        ));
    }

    /// Requests with nothing to answer do not stop the thread: the next
    /// edit is still compiled.
    #[test]
    fn the_worker_keeps_serving_after_an_unanswered_request() {
        use futures::StreamExt as _;
        let dir = std::env::temp_dir().join(format!("malgel-live-alive-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (worker, mut answers) = LiveWorker::spawn(dir.join("main.typ"));
        // Nothing compiled yet: these have no answer.
        assert!(worker.send(Request::SourceAt {
            page: 99,
            x: -1.,
            y: -1.
        }));
        assert!(worker.send(Request::DefinitionAt { offset: 10_000 }));
        std::thread::sleep(Duration::from_millis(50));
        assert!(worker.send(Request::Compile {
            text: "= Still here\n".into(),
            revision: 7,
            force: false,
        }));
        let compiled = futures::executor::block_on(async {
            loop {
                if let Some(Response::Compiled(compiled)) = answers.next().await
                    && !compiled.partial
                {
                    break compiled;
                }
            }
        });
        assert_eq!(compiled.revision, 7);
        assert!(compiled.pages.is_some());
    }

    /// A small edit is sent and, once answered, leaves nothing behind.
    #[test]
    fn sync_a_small_edit_is_compiled_once() {
        let mut sync = LiveSync::default();
        sync.edited(1);
        assert!(sync.behind());
        sync.sent(1, false);
        assert!(!sync.compiled(1, false), "nothing newer: no follow-up");
        assert!(!sync.behind());
    }

    /// An edit made while a compile runs is compiled after it: either its
    /// own send is already queued, or the answer triggers a follow-up.
    #[test]
    fn sync_an_edit_during_a_compile_gets_a_follow_up() {
        let mut sync = LiveSync::default();
        sync.edited(1);
        sync.sent(1, false);
        // Typed while revision 1 compiles; its send is queued.
        sync.edited(2);
        sync.sent(2, false);
        assert!(!sync.compiled(1, false), "revision 2 is already on its way");
        assert!(!sync.compiled(2, false));
        // Typed while revision 2 compiles, but its send never happened (the
        // budget gate flipped, a debounce was dropped, …).
        sync.edited(3);
        assert!(sync.compiled(2, false), "editor is ahead: send again");
        assert!(sync.behind());
        sync.sent(3, false);
        assert!(!sync.compiled(3, false));
        assert!(!sync.behind());
    }

    /// Partial (progressive) answers never decide anything.
    #[test]
    fn sync_partial_answers_do_not_count() {
        let mut sync = LiveSync::default();
        sync.edited(2);
        sync.sent(1, false);
        assert!(!sync.compiled(1, true));
        assert!(sync.behind());
    }

    /// Force Recompile stays "busy" until its own revision is answered.
    #[test]
    fn sync_force_is_busy_until_answered() {
        let mut sync = LiveSync::default();
        sync.edited(4);
        sync.sent(4, false);
        sync.sent(4, true);
        assert!(sync.forcing());
        assert!(!sync.compiled(3, false));
        assert!(sync.forcing(), "an older answer does not end the force");
        assert!(!sync.compiled(4, false));
        assert!(!sync.forcing());
        sync.sent(5, true);
        sync.cancel_force();
        assert!(!sync.forcing());
    }
}
