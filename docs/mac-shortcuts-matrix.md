# Shortcut matrix (Win/Linux ↔ macOS)

Testable reference for default bindings. `secondary` is **Ctrl** on Windows/Linux
and **⌘** on macOS. Aligns with VS Code defaults where Malgel offers the same
command. Mac-only verification needs a real Mac (or Codemagic); Linux box covers
parse tests and Xvfb smoke for the Win/Linux column.

| Action | Win / Linux | macOS | Notes |
| --- | --- | --- | --- |
| New File | Ctrl+N | ⌘N | |
| Open File | Ctrl+O | ⌘O | |
| Open Folder | Ctrl+K Ctrl+O | ⌘⌃O | macOS menus take one key equivalent; chord kept off menu |
| Open Recent | Ctrl+R | ⌘R | |
| Save | Ctrl+S | ⌘S | |
| Save As | Ctrl+Shift+S | ⌘⇧S | |
| Save All | Ctrl+K S | ⌘K S | Editor context rebinds so Link (⌘K) does not steal |
| Close Editor | Ctrl+W | ⌘W | |
| Close Others | Ctrl+Alt+W | ⌘⌥W | |
| Close All Editors | Ctrl+K Ctrl+W | ⌘K ⌘W | Editor-context chord on macOS |
| Reopen Closed | Ctrl+Shift+T | ⌘⇧T | |
| Close Window | Ctrl+Shift+W | ⌘⇧W | |
| Quit | Ctrl+Q | ⌘Q | App menu on macOS |
| Next Tab | Ctrl+Tab / Ctrl+PgDn | Ctrl+Tab / ⌘⌥→ | **Not** ⌘⇧] (Fold) |
| Previous Tab | Ctrl+Shift+Tab / Ctrl+PgUp | Ctrl+Shift+Tab / ⌘⌥← | **Not** ⌘⇧[ (Unfold) |
| Go Back | Alt+← | Ctrl+- | ⌘- is Zoom Out |
| Go Forward | Alt+→ | Ctrl+Shift+- | |
| Toggle Side Bar | Ctrl+B | ⌘B | Bold is Shift variant |
| Bold | Ctrl+Shift+B | ⌘⇧B | |
| Italic | Ctrl+I | ⌘I | |
| Focus Explorer | Ctrl+Shift+E | ⌘⇧E | |
| Find in Files | Ctrl+Shift+F | ⌘⇧F | |
| Replace in Files | Ctrl+Shift+H | ⌘⇧H | |
| Replace (editor) | Ctrl+H | ⌘⌥F | |
| Focus Mode | Ctrl+Alt+F | ⌘⌥⇧F | Not ⌘⌥F (Replace) |
| Find | Ctrl+F | ⌘F | Editor library |
| Go to Line | Ctrl+G | **Ctrl+G** | Not ⌘G (Find Next on Mac) |
| Go to Symbol | Ctrl+Shift+O | ⌘⇧O | |
| Go to Heading | Ctrl+Shift+. | ⌘⇧. | |
| Command Palette | Ctrl+Shift+P | ⌘⇧P | |
| Quick Open | Ctrl+P | ⌘P | |
| Toggle Preview | Ctrl+Shift+V | ⌘⇧V | |
| Split Right | Ctrl+\ | ⌘\ | |
| Toggle Terminal | Ctrl+` | Ctrl+` **and** ⌘J | ⌘` is macOS window-cycle |
| New Terminal | Ctrl+Shift+` | Ctrl+Shift+` | |
| Fold | Ctrl+Shift+[ | ⌘⇧[ | |
| Unfold | Ctrl+Shift+] | ⌘⇧] | |
| Toggle Line Comment | Ctrl+/ | ⌘/ | |
| Delete Line | Ctrl+Shift+K | ⌘⇧K | |
| Move Line Up/Down | Alt+↑ / Alt+↓ | ⌥↑ / ⌥↓ | |
| Toggle Word Wrap | Alt+Z | ⌥Z | |
| Settings | Ctrl+, | ⌘, | App menu on macOS |
| Keyboard Shortcuts | Ctrl+K Ctrl+S | ⌘K ⌘S | |
| Zoom In / Out / Reset | Ctrl+= / - / 0 | ⌘= / - / 0 | |
| Menubar mnemonics | Alt+F/E/V/G/O/H | — | macOS uses system menu bar |

## System / platform conflicts (macOS)

| Chord | System / convention | Malgel choice |
| --- | --- | --- |
| ⌘` | Cycle windows | Terminal uses **Ctrl+`** (+ ⌘J) |
| ⌘H | Hide app | Replace in Files is ⌘⇧H |
| ⌘G | Find Next | Go to Line is **Ctrl+G** |
| ⌘- | Zoom Out | Go Back is **Ctrl+-** |
| ⌘⇧[ / ] | Fold (VS Code) | Tabs use ⌘⌥←/→ instead |
| Menu chords | One key equivalent | Open Folder → ⌘⌃O; Close All stays keyboard chord |

## How to check

- Unit: `cargo test --lib` (binding parse + Fold/tab non-collision).
- Linux/Xvfb: `scripts/e2e/gui_smoke.sh` (Ctrl chords).
- macOS-only: menu accelerators, ⌘J terminal, ⌘⌥←/→ tabs, Ctrl+G go-to-line, Ctrl+- back — Codemagic / local Mac.
