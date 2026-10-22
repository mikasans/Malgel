# End-to-end tests

`gui_smoke.sh` runs the real Malgel window and drives it with the keyboard,
the way a person would. It is the counterpart to `cargo test`: the unit tests
check the pieces in isolation, this checks that the pieces add up to an
application that starts, paints and responds.

## What it does

1. Starts a headless X server (Xvfb) and a throwaway workspace of sample
   files (Markdown, Rust, JSON, a shell script, CSV, an image, a PDF and a
   Typst document).
2. Launches the built binary against that workspace, waits for the window to
   appear and screenshots it.
3. Types into the editor and presses `Ctrl+S`, then checks the text reached
   the file on disk.
4. Opens the command palette (`Ctrl+Shift+P`), filters it by typing, closes it
   with `Escape`, and asserts the frame changed at each step, so a shortcut
   that silently does nothing fails the run.
5. Opens Quick Open (`Ctrl+P`) and closes it.
6. Presses `Ctrl+B` in the editor and checks the file on disk came back bold,
   with `**`.
7. Focuses the Explorer (`Ctrl+Shift+E`), renames the picked file with `F2`,
   and checks the file on disk was renamed and still holds its contents.
8. Closes an editor with `Ctrl+W` and brings it back with `Ctrl+Shift+T`.
9. Pins the tab in front from the palette, then runs Close All Editors
   (`Ctrl+K Ctrl+W`) and checks the editor pane is untouched — the pinned
   tab stayed.
10. Searches the folder's files with `Ctrl+Shift+F`, takes the first hit, and
    types into the file the hit opened — so the check is on the file the hit
    came from, not on a changed frame.
11. Moves that line down with `Alt+↓` and saves, checking the file's first two
    lines swapped.
12. Flips the line ending from the palette and saves, checking the bytes on
    disk carry carriage returns.
13. Opens the PDF fixture and switches the viewer's page thumbnails on and off
    from the command palette, checking the strip takes the width it should and gives it
    back.
14. Zooms the PDF and drags it, checking the pane moved.
15. Opens the image viewer: a bare wheel leaves it alone, `Ctrl` plus the
    wheel zooms it.
16. Zooms the editor itself with `Ctrl` plus the wheel, checking the interface
    font size changed in the saved settings — a scrolled pane would change the
    frame too.
17. Opens a `.typ` file, and rewrites it while it is open: the app compiles the
    source itself and the pages follow the source as it changes on disk. The
    source imports a package from a package folder the run sets up, so a
    package the preview cannot resolve fails this step too.
18. Double-clicks a row and checks it started nothing, then double-clicks the
    space the entries do not fill and checks the file it asked for landed in
    the folder's root, the way VS Code's Explorer makes one.
19. Finds the title bar's Toggle Side Bar button in the frame — the cluster
    it sits in follows the interface font size, so a fixed offset would click
    the wrong button after a zoom — clicks it, and checks the panes shift and
    the setting the app saved says the sidebar is gone; clicking it again
    brings both back.
20. Folds a Markdown heading with `Ctrl+Shift+[`, checks the body leaves the
    frame, then unfolds with `Ctrl+Shift+]`.
21. Checks the process is still alive at the end (no crash from any step).

A viewer has no text field for the keyboard to land in, so steps 13 and 15
click — the two places in this script that do. `title_bar_button.py` locates
one button in a screenshot for step 19; nothing else in the script looks at
pixels to decide where to click.

The script kills the app and its X server on the way out, whatever happens. A
run that stops early would otherwise leave Malgel alive, and a second launch
hands its arguments to the running instance instead of opening a window, so
every later run would drive the stale window.

Screenshots land in `target/e2e-artifacts/` for eyeballing after a failure.

## Running it

Needs a release binary, plus `python3`, `xvfb`, `xdotool`, ImageMagick
(`import` / `compare` / `identify`) and Mesa's software Vulkan (the
`mesa-vulkan-drivers` package). Those are installed for the Linux build in
`.cnb.yml`-style images and in any desktop Linux; the software renderer means
no GPU is required.

```sh
cargo build --release
scripts/e2e/gui_smoke.sh
```

Overridable with environment variables: `MALGEL_BIN` (binary to run),
`E2E_ARTIFACTS` (where shots go), `E2E_DISPLAY`, `E2E_SETTLE` (seconds to let
a frame land before each screenshot — raise it on a slow machine).

A passing run prints `PASS` and the list of screenshots; any failed check
exits non-zero with the reason.

## Performance gate

`perf_scenes.sh` is a regression gate, not a demo: it starts its own Xvfb
(on a display number nobody holds, killed by PID on the way out), runs the
release binary three times with a fresh HOME each time, and has
`perf_probe.py` measure every run from the outside — XTest key events in,
root-window read-backs (XGetImage, every few ms) out. The medians land in
`target/e2e-artifacts/perf-scenes/perf.json` together with each run's
numbers and screenshots.

| Metric | How |
|---|---|
| `first_frame_ms`, `settled_ms` | launch → first changed pixel; → last change before 700 ms of stillness |
| `typst_done_ms` | the 12-page sample's `first_typst_preview_pages_applied` mark |
| `editor_input_p50_ms` / `p95` | key → changed frame, 21 keys typed into Markdown |
| `terminal_input_p50_ms` / `p95` | the same in the terminal composer |
| `terminal_seq_ms`, `terminal_cat20mb_ms` | Enter → shell done and panel still (`seq 1 200000`, `cat` of 20 MB) |
| `ctrl_c_to_prompt_ms` | Ctrl+C during `sleep` → process gone and transcript repainted |
| `idle_cpu_pct` | app + children CPU over 5 s with the window in the background |
| `idle_cpu_focused_pct` | the same with focus (caret blinking); reported, not gated |
| `rss_peak_mib`, `rss_final_mib` | VmHWM / VmRSS at the end |

`perf_thresholds.json` holds the budget. A median over a blocking `max`
exits 1; `"blocking": false` only warns (`PERF_STRICT=1` makes it block).

```sh
cargo build --release
scripts/e2e/perf_scenes.sh                        # software GL (CI)
E2E_GPU=1 scripts/e2e/perf_scenes.sh              # Vulkan, lavapipe autodetected
python3 scripts/e2e/perf_probe.py compare a/perf.json b/perf.json   # ±15%
```

To prove the gate bites, `MALGEL_PERF_INJECT_SLEEP_MS=200` stalls every
frame by 200 ms (honoured only under `MALGEL_PERF=1`, which the script sets)
and the run fails on first frame and input latency.
