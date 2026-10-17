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
    InteractiveElement as _, IntoElement, KeyDownEvent, ListOffset, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Render, ScrollWheelEvent,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, UnderlineStyle,
    Window,
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
    /// Show the editor pane (leave preview-only layout) so a jump to source
    /// is visible — used after a Typst preview click or go-to-definition.
    ShowEditor,
}

pub struct DocumentView {
    editor: Entity<EditorState>,
    preview: Entity<TextViewState>,
    pdf: Option<Entity<crate::pdf_view::PdfView>>,
    image: Option<Entity<crate::image_view::ImageView>>,
    /// Parser configuration for the preview; built once so the preview can
    /// tell that it is unchanged between frames.
    markdown_extensions: gpui_kit::component::text::MarkdownExtensions,

    path: Option<PathBuf>,
    line_ending: LineEnding,
    /// How the file is encoded on disk; saved back the same way.
    encoding: crate::document::Encoding,
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

    /// Status-bar / highlighter language override (VS Code "Change Language Mode").
    /// `None` follows the path; set from the status-bar picker.
    language_override: Option<SharedString>,
    /// Whether the preview shows only the start of a large document.
    preview_limited: bool,
    /// Past `LARGE_DOCUMENT_BYTES`: the editor's rope is the only copy of
    /// the text (`source` stays empty) and analysis streams over the rope.
    large: bool,
    /// Highlighting, preview, outline and spell checking are off, as VS Code
    /// does for a large file, until "Enable anyway" in the info bar.
    reduced: bool,
    /// The large-file info bar was closed for this tab.
    bar_dismissed: bool,
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
    /// Brief highlight of a jumped-to range (preview click / go-to-definition).
    jump_flash: Option<TextDecorationCollection>,
    jump_flash_task: Option<Task<()>>,
    /// Last preview/definition jump, so one gesture does not toast/flash twice.
    last_jump: Option<(std::time::Instant, usize)>,
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
    /// A stand-in tab for a PDF or picture still being read off the UI
    /// thread; the workspace swaps the finished document in, or removes it.
    loading: bool,
    /// Where the caret goes once a loading tab's text arrives, reported as
    /// its cursor meanwhile so a session saved now keeps it.
    pending_cursor: usize,
    /// The layout and focus mode last applied, to notice a switch.
    last_shape: Option<(Layout, bool)>,
    /// The line kept at the top while the pane's width changes.
    shown_line: Option<f32>,
    /// The line(s) a press on the line numbers started on, while it drags.
    gutter_anchor: Option<Range<usize>>,
    /// A press on the line numbers waiting for the editor to place the caret:
    /// the selection before it and whether Shift was held.
    gutter_press: Option<(Range<usize>, bool)>,
    /// A Typst source's live preview: its compiler thread and the pages.
    live: Option<TypstLive>,
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
                .line_number(show_line_numbers(&settings))
                .folding(shows_folding(settings.focus_mode))
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
                this.schedule_live_reveal(cx);
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
                let settings = AppSettings::get(cx);
                let shape = (settings.layout, settings.focus_mode);
                if this
                    .last_shape
                    .replace(shape)
                    .is_some_and(|last| last != shape)
                    && let Some(line) = this.anchor_line(cx)
                {
                    this.editor_scroll_target = Some(EditorScrollTarget { line, attempts: 0 });
                }
                this.apply_settings(window, cx);
                this.sync_preview(true, cx);
                this.recolor_typst_highlights(cx);
                cx.notify();
            }),
        ];

        let mut this = Self {
            editor,
            pdf: None,
            image: None,
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
            language_override: None,
            preview_limited: false,
            large: false,
            reduced: false,
            bar_dismissed: false,
            encoding: Default::default(),
            pending_anchor: None,
            recovery_id: session::new_recovery_id(),
            recovery_written: false,
            recovery_task: None,
            misspellings: Arc::default(),
            spell_task: None,
            spell_checked: None,
            last_cursor: 0,
            focus_decorations: None,
            jump_flash: None,
            jump_flash_task: None,
            last_jump: None,
            focus_block: None,
            scroll_driver: ScrollDriver::Editor,
            synced_editor_scroll: None,
            synced_preview_scroll: None,
            editor_scroll_target: None,
            synced_preview_rows: 0,
            editor_width: px(0.),
            preview_width: px(0.),
            loading: false,
            last_shape: None,
            shown_line: None,
            gutter_anchor: None,
            gutter_press: None,
            live: None,
            pending_cursor: 0,
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

    /// Focus handle for the preview side that is actually on screen: a live
    /// Typst `PdfView` when that pane is showing, otherwise the Markdown
    /// `TextView`. Preview-only layout used to focus the Markdown preview even
    /// for Typst, which left focus on an element not in the tree — title-bar
    /// `dispatch_action` (SetLayout, sidebar toggle, …) then never reached the
    /// workspace.
    pub fn preview_pane_focus_handle(&self, cx: &App) -> FocusHandle {
        if let Some(live) = &self.live {
            return live.view.read(cx).focus_handle(cx);
        }
        self.preview().read(cx).focus_handle().clone()
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

    /// A placeholder for `path` while it loads in the background: its tab
    /// carries the file's name and its body says it is loading.
    pub fn new_loading(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut doc = Self::new(String::new(), Some(path), LineEnding::Lf, window, cx);
        doc.loading = true;
        doc
    }

    /// A loading tab that will put the caret at `cursor`.
    pub fn with_pending_cursor(mut self, cursor: usize) -> Self {
        self.pending_cursor = cursor;
        self
    }

    /// Whether this is a placeholder for a file still loading.
    pub fn is_loading(&self) -> bool {
        self.loading
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
            language_override: None,
            preview_limited: false,
            large: false,
            reduced: false,
            bar_dismissed: false,
            encoding: Default::default(),
            pending_anchor: None,
            recovery_id: session::new_recovery_id(),
            recovery_written: false,
            recovery_task: None,
            misspellings: Arc::default(),
            spell_task: None,
            spell_checked: None,
            last_cursor: 0,
            focus_decorations: None,
            jump_flash: None,
            jump_flash_task: None,
            last_jump: None,
            focus_block: None,
            scroll_driver: ScrollDriver::Editor,
            synced_editor_scroll: None,
            synced_preview_scroll: None,
            editor_scroll_target: None,
            synced_preview_rows: 0,
            editor_width: px(0.),
            preview_width: px(0.),
            loading: false,
            last_shape: None,
            shown_line: None,
            gutter_anchor: None,
            gutter_press: None,
            live: None,
            pending_cursor: 0,
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
            language_override: None,
            preview_limited: false,
            large: false,
            reduced: false,
            bar_dismissed: false,
            encoding: Default::default(),
            pending_anchor: None,
            recovery_id: session::new_recovery_id(),
            recovery_written: false,
            recovery_task: None,
            misspellings: Arc::default(),
            spell_task: None,
            spell_checked: None,
            last_cursor: 0,
            focus_decorations: None,
            jump_flash: None,
            jump_flash_task: None,
            last_jump: None,
            focus_block: None,
            scroll_driver: ScrollDriver::Editor,
            synced_editor_scroll: None,
            synced_preview_scroll: None,
            editor_scroll_target: None,
            synced_preview_rows: 0,
            editor_width: px(0.),
            preview_width: px(0.),
            loading: false,
            last_shape: None,
            shown_line: None,
            gutter_anchor: None,
            gutter_press: None,
            live: None,
            pending_cursor: 0,
            _subscriptions: Vec::new(),
        }
    }

    /// The language the status bar names: empty for a Markdown document, and
    /// the source's own language for a preview that is not Markdown's. A Typst
    /// document's pages are Typst's output, so saying "Markdown" beside them
    /// would name the wrong thing.
    pub fn language_label(&self) -> SharedString {
        if let Some(override_label) = &self.language_override {
            return override_label.clone();
        }
        if self.reduced {
            return "Plain Text".into();
        }
        crate::document::language_name(self.path.as_deref()).into()
    }

    /// The indentation the status bar names.
    pub fn indent(&self) -> crate::document::Indent {
        self.indent
    }

    /// Apply Spaces/Tabs from the status-bar picker (does not reformat existing text).
    pub fn set_indent(&mut self, indent: crate::document::Indent, cx: &mut Context<Self>) {
        if self.indent == indent {
            return;
        }
        self.indent = indent;
        self.editor.update(cx, |editor, cx| {
            editor.set_tab_size(
                TabSize {
                    tab_size: indent.size,
                    hard_tabs: indent.hard_tabs,
                },
                cx,
            );
        });
        cx.notify();
    }

    /// Change Language Mode from the status bar (highlighter + label).
    pub fn set_language_mode(
        &mut self,
        label: SharedString,
        highlighter: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.language_override = Some(label);
        if !self.reduced {
            self.editor
                .update(cx, |editor, cx| editor.set_highlighter(highlighter, cx));
        }
        let _ = window;
        cx.notify();
    }

    /// Whether this text document gets the rendered preview beside it:
    /// Markdown does, source code and other text open in the editor alone.
    pub fn has_preview(&self) -> bool {
        !self.reduced
            && (crate::document::has_markdown_preview(self.path.as_deref()) || self.live.is_some())
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
        // Highlighting parses the whole text on the UI thread, which froze
        // the window for 15 s on a 20 MB file: past the large-file size it
        // stays off, as VS Code keeps tokenization off past its limit.
        if self.editor.read(cx).text().len() <= LARGE_DOCUMENT_BYTES {
            let language = self.highlight_language();
            self.editor
                .update(cx, |editor, cx| editor.set_highlighter(language, cx));
        }
        self.on_text_changed(false, cx);
        self.schedule_analysis(false, cx);
    }

    /// The tree-sitter language the editor highlights this file with.
    fn highlight_language(&self) -> &'static str {
        let Some(path) = self.path.as_deref() else {
            return "markdown";
        };
        if crate::document::is_markdown_path(path) {
            return "markdown";
        }
        // Plain text must not fall through to Markdown: that turns on
        // heading fold chevrons (G8). Typst keeps "text" so tree-sitter
        // stays off; its colours come from typst_live decorations.
        crate::document::language_tag(path).unwrap_or("text")
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
        self.pdf.is_some() || self.image.is_some() || self.loading
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

    /// Diagnostics and misspellings for the Problems panel.
    pub fn problems(&self, cx: &App) -> Vec<ProblemItem> {
        let mut out = Vec::new();
        if let Some(live) = &self.live {
            for diagnostic in &live.diagnostics {
                let line = diagnostic.range.as_ref().map(|range| {
                    let upto = range.start.min(self.source.len());
                    self.source[..floor_char_boundary(&self.source, upto)]
                        .matches('\n')
                        .count()
                        + 1
                });
                let message = diagnostic
                    .message
                    .lines()
                    .next()
                    .unwrap_or(&diagnostic.message)
                    .to_string();
                out.push(ProblemItem {
                    message,
                    line,
                    error: diagnostic.error,
                    warning: !diagnostic.error,
                });
            }
        }
        for misspelling in self.misspellings.iter() {
            let line = {
                let upto = misspelling.range.start.min(self.source.len());
                self.source[..floor_char_boundary(&self.source, upto)]
                    .matches('\n')
                    .count()
                    + 1
            };
            out.push(ProblemItem {
                message: format!("Unknown word: {}", misspelling.word),
                line: Some(line),
                error: false,
                warning: false,
            });
        }
        let _ = cx;
        out
    }

    /// Record the encoding the file was read in, so saving keeps it.
    pub fn with_encoding(mut self, encoding: crate::document::Encoding) -> Self {
        self.encoding = encoding;
        self
    }

    pub fn encoding(&self) -> crate::document::Encoding {
        self.encoding
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

    /// The file as last read or written.
    pub fn disk_stamp(&self) -> Option<DiskStamp> {
        self.disk
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
        if self.loading {
            return self.pending_cursor;
        }
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
        // Error-banner / outline jumps from preview-only must surface the editor.
        if !Self::shows_editor(cx) {
            cx.emit(DocumentEvent::ShowEditor);
        }
        let editor = self.editor.read(cx);
        let rope = editor.text();
        let len = rope.len();
        let row = (line.max(1) - 1).min(rope.lines_len().saturating_sub(1));
        let offset = rope.line_start_offset(row);
        let shown = editor.visible_row_range().map_or(0, |rows| rows.len());
        self.set_cursor(offset.min(len), window, cx);
        // The line lands in the middle of the view, as VS Code's Go to Line
        // puts it, not wherever scrolling just far enough leaves it.
        self.editor_scroll_target = Some(EditorScrollTarget {
            line: centered_top(row, shown),
            attempts: 0,
        });
        self.scroll_editor_to_target(cx);
    }

    /// The line to keep at the top across a change of layout: the caret's
    /// line keeps its place when it is on screen (a few lines down), the
    /// first visible line otherwise. Soft wrap changes with the width, so a
    /// pixel offset would land somewhere else.
    fn anchor_line(&self, cx: &App) -> Option<f32> {
        let editor = self.editor.read(cx);
        let rows = editor.visible_row_range()?;
        let caret = editor.text().offset_to_position(editor.cursor()).line as usize;
        Some(anchor_for(rows, caret))
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
        let focus_mode = AppSettings::get(cx).focus_mode;
        self.editor.update(cx, |editor, cx| {
            editor.set_highlighter(language, cx);
            editor.set_folding(shows_folding(focus_mode), window, cx);
            editor.set_tab_size(
                TabSize {
                    tab_size: indent.size,
                    hard_tabs: indent.hard_tabs,
                },
                cx,
            );
            editor.set_value(text, window, cx)
        });
        self.start_typst_live(window, cx);
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
        self.start_typst_live(window, cx);
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

    /// Rebuild the Markdown preview after a settings change (e.g. GFM breaks).
    pub fn refresh_preview_from_settings(&mut self, cx: &mut Context<Self>) {
        if !self.has_preview() {
            return;
        }
        self.on_text_changed(false, cx);
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
        if self.live.is_some() {
            self.source = source;
            self.dirty = true;
            self.revision += 1;
            self.focus_block = None;
            self.send_live_text(debounce, cx);
            self.schedule_analysis(debounce, cx);
            cx.notify();
            return;
        }
        // Non-markdown text files render as one highlighted code block: the
        // extension picks the tree-sitter language for the preview's
        // highlighter. Markdown keeps its normal document rendering.
        // Source code and other text is not previewed at all, as in VS
        // Code: the editor is the whole view.
        if self.has_preview() {
            let mut preview_source = crate::preview_ext::normalize_tab_containers(&source);
            if crate::themes::AppSettings::get(cx).gfm_breaks {
                preview_source = crate::preview_ext::apply_gfm_breaks(&preview_source);
            }
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
    // Line numbers

    /// Whether `position` is over the line numbers (not the fold arrows).
    ///
    /// The fold column sits immediately to the right of the digit run (gpui
    /// reserves a dedicated hitbox after a small margin). Ending the hit test
    /// at the last digit keeps chevron clicks out of whole-line selection so
    /// the editor's fold toggle can run.
    fn in_line_numbers(
        &self,
        position: gpui_kit::Point<Pixels>,
        window: &Window,
        cx: &App,
    ) -> bool {
        let settings = AppSettings::get(cx);
        if !show_line_numbers(settings) {
            return false;
        }
        let editor = self.editor.read(cx);
        // The viewport, not `text_bounds()`: those move with the scroll
        // offset, so after scrolling the gutter test missed every click.
        let bounds = editor.input_bounds();
        if bounds.size.width <= px(0.) {
            return false;
        }
        let font_size = rems(0.875).to_pixels(window.rem_size());
        let font = gpui_kit::font(cx.theme().mono_font_family.clone());
        let system = window.text_system();
        let digit = system
            .advance(system.resolve_font(&font), font_size, '+')
            .map(|size| size.width)
            .unwrap_or(font_size * 0.6);
        let digits = line_number_digits(editor.text().lines_len());
        // Digits only — leave the margin and fold column for chevron hits.
        let right = bounds.origin.x + digit * digits as f32;
        let left = bounds.origin.x - px(EDITOR_EDGE) - px(1.);
        position.x >= left
            && position.x < right
            && position.y >= bounds.origin.y
            && position.y < bounds.origin.y + bounds.size.height
    }

    /// Whether `position` is over the fold chevron column (right of the digits).
    fn in_fold_column(&self, position: gpui_kit::Point<Pixels>, window: &Window, cx: &App) -> bool {
        if !shows_folding(AppSettings::get(cx).focus_mode) {
            return false;
        }
        let editor = self.editor.read(cx);
        let bounds = editor.input_bounds();
        if bounds.size.width <= px(0.) {
            return false;
        }
        let font_size = rems(0.875).to_pixels(window.rem_size());
        let font = gpui_kit::font(cx.theme().mono_font_family.clone());
        let system = window.text_system();
        let digit = system
            .advance(system.resolve_font(&font), font_size, '+')
            .map(|size| size.width)
            .unwrap_or(font_size * 0.6);
        let digits = line_number_digits(editor.text().lines_len());
        // Match gpui-base (malgel-fold-0.7.0): digits, then margin, then fold hitbox.
        // Keep in sync with LINE_NUMBER_RIGHT_MARGIN + FOLD_ICON_HITBOX_WIDTH.
        let left = bounds.origin.x + digit * digits as f32;
        let right = left + px(FOLD_COLUMN_MARGIN) + px(FOLD_COLUMN_HITBOX);
        position.x >= left
            && position.x < right
            && position.y >= bounds.origin.y
            && position.y < bounds.origin.y + bounds.size.height
    }

    /// Fold at the caret (VS Code Fold).
    pub fn fold_at_cursor(&mut self, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let position = editor.cursor_position();
            editor.fold_at(position, cx);
        });
    }

    /// Unfold at the caret (VS Code Unfold).
    pub fn unfold_at_cursor(&mut self, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let position = editor.cursor_position();
            editor.unfold_line_at(position, cx);
        });
    }

    /// Toggle the fold candidate for the buffer line under `position`.
    fn toggle_fold_at_point(
        &mut self,
        position: gpui_kit::Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor.update(cx, |editor, cx| {
            // Resolve the line via the editor's own hit test (wrap / scroll).
            let (offset, _, _) = editor.resolve_mouse_position(position);
            let line = editor.text().offset_to_point(offset).row;
            // Prefer a candidate that starts on this display line; else the
            // innermost covering candidate (same rule as fold_at).
            let start = editor
                .fold_candidates()
                .iter()
                .find(|c| c.start_line == line)
                .map(|c| c.start_line)
                .or_else(|| {
                    editor
                        .fold_candidates()
                        .iter()
                        .filter(|c| line > c.start_line && line < c.end_line)
                        .max_by_key(|c| c.start_line)
                        .map(|c| c.start_line)
                });
            let Some(start_line) = start else {
                return false;
            };
            editor.toggle_fold(start_line, cx)
        })
    }

    /// After the editor placed the caret for a press on the line numbers,
    /// select the caret's whole logical line (Shift: through the previous
    /// selection's lines).
    fn start_gutter_selection(&mut self, cx: &mut Context<Self>) {
        let Some((previous, shift)) = self.gutter_press.take() else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            let text = editor.text().to_string();
            let anchor = gutter_selection(&text, editor.cursor(), previous.clone(), shift);
            editor.set_selected_range(anchor.clone(), cx);
            self.gutter_anchor = Some(anchor);
        });
    }

    /// Select from the line the gutter press started on to the caret's line.
    fn extend_gutter_selection(&mut self, cx: &mut Context<Self>) {
        let Some(anchor) = self.gutter_anchor.clone() else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            let text = editor.text().to_string();
            let range = extend_line_selection(&text, &anchor, editor.cursor());
            if editor.selected_range() != range {
                editor.set_selected_range(range, cx);
            }
        });
    }

    // ---------------------------------------------------------------------
    // Typst live preview

    /// Give a Typst source its live preview: a compiler thread of its own
    /// and a page viewer beside the editor. Clicking a page moves the caret
    /// to the text it came from.
    fn start_typst_live(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.live.is_some() || self.reduced {
            return;
        }
        let Some(path) = self
            .path
            .clone()
            .filter(|path| crate::document::is_typst_path(path))
        else {
            return;
        };
        let (worker, mut answers) = crate::typst_live::LiveWorker::spawn(path.clone());
        let view = cx.new(|cx| {
            let mut view = crate::pdf_view::PdfView::new(
                Some(path),
                crate::pdf_view::PdfDocument::from_rendered(Vec::new()),
                cx,
            );
            view.fit_width(cx);
            view
        });
        let clicks = cx.subscribe(
            &view,
            |this, _, event: &crate::pdf_view::PdfViewEvent, _| {
                let crate::pdf_view::PdfViewEvent::Clicked { page, x, y } = *event;
                if let Some(live) = &this.live {
                    live.worker
                        .send(crate::typst_live::Request::SourceAt { page, x, y });
                }
            },
        );
        let listen = cx.spawn_in(window, async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(answer) = answers.next().await {
                if this
                    .update_in(cx, |this, window, cx| {
                        this.on_live_answer(answer, window, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        self.live = Some(TypstLive {
            worker,
            view,
            diagnostics: Vec::new(),
            compiled_once: false,
            highlights: None,
            syntax_decorations: None,
            last_tags: Vec::new(),
            send_task: None,
            reveal_task: None,
            revealed_cursor: None,
            typed: None,
            latency: None,
            _listen: listen,
            _clicks: clicks,
        });
    }

    /// Hand the text to the compiler once typing pauses briefly.
    fn send_live_text(&mut self, debounce: bool, cx: &mut Context<Self>) {
        let revision = self.revision;
        let Some(live) = &mut self.live else {
            return;
        };
        live.typed = Some((revision, std::time::Instant::now()));
        live.send_task = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(LIVE_DEBOUNCE).await;
            }
            _ = this.update(cx, |this, cx| {
                let text = this.editor.read(cx).value().to_string();
                if let Some(live) = &this.live {
                    live.worker
                        .send(crate::typst_live::Request::Compile { text, revision });
                }
            });
        }));
    }

    /// Show where the caret's text is on the pages, once the caret rests.
    fn schedule_live_reveal(&mut self, cx: &mut Context<Self>) {
        let cursor = self.last_cursor;
        let Some(live) = &mut self.live else {
            return;
        };
        if live.revealed_cursor == Some(cursor) {
            return;
        }
        live.revealed_cursor = Some(cursor);
        live.reveal_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LIVE_REVEAL_DELAY).await;
            _ = this.update(cx, |this, _| {
                if let Some(live) = &this.live {
                    live.worker
                        .send(crate::typst_live::Request::PageAt { offset: cursor });
                }
            });
        }));
    }

    fn on_live_answer(
        &mut self,
        answer: crate::typst_live::Response,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::typst_live::Response;
        let current = self.revision;
        let Some(live) = &mut self.live else {
            return;
        };
        match answer {
            Response::Compiled(compiled) => {
                if let Some((typed, since)) = live.typed
                    && typed == compiled.revision
                {
                    live.typed = None;
                    let latency = since.elapsed();
                    live.latency = Some(latency);
                    log::info!(
                        "typst live: keystroke to preview {:.1} ms (compile {:.1} ms, {} of {} pages redrawn)",
                        latency.as_secs_f64() * 1000.,
                        compiled.elapsed.as_secs_f64() * 1000.,
                        compiled.redrawn,
                        compiled.pages.as_ref().map_or(0, Vec::len),
                    );
                }
                if let Some(pages) = compiled.pages {
                    let document = crate::pdf_view::PdfDocument::from_rendered(
                        pages
                            .into_iter()
                            .map(|page| (page.width, page.height, page.image))
                            .collect(),
                    );
                    live.view.update(cx, |view, cx| view.reload(document, cx));
                    live.compiled_once = true;
                }
                live.diagnostics = compiled.diagnostics;
                if compiled.revision == current {
                    live.last_tags = compiled.highlights.clone();
                    let theme = cx.theme().highlight_theme.clone();
                    let decorations: Vec<TextDecoration> = compiled
                        .highlights
                        .into_iter()
                        .filter_map(|(range, tag)| {
                            Some(TextDecoration::new(range, typst_tag_style(tag, &theme)?))
                        })
                        .collect();
                    live.syntax_decorations = Some(decorations.clone());
                    match &live.highlights {
                        Some(collection) => collection.set(decorations, cx),
                        None => {
                            let collection = self.editor.update(cx, |editor, cx| {
                                editor.create_decorations_collection(decorations, cx)
                            });
                            if let Some(live) = &mut self.live {
                                live.highlights = Some(collection);
                            }
                        }
                    }
                }
                self.show_misspellings(cx);
                cx.notify();
            }
            Response::SourceAt(offset) => {
                log::debug!("typst live: preview click -> source offset {offset}");
                self.reveal_typst_source(offset, "Preview click", window, cx);
            }
            Response::PageAt(page, _x, y) => {
                log::debug!("typst live: caret reveal -> page {page}");
                live.view.update(cx, |view, cx| view.reveal(page, y, cx));
            }
            Response::DefinitionAt(offset) => {
                log::debug!("typst live: go-to-definition -> offset {offset}");
                self.reveal_typst_source(offset, "Go to Definition", window, cx);
            }
        }
    }

    /// Make a Typst source jump obvious: leave preview-only layout if needed,
    /// scroll the caret into view, flash the target line, and focus the editor.
    fn reveal_typst_source(
        &mut self,
        offset: usize,
        reason: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = std::time::Instant::now();
        if let Some((when, last)) = self.last_jump
            && last == offset
            && now.duration_since(when) < JUMP_DEDUPE
        {
            return;
        }
        self.last_jump = Some((now, offset));
        if !Self::shows_editor(cx) {
            cx.emit(DocumentEvent::ShowEditor);
        }
        let editor = self.editor.read(cx);
        let rope = editor.text();
        let len = rope.len();
        let offset = offset.min(len);
        let offset = floor_char_boundary(&editor.value(), offset);
        let position = rope.offset_to_position(offset);
        let row = position.line as usize;
        let line_start = rope.line_start_offset(row);
        let line_end = if row + 1 < rope.lines_len() {
            rope.line_start_offset(row + 1)
        } else {
            len
        };
        let shown = editor.visible_row_range().map_or(0, |rows| rows.len());
        let _ = editor;
        self.set_cursor(offset, window, cx);
        self.editor_scroll_target = Some(EditorScrollTarget {
            line: centered_top(row, shown),
            attempts: 0,
        });
        self.scroll_editor_to_target(cx);
        self.flash_jump_range(line_start..line_end, cx);
        let focus = self.editor.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        let line = row + 1;
        cx.emit(DocumentEvent::Notify(
            format!("{reason}: line {line}").into(),
        ));
        cx.notify();
    }

    /// Briefly highlight `range` so a jump is visible even when the caret is
    /// already near the target.
    fn flash_jump_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        if range.start >= range.end {
            return;
        }
        let style = HighlightStyle {
            background_color: Some(cx.theme().selection),
            ..HighlightStyle::default()
        };
        let decorations = vec![TextDecoration::new(range, style)];
        match &self.jump_flash {
            Some(collection) => collection.set(decorations, cx),
            None => {
                let collection = self.editor.update(cx, |editor, cx| {
                    editor.create_decorations_collection(decorations, cx)
                });
                self.jump_flash = Some(collection);
            }
        }
        self.jump_flash_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(900))
                .await;
            _ = this.update(cx, |this, cx| {
                if let Some(flash) = this.jump_flash.take() {
                    flash.clear(cx);
                }
                this.jump_flash_task = None;
            });
        }));
    }

    /// Ask the Typst compiler for the definition under the caret (F12).
    pub fn go_to_typst_definition(&mut self, cx: &mut Context<Self>) {
        let Some(live) = &self.live else {
            return;
        };
        let offset = self.editor.read(cx).cursor();
        log::debug!("typst live: request definition at {offset}");
        live.worker
            .send(crate::typst_live::Request::DefinitionAt { offset });
    }

    /// The preview pane: a live Typst preview's pages, under the compiler's
    /// first error when the text does not compile (the pages stay the last
    /// good ones), or the Markdown preview.
    fn render_side(&self, window: &Window, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let Some(live) = &self.live else {
            return self.render_preview(window, cx).into_any_element();
        };
        let theme = cx.theme();
        let error = live
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.error)
            .map(|diagnostic| {
                let line = diagnostic.range.as_ref().map(|range| {
                    let upto = range.start.min(self.source.len());
                    self.source[..floor_char_boundary(&self.source, upto)]
                        .matches('\n')
                        .count()
                        + 1
                });
                let first = diagnostic.message.lines().next().unwrap_or_default();
                match line {
                    Some(line) => format!("Line {line}: {first}"),
                    None => first.to_string(),
                }
            });
        let jump_line = live
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.error)
            .and_then(|diagnostic| diagnostic.range.as_ref())
            .map(|range| {
                let upto = range.start.min(self.source.len());
                self.source[..floor_char_boundary(&self.source, upto)]
                    .matches('\n')
                    .count()
                    + 1
            });
        let banner = error.map(|message| {
            let document = cx.weak_entity();
            h_flex()
                .id("typst-error")
                .w_full()
                .px_3()
                .py_1()
                .gap_2()
                .text_sm()
                .bg(theme.danger.opacity(0.12))
                .text_color(theme.danger)
                .cursor_pointer()
                .when_some(jump_line, |this, line| {
                    this.on_click(move |_, window, cx| {
                        _ = document.update(cx, |this, cx| this.go_to_line(line, window, cx));
                    })
                })
                .child(div().flex_1().min_w_0().truncate().child(message))
                .when(live.compiled_once, |this| {
                    this.child(
                        div()
                            .flex_none()
                            .text_color(theme.muted_foreground)
                            .child("showing the last version that compiled"),
                    )
                })
        });
        let body = if live.compiled_once {
            live.view.clone().into_any_element()
        } else {
            h_flex()
                .size_full()
                .justify_center()
                .items_center()
                .gap_2()
                .text_color(theme.muted_foreground)
                .when(banner.is_none(), |this| {
                    this.child(gpui_kit::component::spinner::Spinner::new())
                        .child("Compiling\u{2026}")
                })
                .when(banner.is_some(), |this| {
                    this.child("Fix the error to see the pages.")
                })
                .into_any_element()
        };
        v_flex()
            .id("typst-live-pane")
            .size_full()
            .min_w_0()
            .min_h_0()
            .bg(theme.background)
            .children(banner)
            .child(div().flex_1().min_h_0().min_w_0().child(body))
            .into_any_element()
    }

    /// The last measured time from a keystroke to its pages being shown.
    #[allow(dead_code)]
    pub fn typst_latency(&self) -> Option<std::time::Duration> {
        self.live.as_ref().and_then(|live| live.latency)
    }

    // ---------------------------------------------------------------------
    // Saving and recovery

    /// Write the current text to `path`, which becomes the document's.
    pub fn write_to(&mut self, path: &Path, cx: &mut Context<Self>) -> std::io::Result<()> {
        if self.pdf.is_some() {
            return Ok(());
        }
        let text = self.editor.read(cx).value();
        crate::document::save_encoded(path, &text, self.line_ending, self.encoding)?;
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
            // A large document lends a clone of its rope (shared nodes, no
            // copy); the snapshot is streamed from its chunks to the file.
            let Ok(Some((id, recovery, rope))) = this.read_with(cx, |this, cx| {
                (this.dirty && this.revision == revision).then(|| {
                    (
                        this.recovery_id.clone(),
                        Recovery {
                            path: this.path.clone(),
                            text: if this.large {
                                String::new()
                            } else {
                                this.source.to_string()
                            },
                            crlf: this.line_ending == LineEnding::CrLf,
                        },
                        this.large.then(|| this.editor.read(cx).text().clone()),
                    )
                })
            }) else {
                return;
            };
            let written = cx
                .background_executor()
                .spawn(async move {
                    match rope {
                        Some(rope) => session::write_recovery_chunks(&id, &recovery, rope.chunks()),
                        None => session::write_recovery(&id, &recovery),
                    }
                    .is_ok()
                })
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
            editor.set_line_number(show_line_numbers(&settings), window, cx);
            editor.set_folding(shows_folding(settings.focus_mode), window, cx);
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
        let typst = self
            .live
            .as_ref()
            .map(|live| live.diagnostics.clone())
            .unwrap_or_default();
        // Language "text" (Typst, plain text) has no tree-sitter highlighter.
        // gpui then skips painting DiagnosticSet, so Typst errors also go into
        // the decoration layer as underlines.
        let paint_typst_underlines = self.live.is_some();
        let theme_danger = cx.theme().danger;
        let theme_warning = cx.theme().warning;
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
            let len = text.len();
            diagnostics.extend(typst.iter().filter_map(|diagnostic| {
                let range = diagnostic.range.clone()?;
                let start = range.start.min(len);
                let end = range.end.clamp(start, len);
                Some(
                    Diagnostic::new(
                        text.offset_to_position(start)..text.offset_to_position(end),
                        diagnostic.message.clone(),
                    )
                    .with_severity(if diagnostic.error {
                        DiagnosticSeverity::Error
                    } else {
                        DiagnosticSeverity::Warning
                    })
                    .with_source("Typst"),
                )
            }));
            cx.notify();
        });
        if paint_typst_underlines {
            self.paint_typst_diagnostic_underlines(&typst, theme_danger, theme_warning, cx);
        }
    }

    /// Squiggles for Typst diagnostics via the decoration layer (see
    /// `show_misspellings`): language "text" never gets a tree-sitter
    /// highlighter, and without one gpui drops DiagnosticSet paint.
    /// Re-apply Typst syntax colours from the current theme (dark/light
    /// switch) without waiting for the next keystroke's compile.
    fn recolor_typst_highlights(&mut self, cx: &mut Context<Self>) {
        let tags = self
            .live
            .as_ref()
            .map(|live| live.last_tags.clone())
            .unwrap_or_default();
        if tags.is_empty() {
            return;
        }
        let theme = cx.theme().highlight_theme.clone();
        let decorations: Vec<TextDecoration> = tags
            .iter()
            .filter_map(|(range, tag)| {
                Some(TextDecoration::new(
                    range.clone(),
                    typst_tag_style(*tag, &theme)?,
                ))
            })
            .collect();
        if let Some(live) = &mut self.live {
            live.syntax_decorations = Some(decorations);
        }
        self.show_misspellings(cx);
    }

    fn paint_typst_diagnostic_underlines(
        &mut self,
        typst: &[crate::typst_live::LiveDiagnostic],
        danger: gpui_kit::Hsla,
        warning: gpui_kit::Hsla,
        cx: &mut Context<Self>,
    ) {
        let len = self.editor.read(cx).text().len();
        let underlines: Vec<TextDecoration> = typst
            .iter()
            .filter_map(|diagnostic| {
                let range = diagnostic.range.clone()?;
                let start = range.start.min(len);
                let end = range.end.clamp(start, len);
                if start >= end {
                    return None;
                }
                let color = if diagnostic.error { danger } else { warning };
                Some(TextDecoration::new(
                    start..end,
                    HighlightStyle {
                        underline: Some(UnderlineStyle {
                            thickness: px(1.5),
                            color: Some(color),
                            wavy: true,
                        }),
                        ..HighlightStyle::default()
                    },
                ))
            })
            .collect();
        let Some(live) = &mut self.live else {
            return;
        };
        // Keep syntax colours and replace only the diagnostic layer.
        let syntax = live.syntax_decorations.clone().unwrap_or_default();
        let mut all = syntax;
        all.extend(underlines);
        match &live.highlights {
            Some(collection) => collection.set(all, cx),
            None if !all.is_empty() => {
                let collection = self.editor.update(cx, |editor, cx| {
                    editor.create_decorations_collection(all, cx)
                });
                if let Some(live) = &mut self.live {
                    live.highlights = Some(collection);
                }
            }
            None => {}
        }
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

    /// Comment the selected lines out, or back in (Ctrl+/), in this file
    /// type's comment syntax; Markdown and untitled documents use `<!-- -->`.
    pub fn toggle_line_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let style = match self.path.as_deref() {
            None => format::comment_style(Some("md")),
            Some(path) => format::comment_style_for_path(Some(path)),
        };
        let Some(style) = style else {
            return;
        };
        let text = self.editor.read(cx).value().to_string();
        let selection = self.editor.read(cx).selected_range();
        if let Some(edit) = format::toggle_line_comment(&text, selection, style) {
            self.apply(edit, window, cx);
        }
        self.editor
            .update(cx, |editor, cx| editor.focus(window, cx));
    }

    /// Ctrl+D: select the word at the caret, then move to its next match.
    pub fn select_next_occurrence(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        let selection = self.editor.read(cx).selected_range();
        if let Some(next) = format::next_occurrence(&text, selection) {
            self.editor.update(cx, |editor, cx| {
                editor.set_selected_range(next, cx);
                editor.focus(window, cx);
            });
        }
    }

    /// Ctrl+Shift+K: delete the lines the selection touches.
    pub fn delete_lines(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        let selection = self.editor.read(cx).selected_range();
        if text.is_empty() {
            return;
        }
        self.apply(format::delete_lines(&text, selection), window, cx);
        self.editor
            .update(cx, |editor, cx| editor.focus(window, cx));
    }

    /// Copy or Cut with nothing selected takes the caret's whole line, as in
    /// VS Code. Returns whether it was handled.
    fn copy_whole_line(&mut self, cut: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let editor = self.editor.read(cx);
        let selection = editor.selected_range();
        if !selection.is_empty() || !self.editor.focus_handle(cx).is_focused(window) {
            return false;
        }
        let text = editor.value().to_string();
        let (_, line) = format::whole_line(&text, selection.start);
        *LINE_CLIPBOARD.lock().unwrap() = Some(line.clone());
        cx.write_to_clipboard(ClipboardItem::new_string(line));
        if cut && !text.is_empty() {
            self.apply(format::cut_whole_line(&text, selection.start), window, cx);
        }
        true
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
                    // A line copied with nothing selected goes in above the
                    // caret's line, whole (VS Code).
                    let copied_line = LINE_CLIPBOARD.lock().unwrap().clone();
                    if selection.is_empty()
                        && copied_line.as_deref() == Some(string.text().as_str())
                    {
                        let edit = format::paste_line_above(&text, selection.start, string.text());
                        self.apply(edit, window, cx);
                        return true;
                    }
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
    #[allow(dead_code)]
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
            && self.live.is_none()
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
        let state = if self.reduced {
            LargeFileState::Reduced
        } else if self.analysis_task.is_some() {
            LargeFileState::Building
        } else {
            LargeFileState::Enabled
        };
        // The window's own text colours, which every built-in theme keeps
        // legible (see `the_large_file_bar_is_legible_in_every_theme`).
        h_flex()
            .id("large-file-bar")
            .w_full()
            .flex_none()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .text_sm()
            .text_color(cx.theme().foreground)
            .child(
                gpui_kit::component::Icon::new(gpui_kit::component::IconName::Info)
                    .text_color(cx.theme().primary),
            )
            .child(div().flex_1().child(large_file_message(megabytes, state)))
            .when(self.reduced, |bar| {
                bar.child(
                    Button::new("enable-large-file-features")
                        .label("Turn on preview and outline")
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| this.enable_anyway(cx))),
                )
            })
            .child(
                Button::new("dismiss-large-file-bar")
                    .ghost()
                    .icon(gpui_kit::assets::IconName::X)
                    .tooltip("Dismiss")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.bar_dismissed = true;
                        cx.notify();
                    })),
            )
    }

    fn render_editor(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let document = cx.weak_entity();
        let settings = AppSettings::get(cx);
        let focus_mode = settings.focus_mode;
        let show_minimap = settings.minimap && !focus_mode;
        let (pad_left, pad_right) = editor_side_padding(
            self.pane_width(self.editor_width, window, cx),
            cx.theme().font_size,
            focus_mode,
            settings.centered_layout,
        );
        let (menu_document, paste_document) = (document.clone(), document.clone());
        let editor_pane = div()
            .id("editor-pane")
            .key_context(crate::actions::EDITOR_CONTEXT)
            .relative()
            .size_full()
            .bg(cx.theme().background)
            .font_family(cx.theme().mono_font_family.clone())
            // The margins beside the text column belong to the editor too:
            // the editor handles the wheel over its text, this handles the
            // rest so the whole pane scrolls.
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                this.set_scroll_driver(ScrollDriver::Editor);
                // Dragging down the line numbers selects whole lines.
                if this.gutter_anchor.is_some() {
                    if event.pressed_button != Some(MouseButton::Left) {
                        this.gutter_anchor = None;
                        return;
                    }
                    let document = cx.entity();
                    window.defer(cx, move |_, cx| {
                        document.update(cx, |this, cx| this.extend_gutter_selection(cx));
                    });
                }
            }))
            // A click on a line number selects that line, as in VS Code;
            // Shift extends the selection by whole lines. The editor moves
            // the caret to the line first, then this widens it.
            .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, window, cx| {
                this.gutter_anchor = None;
                this.gutter_press = None;
                if event.button != MouseButton::Left {
                    return;
                }
                // Fold chevrons sit beside the digits; handle them before
                // whole-line selection so a chevron click toggles instead of
                // selecting the heading line.
                if this.in_fold_column(event.position, window, cx) {
                    if this.toggle_fold_at_point(event.position, cx) {
                        cx.stop_propagation();
                        window.prevent_default();
                    }
                    return;
                }
                if !this.in_line_numbers(event.position, window, cx) {
                    return;
                }
                // Let the editor hit-test the press (it knows soft-wrap rows,
                // folds and the scroll position), then widen its caret to the
                // whole logical line once the press has been handled.
                let previous = this.editor.read(cx).selected_range();
                this.gutter_press = Some((previous, event.modifiers.shift));
                let document = cx.entity();
                window.defer(cx, move |_, cx| {
                    document.update(cx, |this, cx| this.start_gutter_selection(cx));
                });
            }))
            .capture_any_mouse_up(cx.listener(|this, _: &MouseUpEvent, window, cx| {
                if this.gutter_anchor.is_none() && this.gutter_press.is_none() {
                    return;
                }
                // The editor's own click handling may collapse the selection
                // on release; put the whole-line selection back after it.
                let document = cx.entity();
                window.defer(cx, move |_, cx| {
                    document.update(cx, |this, cx| {
                        if this.gutter_press.is_some() {
                            this.start_gutter_selection(cx);
                        }
                        this.extend_gutter_selection(cx);
                        this.gutter_anchor = None;
                    });
                });
            }))
            // A file dragged from the sidebar onto the text inserts a
            // Markdown link to it (VS Code's drop-into-editor); drops
            // elsewhere in the window open the file instead.
            .on_drop(cx.listener(Self::on_drop_tree_files))
            .drag_over::<TreeDrag>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            .capture_key_down(cx.listener(|this, _: &KeyDownEvent, _, _| {
                this.set_scroll_driver(ScrollDriver::Editor)
            }))
            .capture_action(cx.listener(|this, _: &Copy, window, cx| {
                if this.copy_whole_line(false, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Cut, window, cx| {
                if this.copy_whole_line(true, window, cx) {
                    cx.stop_propagation();
                }
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
                    .pl(pad_left)
                    .pr(pad_right)
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
                        if this.editor_scroll_target.is_some() {
                            this.scroll_editor_to_target(cx);
                        }
                        let resized = (this.editor_width - bounds.size.width).abs() >= px(1.);
                        if resized {
                            // A drag of the split re-wraps the lines: go back
                            // to the line that was at the top (or the caret's)
                            // rather than keep a pixel offset.
                            if let Some(line) = this.shown_line.filter(|_| {
                                this.editor_width > px(0.) && this.editor_scroll_target.is_none()
                            }) {
                                this.editor_scroll_target =
                                    Some(EditorScrollTarget { line, attempts: 0 });
                            }
                        } else if this.editor_scroll_target.is_none() {
                            this.shown_line = this.anchor_line(cx);
                        }
                        if resized {
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
            .child(self.render_other_match_outlines(cx));
        if !show_minimap {
            return editor_pane.into_any_element();
        }
        h_flex()
            .id("editor-with-minimap")
            .size_full()
            .min_w_0()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .size_full()
                    .child(editor_pane),
            )
            .child(self.render_minimap(cx))
            .into_any_element()
    }

    /// A thin overview strip: line density + click-to-jump (Toggle Minimap).
    fn render_minimap(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let editor = self.editor.read(cx);
        let rope = editor.text();
        let line_count = rope.lines_len().max(1);
        let slots = 120usize.min(line_count);
        let per = (line_count as f32 / slots as f32).max(1.);
        let source = self.source.as_str();
        let mut slots_data = Vec::with_capacity(slots);
        for slot in 0..slots {
            let start_line = ((slot as f32) * per).floor() as usize;
            let end_line = (((slot as f32 + 1.) * per).floor() as usize).min(line_count);
            let mut density = 0usize;
            for line_ix in start_line..end_line {
                let start = rope.line_start_offset(line_ix);
                let end = if line_ix + 1 < line_count {
                    rope.line_start_offset(line_ix + 1)
                } else {
                    rope.len()
                };
                let slice = &source[start.min(source.len())..end.min(source.len())];
                density += slice.chars().filter(|c| !c.is_whitespace()).count().min(40);
            }
            let alpha = (0.15 + (density as f32 / (40. * per)).min(1.) * 0.7).clamp(0.15, 0.85);
            slots_data.push((start_line + 1, alpha));
        }
        v_flex()
            .id("editor-minimap")
            .w(px(56.))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.sidebar.opacity(0.6))
            .py_1()
            .overflow_hidden()
            .children(
                slots_data
                    .into_iter()
                    .enumerate()
                    .map(|(slot, (jump_line, alpha))| {
                        div()
                            .id(("minimap-slot", slot))
                            .flex_1()
                            .mx_1()
                            .rounded(px(1.))
                            .bg(theme.foreground.opacity(alpha))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.go_to_line(jump_line, window, cx);
                            }))
                    }),
            )
    }

    /// Find's other matches: the editor paints them as a faint, desaturated
    /// copy of the selection that all but vanishes on light backgrounds.
    /// Outline each one in the theme's warning (amber) color, without
    /// covering the glyphs; the current match keeps the selection fill.
    fn render_other_match_outlines(&self, cx: &App) -> impl IntoElement {
        let editor = self.editor.read(cx);
        let session = editor.search_session();
        let mut boxes = Vec::new();
        if session.open && !session.query.is_empty() {
            let viewport = editor.input_bounds();
            let ranges = session.matcher.matched_ranges();
            for range in other_matches(&ranges, session.matcher.current_match_index()) {
                let Some(bounds) = editor.range_to_bounds(range) else {
                    continue;
                };
                if bounds.intersects(&viewport) {
                    boxes.push(bounds.intersect(&viewport));
                }
                if boxes.len() >= MAX_MATCH_OUTLINES {
                    break;
                }
            }
        }
        let color = cx.theme().warning;
        canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                for bounds in &boxes {
                    window.paint_quad(gpui_kit::outline(*bounds, color, Default::default()));
                }
            },
        )
        .absolute()
        .inset_0()
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
        if self.loading {
            return h_flex()
                .size_full()
                .justify_center()
                .items_center()
                .gap_2()
                .text_color(cx.theme().muted_foreground)
                .child(gpui_kit::component::spinner::Spinner::new())
                .child(format!("Loading \u{201c}{}\u{201d}\u{2026}", self.name()))
                .into_any_element();
        }
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
            Layout::Preview => self.render_side(window, cx),
            Layout::Split => h_resizable("document-split")
                .child(
                    resizable_panel()
                        .size_range(rems(16.).to_pixels(window.rem_size())..Pixels::MAX)
                        .child(
                            div()
                                .size_full()
                                .min_w_0()
                                .min_h_0()
                                .child(self.render_editor(window, cx)),
                        ),
                )
                .child(
                    resizable_panel()
                        .size_range(rems(16.).to_pixels(window.rem_size())..Pixels::MAX)
                        .child(
                            div()
                                .size_full()
                                .min_w_0()
                                .min_h_0()
                                .child(self.render_side(window, cx)),
                        ),
                )
                .into_any_element(),
        };
        // min_w_0: flex parents default to min-width:auto, which lets the PDF
        // toolbar/pages force the split pane past the window and clip the right edge.
        let body = div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .on_action(cx.listener(|_, action: &crate::actions::SetLayout, _, cx| {
                log::info!("ui: DocumentView SetLayout -> {:?}", action.0);
                AppSettings::update(cx, |settings| {
                    settings.layout = action.0;
                    settings.focus_mode = false;
                });
            }))
            .on_action(
                cx.listener(|_, _: &crate::actions::ToggleFileSidebar, _, cx| {
                    AppSettings::update(cx, |settings| {
                        settings.file_sidebar = !settings.file_sidebar;
                    });
                }),
            )
            .child(body)
            .into_any_element();
        if !self.large || self.bar_dismissed {
            return body;
        }
        v_flex()
            .size_full()
            .min_w_0()
            .child(self.render_large_file_bar(cx))
            .child(div().flex_1().min_h_0().min_w_0().child(body))
            .into_any_element()
    }
}

/// How long typing pauses before a Typst source is compiled again. Short:
/// a compile of a long document takes a few milliseconds once warm.
const LIVE_DEBOUNCE: Duration = Duration::from_millis(40);
/// Ignore a second jump-to-source at the same offset within this window.
const JUMP_DEDUPE: Duration = Duration::from_millis(350);
/// How long the caret rests before the preview scrolls to show it.
const LIVE_REVEAL_DELAY: Duration = Duration::from_millis(150);

/// One row in the Problems panel.
#[derive(Clone, Debug)]
pub struct ProblemItem {
    pub message: String,
    pub line: Option<usize>,
    pub error: bool,
    pub warning: bool,
}

/// A Typst source's live preview.
struct TypstLive {
    worker: crate::typst_live::LiveWorker,
    view: Entity<crate::pdf_view::PdfView>,
    diagnostics: Vec<crate::typst_live::LiveDiagnostic>,
    /// Whether any compile has succeeded, so there are pages to show.
    compiled_once: bool,
    highlights: Option<TextDecorationCollection>,
    /// Syntax colours alone, so diagnostic underlines can be layered on top
    /// without waiting for another compile.
    syntax_decorations: Option<Vec<TextDecoration>>,
    /// Tags from the last compile, for recolouring when the theme changes.
    last_tags: Vec<(std::ops::Range<usize>, typst::syntax::Tag)>,
    send_task: Option<Task<()>>,
    reveal_task: Option<Task<()>>,
    revealed_cursor: Option<usize>,
    /// The newest edit sent to the compiler and when it was made, to
    /// measure the time from a keystroke to its pages.
    typed: Option<(usize, std::time::Instant)>,
    latency: Option<Duration>,
    _listen: Task<()>,
    _clicks: Subscription,
}

/// How the editor colours a piece of Typst, from the theme's syntax colours
/// (the names tree-sitter grammars use, so every theme covers them).
fn typst_tag_style(
    tag: typst::syntax::Tag,
    theme: &gpui_kit::component::highlighter::HighlightTheme,
) -> Option<HighlightStyle> {
    use gpui_kit::base::input::HighlightStyleResolver as _;
    use typst::syntax::Tag;
    let (name, bold, italic) = match tag {
        Tag::Comment => ("comment", false, false),
        Tag::Punctuation => ("punctuation", false, false),
        Tag::Escape => ("string.escape", false, false),
        Tag::Strong => ("", true, false),
        Tag::Emph => ("", false, true),
        Tag::Link => ("link_uri", false, false),
        Tag::Raw => ("text.literal", false, false),
        Tag::Label | Tag::Ref => ("constant", false, false),
        Tag::Heading => ("title", true, false),
        Tag::ListMarker => ("punctuation.list_marker", false, false),
        Tag::ListTerm => ("", true, false),
        Tag::MathDelimiter | Tag::MathGroupingParens => ("punctuation.bracket", false, false),
        Tag::MathOperator | Tag::Operator => ("operator", false, false),
        Tag::Keyword => ("keyword", false, false),
        Tag::Number => ("number", false, false),
        Tag::String => ("string", false, false),
        Tag::Function => ("function", false, false),
        Tag::Interpolated => ("variable", false, false),
        Tag::Error => return None,
    };
    let mut style = if name.is_empty() {
        HighlightStyle::default()
    } else {
        theme.style(name).unwrap_or_default()
    };
    if bold {
        style.font_weight = Some(gpui_kit::FontWeight::BOLD);
    }
    if italic {
        style.font_style = Some(gpui_kit::FontStyle::Italic);
    }
    (style != HighlightStyle::default()).then_some(style)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LargeFileState {
    Reduced,
    Building,
    Enabled,
}

/// What the large-file info bar says. It never promises highlighting or
/// spell checking: those stay off for a large file.
fn large_file_message(megabytes: f64, state: LargeFileState) -> String {
    match state {
        LargeFileState::Reduced => format!(
            "Large file ({megabytes:.1} MB): preview and outline are off to save memory; \
             highlighting and spell checking stay off at this size."
        ),
        LargeFileState::Building => {
            format!("Large file ({megabytes:.1} MB): building the outline and preview…")
        }
        LargeFileState::Enabled => format!(
            "Large file ({megabytes:.1} MB): preview (of the beginning) and outline are on; \
             highlighting and spell checking stay off at this size."
        ),
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
/// Line numbers show unless turned off, and never in focus mode — the same
/// rule for a document opened in focus mode as for one already open.
fn show_line_numbers(settings: &crate::settings::Settings) -> bool {
    settings.line_numbers && !settings.focus_mode
}

/// Must match gpui-base `LINE_NUMBER_RIGHT_MARGIN` (malgel-fold-0.7.0).
const FOLD_COLUMN_MARGIN: f32 = 8.;
/// Must match gpui-base `FOLD_ICON_HITBOX_WIDTH` (malgel-fold-0.7.0).
const FOLD_COLUMN_HITBOX: f32 = 24.;

/// The folding column: everywhere but focus mode. Plain text (.txt, .typ)
/// has nothing to fold, but keeps the empty column so the gap between the
/// line numbers and the text matches Markdown and code (~1.5ch, G9).
fn shows_folding(focus_mode: bool) -> bool {
    !focus_mode
}

/// The editor's left and right padding in a pane `width` wide. Normally the
/// line numbers sit at the pane's left edge and the text runs to the right
/// edge, as in VS Code; focus mode and the centered layout center a
/// readable column instead.
fn editor_side_padding(
    width: Pixels,
    rem: Pixels,
    focus_mode: bool,
    centered: bool,
) -> (Pixels, Pixels) {
    if focus_mode || centered {
        let inset = ((width - rem * READABLE_WIDTH_REMS) / 2.).max(rem * 1.5);
        return (inset, inset);
    }
    (px(EDITOR_EDGE), px(EDITOR_EDGE))
}

/// The whole line holding byte `offset`, its line break included (`\r\n`
/// too); the last line has none. What a click on a line number selects.
fn line_range_at(text: &str, offset: usize) -> Range<usize> {
    let offset = offset.min(text.len());
    let start = text[..offset].rfind('\n').map_or(0, |at| at + 1);
    let end = text[offset..]
        .find('\n')
        .map_or(text.len(), |at| offset + at + 1);
    start..end
}

/// What a press on the line numbers selects once the editor has put the
/// caret at `caret` (its hit test maps wrapped rows and folds to an offset):
/// the caret's whole logical line, or with Shift that line through the
/// lines of the `previous` selection.
fn gutter_selection(text: &str, caret: usize, previous: Range<usize>, shift: bool) -> Range<usize> {
    let clicked = line_range_at(text, caret);
    if shift {
        let other = line_range_at(text, previous.start);
        other.start.min(clicked.start)..other.end.max(clicked.end)
    } else {
        clicked
    }
}

/// A gutter drag from the `anchor` lines to the caret at `caret`. A caret at
/// `anchor.end` is the anchor's own selection end (the start of the line
/// after it), not a move onto the next line.
fn extend_line_selection(text: &str, anchor: &Range<usize>, caret: usize) -> Range<usize> {
    if caret == anchor.end && anchor.end > anchor.start {
        return anchor.clone();
    }
    let line = line_range_at(text, caret);
    anchor.start.min(line.start)..anchor.end.max(line.end)
}

/// Outlines drawn at most per frame; more matches than this on screen only
/// happen in a tiny font, where the fill alone is enough.
const MAX_MATCH_OUTLINES: usize = 400;

/// Every match but the current one, in document order.
fn other_matches(ranges: &[Range<usize>], current: usize) -> impl Iterator<Item = &Range<usize>> {
    ranges
        .iter()
        .enumerate()
        .filter(move |(ix, _)| *ix != current)
        .map(|(_, range)| range)
}

/// The last line copied whole (nothing selected): pasting exactly that text
/// with nothing selected puts it in as a line above the caret.
static LINE_CLIPBOARD: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The first line to show when `rows` were on screen and the caret is on
/// line `caret`: the caret keeps its distance from the top when visible.
fn anchor_for(rows: Range<usize>, caret: usize) -> f32 {
    if rows.contains(&caret) {
        // Keep a little context above the caret so a layout change that
        // half-clips the top line does not hide it (split → Editor only).
        let from_top = caret - rows.start;
        let offset = from_top.clamp(2, 8);
        caret.saturating_sub(offset) as f32
    } else {
        rows.start as f32
    }
}

/// The top line that puts `line` in the middle of `shown` lines.
fn centered_top(line: usize, shown: usize) -> f32 {
    line.saturating_sub(shown / 2) as f32
}

/// How many digits the gutter keeps room for: three at least, up to seven,
/// as the editor lays it out.
fn line_number_digits(lines: usize) -> usize {
    (lines.max(1).ilog10() as usize + 1).clamp(3, 7)
}

/// Room between the pane's edges and the gutter / text, in pixels.
const EDITOR_EDGE: f32 = 4.;

fn column_inset(width: Pixels, cx: &App) -> Pixels {
    let rem = cx.theme().font_size;
    let min = rem * 1.5;
    let column = rem * READABLE_WIDTH_REMS;
    ((width - column) / 2.).max(min)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_gutter_sits_at_the_left_edge_whatever_the_width() {
        use super::{editor_side_padding, px};
        let rem = px(16.);
        for width in [px(800.), px(2000.)] {
            let (left, right) = editor_side_padding(width, rem, false, false);
            assert!(left <= px(4.) && right <= px(4.), "{width:?}");
        }
        let (left, _) = editor_side_padding(px(2000.), rem, false, true);
        assert!(left > px(200.), "centered layout centers");
        let (left, _) = editor_side_padding(px(2000.), rem, true, false);
        assert!(left > px(200.), "focus mode centers");
    }

    #[test]
    fn plain_text_files_do_not_fold_as_markdown() {
        // G8: .txt used to fall through highlight_language to "markdown".
    }

    #[test]
    fn a_layout_switch_keeps_the_caret_line_and_go_to_line_centers() {
        use super::{anchor_for, centered_top};
        assert_eq!(anchor_for(100..140, 120), 112.);
        assert_eq!(anchor_for(100..140, 102), 100.);
        assert_eq!(anchor_for(100..140, 500), 100.);
        assert_eq!(centered_top(200, 40), 180.);
        assert_eq!(centered_top(3, 40), 0.);
    }

    #[test]
    fn find_outlines_every_match_but_the_current_one() {
        let ranges = vec![0..3, 10..13, 20..23];
        let others: Vec<_> = super::other_matches(&ranges, 1).cloned().collect();
        assert_eq!(others, vec![0..3, 20..23]);
        assert_eq!(super::other_matches(&ranges, 9).count(), 3);
    }

    #[test]
    fn a_line_number_click_selects_the_whole_line() {
        use super::line_range_at;
        let text = "one\ntwo\nthree";
        assert_eq!(&text[line_range_at(text, 5)], "two\n");
        assert_eq!(&text[line_range_at(text, 4)], "two\n");
        assert_eq!(&text[line_range_at(text, text.len())], "three");
        let crlf = "a\r\nb\r\n";
        assert_eq!(&crlf[line_range_at(crlf, 0)], "a\r\n");
        // A press on a soft-wrap continuation row: the editor's hit test puts
        // the caret mid-line, and the whole logical line is selected.
        let wrapped = format!("head\n{}\ntail", "word ".repeat(60));
        let caret = 5 + 140;
        let line = super::gutter_selection(&wrapped, caret, 0..0, false);
        assert_eq!(&wrapped[line.clone()], format!("{}\n", "word ".repeat(60)));
        // Shift extends through the previous selection's lines.
        let both = super::gutter_selection(&wrapped, caret, 0..0, true);
        assert_eq!(both.start, 0);
        assert_eq!(both.end, line.end);
        // Releasing without a drag leaves the caret at the selection end:
        // that must not pull in the following line.
        assert_eq!(
            super::extend_line_selection(&wrapped, &line, line.end),
            line
        );
        let tail = super::extend_line_selection(&wrapped, &line, wrapped.len());
        assert_eq!(tail, line.start..wrapped.len());
        assert_eq!(super::line_number_digits(9), 3);
        assert_eq!(super::line_number_digits(12_000), 5);
    }

    #[test]
    fn fold_column_hitbox_matches_gpui_base_patch() {
        // Keep Malgel's gutter hit-test aligned with gpui-base fold layout.
        assert_eq!(super::FOLD_COLUMN_MARGIN, 8.);
        assert_eq!(super::FOLD_COLUMN_HITBOX, 24.);
    }

    #[test]
    fn line_numbers_and_folding_follow_the_settings() {
        use super::{show_line_numbers, shows_folding};
        let mut settings = crate::settings::Settings::default();
        assert!(show_line_numbers(&settings));
        settings.focus_mode = true;
        assert!(!show_line_numbers(&settings));
        settings.focus_mode = false;
        settings.line_numbers = false;
        assert!(!show_line_numbers(&settings));
        // Plain text keeps the (empty) column so its gutter gap matches.
        assert!(shows_folding(false));
        assert!(!shows_folding(true));
    }

    #[test]
    fn the_large_file_bar_promises_only_what_it_turns_on() {
        use super::{LargeFileState::*, large_file_message};
        let reduced = large_file_message(19.3, Reduced);
        assert!(reduced.contains("19.3 MB") && reduced.contains("stay off"));
        let enabled = large_file_message(19.3, Enabled);
        assert!(enabled.contains("preview") && enabled.contains("outline are on"));
        assert!(enabled.contains("highlighting and spell checking stay off"));
        assert!(large_file_message(1., Building).contains("building"));
    }

    /// The bar draws `foreground` on `background`: WCAG AA (4.5:1) in every
    /// built-in theme.
    #[test]
    fn the_large_file_bar_is_legible_in_every_theme() {
        fn luminance(hex: &str) -> f64 {
            let hex = hex.trim_start_matches('#');
            let channel = |i: usize| {
                let c = u8::from_str_radix(&hex[i..i + 2], 16).unwrap() as f64 / 255.;
                if c <= 0.04045 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(0) + 0.7152 * channel(2) + 0.0722 * channel(4)
        }
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("themes");
        let mut checked = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let json: serde_json::Value =
                serde_json::from_slice(&std::fs::read(entry.unwrap().path()).unwrap()).unwrap();
            for theme in json["themes"].as_array().unwrap() {
                let colors = &theme["colors"];
                let (fg, bg) = (
                    luminance(colors["foreground"].as_str().unwrap()),
                    luminance(colors["background"].as_str().unwrap()),
                );
                let ratio = (fg.max(bg) + 0.05) / (fg.min(bg) + 0.05);
                assert!(ratio >= 4.5, "{}: {ratio:.2}", theme["name"]);
                checked += 1;
            }
        }
        assert!(checked >= 8);
    }

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
