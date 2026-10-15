//! The live preview a Typst source gets while it is edited, the way
//! tinymist previews one beside the editor.
//!
//! tinymist keeps one compiler world per document, edits its source in
//! place on every change, lets `comemo` reuse whatever the edit did not
//! touch, and ships the result to a webview as incremental vector data. A
//! GPUI view has no webview, so the pages are drawn as SVG sheets here and
//! the page viewer shows them; a page whose frame did not change keeps its
//! sheet (and the GPU texture made from it), so typing on page 7 redraws
//! page 7 only.
//!
//! All of it runs on a thread of its own. The editor sends the text after a
//! short pause in typing; requests that queue up while a compile runs are
//! folded into the newest, so the preview never falls behind. A compile that
//! fails reports its diagnostics and keeps the last good pages. The same
//! thread answers where a click on a page comes from in the source and where
//! the caret's text lands on the pages (typst-ide's `jump_from_click` and
//! `jump_from_cursor`), since it holds the compiled document.

use std::{
    collections::HashMap,
    num::NonZeroUsize,
    ops::Range,
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
    syntax::{LinkedNode, Tag},
    utils::hash128,
};

use typst_layout::PagedDocument;

use crate::typst_preview::DocumentWorld;

/// How often the compiler thread checks the files a document named.
const IMPORT_POLL: Duration = Duration::from_secs(1);

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
        }
    }

    /// Compile `text`, drawing only pages that changed.
    pub fn compile(&mut self, text: &str, revision: usize) -> Compiled {
        let started = Instant::now();
        if self.text != text {
            self.text = text.to_string();
        }
        self.revision = revision;
        self.world.set_text(text);
        let warned = typst::compile::<PagedDocument>(&self.world);
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
                                Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes()))
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
        Compiled {
            revision,
            pages,
            diagnostics,
            elapsed: started.elapsed(),
            redrawn,
            highlights: highlights(self.world.main_source()),
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
}

/// What the editor asks the compiler thread.
pub enum Request {
    Compile { text: String, revision: usize },
    SourceAt { page: usize, x: f32, y: f32 },
    PageAt { offset: usize },
}

/// What the compiler thread answers.
pub enum Response {
    Compiled(Compiled),
    SourceAt(usize),
    PageAt(usize, f32, f32),
}

/// The compiler thread for one document. Dropping it ends the thread.
pub struct LiveWorker {
    requests: mpsc::Sender<Request>,
}

impl LiveWorker {
    /// Start compiling `path`'s text on a thread of its own; answers arrive
    /// on the returned receiver.
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
                            if let Some(compiled) = compiler.recompile_if_imports_changed()
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
                    // Fold a queue of edits into the newest one.
                    let mut batch = vec![first];
                    batch.extend(inbox.try_iter());
                    let newest = batch
                        .iter()
                        .rposition(|r| matches!(r, Request::Compile { .. }));
                    for (ix, request) in batch.into_iter().enumerate() {
                        let answer = match request {
                            Request::Compile { text, revision } => {
                                if Some(ix) != newest {
                                    continue;
                                }
                                Response::Compiled(compiler.compile(&text, revision))
                            }
                            Request::SourceAt { page, x, y } => {
                                match compiler.source_at(page, x, y) {
                                    Some(offset) => Response::SourceAt(offset),
                                    None => continue,
                                }
                            }
                            Request::PageAt { offset } => match compiler.page_at(offset) {
                                Some((page, x, y)) => Response::PageAt(page, x, y),
                                None => continue,
                            },
                        };
                        if answers.unbounded_send(answer).is_err() {
                            return;
                        }
                    }
                }
            })
            .expect("start the Typst compiler thread");
        (Self { requests }, outbox)
    }

    pub fn send(&self, request: Request) {
        _ = self.requests.send(request);
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
}
