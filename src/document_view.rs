//! One open document: its text and where it lives, the editor and preview
//! showing it, and everything that follows its edits (statistics, scroll
//! sync, spelling, crash recovery).

use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use gpui_kit::{
    Action, App, AppContext as _, Bounds, ClipboardEntry, ClipboardItem, Context, DispatchPhase,
    Entity, EventEmitter, FocusHandle, Focusable as _, HighlightStyle, Image,
    InteractiveElement as _, IntoElement, KeyDownEvent, ListOffset, MouseMoveEvent,
    ParentElement as _, Pixels, Render, ScrollWheelEvent, SharedString, Styled as _, Subscription,
    Task, Window,
    base::{
        TextView, TextViewState,
        input::{Diagnostic, DiagnosticSeverity},
    },
    canvas,
    component::{
        ActiveTheme as _, ElementExt as _, RopeExt as _,
        button::{Button, ButtonVariants as _},
        clipboard::Clipboard,
        h_flex,
        input::{
            Copy, Cut, Editor, EditorState, Enter, IndentInline, InputEvent, OutdentInline, Paste,
            SelectAll, TabSize, TextDecoration, TextDecorationCollection,
        },
        native_menu::NativeMenu,
        resizable::{h_resizable, resizable_panel},
        v_flex,
    },
    div, point,
    prelude::FluentBuilder as _,
    px, rems,
};

use crate::{
    actions::{AddToDictionary, CopyRichText, ReplaceMisspelling, ZoomIn, ZoomOut},
    analysis::{Analysis, content_hash},
    document::{LineEnding, display_name},
    file_tree::TreeDrag,
    format::{self, Edit},
    images, preview_ext,
    session::{self, DiskStamp, Recovery},
    settings::Layout,
    smart_edit, spell,
    themes::AppSettings,
};

/// Widest the text column grows before extra width becomes margin.
const READABLE_WIDTH_REMS: f32 = 46.;
/// How long typing must pause before statistics and the outline refresh.
const ANALYSIS_DEBOUNCE: Duration = Duration::from_millis(120);
/// How long typing must pause before spelling is checked again.
/// Past this size a Markdown document is "large": its preview shows only the
/// beginning, follows edits once typing pauses rather than on every key,
/// and spelling is not checked, so typing stays responsive.
pub const LARGE_DOCUMENT_BYTES: usize = crate::analysis::FULL_PARSE_LIMIT;
/// How much of a large document the preview renders.
const LARGE_PREVIEW_BYTES: usize = 256 * 1024;
/// The analysis debounce for a large document: a pass over megabytes of
/// text should not run between every two keys.
const LARGE_ANALYSIS_DEBOUNCE: Duration = Duration::from_millis(600);
const SPELL_DEBOUNCE: Duration = Duration::from_millis(400);
/// How long typing must pause before unsaved changes are written aside.
const RECOVERY_DEBOUNCE: Duration = Duration::from_secs(1);
/// Estimated jumps allowed before giving up on landing a target exactly.
const MAX_EDITOR_SCROLL_ATTEMPTS: u8 = 4;
/// How much of the text outside the current paragraph fades in focus mode.
const FOCUS_FADE: f32 = 0.7;

/// Which pane the other one follows while scroll sync is on.
///
/// The pane under the pointer, or the editor while typing, drives: its
/// scrolling moves the other pane, and scrolling it causes (the follower's
/// programmatic moves) never feed back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ScrollDriver {
    #[default]
    Editor,
    Preview,
}

/// A source position the editor is being scrolled to.
///
/// Lines off screen have no layout to measure, so the editor first jumps to
/// an estimate; once the line is laid out, the next pass lands on it exactly.
#[derive(Clone, Copy, Debug)]
struct EditorScrollTarget {
    /// Zero-based source line, with the fraction through it.
    line: f32,
    attempts: u8,
}

/// Something the workspace shows or acts on changed.
pub enum DocumentEvent {
    /// A message for the user, such as a failed image paste.
    Notify(SharedString),
}

pub struct DocumentView {
    editor: Entity<EditorState>,
    preview: Entity<TextViewState>,
    pdf: Option<Entity<crate::pdf_view::PdfView>>,
    image: Option<Entity<crate::image_view::ImageView>>,
    /// The Typst document a `.typ` file's preview is: its source and the
    /// pages its last compile produced, so the preview follows the source as
    /// it changes on disk.
    typst: Option<crate::typst_preview::TypstPreview>,
    /// Parser configuration for the preview; built once so the preview can
    /// tell that it is unchanged between frames.
    markdown_extensions: gpui_kit::component::text::MarkdownExtensions,

    path: Option<PathBuf>,
    line_ending: LineEnding,
    /// The line ending last read from or written to disk, so that changing
    /// only the line ending still counts as an unsaved change.
    saved_line_ending: LineEnding,
    /// The text last read from or written to disk.
    saved_hash: u64,
    dirty: bool,
    /// The file as last read or written, to notice other programs' changes.
    disk: Option<DiskStamp>,
    /// The current source, shared with the preview and background analysis.
    source: SharedString,
    revision: usize,
    analysis: Arc<Analysis>,
    analysis_task: Option<Task<()>>,
    /// The indentation detected when the text was loaded.
    indent: crate::document::Indent,
    /// Whether the preview shows only the start of a large document.
    preview_limited: bool,
    /// Past `LARGE_DOCUMENT_BYTES`: the editor's rope is the only copy of
    /// the text (`source` stays empty) and analysis streams over the rope.
    large: bool,
    /// Highlighting, preview, outline and spell checking are off, as VS Code
    /// does for a large file, until "Enable anyway" in the info bar.
    reduced: bool,
    /// A heading to show once the outline is known, from following a link.
    pending_anchor: Option<String>,

    recovery_id: String,
    recovery_written: bool,
    recovery_task: Option<Task<()>>,

    misspellings: Arc<Vec<spell::Misspelling>>,
    spell_task: Option<Task<()>>,
    /// Spelling was checked with this dictionary at this revision.
    spell_checked: Option<(usize, String)>,

    /// The caret as of the editor's last change, for code that runs while
    /// the editor is busy (its context menu).
    last_cursor: usize,
    focus_decorations: Option<TextDecorationCollection>,
    /// The paragraph left undimmed in focus mode.
    focus_block: Option<Range<usize>>,

    scroll_driver: ScrollDriver,
    /// Editor scroll position the preview was last aligned with.
    synced_editor_scroll: Option<(usize, Pixels)>,
    /// Preview scroll position the editor was last aligned with.
    synced_preview_scroll: Option<ListOffset>,
    editor_scroll_target: Option<EditorScrollTarget>,
    /// Row count of the preview when it was last aligned.
    synced_preview_rows: usize,
    /// Measured widths used to center the text column.
    editor_width: Pixels,
    preview_width: Pixels,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DocumentEvent> for DocumentView {}

impl DocumentView {
    /// A document showing `text`, saved at `path` when it has one.
    pub fn new(
        text: String,
        path: Option<PathBuf>,
        line_ending: LineEnding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = AppSettings::get(cx).clone();
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("markdown")
                .line_number(settings.line_numbers)
                .soft_wrap(settings.soft_wrap)
                .tab_size(TabSize {
                    tab_size: 2,
                    hard_tabs: false,
                })
                .indent_guides(false)
                .searchable(true)
                .placeholder("Start writing…")
        });
        let preview = cx.new(|cx| TextViewState::markdown("", cx));

        let subscriptions = vec![
            cx.subscribe_in(&editor, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.on_text_changed(true, cx);
                }
            }),
            // The editor notifies when it scrolls or its caret moves.
            cx.observe(&editor, |this, editor, cx| {
                this.last_cursor = editor.read(cx).cursor();
                this.sync_preview(false, cx);
                this.update_focus_dimming(cx);
            }),
            // A re-parse can rebuild the preview's rows, which resets its
            // scroll position; realign once the new rows exist.
            cx.observe(&preview, |this, preview, cx| {
                let rows = preview.read(cx).list_state().item_count();
                if rows != this.synced_preview_rows {
                    this.sync_preview(true, cx);
                }
            }),
            cx.observe_global_in::<AppSettings>(window, |this, window, cx| {
                this.apply_settings(window, cx);
                this.sync_preview(true, cx);
                cx.notify();
            }),
        ];

        let mut this = Self {
            editor,
            pdf: None,
            image: None,
            typst: None,
            markdown_extensions: preview_ext::markdown_extensions(&preview),
            preview,
            path: None,
            line_ending: LineEnding::Lf,
            saved_line_ending: LineEnding::Lf,
            saved_hash: content_hash(""),
            dirty: false,
            disk: None,
            source: SharedString::default(),
            revision: 0,
            analysis: Arc::default(),
            analysis_task: None,
            indent: crate::document::Indent::DEFAULT,
            preview_limited: false,
            large: false,
            reduced: false,
            pending_anchor: None,
            recovery_id: session::new_recovery_id(),
            recovery_written: false,
            recovery_task: None,
            misspellings: Arc::default(),
            spell_task: None,
            spell_checked: None,
            last_cursor: 0,
            focus_decorations: None,
            focus_block: None,
            scroll_driver: ScrollDriver::Editor,
            synced_editor_scroll: None,
            synced_preview_scroll: None,
            editor_scroll_target: None,
            synced_preview_rows: 0,
            editor_width: px(0.),
            preview_width: px(0.),
            _subscriptions: subscriptions,
        };
        this.load_text(text, path, line_ending, window, cx);
        this
    }

    // ---------------------------------------------------------------------
    // State

    pub fn editor(&self) -> &Entity<EditorState> {
        &self.editor
    }

    pub fn preview(&self) -> &Entity<TextViewState> {
        &self.preview
    }

    /// The focus handle of the PDF or image viewer this document shows, if it
    /// shows one. Those two replace the editor and the preview in `render`, so
    /// the keyboard has to go to them: focusing a view that is not in the tree
    /// leaves every shortcut dead, which is what happened while a PDF was up.
    pub fn media_focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        if let Some(pdf) = &self.pdf {
            return Some(pdf.read(cx).focus_handle(cx));
        }
        self.image
            .as_ref()
            .map(|image| image.read(cx).focus_handle(cx))
    }

    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    /// Point this document at a new path because its file moved in the
    /// file tree; the content is unchanged, so only the location and the
    /// stamp used to notice other programs' changes follow.
    pub fn retarget(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.path = Some(path);
        self.disk = self.path.as_deref().and_then(DiskStamp::of);
        cx.notify();
    }

    /// A copy of this document as it is right now — same text, path and
    /// unsaved state — for a tab drop that copies (VS Code duplicates
    /// tabs, dirty buffers included).
    pub fn duplicate(&self, window: &mut Window, cx: &mut App) -> Entity<DocumentView> {
        let text = self.editor.read(cx).value().to_string();
        let path = self.path.clone();
        let line_ending = self.line_ending;
        let dirty = self.dirty;
        let saved_hash = self.saved_hash;
        cx.new(|cx| {
            let mut copy = Self::new(text, path, line_ending, window, cx);
            if dirty {
                // Keep the copy's unsaved state true to the source:
                // restore the saved hash so the current text counts as
                // modified, the way `mark_missing` stamps the field.
                copy.saved_hash = saved_hash;
                copy.dirty = true;
            }
            copy
        })
    }

    /// Insert a Markdown link to `path` at the caret, for a file dragged
    /// from the sidebar into the text (VS Code's drop-into-editor).
    pub fn insert_drop_link(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if !Self::shows_editor(cx) {
            return;
        }
        let document_dir = self.path.as_deref().and_then(Path::parent);
        let link = drop_link(path, document_dir);
        let range = self.editor.read(cx).selected_range();
        let end = range.start.saturating_add(link.len());
        self.apply(
            Edit {
                range,
                text: link,
                selection: end..end,
            },
            window,
            cx,
        );
        self.editor
            .update(cx, |editor, cx| editor.focus(window, cx));
    }

    /// Files dropped onto the editor text insert Markdown links to them.
    fn on_drop_tree_files(&mut self, drag: &TreeDrag, window: &mut Window, cx: &mut Context<Self>) {
        for path in &drag.paths {
            if !path.is_dir() {
                self.insert_drop_link(path, window, cx);
            }
        }
    }

    /// A document displaying an image (PNG, JPEG, SVG, …) with zoom.
    pub fn new_image(
        path: PathBuf,
        image: Arc<Image>,
        width: f32,
        height: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let image = cx.new(|cx| {
            crate::image_view::ImageView::new(Some(path.clone()), image, width, height, cx)
        });
        let editor = cx.new(|cx| EditorState::new(window, cx));
        let preview = cx.new(|cx| TextViewState::markdown("", cx));
        let markdown_extensions = preview_ext::markdown_extensions(&preview);
        Self {
            editor,
            preview,
            pdf: None,
            image: Some(image),
            typst: None,
            markdown_extensions,
            path: Some(path),
            line_ending: LineEnding::Lf,
            saved_line_ending: LineEnding::Lf,
            saved_hash: 0,
            dirty: false,
            disk: None,
            source: "".into(),
            revision: 0,
            analysis: Arc::new(Analysis::default()),
            analysis_task: None,
            indent: crate::document::Indent::DEFAULT,
            preview_limited: false,
            large: false,
            reduced: false,
            pending_anchor: None,
            recovery_id: session::new_recovery_id(),
            recovery_written: false,
            recovery_task: None,
            misspellings: Arc::default(),
            spell_task: None,
            spell_checked: None,
            last_cursor: 0,
            focus_decorations: None,
            focus_block: None,
            scroll_driver: ScrollDriver::Editor,
            synced_editor_scroll: None,
            synced_preview_scroll: None,
            editor_scroll_target: None,
            synced_preview_rows: 0,
            editor_width: px(0.),
            preview_width: px(0.),
            _subscriptions: Vec::new(),
        }
    }

    pub fn new_pdf(
        path: PathBuf,
        doc: crate::pdf_view::PdfDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let pdf = cx.new(|cx| crate::pdf_view::PdfView::new(Some(path.clone()), doc, cx));
        let editor = cx.new(|cx| EditorState::new(window, cx));
        let preview = cx.new(|cx| TextViewState::markdown("", cx));
        let markdown_extensions = preview_ext::markdown_extensions(&preview);
        Self {
            editor,
            preview,
            markdown_extensions,
            pdf: Some(pdf),
            image: None,
            typst: None,
            path: Some(path),
            line_ending: LineEnding::Lf,
            saved_line_ending: LineEnding::Lf,
            saved_hash: 0,
            dirty: false,
            disk: None,
            source: "".into(),
            revision: 0,
            analysis: Arc::new(Analysis::default()),
            analysis_task: None,
            indent: crate::document::Indent::DEFAULT,
            preview_limited: false,
            large: false,
            reduced: false,
            pending_anchor: None,
            recovery_id: session::new_recovery_id(),
            recovery_written: false,
            recovery_task: None,
            misspellings: Arc::default(),
            spell_task: None,
            spell_checked: None,
            last_cursor: 0,
            focus_decorations: None,
            focus_block: None,
            scroll_driver: ScrollDriver::Editor,
            synced_editor_scroll: None,
            synced_preview_scroll: None,
            editor_scroll_target: None,
            synced_preview_rows: 0,
            editor_width: px(0.),
            preview_width: px(0.),
            _subscriptions: Vec::new(),
        }
    }

    /// A document whose preview is the Typst compiler's output for `source`.
    ///
    /// The pages are handed to the same PDF viewer a `.pdf` file uses, so a
    /// Typst document reads exactly like a PDF one: same scrolling, same
    /// zoom, same thumbnail strip, same toolbar.
    pub fn new_typst(
        source: PathBuf,
        compiled: crate::typst_preview::TypstPreview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let pdf = cx
            .new(|cx| crate::pdf_view::PdfView::new(Some(source.clone()), compiled.document(), cx));
        // The files as they are right now, so the first watcher pass has
        // something to compare against instead of reporting every typst
        // document as changed.
        let disk = Some(compiled.stamp());
        let editor = cx.new(|cx| EditorState::new(window, cx));
        let preview = cx.new(|cx| TextViewState::markdown("", cx));
        let markdown_extensions = preview_ext::markdown_extensions(&preview);
        Self {
            editor,
            preview,
            markdown_extensions,
            pdf: Some(pdf),
            image: None,
            typst: Some(compiled),
            path: Some(source),
            line_ending: LineEnding::Lf,
            saved_line_ending: LineEnding::Lf,
            saved_hash: 0,
            dirty: false,
            disk,
            source: "".into(),
            revision: 0,
            analysis: Arc::new(Analysis::default()),
            analysis_task: None,
            indent: crate::document::Indent::DEFAULT,
            preview_limited: false,
            large: false,
            reduced: false,
            pending_anchor: None,
            recovery_id: session::new_recovery_id(),
            recovery_written: false,
            recovery_task: None,
            misspellings: Arc::default(),
            spell_task: None,
            spell_checked: None,
            last_cursor: 0,
            focus_decorations: None,
            focus_block: None,
            scroll_driver: ScrollDriver::Editor,
            synced_editor_scroll: None,
            synced_preview_scroll: None,
            editor_scroll_target: None,
            synced_preview_rows: 0,
            editor_width: px(0.),
            preview_width: px(0.),
            _subscriptions: Vec::new(),
        }
    }

    /// A document whose preview is a Typst source's compiled pages, so the
    /// viewer the preview hands its work to.
    pub fn is_typst(&self) -> bool {
        self.typst.is_some()
    }

    /// Recompile the Typst source this document previews, showing the new
    /// pages. A failed compile leaves the pages on screen and reports why.
    /// Returns `false` when this document is not a Typst preview, or when the
    /// source no longer compiles.
    pub fn refresh_typst(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(compiled) = &mut self.typst else {
            return false;
        };
        if compiled.refresh().is_err() {
            return false;
        }
        // Keep the reader's place: the new pages replace the old in the
        // viewer, which holds on to its zoom, spread, scroll and thumbnails.
        if let Some(pdf) = &self.pdf {
            pdf.update(cx, |pdf, cx| pdf.reload(compiled.document(), cx));
        }
        true
    }

    /// The paths whose changes should refresh a Typst preview: its source and
    /// everything the source named.
    pub fn watched_paths(&self) -> Vec<PathBuf> {
        self.typst
            .as_ref()
            .map(|compiled| compiled.watched().to_vec())
            .unwrap_or_default()
    }

    /// The language the status bar names: empty for a Markdown document, and
    /// the source's own language for a preview that is not Markdown's. A Typst
    /// document's pages are Typst's output, so saying "Markdown" beside them
    /// would name the wrong thing.
    pub fn language_label(&self) -> SharedString {
        if self.reduced {
            return "Plain Text".into();
        }
        crate::document::language_name(self.path.as_deref()).into()
    }

    /// A Typst source opened as text because it did not compile, which
    /// turns into its pages once it does.
    pub fn is_typst_source(&self) -> bool {
        self.typst.is_none()
            && self
                .path
                .as_deref()
                .is_some_and(crate::document::is_typst_path)
    }

    /// The indentation the status bar names.
    pub fn indent(&self) -> crate::document::Indent {
        self.indent
    }

    /// Whether this text document gets the rendered preview beside it:
    /// Markdown does, source code and other text open in the editor alone.
    pub fn has_preview(&self) -> bool {
        !self.reduced && crate::document::has_markdown_preview(self.path.as_deref())
    }

    /// Turn off what costs memory per byte, the way VS Code treats a large
    /// file: highlighting, the preview, the outline and spell checking.
    pub fn reduce(&mut self, cx: &mut Context<Self>) {
        self.reduced = true;
        self.preview_limited = false;
        self.preview
            .update(cx, |preview, cx| preview.set_text("", cx));
        self.editor
            .update(cx, |editor, cx| editor.set_highlighter("text", cx));
        self.spell_task = None;
        self.misspellings = Arc::default();
        self.show_misspellings(cx);
        self.schedule_analysis(false, cx);
        cx.notify();
    }

    /// "Enable anyway": highlighting, the preview (of the beginning) and the
    /// outline come back for a large file.
    pub fn enable_anyway(&mut self, cx: &mut Context<Self>) {
        self.reduced = false;
        let language = self.highlight_language();
        self.editor
            .update(cx, |editor, cx| editor.set_highlighter(language, cx));
        self.on_text_changed(false, cx);
    }

    /// The tree-sitter language the editor highlights this file with.
    fn highlight_language(&self) -> &'static str {
        self.path
            .as_deref()
            .filter(|path| !crate::document::is_markdown_path(path))
            .and_then(crate::document::language_tag)
            .unwrap_or("markdown")
    }

    /// The whole text as one string, copied out of the editor for a large
    /// document (which keeps no other copy); the caller should drop it soon.
    pub fn text_now(&self, cx: &App) -> SharedString {
        if self.large {
            self.editor.read(cx).value()
        } else {
            self.source.clone()
        }
    }

    /// Whether the preview shows only the start of a large document.
    pub fn preview_limited(&self) -> bool {
        self.preview_limited
    }

    /// What the status bar says about a viewer instead of text statistics:
    /// the page and zoom of a PDF, the size and zoom of an image.
    pub fn media_status(&self, cx: &App) -> Vec<String> {
        if let Some(pdf) = &self.pdf {
            let pdf = pdf.read(cx);
            let total = pdf.page_count();
            let page = (pdf.current_page() + 1).min(total.max(1));
            return vec![
                format!("Page {page} of {total}"),
                format!("{:.0}%", pdf.zoom() * 100.),
            ];
        }
        if let Some(image) = &self.image {
            let image = image.read(cx);
            let (width, height) = image.natural_size();
            return vec![
                format!("{width:.0} × {height:.0}"),
                format!("{:.0}%", image.zoom() * 100.),
            ];
        }
        Vec::new()
    }

    pub fn is_dirty(&self) -> bool {
        if self.pdf.is_some() || self.image.is_some() {
            return false;
        }
        // A change of line ending alone is an unsaved change, the way VS Code
        // marks the file modified when its EOL is switched.
        self.dirty || self.line_ending != self.saved_line_ending
    }

    /// Whether this document shows a media view (PDF or image) rather than
    /// editable text, so zoom commands act on the viewer instead of the UI.
    pub fn is_media(&self) -> bool {
        self.pdf.is_some() || self.image.is_some()
    }

    /// Forward a zoom step (positive zooms in, negative out, zero resets) to
    /// the PDF or image viewer this document shows.
    pub fn zoom_media(&mut self, step: f32, cx: &mut Context<Self>) {
        if let Some(pdf) = &self.pdf {
            pdf.update(cx, |pdf, cx| match step.partial_cmp(&0.) {
                Some(std::cmp::Ordering::Greater) => pdf.zoom_in(cx),
                Some(std::cmp::Ordering::Less) => pdf.zoom_out(cx),
                _ => pdf.reset_zoom(cx),
            });
        } else if let Some(image) = &self.image {
            image.update(cx, |image, cx| match step.partial_cmp(&0.) {
                Some(std::cmp::Ordering::Greater) => image.zoom_in(cx),
                Some(std::cmp::Ordering::Less) => image.zoom_out(cx),
                _ => image.reset_zoom(cx),
            });
        }
    }

    pub fn analysis(&self) -> &Arc<Analysis> {
        &self.analysis
    }

    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// Switch the line ending the document is saved with — LF or CRLF — the
    /// way clicking the EOL item in VS Code's status bar does. The text is
    /// unchanged (it is held normalised to `\n`), so the document is only
    /// marked unsaved; saving writes the new ending.
    pub fn set_line_ending(&mut self, line_ending: LineEnding, cx: &mut Context<Self>) {
        if self.line_ending == line_ending {
            return;
        }
        self.line_ending = line_ending;
        cx.notify();
    }

    /// The file as last read or written. A Typst preview's stamp covers its
    /// source and every file that source named, so one comparison notices a
    /// change to any of them; it is set when the preview is compiled and
    /// updated when the watcher accepts the files as they are.
    pub fn disk_stamp(&self) -> Option<DiskStamp> {
        if self.typst.is_some() {
            return Some(self.disk.unwrap_or_else(DiskStamp::of_nowhere));
        }
        self.disk
    }

    /// The stamp a Typst preview's next watcher pass compares against.
    pub fn watch_stamp(&self) -> DiskStamp {
        self.typst
            .as_ref()
            .map(|compiled| compiled.stamp())
            .unwrap_or(DiskStamp::of_nowhere())
    }

    /// Accept the stamp the watcher has just read, so the next pass does not
    /// report the same change again.
    pub fn accept_watch_stamp(&mut self, stamp: DiskStamp) {
        if self.typst.is_some() {
            self.disk = Some(stamp);
        }
    }

    pub fn saved_hash(&self) -> u64 {
        self.saved_hash
    }

    pub fn recovery_id(&self) -> Option<&str> {
        self.recovery_written.then_some(self.recovery_id.as_str())
    }

    pub fn name(&self) -> String {
        display_name(self.path.as_ref())
    }

    pub fn dir(&self) -> Option<PathBuf> {
        self.path
            .as_ref()
            .and_then(|path| path.parent())
            .map(Path::to_path_buf)
    }

    /// An untitled document nobody has typed into, which opening a file may
    /// replace without asking.
    pub fn is_blank(&self) -> bool {
        self.path.is_none() && !self.is_dirty()
    }

    pub fn cursor(&self, cx: &App) -> usize {
        self.editor.read(cx).cursor()
    }

    pub fn set_cursor(&mut self, offset: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let offset = floor_char_boundary(&editor.value(), offset);
            editor.set_selected_range(offset..offset, cx);
            let position = editor.text().offset_to_position(offset);
            editor.set_cursor_position(position, window, cx);
        });
    }

    /// Move the caret to the start of `line` (1-based), the way VS Code's Go
    /// to Line (Ctrl+G) does. A line past the end lands on the last line, so
    /// typing too large a number still moves the caret rather than doing
    /// nothing.
    pub fn go_to_line(&mut self, line: usize, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor.read(cx);
        let rope = editor.text();
        let len = rope.len();
        let row = (line.max(1) - 1).min(rope.lines_len().saturating_sub(1));
        let offset = rope.line_start_offset(row);
        self.set_cursor(offset.min(len), window, cx);
    }

    /// Replace the document with `text` read from `path`.
    fn load_text(
        &mut self,
        text: String,
        path: Option<PathBuf>,
        line_ending: LineEnding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.saved_hash = content_hash(&text);
        self.disk = path.as_deref().and_then(DiskStamp::of);
        self.path = path;
        self.line_ending = line_ending;
        self.saved_line_ending = line_ending;
        self.dirty = false;
        self.synced_editor_scroll = None;
        // Like VS Code: the editor highlights the file's own language and
        // indents the way the file already does.
        // A large file opens with highlighting, the preview, the outline and
        // spell checking off, the way VS Code opens one: re-parsing
        // megabytes costs far more memory than the file itself. The info bar
        // above the editor turns them back on.
        self.large = is_large_text(&text);
        self.reduced = self.large;
        if self.large {
            self.source = SharedString::default();
        }
        let language = if self.reduced {
            "text"
        } else {
            self.highlight_language()
        };
        let indent = crate::document::detect_indent_for(&text, self.path.as_deref());
        self.indent = indent;
        self.editor.update(cx, |editor, cx| {
            editor.set_highlighter(language, cx);
            editor.set_tab_size(
                TabSize {
                    tab_size: indent.size,
                    hard_tabs: indent.hard_tabs,
                },
                cx,
            );
            editor.set_value(text, window, cx)
        });
        // `set_value` does not emit a change event.
        self.on_text_changed(false, cx);
        self.dirty = false;
    }

    /// Restore unsaved text recovered after a crash; it stays unsaved.
    pub fn restore_unsaved(
        &mut self,
        id: String,
        recovery: Recovery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let saved = recovery
            .path
            .as_deref()
            .and_then(|path| crate::document::load(path).ok());
        self.path = recovery.path;
        self.disk = self.path.as_deref().and_then(DiskStamp::of);
        self.line_ending = if recovery.crlf {
            LineEnding::CrLf
        } else {
            LineEnding::Lf
        };
        self.saved_line_ending = saved
            .as_ref()
            .map_or(LineEnding::Lf, |saved| saved.line_ending);
        self.saved_hash = saved.map_or(content_hash(""), |saved| content_hash(&saved.text));
        self.recovery_id = id;
        self.recovery_written = true;
        self.editor
            .update(cx, |editor, cx| editor.set_value(recovery.text, window, cx));
        self.on_text_changed(false, cx);
    }

    /// Show `text`, which another program saved to the document's file,
    /// keeping the caret where it was.
    pub fn reload(
        &mut self,
        text: String,
        line_ending: LineEnding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cursor = self.cursor(cx);
        let path = self.path.clone();
        self.load_text(text, path, line_ending, window, cx);
        self.set_cursor(cursor, window, cx);
        self.discard_recovery();
    }

    /// Accept the file on disk as it is now without reloading it.
    pub fn set_disk_stamp(&mut self, stamp: Option<DiskStamp>) {
        self.disk = stamp;
    }

    /// The file was deleted or moved away: the text only exists here now.
    pub fn mark_missing(&mut self, cx: &mut Context<Self>) {
        self.disk = None;
        self.saved_hash = content_hash("\u{0}missing");
        self.dirty = true;
        self.schedule_recovery(cx);
        cx.notify();
    }

    fn on_text_changed(&mut self, debounce: bool, cx: &mut Context<Self>) {
        if !self.large && self.editor.read(cx).text().len() > LARGE_DOCUMENT_BYTES {
            // Grew past the limit: stop keeping a second copy.
            self.large = true;
            self.source = SharedString::default();
        }
        if self.large {
            // The rope is the only copy: nothing is copied per key, the
            // analysis streams over the rope once typing pauses.
            if self.has_preview() {
                if !debounce {
                    let preview_source = large_preview(self.editor.read(cx).text());
                    self.preview
                        .update(cx, |preview, cx| preview.set_text(&preview_source, cx));
                }
                self.preview_limited = true;
            } else if !debounce {
                self.preview
                    .update(cx, |preview, cx| preview.set_text("", cx));
            }
            self.dirty = true;
            self.set_scroll_driver(ScrollDriver::Editor);
            self.revision += 1;
            self.focus_block = None;
            self.schedule_analysis(debounce, cx);
            cx.notify();
            return;
        }
        let source = self.editor.read(cx).value();
        // Non-markdown text files render as one highlighted code block: the
        // extension picks the tree-sitter language for the preview's
        // highlighter. Markdown keeps its normal document rendering.
        // Source code and other text is not previewed at all, as in VS
        // Code: the editor is the whole view.
        if self.has_preview() {
            let preview_source = crate::preview_ext::normalize_tab_containers(&source);
            self.preview
                .update(cx, |preview, cx| preview.set_text(&preview_source, cx));
            self.preview_limited = false;
        } else if !debounce {
            self.preview
                .update(cx, |preview, cx| preview.set_text("", cx));
        }
        self.source = source;
        // Assume an edit until the analysis can tell the text matches disk.
        self.dirty = true;
        self.set_scroll_driver(ScrollDriver::Editor);
        self.revision += 1;
        self.focus_block = None;
        self.schedule_analysis(debounce, cx);
        cx.notify();
    }

    /// Recompute statistics, the outline and the block index off the UI
    /// thread once typing pauses. Stale results are discarded.
    fn schedule_analysis(&mut self, debounce: bool, cx: &mut Context<Self>) {
        let revision = self.revision;
        let delay = if self.large {
            LARGE_ANALYSIS_DEBOUNCE
        } else {
            ANALYSIS_DEBOUNCE
        };
        self.analysis_task = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(delay).await;
            }
            // A large document lends a clone of its rope, which shares the
            // editor's nodes rather than copying the text.
            let Ok(input) = this.update(cx, |this, cx| {
                if this.large {
                    Err((this.editor.read(cx).text().clone(), !this.reduced))
                } else {
                    Ok(this.source.clone())
                }
            }) else {
                return;
            };
            let analysis = cx
                .background_executor()
                .spawn(async move {
                    match input {
                        Ok(source) => Analysis::of(&source),
                        Err((rope, outline)) => Analysis::of_chunks(rope.chunks(), outline),
                    }
                })
                .await;
            _ = this.update(cx, |this, cx| {
                if this.revision != revision {
                    return;
                }
                this.dirty = analysis.content_hash != this.saved_hash;
                this.analysis = Arc::new(analysis);
                this.analysis_task = None;
                if this.preview_limited && debounce {
                    let preview_source = large_preview(this.editor.read(cx).text());
                    this.preview
                        .update(cx, |preview, cx| preview.set_text(&preview_source, cx));
                }
                if this.dirty {
                    this.schedule_recovery(cx);
                } else {
                    this.discard_recovery();
                }
                this.schedule_spell_check(true, cx);
                this.sync_preview(true, cx);
                cx.notify();
            });
        }));
    }

    // ---------------------------------------------------------------------
    // Saving and recovery

    /// Write the current text to `path`, which becomes the document's.
    pub fn write_to(&mut self, path: &Path, cx: &mut Context<Self>) -> std::io::Result<()> {
        if self.pdf.is_some() {
            return Ok(());
        }
        let text = self.editor.read(cx).value();
        crate::document::save(path, &text, self.line_ending)?;
        self.saved_line_ending = self.line_ending;
        self.saved_hash = content_hash(&text);
        self.dirty = !self.large && self.source.as_ref() != text.as_ref();
        self.path = Some(path.to_path_buf());
        self.disk = DiskStamp::of(path);
        if !self.dirty {
            self.discard_recovery();
        }
        cx.notify();
        Ok(())
    }

    /// Write unsaved changes aside once typing pauses, so a crash can't lose
    /// them.
    fn schedule_recovery(&mut self, cx: &mut Context<Self>) {
        let revision = self.revision;
        self.recovery_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RECOVERY_DEBOUNCE).await;
            let Ok(Some((id, recovery))) = this.read_with(cx, |this, cx| {
                (this.dirty && this.revision == revision).then(|| {
                    (
                        this.recovery_id.clone(),
                        Recovery {
                            path: this.path.clone(),
                            text: if this.large {
                                this.editor.read(cx).text().to_string()
                            } else {
                                this.source.to_string()
                            },
                            crlf: this.line_ending == LineEnding::CrLf,
                        },
                    )
                })
            }) else {
                return;
            };
            let written = cx
                .background_executor()
                .spawn(async move { session::write_recovery(&id, &recovery).is_ok() })
                .await;
            _ = this.update(cx, |this, _| {
                this.recovery_written |= written;
                this.recovery_task = None;
            });
        }));
    }

    /// Forget the unsaved changes written aside: they were saved or thrown
    /// away.
    pub fn discard_recovery(&mut self) {
        self.recovery_task = None;
        if self.recovery_written {
            session::remove_recovery(&self.recovery_id);
            self.recovery_written = false;
        }
    }

    // ---------------------------------------------------------------------
    // Settings

    fn apply_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = AppSettings::get(cx).clone();
        self.editor.update(cx, |editor, cx| {
            editor.set_soft_wrap(settings.soft_wrap, window, cx);
            editor.set_line_number(settings.line_numbers && !settings.focus_mode, window, cx);
            editor.set_folding(!settings.focus_mode, window, cx);
        });
        self.schedule_spell_check(false, cx);
        self.focus_block = None;
        self.update_focus_dimming(cx);
    }

    // ---------------------------------------------------------------------
    // Spelling

    fn spell_language(cx: &App) -> Option<String> {
        let settings = AppSettings::get(cx);
        settings.spell_check.then(|| {
            settings
                .spell_language
                .clone()
                .unwrap_or_else(spell::default_language)
        })
    }

    /// Check spelling in the background, after a pause when `debounce`.
    fn schedule_spell_check(&mut self, debounce: bool, cx: &mut Context<Self>) {
        let Some(language) = Self::spell_language(cx) else {
            if self.spell_checked.take().is_some() || !self.misspellings.is_empty() {
                self.spell_task = None;
                self.misspellings = Arc::default();
                self.show_misspellings(cx);
            }
            return;
        };
        if self.reduced || self.large {
            // Checking megabytes on every pause would keep a core busy.
            self.spell_task = None;
            return;
        }
        if self.spell_checked.as_ref() == Some(&(self.revision, language.clone())) {
            return;
        }
        let revision = self.revision;
        let source = self.source.clone();
        self.spell_task = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(SPELL_DEBOUNCE).await;
            }
            let checked_language = language.clone();
            let found = cx
                .background_executor()
                .spawn(async move {
                    spell::dictionary(&language)
                        .map(|dictionary| spell::check(&source, &dictionary))
                        .unwrap_or_default()
                })
                .await;
            _ = this.update(cx, |this, cx| {
                if this.revision != revision {
                    return;
                }
                this.spell_task = None;
                this.spell_checked = Some((revision, checked_language));
                this.misspellings = Arc::new(found);
                this.show_misspellings(cx);
            });
        }));
    }

    /// Check spelling again, now: the dictionary or the personal word list
    /// changed.
    pub fn recheck_spelling(&mut self, cx: &mut Context<Self>) {
        self.spell_checked = None;
        self.schedule_spell_check(false, cx);
    }

    fn show_misspellings(&mut self, cx: &mut Context<Self>) {
        let misspellings = self.misspellings.clone();
        self.editor.update(cx, |editor, cx| {
            let text = editor.text().clone();
            let Some(diagnostics) = editor.diagnostics_mut() else {
                return;
            };
            diagnostics.reset(&text);
            diagnostics.extend(misspellings.iter().map(|misspelling| {
                Diagnostic::new(
                    text.offset_to_position(misspelling.range.start)
                        ..text.offset_to_position(misspelling.range.end),
                    format!("Unknown word “{}”", misspelling.word),
                )
                .with_severity(DiagnosticSeverity::Info)
                .with_source("Spelling")
            }));
            cx.notify();
        });
    }

    fn misspelling_at(&self, offset: usize) -> Option<&spell::Misspelling> {
        self.misspellings.iter().find(|misspelling| {
            misspelling.range.start <= offset && offset <= misspelling.range.end
        })
    }

    /// Replace a misspelled word with a suggestion, if the text there is
    /// still that word.
    pub fn replace_misspelling(
        &mut self,
        action: &ReplaceMisspelling,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = action.start..action.end;
        if self.source.get(range.clone()) != Some(action.word.as_ref()) {
            return;
        }
        self.editor.update(cx, |editor, cx| {
            editor.set_selected_range(range, cx);
            editor.replace(action.with.to_string(), window, cx);
        });
    }

    /// The editor's context menu: spelling suggestions for the word under
    /// the pointer, then the usual edit commands.
    fn context_menu(&self, menu: NativeMenu, cx: &App) -> NativeMenu {
        let mut menu = menu;
        // The editor is mid-update when it asks for its menu; a right-click
        // has already moved the caret to the word clicked.
        let cursor = self.last_cursor;
        if let Some(misspelling) = self.misspelling_at(cursor).cloned()
            && let Some(language) = Self::spell_language(cx)
            && let Some(dictionary) = spell::dictionary(&language)
        {
            let suggestions = spell::suggest(&dictionary, &misspelling.word);
            if suggestions.is_empty() {
                menu = menu.menu_with_disabled(
                    "No suggestions",
                    true,
                    Box::new(AddToDictionary(misspelling.word.clone().into())),
                );
            }
            for suggestion in suggestions {
                menu = menu.menu(
                    suggestion.clone(),
                    Box::new(ReplaceMisspelling {
                        start: misspelling.range.start,
                        end: misspelling.range.end,
                        word: misspelling.word.clone().into(),
                        with: suggestion.into(),
                    }),
                );
            }
            menu = menu
                .menu(
                    format!("Add “{}” to dictionary", misspelling.word),
                    Box::new(AddToDictionary(misspelling.word.into())),
                )
                .separator();
        }
        menu.menu("Cut", Box::new(Cut))
            .menu("Copy", Box::new(Copy))
            .menu("Paste", Box::new(Paste))
            .separator()
            .menu("Copy as rich text", Box::new(CopyRichText))
            .separator()
            .menu("Select all", Box::new(SelectAll))
    }

    // ---------------------------------------------------------------------
    // Focus mode

    /// In focus mode, fade everything but the paragraph with the caret.
    fn update_focus_dimming(&mut self, cx: &mut Context<Self>) {
        let focus_mode = AppSettings::get(cx).focus_mode;
        if !focus_mode {
            if let Some(decorations) = self.focus_decorations.take() {
                decorations.clear(cx);
            }
            self.focus_block = None;
            return;
        }
        let cursor = self.cursor(cx);
        if self
            .focus_block
            .as_ref()
            .is_some_and(|block| block.start <= cursor && cursor <= block.end)
        {
            return;
        }
        let block = paragraph_at(&self.source, cursor);
        let len = self.source.len();
        let fade = HighlightStyle {
            fade_out: Some(FOCUS_FADE),
            ..HighlightStyle::default()
        };
        let decorations: Vec<TextDecoration> = [0..block.start, block.end..len]
            .into_iter()
            .filter(|range| !range.is_empty())
            .map(|range| TextDecoration::new(range, fade))
            .collect();
        self.focus_block = Some(block);
        match &self.focus_decorations {
            Some(collection) => collection.set(decorations, cx),
            None => {
                let collection = self.editor.update(cx, |editor, cx| {
                    editor.create_decorations_collection(decorations, cx)
                });
                self.focus_decorations = Some(collection);
            }
        }
    }

    // ---------------------------------------------------------------------
    // Editing

    fn shows_editor(cx: &App) -> bool {
        let settings = AppSettings::get(cx);
        settings.focus_mode || settings.layout.shows_editor()
    }

    fn apply(&mut self, edit: Edit, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.set_selected_range(edit.range.clone(), cx);
            editor.replace(edit.text, window, cx);
            editor.set_selected_range(edit.selection, cx);
        });
    }

    /// Run a formatting command on the selection.
    pub fn apply_edit(
        &mut self,
        build: impl FnOnce(&str, Range<usize>) -> Edit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !Self::shows_editor(cx) {
            return;
        }
        let text = self.editor.read(cx).value().to_string();
        let edit = build(&text, self.editor.read(cx).selected_range());
        self.apply(edit, window, cx);
        self.editor
            .update(cx, |editor, cx| editor.focus(window, cx));
    }

    pub fn inline(&mut self, style: format::Inline, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_edit(
            |text, selection| format::toggle_inline(text, selection, style),
            window,
            cx,
        );
    }

    pub fn block(&mut self, style: format::Block, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_edit(
            |text, selection| format::toggle_block(text, selection, style),
            window,
            cx,
        );
    }

    pub fn tidy_table(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        let selection = self.editor.read(cx).selected_range();
        match smart_edit::tidy_table(&text, selection) {
            Some(edit) => self.apply(edit, window, cx),
            None => cx.emit(DocumentEvent::Notify(
                "Put the caret in a table to tidy it.".into(),
            )),
        }
    }

    /// Move the lines the caret is on up or down, the way VS Code's Move Line
    /// Up / Down (Alt+↑ / Alt+↓) does. Nothing happens at the ends of the
    /// document, which is what VS Code does too.
    pub fn move_lines(
        &mut self,
        direction: format::LineMove,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.editor.read(cx).value().to_string();
        let selection = self.editor.read(cx).selected_range();
        if let Some(edit) = format::move_lines(&text, selection, direction) {
            self.apply(edit, window, cx);
        }
        self.editor
            .update(cx, |editor, cx| editor.focus(window, cx));
    }

    /// Show or hide the page thumbnails, when a PDF is open. Nothing happens
    /// for any other kind of document.
    pub fn toggle_pdf_thumbnails(&mut self, cx: &mut Context<Self>) {
        if let Some(pdf) = &self.pdf {
            pdf.update(cx, |pdf, cx| pdf.toggle_thumbnails(cx));
        }
    }

    /// Enter, Tab and Shift-Tab in the editor: continue lists, nest items,
    /// move between table cells. Returns whether the key was handled.
    fn smart_key(
        &mut self,
        assist: impl FnOnce(&str, Range<usize>) -> Option<Edit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.editor.focus_handle(cx).is_focused(window) {
            return false;
        }
        let editor = self.editor.read(cx);
        let text = editor.value().to_string();
        let Some(edit) = assist(&text, editor.selected_range()) else {
            return false;
        };
        self.apply(edit, window, cx);
        true
    }

    /// Paste images as files next to the document, and URLs over selected
    /// text as links. Returns whether the paste was handled.
    fn paste(&mut self, item: &ClipboardItem, window: &mut Window, cx: &mut Context<Self>) -> bool {
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) => {
                    let bytes = image.bytes.clone();
                    let extension = image.format.extension();
                    self.insert_images(vec![ImageSource::Bytes(bytes, extension)], window, cx);
                    return true;
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    let images: Vec<_> = paths
                        .paths()
                        .iter()
                        .filter(|path| is_image_path(path))
                        .map(|path| ImageSource::File(path.clone()))
                        .collect();
                    if !images.is_empty() {
                        self.insert_images(images, window, cx);
                        return true;
                    }
                }
                ClipboardEntry::String(string) => {
                    let text = self.editor.read(cx).value().to_string();
                    let selection = self.editor.read(cx).selected_range();
                    if let Some(edit) = smart_edit::paste_link(&text, selection, string.text()) {
                        self.apply(edit, window, cx);
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Store images in the document's image folder (files already beside
    /// the document are linked where they are) and link them at the caret.
    fn insert_images(
        &mut self,
        images: Vec<ImageSource>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = self.dir() else {
            cx.emit(DocumentEvent::Notify(
                "Save the document first, so images can be stored next to it.".into(),
            ));
            return;
        };
        let folder = AppSettings::get(cx).image_folder.clone();
        let stem = self
            .path
            .as_ref()
            .and_then(|path| path.file_stem())
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_else(|| "image".to_string());
        let mut links = Vec::new();
        for image in images {
            match store_image(&image, &dir, &folder, &stem) {
                Ok((relative, alt)) => links.push((relative, alt)),
                Err(err) => {
                    cx.emit(DocumentEvent::Notify(
                        format!("Couldn’t add the image. {err}").into(),
                    ));
                }
            }
        }
        if links.is_empty() {
            return;
        }
        let text = self.editor.read(cx).value().to_string();
        let caret = self.editor.read(cx).selected_range();
        let mut markdown = String::new();
        for (ix, (relative, alt)) in links.iter().enumerate() {
            if ix > 0 {
                markdown.push('\n');
            }
            let offset = if ix == 0 { caret.start } else { 0 };
            markdown.push_str(&smart_edit::image_markdown(
                if ix == 0 { &text } else { "" },
                offset,
                relative,
                alt,
            ));
        }
        let end = caret.start + markdown.len();
        self.apply(
            Edit {
                range: caret,
                text: markdown,
                selection: end..end,
            },
            window,
            cx,
        );
        self.editor
            .update(cx, |editor, cx| editor.focus(window, cx));
    }

    // ---------------------------------------------------------------------
    // Navigation

    /// Show the heading a link's `#anchor` names, once the outline is known.
    pub fn reveal_anchor(&mut self, anchor: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.analysis_task.is_some() {
            self.pending_anchor = Some(anchor.to_string());
            return;
        }
        match crate::links::find_heading(&self.analysis.outline, anchor) {
            Some(ix) => self.reveal_heading(ix, window, cx),
            None => cx.emit(DocumentEvent::Notify(
                format!("“{}” has no heading “#{anchor}”.", self.name()).into(),
            )),
        }
    }

    pub fn reveal_heading(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(heading) = self.analysis.outline.get(ix).cloned() else {
            return;
        };
        if Self::shows_editor(cx) {
            // The editor places the heading exactly; the preview follows it.
            self.set_scroll_driver(ScrollDriver::Editor);
            self.editor.update(cx, |editor, cx| {
                editor.set_cursor_position(
                    gpui_kit::component::input::Position::new(heading.line as u32, 0),
                    window,
                    cx,
                );
            });
            // Revealing the caret scrolls just far enough to show it, often
            // leaving the heading at the bottom edge. Once that frame is laid
            // out, scroll the heading to the top of the editor instead.
            let editor = self.editor.clone();
            window.on_next_frame(move |_, cx| {
                editor.update(cx, |editor, cx| {
                    let Some((caret, _)) = editor.cursor_layout() else {
                        return;
                    };
                    // Caret bounds are laid out before scrolling, so they
                    // are content positions measured from the input's top.
                    let offset = editor.scroll_offset();
                    let target = (editor.input_bounds().top() - caret.top()).min(px(0.));
                    editor.set_scroll_offset(point(offset.x, target), cx);
                });
            });
        }
        if let Some((block, _)) = self.analysis.blocks.block_at_line(heading.line as f32) {
            self.preview.update(cx, |preview, cx| {
                preview.list_state().scroll_to(ListOffset {
                    item_ix: block,
                    offset_in_item: px(0.),
                });
                cx.notify();
            });
        }
    }

    /// The outline entry the caret is in (the last heading above it).
    pub fn current_heading(&self, cx: &App) -> Option<usize> {
        let editor = self.editor.read(cx);
        let line = if Self::shows_editor(cx) {
            editor.cursor_position().line as usize
        } else {
            let top = self.preview.read(cx).list_state().logical_scroll_top();
            self.analysis
                .blocks
                .line_at_block(top.item_ix, 0.)
                .map_or(0, |line| line as usize)
        };
        self.analysis
            .outline
            .iter()
            .rposition(|heading| heading.line <= line)
    }

    // ---------------------------------------------------------------------
    // Scroll sync

    fn set_scroll_driver(&mut self, driver: ScrollDriver) {
        if self.scroll_driver != driver {
            self.scroll_driver = driver;
            // A pending jump belongs to the pane that stopped driving.
            self.editor_scroll_target = None;
        }
    }

    /// Watch `bounds` (one of the text panes) for Command or Control plus the
    /// wheel and zoom, the way the viewers do. On its own the wheel keeps
    /// scrolling.
    ///
    /// The listener goes on the window rather than the pane because the
    /// editor's own wheel handling scrolls the document and consumes the
    /// event before anything underneath sees it, and GPUI gives an element no
    /// capture-phase hook for the wheel. A window listener does run in the
    /// capture phase, ahead of every element, and stopping the event there
    /// keeps the editor from scrolling as it zooms.
    fn zoom_on_wheel(bounds: Bounds<Pixels>, window: &mut Window) {
        window.on_mouse_event(
            move |event: &ScrollWheelEvent,
                  phase: DispatchPhase,
                  window: &mut Window,
                  cx: &mut App| {
                if !phase.capture() || !event.modifiers.secondary() {
                    return;
                }
                if !bounds.contains(&event.position) {
                    return;
                }
                let step = f32::from(event.delta.pixel_delta(window.line_height()).y);
                if step != 0. {
                    let action: Box<dyn Action> = if step > 0. {
                        Box::new(ZoomIn)
                    } else {
                        Box::new(ZoomOut)
                    };
                    window.dispatch_action(action, cx);
                }
                // Otherwise the pane underneath scrolls as the zoom resizes it.
                cx.stop_propagation();
            },
        );
    }

    fn syncs_scrolling(&self, cx: &App) -> bool {
        let settings = AppSettings::get(cx);
        settings.scroll_sync
            && settings.layout == Layout::Split
            && !settings.focus_mode
            && !self.preview_limited
            && self.has_preview()
    }

    /// Scroll the preview to the block at the top of the editor.
    ///
    /// Runs when the editor scrolls or the document's structure changes. While
    /// the preview drives, it only records where the editor is, so the
    /// editor's catching up is not mistaken for the user scrolling it.
    fn sync_preview(&mut self, force: bool, cx: &mut Context<Self>) {
        if !self.syncs_scrolling(cx) {
            return;
        }
        let editor = self.editor.read(cx);
        let Some(rows) = editor.visible_row_range() else {
            return;
        };
        let offset_y = editor.scroll_offset().y;
        let position = (rows.start, offset_y);
        if self.scroll_driver != ScrollDriver::Editor {
            self.synced_editor_scroll = Some(position);
            return;
        }
        if !force && self.synced_editor_scroll == Some(position) {
            return;
        }
        self.synced_editor_scroll = Some(position);

        let list = self.preview.read(cx).list_state().clone();
        let count = list.item_count();
        self.synced_preview_rows = count;
        if count == 0 {
            return;
        }

        if offset_y >= px(-0.5) {
            list.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.),
            });
        } else if self.analysis.blocks.len() == count
            && let Some((ix, fraction)) = self.analysis.blocks.block_at_line(rows.start as f32)
        {
            // Rows above the scroll top report no bounds, so land on the row
            // first, then measure it for the offset inside.
            list.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: px(0.),
            });
            let height = list
                .bounds_for_item(ix)
                .map_or(px(0.), |bounds| bounds.size.height);
            list.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: height * fraction,
            });
        } else {
            // The structure is still being analyzed: approximate by position.
            let lines = self.analysis.stats.lines.max(1) as f32;
            let fraction = (rows.start as f32 / lines).clamp(0., 1.);
            let max = list.max_offset_for_scrollbar().y;
            list.set_offset_from_scrollbar(point(px(0.), -(max * fraction)));
        }
        self.synced_preview_scroll = Some(list.logical_scroll_top());
        self.preview.update(cx, |_, cx| cx.notify());
    }

    /// Scroll the editor to the source of the block at the top of the
    /// preview. Runs after every frame the preview paints, so it catches
    /// wheel, trackpad, scrollbar and keyboard scrolling alike.
    fn sync_editor(&mut self, cx: &mut Context<Self>) {
        if !self.syncs_scrolling(cx) || self.scroll_driver != ScrollDriver::Preview {
            return;
        }
        let list = self.preview.read(cx).list_state().clone();
        let top = list.logical_scroll_top();
        let moved = self.synced_preview_scroll.is_none_or(|synced| {
            synced.item_ix != top.item_ix
                || (synced.offset_in_item - top.offset_in_item).abs() > px(0.5)
        });
        if moved {
            self.synced_preview_scroll = Some(top);
            let blocks = &self.analysis.blocks;
            let line = if top.item_ix == 0 && top.offset_in_item <= px(0.5) {
                Some(0.)
            } else if blocks.len() == list.item_count() {
                // The top row is at the scroll top, so its bounds are known.
                let height = list
                    .bounds_for_item(top.item_ix)
                    .map_or(px(0.), |bounds| bounds.size.height);
                let fraction = if height > px(0.) {
                    top.offset_in_item / height
                } else {
                    0.
                };
                blocks.line_at_block(top.item_ix, fraction)
            } else {
                let max = list.max_offset_for_scrollbar().y;
                let scrolled = -list.scroll_px_offset_for_scrollbar().y;
                let fraction = if max > px(0.) { scrolled / max } else { 0. };
                Some(fraction.clamp(0., 1.) * self.analysis.stats.lines as f32)
            };
            self.editor_scroll_target = line.map(|line| EditorScrollTarget { line, attempts: 0 });
        }
        self.scroll_editor_to_target(cx);
    }

    /// Move the editor toward [`Self::editor_scroll_target`]: exactly when
    /// the target line is laid out, by estimate otherwise.
    fn scroll_editor_to_target(&mut self, cx: &mut Context<Self>) {
        let Some(mut target) = self.editor_scroll_target else {
            return;
        };
        let mut landed = false;
        self.editor.update(cx, |editor, cx| {
            let Some(rows) = editor.visible_row_range() else {
                return;
            };
            let text = editor.text();
            let last_line = text.lines_len().saturating_sub(1);
            let line = (target.line.max(0.).floor() as usize).min(last_line);
            let within_line = (target.line - line as f32).clamp(0., 1.);
            let line_range = text.line_start_offset(line)..text.line_end_offset(line);
            let offset = editor.scroll_offset();

            let desired = match (editor.range_to_bounds(&line_range), editor.text_bounds()) {
                (Some(line_bounds), Some(text_bounds)) => {
                    // Content position of the line: measured from the text
                    // origin, which moves with the scroll offset.
                    landed = true;
                    let top = line_bounds.top() - text_bounds.top();
                    -(top + line_bounds.size.height * within_line)
                }
                _ => {
                    // Off screen: estimate from the lines visible now.
                    let visible_lines = rows.len().max(1) as f32;
                    let per_line = editor.input_bounds().size.height / visible_lines;
                    offset.y - per_line * (line as f32 + within_line - rows.start as f32)
                }
            };
            let desired = desired.min(px(0.));
            if (desired - offset.y).abs() > px(0.5) {
                editor.set_scroll_offset(point(offset.x, desired), cx);
            }
        });
        target.attempts += 1;
        self.editor_scroll_target =
            (!landed && target.attempts < MAX_EDITOR_SCROLL_ATTEMPTS).then_some(target);
    }

    // ---------------------------------------------------------------------
    // Rendering

    /// The info bar over a large file, the way VS Code tells why tokenization
    /// and folding are off; it does not block anything.
    fn render_large_file_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let megabytes = self.editor.read(cx).text().len() as f64 / (1024. * 1024.);
        h_flex()
            .id("large-file-bar")
            .w_full()
            .flex_none()
            .gap_3()
            .px_3()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(div().flex_1().child(format!(
                "Large file ({megabytes:.1} MB): highlighting, preview, outline and spell \
                 checking are off to save memory."
            )))
            .child(
                Button::new("enable-large-file-features")
                    .label("Enable anyway")
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| this.enable_anyway(cx))),
            )
    }

    fn render_editor(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let document = cx.weak_entity();
        let inset = column_inset(self.pane_width(self.editor_width, window, cx), cx);
        let focus_mode = AppSettings::get(cx).focus_mode;
        let (menu_document, paste_document) = (document.clone(), document.clone());
        div()
            .id("editor-pane")
            .key_context(crate::actions::EDITOR_CONTEXT)
            .relative()
            .size_full()
            .bg(cx.theme().background)
            .font_family(cx.theme().mono_font_family.clone())
            // The margins beside the text column belong to the editor too:
            // the editor handles the wheel over its text, this handles the
            // rest so the whole pane scrolls.
            .on_mouse_move(cx.listener(|this, _: &MouseMoveEvent, _, _| {
                this.set_scroll_driver(ScrollDriver::Editor)
            }))
            // A file dragged from the sidebar onto the text inserts a
            // Markdown link to it (VS Code's drop-into-editor); drops
            // elsewhere in the window open the file instead.
            .on_drop(cx.listener(Self::on_drop_tree_files))
            .drag_over::<TreeDrag>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            .capture_key_down(cx.listener(|this, _: &KeyDownEvent, _, _| {
                this.set_scroll_driver(ScrollDriver::Editor)
            }))
            .capture_action(cx.listener(|this, action: &Enter, window, cx| {
                if !action.secondary
                    && !action.shift
                    && this.smart_key(smart_edit::enter, window, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                let handled = this.smart_key(
                    |text, selection| {
                        smart_edit::table_tab(text, selection.clone(), false)
                            .or_else(|| smart_edit::indent(text, selection))
                    },
                    window,
                    cx,
                );
                if handled {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &OutdentInline, window, cx| {
                let handled = this.smart_key(
                    |text, selection| {
                        smart_edit::table_tab(text, selection.clone(), true)
                            .or_else(|| smart_edit::outdent(text, selection))
                    },
                    window,
                    cx,
                );
                if handled {
                    cx.stop_propagation();
                }
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                this.set_scroll_driver(ScrollDriver::Editor);
                this.editor.update(cx, |editor, cx| {
                    let line_height = editor.line_height().unwrap_or(window.line_height());
                    let delta = event.delta.pixel_delta(line_height);
                    let offset = editor.scroll_offset();
                    editor.set_scroll_offset(point(offset.x, offset.y + delta.y), cx);
                });
            }))
            .child(
                Editor::new(&self.editor)
                    .h_full()
                    .text_size(rems(if focus_mode { 1. } else { 0.875 }))
                    .line_height(rems(if focus_mode { 1.75 } else { 1.5 }))
                    .border_0()
                    .rounded_none()
                    .pl(inset)
                    .pr(inset)
                    .pt(if focus_mode { rems(4.) } else { rems(1.) })
                    .aria_label("Markdown source")
                    .context_menu(move |menu, _, cx| {
                        menu_document
                            .read_with(cx, |this, cx| this.context_menu(NativeMenu::new(), cx))
                            .unwrap_or(menu)
                    })
                    .on_paste(move |item, window, cx| {
                        paste_document
                            .update(cx, |this, cx| this.paste(item, window, cx))
                            .unwrap_or(false)
                    }),
            )
            .on_prepaint(move |bounds, _, cx| {
                // Notifying mid-frame can be lost; update once the frame is done.
                cx.defer(move |cx| {
                    _ = document.update(cx, |this, cx| {
                        if (this.editor_width - bounds.size.width).abs() >= px(1.) {
                            this.editor_width = bounds.size.width;
                            cx.notify();
                        }
                    });
                });
            })
            .child(
                canvas(
                    |_, _, _| (),
                    |bounds, _, window, _| Self::zoom_on_wheel(bounds, window),
                )
                .absolute()
                .inset_0(),
            )
    }

    fn render_preview(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let document = cx.weak_entity();
        let base_dir = self.dir().map(Arc::<Path>::from);
        let inset = column_inset(self.pane_width(self.preview_width, window, cx), cx);
        let is_empty = !self.large && self.source.trim().is_empty();

        div()
            .id("preview-pane")
            .size_full()
            .relative()
            .bg(cx.theme().background)
            // Let the margins beside the text column scroll the preview; the
            // list scrolls itself when the pointer is over its viewport.
            .on_mouse_move(cx.listener(|this, _: &MouseMoveEvent, _, _| {
                this.set_scroll_driver(ScrollDriver::Preview)
            }))
            .capture_key_down(cx.listener(|this, _: &KeyDownEvent, _, _| {
                this.set_scroll_driver(ScrollDriver::Preview)
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                this.set_scroll_driver(ScrollDriver::Preview);
                let list = this.preview.read(cx).list_state().clone();
                if list.viewport_bounds().contains(&event.position) {
                    return;
                }
                let delta = event.delta.pixel_delta(window.line_height());
                list.scroll_by(-delta.y);
                this.preview.update(cx, |_, cx| cx.notify());
            }))
            .child(
                TextView::new(&self.preview)
                    .size_full()
                    .scrollable(true)
                    .selectable(true)
                    .markdown_extensions(self.markdown_extensions.clone())
                    .pl(inset)
                    .pr(inset)
                    .py_8()
                    .image_source(move |url| images::resolve(url, base_dir.as_deref()))
                    .code_block_actions(|code_block, _, _| {
                        Clipboard::new("copy-code").value(code_block.code())
                    })
                    .on_link_click(crate::links::on_click),
            )
            .when(is_empty, |this| {
                this.child(
                    v_flex()
                        .absolute()
                        .inset_0()
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(
                            div()
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                .text_color(cx.theme().foreground)
                                .child("Nothing to preview"),
                        )
                        .child("Markdown you write appears here."),
                )
            })
            .on_prepaint(move |bounds, _, cx| {
                // Notifying mid-frame can be lost; update once the frame is done.
                cx.defer(move |cx| {
                    _ = document.update(cx, |this, cx| {
                        if (this.preview_width - bounds.size.width).abs() >= px(1.) {
                            this.preview_width = bounds.size.width;
                            cx.notify();
                        }
                        this.sync_editor(cx);
                    });
                });
            })
            .child(
                canvas(
                    |_, _, _| (),
                    |bounds, _, window, _| Self::zoom_on_wheel(bounds, window),
                )
                .absolute()
                .inset_0(),
            )
    }

    /// A pane fills the view unless the editor and preview share it, in
    /// which case its last measured width is used.
    fn pane_width(&self, measured: Pixels, window: &Window, cx: &App) -> Pixels {
        let settings = AppSettings::get(cx);
        match settings.layout {
            Layout::Split if !settings.focus_mode => measured,
            _ if measured > px(0.) => measured,
            _ => window.viewport_size().width,
        }
    }
}

impl Render for DocumentView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(pdf) = &self.pdf {
            return pdf.clone().into_any_element();
        }
        if let Some(image_view) = &self.image {
            return image_view.clone().into_any_element();
        }
        if self.analysis_task.is_none()
            && let Some(anchor) = self.pending_anchor.take()
        {
            let document = cx.entity();
            window.defer(cx, move |window, cx| {
                document.update(cx, |this, cx| this.reveal_anchor(&anchor, window, cx));
            });
        }
        let settings = AppSettings::get(cx);
        let layout = if settings.focus_mode || !self.has_preview() {
            Layout::Editor
        } else {
            settings.layout
        };
        let body = match layout {
            Layout::Editor => self.render_editor(window, cx).into_any_element(),
            Layout::Preview => self.render_preview(window, cx).into_any_element(),
            Layout::Split => h_resizable("document-split")
                .child(
                    resizable_panel()
                        .size_range(rems(16.).to_pixels(window.rem_size())..Pixels::MAX)
                        .child(self.render_editor(window, cx)),
                )
                .child(
                    resizable_panel()
                        .size_range(rems(16.).to_pixels(window.rem_size())..Pixels::MAX)
                        .child(self.render_preview(window, cx)),
                )
                .into_any_element(),
        };
        if !(self.large && self.reduced) {
            return body;
        }
        v_flex()
            .size_full()
            .child(self.render_large_file_bar(cx))
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }
}

/// Past this many lines a text file is large too: VS Code's large-file limit.
const LARGE_DOCUMENT_LINES: usize = 300_000;

fn is_large_text(text: &str) -> bool {
    text.len() > LARGE_DOCUMENT_BYTES
        || text.bytes().filter(|&b| b == b'\n').count() > LARGE_DOCUMENT_LINES
}

/// The preview of a large document: its beginning, cut at a line, under a
/// note saying so. Only that beginning is copied out of the rope.
fn large_preview(rope: &gpui_kit::component::input::Rope) -> String {
    let mut head = String::new();
    for chunk in rope.chunks() {
        if head.len() >= LARGE_PREVIEW_BYTES {
            break;
        }
        head.push_str(chunk);
    }
    let mut cut = floor_char_boundary(&head, LARGE_PREVIEW_BYTES);
    if cut < rope.len()
        && let Some(newline) = head[..cut].rfind('\n')
    {
        cut = newline + 1;
    }
    let megabytes = rope.len() as f64 / (1024. * 1024.);
    format!(
        "> [!NOTE]\n> This document is {megabytes:.1} MB, too large to preview in full. \
         The preview shows its first {} KB and follows your edits once typing pauses; \
         the editor has all of it.\n\n{}",
        LARGE_PREVIEW_BYTES / 1024,
        crate::preview_ext::normalize_tab_containers(&head[..cut])
    )
}

/// An image to add to the document.
enum ImageSource {
    /// Pasted image data, with its file extension.
    Bytes(Vec<u8>, &'static str),
    File(PathBuf),
}

fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp"
            )
        })
}

/// Put `image` where the document can link it, returning the link path
/// (relative to `dir`) and alt text. Files already inside `dir` stay put.
fn store_image(
    image: &ImageSource,
    dir: &Path,
    folder: &str,
    stem: &str,
) -> std::io::Result<(String, String)> {
    if let ImageSource::File(path) = image
        && let Ok(relative) = path.strip_prefix(dir)
    {
        let alt = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default();
        return Ok((link_path(relative), alt));
    }
    let target_dir = dir.join(folder);
    std::fs::create_dir_all(&target_dir)?;
    let (name, extension) = match image {
        ImageSource::Bytes(_, extension) => {
            let now = chrono_like_timestamp();
            (format!("{stem}-{now}"), extension.to_string())
        }
        ImageSource::File(path) => (
            path.file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
                .unwrap_or_else(|| "image".to_string()),
            path.extension()
                .map(|ext| ext.to_string_lossy().to_lowercase())
                .unwrap_or_else(|| "png".to_string()),
        ),
    };
    let name: String = name
        .chars()
        .map(|ch| {
            if ch.is_whitespace() || "/\\:*?\"<>|".contains(ch) {
                '-'
            } else {
                ch
            }
        })
        .collect();
    // Never overwrite: number the name until it is free.
    let mut target = target_dir.join(format!("{name}.{extension}"));
    let mut counter = 2;
    while target.exists() {
        target = target_dir.join(format!("{name}-{counter}.{extension}"));
        counter += 1;
    }
    match image {
        ImageSource::Bytes(bytes, _) => std::fs::write(&target, bytes)?,
        ImageSource::File(path) => {
            std::fs::copy(path, &target)?;
        }
    }
    let relative = link_path(target.strip_prefix(dir).unwrap_or(&target));
    // Pasted images have no name worth describing them by.
    let alt = match image {
        ImageSource::Bytes(..) => "image".to_string(),
        ImageSource::File(_) => name,
    };
    Ok((relative, alt))
}

/// `path` as a Markdown link, which separates folders with `/` on every
/// system (Windows paths use `\\`).
fn link_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// A Markdown link pointing at `path` — image syntax for images —
/// relative to the document's folder when the file lives under it.
fn drop_link(target: &Path, document_dir: Option<&Path>) -> String {
    let relative = document_dir
        .and_then(|dir| target.strip_prefix(dir).ok())
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| target.to_string_lossy().into_owned());
    let encoded = relative
        .replace(' ', "%20")
        .replace('(', "%28")
        .replace(')', "%29");
    if crate::document::is_image_path(target) {
        let alt = target
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".into());
        format!("![{alt}]({encoded})")
    } else {
        let name = target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| encoded.clone());
        format!("[{name}]({encoded})")
    }
}

/// `YYYYMMDD-HHMMSS` in UTC, for naming pasted images.
fn chrono_like_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

/// The paragraph (run of non-blank lines) around `offset`.
fn paragraph_at(text: &str, offset: usize) -> Range<usize> {
    let offset = floor_char_boundary(text, offset);
    let mut start = text[..offset].rfind('\n').map_or(0, |ix| ix + 1);
    while start > 0 {
        let previous = text[..start - 1].rfind('\n').map_or(0, |ix| ix + 1);
        if text[previous..start - 1].trim().is_empty() {
            break;
        }
        start = previous;
    }
    let mut end = text[offset..]
        .find('\n')
        .map_or(text.len(), |ix| offset + ix);
    while end < text.len() {
        let next_end = text[end + 1..]
            .find('\n')
            .map_or(text.len(), |ix| end + 1 + ix);
        if text[end + 1..next_end].trim().is_empty() {
            break;
        }
        end = next_end;
    }
    start..end
}

fn floor_char_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// Horizontal inset that keeps text within a readable column, centered in a
/// pane of `width`, but never tighter than the standard pane padding.
fn column_inset(width: Pixels, cx: &App) -> Pixels {
    let rem = cx.theme().font_size;
    let min = rem * 1.5;
    let column = rem * READABLE_WIDTH_REMS;
    ((width - column) / 2.).max(min)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_large_document_previews_its_beginning_under_a_note() {
        let line = "word ".repeat(20) + "\n";
        let big = line.repeat(super::LARGE_DOCUMENT_BYTES / line.len() + 10);
        assert!(super::is_large_text(&big));
        assert!(!super::is_large_text("# Title\n\nText.\n"));
        assert!(super::is_large_text(
            &"\n".repeat(super::LARGE_DOCUMENT_LINES + 1)
        ));
        let preview = super::large_preview(&gpui_kit::component::input::Rope::from(big.as_str()));
        assert!(preview.starts_with("> [!NOTE]"));
        assert!(preview.len() < 300 * 1024, "{}", preview.len());
        assert!(preview.ends_with('\n'), "cut at a line end");
    }

    use super::*;

    #[test]
    fn finds_the_paragraph_around_the_caret() {
        let text = "one\n\ntwo\nlines\n\nthree";
        assert_eq!(&text[paragraph_at(text, 6)], "two\nlines");
        assert_eq!(&text[paragraph_at(text, 0)], "one");
        assert_eq!(&text[paragraph_at(text, text.len())], "three");
    }

    #[test]
    fn links_use_forward_slashes() {
        let path = Path::new("assets").join("shots").join("a b.png");
        assert_eq!(link_path(&path), "assets/shots/a b.png");
    }

    #[test]
    fn stores_images_without_overwriting() {
        let dir = std::env::temp_dir().join(format!("malgel-images-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bytes = ImageSource::Bytes(vec![1, 2, 3], "png");
        let (first, _) = store_image(&bytes, &dir, "assets", "My notes").unwrap();
        assert!(
            first.starts_with("assets/My-notes-") && first.ends_with(".png"),
            "{first}"
        );
        let outside = std::env::temp_dir().join(format!("malgel-pic-{}.png", std::process::id()));
        std::fs::write(&outside, [0]).unwrap();
        let (copied, alt) =
            store_image(&ImageSource::File(outside.clone()), &dir, "assets", "x").unwrap();
        assert!(copied.starts_with("assets/malgel-pic-"));
        assert!(alt.starts_with("malgel-pic-"));
        let (again, _) =
            store_image(&ImageSource::File(outside.clone()), &dir, "assets", "x").unwrap();
        assert_ne!(copied, again);
        // A file already next to the document is linked in place.
        let (linked, _) =
            store_image(&ImageSource::File(dir.join(&copied)), &dir, "assets", "x").unwrap();
        assert_eq!(linked, copied);
        let _ = std::fs::remove_file(outside);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn formats_timestamps() {
        let stamp = chrono_like_timestamp();
        assert_eq!(stamp.len(), 15);
        assert!(stamp.starts_with("20"));
    }
}
