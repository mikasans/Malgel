//! The command palette: one searchable list of every command, the way VS
//! Code's Show All Commands (Ctrl+Shift+P) offers them.
//!
//! Every command is already an Action (see [`crate::actions`]), so the palette
//! adds no new dispatch path — it only groups the commands the way VS Code
//! does and hands each one to `CommandItem` (with its Action) so the row can
//! draw the command's current key binding beside the label.
//!
//! The list is built as plain data ([`CommandGroupSpec`]) rather than as
//! palette widgets directly, because the palette lives in a dialog: an action
//! confirmed there is dispatched along the dialog's own focus path, which the
//! window's workspace is not part of, so the widget's own dispatch never
//! arrives. The workspace therefore builds the rows from these specs and
//! dispatches the confirmed row's action itself — which is why each row's
//! Action travels beside its label instead of inside the widget.

use gpui_kit::{
    Action,
    assets::IconName,
    component::{
        Icon,
        command::{CommandGroup, CommandItem},
    },
};

use crate::{
    actions::*,
    settings::{Appearance, Layout},
};

/// One command as the palette offers it: what it reads as, its icon, and the
/// Action that runs when the row is confirmed.
pub struct CommandSpec {
    pub label: &'static str,
    pub icon: IconName,
    pub action: Box<dyn Action>,
}

/// A named group of commands, in the order the palette shows them.
pub struct CommandGroupSpec {
    pub label: &'static str,
    pub items: Vec<CommandSpec>,
}

fn cmd(label: &'static str, icon: IconName, action: impl Action + 'static) -> CommandSpec {
    CommandSpec {
        label,
        icon,
        action: Box::new(action),
    }
}

fn group(label: &'static str, items: Vec<CommandSpec>) -> CommandGroupSpec {
    CommandGroupSpec { label, items }
}

/// The rows the palette draws: labels, icons, and each command's Action so
/// `CommandItem` can resolve and paint the keybinding tip (Ctrl+`, Fold, …).
/// Confirm still dispatches via [`actions`] from the workspace — the dialog
/// focus path does not reach the workspace on its own.
pub fn palette(groups: &[CommandGroupSpec]) -> Vec<CommandGroup> {
    groups
        .iter()
        .map(|group| {
            group
                .items
                .iter()
                .fold(CommandGroup::new().label(group.label), |group, item| {
                    group.item(
                        CommandItem::new()
                            .label(item.label)
                            .keywords(keywords(item.label))
                            .icon(Icon::new(item.icon))
                            .action(item.action.boxed_clone()),
                    )
                })
        })
        .collect()
}

/// What a row answers to besides its label: VS Code's names for it.
fn keywords(label: &'static str) -> Vec<&'static str> {
    match label {
        "Color theme…" => vec![label, "theme", "colour theme", "preferences color theme"],
        "Fold" => vec![label, "fold region", "collapse"],
        "Unfold" => vec![label, "unfold region", "expand fold"],
        // Round-4: palette query "search" must hit Find in files (Ctrl+Shift+F).
        "Find in files…" => vec![
            label,
            "search",
            "search in files",
            "find in folder",
            "workspace search",
        ],
        "Split Editor Right" => vec![
            label,
            "split",
            "split right",
            "split editor",
            "open to the side",
        ],
        "Split Editor Down" => vec![label, "split", "split down", "split editor", "open below"],
        "Keyboard Shortcuts" => {
            vec![
                label,
                "keyboard",
                "shortcut",
                "shortcuts",
                "keybindings",
                "hotkeys",
                "help",
                "help keyboard",
            ]
        }
        // Probe #6: chords work; palette queries must too.
        "Replace…" => vec![label, "replace", "find replace", "search replace"],
        "Go to line…" => vec![label, "goto", "goto line", "go to line", "go to"],
        "Save All" => vec![label, "save all", "saveall"],
        "Open Recent…" => vec![
            label,
            "open recent",
            "recent",
            "recent files",
            "recent folders",
        ],
        "Toggle Maximized Panel" => {
            vec![
                label,
                "maximize",
                "maximize panel",
                "maximized panel",
                "toggle maximized panel",
            ]
        }
        "Outline" => vec![
            label,
            "outline",
            "outline view",
            "outline pane",
            "table of contents",
            "headings",
            "symbol pane",
        ],
        // Probe #7: discoverability for markdown / toggle preview phrasing.
        "Preview" => vec![
            label,
            "markdown preview",
            "open preview",
            "preview markdown",
            "md preview",
        ],
        "Editor and preview" => vec![
            label,
            "markdown preview",
            "open preview",
            "split preview",
            "preview",
        ],
        "Toggle editor and preview" => vec![
            label,
            "toggle preview",
            "markdown preview",
            "open preview",
            "preview",
            "md preview",
        ],
        "Problems" => vec![label, "problems", "diagnostics", "errors", "warnings"],
        "Toggle Minimap" => vec![label, "minimap", "toggle minimap", "code minimap"],
        "Settings…" => vec![
            label,
            "settings",
            "open settings",
            "preferences",
            "open preferences",
        ],
        "Kill Terminal" => vec![label, "kill terminal", "kill", "stop terminal"],
        "New Terminal" => vec![label, "new terminal", "create terminal"],
        "Clear Terminal" => vec![label, "clear", "clear terminal", "clear output"],
        "Split Terminal" => vec![label, "split terminal", "terminal split"],
        "Go to Symbol…" => vec![
            label,
            "go to symbol",
            "goto symbol",
            "symbol",
            "symbols",
            // Probe #13: do not match bare query `outline` (Outline ranks first).
        ],
        "Reload Window" => vec![label, "reload window", "reload", "restart window"],
        "Zen Mode" => vec![label, "zen", "zen mode", "distraction free"],
        "Compare with Saved" => vec![
            label,
            "compare",
            "diff",
            "compare with saved",
            "diff with saved",
        ],
        "Select for Compare" => vec![label, "select for compare", "compare select", "diff select"],
        "Compare with Selected" => vec![
            label,
            "compare with selected",
            "compare selected",
            "diff with selected",
            "compare",
        ],
        "Source Control" => vec![label, "git", "scm", "source control", "version control"],
        "Extensions" => vec![label, "extensions", "extension", "plugins", "marketplace"],
        "Pin Tab" => vec![label, "pin", "unpin", "pin tab", "unpin tab", "pin editor"],
        "Pin Preview" => vec![label, "pin preview", "unpin preview", "freeze preview", "detach preview"],
        "Show Explorer" => vec![label, "explorer", "show explorer", "files"],
        "Show Search" => vec![label, "search", "show search", "find in files view"],
        // Probe #10: Close Others / Close Saved / group close discoverability.
        "Close Others" => vec![
            label,
            "close others",
            "close other",
            "close other tabs",
            "close other editors",
            "closeothers",
        ],
        "Close Saved" => vec![
            label,
            "close saved",
            "close saved editors",
            "close saved tabs",
            "closesaved",
        ],
        "Close Editors in Group" => vec![
            label,
            "close editors in group",
            "close group",
            "close editors group",
            "close editor group",
        ],
        "Replace in Files…" => vec![
            label,
            "replace in files",
            "replace in file",
            "search replace files",
            "workspace replace",
            "rif",
        ],
        "Toggle Auto Save" => vec![
            label,
            "auto save",
            "autosave",
            "auto-save",
            "toggle auto save",
        ],
        "Format Document" => {
            vec![label, "format", "format document", "prettier", "beautify"]
        }
        "Trim Trailing Whitespace" => vec![
            label,
            "trim",
            "trim trailing",
            "trim whitespace",
            "trailing",
            "trailing whitespace",
            "trim trailing whitespace",
        ],
        "Markdown guide" => vec![label, "help", "markdown", "guide", "help markdown"],
        "About Malgel" => vec![label, "help", "about", "version", "help about"],
        _ => vec![label],
    }
}

/// The action behind every row, by group and by position within it — the same
/// coordinates the palette reports when a row is confirmed.
pub fn actions(groups: &[CommandGroupSpec]) -> Vec<Vec<Box<dyn Action>>> {
    groups
        .iter()
        .map(|group| {
            group
                .items
                .iter()
                .map(|item| item.action.boxed_clone())
                .collect()
        })
        .collect()
}

/// The commands the palette lists, grouped as VS Code groups its own. The
/// first group is what a VS Code user reaches for most, so it leads.
pub fn groups() -> Vec<CommandGroupSpec> {
    let file = group(
        "File",
        vec![
            cmd("New file", IconName::FilePlus, NewFile),
            cmd("Open file…", IconName::FolderOpen, Open),
            cmd("Open folder…", IconName::FolderOpen, OpenFolder),
            cmd("Close Folder", IconName::FolderOpen, CloseFolder),
            cmd("Save", IconName::Save, Save),
            cmd("Save as…", IconName::Save, SaveAs),
            cmd("Save All", IconName::Save, SaveAll),
            cmd("Open Recent…", IconName::Clock, OpenRecentPicker),
            cmd(
                "Change end of line sequence",
                IconName::ArrowLeftRight,
                ToggleLineEnding,
            ),
            cmd("Export as HTML…", IconName::FileOutput, ExportHtml),
            cmd("Export as PDF…", IconName::FileOutput, ExportPdf),
            cmd("Export as Word document…", IconName::FileOutput, ExportDocx),
            cmd("Close editor", IconName::X, CloseDocument),
            cmd("Close all editors", IconName::X, CloseAllTabs),
            cmd("Close Others", IconName::X, CloseOtherTabs),
            cmd("Close Saved", IconName::X, CloseSavedEditors),
            cmd("Close Editors in Group", IconName::X, CloseEditorsInGroup),
            cmd(
                "Reopen closed editor",
                IconName::RotateCcw,
                ReopenClosedEditor,
            ),
            cmd("Close tabs to the right", IconName::X, CloseTabsToTheRight),
            cmd("Close window", IconName::X, CloseWindow),
            cmd("Quit Malgel", IconName::LogOut, Quit),
        ],
    );

    // The Explorer's own commands, so the palette can drive the file tree the
    // way VS Code's does.
    let explorer = group(
        "Explorer",
        vec![
            cmd("Focus Explorer", IconName::PanelLeft, FocusExplorer),
            cmd("New file…", IconName::FilePlus, NewFileInTree),
            cmd("New folder…", IconName::FolderPlus, NewFolderInTree),
            cmd("Rename…", IconName::PenLine, RenameInTree),
            cmd("Delete", IconName::Trash, DeleteInTree),
            cmd("Open to the Side", IconName::Columns2, OpenToSideInTree),
            cmd("Cut", IconName::Scissors, CutInTree),
            cmd("Copy", IconName::Copy, CopyInTree),
            cmd("Paste", IconName::Clipboard, PasteInTree),
            cmd("Copy Path", IconName::Copy, CopyPathInTree),
            cmd("Copy Relative Path", IconName::Copy, CopyRelativePathInTree),
            cmd(
                "Reveal in File Manager",
                IconName::FolderOpen,
                RevealPathInTree,
            ),
            cmd("Select for Compare", IconName::Copy, SelectForCompare),
            cmd("Compare with Selected", IconName::Copy, CompareWithSelected),
        ],
    );

    let edit = group(
        "Edit",
        vec![
            cmd("Find in files…", IconName::Search, FindInFiles),
            cmd("Replace in Files…", IconName::Replace, ReplaceInFiles),
            cmd(
                "Replace…",
                IconName::Replace,
                gpui_kit::component::input::Replace,
            ),
            cmd("Copy as rich text", IconName::Clipboard, CopyRichText),
            cmd("Tidy table", IconName::List, TidyTable),
        ],
    );

    let go = group(
        "Go",
        vec![
            cmd("Show all commands", IconName::Command, CommandPalette),
            cmd("Go to file…", IconName::FileText, QuickOpen),
            cmd("Go to heading…", IconName::Heading1, GoToHeading),
            cmd("Go to Symbol…", IconName::ListTree, GoToSymbol),
            cmd("Go to line…", IconName::ArrowRight, GoToLine),
            cmd("Go to Definition", IconName::ArrowRight, GoToDefinition),
            cmd("Go back", IconName::ArrowLeft, GoBack),
            cmd("Go forward", IconName::ArrowRight, GoForward),
        ],
    );

    let view = group(
        "View",
        vec![
            cmd("Editor", IconName::PenLine, SetLayout(Layout::Editor)),
            cmd(
                "Editor and preview",
                IconName::Columns2,
                SetLayout(Layout::Split),
            ),
            cmd("Preview", IconName::Eye, SetLayout(Layout::Preview)),
            cmd(
                "Toggle editor and preview",
                IconName::Columns2,
                TogglePreviewLayout,
            ),
            cmd("Split Editor Right", IconName::Columns2, SplitEditorRight),
            cmd("Split Editor Down", IconName::Rows2, SplitEditorDown),
            cmd("Tabs", IconName::ListTree, ToggleTabs),
            cmd("Pin Tab", IconName::Pin, PinTab),
            cmd(
                "Pin Preview",
                IconName::Pin,
                TogglePinPreview,
            ),
            cmd(
                "PDF page thumbnails",
                IconName::PanelLeft,
                TogglePdfThumbnails,
            ),
            cmd("File sidebar", IconName::PanelLeft, ToggleFileSidebar),
            cmd("Show Explorer", IconName::Files, ShowExplorerView),
            cmd("Show Search", IconName::Search, ShowSearchView),
            cmd("Outline", IconName::ListTree, ToggleOutline),
            cmd("Problems", IconName::TriangleAlert, ToggleProblems),
            cmd("Toggle Minimap", IconName::PanelRight, ToggleMinimap),
            cmd("Focus mode", IconName::Maximize2, ToggleFocusMode),
            cmd("Toggle word wrap", IconName::TextWrap, ToggleSoftWrap),
            cmd(
                "Preview soft line breaks",
                IconName::TextWrap,
                ToggleGfmBreaks,
            ),
            cmd("Line numbers", IconName::ListOrdered, ToggleLineNumbers),
            cmd("Fold", IconName::ChevronDown, Fold),
            cmd("Unfold", IconName::ChevronRight, Unfold),
            cmd("Sync scrolling", IconName::ArrowLeftRight, ToggleScrollSync),
            cmd("Check spelling", IconName::SpellCheck, ToggleSpellCheck),
            cmd("Restore session", IconName::Clock, ToggleRestoreSession),
            cmd("Toggle light and dark", IconName::Moon, ToggleAppearance),
            cmd("Toggle Terminal", IconName::Terminal, SummonTerminal),
            cmd("New Terminal", IconName::Terminal, NewTerminal),
            cmd("Kill Terminal", IconName::X, KillTerminal),
            cmd("Clear Terminal", IconName::Trash, ClearTerminal),
            cmd("Split Terminal", IconName::Columns2, SplitTerminal),
            cmd(
                "Toggle Maximized Panel",
                IconName::Maximize2,
                ToggleMaximizedPanel,
            ),
            cmd("Zen Mode", IconName::Maximize2, ToggleZenMode),
            cmd("Zoom in", IconName::ZoomIn, ZoomIn),
            cmd("Zoom out", IconName::ZoomOut, ZoomOut),
            cmd("Actual size", IconName::ScanText, ZoomReset),
        ],
    );

    let format = group(
        "Format",
        vec![
            cmd("Format Document", IconName::Wand, FormatDocument),
            cmd(
                "Trim Trailing Whitespace",
                IconName::Eraser,
                TrimTrailingWhitespace,
            ),
            cmd("Bold", IconName::Bold, Bold),
            cmd("Italic", IconName::Italic, Italic),
            cmd("Strikethrough", IconName::Strikethrough, Strikethrough),
            cmd("Inline code", IconName::Code, InlineCode),
            cmd("Link", IconName::Link, InsertLink),
            cmd("Code block", IconName::Code, CodeBlock),
            cmd("Quote", IconName::Quote, Quote),
            cmd("Bulleted list", IconName::List, BulletList),
            cmd("Numbered list", IconName::ListOrdered, NumberedList),
            cmd("Task list", IconName::ListTodo, TaskList),
            cmd("Move line up", IconName::ArrowUp, MoveLineUp),
            cmd("Move line down", IconName::ArrowDown, MoveLineDown),
            cmd("Toggle line comment", IconName::Code, ToggleLineComment),
            cmd(
                "Select next occurrence",
                IconName::Search,
                SelectNextOccurrence,
            ),
            cmd("Delete line", IconName::Delete, DeleteLine),
            cmd("Heading 1", IconName::Heading1, Heading(1)),
            cmd("Heading 2", IconName::Heading2, Heading(2)),
            cmd("Heading 3", IconName::Heading3, Heading(3)),
            cmd("Heading 4", IconName::Heading4, Heading(4)),
            cmd("Heading 5", IconName::Heading5, Heading(5)),
            cmd("Heading 6", IconName::Heading6, Heading(6)),
        ],
    );

    let settings = group(
        "Settings",
        vec![
            cmd("Settings…", IconName::Settings, OpenSettings),
            cmd("Toggle Auto Save", IconName::Save, ToggleAutoSave),
            cmd("Reload Window", IconName::RefreshCw, ReloadWindow),
            cmd("Compare with Saved", IconName::Copy, CompareWithSaved),
            cmd("Source Control", IconName::GitBranch, ShowSourceControl),
            cmd("Extensions", IconName::Puzzle, ShowExtensions),
            cmd("Color theme…", IconName::Sun, SelectColorTheme),
            cmd(
                "Appearance: follow the system",
                IconName::Sun,
                SetAppearance(Appearance::System),
            ),
            cmd(
                "Appearance: light",
                IconName::Sun,
                SetAppearance(Appearance::Light),
            ),
            cmd(
                "Appearance: dark",
                IconName::Moon,
                SetAppearance(Appearance::Dark),
            ),
        ],
    );

    let help = group(
        "Help",
        vec![
            cmd(
                "Keyboard Shortcuts",
                IconName::Keyboard,
                OpenKeyboardShortcuts,
            ),
            cmd("Markdown guide", IconName::BookOpen, OpenMarkdownGuide),
            cmd("About Malgel", IconName::Info, About),
        ],
    );

    vec![file, explorer, edit, go, view, format, settings, help]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_palette_offers_the_color_theme_picker() {
        let groups = groups();
        let item = groups
            .iter()
            .flat_map(|group| &group.items)
            .find(|item| item.label == "Color theme…")
            .expect("Color theme… is in the palette");
        assert!(item.action.partial_eq(&SelectColorTheme));
        assert!(keywords(item.label).contains(&"theme"));
    }

    #[test]
    fn palette_rows_carry_actions_for_keybinding_hints() {
        let groups = groups();
        // Building palette widgets must attach each Action so CommandItem can
        // resolve the chord drawn beside the label.
        let _rows = palette(&groups);
        let count = groups.iter().map(|g| g.items.len()).sum::<usize>();
        assert!(count > 20, "expected many palette commands, got {count}");
        let toggle = groups
            .iter()
            .flat_map(|g| &g.items)
            .find(|i| i.label == "Toggle editor and preview")
            .expect("preview toggle in palette");
        assert!(toggle.action.partial_eq(&TogglePreviewLayout));
        let sidebar = groups
            .iter()
            .flat_map(|g| &g.items)
            .find(|i| i.label == "File sidebar")
            .expect("File sidebar in palette");
        assert!(sidebar.action.partial_eq(&ToggleFileSidebar));
        let fold = groups
            .iter()
            .flat_map(|g| &g.items)
            .find(|i| i.label == "Fold")
            .expect("Fold in palette");
        assert!(fold.action.partial_eq(&Fold));
        let terminal = groups
            .iter()
            .flat_map(|g| &g.items)
            .find(|i| i.label == "Toggle Terminal")
            .expect("Toggle Terminal in palette");
        assert!(terminal.action.partial_eq(&SummonTerminal));
    }

    #[test]
    fn palette_probe6_aliases_and_staples() {
        let groups = groups();
        let labels: Vec<_> = groups
            .iter()
            .flat_map(|g| g.items.iter().map(|i| i.label))
            .collect();
        assert!(labels.contains(&"Save All"));
        assert!(labels.contains(&"Open Recent…"));
        assert!(labels.contains(&"Replace…"));
        assert!(labels.contains(&"Toggle Maximized Panel"));
        assert!(keywords("Replace…").contains(&"replace"));
        assert!(keywords("Go to line…").contains(&"goto"));
        assert!(keywords("Go to line…").contains(&"goto line"));
        assert!(keywords("Save All").contains(&"save all"));
        assert!(keywords("Toggle Maximized Panel").contains(&"maximize"));
        assert!(keywords("Open Recent…").contains(&"open recent"));
    }

    #[test]
    fn palette_probe7_preview_problems_minimap() {
        let groups = groups();
        let labels: Vec<_> = groups
            .iter()
            .flat_map(|g| g.items.iter().map(|i| i.label))
            .collect();
        assert!(labels.contains(&"Problems"));
        assert!(labels.contains(&"Toggle Minimap"));
        assert!(keywords("Preview").contains(&"markdown preview"));
        assert!(keywords("Preview").contains(&"open preview"));
        assert!(keywords("Toggle editor and preview").contains(&"toggle preview"));
        assert!(keywords("Problems").contains(&"problems"));
        assert!(keywords("Toggle Minimap").contains(&"minimap"));
    }

    #[test]
    fn palette_probe8_terminal_symbol_aliases() {
        let groups = groups();
        let labels: Vec<_> = groups
            .iter()
            .flat_map(|g| g.items.iter().map(|i| i.label))
            .collect();
        for need in [
            "Kill Terminal",
            "New Terminal",
            "Clear Terminal",
            "Split Terminal",
            "Go to Symbol…",
            "Reload Window",
            "Zen Mode",
            "Compare with Saved",
            "Source Control",
            "Extensions",
        ] {
            assert!(labels.contains(&need), "missing {need}");
        }
        assert!(keywords("Settings…").contains(&"open settings"));
        assert!(keywords("Kill Terminal").contains(&"kill terminal"));
        assert!(keywords("Clear Terminal").contains(&"clear"));
        assert!(keywords("Go to Symbol…").contains(&"go to symbol"));
        assert!(keywords("Zen Mode").contains(&"zen"));
        assert!(keywords("Compare with Saved").contains(&"diff"));
        assert!(keywords("Source Control").contains(&"git"));
        assert!(keywords("Extensions").contains(&"extensions"));
    }

    #[test]
    fn palette_probe10_close_replace_autosave_format_help() {
        let groups = groups();
        let labels: Vec<_> = groups
            .iter()
            .flat_map(|g| g.items.iter().map(|i| i.label))
            .collect();
        for need in [
            "Close Others",
            "Close Saved",
            "Close Editors in Group",
            "Replace in Files…",
            "Toggle Auto Save",
            "Format Document",
            "Trim Trailing Whitespace",
        ] {
            assert!(labels.contains(&need), "missing {need}");
        }
        assert!(keywords("Close Others").contains(&"close others"));
        assert!(keywords("Close Saved").contains(&"close saved"));
        assert!(keywords("Close Editors in Group").contains(&"close group"));
        assert!(keywords("Replace in Files…").contains(&"replace in files"));
        assert!(keywords("Toggle Auto Save").contains(&"autosave"));
        assert!(keywords("Format Document").contains(&"format"));
        assert!(keywords("Trim Trailing Whitespace").contains(&"trailing"));
        assert!(keywords("Keyboard Shortcuts").contains(&"help"));
        assert!(keywords("About Malgel").contains(&"help"));
        assert!(keywords("Markdown guide").contains(&"help"));
    }

    #[test]
    fn palette_probe11_pin_search_scm_extensions() {
        let groups = groups();
        let labels: Vec<_> = groups
            .iter()
            .flat_map(|g| g.items.iter().map(|i| i.label))
            .collect();
        for need in [
            "Pin Tab",
            "Show Explorer",
            "Show Search",
            "Source Control",
            "Extensions",
            "Outline",
            "Compare with Saved",
        ] {
            assert!(labels.contains(&need), "missing {need}");
        }
        assert!(keywords("Pin Tab").contains(&"pin tab"));
        assert!(keywords("Show Search").contains(&"show search"));
        assert!(keywords("Source Control").contains(&"scm"));
        assert!(keywords("Extensions").contains(&"extensions"));
    }

    #[test]
    fn palette_probe12_outline_ranks_over_bare_outline_symbol() {
        let o = keywords("Outline");
        let s = keywords("Go to Symbol…");
        assert!(o.contains(&"outline"));
        assert!(o.contains(&"outline view"));
        assert!(
            !s.iter().any(|k| k.contains("outline")),
            "no Go to Symbol keyword may contain outline (query outline → Outline)"
        );
        assert!(s.contains(&"go to symbol"));
    }

    #[test]
    fn palette_probe13_outline_query_hits_outline_label() {
        let o = keywords("Outline");
        assert!(
            o.iter()
                .any(|k| k == &"outline" || k.to_lowercase().contains("outline"))
        );
        let label = "Outline";
        assert!(label.to_lowercase().contains("outline"));
    }

    #[test]
    fn palette_probe14_compare_select() {
        let groups = groups();
        let labels: Vec<_> = groups
            .iter()
            .flat_map(|g| g.items.iter().map(|i| i.label))
            .collect();
        assert!(labels.contains(&"Select for Compare"));
        assert!(labels.contains(&"Compare with Selected"));
        assert!(keywords("Select for Compare").contains(&"select for compare"));
        assert!(keywords("Compare with Selected").contains(&"compare with selected"));
    }
}
