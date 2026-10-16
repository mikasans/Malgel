//! PDF viewing support: renders PDF documents using Hayro and GPUI.
//!
//! Pages are parsed with `hayro_interpret::hayro_syntax::Pdf` and turned into
//! crisp SVG sheets with `hayro_svg::convert` **on demand**: only the pages in
//! view are rendered, and rendered pages are cached, so long documents open
//! and scroll smoothly. Conversion runs on a background executor — never on
//! the GPUI UI thread — so title-bar and layout buttons stay clickable while
//! pages fill in. The viewer is one continuous vertical scroll with
//! fit-width / fit-page / custom zoom and page navigation, and the gestures
//! `zoom` documents and the image viewer share: Command/Control with the wheel
//! zooms, and a double click swaps between the fit and a closer look. A
//! single left-click (no modifier) on a live Typst page jumps to the source.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Instant,
};

use gpui_kit::{
    Anchor, App, Context, ElementId, FocusHandle, Focusable, Image, ImageFormat,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, PinchEvent, Pixels, Point, Render, ScrollDelta,
    ScrollStrategy, ScrollWheelEvent, Size, Styled as _, Window,
    assets::IconName,
    base::Disableable as _,
    canvas,
    component::{
        ActiveTheme as _, ElementExt as _, Icon, Sizable as _, VirtualListScrollHandle,
        button::{Button, ButtonVariants as _},
        h_flex,
        menu::{DropdownMenu as _, PopupMenuItem},
        v_flex, v_virtual_list,
    },
    div, img,
    prelude::FluentBuilder as _,
    px, rgb,
};

use hayro_svg::{
    RenderCache, SvgRenderSettings, convert,
    hayro_interpret::{
        InterpreterSettings,
        font::{FontData, FontQuery, StandardFont},
        hayro_syntax::Pdf,
    },
};

use crate::actions::{
    SetLayout, ToggleFileSidebar, TogglePdfThumbnails, WORKSPACE_CONTEXT, ZoomIn, ZoomOut,
    ZoomReset,
};
use crate::themes::AppSettings;
use crate::zoom;

/// How a document is zoomed when it is opened: the whole first page, the way
/// a PDF reader shows a file it has not seen before. The image viewer opens
/// the same way.
const DEFAULT_FIT: FitMode = FitMode::Page;

/// Gap between pages, in logical pixels.
const PAGE_GAP: f32 = 20.;
/// Margin around the page column, in logical pixels.
const PAGE_MARGIN: f32 = 24.;

/// Width a page thumbnail is drawn at, in logical pixels.
const THUMB_WIDTH: f32 = 96.;
/// Padding around a thumbnail inside its cell, in logical pixels.
const THUMB_PAD: f32 = 6.;
/// Room under a thumbnail for its page number, in logical pixels.
const THUMB_LABEL: f32 = 18.;
/// The thumbnail strip's own width: a thumbnail, its padding and room for
/// the strip's scrollbar.
const THUMB_STRIP: f32 = THUMB_WIDTH + THUMB_PAD * 2. + 18.;

/// The size a page's thumbnail is drawn at for a given thumbnail width.
fn thumb_size(page: &PdfPage, width: f32) -> (f32, f32) {
    if page.width <= 0. {
        return (width, width);
    }
    (width, width * page.height / page.width)
}

/// The index of the page at the top of a viewport scrolled `offset` pixels
/// into the content, given each row's height. Rows hold one or two pages, so a
/// spread reports its first page the way a reader names the page it is on.
fn page_at_offset(rows: &[Vec<usize>], row_heights: &[f32], offset: f32) -> usize {
    let mut top = 0.;
    for (row, &height) in rows.iter().zip(row_heights) {
        if offset < top + height {
            return row.first().copied().unwrap_or(0);
        }
        top += height;
    }
    // Past the last row (a short document in a tall window) the last page is
    // the one on screen.
    rows.last()
        .and_then(|row| row.first().copied())
        .unwrap_or(0)
}

/// How the zoom factor is chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FitMode {
    /// A user-set factor, from the zoom buttons.
    Custom,
    /// The widest page fills the viewport width.
    Width,
    /// The first page fills the viewport.
    Page,
}

/// A page's fixed geometry, known without rendering it.
#[derive(Clone)]
pub struct PdfPage {
    /// 0-indexed page number in the document.
    pub index: usize,
    /// 1-indexed human-readable page number.
    pub page_number: usize,
    /// Width in PDF points.
    pub width: f32,
    /// Height in PDF points.
    pub height: f32,
}

/// A parsed PDF: its pages' geometry, plus lazily rendered SVGs.
pub struct PdfDocument {
    /// The parsed file; `None` for pages drawn elsewhere and handed over
    /// ready-made, the way a live Typst preview's are.
    pdf: Option<Pdf>,
    /// Raw bytes the PDF was parsed from, so a page can be converted on a
    /// background thread without touching the UI-owned `Pdf`.
    bytes: Option<Arc<Vec<u8>>>,
    pages: Vec<PdfPage>,
    rendered: HashMap<usize, Arc<Image>>,
    /// Pages whose background conversion has been scheduled but not yet
    /// applied. Keeps the UI from spawning the same page twice.
    pending: HashSet<usize>,
    /// Bumped on every [`PdfView::reload`] so a late background result from
    /// a previous document is dropped.
    generation: u64,
    interpreter_settings: InterpreterSettings,
    render_settings: SvgRenderSettings,
}

/// Create default interpreter settings using Typst's bundled Foxit standard fonts.
fn default_interpreter_settings() -> InterpreterSettings {
    let select_standard_font = |font: StandardFont| -> Option<(FontData, u32)> {
        let bytes: &'static [u8] = match font {
            StandardFont::Helvetica => typst_assets::pdf::SANS,
            StandardFont::HelveticaBold => typst_assets::pdf::SANS_BOLD,
            StandardFont::HelveticaOblique => typst_assets::pdf::SANS_ITALIC,
            StandardFont::HelveticaBoldOblique => typst_assets::pdf::SANS_BOLD_ITALIC,
            StandardFont::Courier => typst_assets::pdf::FIXED,
            StandardFont::CourierBold => typst_assets::pdf::FIXED_BOLD,
            StandardFont::CourierOblique => typst_assets::pdf::FIXED_ITALIC,
            StandardFont::CourierBoldOblique => typst_assets::pdf::FIXED_BOLD_ITALIC,
            StandardFont::TimesRoman => typst_assets::pdf::SERIF,
            StandardFont::TimesBold => typst_assets::pdf::SERIF_BOLD,
            StandardFont::TimesItalic => typst_assets::pdf::SERIF_ITALIC,
            StandardFont::TimesBoldItalic => typst_assets::pdf::SERIF_BOLD_ITALIC,
            StandardFont::ZapfDingBats => typst_assets::pdf::DING_BATS,
            StandardFont::Symbol => typst_assets::pdf::SYMBOL,
        };
        Some((Arc::new(bytes) as FontData, 0))
    };

    InterpreterSettings {
        font_resolver: Arc::new(move |query| match query {
            FontQuery::Standard(s) => select_standard_font(*s),
            FontQuery::Fallback(f) => select_standard_font(f.pick_standard_font()),
        }),
        cmap_resolver: Arc::new(|_| None),
        warning_sink: Arc::new(|_| {}),
        render_annotations: true,
    }
}

impl PdfDocument {
    /// Parse `bytes` into a document. Pages are measured now and rendered
    /// lazily, so a long document is cheap to open.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        let shared = Arc::new(bytes);
        let pdf =
            Pdf::new(shared.clone()).map_err(|err| format!("Failed to parse PDF: {err:?}"))?;
        let pages = pdf
            .pages()
            .iter()
            .enumerate()
            .map(|(index, page)| {
                let (width, height) = page.render_dimensions();
                PdfPage {
                    index,
                    page_number: index + 1,
                    width,
                    height,
                }
            })
            .collect();
        Ok(Self {
            pdf: Some(pdf),
            bytes: Some(shared),
            pages,
            rendered: HashMap::new(),
            pending: HashSet::new(),
            generation: 0,
            interpreter_settings: default_interpreter_settings(),
            render_settings: SvgRenderSettings {
                bg_color: [255, 255, 255, 255],
            },
        })
    }

    /// Pages already drawn, as (width, height, sheet) in points: a live
    /// Typst preview's, which its compiler renders off the UI thread and
    /// reuses for every page a keystroke left unchanged.
    pub fn from_rendered(pages: Vec<(f32, f32, Arc<Image>)>) -> Self {
        let mut rendered = HashMap::new();
        let pages = pages
            .into_iter()
            .enumerate()
            .map(|(index, (width, height, image))| {
                rendered.insert(index, image);
                PdfPage {
                    index,
                    page_number: index + 1,
                    width,
                    height,
                }
            })
            .collect();
        Self {
            pdf: None,
            bytes: None,
            pages,
            rendered,
            pending: HashSet::new(),
            generation: 0,
            interpreter_settings: default_interpreter_settings(),
            render_settings: SvgRenderSettings {
                bg_color: [255, 255, 255, 255],
            },
        }
    }

    /// The page geometry, in document order.
    pub fn pages(&self) -> &[PdfPage] {
        &self.pages
    }

    /// A second handle on the same pages, for handing a recompiled document
    /// to a viewer without taking the original apart. The file's bytes are
    /// shared; rendered sheets are not, so each copy draws its own and the
    /// cache is never the reason a page looks stale.
    pub fn clone_document(&self) -> Self {
        let Some(pdf) = &self.pdf else {
            return Self {
                pdf: None,
                bytes: None,
                pages: self.pages.clone(),
                rendered: self.rendered.clone(),
                pending: HashSet::new(),
                generation: self.generation,
                interpreter_settings: self.interpreter_settings.clone(),
                render_settings: self.render_settings.clone(),
            };
        };
        let pdf = Pdf::new(pdf.data().clone())
            .expect("the bytes this document was parsed from still parse");
        let pages = pdf
            .pages()
            .iter()
            .enumerate()
            .map(|(index, page)| {
                let (width, height) = page.render_dimensions();
                PdfPage {
                    index,
                    page_number: index + 1,
                    width,
                    height,
                }
            })
            .collect();
        Self {
            pdf: Some(pdf),
            bytes: self.bytes.clone(),
            pages,
            rendered: HashMap::new(),
            pending: HashSet::new(),
            generation: self.generation,
            interpreter_settings: self.interpreter_settings.clone(),
            render_settings: self.render_settings.clone(),
        }
    }

    /// Total number of pages.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// A cached sheet for page `index`, if conversion has finished.
    #[cfg_attr(not(test), allow(dead_code))]
    fn image(&self, index: usize) -> Option<Arc<Image>> {
        self.rendered.get(&index).cloned()
    }

    /// Convert page `index` of `bytes` to an SVG image. Runs off the UI thread.
    fn render_page_bytes(
        bytes: Arc<Vec<u8>>,
        index: usize,
        interpreter_settings: InterpreterSettings,
        render_settings: SvgRenderSettings,
    ) -> Option<(Arc<Image>, std::time::Duration)> {
        let started = Instant::now();
        let pdf = Pdf::new(bytes).ok()?;
        let page = pdf.pages().get(index)?;
        let cache = RenderCache::new();
        let svg = convert(page, &cache, &interpreter_settings, &render_settings);
        let image = Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes()));
        Some((image, started.elapsed()))
    }
}

/// The page indexes each row shows, in order: one per row, or `per_row`.
fn page_rows(count: usize, per_row: usize) -> Vec<Vec<usize>> {
    let per_row = per_row.max(1);
    (0..count)
        .step_by(per_row)
        .map(|start| (start..(start + per_row).min(count)).collect())
        .collect()
}

/// The zoom a fit mode resolves to for a viewport, given every row of page
/// sizes as (width, height) pairs. A row holds one page, or two side by side.
fn fit_zoom(
    mode: FitMode,
    custom_zoom: f32,
    rows: &[Vec<(f32, f32)>],
    viewport: (f32, f32),
) -> f32 {
    let avail_width = (viewport.0 - 2. * PAGE_MARGIN - 12.).max(1.);
    let avail_height = (viewport.1 - 2. * PAGE_MARGIN).max(1.);
    let row_width = |row: &[(f32, f32)]| {
        row.iter().map(|(width, _)| *width).sum::<f32>()
            + PAGE_GAP * row.len().saturating_sub(1) as f32
    };
    match mode {
        FitMode::Custom => custom_zoom.clamp(zoom::MIN, zoom::MAX),
        FitMode::Width => {
            let widest = rows.iter().map(|row| row_width(row)).fold(1.0f32, f32::max);
            (avail_width / widest).clamp(zoom::MIN, zoom::MAX)
        }
        FitMode::Page => {
            let first = rows.first().map(Vec::as_slice).unwrap_or(&[]);
            let width = row_width(first).max(1.);
            let height = first
                .iter()
                .map(|(_, height)| *height)
                .fold(1.0f32, f32::max);
            (avail_width / width)
                .min(avail_height / height)
                .clamp(zoom::MIN, zoom::MAX)
        }
    }
}

/// Row list + per-row sizes for the page virtual list.
struct ListLayoutCache {
    /// Fingerprint: page count, spread, zoom bits, and page geometry.
    key: u64,
    rows: Rc<Vec<Vec<usize>>>,
    sizes: Rc<Vec<Size<Pixels>>>,
}

/// A GPUI view displaying a scrollable PDF document.
pub struct PdfView {
    path: Option<PathBuf>,
    title: String,
    doc: Option<PdfDocument>,
    fit: FitMode,
    /// The factor actually used, kept in sync each frame for the toolbar.
    zoom: f32,
    /// The user's factor, used when `fit` is [`FitMode::Custom`].
    custom_zoom: f32,
    /// Whether two pages share a row, the way a bound document reads.
    spread: bool,
    /// The page currently at the top of the viewport.
    current_page: usize,
    /// Whether the page thumbnails are shown beside the pages.
    thumbnails: bool,
    /// Where a drag started, and the scroll offset it started from.
    drag: Option<(Point<Pixels>, Point<Pixels>)>,
    scroll: VirtualListScrollHandle,
    /// The thumbnails' own scroll position, so the strip and the pages move
    /// independently.
    thumb_scroll: VirtualListScrollHandle,
    focus_handle: FocusHandle,
    /// Where each page drawn last frame sat on screen, so a click can be
    /// told which page it landed on and where.
    page_bounds: Rc<std::cell::RefCell<HashMap<usize, gpui_kit::Bounds<Pixels>>>>,
    /// Where the left button went down, to tell a click from a drag.
    press: Option<Point<Pixels>>,
    /// Cached virtual-list row geometry, invalidated when zoom/spread/pages change.
    /// Avoids reallocating size vectors on every mouse-move paint while dragging.
    list_cache: Option<ListLayoutCache>,
}

/// What a page viewer reports to the document showing it.
pub enum PdfViewEvent {
    /// A single left-click (not a drag, not a double-click) on page `page`
    /// (0-based) at `x`, `y` points from its top left. A live Typst preview
    /// jumps to the source there — no modifier and no second click needed.
    Clicked { page: usize, x: f32, y: f32 },
}

impl gpui_kit::EventEmitter<PdfViewEvent> for PdfView {}

impl PdfView {
    /// Create a `PdfView` from a parsed document.
    pub fn new(path: Option<PathBuf>, doc: PdfDocument, cx: &mut Context<Self>) -> Self {
        let title = path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "PDF Document".to_string());

        Self {
            path,
            title,
            doc: Some(doc),
            fit: DEFAULT_FIT,
            zoom: zoom::DEFAULT,
            custom_zoom: zoom::DEFAULT,
            spread: false,
            current_page: 0,
            thumbnails: false,
            drag: None,
            scroll: VirtualListScrollHandle::new(),
            thumb_scroll: VirtualListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            page_bounds: Rc::default(),
            press: None,
            list_cache: None,
        }
    }

    /// Show a recompiled version of the same document, the way a Typst
    /// preview updates when its source changes on disk.
    ///
    /// The reader's view is kept where it makes sense: zoom, spread and the
    /// thumbnail strip survive, and the scroll stays on the page that was
    /// showing (which the pages are then re-laid out around). Rendered sheets
    /// are dropped, so the new pages are drawn from the new content rather
    /// than from the old cache.
    pub fn reload(&mut self, doc: PdfDocument, cx: &mut Context<Self>) {
        let current_page = self.current_page;
        let generation = self
            .doc
            .as_ref()
            .map(|doc| doc.generation.wrapping_add(1))
            .unwrap_or(1);
        self.doc = Some(doc);
        if let Some(doc) = &mut self.doc {
            doc.generation = generation;
            doc.pending.clear();
        }
        // Clamped: a document that shrank leaves the reader on its last page.
        self.current_page = current_page.min(self.page_count().saturating_sub(1));
        self.list_cache = None;
        cx.notify();
    }

    /// The sheet for page `index`, scheduling a background conversion when
    /// it is not yet ready. Returns `None` while conversion is in flight so
    /// the caller can paint a placeholder without blocking the UI thread.
    fn page_image(&mut self, index: usize, cx: &mut Context<Self>) -> Option<Arc<Image>> {
        let doc = self.doc.as_mut()?;
        if let Some(image) = doc.rendered.get(&index) {
            return Some(image.clone());
        }
        // Pre-rendered Typst sheets land in `rendered` already; no bytes to convert.
        let bytes = doc.bytes.clone()?;
        if !doc.pending.insert(index) {
            return None;
        }
        let generation = doc.generation;
        let interpreter_settings = doc.interpreter_settings.clone();
        let render_settings = doc.render_settings.clone();
        let task = cx.background_executor().spawn(async move {
            PdfDocument::render_page_bytes(bytes, index, interpreter_settings, render_settings)
        });
        cx.spawn(async move |this, cx| {
            let outcome = task.await;
            _ = this.update(cx, |this, cx| {
                let Some(doc) = this.doc.as_mut() else {
                    return;
                };
                doc.pending.remove(&index);
                if doc.generation != generation {
                    log::debug!(
                        "pdf: drop stale page {index} render (generation {generation} -> {})",
                        doc.generation
                    );
                    return;
                }
                match outcome {
                    Some((image, elapsed)) => {
                        log::debug!(
                            "pdf: page {index} converted in {:.1} ms (off UI thread)",
                            elapsed.as_secs_f64() * 1000.
                        );
                        doc.rendered.insert(index, image);
                        cx.notify();
                    }
                    None => {
                        log::warn!("pdf: page {index} conversion failed");
                    }
                }
            });
        })
        .detach();
        None
    }

    /// Whether two pages are shown side by side.
    pub fn spread(&self) -> bool {
        self.spread
    }

    /// Show two pages side by side, or one at a time.
    pub fn set_spread(&mut self, spread: bool, cx: &mut Context<Self>) {
        if self.spread != spread {
            self.spread = spread;
            self.list_cache = None;
            cx.notify();
        }
    }

    /// How many pages share a row.
    fn per_row(&self) -> usize {
        if self.spread { 2 } else { 1 }
    }

    /// The page indexes each row shows, in order.
    fn rows(&self) -> Vec<Vec<usize>> {
        page_rows(self.page_count(), self.per_row())
    }

    /// The page the viewport is showing, read from the scroll position rather
    /// than written while the page list renders. Everything that names the
    /// current page — the toolbar, the zoom target and the thumbnail strip —
    /// therefore reads one value that was set before any of them were built,
    /// instead of whichever had run first.
    fn visible_page(&self) -> usize {
        let Some(doc) = &self.doc else {
            return 0;
        };
        let rows = self.rows();
        if rows.is_empty() {
            return 0;
        }
        let zoom = self.zoom;
        let heights: Vec<f32> = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|&ix| doc.pages[ix].height)
                    .fold(0.0f32, f32::max)
                    * zoom
                    + PAGE_GAP
            })
            .collect();
        // A scrolling list translates its content by the scroll offset, so the
        // offset is negative once the reader scrolls down; the top of the
        // viewport in content space is the negation.
        let offset = (-self.scroll.base_handle().offset().y.as_f32()).max(0.);
        page_at_offset(&rows, &heights, offset)
    }

    /// The path of the open document, if opened from disk.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Document title shown in the toolbar.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Total number of pages.
    pub fn page_count(&self) -> usize {
        self.doc.as_ref().map_or(0, PdfDocument::page_count)
    }

    /// Current zoom level.
    /// The page at the top of the viewport, counted from zero.
    pub fn current_page(&self) -> usize {
        self.current_page
    }

    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// The zoom mode in effect.
    pub fn fit_mode(&self) -> FitMode {
        self.fit
    }

    /// Zoom in by one step.
    pub fn zoom_in(&mut self, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom * zoom::BUTTON_STEP, cx);
    }

    /// Zoom out by one step.
    pub fn zoom_out(&mut self, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom / zoom::BUTTON_STEP, cx);
    }

    /// Reset zoom level back to 100%.
    pub fn reset_zoom(&mut self, cx: &mut Context<Self>) {
        self.set_zoom(zoom::DEFAULT, cx);
    }

    /// Set an explicit zoom factor, switching to custom zoom.
    pub fn set_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        let clamped = zoom.clamp(zoom::MIN, zoom::MAX);
        if self.fit != FitMode::Custom || (self.custom_zoom - clamped).abs() > f32::EPSILON {
            self.fit = FitMode::Custom;
            self.custom_zoom = clamped;
            self.zoom = clamped;
            self.list_cache = None;
            cx.notify();
        }
    }

    /// Command or Control plus the wheel zooms the pages, as in the image
    /// viewer. A bare wheel is left to the list behind this element, which
    /// scrolls the document the way a PDF reader does.
    fn on_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.modifiers.secondary() {
            return;
        }
        let factor = match event.delta {
            ScrollDelta::Lines(lines) => zoom::WHEEL_STEP.powf(lines.y.clamp(-1., 1.)),
            ScrollDelta::Pixels(delta) => (f32::from(delta.y) / 200.).exp(),
        };
        self.set_zoom(self.zoom * factor, cx);
        // Otherwise the list scrolls the document at the same time as the
        // zoom resizes it.
        cx.stop_propagation();
    }

    fn on_pinch(&mut self, event: &PinchEvent, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom * (1. + event.delta), cx);
    }

    /// Dragging moves the pages, the way it moves an image in its viewer and
    /// a diagram in its own window: zoomed in, the wheel alone leaves the
    /// rest of the sheet out of reach.
    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.pressed_button != Some(MouseButton::Left) {
            if self.drag.take().is_some() {
                cx.notify();
            }
            return;
        }
        // Start a drag only after the pointer leaves the click slop, so a
        // plain single click still counts as a jump (not a pan).
        if self.drag.is_none() {
            let Some(press) = self.press else {
                return;
            };
            let moved = event.position - press;
            if moved.x.abs() <= px(4.) && moved.y.abs() <= px(4.) {
                return;
            }
            self.drag = Some((press, self.scroll.base_handle().offset()));
        }
        let Some((start, origin)) = self.drag else {
            return;
        };
        let next = origin + (event.position - start);
        let handle = self.scroll.base_handle();
        // Skip no-op moves so we do not dirty the view every sub-pixel event.
        if handle.offset() == next {
            return;
        }
        handle.set_offset(next);
        // Keep the page label in sync without waiting for mouse-up.
        let page = self.visible_page();
        if page != self.current_page {
            self.current_page = page;
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let dragged = self.drag.take().is_some();
        let press = self.press.take();
        // Double-click ups are for zoom (handled on mouse_down). Typst jump
        // is a single left-click with no modifier.
        if event.click_count != 1 || dragged {
            return;
        }
        if let Some(press) = press {
            let moved = event.position - press;
            if moved.x.abs() > px(4.) || moved.y.abs() > px(4.) {
                return;
            }
        }
        // Hit-test even when `press` was lost (focusing-click quirks): a
        // single click on a page should still jump.
        let zoom = self.zoom.max(0.01);
        let hit = self
            .page_bounds
            .borrow()
            .iter()
            .find(|(_, bounds)| bounds.contains(&event.position))
            .map(|(&page, bounds)| {
                let local = event.position - bounds.origin;
                (page, local.x.as_f32() / zoom, local.y.as_f32() / zoom)
            });
        if let Some((page, x, y)) = hit {
            cx.emit(PdfViewEvent::Clicked { page, x, y });
        }
    }

    /// Scroll so the point `y` points down page `page` is in view, leaving
    /// the view alone when it already is: a live Typst preview follows the
    /// caret in its source this way.
    pub fn reveal(&mut self, page: usize, y: f32, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else {
            return;
        };
        if page >= doc.pages.len() {
            return;
        }
        let zoom = self.zoom;
        let rows = self.rows();
        let row_of = page / self.per_row();
        let above: f32 = rows
            .iter()
            .take(row_of)
            .map(|row| {
                row.iter()
                    .map(|&ix| doc.pages[ix].height)
                    .fold(0.0f32, f32::max)
                    * zoom
                    + PAGE_GAP
            })
            .sum();
        let target = above + y * zoom;
        let handle = self.scroll.base_handle();
        let offset = handle.offset();
        let height = self.scroll.bounds().size.height.as_f32();
        if let Some(top) = reveal_offset(-offset.y.as_f32(), height, target) {
            handle.set_offset(gpui_kit::point(offset.x, px(-top)));
            cx.notify();
        }
    }

    /// Double-clicking swaps between the fitted view and a closer look, the
    /// way a double-click does in the diagram viewer and in the image viewer:
    /// from a fit it doubles what the fit resolved to, and from a zoom the
    /// user set it goes back to fitting the page.
    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        // Only the left button gets here; the wiring asks for it.
        if event.click_count != 2 {
            // Record the press only — dragging starts after a few pixels of
            // movement so a single click can jump to Typst source.
            self.press = Some(event.position);
            self.drag = None;
            return;
        }
        self.drag = None;
        self.press = None;
        if self.fit == FitMode::Custom {
            self.fit_page(cx);
        } else {
            self.set_zoom(self.zoom * 2., cx);
        }
    }

    /// Scale the widest page to the viewport width.
    pub fn fit_width(&mut self, cx: &mut Context<Self>) {
        if self.fit != FitMode::Width {
            self.fit = FitMode::Width;
            self.list_cache = None;
            cx.notify();
        }
    }

    /// Scale the first page to fit the viewport.
    pub fn fit_page(&mut self, cx: &mut Context<Self>) {
        if self.fit != FitMode::Page {
            self.fit = FitMode::Page;
            self.list_cache = None;
            cx.notify();
        }
    }

    /// Scroll so page `index` (0-based) is at the top.
    pub fn go_to_page(&mut self, index: usize, cx: &mut Context<Self>) {
        let count = self.page_count();
        if count == 0 {
            return;
        }
        let index = index.min(count - 1);
        self.current_page = index;
        // Rows hold one or two pages; scroll to the row showing this one.
        self.scroll
            .scroll_to_item(index / self.per_row(), ScrollStrategy::Top);
        if self.thumbnails {
            // Bring the thumbnail of the page we are going to into view, the
            // way a PDF reader does. Scrolling the pages themselves does not
            // move the strip, so the reader can browse it on its own.
            self.thumb_scroll
                .scroll_to_item(index, ScrollStrategy::Center);
        }
        cx.notify();
    }

    /// Whether the page thumbnails are shown beside the pages.
    pub fn shows_thumbnails(&self) -> bool {
        self.thumbnails
    }

    /// Show or hide the page thumbnails.
    pub fn set_thumbnails(&mut self, shown: bool, cx: &mut Context<Self>) {
        if self.thumbnails == shown {
            return;
        }
        self.thumbnails = shown;
        if shown {
            // Open the strip on the page the reader is already on.
            self.thumb_scroll
                .scroll_to_item(self.current_page, ScrollStrategy::Center);
        }
        cx.notify();
    }

    /// Show or hide the page thumbnails.
    pub fn toggle_thumbnails(&mut self, cx: &mut Context<Self>) {
        let shown = !self.thumbnails;
        self.set_thumbnails(shown, cx);
    }

    /// The zoom factor a fit mode resolves to for a given viewport.
    fn resolved_zoom(&self, viewport: Size<Pixels>) -> f32 {
        let Some(doc) = &self.doc else {
            return zoom::DEFAULT;
        };
        if doc.pages.is_empty() {
            return zoom::DEFAULT;
        }
        let rows: Vec<Vec<(f32, f32)>> = self
            .rows()
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|ix| (doc.pages[ix].width, doc.pages[ix].height))
                    .collect()
            })
            .collect();
        fit_zoom(
            self.fit,
            self.custom_zoom,
            &rows,
            (viewport.width.as_f32(), viewport.height.as_f32()),
        )
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let zoom_pct = zoom::percent(self.zoom);
        let total = self.page_count();
        let current = (self.current_page + 1).min(total.max(1));
        let menu_entity = cx.entity();

        h_flex()
            .id("pdf-toolbar")
            .h_9()
            .px_3()
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .items_center()
            .justify_between()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        Icon::new(IconName::FileText)
                            .small()
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .min_w_0()
                            .truncate()
                            .child(self.title.clone()),
                    )
                    .when(total > 0, |this| {
                        this.child(div().text_xs().text_color(theme.muted_foreground).child(
                            format!("({total} {})", if total == 1 { "page" } else { "pages" }),
                        ))
                    }),
            )
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .min_w_0()
                    .flex_shrink_0()
                    .overflow_x_hidden()
                    .child(
                        Button::new("pdf-thumbnails")
                            .ghost()
                            .small()
                            .compact()
                            .icon(Icon::new(IconName::PanelLeft))
                            .tooltip("Page thumbnails")
                            .toggled(self.thumbnails)
                            .disabled(total == 0)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_thumbnails(cx))),
                    )
                    .child(div().w(px(1.)).h_4().bg(theme.border).mx_1())
                    .child(
                        Button::new("pdf-fit-width")
                            .ghost()
                            .small()
                            .compact()
                            .label("Fit width")
                            .tooltip("Fit page width")
                            .toggled(self.fit == FitMode::Width)
                            .disabled(total == 0)
                            .on_click(cx.listener(|this, _, _, cx| this.fit_width(cx))),
                    )
                    .child(
                        Button::new("pdf-fit-page")
                            .ghost()
                            .small()
                            .compact()
                            .label("Fit page")
                            .tooltip("Fit whole page")
                            .toggled(self.fit == FitMode::Page)
                            .disabled(total == 0)
                            .on_click(cx.listener(|this, _, _, cx| this.fit_page(cx))),
                    )
                    .child(
                        Button::new("pdf-spread")
                            .ghost()
                            .small()
                            .compact()
                            .label("Two pages")
                            .tooltip("Show two pages side by side")
                            .toggled(self.spread)
                            .disabled(total == 0)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.set_spread(!this.spread, cx)),
                            ),
                    )
                    .child(
                        Button::new("pdf-zoom-out")
                            .ghost()
                            .small()
                            .compact()
                            .icon(Icon::new(IconName::ZoomOut))
                            .tooltip("Zoom out")
                            .disabled(total == 0 || self.zoom <= zoom::MIN)
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_out(cx))),
                    )
                    // The same percentage menu the image viewer shows, in the
                    // same order: one press of a zoom button or one entry in
                    // the list has to mean the same thing in both viewers.
                    .child(
                        Button::new("pdf-zoom-menu")
                            .ghost()
                            .small()
                            .compact()
                            .label(zoom_pct)
                            .tooltip("Zoom")
                            .disabled(total == 0)
                            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                                let mut menu = menu;
                                for &(factor, label) in zoom::PRESETS {
                                    let entity = menu_entity.clone();
                                    menu = menu.item(PopupMenuItem::new(label).on_click(
                                        move |_, _, cx| {
                                            entity.update(cx, |this, cx| this.set_zoom(factor, cx));
                                        },
                                    ));
                                }
                                let fit_width = menu_entity.clone();
                                let fit_page = menu_entity.clone();
                                menu.item(PopupMenuItem::new("Fit width").on_click(
                                    move |_, _, cx| {
                                        fit_width.update(cx, |this, cx| this.fit_width(cx));
                                    },
                                ))
                                .item(
                                    PopupMenuItem::new("Fit page").on_click(move |_, _, cx| {
                                        fit_page.update(cx, |this, cx| this.fit_page(cx));
                                    }),
                                )
                            }),
                    )
                    .child(
                        Button::new("pdf-zoom-in")
                            .ghost()
                            .small()
                            .compact()
                            .icon(Icon::new(IconName::ZoomIn))
                            .tooltip("Zoom in")
                            .disabled(total == 0 || self.zoom >= zoom::MAX)
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_in(cx))),
                    )
                    .child(div().w(px(1.)).h_4().bg(theme.border).mx_1())
                    .child(
                        Button::new("pdf-prev-page")
                            .ghost()
                            .small()
                            .compact()
                            .icon(Icon::new(IconName::ChevronUp))
                            .tooltip("Previous page")
                            .disabled(total == 0 || current <= 1)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let page = this.current_page.saturating_sub(1);
                                this.go_to_page(page, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .min_w(px(72.))
                            .text_center()
                            .child(format!("Page {current} / {total}")),
                    )
                    .child(
                        Button::new("pdf-next-page")
                            .ghost()
                            .small()
                            .compact()
                            .icon(Icon::new(IconName::ChevronDown))
                            .tooltip("Next page")
                            .disabled(total == 0 || current >= total)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let page = this.current_page + 1;
                                this.go_to_page(page, cx);
                            })),
                    ),
            )
    }

    /// The page thumbnails beside the pages, when they are shown. Clicking
    /// one jumps to that page, the way a PDF reader's page panel does.
    fn render_thumbnails(&mut self, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        if !self.thumbnails {
            return None;
        }
        let cells = {
            let doc = self.doc.as_ref()?;
            if doc.pages.is_empty() {
                return None;
            }
            doc.pages
                .iter()
                .map(|page| {
                    let (width, height) = thumb_size(page, THUMB_WIDTH);
                    gpui_kit::size(
                        px(width + THUMB_PAD * 2.),
                        px(height + THUMB_PAD * 2. + THUMB_LABEL),
                    )
                })
                .collect::<Vec<_>>()
        };
        let sizes: Rc<Vec<Size<Pixels>>> = Rc::new(cells);
        let theme = cx.theme();
        // The colours the cells need, copied out: the render closure below is
        // `'static`, so it cannot hold on to a borrow of the theme.
        let border = theme.border;
        let muted = theme.muted_foreground;
        let active = theme.list_active;
        let current_border_colour = theme.primary;
        Some(
            div()
                .id("pdf-thumbnails")
                .w(px(THUMB_STRIP))
                .h_full()
                .flex_none()
                .border_r_1()
                .border_color(theme.border)
                .child(
                    v_virtual_list(
                        cx.entity(),
                        "pdf-thumbnail-pages",
                        sizes,
                        move |this, range: Range<usize>, _window, context| {
                            let current = this.current_page;
                            range
                                .filter_map(|ix| {
                                    let page = this.doc.as_ref()?.pages.get(ix)?.clone();
                                    let (width, height) = thumb_size(&page, THUMB_WIDTH);
                                    let image = this.page_image(ix, context);
                                    let current_border = if ix == current {
                                        current_border_colour
                                    } else {
                                        border
                                    };
                                    Some(
                                        v_flex()
                                            .id(ElementId::NamedInteger(
                                                "pdf-thumbnail".into(),
                                                ix as u64,
                                            ))
                                            .w(px(width + THUMB_PAD * 2.))
                                            .items_center()
                                            .gap_1()
                                            .p(px(THUMB_PAD))
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .when(ix == current, |this| this.bg(active))
                                            .child(
                                                div()
                                                    .w(px(width))
                                                    .h(px(height))
                                                    .flex_none()
                                                    .bg(rgb(0xffffff))
                                                    .border_1()
                                                    .border_color(current_border)
                                                    .overflow_hidden()
                                                    .children(image.map(|image| {
                                                        img(image).w(px(width)).h(px(height))
                                                    })),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(muted)
                                                    .child(page.page_number.to_string()),
                                            )
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                context.listener(move |this, _, _, cx| {
                                                    cx.stop_propagation();
                                                    this.go_to_page(ix, cx);
                                                }),
                                            )
                                            .into_any_element(),
                                    )
                                })
                                .collect::<Vec<_>>()
                        },
                    )
                    .track_scroll(&self.thumb_scroll),
                )
                .into_any_element(),
        )
    }

    /// Rows and sizes for the page list, reused across paints while zoom and
    /// page geometry stay the same (especially while dragging).
    #[allow(clippy::type_complexity)]
    fn list_layout(&mut self) -> (Rc<Vec<Vec<usize>>>, Rc<Vec<Size<Pixels>>>) {
        let zoom = self.zoom;
        let Some(doc) = &self.doc else {
            return (Rc::new(Vec::new()), Rc::new(Vec::new()));
        };
        let mut key = doc.pages.len() as u64;
        key = key.wrapping_mul(31).wrapping_add(self.spread as u64);
        key = key.wrapping_mul(31).wrapping_add(zoom.to_bits() as u64);
        for page in &doc.pages {
            key = key
                .wrapping_mul(31)
                .wrapping_add(page.width.to_bits() as u64);
            key = key
                .wrapping_mul(31)
                .wrapping_add(page.height.to_bits() as u64);
        }
        if let Some(cache) = &self.list_cache
            && cache.key == key
        {
            return (cache.rows.clone(), cache.sizes.clone());
        }
        let rows = self.rows();
        let row_width = |row: &[usize]| {
            row.iter().map(|&ix| doc.pages[ix].width).sum::<f32>()
                + PAGE_GAP * row.len().saturating_sub(1) as f32
        };
        let row_height = |row: &[usize]| {
            row.iter()
                .map(|&ix| doc.pages[ix].height)
                .fold(0.0f32, f32::max)
        };
        let sizes: Vec<Size<Pixels>> = rows
            .iter()
            .map(|row| {
                gpui_kit::size(
                    px(row_width(row) * zoom),
                    px(row_height(row) * zoom + PAGE_GAP),
                )
            })
            .collect();
        let rows = Rc::new(rows);
        let sizes = Rc::new(sizes);
        self.list_cache = Some(ListLayoutCache {
            key,
            rows: rows.clone(),
            sizes: sizes.clone(),
        });
        (rows, sizes)
    }

    fn render_content(&mut self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let theme = cx.theme();

        if self.doc.as_ref().is_none_or(|doc| doc.pages.is_empty()) {
            let message = if self.doc.is_none() {
                "No PDF loaded."
            } else {
                "This PDF has no pages."
            };
            return self.render_empty(message, theme.muted_foreground);
        }

        let zoom = self.zoom;
        let total = self.page_count();
        let viewport_width = self.scroll.bounds().size.width.as_f32();
        let dragging = self.drag.is_some();
        let (rows, sizes) = self.list_layout();
        // Measure the widest row, so the content can be wider than the
        // viewport and zoomed-in pages stay horizontally reachable.
        let measure_ix = {
            let doc = self.doc.as_ref().unwrap();
            rows.iter()
                .enumerate()
                .max_by(|a, b| {
                    let aw: f32 = a.1.iter().map(|&ix| doc.pages[ix].width).sum();
                    let bw: f32 = b.1.iter().map(|&ix| doc.pages[ix].width).sum();
                    aw.total_cmp(&bw)
                })
                .map(|(ix, _)| ix)
                .unwrap_or(0)
        };

        let border = theme.border;
        let muted = theme.muted_foreground;

        v_virtual_list(
            cx.entity(),
            "pdf-pages",
            sizes,
            move |this, range: Range<usize>, _window, cx| {
                range
                    .filter_map(|row_ix| {
                        let row = rows.get(row_ix)?.clone();
                        let pages = row
                            .iter()
                            .filter_map(|&ix| this.doc.as_ref()?.pages.get(ix).cloned())
                            .collect::<Vec<PdfPage>>();
                        let width: f32 = pages.iter().map(|page| page.width).sum::<f32>()
                            + PAGE_GAP * pages.len().saturating_sub(1) as f32;
                        let row_width = px((width * zoom).max(viewport_width));
                        let cards = pages
                            .iter()
                            .zip(&row)
                            .map(|(page, &page_ix)| {
                                let width = px(page.width * zoom);
                                let height = px(page.height * zoom);
                                match this.page_image(page_ix, cx) {
                                    Some(image) => {
                                        let mut card = div()
                                            .w(width)
                                            .h(height)
                                            .flex_none()
                                            .relative()
                                            .bg(rgb(0xffffff))
                                            .border_1()
                                            .border_color(border)
                                            .rounded_sm()
                                            .overflow_hidden();
                                        // Skip shadow + hit-test canvases while
                                        // dragging: they dominate paint cost on
                                        // every mouse-move refresh.
                                        if !dragging {
                                            let bounds = this.page_bounds.clone();
                                            card = card
                                                .child(
                                                    canvas(
                                                        move |area, _, _| {
                                                            bounds
                                                                .borrow_mut()
                                                                .insert(page_ix, area);
                                                        },
                                                        |_, _, _, _| {},
                                                    )
                                                    .absolute()
                                                    .size_full(),
                                                )
                                                .shadow_md();
                                        }
                                        card.child(img(image).w(width).h(height)).into_any_element()
                                    }
                                    None => div()
                                        .w(width)
                                        .h(height)
                                        .flex_none()
                                        .bg(rgb(0xffffff))
                                        .border_1()
                                        .border_color(border)
                                        .rounded_sm()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(format!("Page {}", page.page_number))
                                        .into_any_element(),
                                }
                            })
                            .collect::<Vec<_>>();
                        let first = pages.first()?.page_number;
                        Some(
                            v_flex()
                                .id(ElementId::NamedInteger("pdf-page".into(), row_ix as u64))
                                .w(row_width)
                                .items_center()
                                .pb(px(PAGE_GAP))
                                .child(
                                    cards
                                        .into_iter()
                                        .fold(h_flex().gap_2().items_start(), |row, card| {
                                            row.child(card)
                                        }),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(format!("{first} / {total}")),
                                )
                                .into_any_element(),
                        )
                    })
                    .collect::<Vec<_>>()
            },
        )
        .with_item_to_measure_index(measure_ix)
        .track_scroll(&self.scroll)
        .bg(theme.background)
        .into_any_element()
    }

    fn render_empty(&self, message: &str, muted: gpui_kit::Hsla) -> gpui_kit::AnyElement {
        v_flex()
            .id("pdf-empty")
            .size_full()
            .items_center()
            .justify_center()
            .p_8()
            .text_sm()
            .text_color(muted)
            .child(message.to_string())
            .into_any_element()
    }
}

impl Focusable for PdfView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PdfView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Resolve fit modes against the live viewport before drawing the
        // toolbar, so the percentage shown matches the pages below it.
        let viewport = self.scroll.bounds().size;
        let next_zoom = self.resolved_zoom(viewport);
        if next_zoom.to_bits() != self.zoom.to_bits() {
            self.list_cache = None;
        }
        self.zoom = next_zoom;
        // Name the current page before the toolbar, the pages and the
        // thumbnail strip are built, so all three agree within a frame.
        self.current_page = self.visible_page();
        // Bounds are recorded again for the pages drawn this frame — but not
        // while dragging, where we skip the hit-test canvases for speed.
        if self.drag.is_none() {
            self.page_bounds.borrow_mut().clear();
        }
        // The pages were laid out against last frame's viewport. When the
        // pane changes size under them (the thumbnail strip opening or
        // closing, a resized split) draw once more against the new one, or
        // the pages stay off-centre until the pointer next moves.
        let entity = cx.entity();

        v_flex()
            .on_prepaint(move |_, _, cx| {
                cx.defer(move |cx| {
                    entity.update(cx, |this, cx| {
                        if this.scroll.bounds().size != viewport {
                            cx.notify();
                        }
                    });
                });
            })
            .id("pdf-view")
            // So Ctrl+1/2/3 and other workspace bindings still match while the
            // preview holds focus (entity focus does not inherit parent context).
            .key_context(WORKSPACE_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .min_w_0()
            .min_h_0()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom_in(cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom_out(cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.reset_zoom(cx)))
            .on_action(
                cx.listener(|this, _: &TogglePdfThumbnails, _, cx| this.toggle_thumbnails(cx)),
            )
            // Workspace actions do not bubble out of this entity when it holds
            // focus — handle layout/sidebar here so Preview-only is escapable.
            .on_action(cx.listener(|_, action: &SetLayout, _, cx| {
                log::info!("ui: PdfView SetLayout -> {:?}", action.0);
                AppSettings::update(cx, |settings| {
                    settings.layout = action.0;
                    settings.focus_mode = false;
                });
            }))
            .on_action(cx.listener(|_, _: &ToggleFileSidebar, _, cx| {
                log::info!("ui: PdfView ToggleFileSidebar");
                AppSettings::update(cx, |settings| {
                    settings.file_sidebar = !settings.file_sidebar;
                });
            }))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_pinch(cx.listener(|this, event: &PinchEvent, _, cx| this.on_pinch(event, cx)))
            .child(self.render_toolbar(cx))
            // Double-click fits the page or zooms back out, the way the
            // diagram viewer does. On the content, not the whole view, so a
            // double-click on a toolbar button stays a double click on a
            // button.
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .flex()
                    // The strip is outside the click handler below: double
                    // clicking a thumbnail jumps to its page rather than
                    // zooming the pages.
                    .children(self.render_thumbnails(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                            .on_mouse_move(cx.listener(Self::on_mouse_move))
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                            .child(self.render_content(cx)),
                    ),
            )
    }
}

/// The scroll position (distance from the top) that brings `target` into a
/// viewport `height` tall now scrolled `top` down, putting it a third of the
/// way down; `None` when it is already comfortably in view.
fn reveal_offset(top: f32, height: f32, target: f32) -> Option<f32> {
    let margin = (height * 0.1).min(48.);
    if target >= top + margin && target <= top + height - margin {
        return None;
    }
    Some((target - height / 3.).max(0.))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The page named as current must follow the scroll position: at the top
    /// it is the first page, then the first page whose row reaches the top of
    /// the viewport, and the last page when scrolled past the end.
    #[test]
    fn reveal_moves_only_when_the_point_is_out_of_view() {
        assert_eq!(reveal_offset(0., 600., 300.), None);
        assert_eq!(reveal_offset(0., 600., 1200.), Some(1000.));
        assert_eq!(reveal_offset(1000., 600., 100.), Some(0.));
    }

    #[test]
    fn rendered_pages_keep_their_size_and_sheet() {
        let image = Arc::new(Image::from_bytes(ImageFormat::Svg, b"<svg/>".to_vec()));
        let doc = PdfDocument::from_rendered(vec![(100., 200., image.clone())]);
        assert_eq!(doc.page_count(), 1);
        assert_eq!(doc.pages()[0].height, 200.);
        assert!(Arc::ptr_eq(&doc.image(0).unwrap(), &image));
        assert_eq!(doc.clone_document().page_count(), 1);
    }

    #[test]
    fn the_current_page_follows_the_scroll_offset() {
        let rows = vec![vec![0], vec![1], vec![2]];
        let heights = vec![100., 100., 100.];
        assert_eq!(page_at_offset(&rows, &heights, 0.), 0);
        assert_eq!(page_at_offset(&rows, &heights, 99.9), 0);
        assert_eq!(page_at_offset(&rows, &heights, 100.), 1);
        assert_eq!(page_at_offset(&rows, &heights, 250.), 2);
        assert_eq!(page_at_offset(&rows, &heights, 1_000_000.), 2);
    }

    /// A two-page spread names the page it starts with, the way a reader does.
    #[test]
    fn a_spread_row_reports_its_first_page() {
        let rows = vec![vec![0, 1], vec![2, 3]];
        let heights = vec![100., 100.];
        assert_eq!(page_at_offset(&rows, &heights, 0.), 0);
        assert_eq!(page_at_offset(&rows, &heights, 100.), 2);
    }

    #[test]
    fn parses_and_measures_minimal_pdf() {
        // Generate a minimal valid PDF using Typst's world & compiler
        let markup = "= Hello Malgel PDF\n\nThis is a test of Hayro PDF rendering.";
        let doc = crate::typst_world::compile(
            markup.into(),
            std::collections::HashMap::new(),
            &crate::typst_world::BUNDLED_FONTS,
        )
        .expect("compile typst doc");

        let pdf_bytes =
            typst_pdf::pdf(&doc, &typst_pdf::PdfOptions::default()).expect("generate pdf bytes");

        let document = PdfDocument::from_bytes(pdf_bytes).expect("parse pdf");
        assert_eq!(document.page_count(), 1);
        assert_eq!(document.pages()[0].page_number, 1);
        assert!(document.pages()[0].width > 0.0);
        assert!(document.pages()[0].height > 0.0);
        // Pages are not converted on the UI thread: the cache starts empty and
        // fill-in happens via background work (see PdfView::page_image).
        assert!(document.image(0).is_none());
        let bytes = document
            .bytes
            .clone()
            .expect("bytes kept for off-thread render");
        let (image, elapsed) = PdfDocument::render_page_bytes(
            bytes,
            0,
            document.interpreter_settings.clone(),
            document.render_settings.clone(),
        )
        .expect("convert page 0 off the UI thread");
        assert!(!image.bytes.is_empty());
        assert!(elapsed.as_secs_f64() < 30., "{elapsed:?}");
        assert!(document.image(1).is_none());
    }

    #[test]
    fn reports_error_on_invalid_pdf_bytes() {
        let invalid = b"NOT A VALID PDF FILE".to_vec();
        assert!(PdfDocument::from_bytes(invalid).is_err());
    }

    #[test]
    fn thumbnails_keep_the_page_shape() {
        let page = PdfPage {
            index: 0,
            page_number: 1,
            width: 612.0,
            height: 792.0,
        };
        let (width, height) = thumb_size(&page, THUMB_WIDTH);
        assert_eq!(width, THUMB_WIDTH);
        assert!((height - THUMB_WIDTH * 792.0 / 612.0).abs() < 0.001);

        // A landscape page comes out wider than it is tall.
        let wide = PdfPage {
            width: 792.0,
            height: 612.0,
            ..page.clone()
        };
        let (_, short) = thumb_size(&wide, THUMB_WIDTH);
        assert!(short < width);

        // A page with no width does not divide by zero.
        let empty = PdfPage { width: 0.0, ..page };
        assert_eq!(thumb_size(&empty, THUMB_WIDTH), (THUMB_WIDTH, THUMB_WIDTH));
    }

    #[test]
    fn page_rows_pairs_pages_only_when_spread() {
        assert_eq!(page_rows(4, 1), vec![vec![0], vec![1], vec![2], vec![3]]);
        assert_eq!(page_rows(5, 2), vec![vec![0, 1], vec![2, 3], vec![4]]);
        assert_eq!(page_rows(1, 2), vec![vec![0]]);
        assert!(page_rows(0, 2).is_empty());
    }

    #[test]
    fn fit_width_shares_the_viewport_between_two_pages() {
        let page = (600.0, 800.0);
        let viewport = (1260.0, 900.0);
        // 1260 - 2 * 24 - 12 = 1200 usable, so one page fits at 2x…
        let single = fit_zoom(FitMode::Width, 1.0, &[vec![page], vec![page]], viewport);
        assert!((single - 2.0).abs() < 0.001, "got {single}");
        // …and a pair sits at nearly half of that, leaving room for the
        // page beside it.
        let spread = fit_zoom(FitMode::Width, 1.0, &[vec![page, page]], viewport);
        assert!((spread - 0.9836).abs() < 0.001, "got {spread}");
    }

    #[test]
    fn fit_page_measures_the_whole_row() {
        let page = (600.0, 800.0);
        // 1400 - 60 = 1340 wide, 700 - 48 = 652 high: the height binds.
        let zoom = fit_zoom(FitMode::Page, 1.0, &[vec![page, page]], (1400.0, 700.0));
        assert!((zoom - 0.815).abs() < 0.001, "got {zoom}");
    }

    #[test]
    fn a_document_opens_showing_the_whole_first_page() {
        // A4 in points, in a window that is narrower and shorter than the
        // page at 100%: opening must scale it down, not show its corner.
        let page = (595.0, 842.0);
        let viewport = (1173.0, 789.0);
        assert_eq!(DEFAULT_FIT, FitMode::Page);
        let zoom = fit_zoom(DEFAULT_FIT, zoom::DEFAULT, &[vec![page]], viewport);
        assert!(page.0 * zoom <= viewport.0, "width overflows at {zoom}");
        assert!(page.1 * zoom <= viewport.1, "height overflows at {zoom}");
        assert!(zoom < 1.0, "a page taller than the window must shrink");
    }

    #[test]
    fn custom_zoom_passes_through_the_clamp() {
        assert_eq!(fit_zoom(FitMode::Custom, 3.5, &[], (1000.0, 800.0)), 3.5);
        assert_eq!(
            fit_zoom(FitMode::Custom, 100.0, &[], (1000.0, 800.0)),
            zoom::MAX
        );
    }
}
