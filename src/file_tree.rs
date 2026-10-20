//! The file sidebar: a folder's Markdown and text files as a tree.
//!
//! Folders are listed when first expanded and re-listed in the background
//! when the workspace refreshes, so the tree follows files created, renamed
//! and deleted by other programs.

use std::{
    cell::Cell,
    collections::{BTreeSet, HashMap, HashSet},
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use gpui_kit::{
    AppContext as _, ClickEvent, ClipboardItem, Context, DragMoveEvent, Entity, EventEmitter,
    ExternalPaths, FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement,
    Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, UniformListScrollHandle,
    Window,
    assets::IconName,
    component::{
        ActiveTheme as _, Icon, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        dialog::DialogFooter,
        h_flex,
        input::{Escape, Input, InputEvent, InputState},
        menu::ContextMenuExt as _,
        menu::PopupMenuItem,
        notification::Notification,
        scroll::ScrollableElement as _,
        v_flex,
    },
    div,
    prelude::FluentBuilder as _,
    px, uniform_list,
};

use crate::workspace::DragGhost;

use crate::actions::{FILE_TREE_CONTEXT, OpenFolder};
use crate::file_icon::{file_type_icon, folder_type_icon};

/// Entries hidden by default, matching VS Code's `files.exclude`.
pub(crate) const DEFAULT_EXCLUDED: &[&str] =
    &[".git", ".svn", ".hg", "CVS", ".DS_Store", "Thumbs.db"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

struct Row {
    entry: Entry,
    depth: usize,
}

pub enum FileTreeEvent {
    /// A file was clicked: `preview` asks for the slot a plain click reuses,
    /// rather than a tab of its own.
    Open { path: PathBuf, preview: bool },
    /// Files dropped onto the tree were copied into a folder.
    Copied { copied: usize, failed: usize },
    /// An entry (and everything below it) was moved within the tree.
    Moved { from: PathBuf, to: PathBuf },
    /// Entries were deleted from disk (and everything below them).
    Deleted { paths: Vec<PathBuf> },
    /// The space below the entries was double-clicked: VS Code's Explorer
    /// makes a new file in the folder the tree is showing.
    NewFileInRoot,
}

/// Files or folders cut or copied in the tree, waiting for a paste. `cut`
/// remembers which of the two Ctrl+X and Ctrl+C was, so paste moves or
/// copies the way the key that started it says.
#[derive(Clone)]
struct TreeClipboard {
    paths: Vec<PathBuf>,
    cut: bool,
}

/// Files or folders being dragged inside the tree, to move (or, with the
/// invert key, copy) them the way VS Code moves explorer selections.
#[derive(Clone)]
pub struct TreeDrag {
    pub paths: Vec<PathBuf>,
}

/// Whether the drag-and-drop invert key is held while tree entries drag:
/// Ctrl on Windows and Linux, Option on macOS. Held, an in-app drag
/// copies instead of moving, as in VS Code.
/// How long a drag has to rest on a folder before it opens, as in VS Code.
const HOVER_EXPAND_DELAY: Duration = Duration::from_millis(900);
/// How close to an edge a drag has to be before the view scrolls along.
const DRAG_SCROLL_EDGE: f32 = 28.;
/// How often the view moves a row while a drag rests on an edge.
const DRAG_SCROLL_STEP: Duration = Duration::from_millis(80);

/// Which way the view scrolls while a drag rests on an edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DragEdge {
    Up,
    Down,
}

/// Which edge of the list `y` rests on, if any.
fn drag_edge(y: f32, top: f32, bottom: f32) -> Option<DragEdge> {
    if y < top + DRAG_SCROLL_EDGE {
        Some(DragEdge::Up)
    } else if y > bottom - DRAG_SCROLL_EDGE {
        Some(DragEdge::Down)
    } else {
        None
    }
}

pub(crate) fn invert_key_held(modifiers: &Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        modifiers.alt
    } else {
        modifiers.control
    }
}

/// Whether `source` may be dropped into `target_dir`. VS Code rejects
/// invalid targets while dragging, not at drop time: a folder can't go
/// into itself or a descendant, and a drop in the folder an entry already
/// sits in would be a no-op.
fn can_drop_into(source: &Path, target_dir: &Path) -> bool {
    source.parent() != Some(target_dir) && !(source.is_dir() && target_dir.starts_with(source))
}

/// Whether every entry of a selection may be dropped into `target_dir`.
fn can_drop_all(sources: &[PathBuf], target_dir: &Path) -> bool {
    !sources.is_empty()
        && sources
            .iter()
            .all(|source| can_drop_into(source, target_dir))
}

/// Only the top-level entries of a selection: descendants come along
/// with the folder that contains them.
fn top_level(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|path| {
            !paths
                .iter()
                .any(|other| other != *path && path.starts_with(other))
        })
        .cloned()
        .collect()
}

pub struct FileTree {
    root: Option<PathBuf>,
    expanded: HashSet<PathBuf>,
    listings: HashMap<PathBuf, Vec<Entry>>,
    rows: Vec<Row>,
    /// The file the workspace is showing.
    active: Option<PathBuf>,
    /// The rows picked with the multi-select keys, in row order.
    selected: BTreeSet<PathBuf>,
    /// Where Shift-extends starts from.
    anchor: Option<PathBuf>,
    /// The keyboard, so Del, F2, Ctrl+X/C/V and the arrow keys reach the
    /// tree only while it is the focused side of the window.
    focus_handle: FocusHandle,
    /// Entries cut or copied here, waiting for a paste.
    clipboard: Option<TreeClipboard>,
    /// A path to select once the next listing lands, so a file just created
    /// or renamed ends up picked and on screen.
    pending_reveal: Option<PathBuf>,
    /// Whether dotfiles and the default-excluded entries are listed.
    show_hidden: bool,
    /// Shared with the drag-over styles, which run without access to the
    /// tree's state: whether the invert key is held right now.
    drag_copy: Rc<Cell<bool>>,
    /// A collapsed folder a drag is resting on, waiting to open.
    hover_expand: Option<PathBuf>,
    /// The edge a drag is resting on, while the view scrolls along.
    drag_scroll: Option<DragEdge>,
    /// The first row the list is showing, remembered from the last render so
    /// the drag auto-scroll knows where it is. (`UniformListScrollHandle`
    /// only exposes its own index under `cfg(test)`, which a release build
    /// does not have.)
    list_top: usize,
    scroll: UniformListScrollHandle,
    refresh_task: Option<Task<()>>,
    /// Inline F2 rename: path being edited and the input that holds the name.
    renaming: Option<PathBuf>,
    rename_input: Option<Entity<InputState>>,
    _rename_sub: Option<Subscription>,
}

impl EventEmitter<FileTreeEvent> for FileTree {}

impl Focusable for FileTree {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl FileTree {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let this = Self {
            root: None,
            expanded: HashSet::new(),
            listings: HashMap::new(),
            rows: Vec::new(),
            active: None,
            selected: BTreeSet::new(),
            anchor: None,
            focus_handle: cx.focus_handle(),
            clipboard: None,
            pending_reveal: None,
            show_hidden: crate::themes::AppSettings::get(cx).show_hidden_entries,
            drag_copy: Rc::new(Cell::new(false)),
            hover_expand: None,
            drag_scroll: None,
            list_top: 0,
            scroll: UniformListScrollHandle::new(),
            refresh_task: None,
            renaming: None,
            rename_input: None,
            _rename_sub: None,
        };
        cx.observe_global::<crate::themes::AppSettings>(|this, cx| {
            this.set_show_hidden(crate::themes::AppSettings::get(cx).show_hidden_entries, cx);
        })
        .detach();
        this
    }

    /// Apply the "show hidden entries" setting and re-list.
    pub fn set_show_hidden(&mut self, show: bool, cx: &mut Context<Self>) {
        if self.show_hidden == show {
            return;
        }
        self.show_hidden = show;
        self.refresh(cx);
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Show `root`, or nothing.
    pub fn set_root(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.root == root {
            return;
        }
        self.root = root;
        self.expanded.clear();
        self.listings.clear();
        self.rows.clear();
        if let Some(root) = self.root.clone() {
            self.expanded.insert(root);
        }
        // A file opened before its folder was would otherwise lose its way
        // back into the tree.
        self.reveal_active();
        self.refresh(cx);
        cx.notify();
    }

    /// Highlight `path`, expanding the folders above it.
    pub fn set_active(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        self.active = path;
        if self.reveal_active() {
            self.refresh(cx);
        } else {
            self.scroll_to_active();
        }
        cx.notify();
    }

    /// The folders the sidebar has open, for Quick Open's walk.
    pub fn expanded_folders(&self) -> Vec<PathBuf> {
        self.expanded.iter().cloned().collect()
    }

    /// Open every folder above the active file, so it is on screen. Returns
    /// whether that expanded anything, and the tree therefore needs relisting.
    fn reveal_active(&mut self) -> bool {
        match (&self.root, &self.active) {
            (Some(root), Some(active)) => expand_to_reveal(root, active, &mut self.expanded),
            _ => false,
        }
    }

    /// Re-list the root and every expanded folder in the background.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refresh_task.is_some() {
            return;
        }
        let Some(root) = self.root.clone() else {
            return;
        };
        let folders: Vec<PathBuf> = self
            .expanded
            .iter()
            .filter(|folder| folder.starts_with(&root))
            .cloned()
            .collect();
        let show_hidden = self.show_hidden;
        let listing = cx.background_executor().spawn(async move {
            folders
                .into_iter()
                .filter_map(|folder| list(&folder, show_hidden).map(|entries| (folder, entries)))
                .collect::<HashMap<_, _>>()
        });
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let listings = listing.await;
            _ = this.update(cx, |this, cx| {
                this.refresh_task = None;
                // Folders that disappeared collapse.
                this.expanded.retain(|folder| listings.contains_key(folder));
                if listings != this.listings {
                    this.listings = listings;
                    this.rebuild_rows();
                    this.scroll_to_active();
                    if let Some(path) = this.pending_reveal.take() {
                        this.selected = std::iter::once(path.clone()).collect();
                        this.anchor = Some(path.clone());
                        this.scroll_to_path(&path);
                    }
                    cx.notify();
                }
            });
        }));
    }

    fn rebuild_rows(&mut self) {
        fn add(tree: &FileTree, folder: &Path, depth: usize, rows: &mut Vec<Row>) {
            for entry in tree.listings.get(folder).into_iter().flatten() {
                rows.push(Row {
                    entry: entry.clone(),
                    depth,
                });
                if entry.is_dir && tree.expanded.contains(&entry.path) {
                    add(tree, &entry.path, depth + 1, rows);
                }
            }
        }
        let mut rows = Vec::new();
        if let Some(root) = &self.root {
            add(self, root, 0, &mut rows);
        }
        self.rows = rows;
    }

    fn scroll_to_active(&self) {
        if let Some(ix) = self
            .rows
            .iter()
            .position(|row| Some(&row.entry.path) == self.active.as_ref())
        {
            self.scroll
                .scroll_to_item(ix, gpui_kit::ScrollStrategy::Nearest);
        }
    }

    /// Put `path` on screen, if the tree has a row for it.
    fn scroll_to_path(&self, path: &Path) {
        if let Some(ix) = self.rows.iter().position(|row| row.entry.path == path) {
            self.scroll
                .scroll_to_item(ix, gpui_kit::ScrollStrategy::Nearest);
        }
    }

    // ---------------------------------------------------------------------
    // Selection and the keyboard

    /// The row index the selection starts at, if anything is picked.
    fn anchor_row(&self) -> Option<usize> {
        let anchor = self
            .anchor
            .clone()
            .or_else(|| self.selected.iter().next().cloned())?;
        self.rows.iter().position(|row| row.entry.path == anchor)
    }

    /// Pick one row, the way a single click does, and scroll it into view.
    fn select_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix) else {
            return;
        };
        let path = row.entry.path.clone();
        self.selected.clear();
        self.selected.insert(path.clone());
        self.anchor = Some(path.clone());
        self.scroll_to_path(&path);
        cx.notify();
    }

    /// The entries currently picked, in row order.
    pub fn selection(&self) -> Vec<PathBuf> {
        self.selected.iter().cloned().collect()
    }

    /// The one entry picked, with whether it is a folder, when exactly one is.
    pub fn single_selection(&self) -> Option<(PathBuf, bool)> {
        if self.selected.len() != 1 {
            return None;
        }
        let path = self.selected.iter().next()?.clone();
        let is_dir = self
            .rows
            .iter()
            .find(|row| row.entry.path == path)
            .map(|row| row.entry.is_dir)
            .unwrap_or_else(|| path.is_dir());
        Some((path, is_dir))
    }

    /// Where a new file or folder, or a paste, belongs: inside the picked
    /// folder, beside the picked file, or at the root when nothing is picked
    /// — the same choice VS Code's Explorer makes.
    pub fn target_folder(&self) -> Option<PathBuf> {
        let anchor = self
            .selected
            .iter()
            .next()
            .cloned()
            .or_else(|| self.active.clone());
        match anchor {
            Some(path) if path.is_dir() => Some(path),
            Some(path) => path.parent().map(Path::to_path_buf),
            None => self.root.clone(),
        }
    }

    /// Pick the file the workspace is showing, so focusing the Explorer
    /// lands on it, the way VS Code's Ctrl+Shift+E does.
    pub fn select_active(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active.clone() else {
            return;
        };
        if let Some(ix) = self.rows.iter().position(|row| row.entry.path == active) {
            self.select_row(ix, cx);
        }
    }

    pub fn has_clipboard(&self) -> bool {
        self.clipboard.is_some()
    }

    /// Move the selection one row down, as Down does in VS Code's Explorer.
    pub fn select_next(&mut self, cx: &mut Context<Self>) {
        let next = match self.anchor_row() {
            Some(ix) => (ix + 1).min(self.rows.len().saturating_sub(1)),
            None => 0,
        };
        if !self.rows.is_empty() {
            self.select_row(next, cx);
        }
    }

    /// Move the selection one row up.
    pub fn select_previous(&mut self, cx: &mut Context<Self>) {
        let previous = self
            .anchor_row()
            .map(|ix| ix.saturating_sub(1))
            .unwrap_or(0);
        if !self.rows.is_empty() {
            self.select_row(previous, cx);
        }
    }

    pub fn select_first(&mut self, cx: &mut Context<Self>) {
        if !self.rows.is_empty() {
            self.select_row(0, cx);
        }
    }

    pub fn select_last(&mut self, cx: &mut Context<Self>) {
        if !self.rows.is_empty() {
            self.select_row(self.rows.len() - 1, cx);
        }
    }

    /// Left: close an open folder, or step up to the folder that holds the
    /// entry, as VS Code's Explorer does.
    pub fn select_parent(&mut self, cx: &mut Context<Self>) {
        let Some(ix) = self.anchor_row() else {
            return;
        };
        let row = &self.rows[ix];
        let (path, is_dir, expanded) = (
            row.entry.path.clone(),
            row.entry.is_dir,
            self.expanded.contains(&row.entry.path),
        );
        if is_dir && expanded {
            self.toggle(path, cx);
            return;
        }
        let Some(parent) = path.parent() else {
            return;
        };
        if let Some(parent_ix) = self.rows.iter().position(|row| row.entry.path == parent) {
            self.select_row(parent_ix, cx);
        }
    }

    /// Right: open a closed folder, or step into the one already open.
    pub fn select_child(&mut self, cx: &mut Context<Self>) {
        let Some(ix) = self.anchor_row() else {
            return;
        };
        let row = &self.rows[ix];
        if !row.entry.is_dir {
            return;
        }
        let path = row.entry.path.clone();
        if !self.expanded.contains(&path) {
            self.toggle(path, cx);
            return;
        }
        // The first row below it that is nested one level deeper is its
        // first child.
        let depth = row.depth;
        if self
            .rows
            .get(ix + 1)
            .is_some_and(|next| next.depth == depth + 1)
        {
            self.select_row(ix + 1, cx);
        }
    }

    /// Enter on a folder opens or closes it. Files are renamed from the
    /// workspace's Enter binding (VS Code's rename-on-Enter habit); a double
    /// click still opens them from the row click handler.
    pub fn open_selection(&mut self, cx: &mut Context<Self>) {
        let Some(ix) = self.anchor_row() else {
            return;
        };
        let row = &self.rows[ix];
        let (path, is_dir) = (row.entry.path.clone(), row.entry.is_dir);
        if is_dir {
            self.toggle(path, cx);
        }
        // Files: Enter is handled upstream as RenameInTree; keep this a no-op
        // so callers that only want folder expand/collapse stay safe.
    }

    // ---------------------------------------------------------------------
    // Editing files from the tree

    /// Double-clicking the space the entries do not fill: VS Code's Explorer
    /// starts a new file in the folder it is showing, whatever entry happens
    /// to be picked. A click that landed on a row never reaches here — the row
    /// handles its own click and stops it.
    fn new_file_on_empty_space(
        &mut self,
        event: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.root.is_some() && event.click_count() == 2 {
            cx.emit(FileTreeEvent::NewFileInRoot);
        }
    }

    /// Create `name` inside `dir` (a file, or a folder when `is_dir`), then
    /// pick it. Nested names like `notes/plan.md` create the folders on the
    /// way, as VS Code's Explorer does.
    pub fn create_entry(
        &mut self,
        dir: &Path,
        name: &str,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(relative) = sanitize_name(name) else {
            window.push_notification(Notification::info("That name isn’t usable."), cx);
            return;
        };
        let target = dir.join(&relative);
        if target.exists() {
            window.push_notification(Notification::info(format!("“{name}” already exists.")), cx);
            return;
        }
        let created = if is_dir {
            std::fs::create_dir_all(&target)
        } else {
            target
                .parent()
                .map(std::fs::create_dir_all)
                .transpose()
                .and_then(|_| std::fs::write(&target, ""))
        };
        match created {
            Ok(()) => {
                // The folder that gained the entry has to be open for its row
                // to exist.
                self.expanded.insert(dir.to_path_buf());
                self.pending_reveal = Some(target.clone());
                self.refresh(cx);
                if !is_dir {
                    cx.emit(FileTreeEvent::Open {
                        path: target,
                        preview: false,
                    });
                }
            }
            Err(err) => window.push_notification(
                Notification::info(format!("Couldn’t create “{name}”: {err}")),
                cx,
            ),
        }
    }

    /// Start an inline rename on the single selected entry (VS Code Explorer).
    pub fn begin_inline_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((path, is_dir)) = self.single_selection() else {
            return;
        };
        if is_dir && self.root.as_deref() == Some(path.as_path()) {
            return;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(name));
        self._rename_sub = Some(cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let name = input.read(cx).value().to_string();
                    this.commit_inline_rename(name, window, cx);
                }
            },
        ));
        self.renaming = Some(path.clone());
        self.rename_input = Some(input.clone());
        if let Some(ix) = self.rows.iter().position(|r| r.entry.path == path) {
            self.scroll
                .scroll_to_item(ix, gpui_kit::ScrollStrategy::Nearest);
        }
        let focus = input.read(cx).focus_handle(cx);
        focus.focus(window, cx);
        // Select all so typing replaces the name (stem selection is a later polish).
        window.dispatch_action(Box::new(gpui_kit::component::input::SelectAll), cx);
        let _ = is_dir;
        cx.notify();
    }

    pub fn cancel_inline_rename(&mut self, cx: &mut Context<Self>) {
        self.renaming = None;
        self.rename_input = None;
        self._rename_sub = None;
        cx.notify();
    }

    fn commit_inline_rename(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.renaming.clone() else {
            return;
        };
        self.cancel_inline_rename(cx);
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        self.rename_entry(&path, name, window, cx);
    }

    /// Rename `path` to `new_name`, keeping it in its folder. Open documents
    /// follow, because the workspace hears [`FileTreeEvent::Moved`].
    pub fn rename_entry(
        &mut self,
        path: &Path,
        new_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = sanitize_entry_name(new_name) else {
            window.push_notification(Notification::info("That name isn’t usable."), cx);
            return;
        };
        if path.file_name().and_then(|name| name.to_str()) == Some(name.as_str()) {
            return;
        }
        let Some(parent) = path.parent() else {
            return;
        };
        let target = parent.join(&name);
        if target.exists() {
            window.push_notification(Notification::info(format!("“{name}” already exists.")), cx);
            return;
        }
        match move_path(path, &target) {
            Ok(()) => {
                self.pending_reveal = Some(target.clone());
                self.refresh(cx);
                cx.emit(FileTreeEvent::Moved {
                    from: path.to_path_buf(),
                    to: target,
                });
            }
            Err(err) => {
                window.push_notification(Notification::info(format!("Couldn’t rename: {err}")), cx)
            }
        }
    }

    /// Ask before deleting the picked entries, then remove them from disk.
    pub fn delete_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = top_level(&self.selection());
        if paths.is_empty() || window.has_active_dialog(cx) {
            return;
        }
        let title = match paths.len() {
            1 => format!("Delete “{}”?", display_name(&paths[0])),
            count => format!("Delete {count} items?"),
        };
        let tree = cx.weak_entity();
        let targets = paths.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (tree_delete, delete_targets) = (tree.clone(), targets.clone());
            let tree_ok = tree_delete.clone();
            let delete_ok = delete_targets.clone();
            alert
                .title(title.clone())
                .description("They are removed from disk and can’t be recovered.")
                .keyboard(true)
                .on_cancel(|_, window, cx| {
                    window.close_dialog(cx);
                    true
                })
                .on_ok(move |_, window, cx| {
                    window.close_dialog(cx);
                    _ = tree_ok.update(cx, |tree, cx| tree.apply_delete(delete_ok.clone(), cx));
                    true
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
                        .child(Button::new("delete").label("Delete").primary().on_click(
                            move |_, window, cx| {
                                window.close_dialog(cx);
                                _ = tree_delete.update(cx, |tree, cx| {
                                    tree.apply_delete(delete_targets.clone(), cx)
                                });
                            },
                        )),
                )
        });
    }

    fn apply_delete(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let mut deleted = Vec::new();
        for path in paths {
            let removed = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            if removed.is_ok() {
                self.selected.remove(&path);
                if self.anchor.as_deref() == Some(path.as_path()) {
                    self.anchor = None;
                }
                deleted.push(path);
            }
        }
        if !deleted.is_empty() {
            self.refresh(cx);
            cx.emit(FileTreeEvent::Deleted { paths: deleted });
        }
        cx.notify();
    }

    /// Remember the picked entries for a paste, as a cut (Ctrl+X) moves them.
    pub fn cut_selection(&mut self, cx: &mut Context<Self>) {
        let paths = self.selection();
        if paths.is_empty() {
            return;
        }
        self.clipboard = Some(TreeClipboard { paths, cut: true });
        cx.notify();
    }

    /// Remember the picked entries for a paste, as a copy (Ctrl+C).
    pub fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let paths = self.selection();
        if paths.is_empty() {
            return;
        }
        self.clipboard = Some(TreeClipboard { paths, cut: false });
        cx.notify();
    }

    /// Land the clipboard in the target folder: move what was cut, copy what
    /// was copied, the way VS Code's Explorer paste does.
    pub fn paste_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(clipboard) = self.clipboard.clone() else {
            return;
        };
        let Some(target) = self.target_folder() else {
            return;
        };
        let sources = top_level(&clipboard.paths);
        if clipboard.cut {
            for source in sources {
                let Some(name) = source.file_name() else {
                    continue;
                };
                let dest = target.join(name);
                if source == dest || source.parent() == Some(target.as_path()) {
                    continue;
                }
                if source.is_dir() && target.starts_with(&source) {
                    continue;
                }
                match move_path(&source, &dest) {
                    Ok(()) => cx.emit(FileTreeEvent::Moved {
                        from: source,
                        to: dest,
                    }),
                    Err(err) => window
                        .push_notification(Notification::info(format!("Couldn’t move: {err}")), cx),
                }
            }
            self.clipboard = None;
        } else {
            let (mut copied, mut failed) = (0usize, 0usize);
            for source in sources {
                match copy_entry(&source, &target) {
                    Ok(()) => copied += 1,
                    Err(_) => failed += 1,
                }
            }
            cx.emit(FileTreeEvent::Copied { copied, failed });
        }
        self.refresh(cx);
        cx.notify();
    }

    fn toggle(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.expanded.remove(&path) {
            self.expanded.insert(path.clone());
            if !self.listings.contains_key(&path) {
                self.refresh(cx);
                return;
            }
        }
        self.rebuild_rows();
        cx.notify();
    }

    /// Collapse every folder but the root, the way VS Code's "Collapse Folders
    /// in Explorer" does.
    fn collapse_all(&mut self, cx: &mut Context<Self>) {
        if !collapse_to_root(self.root.as_deref(), &mut self.expanded) {
            return;
        }
        self.rebuild_rows();
        self.scroll_to_active();
        cx.notify();
    }

    /// Copy the dropped paths into `target` (VS Code copies an OS drop into
    /// the folder it lands on), then re-list and report the outcome.
    fn copy_paths_into(&mut self, target: &Path, paths: &ExternalPaths, cx: &mut Context<Self>) {
        let (mut copied, mut failed) = (0usize, 0usize);
        for src in paths.paths() {
            match copy_entry(src, target) {
                Ok(()) => copied += 1,
                Err(_) => failed += 1,
            }
        }
        if copied > 0 {
            self.refresh(cx);
        }
        cx.notify();
        cx.emit(FileTreeEvent::Copied { copied, failed });
    }

    /// A drop on the tree's empty space lands in the root folder.
    fn drop_into_root(&mut self, paths: &ExternalPaths, cx: &mut Context<Self>) {
        match self.root.clone() {
            Some(root) => self.copy_paths_into(&root, paths, cx),
            None => cx.emit(FileTreeEvent::Copied {
                copied: 0,
                failed: paths.paths().len(),
            }),
        }
    }

    /// Tree entries dropped on empty space land in the root folder
    /// (copied with the invert key held, otherwise moved).
    fn drop_entry_into_root(
        &mut self,
        sources: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.root.clone() {
            Some(root) => self.drop_tree_entry(sources, &root, window, cx),
            None => cx.emit(FileTreeEvent::Copied {
                copied: 0,
                failed: 1,
            }),
        }
    }

    /// Tree entries dropped on a file row: VS Code rejects the drop; with
    /// the invert key held they copy into the file's folder.
    fn drop_onto_file(&mut self, sources: Vec<PathBuf>, file_dir: &Path, cx: &mut Context<Self>) {
        if !self.drag_copy.get() {
            return;
        }
        let sources = top_level(&sources);
        let mut copied = 0usize;
        let mut failed = 0usize;
        for source in sources {
            if can_drop_into(&source, file_dir) {
                match copy_entry(&source, file_dir) {
                    Ok(()) => copied += 1,
                    Err(_) => failed += 1,
                }
            } else {
                failed += 1;
            }
        }
        if copied > 0 {
            self.refresh(cx);
        }
        cx.emit(FileTreeEvent::Copied { copied, failed });
    }

    /// Open a collapsed folder a drag is resting on, the way VS Code opens
    /// the folder under the pointer after a moment.
    fn note_hover_expand(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        if self.expanded.contains(&folder) {
            self.hover_expand = None;
            return;
        }
        if self
            .hover_expand
            .as_ref()
            .is_some_and(|pending| *pending == folder)
        {
            return;
        }
        self.hover_expand = Some(folder.clone());
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HOVER_EXPAND_DELAY).await;
            _ = this.update(cx, |this, cx| {
                // Cleared either way: the pointer may have moved on, and a
                // later hover waits afresh.
                if this.hover_expand.as_ref() == Some(&folder) {
                    this.hover_expand = None;
                    if cx.has_active_drag() {
                        this.expanded.insert(folder.clone());
                        this.refresh(cx);
                    }
                }
            });
        })
        .detach();
    }

    /// Start, move or stop the view scrolling under a drag that rests on an
    /// edge, the way VS Code's explorer scrolls during a drag.
    fn note_drag_edge(&mut self, edge: Option<DragEdge>, cx: &mut Context<Self>) {
        if self.drag_scroll == edge {
            return;
        }
        let starting = self.drag_scroll.is_none() && edge.is_some();
        self.drag_scroll = edge;
        if starting {
            self.tick_drag_scroll(cx);
        }
    }

    /// Move the view one row and come back for more while the drag stays on
    /// an edge.
    fn tick_drag_scroll(&mut self, cx: &mut Context<Self>) {
        let Some(edge) = self.drag_scroll else {
            return;
        };
        if !cx.has_active_drag() {
            // The drop landed: the chain ends here.
            self.drag_scroll = None;
            return;
        }
        let last = self.rows.len().saturating_sub(1);
        if last > 0 {
            let top = self.list_top.min(last);
            let next = match edge {
                DragEdge::Up => top.saturating_sub(1),
                DragEdge::Down => (top + 1).min(last),
            };
            if next != top {
                self.list_top = next;
                self.scroll
                    .scroll_to_item(next, gpui_kit::ScrollStrategy::Top);
            }
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DRAG_SCROLL_STEP).await;
            _ = this.update(cx, |this, cx| this.tick_drag_scroll(cx));
        })
        .detach();
    }

    /// Record whether the invert key is held right now, so the drag-over
    /// styles (which run without tree state) can reflect copy intent, and
    /// re-render when it flips.
    fn note_drag_intent(&self, modifiers: &Modifiers, cx: &mut Context<Self>) {
        let copy = invert_key_held(modifiers);
        if self.drag_copy.get() != copy {
            self.drag_copy.set(copy);
            cx.notify();
        }
    }

    /// Land dragged tree entries in `target_dir`: copies with the invert
    /// key held, otherwise moves (confirmed first, as in VS Code).
    fn drop_tree_entry(
        &mut self,
        sources: Vec<PathBuf>,
        target_dir: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sources = top_level(&sources);
        if !can_drop_all(&sources, target_dir) {
            // An invalid target stays rejected, exactly as VS Code.
            return;
        }
        if self.drag_copy.get() {
            let mut copied = 0usize;
            let mut failed = 0usize;
            for source in &sources {
                match copy_entry(source, target_dir) {
                    Ok(()) => copied += 1,
                    Err(_) => failed += 1,
                }
            }
            if copied > 0 {
                self.refresh(cx);
            }
            cx.emit(FileTreeEvent::Copied { copied, failed });
            return;
        }
        if crate::themes::AppSettings::get(cx).confirm_drag_and_drop {
            self.confirm_move(sources, target_dir.to_path_buf(), window, cx);
        } else {
            self.apply_moves(sources, target_dir, cx);
        }
    }

    /// Move tree entries into `target_dir` (VS Code moves entries dragged
    /// within the explorer), then re-list and report the outcome.
    fn apply_moves(&mut self, sources: Vec<PathBuf>, target_dir: &Path, cx: &mut Context<Self>) {
        let mut moved = 0usize;
        let mut failed = 0usize;
        for source in sources {
            let Some(name) = source.file_name() else {
                failed += 1;
                continue;
            };
            let dest = target_dir.join(name);
            if source == dest || source.parent() == Some(target_dir) {
                // Already there: nothing to do, as in VS Code.
                continue;
            }
            // A folder can't go inside itself or one of its descendants.
            if source.is_dir() && target_dir.starts_with(&source) {
                failed += 1;
                continue;
            }
            match move_path(&source, &dest) {
                Ok(()) => {
                    moved += 1;
                    cx.emit(FileTreeEvent::Moved {
                        from: source,
                        to: dest,
                    });
                }
                Err(_) => failed += 1,
            }
        }
        if moved > 0 {
            self.refresh(cx);
        }
        if moved == 0 && failed > 0 {
            cx.emit(FileTreeEvent::Copied { copied: 0, failed });
        }
    }

    /// Ask before moving dragged entries, with a "don't ask again" box
    /// that mirrors VS Code's `explorer.confirmDragAndDrop`.
    fn confirm_move(
        &mut self,
        sources: Vec<PathBuf>,
        target_dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let target_name = display_name(&target_dir);
        let title = match sources.len() {
            1 => format!("Move “{}” into “{target_name}”?", display_name(&sources[0])),
            count => format!("Move {count} items into “{target_name}”?"),
        };
        let dont_ask = Rc::new(Cell::new(false));
        let tree = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _cx| {
            let (checkbox, remember) = (dont_ask.clone(), dont_ask.clone());
            let (tree_move, target_move) = (tree.clone(), target_dir.clone());
            let (title, sources_move) = (title.clone(), sources.clone());
            let (tree_ok, target_ok, sources_ok, remember_ok) = (
                tree_move.clone(),
                target_move.clone(),
                sources_move.clone(),
                remember.clone(),
            );
            alert
                .title(title)
                .description(
                    v_flex().gap_3().child(
                        Checkbox::new("dont-ask-move")
                            .label("Don’t ask again when moving files")
                            .checked(dont_ask.get())
                            .on_click(move |checked, window, _| {
                                checkbox.set(*checked);
                                window.refresh();
                            }),
                    ),
                )
                .keyboard(true)
                .on_cancel(|_, window, cx| {
                    window.close_dialog(cx);
                    true
                })
                .on_ok(move |_, window, cx| {
                    window.close_dialog(cx);
                    if remember_ok.get() {
                        crate::themes::AppSettings::update(cx, |settings| {
                            settings.confirm_drag_and_drop = false
                        });
                    }
                    _ = tree_ok.update(cx, |tree, cx| {
                        tree.apply_moves(sources_ok.clone(), &target_ok, cx)
                    });
                    true
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
                        .child(Button::new("move").label("Move").primary().on_click(
                            move |_, window, cx| {
                                window.close_dialog(cx);
                                if remember.get() {
                                    crate::themes::AppSettings::update(cx, |settings| {
                                        settings.confirm_drag_and_drop = false
                                    });
                                }
                                _ = tree_move.update(cx, |tree, cx| {
                                    tree.apply_moves(sources_move.clone(), &target_move, cx)
                                });
                            },
                        )),
                )
        });
    }

    /// Select one entry, clearing the rest. Shift extends from here.
    fn select_only(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.selected.len() == 1 && self.selected.contains(path) {
            return;
        }
        self.selected.clear();
        self.selected.insert(path.to_path_buf());
        self.anchor = Some(path.to_path_buf());
        cx.notify();
    }

    /// Toggle one entry in the selection (the secondary key in VS Code).
    fn toggle_selected(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.selected.contains(path) {
            self.selected.remove(path);
        } else {
            self.selected.insert(path.to_path_buf());
        }
        self.anchor = Some(path.to_path_buf());
        cx.notify();
    }

    /// Extend the selection from the anchor through `path` (Shift-click).
    fn select_range(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(anchor) = self.anchor.clone() else {
            self.select_only(path, cx);
            return;
        };
        let from = self.rows.iter().position(|row| row.entry.path == anchor);
        let to = self.rows.iter().position(|row| row.entry.path == path);
        let (Some(from), Some(to)) = (from, to) else {
            self.select_only(path, cx);
            return;
        };
        let (lo, hi) = (from.min(to), from.max(to));
        self.selected = self.rows[lo..=hi]
            .iter()
            .map(|row| row.entry.path.clone())
            .collect();
        cx.notify();
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui_kit::AnyElement> {
        let theme = cx.theme();
        let (hover, active_bg, muted, fg, marker) = (
            theme.list_hover,
            theme.list_active,
            theme.muted_foreground,
            theme.foreground,
            theme.primary,
        );
        self.list_top = range.start;
        let renaming_path = self.renaming.clone();
        let rename_input = self.rename_input.clone();
        range
            .filter_map(|ix| {
                let row = self.rows.get(ix)?;
                let entry = row.entry.clone();
                let is_active = Some(&entry.path) == self.active.as_ref();
                let expanded = entry.is_dir && self.expanded.contains(&entry.path);
                // Theme SVGs (VS Code icon package) when mapped; Lucide fallback.
                let type_icon = if entry.is_dir {
                    folder_type_icon(&entry.path, expanded)
                } else {
                    file_type_icon(&entry.path)
                };
                let path = entry.path.clone();
                let drop_path = entry.path.clone();
                let drag_name = entry.name.clone();
                // Dragging a selected entry drags the whole selection, the
                // way VS Code drags the explorer's selection.
                let (drag_paths, drag_count) = if self.selected.contains(&path) {
                    let count = self.selected.len();
                    (self.selected.iter().cloned().collect::<Vec<_>>(), count)
                } else {
                    (vec![path.clone()], 1)
                };
                let ghost_label = if drag_count > 1 {
                    format!("{drag_name} +{}", drag_count - 1)
                } else {
                    drag_name.clone()
                };
                let is_selected = self.selected.contains(&path);
                let click_path = entry.path.clone();
                let click_is_dir = entry.is_dir;
                let right_path = entry.path.clone();
                // The Explorer's menu acts on what is picked; the root is
                // never renamed or deleted.
                let at_root = self.root.as_deref() == Some(entry.path.as_path());
                let can_paste = self.has_clipboard();
                let menu_focus = self.focus_handle.clone();
                Some(
                    h_flex()
                        .id(ix)
                        .h(px(26.))
                        .pl(px(8. + 14. * row.depth as f32))
                        .pr_2()
                        .gap_1p5()
                        .rounded_md()
                        .text_sm()
                        .cursor_pointer()
                        .when(is_active, |this| {
                            // On the selection background the row keeps the
                            // normal text colour: painting it in the accent
                            // colour made the open file the least readable
                            // row in the tree — invisible in the light theme,
                            // dimmer than its neighbours in the dark one.
                            this.bg(active_bg)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(fg)
                        })
                        .when(!is_active && is_selected, |this| this.bg(hover))
                        .when(!is_active && !is_selected, |this| {
                            this.hover(move |style| style.bg(hover))
                        })
                        // Entries drag within the tree to move, the way
                        // VS Code's explorer does.
                        .on_drag(TreeDrag { paths: drag_paths }, move |_, _, _, cx| {
                            cx.new(|_| DragGhost {
                                label: ghost_label.clone().into(),
                            })
                        })
                        .child(
                            div()
                                .w(px(12.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .when(entry.is_dir, |this| {
                                    this.child(
                                        Icon::new(if expanded {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        })
                                        .xsmall()
                                        .text_color(muted),
                                    )
                                })
                                // A dot in the same slot marks the file the
                                // window is showing, the way the folder
                                // chevron marks an open folder. `primary` is
                                // the theme's strongest colour, so it reads on
                                // the selection background in either theme.
                                .when(is_active && !entry.is_dir, |this| {
                                    this.child(div().w(px(6.)).h(px(6.)).rounded(px(3.)).bg(marker))
                                }),
                        )
                        .child(type_icon.small().flex_none())
                        .child(if renaming_path.as_ref() == Some(&entry.path) {
                            if let Some(input) = rename_input.clone() {
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .on_action(cx.listener(|this, _: &Escape, _, cx| {
                                        this.cancel_inline_rename(cx);
                                    }))
                                    .child(Input::new(&input).appearance(false))
                                    .into_any_element()
                            } else {
                                div()
                                    .truncate()
                                    .child(SharedString::from(entry.name.clone()))
                                    .into_any_element()
                            }
                        } else {
                            div()
                                .truncate()
                                .child(SharedString::from(entry.name.clone()))
                                .into_any_element()
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                // Shift extends the selection and the
                                // secondary key (Ctrl on Windows and Linux,
                                // Command on macOS) toggles one entry; a
                                // plain click selects one. Opening waits for
                                // the release, so a press that turns into a
                                // drag never opens the file it moves.
                                this.focus_handle.focus(window, cx);
                                if event.modifiers.shift {
                                    this.select_range(&path, cx);
                                } else if event.modifiers.secondary() {
                                    this.toggle_selected(&path.clone(), cx);
                                } else {
                                    this.select_only(&path, cx);
                                }
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, _, window, cx| {
                                // A right click picks the row before the menu
                                // opens, the way VS Code's Explorer does.
                                this.focus_handle.focus(window, cx);
                                this.select_only(&right_path, cx);
                            }),
                        )
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                                // The row's click stops here, so the space
                                // around the entries (which starts a new file
                                // on a double click) never sees one that
                                // landed on a row.
                                cx.stop_propagation();
                                if cx.has_active_drag()
                                    || event.modifiers.shift
                                    || event.modifiers.secondary()
                                {
                                    return;
                                }
                                if click_is_dir {
                                    this.toggle(click_path.clone(), cx);
                                } else {
                                    // One click shows the file in the slot
                                    // the next click replaces; a double
                                    // click keeps it, as in VS Code.
                                    cx.emit(FileTreeEvent::Open {
                                        path: click_path.clone(),
                                        preview: event.click_count < 2,
                                    });
                                }
                            }),
                        )
                        // A drop on a folder copies an OS drop into it, and
                        // moves tree entries into it (copies with the
                        // invert key held). Illegal targets stay
                        // unhighlighted and reject the drop, as in VS Code.
                        .when(entry.is_dir, |this| {
                            let folder = drop_path.clone();
                            let folder_for_drop = folder.clone();
                            let folder_for_style = folder.clone();
                            let folder_for_hover = folder.clone();
                            this.on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                                this.copy_paths_into(&folder, paths, cx)
                            }))
                            .drag_over::<ExternalPaths>(|style, _, _, cx| {
                                style.bg(cx.theme().drop_target)
                            })
                            .on_drop(cx.listener(move |this, drag: &TreeDrag, window, cx| {
                                this.drop_tree_entry(
                                    drag.paths.clone(),
                                    &folder_for_drop,
                                    window,
                                    cx,
                                )
                            }))
                            .drag_over::<TreeDrag>(move |style, drag: &TreeDrag, _, cx| {
                                if can_drop_all(&top_level(&drag.paths), &folder_for_style) {
                                    style.bg(cx.theme().drop_target)
                                } else {
                                    style
                                }
                            })
                            .on_drag_move::<TreeDrag>(cx.listener(
                                move |this, event: &DragMoveEvent<TreeDrag>, _, cx| {
                                    // Resting on a folder that would take the
                                    // drag opens it, as VS Code does.
                                    if can_drop_all(
                                        &top_level(&event.drag(cx).paths),
                                        &folder_for_hover,
                                    ) {
                                        this.note_hover_expand(folder_for_hover.clone(), cx);
                                    }
                                },
                            ))
                        })
                        // A drop on a file row is rejected, as in VS Code;
                        // with the invert key it copies into the file's
                        // folder.
                        .when(!entry.is_dir, |this| {
                            let parent = entry.path.parent().map(Path::to_path_buf);
                            let parent_for_drop = parent.clone();
                            let parent_for_style = parent.clone();
                            let intent = self.drag_copy.clone();
                            this.on_drop(cx.listener(move |this, drag: &TreeDrag, _, cx| {
                                if let Some(parent) = parent_for_drop.clone() {
                                    this.drop_onto_file(drag.paths.clone(), &parent, cx);
                                }
                            }))
                            .drag_over::<TreeDrag>(
                                move |style, drag: &TreeDrag, _, cx| {
                                    let copy = intent.get();
                                    if copy
                                        && parent_for_style.as_deref().is_some_and(|parent| {
                                            can_drop_all(&top_level(&drag.paths), parent)
                                        })
                                    {
                                        style.bg(cx.theme().drop_target)
                                    } else {
                                        style
                                    }
                                },
                            )
                        })
                        // The Explorer's right-click menu: the same commands
                        // the keyboard has, acting on the picked entries. The
                        // menu hands focus back to the tree before it
                        // dispatches, so the workspace hears the action.
                        .context_menu(move |menu, _, _| {
                            menu.action_context(menu_focus.clone())
                                .item(
                                    PopupMenuItem::new("New File…")
                                        .action(Box::new(crate::actions::NewFileInTree)),
                                )
                                .item(
                                    PopupMenuItem::new("New Folder…")
                                        .action(Box::new(crate::actions::NewFolderInTree)),
                                )
                                .separator()
                                .item(
                                    PopupMenuItem::new("Open to the Side")
                                        .disabled(at_root)
                                        .action(Box::new(crate::actions::OpenToSideInTree)),
                                )
                                .item(
                                    PopupMenuItem::new("Select for Compare")
                                        .disabled(at_root)
                                        .action(Box::new(crate::actions::SelectForCompare)),
                                )
                                .item(
                                    PopupMenuItem::new("Compare with Selected")
                                        .disabled(at_root)
                                        .action(Box::new(crate::actions::CompareWithSelected)),
                                )
                                .separator()
                                .item(
                                    PopupMenuItem::new("Rename…")
                                        .disabled(at_root)
                                        .action(Box::new(crate::actions::RenameInTree)),
                                )
                                .item(
                                    PopupMenuItem::new("Delete")
                                        .disabled(at_root)
                                        .action(Box::new(crate::actions::DeleteInTree)),
                                )
                                .separator()
                                .item(
                                    PopupMenuItem::new("Cut")
                                        .action(Box::new(crate::actions::CutInTree)),
                                )
                                .item(
                                    PopupMenuItem::new("Copy")
                                        .action(Box::new(crate::actions::CopyInTree)),
                                )
                                .item(
                                    PopupMenuItem::new("Paste")
                                        .disabled(!can_paste)
                                        .action(Box::new(crate::actions::PasteInTree)),
                                )
                                .separator()
                                .item(
                                    PopupMenuItem::new("Copy Path")
                                        .action(Box::new(crate::actions::CopyPathInTree)),
                                )
                                .item(
                                    PopupMenuItem::new("Copy Relative Path")
                                        .action(Box::new(crate::actions::CopyRelativePathInTree)),
                                )
                                .item(
                                    PopupMenuItem::new("Reveal in File Manager")
                                        .action(Box::new(crate::actions::RevealPathInTree)),
                                )
                        })
                        .into_any_element(),
                )
            })
            .collect()
    }
}

impl Render for FileTree {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _focused = self.focus_handle.contains_focused(window, cx);
        let theme = cx.theme();
        let name = self
            .root
            .as_ref()
            .and_then(|root| root.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "Files".to_string());

        let full_path: SharedString = self
            .root
            .as_ref()
            .map(|root| root.display().to_string())
            .unwrap_or_default()
            .into();
        // As in VS Code: the folder's name in small capitals, its full path
        // on hover, and the header's actions only while the pointer is over
        // it, so a narrow sidebar has room for the name.
        let header = h_flex()
            .group("tree-header")
            .gap_1()
            .h(px(32.))
            .pl_3()
            .pr_1()
            .flex_none()
            .justify_between()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.muted_foreground)
            .child({
                let tip = if full_path.is_empty() {
                    SharedString::from("")
                } else {
                    SharedString::from(format!("{full_path} — click to copy"))
                };
                let root_for_copy = self.root.clone();
                div()
                    .id("tree-root-name")
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .when(!full_path.is_empty(), |el| {
                        el.cursor_pointer()
                            .hover(|s| s.text_color(theme.foreground))
                            .tooltip(move |window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new(tip.clone())
                                    .build(window, cx)
                            })
                            .on_click(cx.listener(move |_, _, window, cx| {
                                let Some(root) = root_for_copy.clone() else {
                                    return;
                                };
                                let abs = root
                                    .canonicalize()
                                    .unwrap_or(root)
                                    .to_string_lossy()
                                    .into_owned();
                                cx.write_to_clipboard(ClipboardItem::new_string(abs.clone()));
                                window.push_notification(
                                    Notification::info(format!("Copied {abs}")),
                                    cx,
                                );
                            }))
                    })
                    .child(name.to_uppercase())
            })
            .relative()
            // The actions float over the end of the name instead of keeping
            // their width while hidden, which cut a long name short.
            .child(
                h_flex()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right_1()
                    .pl_1()
                    .bg(theme.sidebar)
                    .items_center()
                    // New File / New Folder stay visible (round-4): hover-only
                    // hid them from probes that never hovered the header.
                    .child(
                        Button::new("new-file")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::FilePlus))
                            .tooltip("New file")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(crate::actions::NewFileInTree), cx)
                            }),
                    )
                    .child(
                        Button::new("new-folder")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::FolderPlus))
                            .tooltip("New folder")
                            .on_click(|_, window, cx| {
                                window
                                    .dispatch_action(Box::new(crate::actions::NewFolderInTree), cx)
                            }),
                    )
                    .child(
                        Button::new("refresh-folders")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::RefreshCw))
                            .tooltip("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    )
                    .child(
                        Button::new("collapse-folders")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::ChevronsDownUp))
                            .tooltip("Collapse folders")
                            .on_click(cx.listener(|this, _, _, cx| this.collapse_all(cx))),
                    )
                    .child(
                        Button::new("open-folder")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::FolderOpen))
                            .tooltip("Open folder…")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(OpenFolder), cx)
                            }),
                    ),
            );

        let body = if self.root.is_none() {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .p_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("No folder open")
                .child(
                    Button::new("open-folder-empty")
                        .small()
                        .label("Open folder…")
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenFolder), cx)),
                )
                .into_any_element()
        } else if self.rows.is_empty() && self.refresh_task.is_none() {
            div()
                .id("file-tree-empty")
                .flex_1()
                .min_h_0()
                .p_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("No Markdown files here")
                .on_click(cx.listener(Self::new_file_on_empty_space))
                .into_any_element()
        } else {
            div()
                .id("file-tree-body")
                .flex_1()
                .min_h_0()
                .px_1()
                // The space the entries do not fill is the folder itself, as
                // in VS Code: double-clicking it starts a new file there.
                .on_click(cx.listener(Self::new_file_on_empty_space))
                .child(
                    uniform_list(
                        "file-tree-rows",
                        self.rows.len(),
                        cx.processor(Self::render_rows),
                    )
                    .size_full()
                    .track_scroll(&self.scroll),
                )
                .vertical_scrollbar(&self.scroll)
                .into_any_element()
        };

        v_flex()
            .id("file-tree")
            .key_context(FILE_TREE_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(theme.sidebar)
            .on_drop(
                cx.listener(|this, paths: &ExternalPaths, _, cx| this.drop_into_root(paths, cx)),
            )
            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            // Empty space accepts tree entries into the root folder; VS
            // Code shows no highlight for it, so none is drawn here.
            .on_drop(cx.listener(|this, drag: &TreeDrag, window, cx| {
                this.drop_entry_into_root(drag.paths.clone(), window, cx)
            }))
            .on_drag_move::<TreeDrag>(cx.listener(
                |this, event: &DragMoveEvent<TreeDrag>, _, cx| {
                    this.note_drag_intent(&event.event.modifiers, cx);
                    let edge = drag_edge(
                        event.event.position.y.as_f32(),
                        event.bounds.origin.y.as_f32(),
                        (event.bounds.origin.y + event.bounds.size.height).as_f32(),
                    );
                    this.note_drag_edge(edge, cx);
                },
            ))
            .child(header)
            .child(body)
    }
}

/// Keep only the root expanded; returns whether anything actually collapsed.
fn collapse_to_root(root: Option<&Path>, expanded: &mut HashSet<PathBuf>) -> bool {
    let before = expanded.len();
    expanded.retain(|folder| Some(folder.as_path()) == root);
    expanded.len() != before
}

/// Open every folder above `path` that sits under `root`, so the file is on
/// screen. Returns whether `expanded` gained anything.
fn expand_to_reveal(root: &Path, path: &Path, expanded: &mut HashSet<PathBuf>) -> bool {
    let mut revealed = false;
    let mut dir = path.parent();
    while let Some(folder) = dir {
        if !folder.starts_with(root) {
            break;
        }
        revealed |= expanded.insert(folder.to_path_buf());
        dir = folder.parent();
    }
    revealed
}

/// A path's file name for dialog text, falling back to the whole path.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Absolute path text for the clipboard: the OS display form, one path per
/// line when several entries are picked (as in VS Code). Win32 `\\?\`
/// prefixes are stripped so paste targets get a normal `D:\…` path.
pub(crate) fn absolute_paths_for_clipboard(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| crate::paths::path_for_clipboard(path))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Path relative to the open folder root, using `/` separators. Falls back to
/// the absolute display form when `path` is outside `root` or `root` is missing.
pub(crate) fn relative_path_for_clipboard(path: &Path, root: Option<&Path>) -> String {
    match root.and_then(|root| path.strip_prefix(root).ok()) {
        Some(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().replace('\\', "/"),
        // The root folder itself: show its name, not an empty string.
        Some(_) => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        None => crate::paths::path_for_clipboard(path),
    }
}

/// Relative paths for every picked entry, joined the same way as absolute.
pub(crate) fn relative_paths_for_clipboard(paths: &[PathBuf], root: Option<&Path>) -> String {
    paths
        .iter()
        .map(|path| relative_path_for_clipboard(path, root))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The name typed into New File / New Folder, as a path relative to the
/// folder it goes in. `None` for anything empty, absolute or stepping up a
/// level; a nested name like `notes/plan.md` is allowed and keeps its shape.
fn sanitize_name(name: &str) -> Option<PathBuf> {
    let trimmed = name.trim().trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return None;
    }
    let path = Path::new(trimmed);
    let usable = !path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)));
    usable.then(|| path.to_path_buf())
}

/// The name typed into Rename: one entry, no folders and no stepping up.
fn sanitize_entry_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    let usable =
        !trimmed.is_empty() && trimmed != "." && trimmed != ".." && !trimmed.contains(['/', '\\']);
    usable.then(|| trimmed.to_string())
}

/// Move `source` to `dest`, falling back to copy + delete when the rename
/// crosses filesystems.
fn move_path(source: &Path, dest: &Path) -> std::io::Result<()> {
    if std::fs::rename(source, dest).is_ok() {
        return Ok(());
    }
    let target = dest.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination has no parent",
        )
    })?;
    copy_entry(source, target)?;
    if source.is_dir() {
        std::fs::remove_dir_all(source)
    } else {
        std::fs::remove_file(source)
    }
}

/// Copy `src` into the `target` folder, recursing into directories.
fn copy_entry(src: &Path, target: &Path) -> std::io::Result<()> {
    let name = src.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;
    let dst = target.join(name);
    if src == dst {
        return Ok(());
    }
    if src.is_dir() {
        // Copying a folder into itself (or a folder below it) would recurse.
        if target.starts_with(src) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cannot copy a folder into itself",
            ));
        }
        copy_dir_all(src, &dst)
    } else {
        std::fs::copy(src, &dst).map(|_| ())
    }
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let dst = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_all(&path, &dst)?;
        } else {
            std::fs::copy(&path, &dst)?;
        }
    }
    Ok(())
}

/// A folder's subfolders and Markdown files, folders first, by name.
fn list(folder: &Path, show_hidden: bool) -> Option<Vec<Entry>> {
    let mut entries: Vec<Entry> = std::fs::read_dir(folder)
        .ok()?
        .flatten()
        .filter_map(|item| {
            let name = item.file_name().to_string_lossy().to_string();
            let path = item.path();
            // Follow links, so linked folders and files show as what they point to.
            let is_dir = path.is_dir();
            // VS Code's default excluded entries stay hidden until
            // "show hidden entries" is turned on; everything else shows.
            let excluded = !show_hidden && DEFAULT_EXCLUDED.contains(&name.as_str());
            (!excluded).then_some(Entry { name, path, is_dir })
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Some(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_name_is_kept_inside_the_folder_it_goes_in() {
        // A plain name, and a nested one the way VS Code's Explorer accepts.
        assert_eq!(sanitize_name("plan.md"), Some(PathBuf::from("plan.md")));
        assert_eq!(
            sanitize_name(" notes/plan.md "),
            Some(PathBuf::from("notes/plan.md"))
        );
        // Nothing, an absolute path and a step upwards are all refused.
        assert_eq!(sanitize_name(""), None);
        assert_eq!(sanitize_name("   "), None);
        assert_eq!(sanitize_name("/etc/passwd"), None);
        assert_eq!(sanitize_name("../escape.md"), None);
    }

    #[test]
    fn a_rename_is_one_entry_and_nothing_else() {
        assert_eq!(sanitize_entry_name("plan.md").as_deref(), Some("plan.md"));
        assert_eq!(sanitize_entry_name(" notes ").as_deref(), Some("notes"));
        assert_eq!(sanitize_entry_name(".."), None);
        assert_eq!(sanitize_entry_name("."), None);
        // A rename never moves the entry, so a path is not a name.
        assert_eq!(sanitize_entry_name("a/b.md"), None);
        assert_eq!(sanitize_entry_name(""), None);
    }

    #[test]
    fn lists_folders_first_and_everything_else() {
        let dir = std::env::temp_dir().join(format!("malgel-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for folder in ["b-notes", "node_modules", ".git"] {
            std::fs::create_dir_all(dir.join(folder)).unwrap();
        }
        for file in [
            "Zed.md",
            "alpha.markdown",
            "image.png",
            ".hidden.md",
            "todo.txt",
        ] {
            std::fs::write(dir.join(file), "").unwrap();
        }
        let names: Vec<String> = list(&dir, false)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(
            names,
            [
                "b-notes",
                "node_modules",
                ".hidden.md",
                "alpha.markdown",
                "image.png",
                "todo.txt",
                "Zed.md"
            ]
        );
        let all: Vec<String> = list(&dir, true)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(all.contains(&".git".to_string()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn collapse_to_root_keeps_only_the_root_expanded() {
        let root = Path::new("/work/notes");
        let mut expanded: HashSet<PathBuf> = [
            root,
            Path::new("/work/notes/2026"),
            Path::new("/work/notes/2026/feb"),
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect();

        assert!(collapse_to_root(Some(root), &mut expanded));
        let expected: HashSet<PathBuf> = [root.to_path_buf()].into_iter().collect();
        assert_eq!(expanded, expected);
        // A second press has nothing left to collapse.
        assert!(!collapse_to_root(Some(root), &mut expanded));
    }

    #[test]
    fn drag_edge_wants_the_pointer_near_an_end() {
        // A 400px list: the first and last 28px scroll the view along.
        assert_eq!(drag_edge(0., 0., 400.), Some(DragEdge::Up));
        assert_eq!(drag_edge(27., 0., 400.), Some(DragEdge::Up));
        assert_eq!(drag_edge(28., 0., 400.), None);
        assert_eq!(drag_edge(200., 0., 400.), None);
        assert_eq!(drag_edge(372., 0., 400.), None);
        assert_eq!(drag_edge(373., 0., 400.), Some(DragEdge::Down));
        assert_eq!(drag_edge(400., 0., 400.), Some(DragEdge::Down));
        // A list that starts lower down works off its own top edge.
        assert_eq!(drag_edge(100., 100., 400.), Some(DragEdge::Up));
        assert_eq!(drag_edge(200., 100., 400.), None);
    }

    #[test]
    fn expand_to_reveal_opens_the_folders_above_a_file() {
        let root = Path::new("/work/notes");
        let file = Path::new("/work/notes/2026/feb/plan.md");
        let mut expanded: HashSet<PathBuf> = [root.to_path_buf()].into_iter().collect();

        // Both folders on the way down, so the file has a row to sit on.
        assert!(expand_to_reveal(root, file, &mut expanded));
        let expected: HashSet<PathBuf> = [
            root.to_path_buf(),
            PathBuf::from("/work/notes/2026"),
            PathBuf::from("/work/notes/2026/feb"),
        ]
        .into_iter()
        .collect();
        assert_eq!(expanded, expected);
        // A second look has everything it needs already open.
        assert!(!expand_to_reveal(root, file, &mut expanded));
    }

    #[test]
    fn expand_to_reveal_leaves_everything_else_alone() {
        let root = Path::new("/work/notes");
        let mut expanded: HashSet<PathBuf> = [root.to_path_buf()].into_iter().collect();

        // A file outside the open folder, and the folder itself, expand nothing.
        assert!(!expand_to_reveal(
            root,
            Path::new("/elsewhere/x.md"),
            &mut expanded
        ));
        assert!(!expand_to_reveal(root, root, &mut expanded));
        assert_eq!(
            expanded,
            [root.to_path_buf()].into_iter().collect::<HashSet<_>>()
        );
    }

    #[test]
    fn clipboard_paths_are_absolute_or_relative_to_the_root() {
        let root = Path::new("/work/notes");
        let nested = Path::new("/work/notes/2026/feb/plan.md");
        assert_eq!(
            relative_path_for_clipboard(nested, Some(root)),
            "2026/feb/plan.md"
        );
        assert_eq!(relative_path_for_clipboard(root, Some(root)), "notes");
        // Outside the folder, or with no root, the absolute form is kept.
        assert_eq!(
            relative_path_for_clipboard(Path::new("/elsewhere/x.md"), Some(root)),
            "/elsewhere/x.md"
        );
        assert_eq!(
            relative_path_for_clipboard(nested, None),
            "/work/notes/2026/feb/plan.md"
        );
        let paths = vec![
            PathBuf::from("/work/notes/a.md"),
            PathBuf::from("/work/notes/b/c.md"),
        ];
        assert_eq!(
            absolute_paths_for_clipboard(&paths),
            "/work/notes/a.md\n/work/notes/b/c.md"
        );
        assert_eq!(
            relative_paths_for_clipboard(&paths, Some(root)),
            "a.md\nb/c.md"
        );
        // Win32 extended-length paths must not leak onto the clipboard.
        let win = vec![PathBuf::from(r"\\?\D:\work\leo\glint.svg")];
        assert_eq!(
            absolute_paths_for_clipboard(&win),
            r"D:\work\leo\glint.svg"
        );
    }
}
