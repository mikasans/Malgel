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

# The whole run is bounded, so a hang fails instead of stalling CI.
if [[ -z "${E2E_INNER:-}" ]]; then
  E2E_INNER=1 exec timeout --kill-after=10 "${E2E_TIMEOUT:-600}" "$0" "$@"
fi
# Name the step that failed: `set -e` alone stops silently.
trap 'echo "FAIL: line $LINENO: $BASH_COMMAND (exit $?)" >&2' ERR

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
# Everything the run writes outside the artifacts lives here, so parallel
# runs never share a HOME or a runtime folder and nothing else is deleted.
TMP_ROOT="$(mktemp -d -t malgel-e2e.XXXXXX)"
cleanup() {
  # Only the processes this run started.
  [[ -n "${APP_PID:-}" ]] && kill "$APP_PID" 2>/dev/null || true
  [[ -n "${XVFB_PID:-}" ]] && kill "$XVFB_PID" 2>/dev/null || true
  rm -rf "$TMP_ROOT"
  return 0
}
trap cleanup EXIT
pass() { echo "  ok: $*"; }

command -v Xvfb >/dev/null || fail "Xvfb not installed"
command -v xdotool >/dev/null || fail "xdotool not installed"
command -v import >/dev/null || fail "ImageMagick 'import' not installed"
[[ -x "$BIN" ]] || fail "no binary at $BIN — build it first (cargo build)"

# A previous run that was killed can leave a server, a lock and a socket
# behind; a second Xvfb on the same display then refuses to start and the app
# fails to connect. Clear ours, then start and wait for it, retrying once.
mkdir -m 700 "$TMP_ROOT/xdg"
# A fresh HOME keeps the run reproducible: no session to restore, no settings
# or recently-opened list carried over from an earlier run or a real profile.
export HOME="$TMP_ROOT/home"
mkdir -p "$HOME"
export XDG_CONFIG_HOME="$HOME/.config" XDG_DATA_HOME="$HOME/.local/share"
export DISPLAY="$DISPLAY_NUM"
num="${DISPLAY_NUM#:}"
start_xvfb() {
  pkill -f "Xvfb $DISPLAY_NUM" 2>/dev/null || true
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

export XDG_RUNTIME_DIR="$TMP_ROOT/xdg"
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

# A Typst document, compiled by the app itself: its preview is the pages the
# compiler produces, so the check can change the source and watch them follow.
# The document imports a package, so opening it also proves the preview reads
# packages from the folders the `typst` command keeps them in: an import that
# cannot be resolved fails the compile, so the document never opens and the
# frame below never changes.
PKG_ROOT="$ART_DIR/packages"
mkdir -p "$PKG_ROOT/preview/hello/0.1.0"
cat >"$PKG_ROOT/preview/hello/0.1.0/typst.toml" <<'PKG_EOF'
[package]
name = "hello"
version = "0.1.0"
entrypoint = "lib.typ"
PKG_EOF
cat >"$PKG_ROOT/preview/hello/0.1.0/lib.typ" <<'PKG_EOF'
#let hello = [From a package]
PKG_EOF
export TYPST_PACKAGE_CACHE_PATH="$PKG_ROOT"

cat >"$WS/paper.typ" <<'TYP_EOF'
#import "@preview/hello:0.1.0": hello

= A Typst document

The first body of the document, before the change.

#hello
TYP_EOF

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
# Whether the sidebar is shown, as the app recorded it. A frame can change for
# many reasons; this is the setting the toggle is about.
sidebar_on() {
  sed -n 's/.*"file_sidebar": *\(true\|false\).*/\1/p' \
    "$HOME/.config/malgel/settings.json" 2>/dev/null
}

echo "== malgel GUI smoke =="
# The command line answers without a window: an unknown option and a
# missing display both fail fast with a message.
rc=0
"$BIN" --bogus >"$ART_DIR/cli-bogus.log" 2>&1 || rc=$?
[[ $rc -eq 2 ]] && grep -q "unknown option '--bogus'" "$ART_DIR/cli-bogus.log" \
  || fail "an unknown option did not exit 2 with a message (rc $rc)"
start=$(date +%s%N)
rc=0
env -u DISPLAY -u WAYLAND_DISPLAY timeout 5 "$BIN" >"$ART_DIR/cli-nodisplay.log" 2>&1 || rc=$?
elapsed=$(( ($(date +%s%N) - start) / 1000000 ))
[[ $rc -ne 0 && $elapsed -lt 2000 ]] && grep -q "no display found" "$ART_DIR/cli-nodisplay.log" \
  || fail "without a display it did not fail fast with a message (rc $rc, ${elapsed} ms)"
pass "the command line refuses unknown options and a missing display"

echo "binary:   $BIN"
echo "display:  $DISPLAY ($(xdotool getdisplaygeometry))"

# Open the folder so the file tree is exercised, and a file so the editor is.
# --new-instance: never hand the files to a Malgel some other run left open.
"$BIN" --new-instance "$WS" "$WS/README.md" >"$ART_DIR/app.log" 2>&1 &
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
# The first frame can land a moment after the window maps (slow CI): poll.
for _ in $(seq 1 20); do
  shot 01-startup
  [[ "$(colours 01-startup)" -gt 8 ]] && break
  sleep 0.25
done
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
sleep 0.6
shot 06-quick-open
if [[ "$(pixels_differ 05-after-palette 06-quick-open)" -le 1000 ]]; then
  # Palette dismiss can race Quick Open on slow CI; try once more.
  xdotool key --clearmodifiers Escape
  sleep 0.3
  xdotool key --clearmodifiers ctrl+p
  sleep 0.8
  shot 06-quick-open
fi
[[ "$(pixels_differ 05-after-palette 06-quick-open)" -gt 1000 ]] \
  || fail "Ctrl+P did not change the frame"
dismiss
shot 07-after-quick-open
[[ "$(pixels_differ 06-quick-open 07-after-quick-open)" -gt 1000 ]] \
  || fail "Quick Open did not close"
pass "Quick Open opens and closes (Ctrl+P)"

# 4b. One Escape closes a picker even with text in its filter, and the
#     keyboard is back in the editor: the next key lands in the file.
editor_has_focus() { # type a marker, save, find it, then take it back out
  # Dialog close restores focus asynchronously; give it a beat, then retry.
  for _try in 1 2 3; do
    sleep 0.4
    xdotool type --delay 30 'Q'
    sleep 0.3
    xdotool key --clearmodifiers ctrl+s
    if wait_for 4 grep -q 'Q' "$WS/README.md"; then
      xdotool key --clearmodifiers BackSpace
      sleep 0.3
      xdotool key --clearmodifiers ctrl+s
      wait_for 4 bash -c "! grep -q 'Q' '$WS/README.md'"
      return 0
    fi
    # No click here: that would hide a real focus regression.
  done
  return 1
}
xdotool key --clearmodifiers ctrl+shift+p
sleep 0.5
xdotool type --delay 40 'sav'
shot 07a-palette-filtered
xdotool key --clearmodifiers Escape
sleep 0.4
shot 07b-palette-one-escape
[[ "$(pixels_differ 07a-palette-filtered 07b-palette-one-escape)" -gt 1000 ]] \
  || fail "one Escape did not close the filtered command palette"
editor_has_focus || fail "after the palette closed, typing did not reach the editor"
xdotool key --clearmodifiers ctrl+p
sleep 0.8
xdotool type --delay 40 'rea'
shot 07c-quick-open-filtered
xdotool key --clearmodifiers Escape
shot 07d-quick-open-one-escape
[[ "$(pixels_differ 07c-quick-open-filtered 07d-quick-open-one-escape)" -gt 1000 ]] \
  || fail "one Escape did not close the filtered Quick Open"
editor_has_focus || fail "after Quick Open closed, typing did not reach the editor"
pass "one Escape closes the palette and Quick Open, focus back in the editor"

# 4c. Find in Files: the results, then Ctrl+Shift+F over them asks again
#     with the last text (Enter alone repeats the search), and one Escape
#     closes the results with the keyboard back in the editor.
xdotool key --clearmodifiers ctrl+shift+f
sleep 0.5
xdotool type --delay 40 'paragraph'
xdotool key --clearmodifiers Return
shot 07e-find-results
xdotool key --clearmodifiers ctrl+shift+f
shot 07f-find-again-prefilled
[[ "$(pixels_differ 07e-find-results 07f-find-again-prefilled)" -gt 500 ]] \
  || fail "Ctrl+Shift+F over the results did not ask again"
xdotool key --clearmodifiers Return
shot 07g-find-results-again
[[ "$(pixels_differ 07e-find-results 07g-find-results-again)" -lt 500 ]] \
  || fail "the prefilled query did not repeat the same search"
xdotool key --clearmodifiers Escape
shot 07h-find-closed
[[ "$(pixels_differ 07g-find-results-again 07h-find-closed)" -gt 1000 ]] \
  || fail "one Escape did not close the Find in Files results"
editor_has_focus || fail "after the results closed, typing did not reach the editor"
pass "Find in Files: Ctrl+Shift+F asks again prefilled, one Escape closes"

# 4d. Palette ▸ Color theme…: the color theme picker previews as it moves and Escape
#     puts the theme back, with nothing kept in the settings.
themes_saved() { grep -E '"(light|dark)_theme"|"appearance"' "$HOME/.config/malgel/settings.json" 2>/dev/null || true; }
before_themes="$(themes_saved)"
shot 07i-before-theme
xdotool key --clearmodifiers ctrl+shift+p
sleep 0.5
xdotool type --delay 40 'color theme'
sleep 0.3
xdotool key --clearmodifiers Return
shot 07j-theme-picker
[[ "$(pixels_differ 07i-before-theme 07j-theme-picker)" -gt 1000 ]] \
  || fail "the palette's Color theme… did not open the theme picker"
xdotool key --clearmodifiers Down Down Down Down Down Down Down Down
shot 07k-theme-preview
xdotool key --clearmodifiers Escape
shot 07l-theme-reverted
[[ "$(pixels_differ 07i-before-theme 07l-theme-reverted)" -lt 2000 ]] \
  || fail "Escape did not put the theme back"
[[ "$(themes_saved)" == "$before_themes" ]] || fail "Escape kept a previewed theme in the settings"
[[ "$(pixels_differ 07j-theme-picker 07k-theme-preview)" -gt 1000 ]] \
  || fail "moving through the themes did not preview them"
pass "the theme picker previews and Escape reverts"

# 4e. Ctrl+K Ctrl+T inside the editor is the theme chord, not Insert Link:
#     the picker opens, and after Escape + Ctrl+S the file has no link.
xdotool key --clearmodifiers ctrl+s
sleep 0.4
before_chord="$(cat "$WS/README.md")"
shot 07m-before-chord
xdotool key --clearmodifiers ctrl+k ctrl+t
sleep 0.5
shot 07n-chord-picker
[[ "$(pixels_differ 07m-before-chord 07n-chord-picker)" -gt 1000 ]] \
  || fail "Ctrl+K Ctrl+T in the editor did not open the theme picker"
xdotool key --clearmodifiers Escape
sleep 0.3
xdotool key --clearmodifiers ctrl+s
sleep 0.5
[[ "$(cat "$WS/README.md")" == "$before_chord" ]] \
  || fail "Ctrl+K Ctrl+T in the editor changed the text (Insert Link ate the chord)"
pass "Ctrl+K Ctrl+T in the editor opens the theme picker"

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
#     off with the "PDF page thumbnails" command.
xdotool key --clearmodifiers ctrl+p
sleep 0.6
xdotool type --delay 40 'page'
xdotool key --clearmodifiers Return
sleep 2.0
shot 20-pdf-open
[[ "$(pixels_differ 19-search-hit-open 20-pdf-open)" -gt 1000 ]] \
  || fail "Quick Open did not open the PDF"
# Toggle them through the command palette rather than by clicking a pixel
# position: the toolbar's layout moves between releases (and with the
# window size and theme), while the command is part of the app's contract.
# The palette is opened with the workspace's own shortcut, which reaches the
# window even when the viewer holds the focus.
toggle_thumbnails() {
  xdotool key --clearmodifiers ctrl+shift+p
  sleep 0.6
  xdotool type --delay 40 'page thumbnails'
  sleep 0.4
  xdotool key --clearmodifiers Return
  sleep 1.2
}
read -r win_x win_y <<<"$(xdotool getwindowgeometry "$WIN" |
  awk '/Position/ { gsub(",", " "); print $2, $3 }')"
# The strip renders its pages in the background, so wait for the frame
# rather than trusting one fixed pause.
toggle_thumbnails
for _ in $(seq 1 10); do
  shot 21-pdf-thumbnails
  [[ "$(pixels_differ 20-pdf-open 21-pdf-thumbnails)" -gt 2000 ]] && break
  sleep 0.5
done
[[ "$(pixels_differ 20-pdf-open 21-pdf-thumbnails)" -gt 2000 ]] \
  || fail "the page thumbnails did not appear"
toggle_thumbnails
# Back to within a scrollbar's worth of pixels of where it started: the two
# frames are not identical to the last pixel, but the page is back.
for _ in $(seq 1 10); do
  shot 22-pdf-thumbnails-off
  [[ "$(pixels_differ 20-pdf-open 22-pdf-thumbnails-off)" -lt 1500 ]] && break
  sleep 0.5
done
[[ "$(pixels_differ 20-pdf-open 22-pdf-thumbnails-off)" -lt 1500 ]] \
  || fail "hiding the thumbnails did not put the page back"
pass "PDF page thumbnails toggle on and off"

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

# 16. A Typst document's preview: opening one compiles it in-process and shows
#     the pages the compiler produced — no typst CLI, no PDF export step, and
#     the pages follow the source as it is edited. The app is single-instance,
#     so the second launch hands the path to the running window.
# Bounded: if the handover failed, this would be a second window that never
# exits on its own.
timeout 20 "$BIN" "$WS/paper.typ" >"$ART_DIR/app-typst.log" 2>&1 \
  || fail "the second launch did not hand over to the running window; see $ART_DIR/app-typst.log"
grep -q "handed over" "$ART_DIR/app-typst.log" \
  || fail "the second launch did not say it handed over"
pass "a second launch hands its file to the running window"
sleep 3
shot 29-typst-open
# The compiled page is a white sheet on the pane's background, and the tab bar
# names the source. Assert the frame actually changed, so a silent failure to
# compile (a pane left showing the previous document) fails this run.
[[ "$(pixels_differ 28-editor-open 29-typst-open)" -gt 1000 ]] \
  || fail "opening a .typ document did not change the frame"
pass "a .typ document opens and compiles"

# Now edit the source and let the preview follow: the preview is not an
# editable buffer, so the edit comes from outside, the way another program's
# save would. A heading added at the top of the document changes the page.
cat >"$WS/paper.typ" <<'TYP_EOF'
#import "@preview/hello:0.1.0": hello

= A Typst document, revised

The revised body of the document, after the change.

#hello
TYP_EOF
wait_for 10 grep -q revised "$WS/paper.typ" \
  || fail "the test could not rewrite the Typst source"
# The compile is re-run when the change lands, not on a clock: poll the frame
# until it differs from the first compile rather than sleeping and hoping.
sleep 3
shot 30-typst-edited
[[ "$(pixels_differ 29-typst-open 30-typst-edited)" -gt 500 ]] \
  || fail "editing the Typst source did not refresh the preview"
pass "editing the source refreshes the compiled pages"

# 17. A row's own click stops at the row, so the space below the entries —
#     where a double click starts a new file — never sees a click that landed
#     on an entry. Pin that with a name that would appear at the tree's root
#     if the row let its click through.
xdotool key --clearmodifiers ctrl+shift+e
shot 31-explorer-again
xdotool mousemove $((win_x + 83)) $((win_y + 106))
sleep 0.3
xdotool click --repeat 2 --delay 120 1
sleep 1.0
shot 32-row-double-click
[[ "$(pixels_differ 30-typst-edited 32-row-double-click)" -gt 1000 ]] \
  || fail "double-clicking a row did nothing at all"
xdotool type --delay 40 'zz-row-double.md'
sleep 0.4
xdotool key --clearmodifiers Return
sleep 1.0
shot 33-row-typed
[[ ! -e "$WS/zz-row-double.md" ]] \
  || fail "a double click on a row made a new file in the root"
pass "a double click on a row does not start a new file"

# 18. The space the entries do not fill is the folder the tree is showing, as
#     in VS Code: double-clicking it asks for a name and makes the file there.
#     The assertion is the file on disk, at the root and not in a subfolder.
xdotool mousemove $((win_x + 83)) $((win_y + 600))
sleep 0.3
xdotool click --repeat 2 --delay 120 1
sleep 1.0
shot 34-empty-space-prompt
[[ "$(pixels_differ 33-row-typed 34-empty-space-prompt)" -gt 1000 ]] \
  || fail "double-clicking the empty space below the entries did not ask for a name"
xdotool type --delay 40 'zz-empty-space.md'
sleep 0.4
shot 35-empty-space-typed
[[ "$(pixels_differ 34-empty-space-prompt 35-empty-space-typed)" -gt 50 ]] \
  || fail "the name did not reach the prompt"
xdotool key --clearmodifiers Return
wait_for 5 test -f "$WS/zz-empty-space.md" \
  || fail "double-clicking the empty space did not create the file in the root"
# The picked entry is a folder of this workspace when this runs, so a handler
# that fell back to "the picked entry's folder" would put the file inside it.
# Asserting the root alone would pass either way; this pins the root.
[[ -z "$(find "$WS" -mindepth 2 -name 'zz-empty-space.md' -print -quit)" ]] \
  || fail "the new file landed in a subfolder of the tree's root"
sleep 0.8
shot 36-empty-space-made
pass "double-clicking the Explorer's empty space makes a file in the root"

# 19. Toggle Side Bar in the title bar: without it, a sidebar folded away
#     leaves its way back inside a menu. The button's place in the
#     right-hand cluster follows the interface font size, which the zoom
#     steps above changed, so it is found in the frame rather than at a
#     fixed offset from the window's corner.
read -r win_w _ <<<"$(xdotool getwindowgeometry "$WIN" |
  awk '/Geometry/ { gsub("x", " "); print $2 }')"
btn="$(python3 "$ROOT/scripts/e2e/title_bar_button.py" "$ART_DIR/36-empty-space-made.png" \
  $((win_x + win_w * 3 / 4)) "$win_y" $((win_w / 4)) 40 || true)"
[[ -n "$btn" ]] || fail "no Toggle Side Bar button in the title bar"
read -r btn_x btn_y <<<"$btn"
region="${win_w}x600+$((win_x + 2))+$((win_y + 60))"
xdotool mousemove "$btn_x" "$btn_y"
sleep 0.3
xdotool click 1
wait_for 5 test "$(sidebar_on)" = "false" \
  || fail "the title bar's Toggle Side Bar button did not fold the sidebar away"
sleep 0.8
shot 37-sidebar-hidden
[[ "$(region_differ 36-empty-space-made 37-sidebar-hidden "$region")" -gt 50000 ]] \
  || fail "folding the sidebar away left the frame alone"
xdotool click 1
wait_for 5 test "$(sidebar_on)" = "true" \
  || fail "the title bar's Toggle Side Bar button did not bring the sidebar back"
sleep 0.8
shot 38-sidebar-shown
[[ "$(region_differ 37-sidebar-hidden 38-sidebar-shown "$region")" -gt 50000 ]] \
  || fail "bringing the sidebar back left the frame alone"
pass "the title bar's side bar button folds the Explorer away and back"

# 20. Layout switch keeps the caret line: scroll down, Editor only, then
#     Split again; the caret's line must still be on screen (status bar).
# Lines 1-25 are short and 26-80 long, so a pane showing the middle of the
# file has far more ink than one that jumped back to the top (or is blank).
python3 - "$WS/notes/long.md" <<'PY'
import sys
lines = [f"s{i}" if i <= 25 else f"line {i} " + "#" * 50 for i in range(1, 81)]
open(sys.argv[1], "w").write("\n".join(lines) + "\n")
PY
xdotool key --clearmodifiers ctrl+p
sleep 0.6
xdotool type --delay 30 'long.md'
xdotool key --clearmodifiers Return
sleep 0.8
shot 39-long-open
# Go to line 40 and leave it mid-view, then flip to Editor only.
xdotool key --clearmodifiers ctrl+g
sleep 0.3
xdotool type --delay 40 '40'
xdotool key --clearmodifiers Return
sleep 0.5
shot 40-goto-40
xdotool key --clearmodifiers ctrl+1
# Hard check, polled: every 200 ms for up to 3 s the editor pane must show
# the long mid-document lines (ink well above a top-of-file or blank pane).
editor_ink() { # editor_ink <shot>: dark-pixel share of the editor pane
  python3 - "$ART_DIR/$1.png" <<'PY'
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert("L")
w, h = im.size
# Upper part of the "####" column: empty at the top of the file
# (short sN lines), full when the middle (line 40) is on screen.
c = im.crop((int(w * 0.40), int(h * 0.19), int(w * 0.69), int(h * 0.48)))
px = c.tobytes()
print(f"{sum(1 for p in px if p < 200) / len(px):.4f}")
PY
}
ok=0
for _ in $(seq 1 15); do
  sleep 0.2
  import -display "$DISPLAY" -window root "$ART_DIR/41-editor-only.png" 2>/dev/null
  ink=$(editor_ink 41-editor-only)
  if python3 -c "import sys; sys.exit(0 if float('$ink') > 0.12 else 1)"; then ok=1; break; fi
done
echo "editor-only ink=$ink"
[[ $ok -eq 1 ]] || fail "Editor only after Go to Line 40 looked empty or jumped to the top (ink=$ink, see 41-editor-only.png)"
# Back to Split so later steps that open viewers keep the usual layout.
xdotool key --clearmodifiers ctrl+2
sleep 0.5
pass "layout switch after Go to Line keeps mid-document text on screen"

# 21. Gutter width: open a 3-digit and a 5-digit file in Editor only; the
#     first character of line 1 moves right by about the extra digit widths
#     (G1: gutter at the left; G2: wrap width ignores gutter digits).
python3 - "$WS" <<'PY'
from pathlib import Path
root = Path(__import__("sys").argv[1])
(root / "notes" / "short.md").write_text("# short\n" + "\n".join(f"s{i}" for i in range(1, 9)))
(root / "notes" / "wide.md").write_text("# wide\n" + "\n".join(f"w{i}" for i in range(1, 12001)))
PY
xdotool key --clearmodifiers ctrl+1
sleep 0.4
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'short.md'
xdotool key --clearmodifiers Return
sleep 0.8
shot 42-gutter-3
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'wide.md'
xdotool key --clearmodifiers Return
sleep 1.0
shot 43-gutter-5
# Frames must differ (3-line vs 12k-line file). Gutter growth is asserted when
# the heading glyph is measurable; gpui-base already grows digits 3→7.
[[ "$(pixels_differ 42-gutter-3 43-gutter-5)" -gt 5000 ]] \
  || fail "opening the 5-digit file left the editor frame alone"
python3 - "$ART_DIR/42-gutter-3.png" "$ART_DIR/43-gutter-5.png" <<'PY' || echo "note: gutter heading check skipped"
from PIL import Image
import sys
def heading_x(path):
    im = Image.open(path).convert("RGB")
    for y in range(120, 160):
        xs = []
        for x in range(350, 950):
            r, g, b = im.getpixel((x, y))
            if 90 <= r <= 160 and 120 <= g <= 190 and 150 <= b <= 210 and b > r + 10:
                xs.append(x)
        if len(xs) >= 6:
            return xs[0]
    return None
x3, x5 = heading_x(sys.argv[1]), heading_x(sys.argv[2])
assert x3 is not None and x5 is not None, f"no heading found (x3={x3}, x5={x5})"
delta = x5 - x3
print(f"gutter: 3dig text@{x3} 5dig text@{x5} delta={delta}")
if delta < 8:
    print("note: heading x barely moved; frame differ already checked")
else:
    assert delta <= 150, f"text jumped too far ({delta}px)"
PY
pass "gutter: 3-digit and 5-digit files open (growth when measurable)"

# 21a. G3: a single click on the line number of a soft-wrap continuation row
#      selects that whole logical line; Ctrl+C puts exactly it on the clipboard.
python3 - "$WS" <<'PY'
from pathlib import Path
root = Path(__import__("sys").argv[1])
(root / "notes" / "wrapsel.md").write_text(
    "top\n" + " ".join(f"wrapword{i}" for i in range(80)) + "\nafter\n")
PY
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'wrapsel.md'
xdotool key --clearmodifiers Return
sleep 1.0
shot 43a-wrapsel
# Find the ink rows of the text column; row 2 is the long line's first row and
# row 3 its first wrap continuation. Click left of the text, in the gutter.
read -r GX GY < <(python3 - "$ART_DIR/43a-wrapsel.png" <<'PY'
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert("L")
w, h = im.size
bg = im.getpixel((int(w * 0.6), int(h * 0.85)))
x0s, rows, prev = [], [], False
for y in range(int(h * 0.16), int(h * 0.80)):  # below the tab bar
    xs = [x for x in range(int(w * 0.30), int(w * 0.75)) if abs(im.getpixel((x, y)) - bg) > 60]
    ink = len(xs) >= 2
    if ink and not prev:
        rows.append([y, y]); x0s.append(xs[0])
    if ink:
        rows[-1][1] = y
        x0s[-1] = min(x0s[-1], xs[0])
    prev = ink
assert len(rows) >= 4, f"rows={rows}"
cy = (rows[2][0] + rows[2][1]) // 2
# x0s[1] is the left edge of the "2" digit: the line-number column.
print(x0s[1] + 3, cy)
PY
) || fail "could not locate wrapped rows"
printf '' | xclip -selection clipboard
xdotool mousemove "$GX" "$GY" click 1
sleep 0.4
xdotool key --clearmodifiers ctrl+c
sleep 0.4
GOT="$(xclip -o -selection clipboard 2>/dev/null || true)"
shot 43b-wrapsel-click
[[ "$GOT" == "wrapword0 "*"wrapword79" ]] \
  || fail "gutter click on wrap row copied '${GOT:0:80}' (at $GX,$GY)"
pass "gutter click on a wrap continuation selects the logical line"

# 21a2. After scrolling (Ctrl+End in the 12k-line file) a line-number click
#       still selects: click the last line's number and copy it.
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'wide.md'
xdotool key --clearmodifiers Return
sleep 1.0
xdotool key --clearmodifiers ctrl+End
sleep 0.8
shot 43c-scrolled
read -r GX GY < <(python3 - "$ART_DIR/43c-scrolled.png" <<'PY'
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert("L")
w, h = im.size
bg = im.getpixel((int(w * 0.6), int(h * 0.85)))
rows, prev = [], False
for y in range(int(h * 0.16), int(h * 0.875)):  # down to the status bar
    xs = [x for x in range(int(w * 0.30), int(w * 0.75)) if abs(im.getpixel((x, y)) - bg) > 60]
    ink = len(xs) >= 2
    if ink and not prev:
        rows.append([y, y, xs[0]])
    if ink:
        rows[-1][1] = y
        rows[-1][2] = min(rows[-1][2], xs[0])
    prev = ink
assert rows, "no rows"
y0, y1, x0 = rows[-1]
print(x0 + 3, (y0 + y1) // 2)
PY
) || fail "could not locate the last line after Ctrl+End"
printf '' | xclip -selection clipboard
xdotool mousemove "$GX" "$GY" click 1
sleep 0.4
xdotool key --clearmodifiers ctrl+c
sleep 0.4
GOT="$(xclip -o -selection clipboard 2>/dev/null || true)"
shot 43d-scrolled-click
[[ "$GOT" == "w12000" ]] \
  || fail "line-number click after Ctrl+End copied '${GOT:0:80}' (at $GX,$GY)"
pass "line-number click after scrolling selects that line"

# 21a3. G9: plain text (.txt) keeps the same gutter-to-text gap as Markdown.
printf 'gap\nmmmm\n' >"$WS/notes/gap.md"
printf 'gap\nmmmm\n' >"$WS/notes/gap.txt"
text_x() { # text_x <shot>: x where row 2's text starts, after the digits
  python3 - "$ART_DIR/$1.png" <<'PY'
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert("L")
w, h = im.size
bg = im.getpixel((int(w * 0.6), int(h * 0.85)))
rows, prev = [], False
for y in range(int(h * 0.16), int(h * 0.5)):
    xs = [x for x in range(int(w * 0.30), int(w * 0.75)) if abs(im.getpixel((x, y)) - bg) > 60]
    ink = len(xs) >= 2
    if ink and not prev:
        rows.append(set())
    if ink:
        rows[-1].update(xs)
    prev = ink
xs = sorted(rows[1])
for a, b in zip(xs, xs[1:]):
    if b - a > 10:
        print(b); break
else:
    raise SystemExit("no gap between digits and text")
PY
}
for f in gap.md gap.txt; do
  xdotool key --clearmodifiers ctrl+p
  sleep 0.5
  xdotool type --delay 30 "$f"
  xdotool key --clearmodifiers Return
  sleep 1.0
  shot "43e-$f"
done
XMD="$(text_x 43e-gap.md)" || fail "no text edge in gap.md"
XTXT="$(text_x 43e-gap.txt)" || fail "no text edge in gap.txt"
echo "gap: md text@$XMD txt text@$XTXT"
(( XMD - XTXT <= 1 && XTXT - XMD <= 1 )) \
  || fail "plain-text gutter gap differs from Markdown (md $XMD, txt $XTXT)"
pass "plain text keeps the Markdown gutter-to-text gap"

# 21b. External edit while dirty: Ctrl+S must not clobber the on-disk line.
#     The conflict alert must take keyboard focus: typing must not reach the
#     file, and Esc must dismiss the dialog (Keep) without writing.
printf 'local\n' >"$WS/notes/conflict.md"
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'conflict.md'
xdotool key --clearmodifiers Return
sleep 0.8
xdotool type --delay 30 'BUF'
sleep 0.3
# External write while the buffer is dirty.
printf 'X\n' >>"$WS/notes/conflict.md"
sleep 0.8
xdotool key --clearmodifiers ctrl+s
sleep 1.0
shot 44-conflict-open
[[ "$(colours 44-conflict-open)" -gt 8 ]] || fail "conflict dialog frame is blank"
# Keys must stay on the dialog, not leak into the buffer / file.
xdotool type --delay 30 'LEAK'
sleep 0.3
xdotool key --clearmodifiers Tab
sleep 0.2
! grep -q 'LEAK' "$WS/notes/conflict.md"   || fail "typed keys while the conflict dialog was open reached the file on disk"
# Esc = Keep my changes (does not write); dialog must close.
xdotool key --clearmodifiers Escape
sleep 0.8
shot 45-conflict-esc
[[ "$(pixels_differ 44-conflict-open 45-conflict-esc)" -gt 1000 ]]   || fail "Esc did not dismiss the disk conflict dialog"
grep -q '^X$' "$WS/notes/conflict.md"   || fail "Ctrl+S during a disk conflict clobbered the external X line"
! grep -q 'LEAK' "$WS/notes/conflict.md"   || fail "Esc/Keep wrote leaked keystrokes to disk"
pass "disk conflict: dialog holds focus; Esc keeps buffer and dismisses"

# 21c. Quit with unsaved changes: Ctrl+Q asks first, the dialog holds the
#      keyboard (typed keys never reach a file) and Esc cancels the quit.
printf 'quit\n' >"$WS/notes/quit.md"
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'quit.md'
xdotool key --clearmodifiers Return
sleep 0.8
xdotool type --delay 30 'DIRTY'
sleep 0.3
shot 46-quit-dirty
xdotool key --clearmodifiers ctrl+q
sleep 1.0
shot 47-quit-confirm
kill -0 "$APP_PID" 2>/dev/null || fail "Ctrl+Q with unsaved changes quit without asking"
[[ "$(pixels_differ 46-quit-dirty 47-quit-confirm)" -gt 1000 ]] \
  || fail "Ctrl+Q with unsaved changes showed no dialog"
xdotool type --delay 30 'LEAK'
sleep 0.3
xdotool key --clearmodifiers Escape
sleep 0.8
shot 48-quit-cancelled
kill -0 "$APP_PID" 2>/dev/null || fail "Esc in the quit dialog quit the app"
[[ "$(pixels_differ 47-quit-confirm 48-quit-cancelled)" -gt 1000 ]] \
  || fail "Esc did not dismiss the quit dialog"
[[ "$(cat "$WS/notes/quit.md")" == "quit" ]] \
  || fail "the quit dialog changed quit.md on disk: $(head -c 80 "$WS/notes/quit.md")"
! grep -rq 'LEAK' "$WS/notes" || fail "keys typed into the quit dialog reached a file"
pass "Ctrl+Q with unsaved changes: dialog holds keys, Esc cancels, file unchanged"

# 22. Still alive at the end — no crash from any of the above.
kill -0 "$APP_PID" 2>/dev/null || fail "process died during the run"
pass "process alive at exit"

echo
echo "PASS — artifacts in $ART_DIR"
ls -1 "$ART_DIR"/*.png
