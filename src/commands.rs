//! The command palette: one searchable list of every command, the way VS
//! Code's Show All Commands (Ctrl+Shift+P) offers them.
//!
//! Every command is already an Action (see [`crate::actions`]), so the palette
//! adds no new dispatch path — it only groups the commands the way VS Code
//! does and hands each one to `CommandItem`, which draws the command's current
//! key binding beside it.
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

/// The rows the palette draws: labels and icons only.
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
                            .icon(Icon::new(item.icon)),
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
            cmd("Save", IconName::Save, Save),
            cmd("Save as…", IconName::Save, SaveAs),
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
            cmd(
                "Reopen closed editor",
                IconName::RotateCcw,
                ReopenClosedEditor,
            ),
            cmd("Close other tabs", IconName::X, CloseOtherTabs),
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
        ],
    );

    let edit = group(
        "Edit",
        vec![
            cmd("Find in files…", IconName::Search, FindInFiles),
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
            cmd("Tabs", IconName::ListTree, ToggleTabs),
            cmd("Pin or unpin tab", IconName::Pin, PinTab),
            cmd(
                "PDF page thumbnails",
                IconName::PanelLeft,
                TogglePdfThumbnails,
            ),
            cmd("File sidebar", IconName::PanelLeft, ToggleFileSidebar),
            cmd("Outline", IconName::ListTree, ToggleOutline),
            cmd("Focus mode", IconName::Maximize2, ToggleFocusMode),
            cmd("Toggle word wrap", IconName::TextWrap, ToggleSoftWrap),
            cmd("Line numbers", IconName::ListOrdered, ToggleLineNumbers),
            cmd("Fold", IconName::ChevronDown, Fold),
            cmd("Unfold", IconName::ChevronRight, Unfold),
            cmd("Sync scrolling", IconName::ArrowLeftRight, ToggleScrollSync),
            cmd("Check spelling", IconName::SpellCheck, ToggleSpellCheck),
            cmd("Restore session", IconName::Clock, ToggleRestoreSession),
            cmd("Toggle light and dark", IconName::Moon, ToggleAppearance),
            cmd("Open terminal", IconName::Terminal, SummonTerminal),
            cmd("Zoom in", IconName::ZoomIn, ZoomIn),
            cmd("Zoom out", IconName::ZoomOut, ZoomOut),
            cmd("Actual size", IconName::ScanText, ZoomReset),
        ],
    );

    let format = group(
        "Format",
        vec![
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
}
