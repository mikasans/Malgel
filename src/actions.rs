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
        KeyBinding::new("secondary-r", OpenRecent(0), workspace),
        KeyBinding::new("secondary-s", Save, None),
        KeyBinding::new("secondary-shift-s", SaveAs, None),
        // Export lives in the File menu only: VS Code gives Ctrl+Shift+E to
        // the Explorer, and Malgel's sidebar is close enough to want the key
        // free for it.
        KeyBinding::new("secondary-w", CloseDocument, None),
        KeyBinding::new("secondary-alt-w", CloseOtherTabs, workspace),
        // VS Code's Close All Editors and Reopen Closed Editor.
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
        KeyBinding::new("secondary-shift-w", CloseWindow, None),
        KeyBinding::new("ctrl-tab", NextTab, workspace),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, workspace),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-]", NextTab, workspace),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-[", PreviousTab, workspace),
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
        // so the key is free for it.
        KeyBinding::new("secondary-alt-f", ToggleFocusMode, None),
        KeyBinding::new("secondary-shift-f", FindInFiles, None),
        // VS Code Replace in Files.
        KeyBinding::new("secondary-shift-h", ReplaceInFiles, None),
        KeyBinding::new("secondary-alt-c", CopyRichText, None),
        KeyBinding::new("secondary-,", OpenSettings, None),
        // No key-context: the command palette resolves chords only for
        // global (or currently focused) bindings; Workspace is not in the
        // palette dialog's context stack, so Toggle Terminal would otherwise
        // show no tip. Same pattern as Toggle Side Bar / preview.
        KeyBinding::new("secondary-`", SummonTerminal, None),
        KeyBinding::new("secondary-q", Quit, None),
        // No key-context: layout must switch even when the Typst/PDF viewer or
        // Markdown preview holds focus (those entities do not inherit Workspace).
        KeyBinding::new("secondary-1", SetLayout(Layout::Editor), None),
        KeyBinding::new("secondary-2", SetLayout(Layout::Split), None),
        KeyBinding::new("secondary-3", SetLayout(Layout::Preview), None),
        // Toggle editor|preview <-> editor-only (default open is already split,
        // so a one-shot "open preview" feels like a no-op without this).
        KeyBinding::new("secondary-shift-v", TogglePreviewLayout, None),
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
        KeyBinding::new("secondary-g", GoToLine, None),
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
}
