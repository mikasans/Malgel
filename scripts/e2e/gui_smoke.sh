#!/usr/bin/env bash
#
# End-to-end smoke test for Malgel's GUI.
#
# `cargo test` covers the pieces in isolation; nothing so far proves the whole
# application boots, paints a window and answers the keyboard. This script
# does: it starts a headless X server (Xvfb), runs the real binary against a
# throwaway workspace, drives it with xdotool, and checks the results. Where a
# step has an observable result (a saved file, a changed frame) it asserts on
# that rather than on pixels alone.
#
#     scripts/e2e/gui_smoke.sh
#
# Requirements (Debian/Ubuntu names): xvfb, xdotool, imagemagick, and Mesa's
# software renderers (libgl1-mesa-dri + libegl1 + libegl-mesa0). A binary must
# already be built (`cargo build` or `--release`); point MALGEL_BIN at anything
# else to override. scripts/e2e/Dockerfile carries all of this.
#
# The software GL renderer is used by default so no GPU is needed: Xvfb has no
# DRI3, and the Vulkan present path needs it, so a Vulkan render shows a
# mapped but unpainted window, while the GL path draws fine. Set E2E_GPU=1 to
# keep Vulkan (a machine with a real GPU).
#
# Exit status is 0 only when every check passes.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

BIN="${MALGEL_BIN:-$ROOT/target/release/malgel}"
[[ -x "$BIN" ]] || BIN="$ROOT/target/debug/malgel"
ART_DIR="${E2E_ARTIFACTS:-$ROOT/target/e2e-artifacts}"
DISPLAY_NUM="${E2E_DISPLAY:-:99}"
SETTLE="${E2E_SETTLE:-1.2}" # seconds to let a frame land before each shot

mkdir -p "$ART_DIR"
rm -f "$ART_DIR"/*.png "$ART_DIR"/*.log 2>/dev/null || true

fail() { echo "FAIL: $*" >&2; exit 1; }

# Leave nothing behind: a run that stops early would otherwise leave the app
# and its X server alive, and Malgel hands a second launch's arguments to a
# running instance instead of opening a window — every later run would then
# drive the stale window from the failed one.
APP_PID=""
XVFB_PID=""
cleanup() {
  [[ -n "$APP_PID" ]] && kill "$APP_PID" 2>/dev/null
  [[ -n "$XVFB_PID" ]] && kill "$XVFB_PID" 2>/dev/null
  rm -f "/tmp/.X${DISPLAY_NUM#:}-lock" "/tmp/.X11-unix/X${DISPLAY_NUM#:}" 2>/dev/null
  return 0
}
trap cleanup EXIT
pass() { echo "  ok: $*"; }

command -v Xvfb >/dev/null || fail "Xvfb not installed"
command -v xdotool >/dev/null || fail "xdotool not installed"
command -v import >/dev/null || fail "ImageMagick 'import' not installed"
[[ -x "$BIN" ]] || fail "no binary at $BIN — build it first (cargo build)"

cleanup() {
  [[ -n "${APP_PID:-}" ]] && kill "$APP_PID" 2>/dev/null || true
  [[ -n "${XVFB_PID:-}" ]] && kill "$XVFB_PID" 2>/dev/null || true
}
trap cleanup EXIT

# A previous run that was killed can leave a server, a lock and a socket
# behind; a second Xvfb on the same display then refuses to start and the app
# fails to connect. Clear ours, then start and wait for it, retrying once.
rm -rf /tmp/malgel-e2e-xdg && mkdir -p /tmp/malgel-e2e-xdg && chmod 700 /tmp/malgel-e2e-xdg
# A fresh HOME keeps the run reproducible: no session to restore, no settings
# or recently-opened list carried over from an earlier run or a real profile.
export HOME=/tmp/malgel-e2e-home
rm -rf "$HOME" && mkdir -p "$HOME"
export XDG_CONFIG_HOME="$HOME/.config" XDG_DATA_HOME="$HOME/.local/share"
export DISPLAY="$DISPLAY_NUM"
num="${DISPLAY_NUM#:}"
start_xvfb() {
  pkill -f "Xvfb $DISPLAY_NUM" 2>/dev/null || true
pkill -f "$BIN" 2>/dev/null || true
  sleep 0.3
  rm -f "/tmp/.X${num}-lock" "/tmp/.X11-unix/X${num}" 2>/dev/null || true
  Xvfb "$DISPLAY_NUM" -screen 0 1600x1000x24 +extension GLX +render -nolisten tcp \
    >"$ART_DIR/xvfb.log" 2>&1 &
  XVFB_PID=$!
  for _ in $(seq 1 50); do
    xdotool getdisplaygeometry >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  return 1
}
start_xvfb || start_xvfb || { cat "$ART_DIR/xvfb.log" >&2; fail "Xvfb did not come up on $DISPLAY_NUM"; }

export XDG_RUNTIME_DIR=/tmp/malgel-e2e-xdg
if [[ "${E2E_GPU:-0}" != "1" ]]; then
  export LIBGL_ALWAYS_SOFTWARE=1
  export GALLIUM_DRIVER=llvmpipe
  # A path with no ICD makes wgpu skip Vulkan and pick the GL renderer, whose
  # present path Xvfb actually supports (see the header note).
  export VK_ICD_FILENAMES="$ART_DIR/no-such-icd.json"
fi

# A small workspace whose file types exercise the icon map: Markdown, Rust,
# JSON, a script and CSV.
WS="$(mktemp -d)"
mkdir -p "$WS/notes" "$WS/src"
printf '# Malgel end-to-end\n\nHello from the smoke test.\n' >"$WS/README.md"
printf '# Second document\n\nA paragraph.\n' >"$WS/notes/todo.md"
printf 'fn main() { println!("hi"); }\n' >"$WS/src/main.rs"
printf '{"a": 1}\n' >"$WS/config.json"
printf '#!/bin/sh\necho hi\n' >"$WS/run.sh"
printf 'name,qty\nwidget,3\n' >"$WS/data.csv"
# An image for the viewer, made here so the check needs no fixture file.
convert -size 320x240 gradient:'#ffffff-#2f6fd0' "$WS/shot.png"

# A PDF for the viewer, written by hand so the check needs no exporter.
python3 - "$WS/page.pdf" <<'PY_EOF'
import sys

W, H, PAGES = 612.0, 792.0, 3
out = bytearray(b"%PDF-1.4\n")
offsets = []


def add(number, body):
    offsets.append(len(out))
    out.extend(f"{number} 0 obj\n".encode())
    out.extend(body)
    out.extend(b"\nendobj\n")


add(1, b"<< /Type /Catalog /Pages 2 0 R >>")
kids = " ".join(f"{4 + ix} 0 R" for ix in range(PAGES))
add(2, f"<< /Type /Pages /Kids [{kids}] /Count {PAGES} >>".encode())
for ix in range(PAGES):
    add(
        4 + ix,
        (
            f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {W:g} {H:g}] "
            f"/Resources << >> /Contents {4 + PAGES + ix} 0 R >>"
        ).encode(),
    )
for ix in range(PAGES):
    # Stripes rather than a flat fill: a drag scrolls the page, and stripes
    # are what makes that visible in a screenshot.
    stream = "".join(
        f"{(row % 3) * 0.3 + 0.1:.2f} {(row % 5) * 0.18 + 0.1:.2f} 0.7 rg\n"
        f"0 {H * row / 40:g} {W:g} {H / 40:g} re\nf\n"
        for row in range(40)
    ).encode()
    add(4 + PAGES + ix, b"<< /Length %d >>\nstream\n" % len(stream) + stream + b"endstream")

xref_at = len(out)
count = len(offsets) + 1
out.extend(f"xref\n0 {count}\n".encode())
out.extend(b"0000000000 65535 f \n")
for offset in offsets:
    out.extend(f"{offset:010d} 00000 n \n".encode())
out.extend(
    f"trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n".encode()
)
with open(sys.argv[1], "wb") as handle:
    handle.write(out)
PY_EOF

shot() { # shot <name>
  sleep "$SETTLE"
  import -display "$DISPLAY" -window root "$ART_DIR/$1.png" 2>/dev/null
}
colours() { identify -format '%k' "$ART_DIR/$1.png" 2>/dev/null || echo 0; }
# `compare -metric AE` writes the count to stderr and exits non-zero when the
# images differ; capture both and keep the number. ImageMagick prints large
# counts in exponent form (`1.04984e+06`), which `[[ -gt ]]` cannot read, so
# `awk` turns the metric back into a whole number.
pixels_differ() {
  { compare -metric AE "$ART_DIR/$1.png" "$ART_DIR/$2.png" null: 2>&1 || true; } |
    awk '{ printf "%.0f", $1 + 0 }'
}
# The same, over one region of the window (ImageMagick `WxH+X+Y`), so a check
# can look at the editor pane alone and ignore the tab bar next to it.
region_differ() {
  convert "$ART_DIR/$1.png" -crop "$3" +repage "$ART_DIR/.region-a.png"
  convert "$ART_DIR/$2.png" -crop "$3" +repage "$ART_DIR/.region-b.png"
  { compare -metric AE "$ART_DIR/.region-a.png" "$ART_DIR/.region-b.png" null: 2>&1 || true; } |
    awk '{ printf "%.0f", $1 + 0 }'
}
# Dismiss an overlay (palette, Quick Open). The first Escape clears the search
# box, the second closes; sending both is harmless when one would do.
dismiss() {
  xdotool key --clearmodifiers Escape
  sleep 0.2
  xdotool key --clearmodifiers Escape
}
# Poll for a condition instead of sleeping a fixed time before checking it. A
# save, a rename or a search lands when it lands: a fixed sleep after Ctrl+S is
# either wasted or too short. On a slow machine the write finished late, the
# check read the file early, and step 9 failed while the same commit passed on
# the fast CI runner. Making the assertion about the result rather than the
# clock costs nothing when it is already true and waits when it is not.
wait_for() { # wait_for <seconds> <command...>
  local tries=$(( $1 * 10 ))
  shift
  local i
  for ((i = 0; i < tries; i++)); do
    if "$@"; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}
# The interface font size Malgel has saved; the zoom writes it there.
font_size() {
  sed -n 's/.*"font_size": *\([0-9.]*\).*/\1/p' \
    "$HOME/.config/malgel/settings.json" 2>/dev/null
}

echo "== malgel GUI smoke =="
echo "binary:   $BIN"
echo "display:  $DISPLAY ($(xdotool getdisplaygeometry))"

# Open the folder so the file tree is exercised, and a file so the editor is.
"$BIN" "$WS" "$WS/README.md" >"$ART_DIR/app.log" 2>&1 &
APP_PID=$!

# 1. The window comes up and the process survives.
WIN=""
for _ in $(seq 1 100); do
  WIN="$(xdotool search --onlyvisible --class malgel 2>/dev/null | head -1 || true)"
  [[ -z "$WIN" ]] && WIN="$(xdotool search --onlyvisible --name -i malgel 2>/dev/null | head -1 || true)"
  [[ -n "$WIN" ]] && break
  kill -0 "$APP_PID" 2>/dev/null || fail "process exited during startup; see $ART_DIR/app.log"
  sleep 0.1
done
[[ -n "$WIN" ]] || fail "no window appeared"
pass "window $WIN mapped ($(xdotool getwindowgeometry "$WIN" | tail -1 | tr -s ' '))"
# No window manager under Xvfb, so focus the window directly for key events.
xdotool windowfocus "$WIN" 2>/dev/null || true
xdotool windowraise "$WIN" 2>/dev/null || true
shot 01-startup
[[ "$(colours 01-startup)" -gt 8 ]] || fail "startup frame is blank ($(colours 01-startup) colours)"
pass "startup painted ($(colours 01-startup) colours)"

# 2. Typing reaches the editor and Ctrl+S writes the file the app opened.
#    The deepest check: input path, editor, save and disk, end to end.
xdotool type --delay 30 'SMOKE'
sleep 0.3
xdotool key --clearmodifiers ctrl+s
shot 02-edited-and-saved
wait_for 5 grep -q 'SMOKE' "$WS/README.md" || fail "typed text was not saved to README.md"
pass "typed text reached the file (edit + Ctrl+S round-trip)"

# 3. Ctrl+Shift+P opens the command palette; typing filters it; Escape closes.
xdotool key --clearmodifiers ctrl+shift+p
shot 03-command-palette
[[ "$(pixels_differ 02-edited-and-saved 03-command-palette)" -gt 1000 ]] \
  || fail "Ctrl+Shift+P did not change the frame"
xdotool type --delay 40 'save'
shot 04-command-palette-filtered
[[ "$(pixels_differ 03-command-palette 04-command-palette-filtered)" -gt 100 ]] \
  || fail "typing did not filter the command palette"
dismiss
shot 05-after-palette
[[ "$(pixels_differ 03-command-palette 05-after-palette)" -gt 1000 ]] \
  || fail "the command palette did not close"
pass "command palette opens, filters and closes (Ctrl+Shift+P)"

# 4. Ctrl+P opens Quick Open over the workspace files.
xdotool key --clearmodifiers ctrl+p
shot 06-quick-open
[[ "$(pixels_differ 05-after-palette 06-quick-open)" -gt 1000 ]] \
  || fail "Ctrl+P did not change the frame"
dismiss
shot 07-after-quick-open
[[ "$(pixels_differ 06-quick-open 07-after-quick-open)" -gt 1000 ]] \
  || fail "Quick Open did not close"
pass "Quick Open opens and closes (Ctrl+P)"

# 5. Ctrl+B in the editor is Bold, as in VS Code (Toggle Side Bar is the same
#    key outside the editor). Assert on the file, not the pixels.
xdotool key --clearmodifiers ctrl+a
xdotool key --clearmodifiers ctrl+b
sleep 0.3
xdotool key --clearmodifiers ctrl+s
shot 08-bold
wait_for 5 grep -q '\*\*' "$WS/README.md" || fail "Ctrl+B in the editor did not apply bold"
pass "Ctrl+B in the editor applies bold (Ctrl+S round-trip)"

# 6. The Explorer: Ctrl+Shift+E moves the keyboard into the file tree and
#    picks the file that is open; F2 renames it, and the file on disk follows.
xdotool key --clearmodifiers ctrl+shift+e
shot 09-explorer-focus
xdotool key --clearmodifiers F2
shot 10-rename-prompt
[[ "$(pixels_differ 09-explorer-focus 10-rename-prompt)" -gt 1000 ]] \
  || fail "F2 did not open the rename prompt"
xdotool key --clearmodifiers ctrl+a
xdotool type --delay 40 'renamed.md'
xdotool key --clearmodifiers Return
sleep 0.8
shot 11-after-rename
wait_for 5 test -f "$WS/renamed.md" || fail "F2 in the Explorer did not rename the file on disk"
[[ ! -e "$WS/README.md" ]] || fail "the old name is still on disk after the rename"
wait_for 5 grep -q 'SMOKE' "$WS/renamed.md" || fail "the renamed file lost its contents"
pass "F2 in the Explorer renames the picked file on disk"

# 7. Close the editor showing a second file, then bring it back with
#    Ctrl+Shift+T, the way VS Code's Reopen Closed Editor does.
xdotool key --clearmodifiers ctrl+p
xdotool type --delay 40 'todo'
xdotool key --clearmodifiers Return
sleep 0.8
shot 12-second-file
[[ "$(pixels_differ 11-after-rename 12-second-file)" -gt 1000 ]] \
  || fail "Quick Open did not open the second file"
xdotool key --clearmodifiers ctrl+w
sleep 0.6
shot 13-closed
[[ "$(pixels_differ 12-second-file 13-closed)" -gt 1000 ]] \
  || fail "Ctrl+W did not close the second editor"
xdotool key --clearmodifiers ctrl+shift+t
sleep 0.8
shot 14-reopened
[[ "$(pixels_differ 13-closed 14-reopened)" -gt 1000 ]] \
  || fail "Ctrl+Shift+T did not reopen the closed editor"
pass "Ctrl+W closes and Ctrl+Shift+T reopens an editor"

# 8. Pin the tab in front from the palette, then Close All Editors: a pinned
#    tab survives, the way VS Code leaves pinned tabs alone.
xdotool windowfocus "$WIN" 2>/dev/null || true
xdotool key --clearmodifiers ctrl+shift+p
shot 15-palette-for-pin
[[ "$(pixels_differ 14-reopened 15-palette-for-pin)" -gt 1000 ]] \
  || fail "Ctrl+Shift+P did not reopen the command palette"
xdotool type --delay 40 'pin'
xdotool key --clearmodifiers Return
sleep 0.8
shot 16-pinned
[[ "$(pixels_differ 14-reopened 16-pinned)" -gt 30 ]] \
  || fail "pinning the tab did not change the tab bar"
xdotool key --clearmodifiers ctrl+k ctrl+w
sleep 0.8
shot 17-after-close-all
# The pinned editor keeps its place, so the pane it fills is untouched; only
# the unpinned tab leaves the tab bar above it.
[[ "$(region_differ 16-pinned 17-after-close-all 480x600+430+160)" -lt 50 ]] \
  || fail "Close All Editors closed the pinned editor too"
pass "a pinned tab survives Close All Editors (Ctrl+K Ctrl+W)"

# 9. Find in Files (Ctrl+Shift+F): search the folder's files for text, then
#    take a hit, type into the file it opened and save — so the assertion is
#    on the file the hit came from, not on a changed frame.
xdotool key --clearmodifiers ctrl+shift+f
sleep 0.5
xdotool type --delay 40 'SMOKE'
xdotool key --clearmodifiers Return
sleep 1.5
shot 18-find-in-files
[[ "$(pixels_differ 17-after-close-all 18-find-in-files)" -gt 1000 ]] \
  || fail "Ctrl+Shift+F did not open the Find in Files results"
xdotool key --clearmodifiers Return
sleep 1.0
shot 19-search-hit-open
xdotool type --delay 30 'FOUND '
xdotool key --clearmodifiers ctrl+s
wait_for 5 grep -q 'FOUND' "$WS/renamed.md" || fail "the search hit did not open the file it came from"
pass "Find in Files opens the file a hit came from (Ctrl+Shift+F)"

# 10. Alt+↓ moves the caret's line down, the way VS Code's Move Line Down
#     does. The caret is still on the line the search opened, so the check is
#     on the file: the line that was first is second after saving.
before="$(sed -n '1p' "$WS/renamed.md")"
xdotool key --clearmodifiers alt+Down
sleep 0.5
xdotool key --clearmodifiers ctrl+s
sleep 0.5
[[ "$(sed -n '1p' "$WS/renamed.md")" != "$before" ]] \
  || fail "Alt+Down did not move the first line down"
sed -n '2p' "$WS/renamed.md" | grep -q 'FOUND' \
  || fail "the moved line did not land on line 2"
pass "Alt+↓ moves the caret's line down (Ctrl+S round-trip)"

# 11. Switching the line ending, the way clicking the EOL item in VS Code's
#     status bar does: the palette command flips LF to CRLF, and saving writes
#     the file with carriage returns. The assertion is on the bytes on disk.
xdotool key --clearmodifiers ctrl+shift+p
sleep 0.5
xdotool type --delay 40 'end of line'
sleep 0.4
xdotool key --clearmodifiers Return
sleep 0.5
xdotool key --clearmodifiers ctrl+s
shot 20-line-ending
wait_for 5 grep -q $'\r' "$WS/renamed.md" \
  || fail "the EOL toggle did not save the file with CRLF"
pass "changing the line ending saves the file with CRLF"
# 12. The PDF viewer: open the fixture and switch its page thumbnails on and
#     off from the toolbar. A viewer has no text field to focus, and keyboard
#     shortcuts do not reach one in this harness (see scripts/e2e/README.md),
#     so this step clicks, the way a reader would.
xdotool key --clearmodifiers ctrl+p
sleep 0.6
xdotool type --delay 40 'page'
xdotool key --clearmodifiers Return
sleep 2.0
shot 20-pdf-open
[[ "$(pixels_differ 19-search-hit-open 20-pdf-open)" -gt 1000 ]] \
  || fail "Quick Open did not open the PDF"
# The viewer's toolbar sits under its title; the thumbnails button is the
# leftmost thing in it, at a fixed offset from the window's own corner.
read -r win_x win_y <<<"$(xdotool getwindowgeometry "$WIN" |
  awk '/Position/ { gsub(",", " "); print $2, $3 }')"
xdotool mousemove $((win_x + 757)) $((win_y + 86))
sleep 0.3
xdotool click 1
sleep 1.2
shot 21-pdf-thumbnails
[[ "$(pixels_differ 20-pdf-open 21-pdf-thumbnails)" -gt 2000 ]] \
  || fail "the page thumbnails did not appear"
xdotool click 1
sleep 1.2
shot 22-pdf-thumbnails-off
# Back to within a scrollbar's worth of pixels of where it started: the two
# frames are not identical to the last pixel, but the page is back.
[[ "$(pixels_differ 20-pdf-open 22-pdf-thumbnails-off)" -lt 1500 ]] \
  || fail "hiding the thumbnails did not put the page back"
pass "PDF page thumbnails toggle from the toolbar"

# 13. Zoom the page in and drag it: once it is bigger than the pane, a drag
#     has to move it, the way the diagram window already moves.
xdotool mousemove 640 500
xdotool keydown ctrl
for _ in 1 2 3 4; do xdotool click 4; done
xdotool keyup ctrl
sleep 0.8
shot 23-pdf-zoomed
xdotool mousedown 1
for y in 450 400 350 300 250; do xdotool mousemove 640 "$y"; sleep 0.12; done
xdotool mouseup 1
sleep 0.8
shot 24-pdf-dragged
[[ "$(pixels_differ 23-pdf-zoomed 24-pdf-dragged)" -gt 50000 ]] \
  || fail "dragging a zoomed-in PDF page did not move it"
pass "dragging a zoomed-in page moves it"

# 14. The image viewer: a bare wheel does not zoom it, Command or Control
#     plus the wheel does — the same gesture as the editor, the preview, the
#     diagram and the PDF viewer. The image opens at a fit, so a bare wheel
#     has nothing to scroll and has to leave the frame alone. It is opened by
#     clicking the Explorer: keyboard shortcuts do not reach a focused viewer
#     (see scripts/e2e/README.md), and the PDF viewer was just closed over.
xdotool mousemove $((win_x + 83)) $((win_y + 285))
sleep 0.3
xdotool click 1
sleep 1.5
shot 25-image-open
[[ "$(pixels_differ 24-pdf-dragged 25-image-open)" -gt 1000 ]] \
  || fail "clicking the Explorer did not open the image"
xdotool mousemove $((win_x + 640)) $((win_y + 420))
sleep 0.3
xdotool click 4
sleep 0.8
shot 26-image-wheel
[[ "$(pixels_differ 25-image-open 26-image-wheel)" -lt 500 ]] \
  || fail "a bare wheel zoomed the image viewer"
pass "a bare wheel does not zoom the image viewer"
xdotool keydown ctrl
xdotool click 4
xdotool click 4
xdotool keyup ctrl
sleep 0.8
shot 27-image-zoomed
[[ "$(pixels_differ 26-image-wheel 27-image-zoomed)" -gt 1000 ]] \
  || fail "Ctrl+wheel did not zoom the image viewer"
pass "Ctrl+wheel zooms the image viewer"

# 15. Command or Control plus the wheel zooms the editor itself, the way it
#     zooms a viewer. The editor's own wheel handling used to scroll and take
#     the event before the pane underneath saw it, so this gesture only ever
#     scrolled. The zoom is the interface font size, so the assertion reads it
#     back from the saved settings. Opened by clicking the Explorer: keyboard
#     shortcuts do not reach a focused viewer.
xdotool mousemove $((win_x + 83)) $((win_y + 106))
sleep 0.3
xdotool click 1
sleep 1.5
shot 28-editor-open
[[ "$(pixels_differ 27-image-zoomed 28-editor-open)" -gt 1000 ]] \
  || fail "clicking the Explorer did not open a text file"
before="$(font_size)"
[[ -n "$before" ]] || before=16
xdotool mousemove $((win_x + 640)) $((win_y + 420))
sleep 0.3
xdotool keydown ctrl
xdotool click 4
xdotool click 4
xdotool keyup ctrl
sleep 0.8
after="$(font_size)"
[[ "$after" != "$before" ]] \
  || fail "Ctrl+wheel over the editor did not zoom (font size stayed at ${before:-unset})"
pass "Ctrl+wheel zooms the editor"

# 16. Still alive at the end — no crash from any of the above.
kill -0 "$APP_PID" 2>/dev/null || fail "process died during the run"
pass "process alive at exit"

echo
echo "PASS — artifacts in $ART_DIR"
ls -1 "$ART_DIR"/*.png
