//! The window: title bar, open documents (as tabs when enabled), the file
//! and outline sidebars, and the status bar. It routes commands to the
//! document showing, asks before unsaved changes are lost, notices files
//! changed by other programs, and remembers the session.

use std::{
    cell::Cell,
    collections::HashSet,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use gpui_kit::{
    Anchor, App, AppContext as _, Bounds, ClipboardItem, Context, DragMoveEvent, Entity, EntityId,
    ExternalPaths, FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement,
    MouseButton, MouseDownEvent, ParentElement as _, PathPromptOptions, Pixels, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window,
    WindowBounds,
    assets::IconName,
    base::Disableable as _,
    component::{
        ActiveTheme as _, Icon, IndexPath, Selectable as _, Sizable as _, TitleBar, WindowExt as _,
        button::{Button, ButtonGroup, ButtonVariants as _},
        checkbox::Checkbox,
        command::{Command, CommandItem, CommandState},
        dialog::DialogFooter,
        h_flex,
        input::Escape,
        link::Link,
        menu::{AppMenuBar, ContextMenuExt as _, DropdownMenu as _, PopupMenuItem},
        notification::Notification,
        radio::RadioGroup,
        resizable::{h_resizable, resizable_panel},
        scroll::ScrollableElement as _,
        status_bar::StatusBar,
        switch::Switch,
        v_flex,
    },
    div,
    prelude::FluentBuilder as _,
    px, rems,
};

use crate::{
    actions::*,
    analysis::{Analysis, content_hash},
    commands, default_app, diagram_viewer,
    document::{self, LineEnding, display_name},
    document_view::{DocumentEvent, DocumentView},
    docx, export,
    file_icon::file_icon,
    file_tree::{FileTree, FileTreeEvent, TreeDrag, invert_key_held},
    format,
    links::{self, LinkTarget},
    pdf::{self, Paper},
    quick_open, rich_copy,
    search::{self, Hit},
    session::{self, DiskStamp, Session, SessionDocument, WindowPlacement},
    settings::{Appearance, Layout, LinkPlacement, clamp_font_size},
    spell,
    themes::{self, AppSettings},
    typst_preview,
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
}

/// Which view of the active document should hold the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyboardTarget {
    /// The PDF or image viewer, which stands in for both of the others.
    Media,
    Editor,
    Preview,
}

/// A PDF or an image is rendered *instead of* the editor and the preview, so
/// the keyboard has to go to that viewer. Focusing the editor anyway points
/// the keyboard at an element that is not in the render tree — no key binding
/// matches it and every shortcut stops working, which is what happened while a
/// PDF was open.
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

/// Whether opening `path` reads it as text, rather than compiling it
/// (Typst) or showing it in a viewer (PDF, image).
fn reads_as_text(path: &std::path::Path) -> bool {
    !document::is_typst_path(path) && !document::is_pdf_path(path) && !document::is_image_path(path)
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
    Rename,
}

/// Where a document being opened should land once it has loaded.
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
    /// A Typst preview whose source — or a file it named — changed, so
    /// the pages are recompiled.
    Recompiled,
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
    go_to_line: Entity<CommandState>,
    /// The one-line prompt New File / New Folder / Rename ask their name in.
    tree_prompt: Entity<CommandState>,
    /// Find in Files (Ctrl+Shift+F): one for the query it asks for, one for
    /// the hits it lists.
    find_query: Entity<CommandState>,
    find_results: Entity<CommandState>,
    file_tree: Entity<FileTree>,
    app_menu_bar: Entity<AppMenuBar>,

    window_title: String,
    closing: bool,
    /// Documents whose unsaved changes the user chose to discard while
    /// closing.
    discarded: HashSet<EntityId>,
    /// Files changed on disk that are waiting for the user to decide.
    pending_disk_prompt: HashSet<EntityId>,
    /// The paths of editors closed recently, newest last, so Ctrl+Shift+T
    /// can bring the last one back the way VS Code does.
    closed_documents: Vec<PathBuf>,
    /// Tabs held open: Close Other Tabs and Close All Editors leave them
    /// alone, and their close box becomes a pin, as in VS Code.
    pinned: HashSet<EntityId>,
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
    /// Whether the invert key is held while a tab drags: the drop then
    /// duplicates the tab instead of moving it, as in VS Code.
    tab_drag_copy: Cell<bool>,
    _watch_task: Task<()>,
    document_subscriptions: Vec<(EntityId, Vec<Subscription>)>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    /// A workspace restoring the last session (when enabled) and opening
    /// `paths`.
    pub fn new(
        paths: Vec<PathBuf>,
        app_menu_bar: Entity<AppMenuBar>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = AppSettings::get(cx).clone();
        let headings = cx.new(|cx| CommandState::new(window, cx));
        let quick_open = cx.new(|cx| CommandState::new(window, cx));
        let command_palette = cx.new(|cx| CommandState::new(window, cx));
        let go_to_line = cx.new(|cx| CommandState::new(window, cx));
        let tree_prompt = cx.new(|cx| CommandState::new(window, cx));
        let find_query = cx.new(|cx| CommandState::new(window, cx));
        let find_results = cx.new(|cx| CommandState::new(window, cx));
        let file_tree = cx.new(FileTree::new);

        let subscriptions = vec![
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
                    .update_in(cx, |this, window, cx| this.check_disk(window, cx))
                    .is_err()
                {
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
            go_to_line,
            tree_prompt,
            find_query,
            find_results,
            file_tree,
            app_menu_bar,
            window_title: String::new(),
            closing: false,
            discarded: HashSet::new(),
            pending_disk_prompt: HashSet::new(),
            closed_documents: Vec::new(),
            pinned: HashSet::new(),
            nav_history: Vec::new(),
            nav_index: 0,
            last_session: None,
            tab_insertion: None,
            tab_bar_bounds: Cell::new(None),
            tab_drag_copy: Cell::new(false),
            _watch_task: watch_task,
            document_subscriptions: Vec::new(),
            _subscriptions: subscriptions,
        };
        this.restore(paths, &settings, window, cx);
        this
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
        let mut deferred_open = paths.clone();

        for (ix, entry) in previous.documents.iter().enumerate() {
            let recovery = entry.recovery.as_deref().and_then(|id| {
                referenced.insert(id.to_string());
                session::read_recovery(id).map(|recovery| (id.to_string(), recovery))
            });
            // Unsaved changes always come back; saved documents only when
            // the session is restored (just the active one without tabs).
            let wanted = settings.restore_session
                && paths.is_empty()
                && (settings.tabs || ix == previous.active);
            let doc = match recovery {
                Some((id, recovery)) => Some(self.restored_document(id, recovery, window, cx)),
                None if wanted => entry.path.as_ref().and_then(|path| {
                    // Typst and PDF files are not text documents; open them
                    // after the window finishes building via their own path.
                    if document::is_typst_path(path)
                        || document::is_pdf_path(path)
                        || document::is_image_path(path)
                    {
                        deferred_open.push(path.clone());
                        return None;
                    }
                    let loaded = document::load(path).ok()?;
                    Some(cx.new(|cx| {
                        DocumentView::new(
                            loaded.text,
                            Some(path.clone()),
                            loaded.line_ending,
                            window,
                            cx,
                        )
                    }))
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

        if settings.restore_session {
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
        if !deferred_open.is_empty() {
            cx.defer_in(window, move |this, window, cx| {
                this.open_paths(deferred_open, Placement::Default, window, cx);
            });
        }
        self.focus_active(window, cx);
        self.sync_file_tree(cx);
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
            cx.subscribe_in(doc, window, |_, _, event, window, cx| match event {
                DocumentEvent::Notify(message) => {
                    window.push_notification(Notification::info(message.clone()), cx)
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
        self.unpreview(id);
        if self.documents.is_empty() {
            let doc =
                cx.new(|cx| DocumentView::new(String::new(), None, LineEnding::Lf, window, cx));
            self.add_document(doc, false, window, cx);
        }
        if self.active > ix || self.active >= self.documents.len() {
            self.active = self.active.saturating_sub(1);
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
        if self.documents.is_empty() {
            return;
        }
        self.active = ix.min(self.documents.len() - 1);
        if let Some(path) = self.documents[self.active].read(cx).path().cloned() {
            self.record_navigation(path);
        }
        self.focus_active(window, cx);
        self.sync_file_tree(cx);
        self.save_session(window, cx);
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
        let doc = self.active_document().read(cx);
        let shows_editor = AppSettings::get(cx).layout.shows_editor();
        let focus_mode = AppSettings::get(cx).focus_mode;
        let focus = match keyboard_target(doc.is_media(), shows_editor, focus_mode) {
            KeyboardTarget::Media => doc
                .media_focus_handle(cx)
                .unwrap_or_else(|| doc.editor().focus_handle(cx)),
            KeyboardTarget::Editor => doc.editor().focus_handle(cx),
            KeyboardTarget::Preview => doc.preview().read(cx).focus_handle().clone(),
        };
        window.defer(cx, move |window, cx| focus.focus(window, cx));
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
                AppSettings::update(cx, |settings| settings.push_recent(&path));
                self.file_tree
                    .update(cx, |tree, cx| tree.set_root(Some(path), cx));
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
                    .map(|path| reads_as_text(path).then(|| document::load(path)))
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
        // A Typst document's preview is its compiled pages, so it takes the
        // same path a PDF does and is watched for changes to refresh.
        if document::is_typst_path(&path) {
            match crate::typst_preview::TypstPreview::open(&path) {
                Ok(compiled) => {
                    AppSettings::update(cx, |settings| settings.push_recent(&path));
                    let doc =
                        cx.new(|cx| DocumentView::new_typst(path.clone(), compiled, window, cx));
                    self.place_document(doc, placement, window, cx);
                    return;
                }
                Err(err) => {
                    // Show the source so the mistake can be fixed here, the
                    // way VS Code opens a file whose preview fails; it stays
                    // in the recent files, as it still exists.
                    self.notify_error(
                        format!(
                            "Couldn’t compile “{}”. {err} Showing the source instead; reopen it once it compiles to see its pages.",
                            document::display_name(Some(&path))
                        ),
                        window,
                        cx,
                    );
                    // Falls through to open the source as text.
                }
            }
        }

        if document::is_pdf_path(&path) {
            // The async spawn_in continuation never resumes under the current
            // gpui-pre build when spawned from the startup path, so render
            // inline: PDFs are small and this keeps open/restore working.
            let result = std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(crate::pdf_view::PdfDocument::from_bytes);
            match result {
                Ok(document) => {
                    AppSettings::update(cx, |settings| settings.push_recent(&path));
                    let doc =
                        cx.new(|cx| DocumentView::new_pdf(path.clone(), document, window, cx));
                    self.place_document(doc, placement, window, cx);
                }
                Err(err) => {
                    AppSettings::update(cx, |settings| {
                        settings.recent_files.retain(|recent| recent != &path)
                    });
                    self.notify_error(
                        format!(
                            "Couldn’t open “{}”. {err}",
                            document::display_name(Some(&path))
                        ),
                        window,
                        cx,
                    );
                }
            }
            return;
        }

        if document::is_image_path(&path) {
            match std::fs::read(&path) {
                Ok(bytes) => match crate::image_view::load_image(&path, bytes) {
                    Ok((image, width, height)) => {
                        AppSettings::update(cx, |settings| settings.push_recent(&path));
                        let doc = cx.new(|cx| {
                            DocumentView::new_image(path.clone(), image, width, height, window, cx)
                        });
                        self.place_document(doc, placement, window, cx);
                    }
                    Err(err) => {
                        self.notify_error(
                            format!(
                                "Couldn\u{2019}t open \u{201c}{}\u{201d}. {err}",
                                document::display_name(Some(&path))
                            ),
                            window,
                            cx,
                        );
                    }
                },
                Err(err) => {
                    self.notify_error(
                        format!(
                            "Couldn\u{2019}t read \u{201c}{}\u{201d}. {err}",
                            document::display_name(Some(&path))
                        ),
                        window,
                        cx,
                    );
                }
            }
            return;
        }

        let read_path = path.clone();
        let task = cx
            .background_executor()
            .spawn(async move { document::load(&read_path) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, |this, window, cx| match result {
                Ok(loaded) => {
                    AppSettings::update(cx, |settings| settings.push_recent(&path));
                    let doc = cx.new(|cx| {
                        DocumentView::new(
                            loaded.text,
                            Some(path.clone()),
                            loaded.line_ending,
                            window,
                            cx,
                        )
                    });
                    this.place_document(doc.clone(), placement, window, cx);
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
                    this.notify_error(
                        format!("Couldn’t open “{}”. {err}", display_name(Some(&path))),
                        window,
                        cx,
                    );
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
            .filter(|doc| !doc.read(cx).is_typst() && !doc.read(cx).is_media())
            .filter_map(|doc| {
                let id = doc.entity_id();
                let doc = doc.read(cx);
                let path = doc.path()?.clone();
                (!self.pending_disk_prompt.contains(&id))
                    .then(|| (id, path, doc.disk_stamp(), doc.saved_hash()))
            })
            .collect();
        // A Typst preview's file is not text: its pages come from compiling the
        // source, so it is watched as a compilation instead. It watches its
        // source and every file the source named, and one stamp over them is
        // what a change has to move.
        let recompilable: Vec<(EntityId, Vec<PathBuf>, Option<DiskStamp>)> = self
            .documents
            .iter()
            .filter(|doc| doc.read(cx).is_typst())
            .filter(|doc| !self.pending_disk_prompt.contains(&doc.entity_id()))
            .map(|entity| {
                let doc = entity.read(cx);
                (entity.entity_id(), doc.watched_paths(), doc.disk_stamp())
            })
            .collect();
        if targets.is_empty() && recompilable.is_empty() {
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
            for (id, watched, known) in recompilable {
                let stamp = typst_preview::stamp_of(&watched);
                if Some(stamp) == known {
                    continue;
                }
                // Every path vanished: the document is gone. Saying so is the
                // missing-file path's job, and the source is among the paths.
                if stamp.modified.is_none() {
                    continue;
                }
                changes.push((id, DiskChange::Recompiled));
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
            DiskChange::Recompiled => {
                if doc.read(cx).is_typst() {
                    let entity = doc.clone();
                    doc.update(cx, |view, cx| {
                        if view.refresh_typst(cx) {
                            // The stamp now on the files is what the next pass
                            // compares against, so one change is one refresh.
                            let stamp = view.watch_stamp();
                            cx.defer(move |cx| {
                                entity.update(cx, |view, _| view.accept_watch_stamp(stamp));
                            });
                        } else {
                            // The source no longer compiles: the last good
                            // pages stay on screen. Say so once rather than
                            // leaving the reader guessing why nothing moved.
                            window.push_notification(
                                Notification::error(format!(
                                    "“{name}” no longer compiles. The preview shows the last version that did."
                                )),
                                cx,
                            );
                        }
                    });
                }
            }
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
                    return;
                }
                if window.has_active_dialog(cx) {
                    // Ask once the current dialog is answered.
                    return;
                }
                doc.update(cx, |doc, _| doc.set_disk_stamp(stamp));
                self.pending_disk_prompt.insert(id);
                if let Some(ix) = self.document_index(id) {
                    self.activate(ix, window, cx);
                }
                let workspace = cx.weak_entity();
                let text = Arc::new(text);
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let (keep, reload) = (workspace.clone(), workspace.clone());
                    let (reload_doc, text) = (doc.clone(), text.clone());
                    alert
                        .title(format!("“{name}” changed on disk"))
                        .description(
                            "Another program saved this file while you have unsaved changes here.",
                        )
                        .footer(
                            DialogFooter::new()
                                .justify_end()
                                .child(
                                    Button::new("reload")
                                        .label("Use the file on disk")
                                        .outline()
                                        .on_click(move |_, window, cx| {
                                            window.close_dialog(cx);
                                            let text = text.as_ref().clone();
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
                                    Button::new("keep")
                                        .label("Keep my changes")
                                        .primary()
                                        .on_click(move |_, window, cx| {
                                            window.close_dialog(cx);
                                            _ = keep.update(cx, |this, cx| {
                                                this.pending_disk_prompt.remove(&id);
                                                this.focus_active(window, cx);
                                            });
                                        }),
                                ),
                        )
                });
            }
        }
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

    /// Open a system terminal in the current folder (VS Code Ctrl+`).
    fn summon_terminal(&mut self, cx: &mut Context<Self>) {
        let doc_dir = self
            .active_document()
            .read(cx)
            .path()
            .and_then(|path| path.parent().map(|parent| parent.to_path_buf()));
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .ok()
            .map(PathBuf::from);
        let dir = self
            .file_tree
            .read(cx)
            .root()
            .map(Path::to_path_buf)
            .or(doc_dir)
            .or(home);
        let Some(dir) = dir else {
            return;
        };
        #[cfg(target_os = "windows")]
        {
            if std::process::Command::new("wt")
                .arg("-d")
                .arg(&dir)
                .spawn()
                .is_err()
            {
                let _ = std::process::Command::new("cmd")
                    .args(["/C", "start", "", "/D"])
                    .arg(&dir)
                    .spawn();
            }
        }
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("open")
                .arg("-a")
                .arg("Terminal")
                .arg(&dir)
                .spawn();
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let spawned = std::process::Command::new("x-terminal-emulator")
                .current_dir(&dir)
                .spawn()
                .or_else(|_| {
                    std::process::Command::new("xterm")
                        .current_dir(&dir)
                        .spawn()
                });
            let _ = spawned;
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
            let Ok(Ok(Some(paths))) = paths.await else {
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
                this.save_session(window, cx);
            });
        })
        .detach();
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
        let Some((path, is_dir)) = self.file_tree.read(cx).single_selection() else {
            return;
        };
        if is_dir && self.file_tree.read(cx).root() == Some(path.as_path()) {
            // The root folder is not renamed from here, as in VS Code.
            return;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.prompt_tree_name(TreePromptKind::Rename, None, &name, window, cx);
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

    fn tree_open(&mut self, _: &OpenInTree, _: &mut Window, cx: &mut Context<Self>) {
        self.file_tree
            .update(cx, |tree, cx| tree.open_selection(cx));
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
        window.open_dialog(cx, move |dialog, window, cx| {
            let focus = state.read(cx).focus_handle(cx);
            window.defer(cx, move |window, cx| focus.focus(window, cx));
            let typed = state.clone();
            let workspace = workspace.clone();
            let apply = apply.clone();
            dialog.close_button(false).p_0().child(
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
            )
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
            TreePromptKind::Rename => ("Rename", "New name"),
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

    /// Ask what to look for, then search the open folder's files for it, the
    /// way VS Code's Search view (Ctrl+Shift+F) does.
    fn find_in_files(&mut self, _: &FindInFiles, window: &mut Window, cx: &mut Context<Self>) {
        if self.file_tree.read(cx).root().is_none() {
            window.push_notification(Notification::info("Open a folder to search its files."), cx);
            return;
        }
        Self::prompt_line(
            &self.find_query.clone(),
            "Find in files".into(),
            "Text to find".into(),
            "",
            window,
            cx,
            |this, query, window, cx| this.search_folder(query, window, cx),
        );
    }

    /// Walk the folder's files and search their text.
    fn search_folder(&mut self, needle: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.file_tree.read(cx).root().map(Path::to_path_buf) else {
            return;
        };
        if needle.is_empty() {
            return;
        }
        // The walk and the reads are off the UI thread: a folder of notes
        // runs to thousands of files, and the window used to sit still while
        // Quick Open listed them for the same reason.
        let show_hidden = AppSettings::get(cx).show_hidden_entries;
        let opened = self.file_tree.read(cx).expanded_folders();
        let walk_root = root.clone();
        let wanted = needle.clone();
        let listing = cx.background_executor().spawn(async move {
            let files =
                quick_open::files_under(&walk_root, show_hidden, &opened, quick_open::MAX_FILES);
            search::search_files(&files, &wanted, search::MAX_HITS)
        });
        cx.spawn_in(window, async move |this, cx| {
            let hits = listing.await;
            _ = this.update_in(cx, |this, window, cx| {
                this.open_find_results(root, needle, hits, window, cx)
            });
        })
        .detach();
    }

    /// List what the search found; confirming a line opens the file it came
    /// from with the caret on it.
    fn open_find_results(
        &mut self,
        root: PathBuf,
        query: String,
        hits: Vec<Hit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        if hits.is_empty() {
            window.push_notification(Notification::info(format!("No matches for “{query}”.")), cx);
            return;
        }
        self.find_results
            .update(cx, |state, cx| state.set_query("", window, cx));
        let state = self.find_results.clone();
        let workspace = cx.weak_entity();
        window.open_dialog(cx, move |dialog, window, cx| {
            let focus = state.read(cx).focus_handle(cx);
            window.defer(cx, move |window, cx| focus.focus(window, cx));
            let items = hits.iter().map(|hit| {
                let shown = hit
                    .path
                    .strip_prefix(&root)
                    .unwrap_or(hit.path.as_path())
                    .to_string_lossy()
                    .replace('\\', "/");
                let label: SharedString = format!("{shown}:{}  {}", hit.line, hit.text).into();
                CommandItem::new()
                    .label(label)
                    .keywords([shown, hit.text.clone()])
                    .icon(Icon::new(file_icon(&hit.path)))
            });
            let picked = hits.clone();
            let workspace = workspace.clone();
            dialog.close_button(false).p_0().child(
                Command::new(&state)
                    .bordered(false)
                    .placeholder("Filter the matches")
                    .max_h(rems(24.))
                    .items(items)
                    .empty(|_, _, _| "No matching lines")
                    .on_confirm(move |index: IndexPath, window, cx| {
                        window.close_dialog(cx);
                        let Some(hit) = picked.get(index.row).cloned() else {
                            return;
                        };
                        _ = workspace.update(cx, |this, cx| this.open_hit(&hit, window, cx));
                    })
                    .on_cancel(|window, cx| window.close_dialog(cx)),
            )
        });
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
            TreePromptKind::Rename => {
                let Some((path, _)) = self.file_tree.read(cx).single_selection() else {
                    return;
                };
                self.file_tree
                    .update(cx, |tree, cx| tree.rename_entry(&path, &name, window, cx));
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

        let pending = valid_targets.iter().find_map(|id| {
            self.document_index(*id).and_then(|ix| {
                let doc = &self.documents[ix];
                if doc.read(cx).is_dirty() && !self.discarded.contains(id) {
                    Some((ix, doc.clone()))
                } else {
                    None
                }
            })
        });

        match pending {
            Some((ix, doc)) => {
                self.activate(ix, window, cx);
                self.confirm_unsaved(doc, AfterConfirm::CloseDocuments(valid_targets), window, cx);
            }
            None => {
                for id in valid_targets {
                    self.discarded.remove(&id);
                    self.remove_document(id, window, cx);
                }
            }
        }
    }

    fn close_window(&mut self, _: &CloseWindow, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_close(AfterConfirm::CloseWindow, window, cx);
    }

    fn quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_close(AfterConfirm::Quit, window, cx);
    }

    /// Called by the platform before the window closes.
    fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.closing {
            return true;
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
    /// has been saved or discarded, asking about each in turn.
    fn confirm_close(&mut self, next: AfterConfirm, window: &mut Window, cx: &mut Context<Self>) {
        let pending = self
            .documents
            .iter()
            .find(|doc| doc.read(cx).is_dirty() && !self.discarded.contains(&doc.entity_id()));
        match pending.cloned() {
            Some(doc) => {
                if let Some(ix) = self.document_index(doc.entity_id()) {
                    self.activate(ix, window, cx);
                }
                self.confirm_unsaved(doc, next, window, cx);
            }
            None => self.run_after_confirm(next, window, cx),
        }
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
            return;
        }

        let name = doc.read(cx).name();
        let workspace = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (save, discard, cancel) = (workspace.clone(), workspace.clone(), workspace.clone());
            let (save_next, discard_next) = (next.clone(), next.clone());
            let (save_doc, discard_doc) = (doc.clone(), doc.clone());
            alert
                .title(format!("Save changes to “{name}”?"))
                .description("Your changes will be lost if you don’t save them.")
                .footer(
                    DialogFooter::new()
                        .justify_end()
                        .child(
                            Button::new("discard")
                                .label("Don’t save")
                                .outline()
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let next = discard_next.clone();
                                    let id = discard_doc.entity_id();
                                    discard_doc.update(cx, |doc, _| doc.discard_recovery());
                                    _ = discard.update(cx, |this, cx| {
                                        this.discarded.insert(id);
                                        this.focus_active(window, cx);
                                        this.run_after_confirm(next, window, cx)
                                    });
                                }),
                        )
                        .child(div().flex_1())
                        .child(Button::new("cancel").label("Cancel").outline().on_click(
                            move |_, window, cx| {
                                window.close_dialog(cx);
                                _ = cancel.update(cx, |this, cx| {
                                    this.discarded.clear();
                                    this.focus_active(window, cx);
                                });
                            },
                        ))
                        .child(Button::new("save").label("Save").primary().on_click(
                            move |_, window, cx| {
                                window.close_dialog(cx);
                                let next = save_next.clone();
                                let doc = save_doc.clone();
                                _ = save.update(cx, |this, cx| {
                                    this.focus_active(window, cx);
                                    this.save_document(doc, Some(next), window, cx)
                                });
                            },
                        )),
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
            let Ok(Ok(Some(paths))) = paths.await else {
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
        self.prompt_save_path(doc, None, window, cx);
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
        let chosen = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = chosen.await else {
                return;
            };
            _ = this.update_in(cx, |this, window, cx| {
                if this.write(&doc, &path, window, cx) {
                    AppSettings::update(cx, |settings| settings.push_recent(&path));
                    this.sync_file_tree(cx);
                    if let Some(next) = next {
                        this.run_after_confirm(next, window, cx);
                    }
                }
            });
        })
        .detach();
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
                self.notify_error(
                    format!(
                        "Couldn’t save “{}”. {err}",
                        display_name(Some(&path.to_path_buf()))
                    ),
                    window,
                    cx,
                );
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
        let source = doc.source().clone();
        let base_dir = doc.dir();
        let chosen = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(path))) = chosen.await else {
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
            doc.source().to_string()
        } else {
            editor.value()[selection].to_string()
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
            Some(path) => cx.reveal_path(&path),
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

    /// Land a dropped tab: a move, or with the invert key held a copy
    /// (duplicate) at the insertion point, as in VS Code.
    fn finish_tab_drop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(insertion) = self.tab_insertion.take() else {
            return;
        };
        if self.tab_drag_copy.get() {
            self.duplicate_tab(insertion.source, insertion.gap, window, cx);
        } else {
            self.move_tab(insertion.source, insertion.gap, window, cx);
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
        AppSettings::update(cx, |settings| {
            settings.layout = layout;
            settings.focus_mode = false;
        });
        self.focus_active(window, cx);
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
        window.open_dialog(cx, move |dialog, window, cx| {
            let focus = headings.read(cx).focus_handle(cx);
            window.defer(cx, move |window, cx| focus.focus(window, cx));
            let outline = doc.read(cx).analysis().outline.clone();
            let items = outline.iter().map(|heading| {
                CommandItem::new()
                    .label(heading.text.clone())
                    .keywords([heading.text.clone()])
                    .icon(heading_icon(heading.level))
            });
            let confirm = doc.clone();
            dialog.close_button(false).p_0().child(
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
            )
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
        window.open_dialog(cx, move |dialog, window, cx| {
            let focus = state.read(cx).focus_handle(cx);
            window.defer(cx, move |window, cx| focus.focus(window, cx));
            // A query that is a path is checked directly, VS Code style: a
            // file that exists is offered even when the listing left it out
            // (ignored, too deep, outside the folder), and a folder lists its
            // own entries. One `stat`, or one folder read, per keystroke.
            let query = state.read(cx).query(cx).trim().to_string();
            let rows = quick_open_rows(&query, &root, home.as_deref(), &listing);
            let items = rows.iter().map(|row| {
                let shown: SharedString = row.label.clone().into();
                let name: SharedString = row
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default()
                    .into();
                let icon = if row.is_dir {
                    IconName::Folder
                } else {
                    file_icon(&row.path)
                };
                let mut keywords = vec![shown.clone(), name];
                if let Some(keyword) = &row.keyword {
                    keywords.push(keyword.clone().into());
                }
                CommandItem::new()
                    .label(shown)
                    .keywords(keywords)
                    .icon(Icon::new(icon))
            });
            let picked = rows.clone();
            let navigate = state.clone();
            dialog.close_button(false).p_0().child(
                Command::new(&state)
                    .bordered(false)
                    .placeholder("Search files by name, or type a path")
                    .max_h(rems(24.))
                    .items(items)
                    .empty(|_, _, _| "No matching files")
                    // The rows depend on the query when it is a path, so the
                    // dialog is drawn again as it changes.
                    .on_query(|_, window, _| window.refresh())
                    .on_confirm(move |index: IndexPath, window, cx| {
                        let Some(row) = picked.get(index.row).cloned() else {
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
                        window.dispatch_action(Box::new(OpenQuickPath(row.path, row.line)), cx);
                    })
                    .on_cancel(|window, cx| window.close_dialog(cx)),
            )
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
        // arrives. The rows therefore carry no action at all; confirming one
        // hands the Action it stands for back to the workspace and dispatches
        // it from there.
        let ws_focus = self.focus_handle.clone();
        window.open_dialog(cx, move |dialog, window, cx| {
            let focus = state.read(cx).focus_handle(cx);
            window.defer(cx, move |window, cx| focus.focus(window, cx));
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
            dialog.close_button(false).p_0().child(
                command
                    .empty(|_, _, _| "No matching commands")
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
            )
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
        window.open_dialog(cx, move |dialog, window, cx| {
            let focus = state.read(cx).focus_handle(cx);
            window.defer(cx, move |window, cx| focus.focus(window, cx));
            let typed = state.clone();
            dialog.close_button(false).p_0().child(
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
            )
        });
    }

    /// Move the active document's caret to a line asked for by Go to Line.
    fn go_to_line_at(&mut self, action: &GoToLineAt, window: &mut Window, cx: &mut Context<Self>) {
        let line = action.0;
        self.with_active(window, cx, |doc, window, cx| {
            doc.go_to_line(line, window, cx)
        });
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

    // ---------------------------------------------------------------------
    // Rendering

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
                // The document name, centered on the window like native title bars.
                h_flex()
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
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ToggleFileSidebar), cx)
                            }),
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
                            .on_click(cx.listener(|_, clicked: &Vec<usize>, window, cx| {
                                let layout = match clicked.first() {
                                    Some(0) => Layout::Editor,
                                    Some(2) => Layout::Preview,
                                    _ => Layout::Split,
                                };
                                window.dispatch_action(Box::new(SetLayout(layout)), cx);
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
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ToggleAppearance), cx)
                            }),
                    ),
            )
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let active_ix = self.active;
        let count = self.documents.len();
        let workspace = cx.weak_entity();

        h_flex()
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
            .on_drag_move::<TabDrag>(cx.listener(|this, event: &DragMoveEvent<TabDrag>, _, _| {
                this.tab_bar_bounds.set(Some(event.bounds));
                let copy = invert_key_held(&event.event.modifiers);
                if this.tab_drag_copy.get() != copy {
                    this.tab_drag_copy.set(copy);
                }
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
                h_flex()
                    .id("tabs-scroll-area")
                    .flex_1()
                    .h_full()
                    .items_center()
                    .gap_1()
                    .overflow_x_scrollbar()
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
                        let is_last = ix + 1 >= count;
                        let has_other = count > 1;
                        let name = doc.name().to_string();
                        // The tab carries its file's type icon, as in VS Code.
                        let tab_icon = doc.path().map(|path| file_icon(path));
                        let tab_path = doc.path().cloned();
                        let theme = cx.theme();
                        let doc_weak = doc_entity.clone();
                        let ws_menu = workspace.clone();
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
                                    .text_color(if active {
                                        theme.tab_active_foreground
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .child(
                                        Icon::new(tab_icon.unwrap_or(IconName::FileText)).xsmall(),
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
                                                .text_color(theme.muted_foreground),
                                        )
                                    })
                                    .when(!is_pinned, |this| {
                                        this.child(div().when(dirty, |this| {
                                            this.child(
                                                div().size(px(7.)).rounded_full().bg(theme.primary),
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
                            .context_menu({
                                let ws = ws_menu.clone();
                                let path = tab_path.clone();
                                move |menu, _, _| {
                                    let ws_close = ws.clone();
                                    let ws_other = ws.clone();
                                    let ws_right = ws.clone();
                                    let ws_pin = ws.clone();
                                    let doc = doc_weak.clone();

                                    let menu = menu
                                        .item(
                                            PopupMenuItem::new(if is_pinned {
                                                "Unpin"
                                            } else {
                                                "Pin"
                                            })
                                            .on_click(move |_, _, cx| {
                                                _ = ws_pin.update(cx, |this, cx| {
                                                    this.toggle_pin_by_id(doc_id, cx);
                                                });
                                            }),
                                        )
                                        .item(PopupMenuItem::new("Close").on_click(
                                            move |_, window, cx| {
                                                _ = ws_close.update(cx, |this, cx| {
                                                    let id = doc.entity_id();
                                                    this.confirm_unsaved(
                                                        doc.clone(),
                                                        AfterConfirm::CloseDocument(id),
                                                        window,
                                                        cx,
                                                    );
                                                });
                                            },
                                        ))
                                        .item(
                                            PopupMenuItem::new("Close Other Tabs")
                                                .disabled(!has_other)
                                                .on_click(move |_, window, cx| {
                                                    _ = ws_other.update(cx, |this, cx| {
                                                        this.close_other_tabs_by_id(
                                                            doc_id, window, cx,
                                                        );
                                                    });
                                                }),
                                        )
                                        .item(
                                            PopupMenuItem::new("Close Tabs to the Right")
                                                .disabled(is_last)
                                                .on_click(move |_, window, cx| {
                                                    _ = ws_right.update(cx, |this, cx| {
                                                        this.close_tabs_to_the_right_by_id(
                                                            doc_id, window, cx,
                                                        );
                                                    });
                                                }),
                                        );

                                    // The document's file, when it has one. An
                                    // untitled buffer has nowhere to reveal or
                                    // copy, so these rows only appear for a
                                    // saved file, as in VS Code.
                                    match path.clone() {
                                        Some(path) => {
                                            let text = path.display().to_string();
                                            let reveal = path.clone();
                                            menu.item(PopupMenuItem::new("Copy Path").on_click(
                                                move |_, _, cx| {
                                                    cx.write_to_clipboard(
                                                        ClipboardItem::new_string(text.clone()),
                                                    );
                                                },
                                            ))
                                            .item(
                                                PopupMenuItem::new("Reveal in File Manager")
                                                    .on_click(move |_, _, cx| {
                                                        cx.reveal_path(&reveal);
                                                    }),
                                            )
                                        }
                                        None => menu,
                                    }
                                }
                            })
                    })),
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
            })
    }

    fn render_outline(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let doc = self.active_document().read(cx);
        let outline = doc.analysis().outline.clone();
        let current = doc.current_heading(cx);
        let min_level = outline
            .iter()
            .map(|heading| heading.level)
            .min()
            .unwrap_or(1);
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
                    .when(outline.is_empty(), |this| {
                        this.child(
                            div()
                                .p_3()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child("No headings yet"),
                        )
                    })
                    .children(outline.into_iter().enumerate().map(|(ix, heading)| {
                        let theme = cx.theme();
                        let active = Some(ix) == current;
                        div()
                            .id(("heading", ix))
                            .flex_none()
                            .py_1()
                            .pr_2()
                            .pl(px(
                                10. + 12. * f32::from(heading.level.saturating_sub(min_level))
                            ))
                            .rounded_md()
                            .text_sm()
                            .truncate()
                            .cursor_pointer()
                            .when(heading.level <= min_level, |this| {
                                this.font_weight(FontWeight::MEDIUM)
                            })
                            .when(active, |this| {
                                this.bg(theme.list_active).text_color(theme.foreground)
                            })
                            .when(!active, |this| {
                                this.text_color(theme.muted_foreground)
                                    .hover(|style| style.bg(theme.list_hover))
                            })
                            .child(heading.text.clone())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.with_active(window, cx, |doc, window, cx| {
                                    doc.reveal_heading(ix, window, cx)
                                });
                            }))
                    })),
            )
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let doc = self.active_document().read(cx);
        let settings = AppSettings::get(cx);
        let muted = cx.theme().muted_foreground;
        let item = |text: String| div().text_color(muted).child(text);
        let language = item(doc.language_label().to_string());

        // A viewer has no text: no words, caret, line ending or indent to
        // report. It says where you are in it instead, as VS Code's image
        // preview names the picture's size and zoom.
        if doc.is_media() {
            let status = doc.media_status(cx);
            return StatusBar::new()
                .left(h_flex().gap_4().children(status.into_iter().map(item)))
                .right(h_flex().gap_4().child(language))
                .into_any_element();
        }

        let stats = doc.analysis().stats;
        let editor = doc.editor().read(cx);
        let position = editor.cursor_position();
        let selection = editor.selected_range();
        let line_ending = doc.line_ending();
        let shows_editor = settings.layout.shows_editor() || !doc.has_preview();
        StatusBar::new()
            .left(
                h_flex()
                    .gap_4()
                    .child(item(format!(
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
                    .when(doc.preview_limited(), |this| {
                        this.child(item("Preview limited (large file)".to_string()))
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
                                group_thousands(selection.len())
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
                    // The indentation the file uses, detected when it was
                    // read, as VS Code reports it.
                    .child(item(doc.indent().label()))
                    // VS Code's status bar switches the line ending when its
                    // EOL item is clicked; Malgel toggles LF and CRLF there
                    // too.
                    .child(
                        Button::new("line-ending")
                            .ghost()
                            .xsmall()
                            .compact()
                            .label(line_ending.label())
                            .tooltip("Select End of Line Sequence")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ToggleLineEnding), cx)
                            }),
                    )
                    .child(language),
            )
            .into_any_element()
    }
}

/// The settings dialog: Malgel's optional features and how it behaves.
pub fn open_settings(window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    // Ask the system afresh each time, as the user may have changed the
    // default app elsewhere.
    default_app::refresh(cx);
    window.open_dialog(cx, |dialog, _, cx| {
        let settings = AppSettings::get(cx).clone();
        let default_status = default_app::status(cx);
        let row = |id: &'static str,
                   title: &'static str,
                   description: &'static str,
                   checked: bool,
                   update: fn(&mut crate::settings::Settings, bool),
                   cx: &App| {
            h_flex()
                .gap_4()
                .py_2()
                .justify_between()
                .child(
                    v_flex()
                        .gap_0p5()
                        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(description),
                        ),
                )
                .child(
                    Switch::new(id)
                        .checked(checked)
                        .on_click(move |checked, _, cx| {
                            let checked = *checked;
                            AppSettings::update(cx, |settings| update(settings, checked));
                        }),
                )
        };
        let section = |title: &'static str| {
            div()
                .pt_3()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(cx.theme().muted_foreground)
                .child(title)
        };
        dialog.title("Settings").w(px(560.)).child(
            v_flex()
                .child(section("WORKSPACE"))
                .child(row(
                    "tabs",
                    "Tabs",
                    "Open documents side by side in tabs instead of one at a time.",
                    settings.tabs,
                    |settings, on| settings.tabs = on,
                    cx,
                ))
                .child(row(
                    "file-sidebar",
                    "File sidebar",
                    "Browse a folder’s Markdown files beside the editor.",
                    settings.file_sidebar,
                    |settings, on| settings.file_sidebar = on,
                    cx,
                ))
                .child(row(
                    "outline",
                    "Outline",
                    "List the document’s headings beside the editor.",
                    settings.outline_sidebar,
                    |settings, on| settings.outline_sidebar = on,
                    cx,
                ))
                .child(row(
                    "restore-session",
                    "Reopen last session",
                    "Start with the documents, folder and window you had open.",
                    settings.restore_session,
                    |settings, on| settings.restore_session = on,
                    cx,
                ))
                .child(section("WRITING"))
                .child(row(
                    "spell-check",
                    "Check spelling",
                    "Underline unknown words; right-click one for suggestions.",
                    settings.spell_check,
                    |settings, on| settings.spell_check = on,
                    cx,
                ))
                .child(row(
                    "scroll-sync",
                    "Sync scrolling",
                    "Keep the preview on the part of the document being edited.",
                    settings.scroll_sync,
                    |settings, on| settings.scroll_sync = on,
                    cx,
                ))
                .child(row(
                    "soft-wrap",
                    "Wrap lines",
                    "Wrap long lines in the editor instead of scrolling sideways.",
                    settings.soft_wrap,
                    |settings, on| settings.soft_wrap = on,
                    cx,
                ))
                .child(row(
                    "line-numbers",
                    "Line numbers",
                    "Number the lines in the editor.",
                    settings.line_numbers,
                    |settings, on| settings.line_numbers = on,
                    cx,
                ))
                .child(section("LINKS"))
                .child(
                    h_flex()
                        .gap_4()
                        .py_2()
                        .justify_between()
                        .child(
                            v_flex()
                                .gap_0p5()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child("Open linked documents in"),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(if cfg!(target_os = "macos") {
                                            "Command-click a link to do the opposite."
                                        } else {
                                            "Ctrl-click a link to do the opposite."
                                        }),
                                ),
                        )
                        .child(
                            RadioGroup::horizontal("open-links-in")
                                .flex_none()
                                .child("A new tab")
                                .child("The same tab")
                                .selected_index(Some(match settings.open_links_in {
                                    LinkPlacement::NewTab => 0,
                                    LinkPlacement::SameTab => 1,
                                }))
                                .on_click(|ix, _, cx| {
                                    let placement = if *ix == 0 {
                                        LinkPlacement::NewTab
                                    } else {
                                        LinkPlacement::SameTab
                                    };
                                    AppSettings::update(cx, |settings| {
                                        settings.open_links_in = placement
                                    });
                                }),
                        ),
                )
                .child(row(
                    "confirm-web-links",
                    "Ask before opening web links",
                    "Show where a link leads before it opens in your browser.",
                    settings.confirm_web_links,
                    |settings, on| settings.confirm_web_links = on,
                    cx,
                ))
                .child(row(
                    "confirm-drag-and-drop",
                    "Ask before moving files dragged in the sidebar",
                    "Confirm where a dragged file lands before it moves; holding Ctrl (Option on macOS) copies instead.",
                    settings.confirm_drag_and_drop,
                    |settings, on| settings.confirm_drag_and_drop = on,
                    cx,
                ))
                .child(section("FILES"))
                .child(default_app_row(default_status, cx)),
        )
    });
}

/// The settings row that makes Malgel the app for Markdown files.
fn default_app_row(status: default_app::Status, cx: &App) -> impl IntoElement {
    let is_default = status == default_app::Status::Default;
    let description = if is_default {
        "Markdown files you open from your desktop open in Malgel."
    } else if default_app::CHOSEN_IN_SYSTEM_SETTINGS {
        "Choose Malgel for Markdown files in Windows Settings."
    } else {
        "Open Markdown files from your desktop in Malgel."
    };
    let action = if is_default {
        h_flex()
            .gap_1()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(Icon::new(IconName::Check).small())
            .child("Default")
            .into_any_element()
    } else {
        let label = if default_app::CHOSEN_IN_SYSTEM_SETTINGS {
            "Open Settings…"
        } else {
            "Make Default"
        };
        Button::new("make-default")
            .label(label)
            .small()
            .outline()
            .on_click(|_, window, cx| {
                let note = match default_app::make_default(cx) {
                    Ok(default_app::Outcome::Done) => {
                        Notification::success("Malgel now opens Markdown files.")
                    }
                    Ok(default_app::Outcome::SettingsOpened) => {
                        Notification::info("In Settings, set Malgel as the default for .md files.")
                    }
                    Err(message) => Notification::error(message),
                };
                window.push_notification(note, cx);
                window.refresh();
            })
            .into_any_element()
    };
    h_flex()
        .gap_4()
        .py_2()
        .justify_between()
        .child(
            v_flex()
                .gap_0p5()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child("Default app for Markdown"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(description),
                ),
        )
        .child(action)
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
        let document = self.active_document().clone();
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
                    .child(self.file_tree.clone()),
            );
        }
        main = main.child(
            resizable_panel()
                .size_range(rems(20.).to_pixels(rem)..Pixels::MAX)
                .child(
                    v_flex()
                        .size_full()
                        .when(self.shows_tabs(cx), |this| this.child(self.render_tabs(cx)))
                        .child(div().flex_1().min_h_0().child(document)),
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
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::save_as))
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
            .on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
            .on_action(cx.listener(Self::close_other_tabs))
            .on_action(cx.listener(Self::close_tabs_to_the_right))
            .on_action(cx.listener(Self::tree_new_file))
            .on_action(cx.listener(Self::focus_explorer))
            .on_action(cx.listener(Self::tree_new_folder))
            .on_action(cx.listener(Self::tree_rename))
            .on_action(cx.listener(Self::tree_delete))
            .on_action(cx.listener(Self::tree_cut))
            .on_action(cx.listener(Self::tree_copy))
            .on_action(cx.listener(Self::tree_paste))
            .on_action(cx.listener(Self::tree_open))
            .on_action(cx.listener(Self::tree_select_up))
            .on_action(cx.listener(Self::tree_select_down))
            .on_action(cx.listener(Self::tree_select_left))
            .on_action(cx.listener(Self::tree_select_right))
            .on_action(cx.listener(Self::tree_select_first))
            .on_action(cx.listener(Self::tree_select_last))
            .on_action(cx.listener(Self::close_window))
            .on_action(cx.listener(Self::quit))
            .on_action(cx.listener(Self::set_layout))
            .on_action(cx.listener(Self::go_to_heading))
            .on_action(cx.listener(Self::show_command_palette))
            .on_action(cx.listener(Self::show_go_to_line))
            .on_action(cx.listener(Self::go_to_line_at))
            .on_action(cx.listener(Self::show_quick_open))
            .on_action(cx.listener(Self::open_quick_path))
            .on_action(cx.listener(Self::find_in_files))
            .on_action(cx.listener(Self::toggle_appearance))
            .on_action(cx.listener(Self::toggle_focus_mode))
            .on_action(cx.listener(Self::open_settings))
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
            .on_action(cx.listener(|this, _: &SummonTerminal, _, cx| this.summon_terminal(cx)))
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
            // While a tab drags, clear the insertion line whenever the
            // pointer leaves the tab bar; the bar re-sets its bounds and
            // the hovered tab re-sets the line after this (capture order).
            .on_drag_move::<TabDrag>(cx.listener(|this, event: &DragMoveEvent<TabDrag>, _, cx| {
                let inside = this
                    .tab_bar_bounds
                    .get()
                    .is_some_and(|bounds| bounds.contains(&event.event.position));
                if !inside && this.tab_insertion.take().is_some() {
                    cx.notify();
                }
            }))
            .child(self.render_title_bar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .drag_over::<TreeDrag>(|style, _, _, cx| style.bg(cx.theme().drop_target))
                    .child(main),
            )
            .when(!focus_mode, |this| this.child(self.render_status_bar(cx)))
    }
}

impl Workspace {
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

#[cfg(test)]
mod tests {
    #[test]
    fn several_files_read_text_first_and_viewers_inline() {
        use std::path::Path;
        assert!(super::reads_as_text(Path::new("a.md")));
        assert!(super::reads_as_text(Path::new("main.rs")));
        assert!(!super::reads_as_text(Path::new("a.pdf")));
        assert!(!super::reads_as_text(Path::new("a.png")));
        assert!(!super::reads_as_text(Path::new("a.typ")));
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
}
