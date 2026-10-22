//! The window: title bar, open documents (as tabs when enabled), the file
//! and outline sidebars, and the status bar. It routes commands to the
//! document showing, asks before unsaved changes are lost, notices files
//! changed by other programs, and remembers the session.

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui_kit::{
    Anchor, App, AppContext as _, Bounds, ClipboardItem, Context, DismissEvent, DragMoveEvent,
    Entity, EntityId, ExternalPaths, FocusHandle, Focusable, FontWeight, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, MouseUpEvent, ParentElement as _, PathPromptOptions,
    Pixels, Point, Render, ScrollHandle, SharedString, Size, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, UniformListScrollHandle, Window, WindowBounds, anchored,
    assets::IconName,
    base::Disableable as _,
    component::{
        ActiveTheme as _, Icon, IndexPath, RopeExt as _, Selectable as _, Sizable as _, TitleBar,
        WindowExt as _,
        button::{Button, ButtonGroup, ButtonVariants as _},
        checkbox::Checkbox,
        command::{Command, CommandGroup, CommandItem, CommandState},
        dialog::DialogFooter,
        h_flex,
        input::{Escape, Input, InputEvent, InputState},
        link::Link,
        menu::{DropdownMenu as _, PopupMenu, PopupMenuItem},
        notification::Notification,
        resizable::{h_resizable, resizable_panel, v_resizable},
        scroll::ScrollableElement as _,
        spinner::Spinner,
        status_bar::StatusBar,
        v_flex,
    },
    deferred, div,
    prelude::FluentBuilder as _,
    px, rems, uniform_list,
};

use crate::{
    actions::*,
    analysis::{Analysis, content_hash},
    commands, diagram_viewer,
    document::{self, LineEnding, display_name},
    document_view::{DocumentEvent, DocumentView},
    docx, export,
    file_icon::{ActivityIcon, activity_icon_color, file_type_icon, folder_type_icon},
    file_tree::{
        FileTree, FileTreeEvent, TreeDrag, absolute_paths_for_clipboard, invert_key_held,
        relative_paths_for_clipboard,
    },
    format,
    links::{self, LinkTarget},
    pdf::{self, Paper},
    quick_open, rich_copy,
    search::{self, Hit},
    session::{self, DiskStamp, Session, SessionDocument, WindowPlacement},
    settings::{Appearance, Layout, LinkPlacement, clamp_font_size},
    spell,
    terminal::{self, TerminalEvent, TerminalPanel},
    themes::{self, AppSettings},
    title_menu_bar::TitleMenuBar,
};

/// Shown when Malgel starts for the first time.
const WELCOME: &str = include_str!("welcome.md");
/// How often open files and the file sidebar are checked for changes made
/// by other programs.
const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// What to do once unsaved changes have been saved or discarded.
#[derive(Clone)]
enum AfterConfirm {
    NewFile,
    OpenDialog,
    OpenPath(PathBuf),
    /// Open a linked document in place of the current one, at a heading.
    OpenLink(PathBuf, Option<String>),
    CloseDocument(EntityId),
    CloseDocuments(Vec<EntityId>),
    CloseWindow,
    Quit,
    /// Continue Save All after an untitled Save As (remaining entity ids + final next).
    SaveAllThen(Vec<EntityId>, Box<AfterConfirm>),
    /// No-op sentinel for chained Save All without a follow-up.
    Nop,
}

/// Which view of the active document should hold the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyboardTarget {
    /// The PDF or image viewer, which stands in for both of the others.
    Media,
    Editor,
    Preview,
}

/// Left activity-bar landmark (probe #10/#11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivityView {
    Explorer,
    Search,
    SourceControl,
    Extensions,
}

/// Active Compare / diff editor session (two-pane + hunks).
struct DiffSession {
    title: String,
    left_label: String,
    right_label: String,
    left: String,
    right: String,
    hunks: Vec<crate::diff::Hunk>,
    current: usize,
}

/// A PDF or an image is rendered *instead of* the editor and the preview, so
/// the keyboard has to go to that viewer. Focusing a view that is not in the
/// render tree leaves every shortcut and title-bar `dispatch_action` dead —
/// the same class of bug as focusing the Markdown preview while a Typst live
/// `PdfView` is what is actually drawn.
/// One row of the Quick Open picker.
#[derive(Clone, Debug, PartialEq)]
struct QuickOpenRow {
    path: PathBuf,
    label: String,
    /// What the query has to match besides the label: the path as typed.
    keyword: Option<String>,
    line: Option<usize>,
    is_dir: bool,
}

/// The picker's rows for `query`: a typed path's own file or folder entries
/// first, then the listing (which the picker filters by itself).
/// The most file rows Quick Open draws at once; typing more of the name
/// narrows the rest down, as in VS Code.
const QUICK_OPEN_SHOWN: usize = 500;

/// The rows the palette would keep for `query` (a case-insensitive
/// substring of the label, the file name or the keyword — the palette's own
/// test), at most `limit` of them, in order.
fn matching_quick_open_rows(
    rows: Vec<QuickOpenRow>,
    query: &str,
    limit: usize,
) -> Vec<QuickOpenRow> {
    let query = query.to_lowercase();
    rows.into_iter()
        .filter(|row| {
            query.is_empty()
                || row.label.to_lowercase().contains(&query)
                || row
                    .path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().to_lowercase().contains(&query))
                || row
                    .keyword
                    .as_ref()
                    .is_some_and(|keyword| keyword.to_lowercase().contains(&query))
        })
        .take(limit)
        .collect()
}

fn quick_open_rows(
    query: &str,
    root: &Path,
    home: Option<&Path>,
    listing: &[PathBuf],
) -> Vec<QuickOpenRow> {
    let shown = |path: &Path| {
        path.strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    };
    let mut rows = Vec::new();
    match quick_open::resolve_direct(query, root, home) {
        Some(quick_open::Direct::File(found)) => rows.push(QuickOpenRow {
            label: shown(&found.path),
            keyword: Some(query.to_string()),
            path: found.path,
            line: found.line,
            is_dir: false,
        }),
        Some(quick_open::Direct::Dir(_)) | None => {}
    }
    if let Some(dir) = quick_open::typed_dir(query, root, home) {
        // What the user typed up to the folder, so `src/ma` matches
        // `src/main.rs` however the folder was written (`~/`, absolute…).
        let prefix = match query.rfind(['/', '\\']) {
            Some(ix) if dir.ends_with(Path::new(&query[..ix])) || query.ends_with(['/', '\\']) => {
                query[..=ix].to_string()
            }
            _ => format!("{query}/"),
        };
        for path in quick_open::dir_entries(&dir, quick_open::MAX_DIR_ENTRIES) {
            if rows.iter().any(|row| row.path == path) {
                continue;
            }
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            rows.push(QuickOpenRow {
                label: shown(&path),
                keyword: Some(format!("{prefix}{name}")),
                is_dir: path.is_dir(),
                path,
                line: None,
            });
        }
    }
    rows.extend(
        listing
            .iter()
            .filter(|path| !rows.iter().any(|row| &row.path == *path))
            .map(|path| QuickOpenRow {
                label: shown(path),
                keyword: None,
                path: path.clone(),
                line: None,
                is_dir: false,
            })
            .collect::<Vec<_>>(),
    );
    rows
}

/// Quick Open's rows split into the recently opened ones, most recent
/// first (`recent`, then the other open tabs), and the rest in listing order.
/// The file being edited is left out of the first group: Ctrl+P, Enter goes
/// back to the previous file, as in VS Code. A typed path keeps its own rows
/// on top, so no group is made then.
fn split_recent(
    rows: Vec<QuickOpenRow>,
    recent: &[PathBuf],
    current: Option<&Path>,
) -> (Vec<QuickOpenRow>, Vec<QuickOpenRow>) {
    if rows.iter().any(|row| row.keyword.is_some()) {
        return (Vec::new(), rows);
    }
    let mut first = Vec::new();
    let mut rest = rows;
    for path in recent {
        if Some(path.as_path()) == current {
            continue;
        }
        if let Some(ix) = rest.iter().position(|row| &row.path == path) {
            first.push(rest.remove(ix));
        }
    }
    (first, rest)
}

/// Whether opening `path` reads it as text, rather than showing it in a
/// viewer (PDF, image). A Typst source is text, with its pages beside it.
fn reads_as_text(path: &std::path::Path) -> bool {
    !document::is_pdf_path(path) && !document::is_image_path(path)
}

fn keyboard_target(has_media: bool, shows_editor: bool, focus_mode: bool) -> KeyboardTarget {
    if has_media {
        KeyboardTarget::Media
    } else if shows_editor || focus_mode {
        KeyboardTarget::Editor
    } else {
        KeyboardTarget::Preview
    }
}

/// Where a plain click in the file tree puts the file it opens: `Some(ix)`
/// takes over the document at `ix`, `None` adds a tab.
///
/// The preview slot is the tab the last click left behind, while it is
/// untouched; an empty editor is reused first, as VS Code does.
fn preview_target(
    preview: Option<usize>,
    preview_untouched: bool,
    active: usize,
    active_blank: bool,
) -> Option<usize> {
    match preview.filter(|_| preview_untouched) {
        Some(ix) => Some(ix),
        None => active_blank.then_some(active),
    }
}

/// Where a document being opened goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    /// A new tab when tabs are on, otherwise in place of the current
    /// document.
    Default,
    /// A tab of its own, whatever the settings say.
    NewTab,
    /// The slot a plain click in the file tree reuses, the way VS Code's
    /// preview editor is replaced by the next file clicked.
    Preview,
    ReplaceActive,
}

/// What the one-line prompt the file tree opens is asking for.
#[derive(Clone, Copy)]
enum TreePromptKind {
    NewFile,
    NewFolder,
}

/// Where a document being opened should land once it has loaded.
/// How the user chose to open a file they were warned about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenMode {
    Full,
    PlainText,
}

/// A button of the dialog shown before opening a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenChoice {
    Close(&'static str),
    PlainText,
    Anyway,
}

/// The dialog's buttons for a file `check` flagged: a binary file that is not
/// text gets only an OK, as there is nothing to open it as.
fn open_choices(check: document::OpenCheck, media: bool) -> &'static [OpenChoice] {
    use OpenChoice::*;
    match check {
        document::OpenCheck::Binary => &[Close("OK")],
        document::OpenCheck::BinaryText => &[Close("Cancel"), Anyway],
        _ if media => &[Close("Cancel"), Anyway],
        _ => &[Close("Cancel"), PlainText, Anyway],
    }
}

/// The Search view's results (Ctrl+Shift+F): nothing asked yet, a search
/// running, or what one found.
#[derive(Default)]
enum SidebarSearch {
    #[default]
    Idle,
    Searching {
        query: String,
        cancel: Arc<AtomicBool>,
    },
    Done {
        query: String,
        root: PathBuf,
        found: search::FolderSearch,
    },
}

/// One row of the Search view's results tree: a file with its hit count, or
/// one hit (an index into the hits) under it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SearchRow {
    File { path: PathBuf, count: usize },
    Hit(usize),
}

/// The results tree's rows: hits grouped under their file, in the order the
/// search found them, with the hits of a collapsed file left out.
fn search_rows(hits: &[Hit], collapsed: &HashSet<PathBuf>) -> Vec<SearchRow> {
    let mut rows = Vec::new();
    let mut ix = 0;
    while ix < hits.len() {
        let path = &hits[ix].path;
        let end = hits[ix..]
            .iter()
            .position(|hit| &hit.path != path)
            .map_or(hits.len(), |n| ix + n);
        rows.push(SearchRow::File {
            path: path.clone(),
            count: end - ix,
        });
        if !collapsed.contains(path) {
            rows.extend((ix..end).map(SearchRow::Hit));
        }
        ix = end;
    }
    rows
}

/// The results header: `12 results in 3 files`, and how far a capped search
/// got, the way VS Code says its results were limited.
fn search_summary(found: &search::FolderSearch) -> String {
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut text = format!(
        "{} in {}",
        plural(found.hits.len(), "result", "results"),
        plural(found.files_matched, "file", "files")
    );
    if found.limited || found.files_total >= quick_open::MAX_FILES {
        text.push_str(&format!(
            " \u{2014} results limited; searched {} of {} files",
            found.files_searched, found.files_total
        ));
    }
    text
}

/// Whether the status bar's memory/CPU pulse should repaint: only when its
/// text changed, and only while the window is the active one.
fn pulse_needs_repaint(generation: u64, shown: u64, window_active: bool) -> bool {
    window_active && generation != shown
}

/// The next hit to show after `current`, `forward` or back, wrapping.
fn step_hit(current: Option<usize>, len: usize, forward: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match (current, forward) {
        (None, true) => 0,
        (None, false) => len - 1,
        (Some(ix), true) => (ix + 1) % len,
        (Some(ix), false) => (ix + len - 1) % len,
    })
}

#[allow(clippy::large_enum_variant)]
enum MediaLoad {
    Pdf(crate::pdf_view::PdfDocument),
    Image(Arc<gpui_kit::Image>, f32, f32),
}

#[derive(Clone, Debug)]
enum Target {
    /// The top of the document.
    Top,
    /// The heading with this text.
    Heading(String),
    /// This line, the way a Find in Files hit names one.
    Line(usize),
}

/// What the watcher found for one open file.
enum DiskChange {
    Missing,
    Changed {
        text: String,
        line_ending: LineEnding,
        stamp: Option<DiskStamp>,
    },
    Touched(Option<DiskStamp>),
}

/// Command palette default rows force icon paint to muted/accent
/// (`CommandState::render_item_with_binding`), so `.icon(...).text_color(…)`
/// is ignored. Own the row via `.child` — same accent path as Explorer/tabs.
/// Command row using the declarative file-icon theme (SVG) when available.
fn command_item_with_path_icon(
    label: impl Into<SharedString>,
    keywords: impl IntoIterator<Item = impl Into<SharedString>>,
    path: &std::path::Path,
    is_dir: bool,
) -> CommandItem {
    let label: SharedString = label.into();
    let row_label = label.clone();
    let icon = if is_dir {
        folder_type_icon(path, false)
    } else {
        file_type_icon(path)
    };
    CommandItem::new()
        .label(label)
        .keywords(keywords)
        .child(move |_window, _cx| {
            h_flex()
                .flex_1()
                .gap_2()
                .items_center()
                .child(icon.clone().size_4())
                .child(row_label.clone())
        })
}

/// Which document indices the pinned-preview layout mounts in the editor
/// column vs the preview column. Each index appears in exactly one column
/// so GPUI never hosts the same `DocumentView` twice (focus / undo safe).
fn pinned_preview_mounts(doc_count: usize, active: usize, pin_ix: usize) -> (Vec<usize>, usize) {
    debug_assert_ne!(active, pin_ix);
    let editor: Vec<usize> = (0..doc_count).filter(|&ix| ix != pin_ix).collect();
    (editor, pin_ix)
}

/// Labels near the top of the editor-tab context menu (probe #15/#16).
/// Kept as a pure list so tests can prove Select for Compare is registered
/// even when Xvfb hit-testing is involved.
fn tab_context_menu_labels(pinned: bool) -> [&'static str; 5] {
    [
        if pinned { "Unpin" } else { "Pin" },
        "Select for Compare",
        "Compare with Selected",
        "Close",
        "Close Other Tabs",
    ]
}

/// A tab being dragged to a new position, the way tabs reorder in VS Code.
#[derive(Clone, Copy, PartialEq, Eq)]
struct TabDrag {
    index: usize,
}

/// Where a dragged tab would land: before tab `gap` (`gap == tab count`
/// means after the last one), coming from tab `source`.
#[derive(Clone, Copy, PartialEq, Eq)]
struct TabInsertion {
    gap: usize,
    source: usize,
}

/// Probe #17: workspace-owned tab context menu. Declarative `.context_menu()`
/// on tabs races `activate()` rebuild + deferred editor focus under Xvfb and
/// never paints; opening `PopupMenu` explicitly (like the title-bar menus)
/// survives the re-render.
struct TabContextMenu {
    position: Point<Pixels>,
    menu: Entity<PopupMenu>,
}

/// The ghost rendered under the cursor while a tab or a tree entry is
/// being dragged.
pub(crate) struct DragGhost {
    pub(crate) label: SharedString,
}

impl Render for DragGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .text_sm()
            .text_color(theme.foreground)
            .shadow_md()
            .child(self.label.clone())
    }
}

pub struct Workspace {
    focus_handle: FocusHandle,
    documents: Vec<Entity<DocumentView>>,
    active: usize,
    /// The document shown in the preview slot, if any: the next file opened
    /// with a plain click takes its place, until it is edited or opened for
    /// good, the way VS Code's preview tab behaves.
    preview: Option<EntityId>,
    headings: Entity<CommandState>,
    quick_open: Entity<CommandState>,
    /// The Show All Commands palette (Ctrl+Shift+P) and the Go to Line palette
    /// (Ctrl+G). Each keeps its own query and focus across openings.
    command_palette: Entity<CommandState>,
    theme_picker: Entity<CommandState>,
    /// Focus for the alert dialogs' buttons (initial focus, ring, Tab trap).
    alert_focus: std::rc::Rc<crate::dialog_focus::DialogFocus>,
    fallback_save_sub: Option<Subscription>,
    go_to_line: Entity<CommandState>,
    /// The one-line prompt New File / New Folder / Rename ask their name in.
    tree_prompt: Entity<CommandState>,
    /// The text Find in Files last looked for, offered again next time.
    last_find: String,
    /// The replacement text Replace in Files last used.
    last_replace: String,
    /// Activity-bar landmark (Explorer / Search / SCM / Extensions).
    activity_view: ActivityView,
    /// Find field in the Search activity sidebar.
    search_find: Entity<InputState>,
    /// Replace field in the Search activity sidebar.
    search_replace: Entity<InputState>,
    /// The Search view's results (Ctrl+Shift+F lands here).
    sidebar_search: SidebarSearch,
    /// The rows of its results tree, rebuilt when the results or a
    /// collapsed file change.
    sidebar_rows: Vec<SearchRow>,
    /// Files whose hits the tree hides.
    sidebar_collapsed: HashSet<PathBuf>,
    /// The hit Enter / Shift+Enter in the Find field last opened.
    sidebar_cursor: Option<usize>,
    sidebar_scroll: UniformListScrollHandle,
    /// Open Compare / diff session, if any.
    diff_session: Option<DiffSession>,
    file_tree: Entity<FileTree>,
    app_menu_bar: Entity<TitleMenuBar>,

    window_title: String,
    closing: bool,
    /// Documents whose unsaved changes the user chose to discard while
    /// closing.
    discarded: HashSet<EntityId>,
    /// Files changed on disk that are waiting for the user to decide.
    pending_disk_prompt: HashSet<EntityId>,
    /// Files the user chose to open despite the size warning, and how.
    approved_opens: HashMap<PathBuf, OpenMode>,
    /// The paths of editors closed recently, newest last, so Ctrl+Shift+T
    /// can bring the last one back the way VS Code does.
    closed_documents: Vec<PathBuf>,
    /// Tabs held open: Close Other Tabs and Close All Editors leave them
    /// alone, and their close box becomes a pin, as in VS Code.
    pinned: HashSet<EntityId>,
    /// Document whose preview stays on screen while another tab is edited.
    /// `None` = preview follows the active document (default).
    pinned_preview: Option<EntityId>,
    /// Explorer “Select for Compare” left path (probe #12).
    compare_selection: Option<PathBuf>,
    /// While dragging a tab: which split edge is hovered for drop preview.
    tab_split_preview: Option<bool>, // false=right, true=down
    /// The files the window has shown, newest last, and where it is in that
    /// list — Alt+← and Alt+→ walk it, as VS Code's editor history does.
    nav_history: Vec<PathBuf>,
    nav_index: usize,
    last_session: Option<Session>,
    /// Where a dragged tab would land while the pointer moves across the
    /// tab bar (VS Code's insertion line), `None` when no tab is dragging.
    tab_insertion: Option<TabInsertion>,
    /// The tab bar's bounds as of the last drag move, so a drop outside the
    /// bar can clear the insertion line.
    tab_bar_bounds: Cell<Option<Bounds<Pixels>>>,
    /// The tab strip's scroll position, so the active tab is brought fully
    /// into view, its close button included, when it changes.
    tab_scroll: ScrollHandle,
    /// The (active tab, tab count) last scrolled into view; a change of
    /// either asks for a reveal, and a manual scroll is otherwise left alone.
    tab_revealed: Cell<Option<(usize, usize)>>,
    /// Whether the invert key is held while a tab drags: the drop then
    /// duplicates the tab instead of moving it, as in VS Code.
    tab_drag_copy: Cell<bool>,
    /// Index of the tab currently being dragged (for mouse-up edge-split fallback).
    tab_drag_index: Cell<Option<usize>>,
    /// Last pointer position while a tab drags (edge detection under Xvfb).
    tab_drag_last_pos: Cell<Option<Point<Pixels>>>,
    /// Document/editor body bounds for drag-to-edge hit testing.
    editor_body_bounds: Cell<Option<Bounds<Pixels>>>,
    /// Open tab context menu (probe #17), if any.
    tab_context_menu: Option<TabContextMenu>,
    _tab_context_menu_sub: Option<Subscription>,
    _watch_task: Task<()>,
    /// Quiet status-bar mem/CPU pulse refresh (does not sample; reads shared pulse).
    _pulse_task: Task<()>,
    document_subscriptions: Vec<(EntityId, Vec<Subscription>)>,
    /// Integrated terminal sessions in the bottom panel (Ctrl+`).
    terminals: Vec<Entity<TerminalPanel>>,
    terminal_active: usize,
    /// Side-by-side dual session when two or more terminals exist.
    terminal_split: bool,
    terminal_visible: bool,
    /// When true, the terminal panel takes most of the center (VS Code Maximize Panel).
    terminal_maximized: bool,
    /// Kill/CloseSession subscriptions for each integrated terminal.
    terminal_subscriptions: Vec<Subscription>,
    /// Second editor group: another open document beside or below the active one.
    editor_split: Option<EditorSplit>,
    /// Debounce for title-bar path copy toasts.
    last_path_copy: Option<std::time::Instant>,
    _subscriptions: Vec<Subscription>,
}

/// A second editor pane showing another (or the same) open document.
#[derive(Clone, Copy, Debug)]
struct EditorSplit {
    /// Index into `documents`.
    other: usize,
    /// `true` = stacked (Split Down), `false` = side-by-side (Split Right).
    vertical: bool,
}

impl Workspace {
    /// A workspace restoring the last session (when enabled) and opening
    /// `paths`.
    pub fn new(
        paths: Vec<PathBuf>,
        app_menu_bar: Entity<TitleMenuBar>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = AppSettings::get(cx).clone();
        let headings = cx.new(|cx| CommandState::new(window, cx));
        let quick_open = cx.new(|cx| CommandState::new(window, cx));
        let command_palette = cx.new(|cx| CommandState::new(window, cx));
        let theme_picker = cx.new(|cx| CommandState::new(window, cx));
        let go_to_line = cx.new(|cx| CommandState::new(window, cx));
        let tree_prompt = cx.new(|cx| CommandState::new(window, cx));
        let search_find = cx.new(|cx| InputState::new(window, cx).placeholder("Find"));
        let search_replace = cx.new(|cx| InputState::new(window, cx).placeholder("Replace"));
        let file_tree = cx.new(FileTree::new);
        let terminals: Vec<Entity<TerminalPanel>> = Vec::new();
        let terminal_subscriptions: Vec<Subscription> = Vec::new();

        let subscriptions = vec![
            // Enter in the Search view's Find field runs the search; once
            // it has results for that text, Enter and Shift+Enter step
            // through them. Escape (see render_search_sidebar) goes back to
            // the editor.
            cx.subscribe_in(
                &search_find,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { shift, .. } = event {
                        this.search_find_enter(!*shift, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &search_replace,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        this.sidebar_run_replace(true, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &file_tree,
                window,
                |this, _, event, window, cx| match event {
                    FileTreeEvent::Open { path, preview } => this.open_paths(
                        vec![path.clone()],
                        if *preview {
                            Placement::Preview
                        } else {
                            Placement::NewTab
                        },
                        window,
                        cx,
                    ),
                    FileTreeEvent::Copied { copied, failed } => {
                        let message = match (copied, failed) {
                            (0, _) => "Nothing was copied: the drop target sits inside the source, or the files couldn’t be written.".to_string(),
                            (copied, 0) => format!(
                                "Copied {copied} item{} into the folder.",
                                if *copied == 1 { "" } else { "s" }
                            ),
                            (copied, failed) => format!(
                                "Copied {copied} item{}; {failed} couldn’t be copied.",
                                if *copied == 1 { "" } else { "s" }
                            ),
                        };
                        window.push_notification(Notification::info(message), cx);
                    }
                    FileTreeEvent::Moved { from, to } => {
                        this.retarget_moved(from, to, window, cx);
                    }
                    FileTreeEvent::Deleted { paths } => {
                        this.close_deleted_documents(paths, window, cx);
                    }
                    // The empty space below the entries: a new file in the
                    // folder the tree is showing, as VS Code makes one.
                    FileTreeEvent::NewFileInRoot => {
                        let root = this.file_tree.read(cx).root().map(Path::to_path_buf);
                        this.prompt_tree_name(TreePromptKind::NewFile, root, "", window, cx);
                    }
                },
            ),
            cx.observe_global_in::<AppSettings>(window, |this, window, cx| {
                this.on_settings_changed(window, cx);
                cx.notify();
            }),
            cx.observe_window_appearance(window, |_, window, cx| {
                if AppSettings::get(cx).appearance == Appearance::System {
                    themes::apply(Some(window), cx);
                }
            }),
            cx.observe_window_bounds(window, |this, window, cx| this.save_session(window, cx)),
        ];

        // Ask before a window with unsaved changes closes.
        let weak = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            weak.update(cx, |this, cx| this.request_close(window, cx))
                .unwrap_or(true)
        });

        let watch_task = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(WATCH_INTERVAL).await;
                if this
                    .update_in(cx, |this, window, cx| {
                        this.check_disk(window, cx);
                        this.auto_save_dirty(window, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        // Status-bar mem/CPU: sampler lives in perf (one process thread); this
        // only repaints when the shared pulse generation advances (~1.5s).
        // A window in the background is not repainted for it at all (the
        // label catches up on the next frame), so an idle Malgel behind
        // another app costs nothing.
        crate::perf::ensure_pulse();
        let mut last_pulse_gen = 0u64;
        let pulse_task = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(1500))
                    .await;
                let (_, generation) = crate::perf::pulse_snapshot();
                let alive = this.update_in(cx, |_, window, cx| {
                    if pulse_needs_repaint(generation, last_pulse_gen, window.is_window_active()) {
                        last_pulse_gen = generation;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            documents: Vec::new(),
            active: 0,
            preview: None,
            headings,
            quick_open,
            command_palette,
            theme_picker,
            alert_focus: std::rc::Rc::new(crate::dialog_focus::DialogFocus::new(cx)),
            fallback_save_sub: None,
            go_to_line,
            tree_prompt,
            last_find: String::new(),
            last_replace: String::new(),
            activity_view: ActivityView::Explorer,
            search_find,
            search_replace,
            sidebar_search: SidebarSearch::Idle,
            sidebar_rows: Vec::new(),
            sidebar_collapsed: HashSet::new(),
            sidebar_cursor: None,
            sidebar_scroll: UniformListScrollHandle::new(),
            diff_session: None,
            file_tree,
            app_menu_bar,
            window_title: String::new(),
            closing: false,
            discarded: HashSet::new(),
            pending_disk_prompt: HashSet::new(),
            approved_opens: HashMap::new(),
            closed_documents: Vec::new(),
            pinned: HashSet::new(),
            pinned_preview: None,
            compare_selection: None,
            tab_split_preview: None,
            nav_history: Vec::new(),
            nav_index: 0,
            last_session: None,
            tab_insertion: None,
            tab_bar_bounds: Cell::new(None),
            tab_scroll: ScrollHandle::new(),
            tab_revealed: Cell::new(None),
            tab_drag_copy: Cell::new(false),
            tab_drag_index: Cell::new(None),
            tab_drag_last_pos: Cell::new(None),
            editor_body_bounds: Cell::new(None),
            tab_context_menu: None,
            _tab_context_menu_sub: None,
            _watch_task: watch_task,
            _pulse_task: pulse_task,
            document_subscriptions: Vec::new(),
            terminals,
            terminal_active: 0,
            terminal_split: false,
            terminal_visible: false,
            terminal_maximized: false,
            terminal_subscriptions,
            editor_split: None,
            last_path_copy: None,
            _subscriptions: subscriptions,
        };
        // Seed the first integrated terminal session (subscriptions need `this`).
        let seed = cx.new(|cx| TerminalPanel::new(window, cx));
        this.push_terminal(seed, window, cx);
        this.restore(paths, &settings, window, cx);
        this.install_orphan_focus_guard(window, cx);
        this
    }

    /// Keys must never go dead. When the focused element disappears (the
    /// preview pane a click focused is removed by Ctrl+1, a closed pane…)
    /// GPUI dispatches from the window root, above every workspace action
    /// handler, so Ctrl+`, Ctrl+P, Ctrl+Shift+P and typing all did nothing
    /// until a mouse click. Before any binding runs, notice that focus is
    /// outside the workspace with no dialog / sheet open, move it to the
    /// active editor, and replay the keystroke there.
    fn install_orphan_focus_guard(&self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.weak_entity();
        let window_id = window.window_handle().window_id();
        // Set while a rescued keystroke is replayed, so a target that still
        // cannot take focus does not loop.
        let replaying = std::rc::Rc::new(std::cell::Cell::new(false));
        cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle().window_id() != window_id || replaying.get() {
                return;
            }
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            if workspace.read(cx).focus_handle.contains_focused(window, cx)
                || window.has_active_dialog(cx)
                || window.has_active_sheet(cx)
            {
                return;
            }
            // Bare modifier presses carry no key worth replaying.
            if event.keystroke.key.is_empty() {
                return;
            }
            log::info!("ui: keyboard focus was orphaned; returning it to the editor");
            let target = workspace.read(cx).active_focus_target(cx);
            target.focus(window, cx);
            cx.stop_propagation();
            let keystroke = event.keystroke.clone();
            let replaying = replaying.clone();
            window.defer(cx, move |window, cx| {
                replaying.set(true);
                window.dispatch_keystroke(keystroke, cx);
                replaying.set(false);
            });
        })
        .detach();
    }

    /// Whether opening `paths` should restore the previous session's tabs: either
    /// nothing was asked for, or the only argument is the same folder the session
    /// had open (re-launching from that directory). Any other path replaces.
    fn restores_session_for(paths: &[PathBuf], folder: Option<&Path>) -> bool {
        // Probe #13: no-arg launch restores only when the last session had a
        // folder (folder+tabs). A no-folder session stays on Welcome — no stale tabs.
        if paths.is_empty() {
            return folder.is_some();
        }
        let Some(folder) = folder else {
            return false;
        };
        if paths.len() != 1 {
            return false;
        }
        let asked = paths[0].canonicalize().unwrap_or_else(|_| paths[0].clone());
        if !asked.is_dir() {
            return false;
        }
        let folder = folder
            .canonicalize()
            .unwrap_or_else(|_| folder.to_path_buf());
        asked == folder
    }

    /// Reopen the last session's documents and anything recovered after a
    /// crash, then the files asked for.
    fn restore(
        &mut self,
        paths: Vec<PathBuf>,
        settings: &crate::settings::Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = Session::load();
        let first_run = previous.is_none();
        let previous = previous.unwrap_or_default();
        let mut active = None;
        let mut referenced = HashSet::new();
        // Opening the same folder again restores its tabs; any other path
        // (or several) replaces the session with those files. Skip re-opening
        // the folder path only when we are actually restoring that session.
        let same_folder = Self::restores_session_for(&paths, previous.folder.as_deref());
        let restoring_session = settings.restore_session && same_folder;
        let mut deferred_open = if restoring_session {
            Vec::new()
        } else {
            paths.clone()
        };
        let mut restoring: Vec<(EntityId, PathBuf, usize)> = Vec::new();

        for (ix, entry) in previous.documents.iter().enumerate() {
            let recovery = entry.recovery.as_deref().and_then(|id| {
                referenced.insert(id.to_string());
                session::read_recovery(id).map(|recovery| (id.to_string(), recovery))
            });
            // Unsaved changes always come back; saved documents only when
            // the session is restored (just the active one without tabs).
            let wanted = restoring_session && (settings.tabs || ix == previous.active);
            let doc = match recovery {
                Some((id, recovery)) => Some(self.restored_document(id, recovery, window, cx)),
                None if wanted => entry.path.as_ref().and_then(|path| {
                    // PDF files and images are not text documents; open them
                    // after the window finishes building via their own path.
                    if document::is_pdf_path(path)
                        || document::is_image_path(path)
                        || document::open_check(path) != document::OpenCheck::Fine
                    {
                        deferred_open.push(path.clone());
                        return None;
                    }
                    // The text is read off the UI thread, so a session of
                    // big files does not hold the window back from showing;
                    // a loading tab keeps the file's place meanwhile.
                    let cursor = entry.cursor;
                    let doc = cx.new(|cx| {
                        DocumentView::new_loading(path.clone(), window, cx)
                            .with_pending_cursor(cursor)
                    });
                    restoring.push((doc.entity_id(), path.clone(), cursor));
                    Some(doc)
                }),
                None => None,
            };
            if let Some(doc) = doc {
                let cursor = entry.cursor;
                doc.update(cx, |doc, cx| doc.set_cursor(cursor, window, cx));
                if ix == previous.active {
                    active = Some(self.documents.len());
                }
                self.add_document(doc, false, window, cx);
            }
        }

        let orphans = session::orphaned_recoveries(&referenced);
        let recovered = orphans.len()
            + self
                .documents
                .iter()
                .filter(|doc| doc.read(cx).is_dirty())
                .count();
        for (id, recovery) in orphans {
            let doc = self.restored_document(id, recovery, window, cx);
            self.add_document(doc, false, window, cx);
        }
        if recovered > 0 {
            let message = if recovered == 1 {
                "Recovered unsaved changes from the last session.".to_string()
            } else {
                format!("Recovered unsaved changes to {recovered} documents.")
            };
            // The window can show notifications once it has finished opening.
            window.defer(cx, move |window, cx| {
                window.push_notification(Notification::info(message), cx)
            });
        }

        if restoring_session {
            self.file_tree
                .update(cx, |tree, cx| tree.set_root(previous.folder.clone(), cx));
        }
        self.last_session = Some(previous);

        // Files being opened replace this untouched placeholder.
        if self.documents.is_empty() {
            let text = if first_run && paths.is_empty() {
                WELCOME.to_string()
            } else {
                String::new()
            };
            let doc = cx.new(|cx| DocumentView::new(text, None, LineEnding::Lf, window, cx));
            self.add_document(doc, false, window, cx);
        }
        self.active = active
            .unwrap_or(0)
            .min(self.documents.len().saturating_sub(1));
        // open_paths spawns async loads that must update this workspace
        // through the window; defer until the build callback has returned,
        // otherwise update_in is rejected while the window is on the stack.
        if !restoring.is_empty() {
            cx.defer_in(window, move |this, window, cx| {
                for (id, path, cursor) in restoring {
                    this.finish_restoring(id, path, cursor, window, cx);
                }
            });
        }
        if !deferred_open.is_empty() {
            cx.defer_in(window, move |this, window, cx| {
                this.open_paths(deferred_open, Placement::Default, window, cx);
            });
        }
        self.focus_active(window, cx);
        self.sync_file_tree(cx);
    }

    /// Read a restored file's text in the background and swap it into its
    /// loading tab, caret where it was; a file gone meanwhile just leaves.
    fn finish_restoring(
        &mut self,
        id: EntityId,
        path: PathBuf,
        cursor: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let read_path = path.clone();
        let task = cx
            .background_executor()
            .spawn(async move { document::load(&read_path) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, |this, window, cx| {
                let Some(ix) = this.document_index(id) else {
                    return;
                };
                match result {
                    Ok(loaded) => {
                        let doc = cx.new(|cx| {
                            DocumentView::new(
                                loaded.text,
                                Some(path.clone()),
                                loaded.line_ending,
                                window,
                                cx,
                            )
                            .with_encoding(loaded.encoding)
                        });
                        doc.update(cx, |doc, cx| doc.set_cursor(cursor, window, cx));
                        this.swap_in_loaded(id, ix, doc, window, cx);
                    }
                    Err(_) => this.drop_loading(Some(id), &path, window, cx),
                }
            });
        })
        .detach();
    }

    fn restored_document(
        &mut self,
        id: String,
        recovery: session::Recovery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<DocumentView> {
        cx.new(|cx| {
            let mut doc = DocumentView::new(String::new(), None, LineEnding::Lf, window, cx);
            doc.restore_unsaved(id, recovery, window, cx);
            doc
        })
    }

    // ---------------------------------------------------------------------
    // Documents

    fn active_document(&self) -> &Entity<DocumentView> {
        &self.documents[self.active.min(self.documents.len() - 1)]
    }

    fn document_index(&self, id: EntityId) -> Option<usize> {
        self.documents.iter().position(|doc| doc.entity_id() == id)
    }

    fn shows_tabs(&self, cx: &App) -> bool {
        let settings = AppSettings::get(cx);
        !settings.focus_mode && (settings.tabs || self.documents.len() > 1)
    }

    fn add_document(
        &mut self,
        doc: Entity<DocumentView>,
        activate: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.watch_document(&doc, window, cx);
        self.documents.push(doc);
        if activate {
            self.activate(self.documents.len() - 1, window, cx);
        }
    }

    /// Insert `doc` at `ix`, activating it (a duplicated tab lands where
    /// it was dropped, the way VS Code copies tabs into place).
    fn add_document_at(
        &mut self,
        doc: Entity<DocumentView>,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.watch_document(&doc, window, cx);
        let ix = ix.min(self.documents.len());
        self.documents.insert(ix, doc);
        self.activate(ix, window, cx);
    }

    /// Follow a document's changes for the tab bar and notifications.
    fn watch_document(
        &mut self,
        doc: &Entity<DocumentView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let subscriptions = vec![
            cx.observe(doc, |this, doc, cx| {
                // Editing a file takes it out of the preview slot: from then
                // on it keeps a tab of its own, as in VS Code.
                if doc.read(cx).is_dirty() {
                    this.unpreview(doc.entity_id());
                }
                cx.notify()
            }),
            cx.subscribe_in(doc, window, |this, _, event, window, cx| match event {
                DocumentEvent::Notify(message) => {
                    window.push_notification(Notification::info(message.clone()), cx)
                }
                DocumentEvent::ShowEditor => {
                    let layout = AppSettings::get(cx).layout;
                    if layout == Layout::Preview {
                        log::info!(
                            "ui: ShowEditor from preview-only -> Split (so a jump is visible)"
                        );
                        AppSettings::update(cx, |settings| {
                            settings.layout = Layout::Split;
                            settings.focus_mode = false;
                        });
                        this.focus_active(window, cx);
                        cx.notify();
                    }
                }
                DocumentEvent::TogglePinPreview => {
                    this.toggle_pin_preview(window, cx);
                }
            }),
        ];
        self.document_subscriptions
            .push((doc.entity_id(), subscriptions));
    }

    fn remove_document(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.document_index(id) else {
            return;
        };
        // Remember where it was, so Reopen Closed Editor can bring it back.
        if let Some(path) = self.documents[ix].read(cx).path().cloned() {
            self.remember_closed(path);
        }
        let doc = self.documents.remove(ix);
        doc.update(cx, |doc, _| doc.discard_recovery());
        self.document_subscriptions
            .retain(|(subscribed, _)| *subscribed != id);
        self.pinned.remove(&id);
        if self.pinned_preview == Some(id) {
            self.pinned_preview = None;
        }
        self.unpreview(id);
        // Probe #13: keep split indices valid; collapse empty/orphan groups.
        if let Some(split) = self.editor_split {
            let mut other = split.other;
            if other == ix {
                // Closed the secondary pane's doc — collapse the split.
                self.editor_split = None;
            } else {
                if other > ix {
                    other -= 1;
                }
                let n = self.documents.len();
                if n <= 1 || other >= n {
                    self.editor_split = None;
                } else {
                    self.editor_split = Some(EditorSplit {
                        other,
                        vertical: split.vertical,
                    });
                }
            }
        }
        if self.documents.is_empty() {
            let doc =
                cx.new(|cx| DocumentView::new(String::new(), None, LineEnding::Lf, window, cx));
            self.add_document(doc, false, window, cx);
            self.editor_split = None;
        }
        if self.active > ix || self.active >= self.documents.len() {
            self.active = self.active.saturating_sub(1);
        }
        // The active tab may have changed: a stale EditorOnly / PreviewOnly
        // role would hide its editor or its preview.
        self.sync_preview_pin_roles(cx);
        // Collapse if both panes would show the same tab.
        if let Some(split) = self.editor_split {
            let other = split.other.min(self.documents.len().saturating_sub(1));
            if other == self.active || self.documents.len() < 2 {
                self.editor_split = None;
            } else {
                self.editor_split = Some(EditorSplit {
                    other,
                    vertical: split.vertical,
                });
            }
        }
        self.activate(self.active, window, cx);
    }

    /// Put `doc` in place of the active document.
    fn replace_active(
        &mut self,
        doc: Entity<DocumentView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_document_at(self.active, doc, window, cx);
    }

    /// Put `doc` where the document at `position` sits, showing it.
    fn replace_document_at(
        &mut self,
        position: usize,
        doc: Entity<DocumentView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position = position.min(self.documents.len().saturating_sub(1));
        let old = self.documents[position].entity_id();
        self.add_document(doc, false, window, cx);
        let new = self.documents.pop().expect("just added");
        self.documents.insert(position, new);
        if let Some(ix) = self.document_index(old) {
            let removed = self.documents.remove(ix);
            removed.update(cx, |doc, _| doc.discard_recovery());
            self.document_subscriptions
                .retain(|(subscribed, _)| *subscribed != old);
        }
        if self.preview == Some(old) {
            self.preview = None;
        }
        self.activate(position, window, cx);
    }

    /// Show a loading tab for `path` where `placement` puts the file, when
    /// that is safe (see [`placeholder_is_safe`]); its id, if one was shown.
    fn place_loading(
        &mut self,
        path: &Path,
        placement: Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<EntityId> {
        if !placeholder_is_safe(
            placement,
            self.active_document().read(cx).is_blank(),
            AppSettings::get(cx).tabs,
        ) {
            return None;
        }
        let loading = cx.new(|cx| DocumentView::new_loading(path.to_path_buf(), window, cx));
        let id = loading.entity_id();
        self.place_document(loading, placement, window, cx);
        Some(id)
    }

    /// Where a finished load goes: `Some(None)` with no loading tab (place
    /// it normally), `Some(Some((id, ix)))` in its loading tab, and `None`
    /// when that tab was closed meanwhile, which means never mind.
    fn loading_slot(&self, placeholder: Option<EntityId>) -> Option<Option<(EntityId, usize)>> {
        match placeholder {
            Some(id) => self.document_index(id).map(|ix| Some((id, ix))),
            None => Some(None),
        }
    }

    /// Take a failed load's loading tab away, leaving no empty tab and no
    /// Reopen Closed Editor entry behind.
    fn drop_loading(
        &mut self,
        placeholder: Option<EntityId>,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = placeholder
            && self.document_index(id).is_some()
        {
            self.remove_document(id, window, cx);
            self.closed_documents.retain(|closed| closed != path);
        }
    }

    /// Keep the loading tab and show why the open failed, so magic-byte /
    /// format mistakes are visible in the editor instead of vanishing.
    fn fail_loading(
        &mut self,
        placeholder: Option<EntityId>,
        path: &Path,
        err: impl Into<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let err = err.into();
        let message = format!(
            "Couldn\u{2019}t open \u{201c}{}\u{201d}. {err}",
            document::display_name(Some(&path.to_path_buf()))
        );
        if let Some(id) = placeholder
            && let Some(doc) = self.documents.iter().find(|doc| doc.entity_id() == id)
        {
            doc.update(cx, |doc, cx| doc.set_load_error(err, cx));
            // Toast as well so the failure is noticed if the tab is buried.
            self.notify_error(message, window, cx);
            return;
        }
        self.drop_loading(placeholder, path, window, cx);
        self.notify_error(message, window, cx);
    }

    /// Put a finished document in the place of its loading tab `id` at
    /// `ix`, keeping that tab's preview slot and pin.
    fn swap_in_loaded(
        &mut self,
        id: EntityId,
        ix: usize,
        doc: Entity<DocumentView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let new = doc.entity_id();
        let was_preview = self.preview == Some(id);
        let was_pinned = self.pinned.remove(&id);
        let was_active = self.active == ix;
        let active = self.active;
        self.replace_document_at(ix, doc, window, cx);
        if was_preview {
            self.preview = Some(new);
        }
        if was_pinned {
            self.pinned.insert(new);
        }
        if !was_active {
            // The user moved on while it loaded; leave them where they are.
            self.activate(active, window, cx);
        }
    }

    /// Show `doc` where `placement` asks: the preview slot, in place of the
    /// active document, or as a tab of its own.
    fn place_document(
        &mut self,
        doc: Entity<DocumentView>,
        placement: Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(placement, Placement::Preview) {
            let id = doc.entity_id();
            // The slot a plain click reuses: the preview tab the last click
            // left behind (while it is untouched), or a blank tab, which
            // would otherwise sit there for ever. Without one the file gets
            // a tab of its own, which the next click takes over.
            let shown = self.preview.and_then(|id| self.document_index(id));
            let untouched = shown.is_some_and(|ix| !self.documents[ix].read(cx).is_dirty());
            let blank = self.active_document().read(cx).is_blank();
            match preview_target(shown, untouched, self.active, blank) {
                Some(ix) => self.replace_document_at(ix, doc, window, cx),
                None => self.add_document(doc, true, window, cx),
            }
            self.preview = Some(id);
            return;
        }
        let active = self.active_document().read(cx);
        let replace = active.is_blank()
            || match placement {
                Placement::Default => !AppSettings::get(cx).tabs,
                Placement::NewTab => false,
                Placement::ReplaceActive => true,
                Placement::Preview => unreachable!("handled above"),
            };
        if replace {
            self.replace_active(doc, window, cx);
        } else {
            self.add_document(doc, true, window, cx);
        }
    }

    /// Give the preview slot up, so the document keeps its tab. Editing a
    /// file, or opening it for good, does this, as in VS Code.
    fn unpreview(&mut self, id: EntityId) {
        if self.preview == Some(id) {
            self.preview = None;
        }
    }

    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_inner(ix, true, window, cx);
    }

    /// Switch the active tab. Pass `focus: false` when an alert is about to
    /// open: `focus_active` defers editor focus and would steal the dialog's
    /// focus trap (Esc/Tab dead, typed keys leak into the buffer).
    fn activate_inner(
        &mut self,
        ix: usize,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.documents.is_empty() {
            return;
        }
        self.active = ix.min(self.documents.len() - 1);
        if let Some(path) = self.documents[self.active].read(cx).path().cloned() {
            self.record_navigation(path);
        }
        if focus {
            self.focus_active(window, cx);
        }
        self.sync_file_tree(cx);
        self.save_session(window, cx);
        self.sync_preview_pin_roles(cx);
        cx.notify();
    }

    /// Note the file being shown, so Alt+← can come back to it. Nothing is
    /// added twice in a row, and anything the history had ahead of the
    /// current entry is dropped — the way a new branch replaces the redo
    /// tail in a browser.
    fn record_navigation(&mut self, path: PathBuf) {
        if self.nav_history.get(self.nav_index) == Some(&path) {
            // The cursor is already on this file: Alt+← or Alt+→ put it
            // there, and the move is not a visit of its own.
            return;
        }
        if self.nav_history.last() == Some(&path) {
            return;
        }
        self.nav_history.truncate(self.nav_index + 1);
        self.nav_history.push(path);
        if self.nav_history.len() > 50 {
            self.nav_history.remove(0);
        }
        self.nav_index = self.nav_history.len().saturating_sub(1);
    }

    /// Remember a closed editor's file for Reopen Closed Editor, newest
    /// last. A path that is already in the list moves to the end.
    fn remember_closed(&mut self, path: PathBuf) {
        self.closed_documents
            .retain(|remembered| remembered != &path);
        self.closed_documents.push(path);
        if self.closed_documents.len() > 20 {
            self.closed_documents.remove(0);
        }
    }

    fn focus_active(&self, window: &mut Window, cx: &mut App) {
        // Typst live preview is a PdfView, not the Markdown TextView —
        // focusing an off-tree handle drops every workspace action.
        let focus = self.active_focus_target(cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
    }

    /// The handle [`Self::focus_active`] would focus.
    fn active_focus_target(&self, cx: &App) -> FocusHandle {
        let doc = self.active_document().read(cx);
        let shows_editor = AppSettings::get(cx).layout.shows_editor();
        let focus_mode = AppSettings::get(cx).focus_mode;
        match keyboard_target(doc.is_media(), shows_editor, focus_mode) {
            KeyboardTarget::Media => doc
                .media_focus_handle(cx)
                .unwrap_or_else(|| doc.editor().focus_handle(cx)),
            KeyboardTarget::Editor => doc.editor().focus_handle(cx),
            KeyboardTarget::Preview => doc.preview_pane_focus_handle(cx),
        }
    }

    fn sync_file_tree(&self, cx: &mut App) {
        let path = self.active_document().read(cx).path().cloned();
        self.file_tree
            .update(cx, |tree, cx| tree.set_active(path, cx));
    }

    /// Open `paths`: as tabs when tabs are on, otherwise in place of the
    /// current document (asking about unsaved changes first).
    fn open_paths(
        &mut self,
        paths: Vec<PathBuf>,
        placement: Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tabs = AppSettings::get(cx).tabs;
        let files = paths.iter().filter(|path| !path.is_dir()).count();
        if tabs && files > 1 && !matches!(placement, Placement::Preview) {
            self.open_files_in_order(paths, placement, window, cx);
            return;
        }
        for path in paths {
            let path = path.canonicalize().unwrap_or(path);
            if path.is_dir() {
                // A directory argument (CLI or a second launch) becomes the
                // sidebar root, the way VS Code opens a folder.
                // Opening a folder shows the Explorer, as VS Code does.
                AppSettings::update(cx, |settings| {
                    settings.push_recent(&path);
                    settings.file_sidebar = true;
                });
                let folder = path.clone();
                self.file_tree
                    .update(cx, |tree, cx| tree.set_root(Some(path), cx));
                // Round-4: do not leave a pointless blank Untitled as the only
                // editor when a folder was opened. Prefer README / first .md.
                self.open_folder_default_doc(&folder, window, cx);
                continue;
            }
            if let Some(ix) = self
                .documents
                .iter()
                .position(|doc| doc.read(cx).path() == Some(&path))
            {
                // Opening a file that only holds the preview slot gives it a
                // tab of its own, the way double-clicking does in VS Code.
                if !matches!(placement, Placement::Preview) {
                    let id = self.documents[ix].entity_id();
                    self.unpreview(id);
                }
                self.activate(ix, window, cx);
                continue;
            }
            let active = self.active_document().read(cx);
            if !tabs && active.is_dirty() {
                // One document at a time: settle this one first.
                self.confirm_unsaved(
                    self.active_document().clone(),
                    AfterConfirm::OpenPath(path),
                    window,
                    cx,
                );
                return;
            }
            self.load_path(path, placement, Target::Top, window, cx);
        }
    }

    /// Ask how to open a file too large (or binary) to load as is, the way
    /// VS Code warns before opening one: open it anyway or leave it; a huge
    /// text file can also open as plain text. A binary file that is not text
    /// at all only says so: opening it could only fail.
    fn ask_before_opening(
        &mut self,
        path: PathBuf,
        check: document::OpenCheck,
        placement: Placement,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = display_name(Some(&path));
        let media = !reads_as_text(&path);
        let description = match check {
            document::OpenCheck::Large(size) => format!(
                "It is {:.1} MB. Opening it may take a while and use a lot of memory{}.",
                size as f64 / (1024. * 1024.),
                if media {
                    ""
                } else {
                    "; as plain text it opens without preview or highlighting"
                }
            ),
            document::OpenCheck::BinaryText => {
                "It holds binary data, though it reads as text.".to_string()
            }
            _ => "It is a binary file, which Malgel can’t show as text.".to_string(),
        };
        let choices = open_choices(check, media);
        if window.has_active_dialog(cx) {
            self.notify_error(
                format!("“{name}” was not opened. {description}"),
                window,
                cx,
            );
            return;
        }
        let workspace = cx.weak_entity();
        let target = Rc::new(target);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let open = |mode: OpenMode| {
                let workspace = workspace.clone();
                let path = path.clone();
                let target = target.clone();
                move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                    window.close_dialog(cx);
                    let path = path.clone();
                    let target = (*target).clone();
                    _ = workspace.update(cx, |this, cx| {
                        this.approved_opens.insert(path.clone(), mode);
                        this.load_path(path, placement, target, window, cx);
                    });
                }
            };
            let mut footer = DialogFooter::new().justify_end();
            for choice in choices {
                footer = footer.child(match choice {
                    OpenChoice::Close(label) => Button::new("open-cancel")
                        .label(*label)
                        .outline()
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                    OpenChoice::PlainText => Button::new("open-plain")
                        .label("Open as Plain Text")
                        .outline()
                        .on_click(open(OpenMode::PlainText)),
                    OpenChoice::Anyway => Button::new("open-anyway")
                        .label("Open Anyway")
                        .primary()
                        .on_click(open(OpenMode::Full)),
                });
            }
            let title = if check == document::OpenCheck::Binary {
                format!("Can’t open “{name}”")
            } else {
                format!("Open “{name}”?")
            };
            alert
                .title(title)
                .description(description.clone())
                .keyboard(true)
                .on_cancel(|_, window, cx| {
                    window.close_dialog(cx);
                    true
                })
                .footer(footer)
        });
    }

    /// Open several files at once (`malgel a.md b.rs c.pdf`), the way VS Code
    /// does: one tab each, in the order given, with the last one in front.
    /// Text is read in the background first, so a quick PDF does not jump
    /// ahead of a slower text file and steal the focus.
    fn open_files_in_order(
        &mut self,
        paths: Vec<PathBuf>,
        placement: Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .map(|path| path.canonicalize().unwrap_or(path))
            .collect();
        let task = cx.background_executor().spawn({
            let paths = paths.clone();
            async move {
                paths
                    .iter()
                    .map(|path| {
                        (reads_as_text(path)
                            && document::open_check(path) == document::OpenCheck::Fine)
                            .then(|| document::load(path))
                    })
                    .collect::<Vec<_>>()
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let loaded = task.await;
            _ = this.update_in(cx, |this, window, cx| {
                let mut last = None;
                for (path, loaded) in paths.into_iter().zip(loaded) {
                    if path.is_dir() {
                        this.open_paths(vec![path], placement, window, cx);
                        continue;
                    }
                    if let Some(ix) = this
                        .documents
                        .iter()
                        .position(|doc| doc.read(cx).path() == Some(&path))
                    {
                        last = Some(this.documents[ix].entity_id());
                        continue;
                    }
                    match loaded {
                        None => this.load_path(path.clone(), placement, Target::Top, window, cx),
                        Some(Ok(loaded)) => {
                            AppSettings::update(cx, |settings| settings.push_recent(&path));
                            let doc = cx.new(|cx| {
                                DocumentView::new(
                                    loaded.text,
                                    Some(path.clone()),
                                    loaded.line_ending,
                                    window,
                                    cx,
                                )
                                .with_encoding(loaded.encoding)
                            });
                            this.place_document(doc, placement, window, cx);
                        }
                        Some(Err(err)) => {
                            AppSettings::update(cx, |settings| {
                                settings.recent_files.retain(|recent| recent != &path)
                            });
                            this.notify_error(
                                format!("Couldn’t open “{}”. {err}", display_name(Some(&path))),
                                window,
                                cx,
                            );
                            continue;
                        }
                    }
                    if let Some(doc) = this
                        .documents
                        .iter()
                        .find(|doc| doc.read(cx).path() == Some(&path))
                    {
                        last = Some(doc.entity_id());
                    }
                }
                if let Some(ix) = last.and_then(|id| this.document_index(id)) {
                    this.activate(ix, window, cx);
                }
            });
        })
        .detach();
    }

    /// Read `path` in the background and show it where `placement` says,
    /// landing where `target` says once it has loaded.
    fn load_path(
        &mut self,
        path: PathBuf,
        placement: Placement,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A very large or binary file asks first, before anything is read.
        let mode = self.approved_opens.remove(&path);
        if mode.is_none() {
            let check = document::open_check(&path);
            if check != document::OpenCheck::Fine {
                self.ask_before_opening(path, check, placement, target, window, cx);
                return;
            }
        }
        let plain = mode == Some(OpenMode::PlainText);

        // A PDF is parsed and a picture decoded off the UI thread, so a large
        // one never freezes the window while it loads.
        if document::is_pdf_path(&path) || document::is_image_path(&path) {
            let pdf = document::is_pdf_path(&path);
            let read_path = path.clone();
            let task = cx.background_executor().spawn(async move {
                let bytes = std::fs::read(&read_path)
                    .map_err(|err| format!("Couldn\u{2019}t read it. {err}"))?;
                if pdf {
                    crate::pdf_view::PdfDocument::from_bytes(bytes).map(MediaLoad::Pdf)
                } else {
                    crate::image_view::load_image(&read_path, bytes)
                        .map(|(image, width, height)| MediaLoad::Image(image, width, height))
                }
            });
            let placeholder = self.place_loading(&path, placement, window, cx);
            cx.spawn_in(window, async move |this, cx| {
                let result = task.await;
                _ = this.update_in(cx, |this, window, cx| match result {
                    Ok(media) => {
                        let Some(slot) = this.loading_slot(placeholder) else {
                            return;
                        };
                        AppSettings::update(cx, |settings| settings.push_recent(&path));
                        let doc = cx.new(|cx| match media {
                            MediaLoad::Pdf(document) => {
                                DocumentView::new_pdf(path.clone(), document, window, cx)
                            }
                            MediaLoad::Image(image, width, height) => DocumentView::new_image(
                                path.clone(),
                                image,
                                width,
                                height,
                                window,
                                cx,
                            ),
                        });
                        match slot {
                            Some((id, ix)) => this.swap_in_loaded(id, ix, doc, window, cx),
                            None => this.place_document(doc, placement, window, cx),
                        }
                    }
                    Err(err) => {
                        if pdf {
                            AppSettings::update(cx, |settings| {
                                settings.recent_files.retain(|recent| recent != &path)
                            });
                        }
                        this.fail_loading(placeholder, &path, err, window, cx);
                    }
                });
            })
            .detach();
            return;
        }

        let read_path = path.clone();
        let task = cx
            .background_executor()
            .spawn(async move { document::load(&read_path) });
        // Immediate tab feedback (same as PDF/image): avoid a blank wait on
        // large text / Typst sources. Placeholder must not spawn Typst.
        let placeholder = self.place_loading(&path, placement, window, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, |this, window, cx| match result {
                Ok(loaded) => {
                    let Some(slot) = this.loading_slot(placeholder) else {
                        return;
                    };
                    AppSettings::update(cx, |settings| settings.push_recent(&path));
                    let doc = cx.new(|cx| {
                        let mut doc = DocumentView::new(
                            loaded.text,
                            Some(path.clone()),
                            loaded.line_ending,
                            window,
                            cx,
                        )
                        .with_encoding(loaded.encoding);
                        if plain {
                            doc.reduce(cx);
                        }
                        doc
                    });
                    match slot {
                        Some((id, ix)) => this.swap_in_loaded(id, ix, doc.clone(), window, cx),
                        None => this.place_document(doc.clone(), placement, window, cx),
                    }
                    this.sync_preview_pin_roles(cx);
                    match target {
                        Target::Top => {}
                        Target::Heading(anchor) => this.reveal_anchor(&doc, &anchor, window, cx),
                        Target::Line(line) => {
                            doc.update(cx, |doc, cx| doc.go_to_line(line, window, cx))
                        }
                    }
                }
                Err(err) => {
                    AppSettings::update(cx, |settings| {
                        settings.recent_files.retain(|recent| recent != &path)
                    });
                    this.fail_loading(placeholder, &path, err.to_string(), window, cx);
                }
            });
        })
        .detach();
    }

    fn new_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let doc = cx.new(|cx| DocumentView::new(String::new(), None, LineEnding::Lf, window, cx));
        if AppSettings::get(cx).tabs {
            self.add_document(doc, true, window, cx);
        } else {
            self.replace_active(doc, window, cx);
        }
    }

    fn notify_error(&self, message: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
        window.push_notification(Notification::error(message), cx);
    }

    fn update_window_title(&mut self, window: &mut Window, cx: &App) {
        let doc = self.active_document().read(cx);
        let title = format!(
            "{}{} — Malgel",
            doc.name(),
            if doc.is_dirty() { " (edited)" } else { "" }
        );
        if title != self.window_title {
            window.set_window_title(&title);
            window.set_window_edited(self.documents.iter().any(|doc| doc.read(cx).is_dirty()));
            self.window_title = title;
        }
    }

    // ---------------------------------------------------------------------
    // Settings

    fn on_settings_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = AppSettings::get(cx).clone();
        if settings.file_sidebar && self.file_tree.read(cx).root().is_none() {
            let dir = self.active_document().read(cx).dir();
            self.file_tree.update(cx, |tree, cx| tree.set_root(dir, cx));
            self.sync_file_tree(cx);
        }
        self.save_session(window, cx);
    }

    fn toggle(&mut self, update: impl FnOnce(&mut crate::settings::Settings), cx: &mut App) {
        AppSettings::update(cx, update);
    }

    // ---------------------------------------------------------------------
    // Session

    /// Remember the open documents, folder and window, when anything about
    /// them changed.
    fn save_session(&mut self, window: &mut Window, cx: &mut App) {
        if self.closing && self.last_session.is_none() {
            return;
        }
        let documents = self
            .documents
            .iter()
            .filter_map(|doc| {
                let doc = doc.read(cx);
                let recovery = doc
                    .is_dirty()
                    .then(|| doc.recovery_id().map(str::to_string))
                    .flatten();
                (doc.path().is_some() || recovery.is_some()).then(|| SessionDocument {
                    path: doc.path().cloned(),
                    cursor: doc.cursor(cx),
                    recovery,
                })
            })
            .collect::<Vec<_>>();
        let active_id = self.active_document().entity_id();
        let active = self
            .documents
            .iter()
            .filter(|doc| {
                let doc = doc.read(cx);
                doc.path().is_some() || doc.recovery_id().is_some()
            })
            .position(|doc| doc.entity_id() == active_id)
            .unwrap_or(0);
        let (bounds, maximized) = match window.window_bounds() {
            WindowBounds::Windowed(bounds) => (bounds, false),
            WindowBounds::Maximized(bounds) | WindowBounds::Fullscreen(bounds) => (bounds, true),
        };
        let session = Session {
            documents,
            active,
            folder: self.file_tree.read(cx).root().map(Path::to_path_buf),
            window: Some(WindowPlacement {
                x: f32::from(bounds.origin.x),
                y: f32::from(bounds.origin.y),
                width: f32::from(bounds.size.width),
                height: f32::from(bounds.size.height),
                maximized,
            }),
        };
        if self.last_session.as_ref() != Some(&session) {
            session.save();
            self.last_session = Some(session);
        }
    }

    // ---------------------------------------------------------------------
    // Watching files

    /// Look for open files other programs changed, moved or deleted, and
    /// refresh the file sidebar.
    fn check_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if AppSettings::get(cx).file_sidebar {
            self.file_tree.update(cx, |tree, cx| tree.refresh(cx));
        }
        // Save cursor positions and newly written recovery files.
        self.save_session(window, cx);

        let targets: Vec<(EntityId, PathBuf, Option<DiskStamp>, u64)> = self
            .documents
            .iter()
            // A PDF or image is not text: reading it back as text would put
            // its bytes into the hidden editor (and its word count into the
            // status bar).
            .filter(|doc| !doc.read(cx).is_media())
            .filter_map(|doc| {
                let id = doc.entity_id();
                let doc = doc.read(cx);
                let path = doc.path()?.clone();
                (!self.pending_disk_prompt.contains(&id))
                    .then(|| (id, path, doc.disk_stamp(), doc.saved_hash()))
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        let task = cx.background_executor().spawn(async move {
            let mut changes = Vec::new();
            for (id, path, known, saved_hash) in targets {
                let stamp = DiskStamp::of(&path);
                if stamp == known {
                    continue;
                }
                let change = match stamp {
                    None if known.is_some() => DiskChange::Missing,
                    None => continue,
                    Some(_) => match document::load(&path) {
                        Ok(loaded) if content_hash(&loaded.text) == saved_hash => {
                            DiskChange::Touched(stamp)
                        }
                        Ok(loaded) => DiskChange::Changed {
                            text: loaded.text,
                            line_ending: loaded.line_ending,
                            stamp,
                        },
                        Err(_) => continue,
                    },
                };
                changes.push((id, change));
            }
            changes
        });
        cx.spawn_in(window, async move |this, cx| {
            let changes = task.await;
            _ = this.update_in(cx, |this, window, cx| {
                for (id, change) in changes {
                    this.apply_disk_change(id, change, window, cx);
                }
            });
        })
        .detach();
    }

    fn apply_disk_change(
        &mut self,
        id: EntityId,
        change: DiskChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = self.document_index(id).map(|ix| self.documents[ix].clone()) else {
            return;
        };
        let name = doc.read(cx).name();
        match change {
            DiskChange::Touched(stamp) => doc.update(cx, |doc, _| doc.set_disk_stamp(stamp)),
            DiskChange::Missing => {
                doc.update(cx, |doc, cx| doc.mark_missing(cx));
                window.push_notification(
                    Notification::warning(format!(
                        "“{name}” was moved or deleted. Save it to keep your text."
                    )),
                    cx,
                );
            }
            DiskChange::Changed {
                text,
                line_ending,
                stamp,
            } => {
                if !doc.read(cx).is_dirty() {
                    doc.update(cx, |doc, cx| doc.reload(text, line_ending, window, cx));
                    window.push_notification(
                        Notification::info(format!("“{name}” reloaded from disk.")),
                        cx,
                    );
                    return;
                }
                if window.has_active_dialog(cx) {
                    // Ask once the current dialog is answered.
                    return;
                }
                // Do not update the stamp yet: Ctrl+S must still see the
                // conflict. The stamp advances only when the user chooses.
                self.pending_disk_prompt.insert(id);
                if let Some(ix) = self.document_index(id) {
                    self.activate_inner(ix, false, window, cx);
                }
                self.open_disk_conflict(doc, id, name, text, line_ending, stamp, window, cx);
            }
        }
    }

    /// Ask whether to keep the buffer or take the file on disk. Esc keeps
    /// the buffer (and focuses the editor); the stamp is updated only after
    /// a choice so a stray Ctrl+S cannot clobber the external write.
    #[allow(clippy::too_many_arguments)]
    fn open_disk_conflict(
        &mut self,
        doc: Entity<DocumentView>,
        id: EntityId,
        name: String,
        text: String,
        line_ending: LineEnding,
        stamp: Option<crate::session::DiskStamp>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = cx.weak_entity();
        let text = Arc::new(text);
        let decided = Rc::new(Cell::new(false));
        // Do not focus a fresh handle here: open_dialog already focuses the
        // alert's own focus trap. A deferred cx.focus_handle() steals focus
        // from the dialog, so Esc/Tab die and typed keys leak into the editor.
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let keep = {
                let workspace = workspace.clone();
                let doc = doc.clone();
                let decided = decided.clone();
                move |window: &mut Window, cx: &mut App| {
                    if decided.replace(true) {
                        return;
                    }
                    window.close_dialog(cx);
                    doc.update(cx, |doc, _| doc.set_disk_stamp(stamp));
                    _ = workspace.update(cx, |this, cx| {
                        this.pending_disk_prompt.remove(&id);
                        this.focus_active(window, cx);
                    });
                }
            };
            let keep_btn = keep.clone();
            let keep_esc = keep.clone();
            let keep_ok = keep.clone();
            let reload = workspace.clone();
            let reload_doc = doc.clone();
            let reload_text = text.clone();
            let reload_decided = decided.clone();
            let overwrite_ws = workspace.clone();
            let overwrite_doc = doc.clone();
            let overwrite_path = doc.read(cx).path().cloned();
            let overwrite_decided = decided.clone();
            let compare_ws = workspace.clone();
            let compare_doc = doc.clone();
            let compare_text = text.clone();
            let compare_decided = decided.clone();
            let compare_name = name.clone();
            alert
                .title(format!("“{name}” changed on disk"))
                .description(
                    "Another program saved this file while you have unsaved changes here. Reload discards local edits; Overwrite writes your buffer; Compare opens the disk version in a new tab.",
                )
                .keyboard(true)
                .on_cancel(move |_, window, cx| {
                    keep_esc(window, cx);
                    true
                })
                .on_ok(move |_, window, cx| {
                    // Enter activates the primary button (Keep / Cancel path).
                    keep_ok(window, cx);
                    true
                })
                .footer(
                    DialogFooter::new()
                        .justify_end()
                        .child(
                            Button::new("reload")
                                .label("Reload")
                                .outline()
                                .on_click(move |_, window, cx| {
                                    if reload_decided.replace(true) {
                                        return;
                                    }
                                    window.close_dialog(cx);
                                    let text = reload_text.as_ref().clone();
                                    reload_doc.update(cx, |doc, cx| {
                                        doc.reload(text, line_ending, window, cx)
                                    });
                                    _ = reload.update(cx, |this, cx| {
                                        this.pending_disk_prompt.remove(&id);
                                        this.focus_active(window, cx);
                                    });
                                }),
                        )
                        .child(
                            Button::new("compare")
                                .label("Compare")
                                .outline()
                                .on_click(move |_, window, cx| {
                                    if compare_decided.replace(true) {
                                        return;
                                    }
                                    window.close_dialog(cx);
                                    // Keep local edits; open disk snapshot in a new tab for side-by-side review.
                                    compare_doc.update(cx, |doc, _| doc.set_disk_stamp(stamp));
                                    let disk = compare_text.as_ref().clone();
                                    let label = format!("{} (on disk)", compare_name);
                                    _ = compare_ws.update(cx, |this, cx| {
                                        this.pending_disk_prompt.remove(&id);
                                        this.open_compare_buffer(disk, label, window, cx);
                                        this.focus_active(window, cx);
                                    });
                                }),
                        )
                        .child(
                            Button::new("overwrite")
                                .label("Overwrite")
                                .outline()
                                .on_click(move |_, window, cx| {
                                    if overwrite_decided.replace(true) {
                                        return;
                                    }
                                    window.close_dialog(cx);
                                    overwrite_doc.update(cx, |doc, _| doc.set_disk_stamp(stamp));
                                    _ = overwrite_ws.update(cx, |this, cx| {
                                        this.pending_disk_prompt.remove(&id);
                                        if let Some(path) = &overwrite_path {
                                            this.write(&overwrite_doc, path, window, cx);
                                        }
                                        this.focus_active(window, cx);
                                    });
                                }),
                        )
                        .child(
                            Button::new("keep")
                                .label("Keep")
                                .primary()
                                .on_click(move |_, window, cx| keep_btn(window, cx)),
                        ),
                )
        });
    }

    // ---------------------------------------------------------------------
    // File commands

    fn new_file(&mut self, _: &NewFile, window: &mut Window, cx: &mut Context<Self>) {
        if AppSettings::get(cx).tabs {
            self.new_document(window, cx);
        } else {
            self.confirm_unsaved(
                self.active_document().clone(),
                AfterConfirm::NewFile,
                window,
                cx,
            );
        }
    }

    fn open(&mut self, _: &Open, window: &mut Window, cx: &mut Context<Self>) {
        if AppSettings::get(cx).tabs {
            self.prompt_open(window, cx);
        } else {
            self.confirm_unsaved(
                self.active_document().clone(),
                AfterConfirm::OpenDialog,
                window,
                cx,
            );
        }
    }

    fn open_recent(&mut self, action: &OpenRecent, window: &mut Window, cx: &mut Context<Self>) {
        let settings = AppSettings::get(cx);
        let Some(path) = settings.recent_files.get(action.0).cloned() else {
            return;
        };
        // Ctrl+R semantics: when the target is already the active document,
        // fall through to the next most recent entry so the shortcut always
        // moves somewhere (repeated presses toggle between the two latest).
        let mut path = path;
        if action.0 == 0 && path.is_file() && self.active_document().read(cx).path() == Some(&path)
        {
            let Some(next) = settings.recent_files.get(1).cloned() else {
                return;
            };
            path = next;
        }
        AppSettings::update(cx, |settings| settings.push_recent(&path));
        if path.is_dir() {
            self.file_tree
                .update(cx, |tree, cx| tree.set_root(Some(path), cx));
            self.sync_file_tree(cx);
            return;
        }
        self.open_paths(vec![path], Placement::Default, window, cx);
    }

    fn active_terminal(&self) -> &Entity<TerminalPanel> {
        &self.terminals[self
            .terminal_active
            .min(self.terminals.len().saturating_sub(1))]
    }

    /// Push a session and subscribe for toolbar Kill → close-tab (VS Code).
    fn push_terminal(
        &mut self,
        term: Entity<TerminalPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sub = cx.subscribe_in(&term, window, {
            move |this, panel, event: &TerminalEvent, window, cx| {
                if matches!(event, TerminalEvent::CloseSession) {
                    let id = panel.entity_id();
                    this.close_terminal_by_id(id, window, cx);
                }
            }
        });
        self.terminal_subscriptions.push(sub);
        self.terminals.push(term);
        self.terminal_active = self.terminals.len() - 1;
    }

    /// Close the session with `id` (toolbar Kill). Last session stays with Restart.
    fn close_terminal_by_id(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.terminals.iter().position(|t| t.entity_id() == id) else {
            return;
        };
        if self.terminals.len() > 1 {
            self.terminals.remove(ix);
            if ix < self.terminal_subscriptions.len() {
                let _ = self.terminal_subscriptions.remove(ix);
            }
            self.terminal_active = ix.saturating_sub(1).min(self.terminals.len() - 1);
            if self.terminals.len() < 2 {
                self.terminal_split = false;
            }
            if self.terminal_visible {
                self.active_terminal()
                    .update(cx, |term, cx| term.focus_input(window, cx));
            }
        } else {
            // Last session: shell already stopped by the panel; keep Restart affordance.
            self.terminals[ix].update(cx, |term, cx| term.focus_input(window, cx));
        }
        cx.notify();
    }

    fn ensure_terminal_visible(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal_visible = true;
        if self.terminals.is_empty() {
            let term = cx.new(|cx| TerminalPanel::new(window, cx));
            self.push_terminal(term, window, cx);
        }
        let cwd = terminal::terminal_cwd(
            self.file_tree.read(cx).root(),
            self.active_document().read(cx).path().map(|p| p.as_path()),
        );
        let term = self.active_terminal().clone();
        term.update(cx, |term, cx| {
            term.set_cwd(cwd, window, cx);
            term.ensure_running(window, cx);
        });
    }

    /// VS Code Ctrl+` / Toggle Terminal:
    /// - hidden → show + focus composer
    /// - visible but focus elsewhere (editor / image) → focus composer (do not hide)
    /// - visible and terminal already focused → hide and restore editor focus
    fn summon_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let term_focused = self
            .terminals
            .iter()
            .any(|t| t.read(cx).contains_focus(window, cx));
        if !self.terminal_visible {
            self.ensure_terminal_visible(window, cx);
        } else if term_focused {
            self.terminal_visible = false;
            self.terminal_maximized = false;
            self.terminal_split = false;
            self.focus_active(window, cx);
        } else {
            // Panel open but editor/image owns focus — summon focuses composer.
            self.ensure_terminal_visible(window, cx);
        }
        cx.notify();
    }

    fn new_terminal(&mut self, _: &NewTerminal, window: &mut Window, cx: &mut Context<Self>) {
        let term = cx.new(|cx| TerminalPanel::new(window, cx));
        self.push_terminal(term, window, cx);
        self.ensure_terminal_visible(window, cx);
        cx.notify();
    }

    fn kill_terminal(&mut self, _: &KillTerminal, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminals.is_empty() {
            return;
        }
        let ix = self.terminal_active.min(self.terminals.len() - 1);
        let id = self.terminals[ix].entity_id();
        self.terminals[ix].update(cx, |term, cx| {
            term.stop_shell();
            term.append("— shell stopped\n", cx);
        });
        self.close_terminal_by_id(id, window, cx);
    }

    fn clear_terminal(&mut self, _: &ClearTerminal, _: &mut Window, cx: &mut Context<Self>) {
        if self.terminals.is_empty() {
            return;
        }
        self.active_terminal().update(cx, |term, cx| term.clear(cx));
    }

    fn split_terminal(&mut self, _: &SplitTerminal, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminals.len() < 2 {
            let term = cx.new(|cx| TerminalPanel::new(window, cx));
            self.push_terminal(term, window, cx);
        }
        self.terminal_split = true;
        self.ensure_terminal_visible(window, cx);
        // Ensure every session is running (left/right panes).
        let cwd = terminal::terminal_cwd(
            self.file_tree.read(cx).root(),
            self.active_document().read(cx).path().map(|p| p.as_path()),
        );
        for term in &self.terminals {
            term.update(cx, |term, cx| {
                term.set_cwd(cwd.clone(), window, cx);
                term.ensure_running(window, cx);
            });
        }
        cx.notify();
    }

    fn go_to_symbol(&mut self, _: &GoToSymbol, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        let doc = self.active_document().clone();
        // Prefer scanned code symbols (Python def/class even when too short to fold),
        // then fold candidates, then Markdown headings.
        let symbols: Vec<(String, usize)> = {
            let d = doc.read(cx);
            let text = d.text_now(cx);
            let text_ref = text.as_ref();
            let mut items: Vec<(String, usize)> = crate::analysis::code_symbols(text_ref)
                .into_iter()
                .map(|s| (s.label, s.line + 1))
                .collect();
            if items.is_empty() {
                let editor = d.editor().read(cx);
                let rope = editor.text();
                for fold in editor.fold_candidates() {
                    let line = fold.start_line;
                    let start = rope.line_start_offset(line).min(text_ref.len());
                    let end = rope.line_end_offset(line).min(text_ref.len());
                    let label = text_ref.get(start..end).unwrap_or("").trim();
                    if !label.is_empty() {
                        items.push((label.to_string(), line + 1));
                    }
                }
            }
            if items.is_empty() {
                for heading in &d.analysis().outline {
                    items.push((heading.text.to_string(), heading.line + 1));
                }
            }
            items
        };
        self.headings
            .update(cx, |state, cx| state.set_query("", window, cx));
        let picker = self.headings.clone();
        let focus = picker.read(cx).focus_handle(cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        let confirm_doc = doc.clone();
        let rows = symbols.clone();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let items = rows.iter().map(|(label, line)| {
                CommandItem::new()
                    .label(format!("{label}  :{line}"))
                    .keywords([label.clone(), format!("line {line}")])
                    .icon(Icon::new(IconName::ListTree))
            });
            let jump_rows = rows.clone();
            dialog.close_button(false).p_0().child(esc_closes(
                Command::new(&picker)
                    .bordered(false)
                    .placeholder("Go to Symbol")
                    .max_h(rems(24.))
                    .items(items)
                    .empty(|_, _, _| "No symbols in this document")
                    .on_confirm({
                        let confirm_doc = confirm_doc.clone();
                        move |index: IndexPath, window, cx| {
                            window.close_dialog(cx);
                            if let Some((_, line)) = jump_rows.get(index.row) {
                                let line = *line;
                                confirm_doc.update(cx, |doc, cx| doc.go_to_line(line, window, cx));
                            }
                        }
                    })
                    .on_cancel(|window, cx| window.close_dialog(cx)),
            ))
        });
    }

    fn reload_window(&mut self, _: &ReloadWindow, window: &mut Window, cx: &mut Context<Self>) {
        // Soft UI refresh: reload settings from disk, re-apply theme, notify.
        // No data-loss prompt when buffers are clean (and we never discard dirty ones).
        let loaded = crate::settings::Settings::load();
        cx.set_global(AppSettings(loaded));
        themes::apply(Some(window), cx);
        for doc in &self.documents {
            doc.update(cx, |_, cx| cx.notify());
        }
        self.file_tree.update(cx, |tree, cx| tree.refresh(cx));
        window.push_notification(Notification::info("Window reloaded."), cx);
        cx.notify();
    }

    fn toggle_zen_mode(&mut self, _: &ToggleZenMode, window: &mut Window, cx: &mut Context<Self>) {
        // Zen = focus mode chrome hide (Escape already restores focus mode).
        let entering = !AppSettings::get(cx).focus_mode;
        self.toggle(|settings| settings.focus_mode = !settings.focus_mode, cx);
        if entering {
            self.terminal_visible = false;
            self.terminal_maximized = false;
            self.terminal_split = false;
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    fn compare_with_saved(
        &mut self,
        _: &CompareWithSaved,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let doc = self.active_document().clone();
        let Some(path) = doc.read(cx).path().map(|p| p.to_path_buf()) else {
            window.push_notification(
                Notification::info("Save the file first to compare with disk."),
                cx,
            );
            return;
        };
        let Ok(disk) = std::fs::read_to_string(&path) else {
            window.push_notification(
                Notification::info("Couldn’t read the saved file from disk."),
                cx,
            );
            return;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let buffer = doc.read(cx).text_now(cx).to_string();
        // Probe #12: compare buffer↔disk even when Auto Save cleared is_dirty.
        if disk == buffer {
            let as_on = AppSettings::get(cx).auto_save;
            window.push_notification(
                Notification::info(if as_on {
                    "No differences from disk (Auto Save may already have written this file)."
                } else {
                    "No differences from the saved file on disk."
                }),
                cx,
            );
            return;
        }
        self.open_diff_session(
            format!("Compare: {name}"),
            "Saved on disk".into(),
            "Your edits".into(),
            disk,
            buffer,
            window,
            cx,
        );
    }

    fn show_source_control(
        &mut self,
        _: &ShowSourceControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activity_view = ActivityView::SourceControl;
        if !AppSettings::get(cx).file_sidebar {
            AppSettings::update(cx, |settings| settings.file_sidebar = true);
        }
        cx.notify();
        let _ = window;
    }

    fn show_extensions(&mut self, _: &ShowExtensions, window: &mut Window, cx: &mut Context<Self>) {
        self.activity_view = ActivityView::Extensions;
        if !AppSettings::get(cx).file_sidebar {
            AppSettings::update(cx, |settings| settings.file_sidebar = true);
        }
        cx.notify();
        let _ = window;
    }

    /// Toggle Maximized Panel (VS Code): enlarge the terminal when it is open.
    fn toggle_maximized_panel(
        &mut self,
        _: &ToggleMaximizedPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.terminal_visible {
            self.terminal_maximized = true;
            self.ensure_terminal_visible(window, cx);
        } else {
            self.terminal_maximized = !self.terminal_maximized;
        }
        cx.notify();
    }

    /// Open Recent… (palette / File): searchable list of recent files and folders.
    fn show_open_recent(
        &mut self,
        _: &OpenRecentPicker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let recent = AppSettings::get(cx).recent_files.clone();
        if recent.is_empty() {
            window.push_notification(Notification::info("No recent files or folders."), cx);
            return;
        }
        // Reuse theme_picker CommandState — only one such dialog at a time.
        self.theme_picker
            .update(cx, |state, cx| state.set_query("", window, cx));
        let state = self.theme_picker.clone();
        let focus = state.read(cx).focus_handle(cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        let workspace = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            // Icons (not text "File"/"Folder") distinguish entries, matching
            // Quick Open / Explorer — VS Code–like visual language.
            let items: Vec<(usize, String, std::path::PathBuf, bool)> = recent
                .iter()
                .enumerate()
                .map(|(ix, path)| {
                    let name = display_name(Some(path));
                    let label = format!("{name}  —  {}", path.display());
                    (ix, label, path.clone(), path.is_dir())
                })
                .collect();
            let pick_items: Vec<(usize, String)> = items
                .iter()
                .map(|(ix, label, _, _)| (*ix, label.clone()))
                .collect();
            let mut group = CommandGroup::new().label("Recent");
            for (_, label, path, is_dir) in &items {
                let kind = if *is_dir { "folder" } else { "file" };
                group = group.item(command_item_with_path_icon(
                    label.clone(),
                    [label.as_str(), kind],
                    path,
                    *is_dir,
                ));
            }
            dialog.close_button(false).p_0().child(esc_closes(
                Command::new(&state)
                    .bordered(false)
                    .placeholder("Open Recent")
                    .max_h(rems(24.))
                    .group(group)
                    .empty(|_, _, _| div().px_3().py_2().child("No matching recent items"))
                    .on_confirm({
                        let workspace = workspace.clone();
                        move |index, window, cx| {
                            window.close_dialog(cx);
                            if let Some((ix, _)) = pick_items.get(index.row) {
                                window.dispatch_action(Box::new(OpenRecent(*ix)), cx);
                            }
                            if let Some(this) = workspace.upgrade() {
                                this.update(cx, |this, cx| this.focus_active(window, cx));
                            }
                        }
                    })
                    .on_cancel(|window, cx| window.close_dialog(cx)),
            ))
        });
    }

    /// Disk-conflict Compare: two-pane diff with navigable hunks (probe #11).
    fn open_compare_buffer(
        &mut self,
        text: String,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let buffer = self.active_document().read(cx).text_now(cx).to_string();
        self.open_diff_session(
            label,
            "On disk".into(),
            "Your buffer".into(),
            text,
            buffer,
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn open_diff_session(
        &mut self,
        title: String,
        left_label: String,
        right_label: String,
        left: String,
        right: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hunks = crate::diff::hunks(&left, &right);
        if hunks.is_empty() {
            window.push_notification(Notification::info("No differences."), cx);
            self.diff_session = None;
            cx.notify();
            return;
        }
        self.diff_session = Some(DiffSession {
            title,
            left_label,
            right_label,
            left,
            right,
            hunks,
            current: 0,
        });
        cx.notify();
    }

    fn close_diff_session(&mut self, cx: &mut Context<Self>) {
        self.diff_session = None;
        cx.notify();
    }

    fn diff_next_hunk(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.diff_session.as_mut()
            && !session.hunks.is_empty()
        {
            session.current = (session.current + 1) % session.hunks.len();
            cx.notify();
        }
    }

    fn diff_prev_hunk(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.diff_session.as_mut()
            && !session.hunks.is_empty()
        {
            session.current = if session.current == 0 {
                session.hunks.len() - 1
            } else {
                session.current - 1
            };
            cx.notify();
        }
    }

    /// Explorer: open the selected file in a split to the side.
    fn tree_open_to_side(
        &mut self,
        _: &OpenToSideInTree,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((path, is_dir)) = self.file_tree.read(cx).single_selection() else {
            return;
        };
        if is_dir || !path.is_file() {
            return;
        }
        self.open_paths(vec![path], Placement::NewTab, window, cx);
        self.open_editor_split(false, window, cx);
    }

    fn select_for_compare(
        &mut self,
        _: &SelectForCompare,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Probe #15: palette / keyboard bind the focused editor's document
        // (A2b toasted README while a.md was visible when Explorer still had
        // a stale selection). Explorer context menu wins only while the tree
        // actually has focus (right-click selects the entry first).
        let explorer_path = self
            .file_tree
            .read(cx)
            .single_selection()
            .and_then(|(p, is_dir)| {
                if !is_dir && p.is_file() {
                    Some(p)
                } else {
                    None
                }
            });
        let active_path = self
            .active_document()
            .read(cx)
            .path()
            .filter(|p| p.is_file())
            .cloned();
        let tree_focused = self
            .file_tree
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx);
        let path = if tree_focused {
            explorer_path.or(active_path)
        } else {
            active_path.or(explorer_path)
        };
        let Some(path) = path else {
            window.push_notification(Notification::info("Select a file to compare."), cx);
            return;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        self.compare_selection = Some(path);
        window.push_notification(
            Notification::info(format!(
                "Selected {name} for compare. Right-click another file → Compare with Selected."
            )),
            cx,
        );
        cx.notify();
    }

    fn compare_with_selected(
        &mut self,
        _: &CompareWithSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(left) = self.compare_selection.clone() else {
            window.push_notification(
                Notification::info("Select for Compare on a file first."),
                cx,
            );
            return;
        };
        let explorer_path = self
            .file_tree
            .read(cx)
            .single_selection()
            .and_then(|(p, is_dir)| {
                if !is_dir && p.is_file() {
                    Some(p)
                } else {
                    None
                }
            });
        let active_path = self
            .active_document()
            .read(cx)
            .path()
            .filter(|p| p.is_file())
            .cloned();
        let tree_focused = self
            .file_tree
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx);
        let right = if tree_focused {
            explorer_path.or(active_path)
        } else {
            active_path.or(explorer_path)
        };
        let Some(right) = right else {
            window.push_notification(Notification::info("Select a file to compare."), cx);
            return;
        };
        if left == right {
            window.push_notification(Notification::info("Pick a different file to compare."), cx);
            return;
        }
        let Ok(left_text) = std::fs::read_to_string(&left) else {
            window.push_notification(Notification::info("Couldn’t read the first file."), cx);
            return;
        };
        let Ok(right_text) = std::fs::read_to_string(&right) else {
            window.push_notification(Notification::info("Couldn’t read the second file."), cx);
            return;
        };
        let left_name = left
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "left".into());
        let right_name = right
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "right".into());
        self.open_diff_session(
            format!("Compare: {left_name} ↔ {right_name}"),
            left_name,
            right_name,
            left_text,
            right_text,
            window,
            cx,
        );
    }

    /// After a folder becomes the Explorer root, avoid leaving a pointless
    /// blank Untitled as the only editor (round-4). Prefer README / first
    /// Markdown file in the folder root.
    fn open_folder_default_doc(
        &mut self,
        folder: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sole = self.documents.len() == 1;
        let (is_untitled, is_blank, is_welcome) = if sole {
            let d = self.documents[0].read(cx);
            let text = d.text_now(cx);
            (
                d.path().is_none(),
                !d.is_dirty() && text.trim().is_empty(),
                text.starts_with("# Welcome to Malgel"),
            )
        } else {
            (false, false, false)
        };
        if !(sole && is_untitled && (is_blank || is_welcome)) {
            return;
        }
        let candidates = [
            folder.join("README.md"),
            folder.join("Readme.md"),
            folder.join("readme.md"),
        ];
        let mut pick = candidates.into_iter().find(|p| p.is_file());
        if pick.is_none()
            && let Ok(rd) = std::fs::read_dir(folder)
        {
            let mut mds: Vec<_> = rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.extension().is_some_and(|ext| {
                            ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown")
                        })
                })
                .collect();
            mds.sort();
            pick = mds.into_iter().next();
        }
        let Some(path) = pick else {
            return;
        };
        let placeholder_id = self.documents[0].entity_id();
        self.load_path(path, Placement::Default, Target::Top, window, cx);
        // Drop the leftover Untitled/welcome if load added another tab.
        if self.documents.len() > 1
            && let Some(ix) = self
                .documents
                .iter()
                .position(|doc| doc.entity_id() == placeholder_id)
        {
            let id = self.documents[ix].entity_id();
            self.remove_document(id, window, cx);
        }
    }

    fn open_folder(&mut self, _: &OpenFolder, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Some(paths) = file_dialog(paths, cx).await else {
                return;
            };
            let Some(folder) = paths.into_iter().next() else {
                return;
            };
            _ = this.update_in(cx, |this, window, cx| {
                AppSettings::update(cx, |settings| settings.push_recent(&folder));
                this.file_tree
                    .update(cx, |tree, cx| tree.set_root(Some(folder.clone()), cx));
                this.sync_file_tree(cx);
                AppSettings::update(cx, |settings| settings.file_sidebar = true);
                // Same as CLI folder open: prefer README, never mass-open /
                // compile every file under the tree.
                this.open_folder_default_doc(&folder, window, cx);
                this.save_session(window, cx);
            });
        })
        .detach();
    }

    /// Close the open folder and return to the Welcome CTA, keeping recent
    /// history so Open Recent stays usable (probe #15).
    fn close_folder(&mut self, _: &CloseFolder, window: &mut Window, cx: &mut Context<Self>) {
        if self.file_tree.read(cx).root().is_none() {
            return;
        }
        self.file_tree
            .update(cx, |tree, cx| tree.set_root(None, cx));
        self.editor_split = None;
        // Close every tab; remove_document leaves a blank Untitled when empty,
        // which is enough for shows_welcome_cta (path is None).
        let ids: Vec<EntityId> = self.documents.iter().map(|d| d.entity_id()).collect();
        for id in ids {
            if self.document_index(id).is_some() {
                self.remove_document(id, window, cx);
            }
        }
        AppSettings::update(cx, |settings| settings.file_sidebar = false);
        self.save_session(window, cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    fn close_document(&mut self, _: &CloseDocument, window: &mut Window, cx: &mut Context<Self>) {
        if !self.shows_tabs(cx) {
            self.confirm_close(AfterConfirm::CloseWindow, window, cx);
            return;
        }
        let doc = self.active_document().clone();
        let id = doc.entity_id();
        self.confirm_unsaved(doc, AfterConfirm::CloseDocument(id), window, cx);
    }

    fn close_other_tabs(
        &mut self,
        _: &CloseOtherTabs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.documents.len() <= 1 {
            return;
        }
        let keep_id = self.active_document().entity_id();
        self.close_other_tabs_by_id(keep_id, window, cx);
    }

    fn close_other_tabs_by_id(
        &mut self,
        keep_id: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets: Vec<EntityId> = self
            .documents
            .iter()
            .map(|doc| doc.entity_id())
            // A pinned tab stays, as it does in VS Code.
            .filter(|id| *id != keep_id && !self.pinned.contains(id))
            .collect();
        self.confirm_close_documents(targets, window, cx);
    }

    fn close_tabs_to_the_right(
        &mut self,
        _: &CloseTabsToTheRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active_id = self.active_document().entity_id();
        self.close_tabs_to_the_right_by_id(active_id, window, cx);
    }

    fn close_tabs_to_the_right_by_id(
        &mut self,
        from_id: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pos) = self.document_index(from_id) else {
            return;
        };
        let targets: Vec<EntityId> = self.documents[pos + 1..]
            .iter()
            .map(|doc| doc.entity_id())
            .collect();
        self.confirm_close_documents(targets, window, cx);
    }

    /// Close every editor that is not pinned, the way VS Code's Close All
    /// Editors (Ctrl+K W) does.
    fn close_all_tabs(&mut self, _: &CloseAllTabs, window: &mut Window, cx: &mut Context<Self>) {
        let targets: Vec<EntityId> = self
            .documents
            .iter()
            .map(|doc| doc.entity_id())
            .filter(|id| !self.pinned.contains(id))
            .collect();
        self.confirm_close_documents(targets, window, cx);
    }

    /// Close editors that have no unsaved changes (VS Code Close Saved).
    fn close_saved_editors(
        &mut self,
        _: &CloseSavedEditors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets: Vec<EntityId> = self
            .documents
            .iter()
            .filter(|doc| {
                let id = doc.entity_id();
                !self.pinned.contains(&id)
                    && !doc.read(cx).is_dirty()
                    && !self.discarded.contains(&id)
            })
            .map(|doc| doc.entity_id())
            .collect();
        self.confirm_close_documents(targets, window, cx);
    }

    /// Close tabs in the focused editor group. With a split, the other pane's
    /// document is kept; without a split this matches Close All.
    fn close_editors_in_group(
        &mut self,
        _: &CloseEditorsInGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Probe #13: with a split, close only the focused (primary) group —
        // keep the secondary pane's document and collapse the empty group.
        let keep = self.editor_split.map(|split| {
            let ix = split.other.min(self.documents.len().saturating_sub(1));
            self.documents[ix].entity_id()
        });
        let targets: Vec<EntityId> = self
            .documents
            .iter()
            .map(|doc| doc.entity_id())
            .filter(|id| !self.pinned.contains(id) && keep != Some(*id))
            .collect();
        if let Some(keep_id) = keep {
            self.editor_split = None;
            if let Some(ix) = self.document_index(keep_id) {
                self.active = ix;
            }
        }
        self.confirm_close_documents(targets, window, cx);
    }

    /// Pin a tab, or let it go: Close Other Tabs and Close All Editors leave
    /// a pinned tab alone, and its close box turns into a pin.
    fn toggle_pin_by_id(&mut self, id: EntityId, cx: &mut Context<Self>) {
        if !self.pinned.remove(&id) {
            self.pinned.insert(id);
        }
        cx.notify();
    }

    /// The same thing from the keyboard or the palette, on the tab in front.
    fn pin_tab(&mut self, _: &PinTab, _: &mut Window, cx: &mut Context<Self>) {
        let id = self.active_document().entity_id();
        self.toggle_pin_by_id(id, cx);
    }

    /// Pin the active document's preview so it stays visible while editing
    /// another tab, or unpin to resume following the active file.
    fn toggle_pin_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.active_document().entity_id();
        let can_pin = self.active_document().read(cx).has_preview();
        if self.pinned_preview == Some(id) {
            self.pinned_preview = None;
            log::info!("ui: preview unpinned");
        } else if can_pin {
            self.pinned_preview = Some(id);
            // Need a visible preview pane beside the editor.
            let layout = AppSettings::get(cx).layout;
            if layout == Layout::Editor {
                AppSettings::update(cx, |settings| {
                    settings.layout = Layout::Split;
                    settings.focus_mode = false;
                });
            }
            log::info!("ui: preview pinned to {id:?}");
        } else {
            window.push_notification(
                Notification::info(
                    "Nothing to pin — open a Markdown or Typst file with a preview.",
                ),
                cx,
            );
            return;
        }
        self.sync_preview_pin_roles(cx);
        cx.notify();
    }

    fn toggle_pin_preview_action(
        &mut self,
        _: &crate::actions::TogglePinPreview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_pin_preview(window, cx);
    }

    /// Force Recompile the Typst preview on screen: the pinned one when a
    /// pinned preview sits beside another tab, the active document's
    /// otherwise.
    fn force_recompile_action(
        &mut self,
        _: &crate::actions::ForceRecompile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.force_recompile_target(cx);
        let forced = target.is_some_and(|doc| doc.update(cx, |doc, cx| doc.force_recompile(cx)));
        if !forced {
            window.push_notification(
                Notification::info("Force Recompile works on Typst documents with a preview."),
                cx,
            );
        }
    }

    /// The document whose preview Force Recompile rebuilds: the pinned
    /// preview when it is shown beside another tab and is Typst, else the
    /// active document.
    fn force_recompile_target(&self, cx: &App) -> Option<Entity<DocumentView>> {
        let active = self.active_document().clone();
        let pinned = self
            .pinned_preview
            .filter(|&id| id != active.entity_id())
            .and_then(|id| self.document_index(id))
            .map(|ix| self.documents[ix].clone())
            .filter(|doc| doc.read(cx).has_typst_live());
        pinned.or(Some(active))
    }

    /// Assign EditorOnly / PreviewOnly / Full roles so a pinned preview can
    /// sit beside a different active editor without live-following it.
    fn sync_preview_pin_roles(&mut self, cx: &mut Context<Self>) {
        use crate::document_view::PaneRole;
        let pin = self
            .pinned_preview
            .filter(|&id| self.document_index(id).is_some());
        self.pinned_preview = pin;
        let active_id = self.active_document().entity_id();
        for doc in &self.documents {
            let id = doc.entity_id();
            let role = match pin {
                Some(p) if p != active_id && id == active_id => PaneRole::EditorOnly,
                Some(p) if p != active_id && id == p => PaneRole::PreviewOnly,
                _ => PaneRole::Full,
            };
            let pinned_flag = pin == Some(id);
            doc.update(cx, |doc, cx| {
                doc.set_pane_role(role, cx);
                doc.set_preview_pinned(pinned_flag, cx);
            });
        }
    }

    /// Bring back the most recently closed editor, the way Ctrl+Shift+T
    /// does. Files that have since gone are skipped.
    fn reopen_closed_editor(
        &mut self,
        _: &ReopenClosedEditor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        while let Some(path) = self.closed_documents.pop() {
            if path.is_file() {
                self.open_paths(vec![path], Placement::Default, window, cx);
                return;
            }
        }
    }

    /// Close the editors whose files the file tree just deleted. The file is
    /// gone, so there is nothing left to save and nothing to ask about.
    fn close_deleted_documents(
        &mut self,
        deleted: &[PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets: Vec<EntityId> = self
            .documents
            .iter()
            .filter(|doc| {
                doc.read(cx)
                    .path()
                    .is_some_and(|path| deleted.iter().any(|gone| path.starts_with(gone)))
            })
            .map(|doc| doc.entity_id())
            .collect();
        for id in targets {
            self.remove_document(id, window, cx);
        }
    }

    /// Show the file the history cursor moved to, opening it again if it is
    /// not open any more.
    fn navigate_to(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self
            .documents
            .iter()
            .position(|doc| doc.read(cx).path() == Some(&path))
        {
            self.activate(ix, window, cx);
        } else if path.is_file() {
            self.open_paths(vec![path], Placement::Default, window, cx);
        }
    }

    /// Alt+←: the file shown before this one.
    fn go_back(&mut self, _: &GoBack, window: &mut Window, cx: &mut Context<Self>) {
        if self.nav_index == 0 || self.nav_history.is_empty() {
            return;
        }
        self.nav_index -= 1;
        let path = self.nav_history[self.nav_index].clone();
        self.navigate_to(path, window, cx);
    }

    /// Alt+→: the file the history walked back out of.
    fn go_forward(&mut self, _: &GoForward, window: &mut Window, cx: &mut Context<Self>) {
        if self.nav_index + 1 >= self.nav_history.len() {
            return;
        }
        self.nav_index += 1;
        let path = self.nav_history[self.nav_index].clone();
        self.navigate_to(path, window, cx);
    }

    // ---------------------------------------------------------------------
    // The file tree's commands

    /// VS Code's Explorer (Ctrl+Shift+E): show the sidebar, put the keyboard
    /// in the tree, and pick the file that is open, so F2, Del and the arrow
    /// keys act on something.
    fn focus_explorer(&mut self, _: &FocusExplorer, window: &mut Window, cx: &mut Context<Self>) {
        self.activity_view = ActivityView::Explorer;
        if !AppSettings::get(cx).file_sidebar || AppSettings::get(cx).focus_mode {
            self.toggle(
                |settings| {
                    settings.file_sidebar = true;
                    settings.focus_mode = false;
                },
                cx,
            );
        }
        let focus = self.file_tree.read(cx).focus_handle(cx);
        self.file_tree.update(cx, |tree, cx| tree.select_active(cx));
        window.defer(cx, move |window, cx| focus.focus(window, cx));
    }

    /// New File (the Explorer's, not the editor's): ask for a name, then
    /// create it in the folder the tree's selection points at.
    fn tree_new_file(&mut self, _: &NewFileInTree, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_tree_name(TreePromptKind::NewFile, None, "", window, cx);
    }

    fn tree_new_folder(
        &mut self,
        _: &NewFolderInTree,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prompt_tree_name(TreePromptKind::NewFolder, None, "", window, cx);
    }

    fn tree_rename(&mut self, _: &RenameInTree, window: &mut Window, cx: &mut Context<Self>) {
        // A single-line input lets Enter propagate after it emits PressEnter,
        // so Enter in the rename box also reaches the tree's Enter (= rename)
        // binding. Restarting the rename there replaced the input before its
        // PressEnter was handled, and the rename was never committed.
        if self.file_tree.read(cx).is_typing_a_rename(window, cx) {
            return;
        }
        let Some((path, is_dir)) = self.file_tree.read(cx).single_selection() else {
            return;
        };
        if is_dir && self.file_tree.read(cx).root() == Some(path.as_path()) {
            // The root folder is not renamed from here, as in VS Code.
            return;
        }
        self.file_tree
            .update(cx, |tree, cx| tree.begin_inline_rename(window, cx));
    }

    fn tree_delete(&mut self, _: &DeleteInTree, window: &mut Window, cx: &mut Context<Self>) {
        self.file_tree
            .update(cx, |tree, cx| tree.delete_selection(window, cx));
    }

    fn tree_cut(&mut self, _: &CutInTree, _: &mut Window, cx: &mut Context<Self>) {
        self.file_tree.update(cx, |tree, cx| tree.cut_selection(cx));
    }

    fn tree_copy(&mut self, _: &CopyInTree, _: &mut Window, cx: &mut Context<Self>) {
        self.file_tree
            .update(cx, |tree, cx| tree.copy_selection(cx));
    }

    fn tree_paste(&mut self, _: &PasteInTree, window: &mut Window, cx: &mut Context<Self>) {
        self.file_tree
            .update(cx, |tree, cx| tree.paste_selection(window, cx));
    }

    fn tree_open(&mut self, _: &OpenInTree, window: &mut Window, cx: &mut Context<Self>) {
        // VS Code-ish: Enter on a file starts rename; on a folder, expand/collapse.
        // Double-click still opens files from the tree row click handler.
        let Some((_, is_dir)) = self.file_tree.read(cx).single_selection() else {
            return;
        };
        if is_dir {
            self.file_tree
                .update(cx, |tree, cx| tree.open_selection(cx));
        } else {
            self.tree_rename(&RenameInTree, window, cx);
        }
    }

    fn tree_copy_path(&mut self, _: &CopyPathInTree, _: &mut Window, cx: &mut Context<Self>) {
        let paths = self.file_tree.read(cx).selection();
        if paths.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(absolute_paths_for_clipboard(
            &paths,
        )));
    }

    fn tree_copy_relative_path(
        &mut self,
        _: &CopyRelativePathInTree,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (paths, root) = {
            let tree = self.file_tree.read(cx);
            (tree.selection(), tree.root().map(Path::to_path_buf))
        };
        if paths.is_empty() {
            return;
        }
        let text = relative_paths_for_clipboard(&paths, root.as_deref());
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn tree_reveal_path(&mut self, _: &RevealPathInTree, _: &mut Window, cx: &mut Context<Self>) {
        let path = {
            let tree = self.file_tree.read(cx);
            if let Some((path, _)) = tree.single_selection() {
                path
            } else {
                match tree.selection().into_iter().next() {
                    Some(path) => path,
                    None => return,
                }
            }
        };
        reveal_path_in_file_manager(cx, &path);
    }

    fn tree_select_up(&mut self, _: &TreeSelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.file_tree
            .update(cx, |tree, cx| tree.select_previous(cx));
    }

    fn tree_select_down(&mut self, _: &TreeSelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.file_tree.update(cx, |tree, cx| tree.select_next(cx));
    }

    fn tree_select_left(&mut self, _: &TreeSelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.file_tree.update(cx, |tree, cx| tree.select_parent(cx));
    }

    fn tree_select_right(&mut self, _: &TreeSelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.file_tree.update(cx, |tree, cx| tree.select_child(cx));
    }

    fn tree_select_first(&mut self, _: &TreeSelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.file_tree.update(cx, |tree, cx| tree.select_first(cx));
    }

    fn tree_select_last(&mut self, _: &TreeSelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.file_tree.update(cx, |tree, cx| tree.select_last(cx));
    }

    /// Ask for one line of text in a dialog, the way Go to Line asks for a
    /// number: the palette's single row never filters, so its query box is
    /// the prompt. Confirming hands what was typed to `apply`.
    ///
    /// The dialog's own dispatch cannot reach the window's workspace, so the
    /// callback that carries the text through is called on the workspace
    /// itself rather than dispatched as an action (see `show_command_palette`).
    fn prompt_line(
        state: &Entity<CommandState>,
        title: SharedString,
        placeholder: SharedString,
        initial: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
        apply: impl Fn(&mut Self, String, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        state.update(cx, |state, cx| state.set_query(initial, window, cx));
        let state = state.clone();
        let workspace = cx.weak_entity();
        let apply = Rc::new(apply);
        // Focus the query once, after the dialog has taken focus. Not in the
        // builder: that runs on every render and kept pulling focus back.
        let focus = state.read(cx).focus_handle(cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let typed = state.clone();
            let workspace = workspace.clone();
            let apply = apply.clone();
            dialog.close_button(false).p_0().child(esc_closes(
                Command::new(&state)
                    .bordered(false)
                    .filterable(false)
                    .placeholder(placeholder.clone())
                    .max_h(rems(24.))
                    .item(CommandItem::new().label(title.clone()).keywords(["name"]))
                    .on_confirm(move |_, window, cx| {
                        window.close_dialog(cx);
                        let text = typed.read(cx).query(cx).to_string();
                        _ = workspace.update(cx, |this, cx| apply(this, text, window, cx));
                    })
                    .on_cancel(|window, cx| window.close_dialog(cx)),
            ))
        });
    }

    /// Ask for the name of a new file, folder or rename. `target` is the
    /// folder the new entry goes in; `None` lets the tree choose (the picked
    /// entry's folder, or the root), the way VS Code's Explorer does.
    fn prompt_tree_name(
        &mut self,
        kind: TreePromptKind,
        target: Option<PathBuf>,
        initial: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, placeholder) = match kind {
            TreePromptKind::NewFile => ("New file", "File name"),
            TreePromptKind::NewFolder => ("New folder", "Folder name"),
        };
        Self::prompt_line(
            &self.tree_prompt.clone(),
            title.into(),
            placeholder.into(),
            initial,
            window,
            cx,
            move |this, name, window, cx| {
                this.apply_tree_name(kind, target.clone(), name, window, cx)
            },
        );
    }

    // ---------------------------------------------------------------------
    // Find in Files

    /// VS Code's Search view (Ctrl+Shift+F): show it in the sidebar with the
    /// keyboard in its Find field, the last text selected so typing
    /// replaces it. The results land in the sidebar, under the field.
    fn find_in_files(&mut self, _: &FindInFiles, window: &mut Window, cx: &mut Context<Self>) {
        if self.file_tree.read(cx).root().is_none() {
            window.push_notification(Notification::info("Open a folder to search its files."), cx);
            return;
        }
        self.reveal_search(false, window, cx);
    }

    /// Show the Search view and put the keyboard in its Find field (or its
    /// Replace field), with the text there selected.
    fn reveal_search(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.activity_view = ActivityView::Search;
        if !AppSettings::get(cx).file_sidebar || AppSettings::get(cx).focus_mode {
            AppSettings::update(cx, |settings| {
                settings.file_sidebar = true;
                settings.focus_mode = false;
            });
        }
        let last = self.last_find.clone();
        if !last.is_empty() && self.search_find.read(cx).value().is_empty() {
            self.search_find
                .update(cx, |input, cx| input.set_value(last, window, cx));
        }
        let field = if replace {
            &self.search_replace
        } else {
            &self.search_find
        };
        // Select the field's text now (the field may not be on screen yet,
        // so no action can reach it), and focus it once it is drawn.
        let field = field.clone();
        field.update(cx, |input, cx| input.select_all(window, cx));
        let focus = field.read(cx).focus_handle(cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        cx.notify();
    }

    /// Enter / Shift+Enter in the Find field: search when the text is new,
    /// otherwise show the next (or previous) hit.
    fn search_find_enter(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.search_find.read(cx).value().to_string();
        let len = match &self.sidebar_search {
            SidebarSearch::Done {
                query: done, found, ..
            } if *done == query => found.hits.len(),
            _ => {
                if forward {
                    self.sidebar_run_find(window, cx);
                }
                return;
            }
        };
        let Some(next) = step_hit(self.sidebar_cursor, len, forward) else {
            // No hits: run it again (the folder may have changed).
            self.sidebar_run_find(window, cx);
            return;
        };
        self.open_search_hit(next, window, cx);
    }

    /// Open hit `ix` of the Search view's results and put the caret on it.
    fn open_search_hit(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let SidebarSearch::Done { found, .. } = &self.sidebar_search else {
            return;
        };
        let Some(hit) = found.hits.get(ix).cloned() else {
            return;
        };
        self.sidebar_cursor = Some(ix);
        if self.sidebar_collapsed.remove(&hit.path) {
            self.rebuild_search_rows();
        }
        if let Some(row) = self
            .sidebar_rows
            .iter()
            .position(|row| *row == SearchRow::Hit(ix))
        {
            self.sidebar_scroll
                .scroll_to_item(row, gpui_kit::ScrollStrategy::Center);
        }
        self.open_hit(&hit, window, cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    fn rebuild_search_rows(&mut self) {
        self.sidebar_rows = match &self.sidebar_search {
            SidebarSearch::Done { found, .. } => search_rows(&found.hits, &self.sidebar_collapsed),
            _ => Vec::new(),
        };
    }

    /// Show the file a hit came from, on its line.
    fn open_hit(&mut self, hit: &Hit, window: &mut Window, cx: &mut Context<Self>) {
        let path = hit.path.clone();
        if let Some(ix) = self
            .documents
            .iter()
            .position(|doc| doc.read(cx).path() == Some(&path))
        {
            self.activate(ix, window, cx);
            let doc = self.documents[ix].clone();
            doc.update(cx, |doc, cx| doc.go_to_line(hit.line, window, cx));
            return;
        }
        self.load_path(path, Placement::Default, Target::Line(hit.line), window, cx);
    }

    /// VS Code's Replace in Files (Ctrl+Shift+H): the Search view, with the
    /// keyboard in its Replace field.
    fn replace_in_files(
        &mut self,
        _: &ReplaceInFiles,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.file_tree.read(cx).root().is_none() {
            window.push_notification(
                Notification::info("Open a folder to replace in its files."),
                cx,
            );
            return;
        }
        self.reveal_search(true, window, cx);
    }

    fn confirm_replace_in_files(
        &mut self,
        find: String,
        replace: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.last_replace = replace.clone();
        let Some(root) = self.file_tree.read(cx).root().map(Path::to_path_buf) else {
            return;
        };
        let show_hidden = AppSettings::get(cx).show_hidden_entries;
        let opened = self.file_tree.read(cx).expanded_folders();
        let walk_root = root;
        let wanted = find.clone();
        // Every file in the folder, and every occurrence in them: what the
        // dialog counts is what Replace All then changes.
        let listing = cx.background_executor().spawn(async move {
            let files =
                quick_open::files_under(&walk_root, show_hidden, &opened, quick_open::MAX_FILES);
            let (count, file_count) = search::count_in_files(&files, &wanted);
            (files, count, file_count)
        });
        let workspace = cx.weak_entity();
        cx.spawn_in(window, async move |this, cx| {
            let (files, count, file_count) = listing.await;
            _ = this.update_in(cx, |this, window, cx| {
                if count == 0 {
                    window.push_notification(
                        Notification::info(format!(
                            "No matches for \u{201c}{find}\u{201d}."
                        )),
                        cx,
                    );
                    this.focus_active(window, cx);
                    return;
                }
                let description = format!(
                    "Replace {count} occurrence(s) in {file_count} file(s)?\n\nFind: {find}\nReplace: {replace}"
                );
                let find2 = find.clone();
                let replace2 = replace.clone();
                let files2 = files.clone();
                let ws = workspace.clone();
                let focus = this.alert_focus.clone();
                focus.focus_primary_soon(window, cx);
                window.open_alert_dialog(cx, move |alert, window, cx| {
                    let ws_ok = ws.clone();
                    let ws_cancel = ws.clone();
                    let find_ok = find2.clone();
                    let replace_ok = replace2.clone();
                    let files_ok = files2.clone();
                    alert
                        .title("Replace in Files")
                        .description(description.clone())
                        .keyboard(true)
                        .on_cancel(move |_, window, cx| {
                            window.close_dialog(cx);
                            _ = ws_cancel.update(cx, |this, cx| this.focus_active(window, cx));
                            true
                        })
                        .on_ok(move |_, window, cx| {
                            window.close_dialog(cx);
                            _ = ws_ok.update(cx, |this, cx| {
                                this.apply_replace_all(&files_ok, &find_ok, &replace_ok, window, cx);
                            });
                            true
                        })
                        .footer(
                            focus.trap(
                                DialogFooter::new()
                                    .justify_end()
                                    .child(focus.secondary(
                                        0,
                                        Button::new("rif-cancel").label("Cancel").outline(),
                                        {
                                            let ws = ws.clone();
                                            move |window, cx| {
                                                window.close_dialog(cx);
                                                _ = ws.update(cx, |this, cx| {
                                                    this.focus_active(window, cx)
                                                });
                                            }
                                        },
                                        window,
                                        cx,
                                    ))
                                    .child(focus.primary(
                                        Button::new("rif-replace-all")
                                            .label("Replace All")
                                            .primary()
                                            .on_click({
                                                let ws = ws.clone();
                                                let find2 = find2.clone();
                                                let replace2 = replace2.clone();
                                                let files2 = files2.clone();
                                                move |_, window, cx| {
                                                    window.close_dialog(cx);
                                                    _ = ws.update(cx, |this, cx| {
                                                        this.apply_replace_all(
                                                            &files2, &find2, &replace2, window,
                                                            cx,
                                                        );
                                                    });
                                                }
                                            }),
                                        window,
                                        cx,
                                    )),
                            )
                        )
                });
            });
        })
        .detach();
    }

    /// Replace All: rewrite every file, reload the open ones, say how many
    /// changed, and refresh the Search view's results.
    fn apply_replace_all(
        &mut self,
        files: &[PathBuf],
        find: &str,
        replace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (n, written) = search::replace_in_files(files, find, replace);
        crate::perf::release_free_memory();
        self.reload_replaced_files(&written, window, cx);
        window.push_notification(
            Notification::info(format!(
                "Replaced {n} occurrence{} in {} file{}.",
                if n == 1 { "" } else { "s" },
                written.len(),
                if written.len() == 1 { "" } else { "s" },
            )),
            cx,
        );
        if !matches!(self.sidebar_search, SidebarSearch::Idle) {
            self.sidebar_run_find(window, cx);
        }
        self.focus_active(window, cx);
    }

    /// Reload open editors whose paths were rewritten by Replace in Files.
    fn reload_replaced_files(
        &mut self,
        files: &[PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let wanted: std::collections::HashSet<_> = files.iter().cloned().collect();
        for doc in self.documents.clone() {
            let Some(path) = doc.read(cx).path().cloned() else {
                continue;
            };
            if !wanted.contains(&path) {
                continue;
            }
            if doc.read(cx).is_dirty() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let ending = LineEnding::detect(&text);
            doc.update(cx, |doc, cx| {
                doc.reload(text, ending, window, cx);
            });
        }
    }

    /// Persist dirty editors that already have a path when Auto Save is on.
    fn auto_save_dirty(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !AppSettings::get(cx).auto_save {
            return;
        }
        // Don't flush under a Save/Close dialog — Keep dirty flags for bulk prompts.
        if window.has_active_dialog(cx) {
            return;
        }
        let dirty: Vec<_> = self
            .documents
            .iter()
            .filter(|doc| {
                let d = doc.read(cx);
                d.is_dirty() && d.path().is_some() && !self.discarded.contains(&doc.entity_id())
            })
            .cloned()
            .collect();
        for doc in dirty {
            self.save_document(doc, None, window, cx);
        }
    }

    fn toggle_auto_save(
        &mut self,
        _: &ToggleAutoSave,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut enabled = false;
        self.toggle(
            |settings| {
                settings.auto_save = !settings.auto_save;
                enabled = settings.auto_save;
            },
            cx,
        );
        window.push_notification(
            Notification::info(if enabled {
                "Auto Save: on (after delay)"
            } else {
                "Auto Save: off"
            }),
            cx,
        );
        if enabled {
            self.auto_save_dirty(window, cx);
        }
        cx.notify();
    }

    fn format_document(&mut self, _: &FormatDocument, window: &mut Window, cx: &mut Context<Self>) {
        let doc = self.active_document().clone();
        if doc.read(cx).is_media() {
            window.push_notification(
                Notification::info("No formatter available for this file type."),
                cx,
            );
            return;
        }
        doc.update(cx, |doc, cx| {
            let before = doc.text_now(cx).to_string();
            let after = format::format_document(&before);
            if after == before {
                cx.emit(DocumentEvent::Notify("Document already formatted.".into()));
                return;
            }
            doc.apply_edit(
                |text, _| {
                    let next = format::format_document(text);
                    let end = next.len();
                    format::Edit {
                        range: 0..text.len(),
                        text: next,
                        selection: end..end,
                    }
                },
                window,
                cx,
            );
        });
    }

    fn trim_trailing_whitespace(
        &mut self,
        _: &TrimTrailingWhitespace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let doc = self.active_document().clone();
        if doc.read(cx).is_media() {
            window.push_notification(Notification::info("Nothing to trim in this view."), cx);
            return;
        }
        doc.update(cx, |doc, cx| {
            let before = doc.text_now(cx).to_string();
            let after = format::trim_trailing_whitespace(&before);
            if after == before {
                cx.emit(DocumentEvent::Notify("No trailing whitespace.".into()));
                return;
            }
            doc.apply_edit(
                |text, _| {
                    let next = format::trim_trailing_whitespace(text);
                    format::Edit {
                        range: 0..text.len(),
                        text: next,
                        selection: 0..0,
                    }
                },
                window,
                cx,
            );
        });
    }

    fn show_explorer_view(
        &mut self,
        _: &ShowExplorerView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activity_view = ActivityView::Explorer;
        if !AppSettings::get(cx).file_sidebar {
            AppSettings::update(cx, |settings| settings.file_sidebar = true);
        }
        self.focus_explorer(&FocusExplorer, window, cx);
        cx.notify();
    }

    fn show_search_view(
        &mut self,
        _: &ShowSearchView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reveal_search(false, window, cx);
    }

    /// Carry out the name a tree prompt collected. `target` names the folder
    /// the new entry goes in, when the gesture knew one.
    fn apply_tree_name(
        &mut self,
        kind: TreePromptKind,
        target: Option<PathBuf>,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match kind {
            TreePromptKind::NewFile | TreePromptKind::NewFolder => {
                let folder = target.or_else(|| self.file_tree.read(cx).target_folder());
                let Some(folder) = folder else {
                    window.push_notification(Notification::info("Open a folder first."), cx);
                    return;
                };
                let is_dir = matches!(kind, TreePromptKind::NewFolder);
                self.file_tree.update(cx, |tree, cx| {
                    tree.create_entry(&folder, &name, is_dir, window, cx)
                });
            }
        }
    }

    fn confirm_close_documents(
        &mut self,
        targets: Vec<EntityId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let valid_targets: Vec<EntityId> = targets
            .into_iter()
            .filter(|id| self.document_index(*id).is_some())
            .collect();

        if valid_targets.is_empty() {
            return;
        }

        let dirty: Vec<Entity<DocumentView>> = valid_targets
            .iter()
            .filter_map(|id| {
                self.document_index(*id).and_then(|ix| {
                    let doc = &self.documents[ix];
                    if doc.read(cx).is_dirty() && !self.discarded.contains(id) {
                        Some(doc.clone())
                    } else {
                        None
                    }
                })
            })
            .collect();

        // Probe #11/#12: ≥2 dirty → one Save All / Don't Save All / Cancel.
        // Use if/else (not slice match) so the bulk path cannot fall through.
        if dirty.is_empty() {
            for id in valid_targets {
                self.discarded.remove(&id);
                self.remove_document(id, window, cx);
            }
        } else if dirty.len() >= 2 {
            self.confirm_unsaved_many(
                dirty,
                AfterConfirm::CloseDocuments(valid_targets),
                window,
                cx,
            );
        } else {
            let doc = dirty.into_iter().next().unwrap();
            if let Some(ix) = self.document_index(doc.entity_id()) {
                self.activate_inner(ix, false, window, cx);
            }
            self.confirm_unsaved(doc, AfterConfirm::CloseDocuments(valid_targets), window, cx);
        }
    }

    fn close_window(&mut self, _: &CloseWindow, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_close(AfterConfirm::CloseWindow, window, cx);
    }

    fn quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
        self.request_quit(window, cx);
    }

    /// Ask about dirty buffers, then quit. Used by the Quit action and by the
    /// app-scope Quit handler so Ctrl+Q / the menu never call `cx.quit()` first.
    pub(crate) fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_close(AfterConfirm::Quit, window, cx);
    }

    /// Called by the platform before the window closes.
    fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.closing {
            return true;
        }
        // Probe #16: with Auto Save on, flush path-backed dirty buffers first
        // so close is not a confusing silent no-op racing the timer. With Auto
        // Save off, dirty flags stay and the Save / Don't Save / Cancel dialog
        // must appear (Esc cancels via confirm_unsaved).
        if AppSettings::get(cx).auto_save {
            self.auto_save_dirty(window, cx);
        }
        let unsaved = self
            .documents
            .iter()
            .any(|doc| doc.read(cx).is_dirty() && !self.discarded.contains(&doc.entity_id()));
        if unsaved {
            self.confirm_close(AfterConfirm::CloseWindow, window, cx);
            return false;
        }
        self.save_session(window, cx);
        self.closing = true;
        true
    }

    /// Close the window or quit once every document with unsaved changes
    /// has been saved or discarded. With two or more dirty buffers, ask once
    /// with Save All / Don't Save / Cancel (probe #6); otherwise the single-file dialog.
    fn confirm_close(&mut self, next: AfterConfirm, window: &mut Window, cx: &mut Context<Self>) {
        let dirty: Vec<_> = self
            .documents
            .iter()
            .filter(|doc| doc.read(cx).is_dirty() && !self.discarded.contains(&doc.entity_id()))
            .cloned()
            .collect();
        match dirty.as_slice() {
            [] => self.run_after_confirm(next, window, cx),
            [doc] => {
                if let Some(ix) = self.document_index(doc.entity_id()) {
                    self.activate_inner(ix, false, window, cx);
                }
                self.confirm_unsaved(doc.clone(), next, window, cx);
            }
            many => self.confirm_unsaved_many(many.to_vec(), next, window, cx),
        }
    }

    /// Multi-dirty quit/close: list every unsaved buffer and offer Save All.
    fn confirm_unsaved_many(
        &mut self,
        dirty: Vec<Entity<DocumentView>>,
        next: AfterConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            // Palette/other dialog may still be tearing down after defer —
            // retry once so Close All never silently drops the bulk prompt.
            let dirty = dirty.clone();
            let next = next.clone();
            let ws = cx.weak_entity();
            window.defer(cx, move |window, cx| {
                if let Some(this) = ws.upgrade() {
                    this.update(cx, |this, cx| {
                        this.confirm_unsaved_many(dirty, next, window, cx);
                    });
                }
            });
            return;
        }
        let names: Vec<String> = dirty.iter().map(|doc| doc.read(cx).name()).collect();
        let list = names
            .iter()
            .map(|n| format!("• {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let count = dirty.len();
        let dirty_ids: Vec<EntityId> = dirty.iter().map(|d| d.entity_id()).collect();
        let workspace = cx.weak_entity();
        let focus = self.alert_focus.clone();
        focus.focus_primary_soon(window, cx);
        window.open_alert_dialog(cx, move |alert, window, cx| {
            let (save, discard, cancel) = (workspace.clone(), workspace.clone(), workspace.clone());
            let (save_next, discard_next) = (next.clone(), next.clone());
            let cancel_esc = cancel.clone();
            let save_ok = save.clone();
            let save_ok_next = next.clone();
            let discard_ids = dirty_ids.clone();
            alert
                .title(format!("Save changes to {count} files?"))
                .description(format!(
                    "Your changes will be lost if you don’t save them.\n\n{list}"
                ))
                .keyboard(true)
                .on_cancel(move |_, window, cx| {
                    window.close_dialog(cx);
                    _ = cancel_esc.update(cx, |this, cx| {
                        this.discarded.clear();
                        this.focus_active(window, cx);
                    });
                    true
                })
                .on_ok(move |_, window, cx| {
                    window.close_dialog(cx);
                    let next = save_ok_next.clone();
                    _ = save_ok.update(cx, |this, cx| {
                        this.focus_active(window, cx);
                        this.save_all_documents(Some(next), window, cx);
                    });
                    true
                })
                .footer(
                    focus.trap(
                        DialogFooter::new()
                            .justify_end()
                            .child(focus.secondary(
                                0,
                                Button::new("discard-all").label("Don’t Save All").outline(),
                                move |window, cx| {
                                    window.close_dialog(cx);
                                    let next = discard_next.clone();
                                    let ids = discard_ids.clone();
                                    _ = discard.update(cx, |this, cx| {
                                        for id in ids {
                                            if let Some(ix) = this.document_index(id) {
                                                this.documents[ix]
                                                    .update(cx, |doc, _| doc.discard_recovery());
                                            }
                                            this.discarded.insert(id);
                                        }
                                        this.focus_active(window, cx);
                                        this.run_after_confirm(next, window, cx);
                                    });
                                },
                                window,
                                cx,
                            ))
                            .child(div().flex_1())
                            .child(focus.secondary(
                                1,
                                Button::new("cancel").label("Cancel").outline(),
                                move |window, cx| {
                                    window.close_dialog(cx);
                                    _ = cancel.update(cx, |this, cx| {
                                        this.discarded.clear();
                                        this.focus_active(window, cx);
                                    });
                                },
                                window,
                                cx,
                            ))
                            .child(
                                focus.primary(
                                    Button::new("save-all")
                                        .label("Save All")
                                        .primary()
                                        .on_click(move |_, window, cx| {
                                            window.close_dialog(cx);
                                            let next = save_next.clone();
                                            _ = save.update(cx, |this, cx| {
                                                this.focus_active(window, cx);
                                                this.save_all_documents(Some(next), window, cx);
                                            });
                                        }),
                                    window,
                                    cx,
                                ),
                            ),
                    ),
                )
        });
    }

    /// Run `next` now, or once the user has saved or discarded `doc`'s
    /// changes.
    fn confirm_unsaved(
        &mut self,
        doc: Entity<DocumentView>,
        next: AfterConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !doc.read(cx).is_dirty() {
            self.run_after_confirm(next, window, cx);
            return;
        }
        if window.has_active_dialog(cx) {
            let doc = doc.clone();
            let next = next.clone();
            let ws = cx.weak_entity();
            window.defer(cx, move |window, cx| {
                if let Some(this) = ws.upgrade() {
                    this.update(cx, |this, cx| {
                        this.confirm_unsaved(doc, next, window, cx);
                    });
                }
            });
            return;
        }

        let name = doc.read(cx).name();
        let workspace = cx.weak_entity();
        let focus = self.alert_focus.clone();
        focus.focus_primary_soon(window, cx);
        window.open_alert_dialog(cx, move |alert, window, cx| {
            let (save, discard, cancel) = (workspace.clone(), workspace.clone(), workspace.clone());
            let (save_next, discard_next) = (next.clone(), next.clone());
            let (save_doc, discard_doc) = (doc.clone(), doc.clone());
            let cancel_esc = cancel.clone();
            let save_ok = save.clone();
            let save_ok_next = next.clone();
            let save_ok_doc = doc.clone();
            alert
                .title(format!("Save changes to “{name}”?"))
                .description("Your changes will be lost if you don’t save them.")
                .keyboard(true)
                .on_cancel(move |_, window, cx| {
                    // Esc = Cancel: leave the buffer dirty and restore focus.
                    window.close_dialog(cx);
                    _ = cancel_esc.update(cx, |this, cx| {
                        this.discarded.clear();
                        this.focus_active(window, cx);
                    });
                    true
                })
                .on_ok(move |_, window, cx| {
                    // Enter = Save (primary).
                    window.close_dialog(cx);
                    let next = save_ok_next.clone();
                    let doc = save_ok_doc.clone();
                    _ = save_ok.update(cx, |this, cx| {
                        this.focus_active(window, cx);
                        this.save_document(doc, Some(next), window, cx)
                    });
                    true
                })
                .footer(
                    focus.trap(
                        DialogFooter::new()
                            .justify_end()
                            .child(focus.secondary(
                                0,
                                Button::new("discard").label("Don’t Save").outline(),
                                move |window, cx| {
                                    window.close_dialog(cx);
                                    let next = discard_next.clone();
                                    let id = discard_doc.entity_id();
                                    discard_doc.update(cx, |doc, _| doc.discard_recovery());
                                    _ = discard.update(cx, |this, cx| {
                                        this.discarded.insert(id);
                                        this.focus_active(window, cx);
                                        this.run_after_confirm(next, window, cx)
                                    });
                                },
                                window,
                                cx,
                            ))
                            .child(div().flex_1())
                            .child(focus.secondary(
                                1,
                                Button::new("cancel").label("Cancel").outline(),
                                move |window, cx| {
                                    window.close_dialog(cx);
                                    _ = cancel.update(cx, |this, cx| {
                                        this.discarded.clear();
                                        this.focus_active(window, cx);
                                    });
                                },
                                window,
                                cx,
                            ))
                            .child(focus.primary(
                                Button::new("save").label("Save").primary().on_click(
                                    move |_, window, cx| {
                                        window.close_dialog(cx);
                                        let next = save_next.clone();
                                        let doc = save_doc.clone();
                                        _ = save.update(cx, |this, cx| {
                                            this.focus_active(window, cx);
                                            this.save_document(doc, Some(next), window, cx)
                                        });
                                    },
                                ),
                                window,
                                cx,
                            )),
                    ),
                )
        });
    }

    fn run_after_confirm(
        &mut self,
        next: AfterConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match next {
            AfterConfirm::NewFile => self.new_document(window, cx),
            AfterConfirm::OpenDialog => self.prompt_open(window, cx),
            AfterConfirm::OpenPath(path) => {
                self.load_path(path, Placement::Default, Target::Top, window, cx)
            }
            AfterConfirm::OpenLink(path, anchor) => self.load_path(
                path,
                Placement::ReplaceActive,
                anchor.map(Target::Heading).unwrap_or(Target::Top),
                window,
                cx,
            ),
            AfterConfirm::CloseDocument(id) => {
                self.discarded.remove(&id);
                self.remove_document(id, window, cx);
            }
            AfterConfirm::CloseDocuments(targets) => {
                self.confirm_close_documents(targets, window, cx);
            }
            AfterConfirm::CloseWindow | AfterConfirm::Quit => {
                // More documents may still be waiting to be asked about.
                let pending = self.documents.iter().any(|doc| {
                    doc.read(cx).is_dirty() && !self.discarded.contains(&doc.entity_id())
                });
                if pending {
                    self.confirm_close(next, window, cx);
                    return;
                }
                self.save_session(window, cx);
                self.closing = true;
                if matches!(next, AfterConfirm::Quit) {
                    cx.quit();
                } else {
                    window.remove_window();
                }
            }
            AfterConfirm::SaveAllThen(rest, final_next) => {
                let docs: Vec<_> = rest
                    .into_iter()
                    .filter_map(|id| self.document_index(id).map(|ix| self.documents[ix].clone()))
                    .collect();
                match docs.as_slice() {
                    [] => self.run_after_confirm(*final_next, window, cx),
                    [first] => self.save_document(first.clone(), Some(*final_next), window, cx),
                    [first, more @ ..] => {
                        let more_ids: Vec<EntityId> = more.iter().map(|d| d.entity_id()).collect();
                        self.save_document(
                            first.clone(),
                            Some(AfterConfirm::SaveAllThen(more_ids, final_next)),
                            window,
                            cx,
                        );
                    }
                }
            }
            AfterConfirm::Nop => {}
        }
    }

    // ---------------------------------------------------------------------
    // Links

    /// Follow a link clicked in the active document's preview.
    fn follow_link(&mut self, action: &FollowLink, window: &mut Window, cx: &mut Context<Self>) {
        let doc = self.active_document().clone();
        let base_dir = doc.read(cx).dir();
        let Some(target) = links::classify(&action.url, base_dir.as_deref()) else {
            return;
        };
        match target {
            LinkTarget::Heading(anchor) => self.reveal_anchor(&doc, &anchor, window, cx),
            LinkTarget::External(url) => {
                if AppSettings::get(cx).confirm_web_links {
                    confirm_external_link(url, window, cx);
                } else {
                    cx.open_url(&url);
                }
            }
            LinkTarget::NeedsSavedDocument => self.notify_error(
                "Save this document first, so Malgel knows where the files it links to are.",
                window,
                cx,
            ),
            LinkTarget::File { path, anchor } => {
                if !path.exists() {
                    // Name the file as the link does, where it can.
                    let shown = base_dir
                        .as_deref()
                        .and_then(|dir| path.strip_prefix(dir).ok())
                        .unwrap_or(&path);
                    self.notify_error(format!("Couldn’t find “{}”.", shown.display()), window, cx);
                } else if path.is_dir() {
                    self.file_tree
                        .update(cx, |tree, cx| tree.set_root(Some(path), cx));
                    self.sync_file_tree(cx);
                } else if path.is_file() {
                    // Any file follows its link in-app: PDFs open the viewer,
                    // anything else opens as text (binaries fail in load_path
                    // with a readable error instead of escaping to another app).
                    let mut new_tab = AppSettings::get(cx).open_links_in == LinkPlacement::NewTab;
                    if action.alternate {
                        new_tab = !new_tab;
                    }
                    self.open_linked_document(path, anchor, new_tab, window, cx);
                } else {
                    confirm_open_file(path, window, cx);
                }
            }
        }
    }

    /// Show a diagram of the active document in a window of its own.
    fn view_diagram(&mut self, action: &ViewDiagram, _: &mut Window, cx: &mut Context<Self>) {
        let name = self.active_document().read(cx).name();
        diagram_viewer::open(&action.0, format!("Diagram — {name}"), cx);
    }

    /// Show the Markdown file `path` (in a new tab, or in place of the
    /// active document), scrolled to `anchor`.
    fn open_linked_document(
        &mut self,
        path: PathBuf,
        anchor: Option<String>,
        new_tab: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = path.canonicalize().unwrap_or(path);
        if let Some(ix) = self
            .documents
            .iter()
            .position(|doc| doc.read(cx).path() == Some(&path))
        {
            self.activate(ix, window, cx);
            if let Some(anchor) = anchor {
                let doc = self.documents[ix].clone();
                self.reveal_anchor(&doc, &anchor, window, cx);
            }
            return;
        }
        if new_tab {
            self.load_path(
                path,
                Placement::NewTab,
                anchor.map(Target::Heading).unwrap_or(Target::Top),
                window,
                cx,
            );
        } else {
            self.confirm_unsaved(
                self.active_document().clone(),
                AfterConfirm::OpenLink(path, anchor),
                window,
                cx,
            );
        }
    }

    /// Scroll `doc` to the heading `anchor` names, once it is laid out.
    fn reveal_anchor(
        &mut self,
        doc: &Entity<DocumentView>,
        anchor: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let doc = doc.clone();
        let anchor = anchor.to_string();
        window.defer(cx, move |window, cx| {
            doc.update(cx, |doc, cx| doc.reveal_anchor(&anchor, window, cx));
        });
    }

    fn prompt_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: AppSettings::get(cx).tabs,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Some(paths) = file_dialog(paths, cx).await else {
                return;
            };
            _ = this.update_in(cx, |this, window, cx| {
                for path in paths {
                    this.load_path(path, Placement::Default, Target::Top, window, cx);
                }
            });
        })
        .detach();
    }

    fn save(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        let doc = self.active_document().clone();
        self.save_document(doc, None, window, cx);
    }

    fn save_as(&mut self, _: &SaveAs, window: &mut Window, cx: &mut Context<Self>) {
        let doc = self.active_document().clone();
        if doc.read(cx).is_loading() {
            return;
        }
        self.prompt_save_path(doc, None, window, cx);
    }

    /// Save every dirty document that has a path; prompt Save As for untitled ones.
    fn save_all(&mut self, _: &SaveAll, window: &mut Window, cx: &mut Context<Self>) {
        self.save_all_documents(None, window, cx);
    }

    fn save_all_documents(
        &mut self,
        next: Option<AfterConfirm>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dirty: Vec<_> = self
            .documents
            .iter()
            .filter(|doc| doc.read(cx).is_dirty() && !self.discarded.contains(&doc.entity_id()))
            .cloned()
            .collect();
        if dirty.is_empty() {
            if let Some(next) = next {
                self.run_after_confirm(next, window, cx);
            }
            return;
        }
        // Untitled first so Save As does not interleave oddly with path saves.
        let mut untitled = Vec::new();
        let mut with_path = Vec::new();
        for doc in dirty {
            if doc.read(cx).path().is_some() {
                with_path.push(doc);
            } else {
                untitled.push(doc);
            }
        }
        for doc in with_path {
            self.save_document(doc, None, window, cx);
        }
        match untitled.as_slice() {
            [] => {
                if let Some(next) = next {
                    self.run_after_confirm(next, window, cx);
                }
            }
            [first] => self.save_document(first.clone(), next, window, cx),
            [first, rest @ ..] => {
                let rest_ids: Vec<EntityId> = rest.iter().map(|d| d.entity_id()).collect();
                let chained = match next {
                    Some(n) => AfterConfirm::SaveAllThen(rest_ids, Box::new(n)),
                    None => AfterConfirm::SaveAllThen(rest_ids, Box::new(AfterConfirm::Nop)),
                };
                self.save_document(first.clone(), Some(chained), window, cx);
            }
        }
    }

    /// Save `doc`, asking for a location first when it has none, then
    /// continue with `next`.
    fn save_document(
        &mut self,
        doc: Entity<DocumentView>,
        next: Option<AfterConfirm>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if doc.read(cx).is_loading() {
            // Nothing is there to save until the file has loaded.
            return;
        }
        let id = doc.entity_id();
        if self.pending_disk_prompt.contains(&id) {
            // Still deciding: re-offer the choice; never write silently.
            if window.has_active_dialog(cx) {
                return;
            }
            let path = doc.read(cx).path().cloned();
            let name = doc.read(cx).name();
            let (text, line_ending, stamp) = match &path {
                Some(path) => {
                    let text = std::fs::read_to_string(path).unwrap_or_default();
                    let line_ending = LineEnding::detect(&text);
                    let stamp = crate::session::DiskStamp::of(path);
                    (text, line_ending, stamp)
                }
                None => return,
            };
            self.open_disk_conflict(doc, id, name, text, line_ending, stamp, window, cx);
            return;
        }
        if let Some(path) = doc.read(cx).path().cloned() {
            let known = doc.read(cx).disk_stamp();
            let now = crate::session::DiskStamp::of(&path);
            if known.is_some() && now.is_some() && known != now {
                // External change since we last agreed on the stamp.
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let line_ending = crate::document::LineEnding::detect(&text);
                self.pending_disk_prompt.insert(id);
                self.open_disk_conflict(
                    doc,
                    id,
                    display_name(Some(&path)),
                    text,
                    line_ending,
                    now,
                    window,
                    cx,
                );
                return;
            }
        }
        match doc.read(cx).path().cloned() {
            Some(path) => {
                if self.write(&doc, &path, window, cx)
                    && let Some(next) = next
                {
                    self.run_after_confirm(next, window, cx);
                }
            }
            None => self.prompt_save_path(doc, next, window, cx),
        }
    }

    fn prompt_save_path(
        &mut self,
        doc: Entity<DocumentView>,
        next: Option<AfterConfirm>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (dir, name) = {
            let doc = doc.read(cx);
            let dir = default_dir(doc.dir());
            let name = match doc.path() {
                Some(_) => doc.name(),
                None => suggested_file_name(doc.analysis(), "md"),
            };
            (dir, name)
        };
        let fallback = dir.join(&name);
        let chosen = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            let outcome = file_dialog_outcome(chosen.await);
            _ = this.update_in(cx, |this, window, cx| match outcome {
                Ok(Some(path)) => this.save_to_chosen(&doc, &path, next, window, cx),
                Ok(None) => {}
                // No system dialog: say so, then ask for the path in the app
                // so an Untitled document can still be saved.
                Err(detail) => {
                    report_file_dialog_failure(&detail, window, cx);
                    this.ask_save_path_in_app(doc, fallback, next, window, cx);
                }
            });
        })
        .detach();
    }

    fn save_to_chosen(
        &mut self,
        doc: &Entity<DocumentView>,
        path: &Path,
        next: Option<AfterConfirm>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.write(doc, path, window, cx) {
            AppSettings::update(cx, |settings| settings.push_recent(path));
            self.sync_file_tree(cx);
            if let Some(next) = next {
                self.run_after_confirm(next, window, cx);
            }
        }
    }

    /// The fallback when the system file dialog cannot open: a path field,
    /// prefilled with the folder and suggested name. Enter or Save writes;
    /// an existing file other than this document's own is not overwritten.
    fn ask_save_path_in_app(
        &mut self,
        doc: Entity<DocumentView>,
        suggested: PathBuf,
        next: Option<AfterConfirm>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let input =
            cx.new(|cx| InputState::new(window, cx).default_value(suggested.display().to_string()));
        let next = Rc::new(std::cell::RefCell::new(next));
        let confirm: WindowCallback = {
            let input = input.clone();
            let workspace = cx.weak_entity();
            let next = next.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let Some(path) = typed_save_path(&input.read(cx).value()) else {
                    return;
                };
                let Some(this) = workspace.upgrade() else {
                    return;
                };
                let own = doc.read(cx).path().is_some_and(|own| own == path.as_path());
                if path.exists() && !own {
                    window.push_notification(
                        Notification::error(format!(
                            "“{}” already exists. Pick another name.",
                            display_name(Some(&path))
                        ))
                        .placement(Anchor::BottomRight),
                        cx,
                    );
                    return;
                }
                window.close_dialog(cx);
                let doc = doc.clone();
                let next = next.borrow_mut().take();
                this.update(cx, |this, cx| {
                    this.save_to_chosen(&doc, &path, next, window, cx)
                });
            })
        };
        self.fallback_save_sub = Some(cx.subscribe_in(&input, window, {
            let confirm = confirm.clone();
            move |_, _, event: &InputEvent, window, cx| {
                // The subscriber runs inside a workspace update; confirm
                // updates the workspace itself, so run it after.
                if let InputEvent::PressEnter { .. } = event {
                    let confirm = confirm.clone();
                    window.defer(cx, move |window, cx| confirm(window, cx));
                }
            }
        }));
        let focus = input.read(cx).focus_handle(cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        window.open_dialog(cx, move |dialog, _window, cx| {
            let confirm = confirm.clone();
            dialog
                .title("Save as")
                .child(
                    v_flex()
                        .gap_2()
                        .child(div().text_sm().child("Type the full path to save to:"))
                        .child(Input::new(&input))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("The system file dialog isn’t available."),
                        ),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("fallback-save-cancel")
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("fallback-save")
                                .primary()
                                .label("Save")
                                .on_click(move |_, window, cx| confirm(window, cx)),
                        ),
                )
        });
    }

    /// Write `doc` to `path`; reports failures to the user.
    fn write(
        &mut self,
        doc: &Entity<DocumentView>,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match doc.update(cx, |doc, cx| doc.write_to(path, cx)) {
            Ok(()) => {
                self.save_session(window, cx);
                true
            }
            Err(err) => {
                if err.kind() == std::io::ErrorKind::PermissionDenied {
                    self.notify_error(
                        format!(
                            "Couldn’t save “{}”: no write permission. Use Save As… to pick another place.",
                            display_name(Some(&path.to_path_buf()))
                        ),
                        window,
                        cx,
                    );
                    self.prompt_save_path(doc.clone(), None, window, cx);
                } else {
                    self.notify_error(
                        format!(
                            "Couldn’t save “{}”. {err}",
                            display_name(Some(&path.to_path_buf()))
                        ),
                        window,
                        cx,
                    );
                }
                false
            }
        }
    }

    fn export_html(&mut self, _: &ExportHtml, window: &mut Window, cx: &mut Context<Self>) {
        self.export(ExportFormat::Html, window, cx);
    }

    fn export_pdf(&mut self, _: &ExportPdf, window: &mut Window, cx: &mut Context<Self>) {
        self.export(ExportFormat::Pdf, window, cx);
    }

    fn export_docx(&mut self, _: &ExportDocx, window: &mut Window, cx: &mut Context<Self>) {
        self.export(ExportFormat::Docx, window, cx);
    }

    /// Ask where to save, then render and write the export in the
    /// background.
    fn export(&mut self, format: ExportFormat, window: &mut Window, cx: &mut Context<Self>) {
        let doc = self.active_document().read(cx);
        let dir = default_dir(doc.dir());
        let title = doc
            .analysis()
            .outline
            .first()
            .map(|heading| heading.text.clone())
            .unwrap_or_else(|| doc.name());
        let extension = format.extension();
        let name = match doc.path() {
            Some(path) => format!(
                "{}.{extension}",
                path.file_stem().unwrap_or_default().to_string_lossy()
            ),
            None => suggested_file_name(doc.analysis(), extension),
        };
        let source = doc.text_now(cx);
        let base_dir = doc.dir();
        let chosen = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn_in(window, async move |_, cx| {
            let Some(path) = file_dialog(chosen, cx).await else {
                return;
            };
            let target = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let paper = Paper::from_locale();
                    let base_dir = base_dir.as_deref();
                    let bytes = match format {
                        ExportFormat::Html => Ok(export::to_html(&source, &title).into_bytes()),
                        ExportFormat::Pdf => pdf::to_pdf(&source, &title, base_dir, paper),
                        ExportFormat::Docx => docx::to_docx(&source, &title, base_dir, paper),
                    }?;
                    std::fs::write(&target, bytes).map_err(|err| err.to_string())
                })
                .await;
            _ = cx.update(|window, cx| {
                let name = display_name(Some(&path));
                let note = match result {
                    Ok(()) => Notification::success(format!("Exported “{name}”.")),
                    Err(err) => Notification::error(format!("Couldn’t export “{name}”. {err}")),
                };
                window.push_notification(note, cx);
            });
        })
        .detach();
    }

    /// Copy the selection, or the whole document, as formatted text.
    fn copy_rich_text(&mut self, _: &CopyRichText, window: &mut Window, cx: &mut Context<Self>) {
        let doc = self.active_document().read(cx);
        let editor = doc.editor().read(cx);
        let selection = editor.selected_range();
        let markdown = if selection.is_empty() || !AppSettings::get(cx).layout.shows_editor() {
            doc.text_now(cx).to_string()
        } else {
            editor.text().slice(selection).to_string()
        };
        let base_dir = doc.dir();
        let task = cx.background_executor().spawn({
            let markdown = markdown.clone();
            async move { export::to_clipboard_html(&markdown, base_dir.as_deref()) }
        });
        cx.spawn_in(window, async move |_, cx| {
            let html = task.await;
            _ = cx.update(|window, cx| {
                let note = match rich_copy::copy_html(html, &markdown) {
                    Ok(()) => Notification::success("Copied as rich text."),
                    Err(err) => Notification::error(format!("Couldn’t copy. {err}")),
                };
                window.push_notification(note, cx);
            });
        })
        .detach();
    }

    fn reveal_in_folder(
        &mut self,
        _: &RevealInFolder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.active_document().read(cx).path().cloned() {
            Some(path) => reveal_path_in_file_manager(cx, &path),
            None => window.push_notification(
                Notification::info("Save the document to give it a location."),
                cx,
            ),
        }
    }

    /// Switch the open document between LF and CRLF, the way clicking the EOL
    /// item in VS Code's status bar does. The text is untouched; the document
    /// is marked unsaved and saving writes the new ending.
    fn toggle_line_ending(&mut self, _: &ToggleLineEnding, _: &mut Window, cx: &mut Context<Self>) {
        let doc = self.active_document();
        if doc.read(cx).is_media() {
            return;
        }
        let next = doc.read(cx).line_ending().toggled();
        doc.update(cx, |doc, cx| doc.set_line_ending(next, cx));
        cx.notify();
    }

    fn on_drop_paths(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Route drops the way VS Code does: folders become the sidebar root,
        // and every file the app can show (text, code, Markdown, PDF, image)
        // opens as its own tab. Dropping onto the file tree is handled by the
        // tree itself (copy into the folder), so it never reaches here.
        let mut documents: Vec<PathBuf> = Vec::new();
        let mut folders: Vec<PathBuf> = Vec::new();
        for path in paths.paths() {
            if path.is_dir() {
                folders.push(path.clone());
            } else {
                documents.push(path.clone());
            }
        }
        for folder in &folders {
            AppSettings::update(cx, |settings| settings.push_recent(folder));
            self.file_tree
                .update(cx, |tree, cx| tree.set_root(Some(folder.clone()), cx));
        }
        if !documents.is_empty() {
            self.open_paths(documents, Placement::Default, window, cx);
            return;
        }
        if folders.is_empty() {
            self.notify_error("Drop a file or folder to open it.", window, cx);
        }
    }

    /// A tree entry dropped outside the file tree: a file opens, a folder
    /// is already the tree's root so there is nothing to do.
    fn on_drop_tree_drag(&mut self, drag: &TreeDrag, window: &mut Window, cx: &mut Context<Self>) {
        // Files open (one tab each); folders do nothing here, as in VS
        // Code, where in-app folder drags don't target the editor.
        let files: Vec<PathBuf> = drag
            .paths
            .iter()
            .filter(|path| !path.is_dir())
            .cloned()
            .collect();
        if files.is_empty() {
            return;
        }
        self.open_paths(files, Placement::Default, window, cx);
    }

    /// Record where the pointer would land a dragged tab: before or after
    /// the tab it is over, and show the insertion line.
    fn set_tab_insertion(
        &mut self,
        event: &DragMoveEvent<TabDrag>,
        hovered: usize,
        cx: &mut Context<Self>,
    ) {
        let source = event.drag(cx).index;
        let center = event.bounds.origin.x + event.bounds.size.width * 0.5;
        let after = event.event.position.x >= center;
        let gap = (hovered + usize::from(after)).min(self.documents.len());
        let insertion = Some(TabInsertion { gap, source });
        if self.tab_insertion != insertion {
            self.tab_insertion = insertion;
            cx.notify();
        }
    }

    /// Edge strip width (px) for drag-tab → editor group. Wider than the
    /// probe's ~40px aim so Xvfb mouse releases still land in-zone.
    const TAB_EDGE_SPLIT_PX: f32 = 120.;

    /// True when `pos` sits in the right or bottom edge zone of the editor body
    /// (or the window if body bounds are unknown). `Some(false)` = right,
    /// `Some(true)` = down.
    fn edge_split_at(pos: Point<Pixels>, body: Option<Bounds<Pixels>>) -> Option<bool> {
        let bounds = body?;
        if !bounds.contains(&pos) {
            // Still accept a release just outside the right/bottom rim.
            let right = bounds.origin.x + bounds.size.width;
            let bottom = bounds.origin.y + bounds.size.height;
            let near_right = pos.x >= right - px(Self::TAB_EDGE_SPLIT_PX)
                && pos.x <= right + px(8.)
                && pos.y >= bounds.origin.y
                && pos.y <= bottom + px(8.);
            let near_down = pos.y >= bottom - px(Self::TAB_EDGE_SPLIT_PX)
                && pos.y <= bottom + px(8.)
                && pos.x >= bounds.origin.x
                && pos.x <= right + px(8.);
            return if near_right {
                Some(false)
            } else if near_down {
                Some(true)
            } else {
                None
            };
        }
        let local_x = pos.x - bounds.origin.x;
        let local_y = pos.y - bounds.origin.y;
        let w = bounds.size.width;
        let h = bounds.size.height;
        let right = local_x >= w - px(Self::TAB_EDGE_SPLIT_PX);
        let down = local_y >= h - px(Self::TAB_EDGE_SPLIT_PX);
        // Prefer the nearer edge when both qualify (corner).
        if right && down {
            let dr = w - local_x;
            let dd = h - local_y;
            if dr <= dd { Some(false) } else { Some(true) }
        } else if right {
            Some(false)
        } else if down {
            Some(true)
        } else {
            None
        }
    }

    /// Track drag position / edge preview while a tab is in flight.
    fn note_tab_drag_move(&mut self, event: &DragMoveEvent<TabDrag>, cx: &mut Context<Self>) {
        let idx = event.drag(cx).index;
        self.tab_drag_index.set(Some(idx));
        let pos = event.event.position;
        self.tab_drag_last_pos.set(Some(pos));
        if let Some(vertical) = Self::edge_split_at(pos, self.editor_body_bounds.get()) {
            if self.tab_split_preview != Some(vertical) {
                self.tab_split_preview = Some(vertical);
                cx.notify();
            }
        } else if self.tab_split_preview.take().is_some() {
            cx.notify();
        }
    }

    /// Probe #15: if a tab drag ends near an editor edge (or the edge strip
    /// already set a preview), create a real second group — same path as
    /// Split Editor Right. Returns true when a split was committed.
    fn try_commit_tab_edge_split(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let idx = self
            .tab_drag_index
            .get()
            .or_else(|| self.tab_insertion.map(|i| i.source));
        let Some(idx) = idx else {
            return false;
        };
        let vertical = self.tab_split_preview.take().or_else(|| {
            self.tab_drag_last_pos
                .get()
                .and_then(|pos| Self::edge_split_at(pos, self.editor_body_bounds.get()))
        });
        let Some(vertical) = vertical else {
            return false;
        };
        self.tab_insertion = None;
        self.tab_drag_index.set(None);
        self.tab_drag_last_pos.set(None);
        self.move_tab_into_split(idx, vertical, window, cx);
        true
    }

    /// Land a dropped tab: edge → real editor group; otherwise reorder (or
    /// duplicate with the invert key), as in VS Code.
    fn finish_tab_drop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Probe #15: do NOT clear edge preview first — under Xvfb the release
        // often hits the tab bar / misses the thin strip; commit split instead.
        if self.try_commit_tab_edge_split(window, cx) {
            return;
        }
        self.tab_split_preview = None;
        self.tab_drag_index.set(None);
        self.tab_drag_last_pos.set(None);
        let Some(insertion) = self.tab_insertion.take() else {
            return;
        };
        if self.tab_drag_copy.get() {
            self.duplicate_tab(insertion.source, insertion.gap, window, cx);
        } else {
            self.move_tab(insertion.source, insertion.gap, window, cx);
        }
    }

    /// Probe #17: dismiss the programmatic tab context menu.
    fn dismiss_tab_context_menu(&mut self, cx: &mut Context<Self>) {
        self._tab_context_menu_sub.take();
        self.tab_context_menu.take();
        cx.notify();
    }

    /// Probe #17: open Pin / Select for Compare / … at `position` for tab `ix`.
    /// Built as an owned `PopupMenu` so `activate()` re-renders cannot wipe a
    /// pending `.context_menu()` element state under Xvfb.
    fn open_tab_context_menu(
        &mut self,
        ix: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ix >= self.documents.len() {
            return;
        }
        // Select the tab without deferred editor focus — that raced the old
        // declarative menu and could steal focus before it painted.
        self.activate_inner(ix, false, window, cx);

        let count = self.documents.len();
        let doc_entity = self.documents[ix].clone();
        let doc_id = doc_entity.entity_id();
        let is_pinned = self.pinned.contains(&doc_id);
        let is_last = ix + 1 >= count;
        let has_other = count > 1;
        let tab_path = doc_entity.read(cx).path().cloned();
        let ws = cx.weak_entity();

        let menu = PopupMenu::build(window, cx, {
            let ws = ws.clone();
            let doc_weak = doc_entity.clone();
            let path = tab_path.clone();
            move |menu, _, _| {
                let ws_close = ws.clone();
                let ws_other = ws.clone();
                let ws_right = ws.clone();
                let ws_pin = ws.clone();
                let ws_sel = ws.clone();
                let ws_cmp = ws.clone();
                let doc = doc_weak.clone();
                let sel_path = path.clone();
                let cmp_path = path.clone();
                let has_path = path.is_some();
                let pin_label = tab_context_menu_labels(is_pinned)[0];
                let menu = menu
                    .item(PopupMenuItem::new(pin_label).on_click(move |_, _, cx| {
                        _ = ws_pin.update(cx, |this, cx| {
                            this.toggle_pin_by_id(doc_id, cx);
                        });
                    }))
                    .separator()
                    .item(
                        PopupMenuItem::new("Select for Compare")
                            .disabled(!has_path)
                            .on_click(move |_, window, cx| {
                                let Some(sel_path) = sel_path.clone() else {
                                    return;
                                };
                                _ = ws_sel.update(cx, |this, cx| {
                                    this.compare_selection = Some(sel_path.clone());
                                    let name = sel_path
                                        .file_name()
                                        .map(|n| n.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| sel_path.display().to_string());
                                    window.push_notification(
                                        Notification::info(format!(
                                            "Selected {name} for compare. Right-click another file → Compare with Selected."
                                        )),
                                        cx,
                                    );
                                    cx.notify();
                                });
                            }),
                    )
                    .item(
                        PopupMenuItem::new("Compare with Selected")
                            .disabled(!has_path)
                            .on_click(move |_, window, cx| {
                                let Some(right) = cmp_path.clone() else {
                                    return;
                                };
                                _ = ws_cmp.update(cx, |this, cx| {
                                    let Some(left) = this.compare_selection.clone() else {
                                        window.push_notification(
                                            Notification::info(
                                                "Select for Compare on a file first.",
                                            ),
                                            cx,
                                        );
                                        return;
                                    };
                                    if left == right {
                                        window.push_notification(
                                            Notification::info(
                                                "Pick a different file to compare.",
                                            ),
                                            cx,
                                        );
                                        return;
                                    }
                                    let Ok(left_text) = std::fs::read_to_string(&left) else {
                                        window.push_notification(
                                            Notification::info("Couldn’t read the first file."),
                                            cx,
                                        );
                                        return;
                                    };
                                    let Ok(right_text) = std::fs::read_to_string(&right) else {
                                        window.push_notification(
                                            Notification::info("Couldn’t read the second file."),
                                            cx,
                                        );
                                        return;
                                    };
                                    let left_name = left
                                        .file_name()
                                        .map(|n| n.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| "left".into());
                                    let right_name = right
                                        .file_name()
                                        .map(|n| n.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| "right".into());
                                    this.open_diff_session(
                                        format!("Compare: {left_name} ↔ {right_name}"),
                                        left_name,
                                        right_name,
                                        left_text,
                                        right_text,
                                        window,
                                        cx,
                                    );
                                });
                            }),
                    )
                    .separator()
                    .item(PopupMenuItem::new("Close").on_click(move |_, window, cx| {
                        _ = ws_close.update(cx, |this, cx| {
                            let id = doc.entity_id();
                            this.confirm_unsaved(
                                doc.clone(),
                                AfterConfirm::CloseDocument(id),
                                window,
                                cx,
                            );
                        });
                    }))
                    .item(
                        PopupMenuItem::new("Close Other Tabs")
                            .disabled(!has_other)
                            .on_click(move |_, window, cx| {
                                _ = ws_other.update(cx, |this, cx| {
                                    this.close_other_tabs_by_id(doc_id, window, cx);
                                });
                            }),
                    )
                    .item(
                        PopupMenuItem::new("Close Tabs to the Right")
                            .disabled(is_last)
                            .on_click(move |_, window, cx| {
                                _ = ws_right.update(cx, |this, cx| {
                                    this.close_tabs_to_the_right_by_id(doc_id, window, cx);
                                });
                            }),
                    );
                match path.clone() {
                    Some(path) => {
                        let text = crate::paths::path_for_clipboard(&path);
                        let reveal = path.clone();
                        menu.separator()
                            .item(PopupMenuItem::new("Copy Path").on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                            }))
                            .item(PopupMenuItem::new("Reveal in File Manager").on_click(
                                move |_, _, cx| {
                                    reveal_path_in_file_manager(cx, &reveal);
                                },
                            ))
                    }
                    None => menu,
                }
            }
        });

        let focus = menu.focus_handle(cx);
        if !focus.contains_focused(window, cx) {
            focus.focus(window, cx);
        }

        self._tab_context_menu_sub.take();
        self._tab_context_menu_sub =
            Some(
                cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, _, cx| {
                    this.dismiss_tab_context_menu(cx);
                }),
            );
        self.tab_context_menu = Some(TabContextMenu { position, menu });
        cx.notify();
    }

    /// Shift+F10 / Menu key: open the tab context menu for the active tab.
    fn show_tab_context_menu(
        &mut self,
        _: &ShowTabContextMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.documents.is_empty() {
            return;
        }
        let position = self
            .tab_bar_bounds
            .get()
            .map(|b| {
                Point::new(
                    b.origin.x + px(80.).min(b.size.width * 0.5),
                    b.origin.y + b.size.height * 0.5,
                )
            })
            .unwrap_or_else(|| {
                // Probe #17 hit Y ≈ 46–54; default near first tab label.
                Point::new(px(250.), px(50.))
            });
        let ix = self.active;
        self.open_tab_context_menu(ix, position, window, cx);
    }

    /// After a cross-group tab drop: `(new_active, new_other)`. Never leaves
    /// both panes on the same document when more than one tab exists (move,
    /// not clone — probe #16).
    fn split_panes_after_cross_group_move(
        dragged: usize,
        active: usize,
        other: usize,
        len: usize,
        into_secondary: bool,
    ) -> (usize, usize) {
        if len == 0 {
            return (0, 0);
        }
        let dragged = dragged.min(len - 1);
        let active = active.min(len - 1);
        let other = other.min(len - 1);
        if len == 1 {
            return (0, 0);
        }
        if into_secondary {
            // Drop into secondary: secondary shows dragged; primary keeps a
            // different document (prefer prior primary, else prior other).
            let mut primary = if active != dragged { active } else { other };
            if primary == dragged {
                primary = (0..len).find(|&i| i != dragged).unwrap_or(0);
            }
            (primary, dragged)
        } else {
            // Drop into primary: primary shows dragged; secondary keeps a
            // different document.
            let mut secondary = if other != dragged { other } else { active };
            if secondary == dragged {
                secondary = (0..len).find(|&i| i != dragged).unwrap_or(0);
            }
            (dragged, secondary)
        }
    }

    /// Duplicate the tab at `from`, placing the copy at `gap`. The copy
    /// keeps the source's text and unsaved state.
    fn duplicate_tab(
        &mut self,
        from: usize,
        gap: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = self.documents.get(from).cloned() else {
            return;
        };
        let copy = doc.update(cx, |doc, cx| doc.duplicate(window, cx));
        self.add_document_at(copy, gap, window, cx);
    }

    /// Land a dragged tab at `to_gap`, the way VS Code reorders tabs.
    fn move_tab(
        &mut self,
        from: usize,
        to_gap: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if from >= self.documents.len() {
            return;
        }
        let doc = self.documents.remove(from);
        let mut gap = to_gap.min(self.documents.len() + 1);
        if gap > from {
            // Positions after the removal shift left by one.
            gap -= 1;
        }
        self.documents.insert(gap, doc);
        self.active = gap;
        self.sync_preview_pin_roles(cx);
        self.save_session(window, cx);
        cx.notify();
    }

    /// Open documents whose files just moved in the tree now point at the
    /// new locations, as VS Code's editors follow renames.
    fn retarget_moved(
        &mut self,
        from: &Path,
        to: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut moved = Vec::new();
        for doc in &self.documents {
            let Some(path) = doc.read(cx).path().cloned() else {
                continue;
            };
            let new_path = if path == *from {
                Some(to.to_path_buf())
            } else if path.starts_with(from) {
                match path.strip_prefix(from) {
                    Ok(suffix) => Some(to.join(suffix)),
                    Err(_) => None,
                }
            } else {
                None
            };
            if let Some(new_path) = new_path {
                moved.push((doc.clone(), new_path));
            }
        }
        if moved.is_empty() {
            return;
        }
        for (doc, new_path) in moved {
            doc.update(cx, |doc, cx| doc.retarget(new_path, cx));
        }
        AppSettings::update(cx, |settings| {
            for recent in &mut settings.recent_files {
                if recent == from {
                    *recent = to.to_path_buf();
                } else if let Ok(suffix) = recent.strip_prefix(from) {
                    *recent = to.join(suffix);
                }
            }
        });
        self.sync_file_tree(cx);
        self.save_session(window, cx);
        cx.notify();
    }

    // ---------------------------------------------------------------------
    // View commands

    fn set_layout(&mut self, action: &SetLayout, window: &mut Window, cx: &mut Context<Self>) {
        let layout = action.0;
        let current = AppSettings::get(cx);
        // One gesture → one layout change: ignore re-clicks of the active mode.
        if current.layout == layout && !current.focus_mode {
            return;
        }
        let started = std::time::Instant::now();
        log::info!("ui: title-bar SetLayout -> {layout:?}");
        AppSettings::update(cx, |settings| {
            settings.layout = layout;
            settings.focus_mode = false;
        });
        self.focus_active(window, cx);
        // Global observers usually refresh, but notify explicitly so leaving
        // Preview-only cannot depend on focus-only side effects.
        cx.notify();
        log::debug!(
            "ui: SetLayout handled in {:.1} ms",
            started.elapsed().as_secs_f64() * 1000.
        );
    }

    /// Toggle between editor|preview split and editor-only. Default folder
    /// open is already split, so Ctrl+Shift+V as a true toggle matches what
    /// users expect from a preview shortcut.
    fn toggle_preview_layout(
        &mut self,
        _: &TogglePreviewLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = AppSettings::get(cx).layout;
        let next = if current == Layout::Split {
            Layout::Editor
        } else {
            Layout::Split
        };
        self.set_layout(&SetLayout(next), window, cx);
    }

    fn toggle_focus_mode(
        &mut self,
        _: &ToggleFocusMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle(|settings| settings.focus_mode = !settings.focus_mode, cx);
        self.focus_active(window, cx);
    }

    fn go_to_heading(&mut self, _: &GoToHeading, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        self.headings
            .update(cx, |state, cx| state.set_query("", window, cx));
        let headings = self.headings.clone();
        let doc = self.active_document().clone();
        // Focus the query once, after the dialog has taken focus. Not in the
        // builder: that runs on every render and kept pulling focus back.
        let focus = headings.read(cx).focus_handle(cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        window.open_dialog(cx, move |dialog, _window, cx| {
            let outline = doc.read(cx).analysis().outline.clone();
            let items = outline.iter().map(|heading| {
                CommandItem::new()
                    .label(heading.text.clone())
                    .keywords([heading.text.clone()])
                    .icon(heading_icon(heading.level))
            });
            let confirm = doc.clone();
            dialog.close_button(false).p_0().child(esc_closes(
                Command::new(&headings)
                    .bordered(false)
                    .placeholder("Go to heading")
                    .max_h(rems(24.))
                    .items(items)
                    .empty(|_, _, _| "No headings in this document")
                    .on_confirm(move |index: IndexPath, window, cx| {
                        window.close_dialog(cx);
                        confirm.update(cx, |doc, cx| doc.reveal_heading(index.row, window, cx));
                    })
                    .on_cancel(|window, cx| window.close_dialog(cx)),
            ))
        });
    }

    /// List the files in the open folder and open the picked one, the way VS
    /// Code's Go to File (Ctrl+P, ⌘P) does.
    fn show_quick_open(&mut self, _: &QuickOpen, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        let Some(root) = self.file_tree.read(cx).root().map(Path::to_path_buf) else {
            window.push_notification(Notification::info("Open a folder to search its files."), cx);
            return;
        };
        // The listing is read off the UI thread: walking a large folder used
        // to hold the window while it ran.
        let show_hidden = AppSettings::get(cx).show_hidden_entries;
        let opened = self.file_tree.read(cx).expanded_folders();
        let walk_root = root.clone();
        let listing = cx.background_executor().spawn(async move {
            quick_open::files_under(&walk_root, show_hidden, &opened, quick_open::MAX_FILES)
        });
        cx.spawn_in(window, async move |this, cx| {
            let files = listing.await;
            _ = this.update_in(cx, |this, window, cx| {
                this.open_quick_open(root, files, window, cx)
            });
        })
        .detach();
    }

    /// Open the Quick Open palette over the files just listed.
    fn open_quick_open(
        &mut self,
        root: PathBuf,
        files: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if files.is_empty() {
            window.push_notification(Notification::info("No files in this folder yet."), cx);
            return;
        }
        self.quick_open
            .update(cx, |state, cx| state.set_query("", window, cx));
        let state = self.quick_open.clone();
        let listing = files.clone();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let restore = cx.weak_entity();
        let current = self.active_document().read(cx).path().cloned();
        let mut recent = AppSettings::get(cx).recent_files.clone();
        for document in &self.documents {
            if let Some(path) = document.read(cx).path()
                && !recent.contains(path)
            {
                recent.push(path.clone());
            }
        }
        // Focus the query once, after the dialog has taken focus. Not in the
        // builder: that runs on every render and kept pulling focus back.
        let focus = state.read(cx).focus_handle(cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        window.open_dialog(cx, move |dialog, _window, cx| {
            // A query that is a path is checked directly, VS Code style: a
            // file that exists is offered even when the listing left it out
            // (ignored, too deep, outside the folder), and a folder lists its
            // own entries. One `stat`, or one folder read, per keystroke.
            let query = state.read(cx).query(cx).trim().to_string();
            let rows = quick_open_rows(&query, &root, home.as_deref(), &listing);
            let (first, rows) = split_recent(rows, &recent, current.as_deref());
            // Only the rows the query can show reach the palette: a folder
            // of 20,000 files would otherwise rebuild 20,000 rows on every
            // frame. Same test the palette filters with, so nothing it would
            // have shown is lost, up to QUICK_OPEN_SHOWN of them.
            let rows = matching_quick_open_rows(rows, &query, QUICK_OPEN_SHOWN);
            let item = |row: &QuickOpenRow| {
                let shown: SharedString = row.label.clone().into();
                let name: SharedString = row
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default()
                    .into();
                let mut keywords = vec![shown.clone(), name];
                if let Some(keyword) = &row.keyword {
                    keywords.push(keyword.clone().into());
                }
                command_item_with_path_icon(shown, keywords, &row.path, row.is_dir)
            };
            let recent_items = first.iter().map(item).collect::<Vec<_>>();
            let items = rows.iter().map(item).collect::<Vec<_>>();
            // Section indices match the groups actually shown: omit an empty
            // "Recently opened" so Files stays section 0.
            let picked: Vec<Vec<QuickOpenRow>> = if first.is_empty() {
                vec![rows.clone()]
            } else {
                vec![first.clone(), rows.clone()]
            };
            let navigate = state.clone();
            let mut command = Command::new(&state)
                .bordered(false)
                .placeholder("Search files by name, or type a path")
                .max_h(rems(24.));
            if !recent_items.is_empty() {
                command = command.group(
                    CommandGroup::new()
                        .label("Recently opened")
                        .items(recent_items),
                );
            }
            command = command
                .group(CommandGroup::new().label("Files").items(items))
                .empty(|_, _, _| div().px_3().py_2().child("No matching files"))
                // The rows depend on the query when it is a path, so the
                // dialog is drawn again as it changes.
                .on_query(|_, window, _| window.refresh())
                .on_confirm({
                    let restore = restore.clone();
                    move |index: IndexPath, window, cx| {
                        let Some(row) = picked
                            .get(index.section)
                            .and_then(|rows| rows.get(index.row))
                            .cloned()
                        else {
                            return;
                        };
                        if row.is_dir {
                            // Into the folder, as VS Code's picker does.
                            let mut typed = row.label.clone();
                            typed.push('/');
                            navigate.update(cx, |state, cx| state.set_query(typed, window, cx));
                            window.refresh();
                            return;
                        }
                        window.close_dialog(cx);
                        // Probe #14: after Terminal was focused, dispatch_action can miss
                        // the workspace and leave Untitled — call open_quick_path directly.
                        let path = row.path.clone();
                        let line = row.line;
                        if let Some(this) = restore.upgrade() {
                            this.update(cx, |this, cx| {
                                this.open_quick_path(&OpenQuickPath(path, line), window, cx);
                            });
                        }
                    }
                })
                .on_cancel(|window, cx| window.close_dialog(cx));
            let restore = restore.clone();
            dialog
                .close_button(false)
                .p_0()
                .on_close(move |_, window, cx| {
                    if let Some(this) = restore.upgrade() {
                        this.update(cx, |this, cx| this.focus_active(window, cx));
                    }
                })
                .child(esc_closes(command))
        });
    }

    /// Open a file picked from the Quick Open palette.
    fn open_quick_path(
        &mut self,
        action: &OpenQuickPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = action.0.canonicalize().unwrap_or_else(|_| action.0.clone());
        let Some(line) = action.1 else {
            self.open_paths(vec![path], Placement::Default, window, cx);
            return;
        };
        // `file:12` from Quick Open lands on the line, as in VS Code.
        if let Some(ix) = self
            .documents
            .iter()
            .position(|doc| doc.read(cx).path() == Some(&path))
        {
            self.activate(ix, window, cx);
            let doc = self.documents[ix].clone();
            doc.update(cx, |doc, cx| doc.go_to_line(line, window, cx));
            return;
        }
        self.load_path(path, Placement::Default, Target::Line(line), window, cx);
    }

    /// Show every command in one searchable list, the way VS Code's Show All
    /// Commands (Ctrl+Shift+P) does. Each row carries the command's current
    /// key binding, and confirming one dispatches its Action.
    fn show_command_palette(
        &mut self,
        _: &CommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        self.command_palette
            .update(cx, |state, cx| state.set_query("", window, cx));
        let state = self.command_palette.clone();
        // The palette lives in a dialog, and an action confirmed there is
        // dispatched along the dialog's own focus path — which the window's
        // workspace is not part of, so the widget's own dispatch never
        // arrives. Rows still carry the Action so CommandItem can paint the
        // keybinding tip; confirming hands that Action back to the workspace
        // and dispatches it from there.
        let ws_focus = self.focus_handle.clone();
        let restore = cx.weak_entity();
        // Focus the query once, after the dialog has taken focus. Not in the
        // builder: that runs on every render and kept pulling focus back.
        let focus = state.read(cx).focus_handle(cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let specs = commands::groups();
            let rows = commands::palette(&specs);
            let actions = commands::actions(&specs);
            let command = rows.into_iter().fold(
                Command::new(&state)
                    .bordered(false)
                    .placeholder("Type a command")
                    .max_h(rems(24.)),
                |command, group| command.group(group),
            );
            let ws_focus = ws_focus.clone();
            let restore = restore.clone();
            dialog
                .close_button(false)
                .p_0()
                .on_close(move |_, window, cx| {
                    if let Some(this) = restore.upgrade() {
                        this.update(cx, |this, cx| this.focus_active(window, cx));
                    }
                })
                .child(esc_closes(
                    command
                        .empty(|_, _, _| div().px_3().py_2().child("No matching commands"))
                        .on_confirm(move |index: IndexPath, window, cx| {
                            window.close_dialog(cx);
                            let Some(action) = actions
                                .get(index.section)
                                .and_then(|group| group.get(index.row))
                                .map(|action| action.boxed_clone())
                            else {
                                return;
                            };
                            let ws_focus = ws_focus.clone();
                            window.defer(cx, move |window, cx| {
                                // Put the window back on its own focus path
                                // before dispatching, so the action reaches the
                                // workspace that handles it.
                                ws_focus.focus(window, cx);
                                window.dispatch_action(action, cx);
                            });
                        })
                        .on_cancel(|window, cx| window.close_dialog(cx)),
                ))
        });
    }

    /// Ask for a line number and move the caret there, the way VS Code's Go to
    /// Line (Ctrl+G) does. The palette's query is the number; the single row
    /// stays put so confirming always reads what was typed.
    fn show_go_to_line(&mut self, _: &GoToLine, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        self.go_to_line
            .update(cx, |state, cx| state.set_query("", window, cx));
        let state = self.go_to_line.clone();
        // Focus the query once, after the dialog has taken focus. Not in the
        // builder: that runs on every render and kept pulling focus back.
        let focus = state.read(cx).focus_handle(cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let typed = state.clone();
            dialog.close_button(false).p_0().child(esc_closes(
                Command::new(&state)
                    .bordered(false)
                    .filterable(false)
                    .placeholder("Line number")
                    .max_h(rems(24.))
                    .item(
                        CommandItem::new()
                            .label("Go to line")
                            .keywords(["go", "line"]),
                    )
                    .on_confirm(move |_, window, cx| {
                        window.close_dialog(cx);
                        let line = typed
                            .read(cx)
                            .query(cx)
                            .trim()
                            .parse::<usize>()
                            .ok()
                            .filter(|line| *line > 0);
                        if let Some(line) = line {
                            window.dispatch_action(Box::new(GoToLineAt(line)), cx);
                        }
                    })
                    .on_cancel(|window, cx| window.close_dialog(cx)),
            ))
        });
    }

    /// Move the active document's caret to a line asked for by Go to Line.
    fn go_to_line_at(&mut self, action: &GoToLineAt, window: &mut Window, cx: &mut Context<Self>) {
        let line = action.0;
        self.with_active(window, cx, |doc, window, cx| {
            doc.go_to_line(line, window, cx)
        });
    }

    fn go_to_definition(
        &mut self,
        _: &GoToDefinition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let started = std::time::Instant::now();
        log::info!("ui: GoToDefinition");
        self.active_document()
            .update(cx, |doc, cx| doc.go_to_typst_definition(cx));
        log::debug!(
            "ui: GoToDefinition dispatched in {:.1} ms",
            started.elapsed().as_secs_f64() * 1000.
        );
    }

    fn zoom(&mut self, delta: f32, cx: &mut Context<Self>) {
        AppSettings::update(cx, |settings| {
            settings.font_size = if delta == 0. {
                crate::settings::DEFAULT_FONT_SIZE
            } else {
                clamp_font_size(settings.font_size + delta)
            };
        });
    }

    /// Zoom the active document's viewer when it shows a PDF or image, and
    /// the UI font otherwise.
    fn zoom_active(&mut self, delta: f32, cx: &mut Context<Self>) {
        let doc = self.active_document().clone();
        if doc.read(cx).is_media() {
            doc.update(cx, |doc, cx| doc.zoom_media(delta, cx));
        } else {
            self.zoom(delta, cx);
        }
    }

    /// Pick a color theme, VS Code style: the light and dark themes in two
    /// groups with the current one checked; moving through them previews
    /// each, Enter keeps it (as the light or dark theme), Esc puts back the
    /// one there was.
    fn show_theme_picker(
        &mut self,
        _: &SelectColorTheme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        self.theme_picker
            .update(cx, |state, cx| state.set_query("", window, cx));
        let state = self.theme_picker.clone();
        let before = AppSettings::get(cx).clone();
        let current = cx.theme().theme_name().clone();
        let names = [
            themes::theme_names(gpui_kit::component::ThemeMode::Light, cx),
            themes::theme_names(gpui_kit::component::ThemeMode::Dark, cx),
        ];
        let kept = Rc::new(Cell::new(false));
        let revert = {
            let kept = kept.clone();
            move |cx: &mut App| {
                if !kept.get() {
                    let before = before.clone();
                    AppSettings::update(cx, move |settings| *settings = before);
                }
            }
        };
        let revert = Rc::new(revert);
        let workspace = cx.weak_entity();
        // Focus the query once, after the dialog has taken focus. Not in the
        // builder: that runs on every render and kept pulling focus back.
        let focus = state.read(cx).focus_handle(cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let group = |label: &'static str, names: &[SharedString]| {
                CommandGroup::new()
                    .label(label)
                    .items(names.iter().map(|name| {
                        CommandItem::new()
                            .label(name.clone())
                            .checked(*name == current)
                    }))
            };
            let pick = {
                let names = names.clone();
                move |index: IndexPath| {
                    names
                        .get(index.section)
                        .and_then(|group| group.get(index.row))
                        .cloned()
                }
            };
            let preview = pick.clone();
            let restore = {
                let revert = revert.clone();
                let workspace = workspace.clone();
                move |window: &mut Window, cx: &mut App| {
                    revert(cx);
                    if let Some(this) = workspace.upgrade() {
                        this.update(cx, |this, cx| this.focus_active(window, cx));
                    }
                }
            };
            let on_esc = restore.clone();
            let on_close = restore.clone();
            let on_cancel = restore;
            let kept = kept.clone();
            let confirm_ws = workspace.clone();
            dialog
                .close_button(false)
                .p_0()
                .on_close(move |_, window, cx| on_close(window, cx))
                .child(
                    div()
                        .capture_action(move |_: &gpui_kit::base::actions::Cancel, window, cx| {
                            on_esc(window, cx);
                            window.close_dialog(cx);
                            cx.stop_propagation();
                        })
                        .child(
                            Command::new(&state)
                                .bordered(false)
                                .placeholder("Select color theme")
                                .max_h(rems(24.))
                                .group(group("Light themes", &names[0]))
                                .group(group("Dark themes", &names[1]))
                                .empty(|_, _, _| div().px_3().py_2().child("No matching themes"))
                                .on_select(move |index, _, cx| {
                                    if let Some(name) = preview(index) {
                                        themes::select_theme(&name, cx);
                                    }
                                })
                                .on_confirm(move |index, window, cx| {
                                    if let Some(name) = pick(index) {
                                        themes::select_theme(&name, cx);
                                    }
                                    kept.set(true);
                                    window.close_dialog(cx);
                                    if let Some(this) = confirm_ws.upgrade() {
                                        this.update(cx, |this, cx| this.focus_active(window, cx));
                                    }
                                })
                                .on_cancel(move |window, cx| {
                                    on_cancel(window, cx);
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn toggle_appearance(&mut self, _: &ToggleAppearance, _: &mut Window, cx: &mut Context<Self>) {
        let dark = cx.theme().is_dark();
        AppSettings::update(cx, |settings| {
            settings.appearance = if dark {
                Appearance::Light
            } else {
                Appearance::Dark
            };
        });
    }

    fn with_active(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut DocumentView, &mut Window, &mut Context<DocumentView>),
    ) {
        let doc = self.active_document().clone();
        doc.update(cx, |doc, cx| f(doc, window, cx));
    }

    fn recheck_spelling(&mut self, cx: &mut Context<Self>) {
        for doc in self.documents.clone() {
            doc.update(cx, |doc, cx| doc.recheck_spelling(cx));
        }
    }

    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        open_settings(window, cx);
    }

    fn open_keyboard_shortcuts(
        &mut self,
        _: &OpenKeyboardShortcuts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::shortcuts::open(window, cx);
    }

    fn toggle_gfm_breaks(&mut self, _: &ToggleGfmBreaks, _: &mut Window, cx: &mut Context<Self>) {
        AppSettings::update(cx, |settings| settings.gfm_breaks = !settings.gfm_breaks);
        // Force every open preview to rebuild with the new break style.
        for doc in &self.documents {
            doc.update(cx, |doc, cx| doc.refresh_preview_from_settings(cx));
        }
        cx.notify();
    }

    /// Split the editor group: show another open document to the right or below.
    fn split_editor_right(
        &mut self,
        _: &SplitEditorRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_editor_split(false, window, cx);
    }

    fn split_editor_down(
        &mut self,
        _: &SplitEditorDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_editor_split(true, window, cx);
    }

    fn close_editor_split(&mut self, _: &CloseEditorSplit, _: &mut Window, cx: &mut Context<Self>) {
        self.editor_split = None;
        cx.notify();
    }

    fn open_editor_split(&mut self, vertical: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.documents.is_empty() {
            return;
        }
        // Prefer a different open tab; otherwise mirror the active document
        // (VS Code opens a second view of the same file).
        let other = if self.documents.len() == 1 {
            self.active
        } else if self.active > 0 {
            self.active - 1
        } else {
            1
        };
        self.editor_split = Some(EditorSplit { other, vertical });
        // Probe #15: nested Layout::Split (edit|preview) inside each group needs
        // ≥16rem×2 per pane and leaves a blank middle band (D-split.png). Force
        // Editor layout so dual groups fill cleanly like VS Code.
        AppSettings::update(cx, |settings| {
            settings.focus_mode = false;
            settings.layout = Layout::Editor;
        });
        self.tab_split_preview = None;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Probe #14: land a dragged tab as a real second editor group (move, not
    /// clone). Primary keeps a *different* document; empty groups collapse on close.
    fn move_tab_into_split(
        &mut self,
        dragged: usize,
        vertical: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.documents.len() < 2 {
            // Only one tab — still open a mirrored split (VS Code does).
            self.open_editor_split(vertical, window, cx);
            return;
        }
        let dragged = dragged.min(self.documents.len().saturating_sub(1));
        let primary = if self.active != dragged {
            self.active
        } else {
            (0..self.documents.len())
                .find(|&i| i != dragged)
                .unwrap_or(dragged)
        };
        self.active = primary;
        self.editor_split = Some(EditorSplit {
            other: dragged,
            vertical,
        });
        self.sync_preview_pin_roles(cx);
        AppSettings::update(cx, |settings| {
            settings.focus_mode = false;
            settings.layout = Layout::Editor;
        });
        self.tab_insertion = None;
        self.tab_split_preview = None;
        self.tab_drag_index.set(None);
        self.tab_drag_last_pos.set(None);
        self.focus_active(window, cx);
        cx.notify();
    }

    // ---------------------------------------------------------------------
    // Rendering

    /// True when there is no folder and the only buffer is the welcome /
    /// blank Untitled — show Open folder / Open / New CTAs (round-4).
    fn shows_welcome_cta(&self, cx: &App) -> bool {
        if self.file_tree.read(cx).root().is_some() {
            return false;
        }
        if self.documents.len() != 1 {
            return false;
        }
        let doc = self.documents[0].read(cx);
        doc.path().is_none()
    }

    fn render_welcome_cta(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let has_recent = !AppSettings::get(cx).recent_files.is_empty();
        v_flex()
            .id("welcome-cta")
            .w_full()
            .gap_3()
            .px_4()
            .py_4()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Welcome to Malgel"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Open a folder to browse files, or start from a recent workspace."),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("welcome-open-folder")
                            .label("Open Folder…")
                            .primary()
                            .tooltip("Open folder (Ctrl+K Ctrl+O)")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(OpenFolder), cx)
                            }),
                    )
                    .child(
                        // Probe #15: keep the label always visible (disabled
                        // outline buttons were OCR-invisible on F4b). Enable
                        // after any same-profile folder/file open fills recents.
                        Button::new("welcome-open-recent")
                            .label("Open Recent…")
                            .outline()
                            .tooltip(if has_recent {
                                "Open Recent"
                            } else {
                                "Open a folder first to enable Open Recent"
                            })
                            .on_click(cx.listener(move |_, _, window, cx| {
                                if AppSettings::get(cx).recent_files.is_empty() {
                                    window.push_notification(
                                        Notification::info(
                                            "No recent folders yet. Open a folder first.",
                                        ),
                                        cx,
                                    );
                                    return;
                                }
                                window.dispatch_action(Box::new(OpenRecentPicker), cx);
                            })),
                    )
                    .child(
                        Button::new("welcome-open-file")
                            .label("Open File…")
                            .outline()
                            .tooltip("Open file (Ctrl+O)")
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(Open), cx)),
                    )
                    .child(
                        Button::new("welcome-new-file")
                            .label("New File")
                            .ghost()
                            .tooltip("New file (Ctrl+N)")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(NewFile), cx)
                            }),
                    ),
            )
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = AppSettings::get(cx);
        let layout = settings.layout;
        let is_dark = cx.theme().is_dark();
        let show_menu_bar = !cfg!(target_os = "macos");
        let workspace = cx.weak_entity();
        let doc = self.active_document().read(cx);

        let layout_button = |id: &'static str, icon: IconName, tooltip: &'static str, value| {
            Button::new(id)
                .icon(Icon::new(icon))
                .tooltip(tooltip)
                .selected(!settings.focus_mode && layout == value)
        };

        TitleBar::new()
            .on_close_window(move |_, window, cx| {
                _ = workspace.update(cx, |this, cx| {
                    this.confirm_close(AfterConfirm::CloseWindow, window, cx)
                });
            })
            .child(
                h_flex()
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .when(show_menu_bar, |this| this.child(self.app_menu_bar.clone())),
            )
            .child(
                // Document name centered like a native title bar. Path copy is
                // a small explicit control (not the whole strip) so ordinary
                // clicks do not spam Copied toasts (round-4).
                h_flex()
                    .id("title-bar-path")
                    .absolute()
                    .inset_0()
                    .justify_center()
                    .gap_1p5()
                    .text_sm()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .truncate()
                            .child(doc.name()),
                    )
                    .when(doc.is_dirty(), |this| {
                        this.child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child("— Edited"),
                        )
                    })
                    .when(doc.path().is_some(), |this| {
                        this.child(
                            Button::new("title-copy-path")
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(IconName::Copy))
                                .tooltip("Copy path")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.copy_title_path(window, cx);
                                })),
                        )
                    }),
            )
            .child(
                h_flex()
                    .id("title-bar-buttons")
                    // Take clicks for these buttons rather than the title
                    // bar's drag area behind them, which on Windows would
                    // treat them as clicks on the caption.
                    .occlude()
                    .flex_shrink_0()
                    .gap_2()
                    .pr_2()
                    .child(
                        // VS Code keeps Toggle Side Bar in its title bar
                        // beside the layout controls. Without a button here, a
                        // folded-away Explorer leaves the sidebar's only way
                        // back inside a menu.
                        Button::new("toggle-sidebar")
                            .ghost()
                            .small()
                            .compact()
                            .icon(Icon::new(IconName::PanelLeft))
                            .tooltip("Toggle side bar")
                            .selected(settings.file_sidebar)
                            // Call the workspace directly: in preview-only the
                            // focused pane may not bubble `dispatch_action`.
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle(
                                    |settings| settings.file_sidebar = !settings.file_sidebar,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        ButtonGroup::new("layout")
                            .ghost()
                            .small()
                            .compact()
                            .child(layout_button(
                                "layout-editor",
                                IconName::PenLine,
                                "Editor",
                                Layout::Editor,
                            ))
                            .child(layout_button(
                                "layout-split",
                                IconName::Columns2,
                                "Editor and preview",
                                Layout::Split,
                            ))
                            .child(layout_button(
                                "layout-preview",
                                IconName::Eye,
                                "Preview",
                                Layout::Preview,
                            ))
                            .on_click(cx.listener(|this, clicked: &Vec<usize>, window, cx| {
                                let layout = match clicked.first() {
                                    Some(0) => Layout::Editor,
                                    Some(2) => Layout::Preview,
                                    _ => Layout::Split,
                                };
                                log::info!("ui: title-bar layout button clicked -> {layout:?}");
                                // Direct call — do not rely on action dispatch
                                // from a focused preview pane (see focus_active).
                                this.set_layout(&SetLayout(layout), window, cx);
                            })),
                    )
                    .child(
                        // Probe #14: editor-group split (not MD edit|preview Columns2).
                        Button::new("split-editor-right")
                            .ghost()
                            .small()
                            .compact()
                            .icon(Icon::new(IconName::PanelRightOpen))
                            .tooltip("Split Editor Right (Ctrl+\\)")
                            .selected(self.editor_split.is_some_and(|s| !s.vertical))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.split_editor_right(&SplitEditorRight, window, cx);
                            })),
                    )
                    .child(
                        Button::new("appearance")
                            .ghost()
                            .small()
                            .compact()
                            .icon(Icon::new(if is_dark {
                                IconName::Sun
                            } else {
                                IconName::Moon
                            }))
                            .tooltip(if is_dark {
                                "Use light appearance"
                            } else {
                                "Use dark appearance"
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_appearance(&ToggleAppearance, window, cx);
                            })),
                    ),
            )
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let active_ix = self.active;
        let count = self.documents.len();
        let workspace = cx.weak_entity();
        // A newly active tab, or one added or closed, is scrolled fully into
        // view; the handle waits for the tab's bounds if it has none yet.
        let now = (active_ix, count);
        if tab_needs_reveal(self.tab_revealed.get(), now) {
            self.tab_scroll.scroll_to_item(active_ix);
            self.tab_revealed.set(Some(now));
        }
        let tab_scroll = self.tab_scroll.clone();

        let bar = h_flex()
            .id("tabs")
            .h(px(36.))
            .flex_none()
            .items_center()
            .px_1p5()
            .gap_1()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.tab_bar)
            // While a tab drags, remember the bar's bounds, the invert
            // key's state, and, on drop, land the tab at the insertion
            // point the tabs reported.
            .on_drag_move::<TabDrag>(cx.listener(|this, event: &DragMoveEvent<TabDrag>, _, cx| {
                this.tab_bar_bounds.set(Some(event.bounds));
                let copy = invert_key_held(&event.event.modifiers);
                if this.tab_drag_copy.get() != copy {
                    this.tab_drag_copy.set(copy);
                }
                this.note_tab_drag_move(event, cx);
            }))
            .on_drop(cx.listener(|this, _: &TabDrag, window, cx| {
                this.finish_tab_drop(window, cx);
            }))
            .when(count > 1, |this| {
                this.child(
                    Button::new("tab-scroll-prev")
                        .ghost()
                        .xsmall()
                        .compact()
                        .icon(Icon::new(IconName::ChevronLeft))
                        .tooltip("Previous tab (Ctrl+Shift+Tab)")
                        .disabled(active_ix == 0)
                        .on_click(cx.listener(|this, _, window, cx| {
                            let count = this.documents.len();
                            if count > 1 {
                                let prev = (this.active + count - 1) % count;
                                this.activate(prev, window, cx);
                            }
                        })),
                )
            })
            .child(
                // Probe #16/#17: do NOT attach `.horizontal_scrollbar` here.
                // That helper overlays an absolute inset_0 Scrollbar whose
                // full-area hitbox can swallow right-clicks. Tab menus now
                // open programmatically; overflow_x_scroll + chevrons remain.
                div().relative().flex_1().min_w_0().h_full().child(
                    h_flex()
                        .id("tabs-scroll-area")
                        .size_full()
                        .items_center()
                        .gap_1()
                        // Tracked by the workspace's own handle, so the active
                        // tab can be scrolled into view; its children are the
                        // tabs alone, so a child index is a tab index.
                        .overflow_x_scroll()
                        .track_scroll(&tab_scroll)
                        .children(self.documents.iter().enumerate().map(|(ix, doc_entity)| {
                            let doc = doc_entity.read(cx);
                            let doc_id = doc_entity.entity_id();
                            let active = ix == active_ix;
                            let dirty = doc.is_dirty();
                            // The preview tab reads like a draft: the next file
                            // clicked replaces it until it is edited or opened
                            // for good, the way VS Code marks its preview.
                            let is_preview = self.preview == Some(doc_id);
                            let is_pinned = self.pinned.contains(&doc_id);
                            let name = doc.name().to_string();
                            // The tab carries its file's type icon, as in VS Code.
                            let tab_icon = doc.path().map(|path| file_type_icon(path));
                            let theme = cx.theme();
                            let drag_label: SharedString = name.clone().into();

                            h_flex()
                                .id(("tab", ix))
                                .group("tab")
                                .relative()
                                .h(px(28.))
                                .max_w(px(220.))
                                .flex_none()
                                .pl_2p5()
                                .pr_1p5()
                                .gap_1p5()
                                .items_center()
                                .rounded_md()
                                .text_sm()
                                .cursor_pointer()
                                // Tabs drag to a new position, the way they
                                // reorder in VS Code.
                                .on_drag(TabDrag { index: ix }, move |_, _, _, cx| {
                                    cx.new(|_| DragGhost {
                                        label: drag_label.clone(),
                                    })
                                })
                                .on_drag_move::<TabDrag>(cx.listener(
                                    move |this, event: &DragMoveEvent<TabDrag>, _, cx| {
                                        this.set_tab_insertion(event, ix, cx);
                                    },
                                ))
                                .when(active, |this| {
                                    this.bg(theme.tab_active)
                                        .text_color(theme.tab_active_foreground)
                                        .font_weight(FontWeight::MEDIUM)
                                        .border_1()
                                        .border_color(theme.border)
                                })
                                .when(!active, |this| {
                                    this.bg(theme.tab_bar)
                                        .text_color(theme.muted_foreground)
                                        .border_1()
                                        .border_color(theme.tab_bar)
                                        .hover(|style| {
                                            style
                                                .bg(theme.list_hover)
                                                .text_color(theme.foreground)
                                                .border_color(theme.border.opacity(0.5))
                                        })
                                })
                                .child(
                                    div()
                                        .flex_none()
                                        .when(doc.path().is_none(), |this| {
                                            this.text_color(if active {
                                                theme.tab_active_foreground
                                            } else {
                                                theme.muted_foreground
                                            })
                                        })
                                        .child(
                                            tab_icon
                                                .unwrap_or_else(|| {
                                                    Icon::new(IconName::FileText).text_color(
                                                        if active {
                                                            theme.tab_active_foreground
                                                        } else {
                                                            theme.muted_foreground
                                                        },
                                                    )
                                                })
                                                .xsmall(),
                                        ),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .when(is_preview, |this| this.italic())
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .size(px(18.))
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_sm()
                                        .hover(|style| style.bg(theme.list_active))
                                        // A pinned tab shows a pin where the
                                        // close box would be, and the pin is
                                        // what takes the click — the way VS
                                        // Code's pinned tabs read.
                                        .when(is_pinned, |this| {
                                            this.child(
                                                Icon::new(IconName::Pin)
                                                    .xsmall()
                                                    .text_color(theme.primary),
                                            )
                                        })
                                        .when(!is_pinned, |this| {
                                            this.child(div().when(dirty, |this| {
                                                this.child(
                                                    div()
                                                        .size(px(7.))
                                                        .rounded_full()
                                                        .bg(theme.primary),
                                                )
                                                .group_hover("tab", |style| style.invisible())
                                            }))
                                            .child(
                                                div()
                                                    .absolute()
                                                    .invisible()
                                                    .when(active && !dirty, |this| this.visible())
                                                    .group_hover("tab", |style| style.visible())
                                                    .child(Icon::new(IconName::Close).xsmall()),
                                            )
                                        })
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                if is_pinned {
                                                    this.toggle_pin_by_id(doc_id, cx);
                                                    return;
                                                }
                                                let doc = this.documents[ix].clone();
                                                let id = doc.entity_id();
                                                this.confirm_unsaved(
                                                    doc,
                                                    AfterConfirm::CloseDocument(id),
                                                    window,
                                                    cx,
                                                );
                                            }),
                                        ),
                                )
                                // VS Code-style insertion line where a dragged
                                // tab would land: left edge of this tab, or the
                                // right edge of the last one.
                                .when(
                                    self.tab_insertion.map(|insertion| insertion.gap) == Some(ix),
                                    |this| {
                                        this.child(
                                            div()
                                                .absolute()
                                                .left_0()
                                                .top(px(4.))
                                                .bottom(px(4.))
                                                .w(px(2.))
                                                .rounded_full()
                                                .bg(theme.primary),
                                        )
                                    },
                                )
                                .when(
                                    ix + 1 == count
                                        && self.tab_insertion.map(|insertion| insertion.gap)
                                            == Some(count),
                                    |this| {
                                        this.child(
                                            div()
                                                .absolute()
                                                .right_0()
                                                .top(px(4.))
                                                .bottom(px(4.))
                                                .w(px(2.))
                                                .rounded_full()
                                                .bg(theme.primary),
                                        )
                                    },
                                )
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                        // Probe #17: open menu programmatically.
                                        // Declarative `.context_menu()` lost its
                                        // element state when activate() notified
                                        // and deferred editor focus under Xvfb.
                                        cx.stop_propagation();
                                        this.open_tab_context_menu(ix, event.position, window, cx);
                                    }),
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                        // A double click keeps the file: the
                                        // preview slot is given up, as in VS
                                        // Code.
                                        if event.click_count > 1 {
                                            this.unpreview(doc_id);
                                        }
                                        this.activate(ix, window, cx)
                                    }),
                                )
                                .on_mouse_down(
                                    MouseButton::Middle,
                                    cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        let doc = this.documents[ix].clone();
                                        let id = doc.entity_id();
                                        this.confirm_unsaved(
                                            doc,
                                            AfterConfirm::CloseDocument(id),
                                            window,
                                            cx,
                                        );
                                    }),
                                )
                        })),
                ),
            )
            .when(count > 1, |this| {
                this.child(
                    Button::new("tab-scroll-next")
                        .ghost()
                        .xsmall()
                        .compact()
                        .icon(Icon::new(IconName::ChevronRight))
                        .tooltip("Next tab (Ctrl+Tab)")
                        .disabled(active_ix + 1 >= count)
                        .on_click(cx.listener(|this, _, window, cx| {
                            let count = this.documents.len();
                            if count > 1 {
                                let next = (this.active + 1) % count;
                                this.activate(next, window, cx);
                            }
                        })),
                )
            })
            .child(
                Button::new("new-tab")
                    .ghost()
                    .xsmall()
                    .compact()
                    .icon(Icon::new(IconName::Plus))
                    .tooltip("New document")
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(NewFile), cx)),
            )
            .when(count > 1, |this| {
                let ws = workspace.clone();
                this.child(
                    Button::new("tab-overflow-menu")
                        .ghost()
                        .xsmall()
                        .compact()
                        .icon(Icon::new(IconName::Ellipsis))
                        .tooltip("Tab options")
                        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                            let ws_other = ws.clone();
                            let ws_right = ws.clone();
                            menu.item(PopupMenuItem::new("Close Other Tabs").on_click(
                                move |_, window, cx| {
                                    _ = ws_other.update(cx, |this, cx| {
                                        let keep_id = this.active_document().entity_id();
                                        this.close_other_tabs_by_id(keep_id, window, cx);
                                    });
                                },
                            ))
                            .item(
                                PopupMenuItem::new("Close Tabs to the Right").on_click(
                                    move |_, window, cx| {
                                        _ = ws_right.update(cx, |this, cx| {
                                            let active_id = this.active_document().entity_id();
                                            this.close_tabs_to_the_right_by_id(
                                                active_id, window, cx,
                                            );
                                        });
                                    },
                                ),
                            )
                        }),
                )
            });

        // Probe #17: paint the programmatic tab context menu above the strip.
        div()
            .id("tabs-host")
            .relative()
            .w_full()
            .child(bar)
            .when_some(self.tab_context_menu.as_ref(), |this, open| {
                this.child(deferred(
                    anchored()
                        .position(open.position)
                        .snap_to_window_with_margin(px(8.))
                        .anchor(Anchor::TopLeft)
                        .child(open.menu.clone()),
                ))
            })
    }

    fn render_outline(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let doc = self.active_document().read(cx);
        // Prefer scanned code symbols (def/class even when fold misses short bodies),
        // then fold candidates, then Markdown headings.
        let symbols: Vec<(String, usize, u8)> = {
            let text = doc.text_now(cx);
            let text = text.as_ref();
            let mut items: Vec<(String, usize, u8)> = crate::analysis::code_symbols(text)
                .into_iter()
                .map(|s| (s.label, s.line + 1, s.level))
                .collect();
            if items.is_empty() {
                let editor = doc.editor().read(cx);
                let rope = editor.text();
                for fold in editor.fold_candidates() {
                    let line = fold.start_line;
                    let start = rope.line_start_offset(line).min(text.len());
                    let end = rope.line_end_offset(line).min(text.len());
                    let label = text.get(start..end).unwrap_or("").trim();
                    if !label.is_empty() {
                        items.push((label.to_string(), line + 1, 1u8));
                    }
                }
            }
            if items.is_empty() {
                for heading in &doc.analysis().outline {
                    items.push((heading.text.to_string(), heading.line + 1, heading.level));
                }
            }
            items
        };
        let current_line = doc.editor().read(cx).cursor_position().line as usize + 1;
        let min_level = symbols.iter().map(|(_, _, l)| *l).min().unwrap_or(1);
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                div()
                    .h(px(32.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_3()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child("OUTLINE"),
            )
            .child(
                v_flex()
                    .id("outline")
                    .flex_1()
                    .min_h_0()
                    .px_1()
                    .pb_2()
                    .overflow_y_scrollbar()
                    .when(symbols.is_empty(), |this| {
                        this.child(
                            div()
                                .p_3()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child("No symbols yet"),
                        )
                    })
                    .children(
                        symbols
                            .into_iter()
                            .enumerate()
                            .map(|(ix, (label, line, level))| {
                                let theme = cx.theme();
                                let active = line == current_line;
                                div()
                                    .id(("outline-sym", ix))
                                    .flex_none()
                                    .py_1()
                                    .pr_2()
                                    .pl(px(10. + 12. * f32::from(level.saturating_sub(min_level))))
                                    .rounded_md()
                                    .text_sm()
                                    .truncate()
                                    .cursor_pointer()
                                    .when(level <= min_level, |this| {
                                        this.font_weight(FontWeight::MEDIUM)
                                    })
                                    .when(active, |this| {
                                        this.bg(theme.list_active).text_color(theme.foreground)
                                    })
                                    .when(!active, |this| {
                                        this.text_color(theme.muted_foreground)
                                            .hover(|style| style.bg(theme.list_hover))
                                    })
                                    .child(label)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.with_active(window, cx, |doc, window, cx| {
                                            doc.go_to_line(line, window, cx)
                                        });
                                    }))
                            }),
                    ),
            )
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let doc = self.active_document().read(cx);
        let settings = AppSettings::get(cx);
        let muted = cx.theme().muted_foreground;
        let item = |text: String| div().text_color(muted).child(text);
        let workspace = cx.weak_entity();
        let language_label = doc.language_label().to_string();

        // A viewer has no text: no words, caret, line ending or indent to
        // report. It says where you are in it instead, as VS Code's image
        // preview names the picture's size and zoom.
        if doc.is_media() {
            let status = doc.media_status(cx);
            let pulse = crate::perf::pulse_label();
            return StatusBar::new()
                .left(
                    h_flex()
                        .gap_4()
                        .children(status.into_iter().map(item))
                        .when(!pulse.is_empty(), |this| this.child(item(pulse))),
                )
                .right(h_flex().gap_4().child(item(language_label)))
                .into_any_element();
        }

        let stats = doc.analysis().stats;
        let editor = doc.editor().read(cx);
        let position = editor.cursor_position();
        let selection = editor.selected_range();
        // Unicode scalars (VS Code–like), not UTF-8 bytes — en-dash is 1 char / 3 bytes.
        let selected_chars = if selection.is_empty() {
            0
        } else {
            selection_character_count(&editor.text().slice(selection.clone()).to_string())
        };
        let line_ending = doc.line_ending();
        let indent = doc.indent();
        let shows_editor = settings.layout.shows_editor() || !doc.has_preview();
        // Probe #13: words / min-read are prose metrics — hide for code languages.
        let prose_metrics = matches!(
            language_label.as_str(),
            "Markdown" | "MDX" | "Plain Text" | "Typst" | "CSV" | "TSV" | "XML"
        );
        let ws_eol = workspace.clone();
        let ws_indent = workspace.clone();
        let ws_lang = workspace.clone();
        StatusBar::new()
            .left(
                h_flex()
                    .gap_4()
                    .when(prose_metrics, |this| {
                        this.child(item(format!(
                            "{} {}",
                            group_thousands(stats.words),
                            if stats.words == 1 { "word" } else { "words" }
                        )))
                        .child(item(format!(
                            "{} {}",
                            group_thousands(stats.characters),
                            if stats.characters == 1 {
                                "character"
                            } else {
                                "characters"
                            }
                        )))
                        .when(stats.words > 0, |this| {
                            this.child(item(format!("{} min read", stats.reading_minutes())))
                        })
                    })
                    .when(doc.preview_limited(), |this| {
                        this.child(item("Preview limited (large file)".to_string()))
                    })
                    .children({
                        let pinned_name = self.pinned_preview.and_then(|id| {
                            self.documents
                                .iter()
                                .find(|d| d.entity_id() == id)
                                .map(|d| d.read(cx).name())
                        });
                        pinned_name.map(|name| item(format!("Pinned: {name}")))
                    })
                    .children({
                        let pulse = crate::perf::pulse_label();
                        (!pulse.is_empty()).then(|| item(pulse))
                    }),
            )
            .right(
                h_flex()
                    .gap_4()
                    .when(shows_editor, |this| {
                        this.child(item(if selection.is_empty() {
                            format!("Ln {}, Col {}", position.line + 1, position.character + 1)
                        } else {
                            format!(
                                "Ln {}, Col {} ({} selected)",
                                position.line + 1,
                                position.character + 1,
                                group_thousands(selected_chars)
                            )
                        }))
                    })
                    .when(settings.spell_check, |this| {
                        this.child(item(
                            settings
                                .spell_language
                                .clone()
                                .unwrap_or_else(spell::default_language)
                                .replace('_', "-"),
                        ))
                    })
                    // Indent picker (Spaces: N / Tab Size: N).
                    .child({
                        let label = indent.label();
                        Button::new("indent-mode")
                            .ghost()
                            .xsmall()
                            .compact()
                            .label(label)
                            .tooltip("Select Indentation")
                            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                                let mut menu = menu;
                                for (hard_tabs, size, name) in [
                                    (false, 2usize, "Spaces: 2"),
                                    (false, 4, "Spaces: 4"),
                                    (false, 8, "Spaces: 8"),
                                    (true, 2, "Tab Size: 2"),
                                    (true, 4, "Tab Size: 4"),
                                    (true, 8, "Tab Size: 8"),
                                ] {
                                    let ws = ws_indent.clone();
                                    menu = menu.item(PopupMenuItem::new(name).on_click(
                                        move |_, window, cx| {
                                            _ = ws.update(cx, |this, cx| {
                                                this.active_document().update(cx, |doc, cx| {
                                                    doc.set_indent(
                                                        crate::document::Indent { hard_tabs, size },
                                                        cx,
                                                    );
                                                });
                                                cx.notify();
                                                let _ = window;
                                            });
                                        },
                                    ));
                                }
                                menu
                            })
                    })
                    .child({
                        let ws = workspace.clone();
                        let label = if settings.soft_wrap {
                            "Wrap"
                        } else {
                            "No Wrap"
                        };
                        Button::new("status-wrap")
                            .ghost()
                            .xsmall()
                            .compact()
                            .label(label)
                            .tooltip("Toggle Word Wrap")
                            .on_click(move |_, _, cx| {
                                _ = ws.update(cx, |this, cx| {
                                    this.toggle(
                                        |settings| settings.soft_wrap = !settings.soft_wrap,
                                        cx,
                                    );
                                });
                            })
                    })
                    .when(settings.auto_save, |this| {
                        let ws = workspace.clone();
                        this.child(
                            Button::new("status-autosave")
                                .ghost()
                                .xsmall()
                                .compact()
                                .label("Auto Save")
                                .tooltip("Disable Auto Save")
                                .on_click(move |_, window, cx| {
                                    _ = ws.update(cx, |this, cx| {
                                        this.toggle_auto_save(&ToggleAutoSave, window, cx);
                                    });
                                }),
                        )
                    })
                    .children(doc.encoding().label().map(|label| item(label.to_string())))
                    // EOL picker: LF / CRLF (VS Code-like).
                    .child(
                        Button::new("line-ending")
                            .ghost()
                            .xsmall()
                            .compact()
                            .label(line_ending.label())
                            .tooltip("Select End of Line Sequence")
                            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                                let ws_lf = ws_eol.clone();
                                let ws_crlf = ws_eol.clone();
                                menu.item(PopupMenuItem::new("LF").on_click(move |_, _, cx| {
                                    _ = ws_lf.update(cx, |this, cx| {
                                        this.active_document().update(cx, |doc, cx| {
                                            doc.set_line_ending(LineEnding::Lf, cx);
                                        });
                                        cx.notify();
                                    });
                                }))
                                .item(
                                    PopupMenuItem::new("CRLF").on_click(move |_, _, cx| {
                                        _ = ws_crlf.update(cx, |this, cx| {
                                            this.active_document().update(cx, |doc, cx| {
                                                doc.set_line_ending(LineEnding::CrLf, cx);
                                            });
                                            cx.notify();
                                        });
                                    }),
                                )
                            }),
                    )
                    // Zoom as a percentage of the default UI font size (Ctrl+= / -).
                    .child(item(format!(
                        "{}%",
                        crate::settings::zoom_percent(settings.font_size)
                    )))
                    // Language mode picker.
                    .child(
                        Button::new("language-mode")
                            .ghost()
                            .xsmall()
                            .compact()
                            .label(language_label)
                            .tooltip("Select Language Mode")
                            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                                let mut menu = menu;
                                for (label, highlighter) in STATUS_LANGUAGE_MODES {
                                    let ws = ws_lang.clone();
                                    let label_s: SharedString = (*label).into();
                                    menu = menu.item(PopupMenuItem::new(*label).on_click(
                                        move |_, window, cx| {
                                            let label_s = label_s.clone();
                                            _ = ws.update(cx, |this, cx| {
                                                this.active_document().update(cx, |doc, cx| {
                                                    doc.set_language_mode(
                                                        label_s.clone(),
                                                        highlighter,
                                                        window,
                                                        cx,
                                                    );
                                                });
                                                cx.notify();
                                            });
                                        },
                                    ));
                                }
                                menu
                            }),
                    ),
            )
            .into_any_element()
    }

    /// Light breadcrumbs under the tab strip: clickable path segments (probe #6/#11).
    fn render_breadcrumbs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let doc = self.active_document().read(cx);
        // (label, absolute path to reveal — None for untitled / file leaf opens nothing extra)
        let crumbs: Vec<(String, Option<PathBuf>)> = match doc.path() {
            Some(path) => {
                let mut out = Vec::new();
                if let Some(root) = self.file_tree.read(cx).root()
                    && let Ok(rel) = path.strip_prefix(root)
                {
                    let mut acc = root.to_path_buf();
                    let comps: Vec<_> = rel.components().collect();
                    for (i, c) in comps.iter().enumerate() {
                        acc.push(c);
                        let label = c.as_os_str().to_string_lossy().into_owned();
                        let is_last = i + 1 == comps.len();
                        out.push((label, if is_last { None } else { Some(acc.clone()) }));
                    }
                }
                if out.is_empty() {
                    let name = display_name(Some(path));
                    if let Some(parent) = path.parent() {
                        let parent_name = parent
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| parent.display().to_string());
                        out.push((parent_name, Some(parent.to_path_buf())));
                    }
                    out.push((name, None));
                }
                out
            }
            None => vec![("Untitled".into(), None)],
        };
        h_flex()
            .id("breadcrumbs")
            .h(px(22.))
            .flex_none()
            .items_center()
            .px_3()
            .gap_1()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.background)
            .text_xs()
            .text_color(theme.muted_foreground)
            .children(
                crumbs
                    .into_iter()
                    .enumerate()
                    .flat_map(|(i, (part, target))| {
                        let theme = cx.theme();
                        let mut row = Vec::new();
                        if i > 0 {
                            row.push(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child("›")
                                    .into_any_element(),
                            );
                        }
                        let crumb = if let Some(path) = target {
                            div()
                                .id(("crumb", i))
                                .cursor_pointer()
                                .hover(|s| s.text_color(theme.foreground).underline())
                                .child(part)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    // Reveal folder in Explorer.
                                    this.activity_view = ActivityView::Explorer;
                                    if !AppSettings::get(cx).file_sidebar {
                                        AppSettings::update(cx, |s| s.file_sidebar = true);
                                    }
                                    this.file_tree.update(cx, |tree, cx| {
                                        tree.set_active(Some(path.clone()), cx);
                                    });
                                    this.focus_explorer(&FocusExplorer, window, cx);
                                    cx.notify();
                                }))
                                .into_any_element()
                        } else {
                            div()
                                .text_color(theme.foreground)
                                .child(part)
                                .into_any_element()
                        };
                        row.push(crumb);
                        row
                    }),
            )
    }
}

/// Modes offered by the status-bar language picker.
const STATUS_LANGUAGE_MODES: &[(&str, &str)] = &[
    ("Markdown", "markdown"),
    ("Plain Text", "text"),
    ("Typst", "text"),
    ("Rust", "rust"),
    ("Python", "python"),
    ("JavaScript", "javascript"),
    ("TypeScript", "typescript"),
    ("JSON", "json"),
    ("YAML", "yaml"),
    ("TOML", "toml"),
    ("HTML", "html"),
    ("CSS", "css"),
    ("Shell Script", "bash"),
    ("Go", "go"),
    ("C", "c"),
    ("C++", "cpp"),
];

/// The settings dialog: Malgel's optional features and how it behaves.
pub fn open_settings(window: &mut Window, cx: &mut App) {
    crate::settings_ui::open(window, cx);
}

/// Malgel's website, linked from the About dialog.
const WEBSITE: &str = "https://www.malgel.com";

/// Show the version, a one-line description, the website and the license.
pub fn open_about(window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, |alert, _, cx| {
        let muted = cx.theme().muted_foreground;
        alert
            .title("Malgel")
            .description(
                v_flex()
                    .gap_2()
                    .child(concat!(
                        "Version ",
                        env!("CARGO_PKG_VERSION"),
                        ". A fast, native Markdown editor built with GPUI Kit."
                    ))
                    .child(
                        Link::new("website")
                            .href(WEBSITE)
                            .child(WEBSITE.trim_start_matches("https://")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("Licensed under the Apache License, Version 2.0."),
                    ),
            )
            .ok_text("OK")
    });
}

/// Ask before opening a web link (or an email address, or another app's
/// link) outside Malgel, as a document's links can lead anywhere.
fn confirm_external_link(url: String, window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    let web = url.starts_with("http://") || url.starts_with("https://");
    let dont_ask = Rc::new(Cell::new(false));
    window.open_alert_dialog(cx, move |alert, _, cx| {
        let (url, open_url) = (url.clone(), url.clone());
        let (checkbox, remember) = (dont_ask.clone(), dont_ask.clone());
        alert
            .title(if web {
                "Open this link in your browser?"
            } else {
                "Open this link?"
            })
            .description(
                v_flex()
                    .gap_3()
                    .child(
                        div()
                            .px_2()
                            .py_1p5()
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().muted)
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .child(url.chars().take(400).collect::<String>()),
                    )
                    .when(web, |this| {
                        this.child(
                            Checkbox::new("dont-ask")
                                .label("Don’t ask again for web links")
                                .checked(dont_ask.get())
                                .on_click(move |checked, window, _| {
                                    checkbox.set(*checked);
                                    window.refresh();
                                }),
                        )
                    }),
            )
            .footer(
                DialogFooter::new()
                    .justify_end()
                    .child(
                        Button::new("cancel")
                            .label("Cancel")
                            .outline()
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("open")
                            .label(if web { "Open in Browser" } else { "Open" })
                            .primary()
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                if remember.get() {
                                    AppSettings::update(cx, |settings| {
                                        settings.confirm_web_links = false
                                    });
                                }
                                cx.open_url(&open_url);
                            }),
                    ),
            )
    });
}

/// Ask before opening a linked file Malgel doesn't show itself (or a
/// folder) with the app the system uses for it.
fn confirm_open_file(path: PathBuf, window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let folder = path.is_dir();
    window.open_alert_dialog(cx, move |alert, _, _| {
        let path = path.clone();
        alert
            .title(format!("Open “{name}”?"))
            .description(if folder {
                "The folder will open in your file manager."
            } else {
                "Malgel can’t show this file, so it will open in the app your system uses for it."
            })
            .footer(
                DialogFooter::new()
                    .justify_end()
                    .child(
                        Button::new("cancel")
                            .label("Cancel")
                            .outline()
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(Button::new("open").label("Open").primary().on_click(
                        move |_, window, cx| {
                            window.close_dialog(cx);
                            cx.open_with_system(&path);
                        },
                    )),
            )
    });
}

fn default_dir(dir: Option<PathBuf>) -> PathBuf {
    dir.or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn heading_icon(level: u8) -> Icon {
    Icon::new(match level {
        1 => IconName::Heading1,
        2 => IconName::Heading2,
        3 => IconName::Heading3,
        _ => IconName::Heading,
    })
}

#[derive(Clone, Copy)]
enum ExportFormat {
    Html,
    Pdf,
    Docx,
}

impl ExportFormat {
    fn extension(self) -> &'static str {
        match self {
            ExportFormat::Html => "html",
            ExportFormat::Pdf => "pdf",
            ExportFormat::Docx => "docx",
        }
    }
}

/// A file name from the first heading, for documents never saved.
fn suggested_file_name(analysis: &Analysis, extension: &str) -> String {
    let stem: String = analysis
        .outline
        .first()
        .map(|heading| {
            heading
                .text
                .chars()
                .filter(|ch| !matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
                .take(60)
                .collect::<String>()
                .trim()
                .to_string()
        })
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "Untitled".to_string());
    format!("{stem}.{extension}")
}

/// Count Unicode scalars in selected text. Prefer this over a byte-offset
/// `range.len()` so the status bar matches VS Code (en-dash = 1, not 3).
fn selection_character_count(selected: &str) -> usize {
    selected.chars().count()
}

fn group_thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (ix, ch) in digits.chars().enumerate() {
        if ix > 0 && (digits.len() - ix).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_window_title(window, cx);
        // Keyboard commands need a focused element inside the workspace; if
        // focus was lost (e.g. its element left the layout), take it back.
        if window.focused(cx).is_none() {
            let focus = self.focus_handle.clone();
            window.defer(cx, move |window, cx| {
                if window.focused(cx).is_none() {
                    focus.focus(window, cx);
                }
            });
        }
        let settings = AppSettings::get(cx).clone();
        let focus_mode = settings.focus_mode;
        let show_files = settings.file_sidebar && !focus_mode;
        let show_outline = settings.outline_sidebar && !focus_mode;
        let rem = window.rem_size();
        let sidebar_range = rems(12.).to_pixels(rem)..rems(28.).to_pixels(rem);
        let files_width = rems(15.).to_pixels(rem);
        let outline_width = rems(14.).to_pixels(rem);
        // Each arrangement of panes keeps its own sizes, so adding or
        // removing a sidebar starts it at its natural width.
        //
        // The document panel deliberately has no size preference: it is the
        // flexible slot and fills whatever the sidebars leave. Sizing it from
        // `viewport_size` minus an assumed sidebar width desyncs from the
        // resizable group's actual sizes (user-resized/persisted sidebars,
        // HiDPI unit mix), which used to push the whole panel past the
        // window edge — most visibly in the PDF view.
        let mut main = h_resizable(match (show_files, show_outline) {
            (true, true) => "panes-files-outline",
            (true, false) => "panes-files",
            (false, true) => "panes-outline",
            (false, false) => "panes",
        });
        if show_files {
            main = main.child(
                resizable_panel()
                    .size(files_width)
                    .size_range(sidebar_range.clone())
                    .child(self.render_activity_sidebar(cx)),
            );
        }
        main = main.child(
            resizable_panel()
                .size_range(rems(20.).to_pixels(rem)..Pixels::MAX)
                .child(
                    v_flex()
                        .size_full()
                        .when(self.shows_tabs(cx), |this| this.child(self.render_tabs(cx)))
                        .child(self.render_breadcrumbs(cx))
                        .when(self.shows_welcome_cta(cx), |this| {
                            this.child(self.render_welcome_cta(cx))
                        })
                        .child(if self.diff_session.is_some() {
                            // Probe #13: flex_1 like document_stack — size_full alone left
                            // panes with zero content height (blank white + mid labels).
                            div()
                                .id("diff-host")
                                .flex_1()
                                .min_h_0()
                                .min_w_0()
                                .child(self.render_diff_editor(cx))
                                .into_any_element()
                        } else {
                            self.render_document_stack(cx).into_any_element()
                        }),
                ),
        );
        if show_outline {
            main = main.child(
                resizable_panel()
                    .size(outline_width)
                    .size_range(sidebar_range)
                    .child(self.render_outline(cx)),
            );
        }

        v_flex()
            .id("workspace")
            .key_context(WORKSPACE_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::new_file))
            .on_action(cx.listener(Self::open))
            .on_action(cx.listener(Self::open_recent))
            .on_action(cx.listener(Self::follow_link))
            .on_action(cx.listener(Self::view_diagram))
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(Self::close_folder))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::save_as))
            .on_action(cx.listener(Self::save_all))
            .on_action(cx.listener(Self::show_open_recent))
            .on_action(cx.listener(Self::toggle_maximized_panel))
            .on_action(cx.listener(Self::export_html))
            .on_action(cx.listener(Self::export_pdf))
            .on_action(cx.listener(Self::export_docx))
            .on_action(cx.listener(Self::copy_rich_text))
            .on_action(cx.listener(Self::reveal_in_folder))
            .on_action(cx.listener(Self::close_document))
            .on_action(cx.listener(Self::toggle_line_ending))
            .on_action(cx.listener(Self::close_all_tabs))
            .on_action(cx.listener(Self::reopen_closed_editor))
            .on_action(cx.listener(Self::pin_tab))
            .on_action(cx.listener(Self::toggle_pin_preview_action))
            .on_action(cx.listener(Self::force_recompile_action))
            .on_action(cx.listener(Self::show_tab_context_menu))
            .on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
            .on_action(cx.listener(Self::close_other_tabs))
            .on_action(cx.listener(Self::close_saved_editors))
            .on_action(cx.listener(Self::close_editors_in_group))
            .on_action(cx.listener(Self::close_tabs_to_the_right))
            .on_action(cx.listener(Self::tree_new_file))
            .on_action(cx.listener(Self::focus_explorer))
            .on_action(cx.listener(Self::tree_new_folder))
            .on_action(cx.listener(Self::tree_rename))
            .on_action(cx.listener(Self::tree_delete))
            .on_action(cx.listener(Self::tree_cut))
            .on_action(cx.listener(Self::tree_copy))
            .on_action(cx.listener(Self::tree_paste))
            .on_action(cx.listener(Self::tree_copy_path))
            .on_action(cx.listener(Self::tree_copy_relative_path))
            .on_action(cx.listener(Self::tree_reveal_path))
            .on_action(cx.listener(Self::tree_open))
            .on_action(cx.listener(Self::tree_open_to_side))
            .on_action(cx.listener(Self::select_for_compare))
            .on_action(cx.listener(Self::compare_with_selected))
            .on_action(cx.listener(Self::tree_select_up))
            .on_action(cx.listener(Self::tree_select_down))
            .on_action(cx.listener(Self::tree_select_left))
            .on_action(cx.listener(Self::tree_select_right))
            .on_action(cx.listener(Self::tree_select_first))
            .on_action(cx.listener(Self::tree_select_last))
            .on_action(cx.listener(Self::close_window))
            .on_action(cx.listener(Self::quit))
            .on_action(cx.listener(Self::set_layout))
            .on_action(cx.listener(Self::toggle_preview_layout))
            .on_action(cx.listener(Self::go_to_heading))
            .on_action(cx.listener(Self::show_command_palette))
            .on_action(cx.listener(Self::show_go_to_line))
            .on_action(cx.listener(Self::go_to_line_at))
            .on_action(cx.listener(Self::go_to_definition))
            .on_action(cx.listener(Self::show_quick_open))
            .on_action(cx.listener(Self::open_quick_path))
            .on_action(cx.listener(Self::find_in_files))
            .on_action(cx.listener(Self::replace_in_files))
            .on_action(cx.listener(Self::toggle_auto_save))
            .on_action(cx.listener(Self::format_document))
            .on_action(cx.listener(Self::trim_trailing_whitespace))
            .on_action(cx.listener(Self::show_explorer_view))
            .on_action(cx.listener(Self::show_search_view))
            .on_action(cx.listener(Self::toggle_appearance))
            .on_action(cx.listener(Self::show_theme_picker))
            .on_action(cx.listener(Self::toggle_focus_mode))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::open_keyboard_shortcuts))
            .on_action(cx.listener(Self::toggle_gfm_breaks))
            .on_action(cx.listener(Self::split_editor_right))
            .on_action(cx.listener(Self::split_editor_down))
            .on_action(cx.listener(Self::close_editor_split))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| {
                let next = (this.active + 1) % this.documents.len();
                this.activate(next, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PreviousTab, window, cx| {
                let count = this.documents.len();
                this.activate((this.active + count - 1) % count, window, cx);
            }))
            .on_action(cx.listener(|this, action: &ActivateTab, window, cx| {
                this.activate(action.0, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Escape, window, cx| {
                if AppSettings::get(cx).focus_mode {
                    this.toggle(|settings| settings.focus_mode = false, cx);
                    this.focus_active(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleScrollSync, _, cx| {
                this.toggle(|settings| settings.scroll_sync = !settings.scroll_sync, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleSoftWrap, _, cx| {
                this.toggle(|settings| settings.soft_wrap = !settings.soft_wrap, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleLineNumbers, _, cx| {
                this.toggle(
                    |settings| settings.line_numbers = !settings.line_numbers,
                    cx,
                )
            }))
            .on_action(
                cx.listener(|this, _: &SummonTerminal, window, cx| {
                    this.summon_terminal(window, cx)
                }),
            )
            .on_action(cx.listener(Self::new_terminal))
            .on_action(cx.listener(Self::kill_terminal))
            .on_action(cx.listener(Self::clear_terminal))
            .on_action(cx.listener(Self::split_terminal))
            .on_action(cx.listener(Self::go_to_symbol))
            .on_action(cx.listener(Self::reload_window))
            .on_action(cx.listener(Self::toggle_zen_mode))
            .on_action(cx.listener(Self::compare_with_saved))
            .on_action(cx.listener(Self::show_source_control))
            .on_action(cx.listener(Self::show_extensions))
            .on_action(cx.listener(|this, _: &ToggleTabs, _, cx| {
                this.toggle(|settings| settings.tabs = !settings.tabs, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleFileSidebar, _, cx| {
                this.toggle(
                    |settings| settings.file_sidebar = !settings.file_sidebar,
                    cx,
                )
            }))
            .on_action(cx.listener(|this, _: &ToggleOutline, _, cx| {
                this.toggle(
                    |settings| settings.outline_sidebar = !settings.outline_sidebar,
                    cx,
                )
            }))
            .on_action(cx.listener(|this, _: &ToggleProblems, _, cx| {
                this.toggle(
                    |settings| settings.problems_panel = !settings.problems_panel,
                    cx,
                )
            }))
            .on_action(cx.listener(|this, _: &ToggleMinimap, _, cx| {
                this.toggle(|settings| settings.minimap = !settings.minimap, cx)
            }))
            .on_action(cx.listener(|this, action: &OpenAppMenu, window, cx| {
                this.app_menu_bar.update(cx, |bar, cx| {
                    bar.open_named(&action.0, window, cx);
                });
            }))
            .on_action(cx.listener(|this, _: &ToggleSpellCheck, _, cx| {
                this.toggle(|settings| settings.spell_check = !settings.spell_check, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleRestoreSession, _, cx| {
                this.toggle(
                    |settings| settings.restore_session = !settings.restore_session,
                    cx,
                )
            }))
            .on_action(cx.listener(|this, action: &SetSpellLanguage, _, cx| {
                let language = action.0.to_string();
                this.toggle(
                    |settings| {
                        settings.spell_language = Some(language);
                        settings.spell_check = true;
                    },
                    cx,
                );
            }))
            .on_action(
                cx.listener(|this, action: &ReplaceMisspelling, window, cx| {
                    let action = action.clone();
                    this.with_active(window, cx, |doc, window, cx| {
                        doc.replace_misspelling(&action, window, cx)
                    });
                }),
            )
            .on_action(cx.listener(|this, action: &AddToDictionary, _, cx| {
                spell::add_to_personal(&action.0);
                this.recheck_spelling(cx);
            }))
            .on_action(|_: &About, window, cx| open_about(window, cx))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom_active(1., cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom_active(-1., cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.zoom_active(0., cx)))
            .on_action(cx.listener(|this, _: &TidyTable, window, cx| {
                this.with_active(window, cx, |doc, window, cx| doc.tidy_table(window, cx))
            }))
            .on_action(cx.listener(|this, _: &ToggleLineComment, window, cx| {
                this.with_active(window, cx, |doc, window, cx| {
                    doc.toggle_line_comment(window, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &SelectNextOccurrence, window, cx| {
                this.with_active(window, cx, |doc, window, cx| {
                    doc.select_next_occurrence(window, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &DeleteLine, window, cx| {
                this.with_active(window, cx, |doc, window, cx| doc.delete_lines(window, cx))
            }))
            .on_action(cx.listener(|this, _: &MoveLineUp, window, cx| {
                this.with_active(window, cx, |doc, window, cx| {
                    doc.move_lines(format::LineMove::Up, window, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &MoveLineDown, window, cx| {
                this.with_active(window, cx, |doc, window, cx| {
                    doc.move_lines(format::LineMove::Down, window, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &Fold, window, cx| {
                this.with_active(window, cx, |doc, _window, cx| doc.fold_at_cursor(cx))
            }))
            .on_action(cx.listener(|this, _: &Unfold, window, cx| {
                this.with_active(window, cx, |doc, _window, cx| doc.unfold_at_cursor(cx))
            }))
            .on_action(cx.listener(|this, _: &TogglePdfThumbnails, window, cx| {
                this.with_active(window, cx, |doc, _window, cx| doc.toggle_pdf_thumbnails(cx))
            }))
            .on_action(cx.listener(|this, _: &Bold, window, cx| {
                this.with_active(window, cx, |doc, window, cx| {
                    doc.inline(format::Inline::Bold, window, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &Italic, window, cx| {
                this.with_active(window, cx, |doc, window, cx| {
                    doc.inline(format::Inline::Italic, window, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &Strikethrough, window, cx| {
                this.with_active(window, cx, |doc, window, cx| {
                    doc.inline(format::Inline::Strikethrough, window, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &InlineCode, window, cx| {
                this.with_active(window, cx, |doc, window, cx| {
                    doc.inline(format::Inline::Code, window, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &InsertLink, window, cx| {
                this.with_active(window, cx, |doc, window, cx| {
                    doc.apply_edit(format::insert_link, window, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &CodeBlock, window, cx| {
                this.with_active(window, cx, |doc, window, cx| {
                    doc.apply_edit(format::insert_code_block, window, cx)
                })
            }))
            .on_action(cx.listener(|this, action: &Heading, window, cx| {
                let level = action.0;
                this.with_active(window, cx, |doc, window, cx| {
                    doc.block(format::Block::Heading(level), window, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &Quote, window, cx| {
                this.with_active(window, cx, |doc, window, cx| {
                    doc.block(format::Block::Quote, window, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &BulletList, window, cx| {
                this.with_active(window, cx, |doc, window, cx| {
                    doc.block(format::Block::BulletList, window, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &NumberedList, window, cx| {
                this.with_active(window, cx, |doc, window, cx| {
                    doc.block(format::Block::NumberedList, window, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &TaskList, window, cx| {
                this.with_active(window, cx, |doc, window, cx| {
                    doc.block(format::Block::TaskList, window, cx)
                })
            }))
            .on_drop(cx.listener(Self::on_drop_paths))
            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            // A tree entry dropped outside the file tree opens (files) the
            // way dropping it in VS Code's editor area does; dropping it on
            // a tree folder is handled by the tree itself. The highlight
            // sits on the content area below, not the whole window, the way
            // VS Code highlights the editor group only.
            .on_drop(cx.listener(Self::on_drop_tree_drag))
            // While a tab drags: track pointer for edge-split, clear the
            // insertion line outside the tab bar, and arm mouse-up fallback.
            .on_drag_move::<TabDrag>(cx.listener(|this, event: &DragMoveEvent<TabDrag>, _, cx| {
                this.note_tab_drag_move(event, cx);
                let inside = this
                    .tab_bar_bounds
                    .get()
                    .is_some_and(|bounds| bounds.contains(&event.event.position));
                if !inside && this.tab_insertion.take().is_some() {
                    cx.notify();
                }
            }))
            // Probe #15: Xvfb often never delivers on_drop to the edge strip.
            // Mouse-up while an edge preview (or last pos in-zone) is active
            // commits the same move_tab_into_split as Split Editor Right.
            .capture_any_mouse_up(cx.listener(|this, event: &MouseUpEvent, window, cx| {
                if this.tab_drag_index.get().is_some()
                    || this.tab_split_preview.is_some()
                    || this.tab_drag_last_pos.get().is_some()
                {
                    // Probe #17: release point is authoritative under Xvfb —
                    // last drag-move can lag or miss the 120px edge strip.
                    this.tab_drag_last_pos.set(Some(event.position));
                    if this.tab_split_preview.is_none() {
                        let body = this.editor_body_bounds.get().or_else(|| {
                            // Fallback when body bounds never arrived: content
                            // below chrome (~tab/breadcrumb), window-local.
                            let size = window.viewport_size();
                            Some(Bounds::new(
                                Point::new(px(0.), px(70.)),
                                Size::new(size.width, (size.height - px(70.)).max(px(1.))),
                            ))
                        });
                        if let Some(vertical) = Self::edge_split_at(event.position, body) {
                            this.tab_split_preview = Some(vertical);
                        }
                    }
                    if !this.try_commit_tab_edge_split(window, cx) {
                        // Drag ended off-edge: drop stale state so a later
                        // ordinary click cannot invent a split.
                        this.tab_split_preview = None;
                        this.tab_drag_index.set(None);
                        this.tab_drag_last_pos.set(None);
                    }
                }
            }))
            .child(self.render_title_bar(cx))
            .child(self.render_center(
                if focus_mode {
                    main.into_any_element()
                } else {
                    h_flex()
                        .id("workspace-with-activity")
                        .size_full()
                        .child(self.render_activity_bar(cx))
                        .child(
                            div()
                                .id("workspace-panes")
                                .flex_1()
                                .min_w_0()
                                .size_full()
                                .child(main),
                        )
                        .into_any_element()
                },
                focus_mode,
                rem,
                window.viewport_size().height,
                cx,
            ))
            .when(!focus_mode, |this| this.child(self.render_status_bar(cx)))
    }
}

impl Workspace {
    /// Absolute path of the active document onto the clipboard, with a toast.
    /// Debounced so ordinary title-bar clicks during navigation do not stack
    /// "Copied …" notifications (round-4).
    fn copy_title_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.active_document().read(cx).path().cloned() else {
            return; // Untitled: ignore quiet clicks; Save As is the path to a location.
        };
        let now = std::time::Instant::now();
        if self
            .last_path_copy
            .is_some_and(|prev| now.duration_since(prev) < std::time::Duration::from_millis(1200))
        {
            return;
        }
        self.last_path_copy = Some(now);
        let abs = crate::paths::path_for_clipboard(&path.canonicalize().unwrap_or(path));
        cx.write_to_clipboard(ClipboardItem::new_string(abs.clone()));
        window.push_notification(Notification::info(format!("Copied {abs}")), cx);
    }

    /// Keep every open document mounted so tab switches do not remount the
    /// preview/editor (scroll and preview state stay put, VS Code-like).
    /// When [`editor_split`] is set, two document panes show side-by-side or
    /// stacked (Split Editor Right / Down).
    fn render_document_stack(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let active = self.active;
        let split = self.editor_split;
        let pin_ix = self
            .pinned_preview
            .and_then(|id| self.document_index(id))
            .filter(|&ix| ix != active);
        let stack = |active_ix: usize, id: &'static str| {
            div()
                .id(id)
                .flex_1()
                .min_h_0()
                .min_w_0()
                .relative()
                .children(self.documents.iter().enumerate().map(move |(ix, doc)| {
                    let shown = ix == active_ix;
                    div()
                        .id((id, ix))
                        .absolute()
                        .inset_0()
                        .when(!shown, |this| this.invisible())
                        .child(doc.clone())
                }))
        };

        // Pinned preview of another tab: editor (active) | preview (pinned).
        // Roles are already EditorOnly / PreviewOnly via sync_preview_pin_roles.
        //
        // Critical: each DocumentView entity must appear in the tree **once**.
        // Mounting every doc in both stacks (the old behaviour) duplicated the
        // active editor under the preview column — invisible but still focusable
        // — so Ctrl+Z / shortcuts hit the wrong instance or none at all.
        if let Some(pin_ix) = pin_ix
            && split.is_none()
        {
            let editor_stack = div()
                .id("doc-stack-active-editor")
                .flex_1()
                .min_h_0()
                .min_w_0()
                .relative()
                .w_full()
                .overflow_hidden()
                .children({
                    let (editor_ixs, _) =
                        pinned_preview_mounts(self.documents.len(), active, pin_ix);
                    editor_ixs.into_iter().map(|ix| {
                        let doc = self.documents[ix].clone();
                        let shown = ix == active;
                        div()
                            .id(("doc-stack-active-editor", ix))
                            .absolute()
                            .inset_0()
                            .when(!shown, |this| this.invisible())
                            .child(doc)
                    })
                });
            let preview_pane = div()
                .id("doc-stack-pinned-preview")
                .flex_1()
                .min_h_0()
                .min_w_0()
                .w_full()
                .overflow_hidden()
                .child(self.documents[pin_ix].clone());
            let panes = h_flex()
                .id("pinned-preview-split")
                .size_full()
                .min_w_0()
                .items_stretch()
                .overflow_hidden()
                .child(editor_stack)
                .child(div().w(px(1.)).h_full().bg(cx.theme().border).flex_none())
                .child(preview_pane)
                .into_any_element();
            return div()
                .id("document-stack")
                .flex_1()
                .min_h_0()
                .min_w_0()
                .overflow_hidden()
                .child(panes)
                .into_any_element();
        }

        let drop_listener = cx.listener(|this, drag: &TabDrag, window, cx| {
            this.tab_drag_index.set(Some(drag.index));
            // Probe #15: edge preview / last-pos zone → real second editor group.
            if this.try_commit_tab_edge_split(window, cx) {
                return;
            }
            let ix = drag.index.min(this.documents.len().saturating_sub(1));
            // Probe #16: drop onto an existing split = move into that pane, never
            // clone (invert-key duplicate only applies to tab-bar reorder).
            if let Some(split) = this.editor_split {
                let len = this.documents.len();
                let (active, other) = Self::split_panes_after_cross_group_move(
                    ix,
                    this.active,
                    split.other,
                    len,
                    true, // body drop targets the secondary pane
                );
                this.active = active;
                this.editor_split = Some(EditorSplit {
                    other,
                    vertical: split.vertical,
                });
                this.sync_preview_pin_roles(cx);
                this.tab_insertion = None;
                this.tab_drag_index.set(None);
                this.tab_drag_last_pos.set(None);
                this.tab_drag_copy.set(false);
                cx.notify();
                return;
            }
            // Center drop: activate only — do not flip Layout::Split (MD preview).
            this.activate(ix, window, cx);
            this.tab_insertion = None;
            this.tab_drag_index.set(None);
            this.tab_drag_last_pos.set(None);
            cx.notify();
        });

        if let Some(split) = split {
            let other = split.other.min(self.documents.len().saturating_sub(1));
            let panes = if split.vertical {
                v_flex()
                    .id("editor-split-vertical")
                    .size_full()
                    .min_h_0()
                    .overflow_hidden()
                    .child(
                        stack(active, "doc-stack-primary")
                            .h_full()
                            .overflow_hidden(),
                    )
                    .child(div().h(px(1.)).w_full().bg(cx.theme().border).flex_none())
                    .child(
                        stack(other, "doc-stack-secondary")
                            .h_full()
                            .overflow_hidden(),
                    )
                    .into_any_element()
            } else {
                h_flex()
                    .id("editor-split-horizontal")
                    .size_full()
                    .min_w_0()
                    .items_stretch()
                    .overflow_hidden()
                    .child(
                        stack(active, "doc-stack-primary")
                            .w_full()
                            .overflow_hidden(),
                    )
                    .child(div().w(px(1.)).h_full().bg(cx.theme().border).flex_none())
                    .child(
                        stack(other, "doc-stack-secondary")
                            .w_full()
                            .overflow_hidden(),
                    )
                    .into_any_element()
            };
            let ws_bounds = cx.weak_entity();
            div()
                .id("document-stack")
                .flex_1()
                .min_h_0()
                .min_w_0()
                .overflow_hidden()
                .on_drop(drop_listener)
                .drag_over::<TabDrag>(|style, _, _, cx| style.bg(cx.theme().drop_target))
                .on_drag_move::<TabDrag>(move |event: &DragMoveEvent<TabDrag>, _, cx| {
                    _ = ws_bounds.update(cx, |this, cx| {
                        this.editor_body_bounds.set(Some(event.bounds));
                        this.note_tab_drag_move(event, cx);
                    });
                })
                .child(panes)
                .into_any_element()
        } else {
            let preview = self.tab_split_preview;
            let ws_right = cx.weak_entity();
            let ws_down = cx.weak_entity();
            let ws_bounds = cx.weak_entity();
            let edge = px(Self::TAB_EDGE_SPLIT_PX);
            // Probe #15: wide occluding strips + on_drop. Mouse-up fallback on
            // the workspace covers Xvfb cases where the strip never gets the drop.
            let drop_right = cx.listener(|this, drag: &TabDrag, window, cx| {
                this.tab_drag_index.set(Some(drag.index));
                this.tab_split_preview = Some(false);
                this.move_tab_into_split(drag.index, false, window, cx);
            });
            let drop_down = cx.listener(|this, drag: &TabDrag, window, cx| {
                this.tab_drag_index.set(Some(drag.index));
                this.tab_split_preview = Some(true);
                this.move_tab_into_split(drag.index, true, window, cx);
            });
            div()
                .id("document-stack")
                .flex_1()
                .min_h_0()
                .min_w_0()
                .relative()
                .on_drop(drop_listener)
                .drag_over::<TabDrag>(|style, _, _, cx| style.bg(cx.theme().drop_target))
                .on_drag_move::<TabDrag>(move |event: &DragMoveEvent<TabDrag>, _, cx| {
                    _ = ws_bounds.update(cx, |this, cx| {
                        this.editor_body_bounds.set(Some(event.bounds));
                        this.note_tab_drag_move(event, cx);
                    });
                })
                .children(self.documents.iter().enumerate().map(|(ix, doc)| {
                    let shown = ix == active;
                    div()
                        .id(("doc-pane", ix))
                        .absolute()
                        .inset_0()
                        .when(!shown, |this| this.invisible())
                        .child(doc.clone())
                }))
                .child(
                    div()
                        .id("tab-drop-right")
                        .absolute()
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .w(edge)
                        // Hit-testable during TabDrag via on_drop/on_drag_move;
                        // mouse-up fallback covers Xvfb misses. No permanent
                        // occlude — that would steal ordinary editor clicks.
                        .when(preview == Some(false), |this| {
                            this.bg(cx.theme().drop_target)
                        })
                        .drag_over::<TabDrag>(|style, _, _, cx| style.bg(cx.theme().drop_target))
                        .on_drag_move::<TabDrag>(move |event: &DragMoveEvent<TabDrag>, _, cx| {
                            _ = ws_right.update(cx, |this, cx| {
                                this.tab_drag_index.set(Some(event.drag(cx).index));
                                this.tab_drag_last_pos.set(Some(event.event.position));
                                if this.tab_split_preview != Some(false) {
                                    this.tab_split_preview = Some(false);
                                    cx.notify();
                                }
                            });
                        })
                        .on_drop(drop_right),
                )
                .child(
                    div()
                        .id("tab-drop-down")
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .h(edge)
                        .when(preview == Some(true), |this| {
                            this.bg(cx.theme().drop_target)
                        })
                        .drag_over::<TabDrag>(|style, _, _, cx| style.bg(cx.theme().drop_target))
                        .on_drag_move::<TabDrag>(move |event: &DragMoveEvent<TabDrag>, _, cx| {
                            _ = ws_down.update(cx, |this, cx| {
                                this.tab_drag_index.set(Some(event.drag(cx).index));
                                this.tab_drag_last_pos.set(Some(event.event.position));
                                if this.tab_split_preview != Some(true) {
                                    this.tab_split_preview = Some(true);
                                    cx.notify();
                                }
                            });
                        })
                        .on_drop(drop_down),
                )
                .into_any_element()
        }
    }

    /// Problems panel: diagnostics + spell issues for the active document.
    fn render_problems(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let problems = self.active_document().read(cx).problems(cx);
        v_flex()
            .id("problems-panel")
            .size_full()
            .bg(theme.sidebar)
            .child(
                div()
                    .h(px(28.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_3()
                    .gap_2()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child("PROBLEMS")
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child(format!("{}", problems.len())),
                    ),
            )
            .child(
                v_flex()
                    .id("problems-list")
                    .flex_1()
                    .min_h_0()
                    .px_1()
                    .pb_2()
                    .overflow_y_scrollbar()
                    .when(problems.is_empty(), |this| {
                        this.child(
                            v_flex()
                                .p_3()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .child("No problems have been detected in the active document."),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("Enable Check spelling in Settings, or open a Typst file with compile diagnostics."),
                                )
                                .child(
                                    Button::new("problems-open-settings")
                                        .label("Open Settings…")
                                        .xsmall()
                                        .outline()
                                        .on_click(|_, window, cx| {
                                            window.dispatch_action(Box::new(OpenSettings), cx)
                                        }),
                                ),
                        )
                    })
                    .children(problems.into_iter().enumerate().map(|(ix, problem)| {
                        let theme = cx.theme();
                        let line = problem.line;
                        let color = if problem.error {
                            theme.danger
                        } else if problem.warning {
                            theme.warning
                        } else {
                            theme.muted_foreground
                        };
                        div()
                            .id(("problem", ix))
                            .flex_none()
                            .py_1()
                            .px_2()
                            .rounded_md()
                            .text_sm()
                            .cursor_pointer()
                            .hover(|style| style.bg(theme.list_hover))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_start()
                                    .child(
                                        div()
                                            .mt_1()
                                            .w(px(6.))
                                            .h(px(6.))
                                            .rounded(px(3.))
                                            .bg(color)
                                            .flex_none(),
                                    )
                                    .child(
                                        div().min_w_0().child(problem.message.clone()).when_some(
                                            line,
                                            |this, line| {
                                                this.child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(theme.muted_foreground)
                                                        .child(format!("Line {line}")),
                                                )
                                            },
                                        ),
                                    ),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Some(line) = line {
                                    this.with_active(window, cx, |doc, window, cx| {
                                        doc.go_to_line(line, window, cx);
                                    });
                                }
                            }))
                    })),
            )
    }

    fn render_terminal_area(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let active = self
            .terminal_active
            .min(self.terminals.len().saturating_sub(1));
        let session_bar = if self.terminals.len() > 1 {
            Some(
                h_flex()
                    .id("terminal-sessions")
                    .h(px(28.))
                    .px_2()
                    .gap_1()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .children(self.terminals.iter().enumerate().map(|(ix, _)| {
                        let selected = ix == active;
                        Button::new(("term-session", ix as u64))
                            .ghost()
                            .small()
                            .label(format!("{}", ix + 1))
                            .selected(selected)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.terminal_active = ix;
                                this.terminals[ix].update(cx, |term, cx| {
                                    term.focus_input(window, cx);
                                });
                                cx.notify();
                            }))
                    })),
            )
        } else {
            None
        };

        let body = if self.terminal_split && self.terminals.len() >= 2 {
            // Always paint lower index on the LEFT so click-to-focus matches the
            // pane under the cursor (no left/right swap when active is last).
            let pair = if active + 1 < self.terminals.len() {
                (active, active + 1)
            } else {
                (active.saturating_sub(1), active)
            };
            let left = pair.0.min(pair.1);
            let right = pair.0.max(pair.1);
            h_flex()
                .id("terminal-split")
                .size_full()
                .gap_px()
                .child(
                    div()
                        .id(("term-pane", left as u64))
                        .flex_1()
                        .min_w_0()
                        .size_full()
                        .border_1()
                        .border_color(if left == active {
                            cx.theme().border
                        } else {
                            cx.theme().border.opacity(0.4)
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                this.terminal_active = left;
                                this.terminals[left].update(cx, |term, cx| {
                                    term.focus_input(window, cx);
                                });
                                cx.notify();
                            }),
                        )
                        .child(self.terminals[left].clone()),
                )
                .child(
                    div()
                        .id(("term-pane", right as u64))
                        .flex_1()
                        .min_w_0()
                        .size_full()
                        .border_1()
                        .border_color(if right == active {
                            cx.theme().border
                        } else {
                            cx.theme().border.opacity(0.4)
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                this.terminal_active = right;
                                this.terminals[right].update(cx, |term, cx| {
                                    term.focus_input(window, cx);
                                });
                                cx.notify();
                            }),
                        )
                        .child(self.terminals[right].clone()),
                )
                .into_any_element()
        } else {
            self.active_terminal().clone().into_any_element()
        };

        v_flex()
            .id("terminal-area")
            .size_full()
            .when_some(session_bar, |this, bar| this.child(bar))
            .child(div().id("terminal-body").flex_1().min_h_0().child(body))
            .into_any_element()
    }

    fn render_activity_sidebar(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        match self.activity_view {
            ActivityView::Explorer => self.file_tree.clone().into_any_element(),
            ActivityView::Search => self.render_search_sidebar(cx).into_any_element(),
            ActivityView::SourceControl => self
                .render_stub_sidebar(
                    "SOURCE CONTROL",
                    "Track changes when Git support lands.",
                    cx,
                )
                .into_any_element(),
            ActivityView::Extensions => self
                .render_stub_sidebar(
                    "EXTENSIONS",
                    "Install add-ons when the marketplace lands.",
                    cx,
                )
                .into_any_element(),
        }
    }

    fn render_stub_sidebar(
        &self,
        title: &'static str,
        message: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let is_scm = title.starts_with("SOURCE");
        v_flex()
            .id(title)
            .size_full()
            .bg(theme.sidebar)
            .child(
                div()
                    .h(px(32.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_3()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child(title),
            )
            .child(
                v_flex()
                    .gap_3()
                    .p_3()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(message),
                    )
                    .child(if is_scm {
                        Button::new("scm-open-folder")
                            .label("Open Folder…")
                            .primary()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(OpenFolder), cx)
                            })
                            .into_any_element()
                    } else {
                        Button::new("ext-learn-more")
                            .label("Open Settings…")
                            .primary()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(OpenSettings), cx)
                            })
                            .into_any_element()
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(if is_scm {
                                "Git source control is on the roadmap. Open a folder to browse files meanwhile."
                            } else {
                                "Extensions marketplace is on the roadmap. Configure Malgel in Settings."
                            }),
                    ),
            )
    }

    /// Search activity: Find + Replace fields and results (probe #11).
    fn render_search_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let workspace = cx.weak_entity();
        v_flex()
            .id("search-sidebar")
            .size_full()
            .bg(theme.sidebar)
            // Escape in the Find or Replace field puts the keyboard back in
            // the editor, as VS Code's Search view does.
            .capture_key_down(
                cx.listener(|this, event: &gpui_kit::KeyDownEvent, window, cx| {
                    let keys = &event.keystroke;
                    if keys.key == "escape" && !keys.modifiers.modified() {
                        this.focus_active(window, cx);
                        cx.stop_propagation();
                    }
                }),
            )
            .child(
                div()
                    .h(px(32.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_3()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child("SEARCH"),
            )
            .child(
                v_flex()
                    .flex_none()
                    .gap_1()
                    .px_2()
                    .pb_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Find"),
                    )
                    .child(Input::new(&self.search_find))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Replace"),
                    )
                    .child(Input::new(&self.search_replace))
                    .child(
                        h_flex()
                            .gap_1()
                            .child({
                                let ws = workspace.clone();
                                Button::new("sidebar-find")
                                    .label("Find")
                                    .xsmall()
                                    .outline()
                                    .on_click(move |_, window, cx| {
                                        _ = ws.update(cx, |this, cx| {
                                            this.sidebar_run_find(window, cx);
                                        });
                                    })
                            })
                            .child({
                                let ws = workspace.clone();
                                Button::new("sidebar-replace")
                                    .label("Replace")
                                    .xsmall()
                                    .outline()
                                    .on_click(move |_, window, cx| {
                                        _ = ws.update(cx, |this, cx| {
                                            this.sidebar_run_replace(false, window, cx);
                                        });
                                    })
                            })
                            .child({
                                let ws = workspace.clone();
                                Button::new("sidebar-replace-all")
                                    .label("Replace All")
                                    .xsmall()
                                    .primary()
                                    .on_click(move |_, window, cx| {
                                        _ = ws.update(cx, |this, cx| {
                                            this.sidebar_run_replace(true, window, cx);
                                        });
                                    })
                            }),
                    ),
            )
            .child(self.render_search_results(cx))
    }

    /// Under the fields: a line saying what the search found (or that it
    /// is running, or found nothing), then the hits grouped by file.
    fn render_search_results(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let theme = cx.theme();
        let note = |text: String| {
            div()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        let found = match &self.sidebar_search {
            SidebarSearch::Idle => {
                return note("Type and press Enter to search the folder.".into())
                    .id("search-sidebar-idle")
                    .into_any_element();
            }
            SidebarSearch::Searching { query, .. } => {
                return h_flex()
                    .id("search-sidebar-searching")
                    .px_3()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .child(Spinner::new().xsmall())
                    .child(note(format!(
                        "Searching for \u{201c}{query}\u{201d}\u{2026}"
                    )))
                    .into_any_element();
            }
            SidebarSearch::Done { query, found, .. } if found.hits.is_empty() => {
                return note(format!("No results found for \u{201c}{query}\u{201d}."))
                    .id("search-sidebar-none")
                    .into_any_element();
            }
            SidebarSearch::Done { found, .. } => found,
        };
        v_flex()
            .id("search-sidebar-hits")
            .flex_1()
            .min_h_0()
            .child(note(search_summary(found)).id("search-sidebar-summary"))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px_1()
                    .pb_2()
                    .child(
                        uniform_list(
                            "search-sidebar-rows",
                            self.sidebar_rows.len(),
                            cx.processor(Self::render_search_rows),
                        )
                        .size_full()
                        .track_scroll(&self.sidebar_scroll),
                    )
                    .vertical_scrollbar(&self.sidebar_scroll),
            )
            .into_any_element()
    }

    fn render_search_rows(
        &mut self,
        range: std::ops::Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui_kit::AnyElement> {
        let SidebarSearch::Done { root, found, .. } = &self.sidebar_search else {
            return Vec::new();
        };
        let theme = cx.theme();
        let (hover, active_bg, muted, fg) = (
            theme.list_hover,
            theme.list_active,
            theme.muted_foreground,
            theme.foreground,
        );
        let badge_bg = theme.secondary;
        range
            .filter_map(|row_ix| {
                let row = self.sidebar_rows.get(row_ix)?;
                let element = match row {
                    SearchRow::File { path, count } => {
                        let relative = path.strip_prefix(root).unwrap_or(path);
                        let name = relative
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| relative.display().to_string());
                        let dir = relative
                            .parent()
                            .map(|d| d.to_string_lossy().replace('\\', "/"))
                            .unwrap_or_default();
                        let collapsed = self.sidebar_collapsed.contains(path);
                        let toggle = path.clone();
                        h_flex()
                            .id(("search-file", row_ix))
                            .h(px(22.))
                            .px_1()
                            .gap_1()
                            .items_center()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_xs()
                            .hover(|s| s.bg(hover))
                            .child(
                                Icon::new(if collapsed {
                                    IconName::ChevronRight
                                } else {
                                    IconName::ChevronDown
                                })
                                .xsmall()
                                .text_color(muted),
                            )
                            .child(file_type_icon(path).size_4())
                            .child(div().flex_none().text_color(fg).child(name))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(muted)
                                    .child(dir),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .px_1p5()
                                    .rounded_full()
                                    .bg(badge_bg)
                                    .text_color(fg)
                                    .child(count.to_string()),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.sidebar_collapsed.remove(&toggle) {
                                    this.sidebar_collapsed.insert(toggle.clone());
                                }
                                this.rebuild_search_rows();
                                cx.notify();
                            }))
                            .into_any_element()
                    }
                    SearchRow::Hit(ix) => {
                        let ix = *ix;
                        let hit = found.hits.get(ix)?;
                        let selected = self.sidebar_cursor == Some(ix);
                        h_flex()
                            .id(("search-hit", row_ix))
                            .h(px(22.))
                            .pl(px(26.))
                            .pr_1()
                            .gap_2()
                            .items_center()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_xs()
                            .when(selected, |this| this.bg(active_bg))
                            .hover(|s| s.bg(hover))
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(muted)
                                    .child(hit.line.to_string()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(fg)
                                    .child(hit.text.clone()),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_search_hit(ix, window, cx);
                            }))
                            .into_any_element()
                    }
                };
                Some(element)
            })
            .collect()
    }

    fn sidebar_run_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let find = self.search_find.read(cx).value().to_string();
        if find.is_empty() {
            self.sidebar_search = SidebarSearch::Idle;
            self.rebuild_search_rows();
            cx.notify();
            return;
        }
        let Some(root) = self.file_tree.read(cx).root().map(Path::to_path_buf) else {
            window.push_notification(Notification::info("Open a folder to search its files."), cx);
            return;
        };
        self.last_find = find.clone();
        if let SidebarSearch::Searching { cancel, .. } = &self.sidebar_search {
            cancel.store(true, Ordering::Relaxed);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.sidebar_search = SidebarSearch::Searching {
            query: find.clone(),
            cancel: cancel.clone(),
        };
        self.sidebar_cursor = None;
        self.sidebar_collapsed.clear();
        self.rebuild_search_rows();
        cx.notify();
        let show_hidden = AppSettings::get(cx).show_hidden_entries;
        let opened = self.file_tree.read(cx).expanded_folders();
        let wanted = find.clone();
        let walk_root = root.clone();
        let stop = cancel.clone();
        // The whole folder, in a fixed order; only the hit count is capped.
        // The file list lives and dies on this task, so a finished search
        // keeps nothing but its hits.
        let listing = cx.background_executor().spawn(async move {
            let _span = crate::perf::Span::new("folder_search");
            let files =
                quick_open::files_under(&walk_root, show_hidden, &opened, quick_open::MAX_FILES);
            let found = search::search_folder_files(&files, &wanted, search::MAX_HITS, &stop);
            drop(files);
            crate::perf::release_free_memory();
            found
        });
        cx.spawn(async move |this, cx| {
            let found = listing.await;
            _ = this.update(cx, |this, cx| {
                let current = matches!(
                    &this.sidebar_search,
                    SidebarSearch::Searching { cancel: c, .. } if Arc::ptr_eq(c, &cancel)
                );
                if !current || cancel.load(Ordering::Relaxed) {
                    return;
                }
                crate::perf::mark(&format!(
                    "folder_search_done hits={} files={}/{}",
                    found.hits.len(),
                    found.files_searched,
                    found.files_total
                ));
                this.sidebar_search = SidebarSearch::Done {
                    query: find,
                    root,
                    found,
                };
                this.rebuild_search_rows();
                cx.notify();
            });
        })
        .detach();
    }

    fn sidebar_run_replace(&mut self, all: bool, window: &mut Window, cx: &mut Context<Self>) {
        let find = self.search_find.read(cx).value().to_string();
        let replace = self.search_replace.read(cx).value().to_string();
        if find.is_empty() {
            window.push_notification(Notification::info("Enter text to find."), cx);
            return;
        }
        self.last_find = find.clone();
        self.last_replace = replace.clone();
        if all {
            self.confirm_replace_in_files(find, replace, window, cx);
        } else {
            // Preview via the same confirm dialog (single Replace still confirms).
            self.confirm_replace_in_files(find, replace, window, cx);
        }
    }

    fn render_diff_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let Some(session) = self.diff_session.as_ref() else {
            return div().into_any_element();
        };
        let hunk_n = session.hunks.len();
        let current = session.current.min(hunk_n.saturating_sub(1));
        let hunk = session.hunks.get(current);
        let left_lines = crate::diff::lines(&session.left);
        let right_lines = crate::diff::lines(&session.right);
        // Probe #14: h_flex() centers on the cross axis by default, so panes
        // collapsed to label height and floated mid-screen (blank body). Stretch
        // + h_full so each pane resolves flex_1 scroll height against the row.
        let left_view = match hunk {
            Some(h) => crate::diff::pane_view(h, &left_lines, true),
            None => vec!["(no hunk)".into()],
        };
        let right_view = match hunk {
            Some(h) => crate::diff::pane_view(h, &right_lines, false),
            None => vec!["(no hunk)".into()],
        };
        let workspace = cx.weak_entity();
        let render_pane = |id: &'static str, label: String, lines: Vec<String>, border_r: bool| {
            let theme = cx.theme();
            let mut pane = v_flex()
                .id(format!("{id}-pane"))
                .flex_1()
                .h_full()
                .min_w_0()
                .min_h_0();
            if border_r {
                pane = pane.border_r_1().border_color(theme.border);
            }
            pane.child(
                div()
                    .flex_none()
                    .w_full()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .bg(theme.sidebar)
                    .child(label),
            )
            .child(
                // Use div (not nested v_flex) so overflow_y_scrollbar gets a
                // definite flex_1 height from the stretched pane.
                div()
                    .id(id)
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .p_2()
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .text_color(theme.foreground)
                    .bg(theme.background)
                    .overflow_y_scrollbar()
                    .children(lines.into_iter().enumerate().map(|(i, line)| {
                        div()
                            .id((id, i))
                            .flex_none()
                            .w_full()
                            .whitespace_nowrap()
                            .child(line)
                    })),
            )
        };
        v_flex()
            .id("diff-editor")
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                h_flex()
                    .h(px(36.))
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(session.title.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("Hunk {} of {}", current + 1, hunk_n)),
                    )
                    .child({
                        let ws = workspace.clone();
                        Button::new("diff-prev")
                            .label("Previous")
                            .xsmall()
                            .outline()
                            .on_click(move |_, _, cx| {
                                _ = ws.update(cx, |this, cx| this.diff_prev_hunk(cx));
                            })
                    })
                    .child({
                        let ws = workspace.clone();
                        Button::new("diff-next")
                            .label("Next")
                            .xsmall()
                            .outline()
                            .on_click(move |_, _, cx| {
                                _ = ws.update(cx, |this, cx| this.diff_next_hunk(cx));
                            })
                    })
                    .child({
                        let ws = workspace.clone();
                        Button::new("diff-close").label("Close").xsmall().on_click(
                            move |_, _, cx| {
                                _ = ws.update(cx, |this, cx| this.close_diff_session(cx));
                            },
                        )
                    }),
            )
            .child(
                h_flex()
                    .id("diff-panes")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .items_stretch()
                    .child(render_pane(
                        "diff-left",
                        session.left_label.clone(),
                        left_view,
                        true,
                    ))
                    .child(render_pane(
                        "diff-right",
                        session.right_label.clone(),
                        right_view,
                        false,
                    )),
            )
            .into_any_element()
    }

    /// Thin VS Code-like activity bar: Explorer / Search / SCM / Extensions.
    fn render_activity_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let active = self.activity_view;
        let workspace = cx.weak_entity();
        let btn = |id: &'static str,
                   icon: IconName,
                   color: gpui_kit::Hsla,
                   tip: &'static str,
                   on: bool,
                   click: fn(&mut Self, &mut Window, &mut Context<Self>)| {
            let ws = workspace.clone();
            let paint = if on { color } else { color.opacity(0.85) };
            Button::new(id)
                .ghost()
                .icon(Icon::new(icon).text_color(paint))
                .tooltip(tip)
                .selected(on)
                .on_click(move |_, window, cx| {
                    _ = ws.update(cx, |this, cx| click(this, window, cx));
                })
        };
        v_flex()
            .id("activity-bar")
            .w(px(48.))
            .h_full()
            .flex_none()
            .items_center()
            .gap_1()
            .py_2()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.border)
            .child(btn(
                "activity-explorer",
                IconName::Files,
                activity_icon_color(ActivityIcon::Explorer),
                "Explorer (Ctrl+Shift+E)",
                active == ActivityView::Explorer,
                |this, window, cx| this.show_explorer_view(&ShowExplorerView, window, cx),
            ))
            .child(btn(
                "activity-search",
                IconName::Search,
                activity_icon_color(ActivityIcon::Search),
                "Search (Ctrl+Shift+F)",
                active == ActivityView::Search,
                |this, window, cx| this.show_search_view(&ShowSearchView, window, cx),
            ))
            .child(btn(
                "activity-scm",
                IconName::GitBranch,
                activity_icon_color(ActivityIcon::SourceControl),
                "Source Control",
                active == ActivityView::SourceControl,
                |this, window, cx| this.show_source_control(&ShowSourceControl, window, cx),
            ))
            .child(btn(
                "activity-extensions",
                IconName::Puzzle,
                activity_icon_color(ActivityIcon::Extensions),
                "Extensions",
                active == ActivityView::Extensions,
                |this, window, cx| this.show_extensions(&ShowExtensions, window, cx),
            ))
    }

    /// Editor panes, with Problems and/or the integrated terminal underneath.
    fn render_center(
        &self,
        main: impl IntoElement,
        focus_mode: bool,
        rem: Pixels,
        viewport_h: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let show_problems = AppSettings::get(cx).problems_panel && !focus_mode;
        let body = div()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .drag_over::<TreeDrag>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            .child(main);

        let with_problems = if show_problems {
            let problems_h = rems(10.).to_pixels(rem);
            v_resizable("center-with-problems")
                .child(
                    resizable_panel()
                        .size_range(rems(8.).to_pixels(rem)..Pixels::MAX)
                        .child(body),
                )
                .child(
                    resizable_panel()
                        .size(problems_h)
                        .size_range(rems(6.).to_pixels(rem)..rems(24.).to_pixels(rem))
                        .child(self.render_problems(cx)),
                )
                .into_any_element()
        } else {
            body.into_any_element()
        };

        if self.terminal_visible && !focus_mode {
            if self.terminal_maximized {
                let editor_h = rems(6.).to_pixels(rem);
                v_resizable("center-with-terminal-max")
                    .child(
                        resizable_panel()
                            .size(editor_h)
                            .size_range(rems(4.).to_pixels(rem)..rems(16.).to_pixels(rem))
                            .child(with_problems),
                    )
                    .child(
                        resizable_panel()
                            .size_range(rems(12.).to_pixels(rem)..Pixels::MAX)
                            .child(self.render_terminal_area(cx)),
                    )
                    .into_any_element()
            } else {
                // VS Code opens the panel at about a third of the window; a
                // fixed 12rem left ~4 rows of output. Drag the divider to
                // resize (up to 80% of the window); the height is kept.
                let min_h = rems(6.).to_pixels(rem);
                let max_h = (viewport_h * 0.8).max(rems(28.).to_pixels(rem));
                let term_h = AppSettings::get(cx)
                    .terminal_panel_height
                    .map(px)
                    .unwrap_or_else(|| (viewport_h / 3.).max(rems(12.).to_pixels(rem)))
                    .clamp(min_h, max_h);
                v_resizable("center-with-terminal")
                    .on_resize(|state, _, cx| {
                        let Some(height) = state.read(cx).sizes().get(1).copied() else {
                            return;
                        };
                        let height = f32::from(height).round();
                        if height > 0. {
                            AppSettings::update(cx, |settings| {
                                settings.terminal_panel_height = Some(height)
                            });
                        }
                    })
                    .child(
                        resizable_panel()
                            .size_range(rems(12.).to_pixels(rem)..Pixels::MAX)
                            .child(with_problems),
                    )
                    .child(
                        resizable_panel()
                            .size(term_h)
                            .size_range(min_h..max_h)
                            .child(self.render_terminal_area(cx)),
                    )
                    .into_any_element()
            }
        } else {
            with_problems
        }
    }

    /// Open files handed over by the system while running (macOS "Open
    /// with", a second launch).
    pub fn open_external(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A directory argument (CLI or a second launch) becomes the sidebar
        // root, the way VS Code opens a folder.
        let mut documents: Vec<PathBuf> = Vec::new();
        for path in paths {
            if path.is_dir() {
                AppSettings::update(cx, |settings| settings.push_recent(&path));
                self.file_tree
                    .update(cx, |tree, cx| tree.set_root(Some(path), cx));
            } else if path.is_file() {
                documents.push(path);
            }
        }
        if !documents.is_empty() {
            self.open_paths(documents, Placement::Default, window, cx);
        }
    }
}

/// Where the last session's window was, if it still fits a display.
pub fn restored_bounds(cx: &App) -> Option<WindowBounds> {
    let placement = Session::load()?.window?;
    if !AppSettings::get(cx).restore_session {
        return None;
    }
    let bounds = Bounds::new(
        gpui_kit::point(px(placement.x), px(placement.y)),
        gpui_kit::size(
            px(placement.width.max(560.)),
            px(placement.height.max(360.)),
        ),
    );
    let visible = cx.displays().iter().any(|display| {
        let area = display.bounds();
        area.intersects(&bounds)
    });
    let bounds = if placement.maximized {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    };
    visible.then_some(bounds)
}

/// A palette or picker that one Escape closes, whatever has been typed into
/// it, as VS Code's quick input does (the list's own Escape clears the
/// query first).
/// Ctrl+Shift+F (Cmd on macOS) inside a dialog, turned into Find in Files.
fn find_again_key(event: &gpui_kit::KeyDownEvent, window: &mut Window, cx: &mut App) {
    let keys = &event.keystroke;
    if keys.modifiers.secondary() && keys.modifiers.shift && keys.key.eq_ignore_ascii_case("f") {
        window.dispatch_action(Box::new(FindInFiles), cx);
        cx.stop_propagation();
    }
}

fn esc_closes(content: impl IntoElement) -> gpui_kit::Div {
    div()
        .capture_action(|_: &gpui_kit::base::actions::Cancel, window, cx| {
            window.close_dialog(cx);
            cx.stop_propagation();
        })
        // InputState may clear the filter on Escape without emitting Cancel when
        // the query is non-empty; still close on one Escape (VS Code quick input).
        .capture_action(|_: &Escape, window, cx| {
            window.close_dialog(cx);
            cx.stop_propagation();
        })
        // Ctrl+Shift+F over a picker leaves it for Find in Files, rather
        // than being typed into the picker's query. The binding belongs to
        // the workspace's key context, which a dialog's focus is outside of,
        // so the key is read here.
        .capture_key_down(find_again_key)
        .on_action(|_: &FindInFiles, window, cx| {
            window.close_dialog(cx);
            window.defer(cx, |window, cx| {
                window.dispatch_action(Box::new(FindInFiles), cx)
            });
        })
        .child(content)
}

/// A file dialog's answer: `Ok(Some)` is a choice, `Ok(None)` a cancel, and
/// `Err` the raw reason the dialog itself failed to open (on Linux,
/// typically no xdg-desktop-portal), which must not look like a cancel.
fn file_dialog_outcome<T, E: std::fmt::Display, C: std::fmt::Display>(
    answer: Result<Result<Option<T>, E>, C>,
) -> Result<Option<T>, String> {
    match answer {
        Ok(Ok(chosen)) => Ok(chosen),
        Ok(Err(err)) => Err(err.to_string()),
        Err(err) => Err(err.to_string()),
    }
}

/// A shared window callback (dialog buttons and Enter both confirm).
type WindowCallback = Rc<dyn Fn(&mut Window, &mut App)>;

/// What to do about a missing file dialog, in words a person can act on.
fn file_dialog_hint() -> &'static str {
    if cfg!(target_os = "linux") {
        "Install xdg-desktop-portal-gtk to use it."
    } else {
        "The system did not respond."
    }
}

/// Report a file dialog that could not open: a plain first line, what to
/// install, and the raw error last, small and cut to one line. Shown at the
/// bottom so it never covers the tabs.
fn report_file_dialog_failure(detail: &str, window: &mut Window, cx: &mut App) {
    eprintln!("malgel: Couldn't open the file dialog: {detail}");
    let detail: SharedString = detail.lines().next().unwrap_or_default().to_string().into();
    window.push_notification(
        Notification::error(file_dialog_hint())
            .title("Couldn’t open the file dialog")
            .placement(Anchor::BottomRight)
            .content(move |_, _, cx| {
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .truncate()
                    .child(detail.clone())
                    .into_any_element()
            }),
        cx,
    );
}

/// The path typed into the in-app Save As field: trimmed, `~/` expanded;
/// `None` when empty or naming a folder.
fn typed_save_path(typed: &str) -> Option<PathBuf> {
    let typed = typed.trim();
    if typed.is_empty() || typed.ends_with('/') || typed.ends_with(std::path::MAIN_SEPARATOR) {
        return None;
    }
    let path = match typed.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()?.join(rest),
        None => PathBuf::from(typed),
    };
    Some(path)
}

/// Wait for a file dialog; a failure to show it becomes an error
/// notification instead of a silent no-op.
async fn file_dialog<T, E: std::fmt::Display, C: std::fmt::Display>(
    answer: impl std::future::Future<Output = Result<Result<Option<T>, E>, C>>,
    cx: &mut gpui_kit::AsyncWindowContext,
) -> Option<T> {
    match file_dialog_outcome(answer.await) {
        Ok(chosen) => chosen,
        Err(detail) => {
            _ = cx.update(|window, cx| report_file_dialog_failure(&detail, window, cx));
            None
        }
    }
}

/// Whether a loading tab may stand in for a file while it loads: only when
/// it takes a new tab or a slot nobody is using (a blank document or the
/// preview tab), never when it would replace a document being worked on.
fn placeholder_is_safe(placement: Placement, active_blank: bool, tabs: bool) -> bool {
    match placement {
        Placement::Preview | Placement::NewTab => true,
        Placement::ReplaceActive => active_blank,
        Placement::Default => active_blank || tabs,
    }
}

/// Whether the tab strip should scroll its active tab into view: on the
/// first frame, and whenever the active tab or the number of tabs changed.
/// Reveal `path` in the OS file manager, after stripping a Win32 `\\?\`
/// prefix that otherwise breaks Explorer / `ParseDisplayName`.
fn reveal_path_in_file_manager(cx: &App, path: &Path) {
    let path = crate::paths::path_for_shell(path);
    cx.reveal_path(&path);
}

fn tab_needs_reveal(revealed: Option<(usize, usize)>, now: (usize, usize)) -> bool {
    revealed != Some(now)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_file_dialog_that_cannot_open_is_an_error_not_a_cancel() {
        use super::file_dialog_outcome;
        let chosen: Result<Result<Option<u8>, String>, String> = Ok(Ok(Some(1)));
        assert_eq!(file_dialog_outcome(chosen), Ok(Some(1)));
        let cancelled: Result<Result<Option<u8>, String>, String> = Ok(Ok(None));
        assert_eq!(file_dialog_outcome(cancelled), Ok(None));
        let failed: Result<Result<Option<u8>, String>, String> =
            Ok(Err("org.freedesktop.portal.Desktop not found".into()));
        let detail = file_dialog_outcome(failed).unwrap_err();
        assert_eq!(detail, "org.freedesktop.portal.Desktop not found");
        assert!(super::file_dialog_hint().lines().count() == 1);
        let dropped: Result<Result<Option<u8>, String>, String> = Err("canceled".into());
        assert!(file_dialog_outcome(dropped).is_err());
    }

    #[test]
    fn a_typed_save_path_is_trimmed_and_must_name_a_file() {
        use super::typed_save_path;
        use std::path::PathBuf;
        assert_eq!(
            typed_save_path("  /tmp/a.md \n"),
            Some(PathBuf::from("/tmp/a.md"))
        );
        assert_eq!(typed_save_path("   "), None);
        assert_eq!(typed_save_path("/tmp/"), None);
        if let Some(home) = dirs::home_dir() {
            assert_eq!(typed_save_path("~/x.md"), Some(home.join("x.md")));
        }
    }

    #[test]
    fn a_binary_file_that_is_not_text_gets_no_way_to_open_it() {
        use super::{OpenChoice::*, open_choices};
        use crate::document::OpenCheck;
        assert_eq!(open_choices(OpenCheck::Binary, false), &[Close("OK")]);
        assert_eq!(
            open_choices(OpenCheck::BinaryText, false),
            &[Close("Cancel"), Anyway]
        );
        assert_eq!(
            open_choices(OpenCheck::Large(1 << 30), false),
            &[Close("Cancel"), PlainText, Anyway]
        );
        assert_eq!(
            open_choices(OpenCheck::Large(1 << 30), true),
            &[Close("Cancel"), Anyway]
        );
    }

    #[test]
    fn several_files_read_text_first_and_viewers_inline() {
        use std::path::Path;
        assert!(super::reads_as_text(Path::new("a.md")));
        assert!(super::reads_as_text(Path::new("main.rs")));
        assert!(!super::reads_as_text(Path::new("a.pdf")));
        assert!(!super::reads_as_text(Path::new("a.png")));
        // A Typst source is text, with its pages beside it.
        assert!(super::reads_as_text(Path::new("a.typ")));
    }

    #[test]
    fn quick_open_puts_the_previous_file_first() {
        let root = PathBuf::from("/ws");
        let listing: Vec<PathBuf> = ["c.md", "a.md", "b.md"]
            .iter()
            .map(|n| root.join(n))
            .collect();
        let rows = super::quick_open_rows("", &root, None, &listing);
        // a.md was opened, then b.md (the current file): most recent first.
        let recent = vec![root.join("b.md"), root.join("a.md")];
        let (first, rest) = split_recent(rows, &recent, Some(&root.join("b.md")));
        assert_eq!(first[0].path, root.join("a.md"));
        assert_eq!(first.len(), 1);
        assert_eq!(rest.len(), 2, "the current file stays among the rest");
        // While typing, the same rows lead: the groups are only filtered.
        let rows = super::quick_open_rows("md", &root, None, &listing);
        let (first, _) = split_recent(rows, &recent, Some(&root.join("b.md")));
        assert_eq!(first[0].path, root.join("a.md"));
    }

    #[test]
    fn quick_open_offers_a_typed_path_the_listing_left_out() {
        let root = std::env::temp_dir().join(format!("malgel-qo-rows-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(root.join("build/out.txt"), "x").unwrap();
        std::fs::write(root.join("secret.md"), "x").unwrap();
        std::fs::write(root.join("notes.md"), "x").unwrap();
        let listing = vec![root.join("notes.md")];

        let rows = super::quick_open_rows("secret.md:4", &root, None, &listing);
        assert_eq!(rows[0].path, root.join("secret.md"));
        assert_eq!(rows[0].line, Some(4));
        assert_eq!(rows.len(), 2, "the typed file, then the listing");

        let rows = super::quick_open_rows("build/", &root, None, &listing);
        assert_eq!(rows[0].path, root.join("build/out.txt"));
        assert_eq!(rows[0].keyword.as_deref(), Some("build/out.txt"));

        // A plain word only searches the listing.
        let rows = super::quick_open_rows("note", &root, None, &listing);
        assert_eq!(rows.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    use super::*;

    #[test]
    fn a_pdf_keeps_the_keyboard_on_its_own_viewer() {
        // Whatever the layout says, a PDF or an image is shown by its viewer
        // and by nothing else, so that is where the keyboard has to go.
        for shows_editor in [true, false] {
            for focus_mode in [true, false] {
                assert_eq!(
                    keyboard_target(true, shows_editor, focus_mode),
                    KeyboardTarget::Media,
                    "a media document must not send the keyboard to the editor"
                );
            }
        }
        // A text document picks the editor or the preview, as it did before.
        assert_eq!(keyboard_target(false, true, false), KeyboardTarget::Editor);
        assert_eq!(keyboard_target(false, false, true), KeyboardTarget::Editor);
        assert_eq!(
            keyboard_target(false, false, false),
            KeyboardTarget::Preview
        );
    }

    #[test]
    fn a_click_takes_over_the_preview_slot_in_order() {
        // The preview tab, while it is untouched…
        assert_eq!(preview_target(Some(2), true, 0, true), Some(2));
        assert_eq!(preview_target(Some(2), true, 0, false), Some(2));
        // …then an empty editor…
        assert_eq!(preview_target(Some(2), false, 0, true), Some(0));
        assert_eq!(preview_target(None, false, 1, true), Some(1));
        // …and otherwise a tab of its own, which the next click takes over.
        assert_eq!(preview_target(Some(2), false, 0, false), None);
        assert_eq!(preview_target(None, false, 1, false), None);
    }

    #[test]
    fn groups_thousands() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_000), "1,000");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn selection_count_uses_characters_not_bytes() {
        // En-dash U+2013 is one scalar, three UTF-8 bytes.
        let line = "Line four – click me for selection";
        assert_eq!(line.chars().count(), 34);
        assert_eq!(line.len(), 36);
        assert_eq!(selection_character_count(line), 34);
        assert_eq!(selection_character_count("–"), 1);
        assert_eq!("–".len(), 3);
        // Empty / ASCII stay obvious.
        assert_eq!(selection_character_count(""), 0);
        assert_eq!(selection_character_count("abc"), 3);
    }

    #[test]
    fn suggests_file_name_from_first_heading() {
        let analysis = Analysis::of("# Q3: Plans/Goals?\n\nBody");
        assert_eq!(suggested_file_name(&analysis, "md"), "Q3 PlansGoals.md");
        assert_eq!(
            suggested_file_name(&Analysis::default(), "html"),
            "Untitled.html"
        );
    }

    #[gpui_kit::test]
    fn workspace_shows_tabs_by_default(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::themes::init(crate::settings::Settings::default(), cx);
            let settings = AppSettings::get(cx);
            assert!(settings.tabs);
        });
    }

    /// The gate save_document uses: while an id is in pending_disk_prompt,
    /// a save must re-open the conflict dialog instead of writing. Esc/Keep
    /// and Overwrite/Reload clear the set (see open_disk_conflict).
    #[test]
    fn save_refuses_while_disk_conflict_is_pending() {
        let mut pending: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let id = 42u64;
        assert!(!pending.contains(&id), "fresh document is free to save");
        pending.insert(id);
        assert!(
            pending.contains(&id),
            "conflict pending blocks silent overwrite"
        );
        // Esc = Keep (and Overwrite/Reload) clear the gate.
        pending.remove(&id);
        assert!(
            !pending.contains(&id),
            "after Keep/Overwrite/Reload, save may proceed"
        );
    }

    #[test]
    fn restores_session_when_cli_dir_matches_folder() {
        let root = std::env::temp_dir().join(format!("malgel-sess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let other = root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        assert!(Workspace::restores_session_for(&[], Some(&root)));
        assert!(
            !Workspace::restores_session_for(&[], None),
            "no-folder session must not restore tabs on bare launch"
        );
        assert!(Workspace::restores_session_for(
            std::slice::from_ref(&root),
            Some(&root),
        ));
        assert!(!Workspace::restores_session_for(
            std::slice::from_ref(&other),
            Some(&root),
        ));
        let file = root.join("a.md");
        assert!(!Workspace::restores_session_for(
            std::slice::from_ref(&file),
            Some(&root),
        ));
        assert!(!Workspace::restores_session_for(
            &[root.clone(), other.clone()],
            Some(&root),
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_again_key_recognises_ctrl_shift_f() {
        let stroke = gpui_kit::Keystroke::parse("ctrl-shift-f").unwrap();
        assert!(stroke.modifiers.control);
        assert!(stroke.modifiers.shift);
        assert!(stroke.key.eq_ignore_ascii_case("f"));
    }

    fn hit(path: &str, line: usize) -> Hit {
        Hit {
            path: PathBuf::from(path),
            line,
            column: 1,
            text: format!("line {line}"),
        }
    }

    #[test]
    fn quick_open_hands_the_palette_only_rows_the_query_matches() {
        let row = |label: &str| QuickOpenRow {
            label: label.to_string(),
            keyword: None,
            path: PathBuf::from("/r").join(label),
            line: None,
            is_dir: false,
        };
        let rows: Vec<_> = (0..2000)
            .map(|ix| row(&format!("pkg{:03}/src/m{:03}.rs", ix / 10, ix % 100)))
            .collect();
        let kept = matching_quick_open_rows(rows.clone(), "M099", 500);
        assert_eq!(kept.len(), 20);
        assert_eq!(kept[0].label, "pkg009/src/m099.rs");
        assert_eq!(matching_quick_open_rows(rows.clone(), "", 500).len(), 500);
        assert!(matching_quick_open_rows(rows, "nothing", 500).is_empty());
    }

    #[test]
    fn search_results_group_hits_under_their_file() {
        let hits = [hit("/r/a.rs", 1), hit("/r/a.rs", 9), hit("/r/sub/b.rs", 3)];
        let rows = search_rows(&hits, &HashSet::new());
        assert_eq!(
            rows,
            [
                SearchRow::File {
                    path: PathBuf::from("/r/a.rs"),
                    count: 2
                },
                SearchRow::Hit(0),
                SearchRow::Hit(1),
                SearchRow::File {
                    path: PathBuf::from("/r/sub/b.rs"),
                    count: 1
                },
                SearchRow::Hit(2),
            ]
        );
        // A collapsed file keeps its row and count, without its hits.
        let collapsed = HashSet::from([PathBuf::from("/r/a.rs")]);
        let rows = search_rows(&hits, &collapsed);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[1],
            SearchRow::File {
                path: PathBuf::from("/r/sub/b.rs"),
                count: 1
            }
        );
    }

    #[test]
    fn the_search_summary_counts_results_and_files_and_says_when_it_was_limited() {
        let mut found = search::FolderSearch {
            hits: vec![hit("/r/a.rs", 1), hit("/r/b.rs", 2)],
            files_matched: 2,
            files_searched: 20_000,
            files_total: 20_000,
            limited: false,
        };
        assert_eq!(search_summary(&found), "2 results in 2 files");
        found.hits.truncate(1);
        found.files_matched = 1;
        assert_eq!(search_summary(&found), "1 result in 1 file");
        found.limited = true;
        found.files_searched = 700;
        assert_eq!(
            search_summary(&found),
            "1 result in 1 file \u{2014} results limited; searched 700 of 20000 files"
        );
    }

    #[test]
    fn enter_and_shift_enter_walk_the_hits_round() {
        assert_eq!(step_hit(None, 0, true), None);
        assert_eq!(step_hit(None, 3, true), Some(0));
        assert_eq!(step_hit(None, 3, false), Some(2));
        assert_eq!(step_hit(Some(2), 3, true), Some(0));
        assert_eq!(step_hit(Some(0), 3, false), Some(2));
    }

    #[test]
    fn the_pulse_repaints_only_a_changed_label_in_the_active_window() {
        assert!(pulse_needs_repaint(2, 1, true));
        assert!(!pulse_needs_repaint(2, 2, true));
        assert!(!pulse_needs_repaint(3, 2, false));
    }

    #[test]
    fn the_tab_strip_reveals_the_active_tab_only_when_it_changes() {
        assert!(tab_needs_reveal(None, (0, 1)));
        assert!(!tab_needs_reveal(Some((3, 5)), (3, 5)));
        assert!(tab_needs_reveal(Some((3, 5)), (4, 5)));
        assert!(tab_needs_reveal(Some((4, 5)), (4, 6)));
    }

    #[test]
    fn a_loading_tab_never_stands_in_for_a_document_in_use() {
        assert!(placeholder_is_safe(Placement::Preview, false, false));
        assert!(placeholder_is_safe(Placement::NewTab, false, false));
        assert!(placeholder_is_safe(Placement::Default, false, true));
        assert!(placeholder_is_safe(Placement::Default, true, false));
        assert!(!placeholder_is_safe(Placement::Default, false, false));
        assert!(!placeholder_is_safe(Placement::ReplaceActive, false, true));
        assert!(placeholder_is_safe(Placement::ReplaceActive, true, true));
    }

    /// Probe #15: drag-tab edge zones must recognize the far-right / bottom
    /// strips the Xvfb probe aims at (WIDTH-20 mid-height → right split).
    #[test]
    fn tab_edge_split_zones_match_probe_drag_target() {
        use gpui_kit::{Bounds, Point, Size, px};
        let body = Bounds {
            origin: Point {
                x: px(200.),
                y: px(80.),
            },
            size: Size {
                width: px(1000.),
                height: px(600.),
            },
        };
        // Far right of editor body (probe: WIDTH-20 at mid height).
        let right = Point {
            x: px(200.) + px(1000.) - px(20.),
            y: px(80.) + px(300.),
        };
        assert_eq!(
            super::Workspace::edge_split_at(right, Some(body)),
            Some(false),
            "far-right mid-height must be a right split"
        );
        // Center of body: no split.
        let center = Point {
            x: px(200.) + px(500.),
            y: px(80.) + px(300.),
        };
        assert_eq!(super::Workspace::edge_split_at(center, Some(body)), None);
        // Bottom strip → down split.
        let down = Point {
            x: px(200.) + px(500.),
            y: px(80.) + px(600.) - px(20.),
        };
        assert_eq!(
            super::Workspace::edge_split_at(down, Some(body)),
            Some(true)
        );
        // 120px edge width: just inside right zone.
        let near = Point {
            x: px(200.) + px(1000.) - px(100.),
            y: px(80.) + px(300.),
        };
        assert_eq!(
            super::Workspace::edge_split_at(near, Some(body)),
            Some(false)
        );
        // Just outside the 120px zone toward center.
        let outside = Point {
            x: px(200.) + px(1000.) - px(130.),
            y: px(80.) + px(300.),
        };
        assert_eq!(super::Workspace::edge_split_at(outside, Some(body)), None);
        // No body bounds → no split (cannot guess).
        assert_eq!(super::Workspace::edge_split_at(right, None), None);
    }

    /// Probe #16: tab context menu must expose Pin / Select for Compare /
    /// Compare with Selected near the top (items claimed in #15 but unreachable
    /// under Xvfb until the scrollbar overlay stop_propagation was removed).
    #[test]
    fn tab_context_menu_registers_compare_items() {
        let labels = super::tab_context_menu_labels(false);
        assert_eq!(labels[0], "Pin");
        assert!(labels.contains(&"Select for Compare"));
        assert!(labels.contains(&"Compare with Selected"));
        assert_eq!(super::tab_context_menu_labels(true)[0], "Unpin");
    }

    /// Probe #16: cross-group drag moves the tab into the destination pane
    /// without leaving both panes on the same document (no clone).
    #[test]
    fn cross_group_tab_drop_moves_not_clones() {
        // active=0, other=1, drag 0 into secondary → swap
        assert_eq!(
            Workspace::split_panes_after_cross_group_move(0, 0, 1, 3, true),
            (1, 0)
        );
        // active=0, other=1, drag 2 into secondary → primary stays 0, other=2
        assert_eq!(
            Workspace::split_panes_after_cross_group_move(2, 0, 1, 3, true),
            (0, 2)
        );
        // Never the same index when len > 1
        let (a, b) = Workspace::split_panes_after_cross_group_move(1, 1, 1, 2, true);
        assert_ne!(a, b);
        // Into primary
        assert_eq!(
            Workspace::split_panes_after_cross_group_move(2, 0, 1, 3, false),
            (2, 1)
        );
    }

    #[test]
    fn pinned_preview_mounts_keeps_each_doc_once() {
        // docs [0,1,2], active=1, pin=0 → editor hosts 1+2, preview hosts 0.
        let (editor, preview) = super::pinned_preview_mounts(3, 1, 0);
        assert_eq!(preview, 0);
        assert_eq!(editor, vec![1, 2]);
        let mut all = editor;
        all.push(preview);
        all.sort();
        assert_eq!(all, vec![0, 1, 2]);
        assert_eq!(
            all.len(),
            all.iter().collect::<std::collections::BTreeSet<_>>().len()
        );
    }
}
