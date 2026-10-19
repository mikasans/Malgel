# Aligning Malgel with VS Code

Malgel is a Markdown editor, not an IDE. The goal is to feel familiar to
someone who lives in VS Code — the same keys, the same places to find
commands, the same visual language for a file's type — not to grow every IDE
feature. The scope is documents: writing, navigating and reading Markdown.

The reference is VS Code's **default** behaviour (its own `keybindings.json`
is not considered).

## What is done

- **Command palette** (Ctrl+Shift+P). Every command in one searchable list,
  grouped File / Explorer / Edit / Go / View / Format / Settings / Help. Each
  row shows the command's current key binding, and confirming a row runs its
  Action. `src/commands.rs` holds the list; the palette reuses GPUI Kit's
  `Command` component.

  The palette lives in a dialog, and an action confirmed there is dispatched
  along the dialog's own focus path — which the window's workspace is not part
  of. Its rows therefore carry no action at all; the workspace keeps the Action
  beside each label and dispatches the confirmed row itself, from the window's
  own focus path. Without that, every row looked right and did nothing.

- **File-type icons.** `src/file_icon.rs` loads a VS Code–compatible icon
  theme (`icon-themes/material/theme.json` + SVGs from Material Icon Theme).
  The file tree, tab strip, Quick Open and Recent draw those icons; Lucide
  glyphs remain the fallback. Swap packages by dropping another theme folder
  with the same JSON shape.

- **Find in Files** (Ctrl+Shift+F). Asks what to look for, searches the text
  of every file under the open folder — the same ignore-aware walk Quick Open
  uses — and lists the lines that hold it as `path:line  text`. Confirming a
  line opens the file with the caret on it. `src/search.rs` holds the search
  itself, off the UI thread; a file that is not UTF-8, or that holds a NUL
  byte, is skipped rather than guessed at. Matching is VS Code's smart case: a
  query with no uppercase letter matches either case, one with an uppercase
  letter matches exactly.

- **The Explorer's commands** (Ctrl+Shift+E shows the sidebar, focuses the
  tree and picks the open file). While the tree has the keyboard its keys are
  VS Code's, and they mean nothing outside it — in the editor Del still
  deletes text and Ctrl+X still cuts the selection. `src/file_tree.rs` holds
  them, under its own key context:

  | Command | Key | Also |
  | --- | --- | --- |
  | Move the pick, open and close folders | ↑ ↓ ← → | |
  | First / last entry | Home / End | |
  | Open the picked file | Enter | double click |
  | Rename | F2 | right-click menu |
  | Delete | Del / Backspace | right-click menu |
  | Cut / copy / paste | Ctrl+X / Ctrl+C / Ctrl+V | right-click menu |
  | New file / new folder | — | the view header buttons, the right-click menu, the palette |

  A new name may be nested (`notes/plan.md`) and builds the folders on the way;
  a rename stays in its folder. Open editors follow both, and deleting an entry
  closes the editors that were showing it.

- **Pinned tabs.** Pin or unpin from a tab's own menu or the palette; a pinned
  tab shows a pin where its close box was, survives Close Other Tabs and Close
  All Editors, and is unpinned when its editor closes.

- **Close All Editors** (Ctrl+K Ctrl+W) and **Reopen Closed Editor**
  (Ctrl+Shift+T, which skips files that have since gone).

- **Back / forward** through the editors shown (Alt+← / Alt+→). The history is
  the sequence of files the window has shown, so it walks tabs and files alike.

- **A tab's own menu** carries **Copy Path** and **Reveal in File Manager**,
  as VS Code's does. Both act on that tab's file and only appear once it has
  one — an untitled buffer has no path to copy or reveal.

- **The status bar** reads like VS Code's. The line ending is a button:
  clicking it switches the document between LF and CRLF (the text is
  untouched; the file is marked unsaved and saving writes the new ending, the
  way VS Code's *Change End of Line Sequence* does). Beside it sit the indent
  size (`Spaces: 2`, the editor's tab size) and the language — `Markdown`,
  which is the only one Malgel edits.

- **Move Line Up / Down** (Alt+↑ / Alt+↓). The lines the selection touches
  move as one block and the selection travels with them; at the ends of the
  document nothing happens, as in VS Code. The editor library has no action
  for this, so `src/format.rs` owns it as a pure text transform, like the
  other formatting commands. Plain Alt with an arrow is free: the library
  binds Ctrl+Alt+↑/↓ (and Shift+Alt+↑/↓) to adding a cursor above and below.

- **The wheel means one thing everywhere.** Command or Control plus the wheel
  zooms — the editor, the preview, the image viewer, the diagram window and
  the PDF viewer. On its own the wheel scrolls, or moves a diagram. The
  editor and the preview need a window-level listener for this: the editor's
  own wheel handling scrolls and takes the event before the pane underneath
  sees it, and GPUI gives an element no capture-phase hook for the wheel.

- **Go to Line** (Ctrl+G) and **Toggle Word Wrap** (Alt+Z).

- **Current-line highlight.** The editor draws a band over the line the caret
  is on, the way VS Code does, in `editor.active_line.background`
  (`#F5F5F5` light, `#171717` dark). GPUI Kit's editor paints it from the
  theme and both of Malgel's themes set that colour, so no Malgel code turns
  it on or off — it is the editor's own default. It is listed here because
  the alignment's first draft had it as still to do; it turned out to be
  already there.

- **Key bindings follow VS Code.** Where Malgel had taken a VS Code key for
  something else, the other command moved or lost its key:

  | Key | VS Code | Malgel before | Now |
  | --- | --- | --- | --- |
  | Ctrl+B | Toggle Side Bar | Ctrl+Shift+B | Ctrl+B from the workspace; Bold still takes Ctrl+B inside the editor |
  | Ctrl+Shift+F | Find in Files | Focus mode | Find in Files; Focus mode moved to Ctrl+Alt+F |
  | Ctrl+Shift+E | Explorer | Export as HTML | Explorer; Export is in the File menu |
  | Ctrl+Shift+L | Select All Occurrences | Switch light and dark | freed; the switch is in the View menu |

  Ctrl+1/2/3 (layout, not editor groups) and Ctrl+K (Link, and a chord
  prefix) stay as Malgel has them: there are no editor groups and no other
  chord to prefix.

- **Menus.** A **Go** menu (Show all commands, Go to file / heading / line,
  Back, Forward) groups navigation the way VS Code does; **Settings** moved
  under File on Windows and Linux, matching File ▸ Preferences; the File menu
  carries Close all editors and Reopen closed editor.

## Platform keys (macOS)

See [`mac-shortcuts-matrix.md`](./mac-shortcuts-matrix.md) for the Win/Linux ↔ macOS
chord matrix (Cmd vs Ctrl, Fold vs tab, Go to Line, terminal).

## How it is checked

`cargo test` covers the pieces (the key-binding table, the icon map, the
entry-name rules, the formatting transforms). On top of that,
`scripts/e2e/gui_smoke.sh` runs the real window headlessly and drives it the
way a person would. It types into the open document and saves it; opens,
filters and confirms the command palette; opens Quick Open and picks a file;
presses Ctrl+B inside the editor to check Bold still wins there; focuses the
Explorer and renames the picked file with F2; closes an editor and brings it
back with Ctrl+Shift+T; pins a tab; searches the folder with Ctrl+Shift+F,
taking a hit and typing into the file it opened; moves that line down with
Alt+↓; and switches the line ending to CRLF, checking the bytes on disk. It
asserts on the results —
the file on disk, the changed frame, the editor pane a pinned tab keeps — so a
shortcut or a command that stops working fails the run. See
`scripts/e2e/README.md`.

## What is left

- Editor behaviour that GPUI Kit's editor does not offer yet: bracket
  matching, indent guides and Ctrl+D (Select All Occurrences). Each needs a
  piece of the editor the library does not expose — an action, a selection
  API or a renderer — so they wait on it, not on Malgel. They are also the
  code-editing side of VS Code rather than the document side Malgel aims at,
  so they are not being pursued.

## Non-goals

Activity bar, minimap, breadcrumbs, an embedded terminal panel, **coloured
(Seti) file icons**, and Debug / Source Control / Extensions / multi-root
workspaces. Malgel edits documents; these belong to an IDE.
