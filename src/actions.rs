//! Commands and their key bindings.
//!
//! Every command is an Action so the title bar, the menu bar, the editor's
//! context menu and the keyboard all dispatch the same thing.

use std::path::PathBuf;

use gpui_kit::{Action, App, KeyBinding, SharedString, actions};
use serde::Deserialize;

use crate::settings::{Appearance, Layout};

actions!(
    malgel,
    [
        NewFile,
        Open,
        OpenFolder,
        CloseFolder,
        OpenRecentPicker,
        SummonTerminal,
        KillTerminal,
        NewTerminal,
        ClearTerminal,
        TerminalCopy,
        DebugCrash,
        TerminalPaste,
        TerminalSelectAll,
        TerminalClear,
        SplitTerminal,
        GoToSymbol,
        ReloadWindow,
        ToggleZenMode,
        CompareWithSaved,
        ShowSourceControl,
        ShowExtensions,
        Save,
        SaveAs,
        SaveAll,
        ExportHtml,
        ExportPdf,
        ExportDocx,
        RevealInFolder,
        CloseDocument,
        CloseAllTabs,
        CloseOtherTabs,
        CloseSavedEditors,
        CloseEditorsInGroup,
        CloseTabsToTheRight,
        CloseWindow,
        ReopenClosedEditor,
        ToggleLineEnding,
        PinTab,
        ShowTabContextMenu,
        NextTab,
        PreviousTab,
        GoBack,
        GoForward,
        Quit,
        About,
        OpenMarkdownGuide,
        ToggleScrollSync,
        ToggleSoftWrap,
        ToggleGfmBreaks,
        ToggleLineNumbers,
        ToggleAppearance,
        SelectColorTheme,
        ToggleTabs,
        ToggleFileSidebar,
        ToggleOutline,
        ToggleProblems,
        ToggleMinimap,
        ToggleMaximizedPanel,
        TogglePdfThumbnails,
        ToggleSpellCheck,
        ToggleFocusMode,
        TogglePreviewLayout,
        TogglePinPreview,
        ForceRecompile,
        SplitEditorRight,
        SplitEditorDown,
        CloseEditorSplit,
        OpenKeyboardShortcuts,
        ToggleRestoreSession,
        OpenSettings,
        CopyRichText,
        TidyTable,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        GoToHeading,
        GoToLine,
        GoToDefinition,
        CommandPalette,
        QuickOpen,
        FindInFiles,
        ReplaceInFiles,
        ToggleAutoSave,
        FormatDocument,
        TrimTrailingWhitespace,
        ConvertBracketMath,
        ShowExplorerView,
        ShowSearchView,
        NewFileInTree,
        NewFolderInTree,
        RenameInTree,
        DeleteInTree,
        CutInTree,
        CopyInTree,
        PasteInTree,
        CopyPathInTree,
        CopyRelativePathInTree,
        RevealPathInTree,
        OpenInTree,
        OpenToSideInTree,
        SelectForCompare,
        CompareWithSelected,
        FocusExplorer,
        TreeSelectUp,
        TreeSelectDown,
        TreeSelectLeft,
        TreeSelectRight,
        TreeSelectFirst,
        TreeSelectLast,
        Bold,
        Italic,
        Strikethrough,
        InlineCode,
        InsertLink,
        CodeBlock,
        Quote,
        BulletList,
        NumberedList,
        TaskList,
        MoveLineUp,
        MoveLineDown,
        ToggleLineComment,
        SelectNextOccurrence,
        DeleteLine,
        Fold,
        Unfold,
    ]
);

/// Show the editor, the preview, or both.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = malgel, no_json)]
pub struct SetLayout(pub Layout);

/// Open a top-level title-bar menu by name (Alt+F → `"File"`, …).
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = malgel, no_json)]
pub struct OpenAppMenu(pub String);

/// Turn the current lines into a heading of the given level.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = malgel, no_json)]
pub struct Heading(pub u8);

/// Follow the system appearance, or force light or dark.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = malgel, no_json)]
pub struct SetAppearance(pub Appearance);

/// Use the named theme for its appearance (light or dark).
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = malgel, no_json)]
pub struct SetTheme(pub SharedString);

/// Open an entry of the recent files list.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = malgel, no_json)]
pub struct OpenRecent(pub usize);

/// Show the open document at this index.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = malgel, no_json)]
pub struct ActivateTab(pub usize);

/// Check spelling against the named dictionary, like `en_US`.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = malgel, no_json)]
pub struct SetSpellLanguage(pub SharedString);

/// Replace the misspelled `word` at `start..end` with `with`.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = malgel, no_json)]
pub struct ReplaceMisspelling {
    pub start: usize,
    pub end: usize,
    pub word: SharedString,
    pub with: SharedString,
}

/// Follow a link clicked in the preview. `alternate` opens a linked
/// document the other way from the setting (a new tab or the same one).
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = malgel, no_json)]
pub struct FollowLink {
    pub url: SharedString,
    pub alternate: bool,
}

/// Show the Mermaid diagram with this source in its own window.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = malgel, no_json)]
pub struct ViewDiagram(pub SharedString);

/// Open the file picked in the Quick Open palette. The palette's callback has
/// no access to the workspace, so it hands the path back as an action.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = malgel, no_json)]
pub struct OpenQuickPath(pub PathBuf, pub Option<usize>);

/// Move the caret to the given 1-based line, the way VS Code's Go to Line
/// (Ctrl+G) does. The palette has no access to the workspace, so it hands the
/// line back as an action.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = malgel, no_json)]
pub struct GoToLineAt(pub usize);

/// Accept a word as correctly spelled from now on.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = malgel, no_json)]
pub struct AddToDictionary(pub SharedString);

/// The key context of the workspace view.
pub const WORKSPACE_CONTEXT: &str = "Workspace";
/// The key context of the document editor, nested inside the workspace.
/// Commands that only make sense while typing (Bold, for one) bind here, so a
/// shortcut can mean one thing in the editor and another in the workspace —
/// the way VS Code scopes editor actions to the editor.
pub const EDITOR_CONTEXT: &str = "Editor";
/// The key context of the file sidebar's tree. File operations bind here so
/// that Del, F2, Ctrl+X/C/V and the arrow keys mean what they mean in VS
/// Code's Explorer only while the tree has the keyboard — inside the editor
/// they keep deleting text, moving the caret and cutting the selection.
pub const FILE_TREE_CONTEXT: &str = "FileTree";

pub fn bind_keys(cx: &mut App) {
    let workspace = Some(WORKSPACE_CONTEXT);
    // `secondary` is Command on macOS and Control elsewhere.
    cx.bind_keys([
        KeyBinding::new("secondary-n", NewFile, None),
        KeyBinding::new("secondary-o", Open, None),
        // VS Code's chord. Not on macOS: a menu item there carries exactly one
        // key equivalent, `secondary-k` is already the Format ▸ Link item's,
        // and Open folder is itself a menu item — so the first key of the
        // chord goes to Link and the chord can never complete.
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("secondary-k secondary-o", OpenFolder, workspace),
        // One key instead, on a combination nothing else uses, so it shows up
        // in the File menu the way a Mac shortcut should.
        #[cfg(target_os = "macos")]
        KeyBinding::new("secondary-ctrl-o", OpenFolder, workspace),
        // VS Code: Ctrl/Cmd+R opens the Recent quick-pick (not jump-to-#0).
        KeyBinding::new("secondary-r", OpenRecentPicker, workspace),
        KeyBinding::new("secondary-s", Save, None),
        KeyBinding::new("secondary-shift-s", SaveAs, None),
        // Export lives in the File menu only: VS Code gives Ctrl+Shift+E to
        // the Explorer, and Malgel's sidebar is close enough to want the key
        // free for it.
        KeyBinding::new("secondary-w", CloseDocument, None),
        KeyBinding::new("secondary-alt-w", CloseOtherTabs, workspace),
        // VS Code's Close All Editors and Reopen Closed Editor.
        // Chord works at the keyboard; macOS menus still show only the first
        // key (same limitation as Open folder — see cfg below).
        KeyBinding::new("secondary-k secondary-w", CloseAllTabs, workspace),
        // VS Code's chord, off macOS (see Open folder above); the editor
        // binds it again beside Link. The palette's "Color theme…" reaches
        // it from anywhere.
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("secondary-k secondary-t", SelectColorTheme, workspace),
        KeyBinding::new("secondary-shift-t", ReopenClosedEditor, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("alt-left", GoBack, workspace),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("alt-right", GoForward, workspace),
        // VS Code on macOS: Ctrl+- / Ctrl+Shift+- (Cmd+- is Zoom Out).
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl--", GoBack, workspace),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-shift--", GoForward, workspace),
        KeyBinding::new("secondary-shift-w", CloseWindow, None),
        KeyBinding::new("ctrl-tab", NextTab, workspace),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, workspace),
        // Probe #17 a11y: Shift+F10 / Menu key open the tab context menu.
        KeyBinding::new("shift-f10", ShowTabContextMenu, workspace),
        KeyBinding::new("menu", ShowTabContextMenu, workspace),
        // macOS: Cmd+Alt+←/→ for next/prev tab (VS Code). Do NOT use
        // Cmd+Shift+[ / ] — those are Fold / Unfold and would conflict.
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-right", NextTab, workspace),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-left", PreviousTab, workspace),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-pagedown", NextTab, workspace),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-pageup", PreviousTab, workspace),
        // VS Code's Toggle Side Bar: Ctrl+B always, including while the
        // editor has focus. Markdown Bold moves to Ctrl+Shift+B (below).
        KeyBinding::new("secondary-b", ToggleFileSidebar, None),
        // VS Code's Explorer: show the sidebar if it is hidden, put the
        // keyboard in the tree, and pick the file that is open.
        KeyBinding::new("secondary-shift-e", FocusExplorer, None),
        KeyBinding::new("secondary-alt-o", ToggleOutline, None),
        // Linux/Windows menubar mnemonics (probe #7): Alt+letter opens the menu.
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("alt-f", OpenAppMenu("File".into()), None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("alt-e", OpenAppMenu("Edit".into()), None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("alt-v", OpenAppMenu("View".into()), None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("alt-g", OpenAppMenu("Go".into()), None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("alt-o", OpenAppMenu("Format".into()), None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("alt-h", OpenAppMenu("Help".into()), None),
        // VS Code gives Ctrl+Shift+F to Find in Files; focus mode moves aside
        // so the key is free for it. On macOS Cmd+Alt+F is Replace (VS Code),
        // so Focus Mode uses Cmd+Alt+Shift+F there instead.
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("secondary-alt-f", ToggleFocusMode, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-shift-f", ToggleFocusMode, None),
        KeyBinding::new("secondary-shift-f", FindInFiles, None),
        // VS Code Replace in Files.
        KeyBinding::new("secondary-shift-h", ReplaceInFiles, None),
        KeyBinding::new("secondary-alt-c", CopyRichText, None),
        KeyBinding::new("secondary-,", OpenSettings, None),
        // Terminal chords match VS Code and must stay platform-correct:
        // Ctrl+` toggles on Windows/Linux *and* macOS (not Cmd+`, which is
        // window-cycle on macOS). Ctrl+Shift+` always creates a new terminal.
        // macOS also gets Cmd+J (Toggle Panel), which commonly reveals the
        // terminal — a separate chord, never shared with Ctrl+`.
        // No key-context: palette tip resolution (same as Toggle Side Bar).
        KeyBinding::new("ctrl-`", SummonTerminal, None),
        KeyBinding::new("ctrl-shift-`", NewTerminal, None),
        // X11 reports Ctrl+Shift+` as Ctrl+~ (shift is dropped for symbols).
        KeyBinding::new("ctrl-~", NewTerminal, None),
        // VS Code: Split Terminal (Ctrl+Shift+5; X11 may report it as Ctrl+%).
        KeyBinding::new("ctrl-shift-5", SplitTerminal, None),
        KeyBinding::new("ctrl-%", SplitTerminal, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-j", SummonTerminal, None),
        KeyBinding::new("secondary-q", Quit, None),
        // No key-context: layout must switch even when the Typst/PDF viewer or
        // Markdown preview holds focus (those entities do not inherit Workspace).
        KeyBinding::new("secondary-1", SetLayout(Layout::Editor), None),
        KeyBinding::new("secondary-2", SetLayout(Layout::Split), None),
        KeyBinding::new("secondary-3", SetLayout(Layout::Preview), None),
        // Toggle editor|preview <-> editor-only (default open is already split,
        // so a one-shot "open preview" feels like a no-op without this).
        KeyBinding::new("secondary-shift-v", TogglePreviewLayout, None),
        // Typst Force Recompile. VS Code leaves Ctrl+Alt+Shift+B (Cmd+Alt+Shift+B)
        // unbound; Ctrl+Shift+B is its Run Build Task and Ctrl+Alt+R / Shift+Alt+R
        // reveal the file in the OS, so the "build" letter with every modifier
        // stays clear of both. No key-context: it must work with the preview
        // focused too.
        KeyBinding::new("secondary-alt-shift-b", ForceRecompile, None),
        // VS Code Split Editor Right (Ctrl+\).
        KeyBinding::new("secondary-\\", SplitEditorRight, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("secondary-k secondary-s", OpenKeyboardShortcuts, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-k cmd-s", OpenKeyboardShortcuts, None),
        // VS Code Save All: Ctrl+K S (S without Ctrl). Distinct from Ctrl+K Ctrl+S.
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("secondary-k s", SaveAll, workspace),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("secondary-k s", SaveAll, Some(EDITOR_CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-k s", SaveAll, workspace),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-k s", SaveAll, Some(EDITOR_CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-k cmd-w", CloseAllTabs, Some(EDITOR_CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-k z", ToggleZenMode, Some(EDITOR_CONTEXT)),
        // The last binding for an action is the one menus display.
        KeyBinding::new("secondary-+", ZoomIn, None),
        KeyBinding::new("secondary-=", ZoomIn, None),
        KeyBinding::new("secondary--", ZoomOut, None),
        KeyBinding::new("secondary-0", ZoomReset, None),
        // Appearance lives in the View menu only: VS Code gives Ctrl+Shift+L
        // to "select all occurrences".
        // VS Code: Ctrl+Shift+O = Go to Symbol in Editor (headings via palette / Outline).
        KeyBinding::new("secondary-shift-o", GoToSymbol, None),
        KeyBinding::new("secondary-shift-.", GoToHeading, None),
        // VS Code: Go to Line is Ctrl+G on every platform. On macOS Cmd+G is
        // Find Next (editor library); binding secondary-g there steals it.
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("secondary-g", GoToLine, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-g", GoToLine, None),
        // Typst (and later other languages): jump to the definition under the caret.
        KeyBinding::new("f12", GoToDefinition, Some(EDITOR_CONTEXT)),
        KeyBinding::new("secondary-shift-p", CommandPalette, None),
        KeyBinding::new("secondary-p", QuickOpen, None),
        // VS Code's Toggle Word Wrap.
        KeyBinding::new("alt-z", ToggleSoftWrap, None),
        // VS Code Zen Mode: Ctrl+K Z
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("secondary-k z", ToggleZenMode, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-k z", ToggleZenMode, None),
        // The Explorer's keys, scoped to the file tree: Del, F2 and
        // Ctrl+X/C/V are what VS Code binds them to, and they only reach the
        // tree while it has the keyboard — in the editor the same keys keep
        // editing text.
        // FILE_TREE_CONTEXT keeps Del/arrows scoped; F2 also binds globally so
        // an explorer selection still renames after a click that focused the
        // editor (probe #6 AC). Handler no-ops without a tree selection.
        KeyBinding::new("f2", RenameInTree, Some(FILE_TREE_CONTEXT)),
        KeyBinding::new("f2", RenameInTree, None),
        KeyBinding::new("delete", DeleteInTree, Some(FILE_TREE_CONTEXT)),
        KeyBinding::new("backspace", DeleteInTree, Some(FILE_TREE_CONTEXT)),
        KeyBinding::new("secondary-x", CutInTree, Some(FILE_TREE_CONTEXT)),
        KeyBinding::new("secondary-c", CopyInTree, Some(FILE_TREE_CONTEXT)),
        KeyBinding::new("secondary-v", PasteInTree, Some(FILE_TREE_CONTEXT)),
        KeyBinding::new("enter", OpenInTree, Some(FILE_TREE_CONTEXT)),
        KeyBinding::new("up", TreeSelectUp, Some(FILE_TREE_CONTEXT)),
        KeyBinding::new("down", TreeSelectDown, Some(FILE_TREE_CONTEXT)),
        KeyBinding::new("left", TreeSelectLeft, Some(FILE_TREE_CONTEXT)),
        KeyBinding::new("right", TreeSelectRight, Some(FILE_TREE_CONTEXT)),
        KeyBinding::new("home", TreeSelectFirst, Some(FILE_TREE_CONTEXT)),
        KeyBinding::new("end", TreeSelectLast, Some(FILE_TREE_CONTEXT)),
        // Typing commands, scoped to the editor so they mean nothing while the
        // sidebar has the keyboard. Bold is Ctrl+Shift+B so Ctrl+B can match
        // VS Code's Toggle Side Bar with the caret in the editor.
        KeyBinding::new("secondary-shift-b", Bold, None),
        KeyBinding::new("secondary-i", Italic, None),
        KeyBinding::new("secondary-shift-x", Strikethrough, None),
        KeyBinding::new("secondary-e", InlineCode, None),
        KeyBinding::new("secondary-k", InsertLink, Some(EDITOR_CONTEXT)),
        // The workspace's Ctrl+K chords, again in the editor: its single-key
        // Link sits deeper and would otherwise take the first Ctrl+K before
        // the chord could complete. With both here, Ctrl+K waits for the
        // second key, and anything else (or a pause) still inserts a link.
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new(
            "secondary-k secondary-t",
            SelectColorTheme,
            Some(EDITOR_CONTEXT),
        ),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("secondary-k secondary-o", OpenFolder, Some(EDITOR_CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new(
            "secondary-k secondary-w",
            CloseAllTabs,
            Some(EDITOR_CONTEXT),
        ),
        // Redo: the editor library binds Ctrl+Y off macOS (and Cmd+Shift+Z on
        // it); Ctrl+Shift+Z is the other key every Linux/Windows editor takes.
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new(
            "ctrl-shift-z",
            gpui_kit::component::input::Redo,
            Some(INPUT_CONTEXT),
        ),
        // VS Code Replace: Ctrl+H (Win/Linux), Cmd+Option+F (macOS).
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new(
            "ctrl-h",
            gpui_kit::component::input::Replace,
            Some(INPUT_CONTEXT),
        ),
        #[cfg(target_os = "macos")]
        KeyBinding::new(
            "cmd-alt-f",
            gpui_kit::component::input::Replace,
            Some(EDITOR_CONTEXT),
        ),
        KeyBinding::new("secondary-shift-c", CodeBlock, None),
        // Shifted punctuation/numbers: on Linux/Windows GPUI reports the
        // produced character and strips Shift (Shift+[ → `{`, Shift+8 → `*`).
        // Bind that character without a shift- modifier, or the shortcut never
        // matches. Letters still use secondary-shift-x (Shift stays for a–z).
        KeyBinding::new("secondary-shift-.", Quote, None),
        KeyBinding::new("secondary-*", BulletList, None),
        KeyBinding::new("secondary-&", NumberedList, None),
        KeyBinding::new("secondary-(", TaskList, None),
        KeyBinding::new("secondary-alt-1", Heading(1), None),
        KeyBinding::new("secondary-alt-2", Heading(2), None),
        KeyBinding::new("secondary-alt-3", Heading(3), None),
        KeyBinding::new("secondary-alt-4", Heading(4), None),
        KeyBinding::new("secondary-alt-5", Heading(5), None),
        KeyBinding::new("secondary-alt-6", Heading(6), None),
        // VS Code's Move Line Up / Down. Plain Alt with an arrow is free:
        // the editor library binds Ctrl+Alt+↑/↓ (and Shift+Alt+↑/↓) to adding
        // a cursor above and below, and Alt+←/→ to moving by word, so this
        // pair does not fight either.
        // VS Code's Toggle Line Comment.
        KeyBinding::new("secondary-/", ToggleLineComment, None),
        // VS Code's Add Selection To Next Find Match (one selection here: it
        // moves to the next match) and Delete Line.
        KeyBinding::new("secondary-d", SelectNextOccurrence, None),
        KeyBinding::new("secondary-shift-k", DeleteLine, None),
        KeyBinding::new("alt-up", MoveLineUp, None),
        KeyBinding::new("alt-down", MoveLineDown, None),
        // VS Code Fold / Unfold (Ctrl/Cmd+Shift+[ / ]). GPUI key is `{` / `}`
        // with Shift stripped — see comment on BulletList above. Also bind the
        // naive secondary-shift-[ / ] spellings so Xvfb xdotool chord delivery
        // matches when it keeps Shift+[ instead of rewriting to {.
        // No EDITOR_CONTEXT: the palette dialog is not under Editor, so a
        // context-scoped binding never paints the chord tip (same as Terminal).
        KeyBinding::new("secondary-{", Fold, None),
        KeyBinding::new("secondary-}", Unfold, None),
        KeyBinding::new("secondary-shift-[", Fold, None),
        KeyBinding::new("secondary-shift-]", Unfold, None),
    ]);
}

/// gpui-base's key context for its text inputs and editors.
#[cfg(not(target_os = "macos"))]
const INPUT_CONTEXT: &str = "Input";

#[cfg(test)]
mod tests {
    use super::*;

    /// `KeyBinding::new` panics on anything it cannot parse, so this also
    /// guards the spelling of every shortcut in `bind_keys`.
    #[test]
    fn open_folder_is_a_chord_off_macos_and_one_key_on_it() {
        let chord = KeyBinding::new("secondary-k secondary-o", OpenFolder, None);
        assert_eq!(
            chord.keystrokes().len(),
            2,
            "should be a two-keystroke chord"
        );
        let mac = KeyBinding::new("secondary-ctrl-o", OpenFolder, None);
        assert_eq!(mac.keystrokes().len(), 1, "macOS gets a single key");
        // The plain shortcuts both forms have to coexist with stay single.
        let link = KeyBinding::new("secondary-k", InsertLink, None);
        assert_eq!(link.keystrokes().len(), 1);
        let open = KeyBinding::new("secondary-o", Open, None);
        assert_eq!(open.keystrokes().len(), 1);
    }

    /// Ctrl+B is Toggle Side Bar everywhere (VS Code). Bold is Ctrl+Shift+B
    /// so editor focus never steals the side-bar chord.
    #[test]
    fn control_b_toggles_the_sidebar_and_bold_uses_shift() {
        let sidebar = KeyBinding::new("secondary-b", ToggleFileSidebar, None);
        assert_eq!(sidebar.keystrokes().len(), 1);
        let bold = KeyBinding::new("secondary-shift-b", Bold, None);
        assert_eq!(bold.keystrokes().len(), 1);
        assert_ne!(
            (
                sidebar.keystrokes()[0].key(),
                sidebar.keystrokes()[0].modifiers().shift
            ),
            (
                bold.keystrokes()[0].key(),
                bold.keystrokes()[0].modifiers().shift
            ),
            "Bold must not share Ctrl+B with Toggle Side Bar"
        );
    }

    /// Ctrl+Shift+[ / ] arrive as ctrl-{ / ctrl-} (Shift stripped for symbols).
    /// The Fold bindings must use those keys or Xvfb / real Linux keyboards miss.
    #[test]
    fn fold_bindings_use_shifted_bracket_characters() {
        let fold = KeyBinding::new("secondary-{", Fold, None);
        let unfold = KeyBinding::new("secondary-}", Unfold, None);
        assert_eq!(fold.keystrokes().len(), 1);
        assert_eq!(unfold.keystrokes().len(), 1);
        let fold_ks = &fold.keystrokes()[0];
        let unfold_ks = &unfold.keystrokes()[0];
        assert_eq!(fold_ks.key(), "{");
        assert_eq!(unfold_ks.key(), "}");
        assert!(!fold_ks.modifiers().shift, "Shift is already encoded in {{");
        assert!(
            !unfold_ks.modifiers().shift,
            "Shift is already encoded in }}"
        );
        assert!(fold_ks.modifiers().control || fold_ks.modifiers().platform);

        // The naive VS Code spellings parse, but they are not what Linux delivers
        // (platform key is `{` without Shift, not `[` with Shift).
        let naive = KeyBinding::new("secondary-shift-[", Fold, None);
        let naive_ks = &naive.keystrokes()[0];
        assert_eq!(naive_ks.key(), "[");
        assert!(naive_ks.modifiers().shift);
        assert_ne!(
            (fold_ks.key(), fold_ks.modifiers().shift),
            (naive_ks.key(), naive_ks.modifiers().shift),
            "secondary-shift-[ would never match a real Ctrl+Shift+[ on Linux"
        );

        // Same convention for shifted number-row markdown shortcuts.
        for (binding, key) in [
            ("secondary-*", "*"),
            ("secondary-&", "&"),
            ("secondary-(", "("),
        ] {
            let b = KeyBinding::new(binding, BulletList, Some(EDITOR_CONTEXT));
            assert_eq!(b.keystrokes()[0].key(), key);
            assert!(!b.keystrokes()[0].modifiers().shift);
        }
    }

    /// macOS must not bind Next/Prev tab to Cmd+Shift+[ / ] — Fold owns those.
    #[test]
    fn macos_tab_chords_do_not_collide_with_fold() {
        let fold = KeyBinding::new("secondary-shift-[", Fold, None);
        let unfold = KeyBinding::new("secondary-shift-]", Unfold, None);
        let prev = KeyBinding::new("cmd-alt-left", PreviousTab, None);
        let next = KeyBinding::new("cmd-alt-right", NextTab, None);
        assert_eq!(fold.keystrokes()[0].key(), "[");
        assert!(fold.keystrokes()[0].modifiers().shift);
        assert_eq!(prev.keystrokes()[0].key(), "left");
        assert!(prev.keystrokes()[0].modifiers().alt);
        assert_eq!(next.keystrokes()[0].key(), "right");
        assert_ne!(
            (
                fold.keystrokes()[0].key(),
                fold.keystrokes()[0].modifiers().shift
            ),
            (
                prev.keystrokes()[0].key(),
                prev.keystrokes()[0].modifiers().shift
            ),
        );
        assert_ne!(
            (
                unfold.keystrokes()[0].key(),
                unfold.keystrokes()[0].modifiers().shift
            ),
            (
                next.keystrokes()[0].key(),
                next.keystrokes()[0].modifiers().shift
            ),
        );
    }

    /// Force Recompile is Ctrl+Alt+Shift+B (Cmd+Alt+Shift+B): one key, every
    /// modifier, so it never shadows VS Code's Ctrl+Shift+B (Run Build Task)
    /// or Ctrl+B (Toggle Side Bar).
    #[test]
    fn force_recompile_has_its_own_chord() {
        let force = KeyBinding::new("secondary-alt-shift-b", ForceRecompile, None);
        assert_eq!(force.keystrokes().len(), 1);
        let ks = &force.keystrokes()[0];
        assert_eq!(ks.key(), "b");
        let m = ks.modifiers();
        assert!(m.alt && m.shift && (m.control || m.platform));
        for other in ["secondary-b", "secondary-shift-b", "secondary-alt-b"] {
            let taken = KeyBinding::new(other, Bold, None);
            assert_ne!(
                taken.keystrokes()[0].modifiers(),
                m,
                "{other} must not collide with Force Recompile"
            );
        }
    }

    /// Go to Line stays Ctrl+G on macOS so Cmd+G remains Find Next.
    #[test]
    fn go_to_line_mac_spelling_is_ctrl_g() {
        let mac = KeyBinding::new("ctrl-g", GoToLine, None);
        assert_eq!(mac.keystrokes().len(), 1);
        assert!(mac.keystrokes()[0].modifiers().control);
        assert!(!mac.keystrokes()[0].modifiers().platform);
        let back = KeyBinding::new("ctrl--", GoBack, None);
        let forward = KeyBinding::new("ctrl-shift--", GoForward, None);
        assert_eq!(back.keystrokes().len(), 1);
        assert_eq!(forward.keystrokes().len(), 1);
    }
}
