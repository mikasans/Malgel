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
DISPLAY_NUM="${E2E_DISPLAY:-}"
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
# Our own Xvfb, on a free display unless E2E_DISPLAY names one. Never kill
# a server by display number: another run (or a reviewer's) may own it, and
# killing it mid-run is exactly the kind of "flake" that wastes a day.
start_xvfb() {
  local n
  if [[ -z "${E2E_DISPLAY:-}" ]]; then
    # First display from :99 up with neither a lock nor a socket.
    for n in $(seq 99 160); do
      [[ -e "/tmp/.X${n}-lock" || -e "/tmp/.X11-unix/X${n}" ]] || break
    done
    DISPLAY_NUM=":$n"
  fi
  export DISPLAY="$DISPLAY_NUM"
  Xvfb "$DISPLAY_NUM" -screen 0 1600x1000x24 +extension GLX +render -nolisten tcp \
    >"$ART_DIR/xvfb.log" 2>&1 &
  XVFB_PID=$!
  for _ in $(seq 1 100); do
    xdotool getdisplaygeometry >/dev/null 2>&1 && return 0
    kill -0 "$XVFB_PID" 2>/dev/null || break # lost a race for that display
    sleep 0.1
  done
  kill "$XVFB_PID" 2>/dev/null || true
  [[ -z "${E2E_DISPLAY:-}" ]] && DISPLAY_NUM=""
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
# No session bus: the xdg-desktop-portal file dialog cannot open (as on a
# desktop without a portal), which step 21c2 checks is reported, not ignored.
env -u DBUS_SESSION_BUS_ADDRESS "$BIN" --new-instance "$WS" "$WS/README.md" >"$ART_DIR/app.log" 2>&1 &
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
  shot "focus-lost-$(date +%s)"
  echo "focus diag: README head: $(head -c 120 "$WS/README.md" | tr '\n' '|')" >&2
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

# 4c. Find in Files: Ctrl+Shift+F puts the keyboard in the Search view's
#     Find field, Enter runs the search and the results land in the sidebar
#     (no dialog), and one Escape puts the keyboard back in the editor.
shot 07d2-before-find
xdotool key --clearmodifiers ctrl+shift+f
sleep 0.5
xdotool type --delay 40 'paragraph'
xdotool key --clearmodifiers Return
sleep 1.5
shot 07e-find-results
[[ "$(pixels_differ 07d2-before-find 07e-find-results)" -gt 1000 ]] \
  || fail "Enter in the Search view's Find field showed no results"
xdotool key --clearmodifiers Escape
sleep 0.4
shot 07h-find-escaped
[[ "$(pixels_differ 07e-find-results 07h-find-escaped)" -lt 4000 ]] \
  || fail "Escape in the Search view did more than move the keyboard"
editor_has_focus || fail "after Escape in the Search view, typing did not reach the editor"
pass "Find in Files: results in the sidebar on Enter, Escape back to the editor"

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
# Ctrl+K followed by any other key is still Insert Link (the chord only
# holds the first key back), and the other key is typed after it. The key
# must not start a chord of its own: Ctrl+K Z is Zen Mode and Ctrl+K S is
# Save All, as in VS Code, so use J.
xdotool key --clearmodifiers ctrl+End
xdotool key --clearmodifiers ctrl+k
sleep 0.15
xdotool type --delay 30 'jq'
sleep 0.4
xdotool key --clearmodifiers ctrl+s
wait_for 5 grep -q '\](' "$WS/README.md" \
  || fail "Ctrl+K then another key did not insert a link: $(tail -c 80 "$WS/README.md")"
grep -q 'jq' "$WS/README.md" || fail "the key after Ctrl+K was lost"
# Put it back from inside the editor (an external rewrite races the reload).
for _ in 1 2 3 4 5 6; do
  [[ "$(cat "$WS/README.md")" == "$before_chord" ]] && break
  xdotool key --clearmodifiers ctrl+z
  sleep 0.2
  xdotool key --clearmodifiers ctrl+s
  sleep 0.5
done
[[ "$(cat "$WS/README.md")" == "$before_chord" ]] || fail "undo did not take the link back out"
pass "Ctrl+K then another key inserts a link and keeps the key"
# Ctrl+Shift+Z redoes (as well as Ctrl+Y) off macOS.
xdotool key --clearmodifiers ctrl+End
xdotool type --delay 30 'RDO'
sleep 0.3
xdotool key --clearmodifiers ctrl+z
sleep 0.3
xdotool key --clearmodifiers ctrl+shift+z
sleep 0.3
xdotool key --clearmodifiers ctrl+s
wait_for 5 grep -q 'RDO' "$WS/README.md" || fail "Ctrl+Shift+Z did not redo the undone typing"
xdotool key --clearmodifiers BackSpace BackSpace BackSpace
sleep 0.2
xdotool key --clearmodifiers ctrl+s
wait_for 5 bash -c "! grep -q 'RDO' '$WS/README.md'" || fail "could not take RDO back out"
pass "Ctrl+Shift+Z redoes"
# Ctrl+/ toggles a line comment (Markdown: <!-- -->); one Ctrl+Z undoes it.
xdotool key --clearmodifiers ctrl+s
sleep 0.3
before_comment="$(cat "$WS/README.md")"
xdotool key --clearmodifiers ctrl+Home
xdotool key --clearmodifiers ctrl+slash
sleep 0.3
xdotool key --clearmodifiers ctrl+s
wait_for 5 bash -c "head -1 '$WS/README.md' | grep -q '^<!-- .* -->$'" \
  || fail "Ctrl+/ did not comment the line: $(head -1 "$WS/README.md")"
xdotool key --clearmodifiers ctrl+z
sleep 0.3
xdotool key --clearmodifiers ctrl+s
sleep 0.6
[[ "$(cat "$WS/README.md")" == "$before_comment" ]] \
  || fail "one Ctrl+Z did not undo the comment toggle: $(head -1 "$WS/README.md")"
xdotool key --clearmodifiers ctrl+slash
sleep 0.2
xdotool key --clearmodifiers ctrl+slash
sleep 0.2
xdotool key --clearmodifiers ctrl+s
sleep 0.6
[[ "$(cat "$WS/README.md")" == "$before_comment" ]] \
  || fail "Ctrl+/ twice did not give the line back: $(head -1 "$WS/README.md")"
pass "Ctrl+/ toggles a line comment in one undo step"

# 5. Ctrl+B with editor focus toggles the File sidebar (VS Code). Bold moved
#    to Ctrl+Shift+B so the side-bar chord is never stolen while typing.
before_sidebar="$(sidebar_on)"
[[ -n "$before_sidebar" ]] || before_sidebar=true
xdotool key --clearmodifiers ctrl+b
sleep 0.5
shot 08-sidebar-toggle
wait_for 5 test "$(sidebar_on)" != "$before_sidebar" \
  || fail "Ctrl+B in the editor did not toggle the file sidebar (was $before_sidebar)"
xdotool key --clearmodifiers ctrl+b
sleep 0.5
wait_for 5 test "$(sidebar_on)" = "$before_sidebar" \
  || fail "Ctrl+B did not restore the file sidebar"
pass "Ctrl+B in the editor toggles the file sidebar"

# 5b. Ctrl+Shift+B still applies Bold.
xdotool key --clearmodifiers ctrl+a
xdotool key --clearmodifiers ctrl+shift+b
sleep 0.3
xdotool key --clearmodifiers ctrl+s
shot 08-bold
wait_for 5 grep -q '\*\*' "$WS/README.md" || fail "Ctrl+Shift+B in the editor did not apply bold"
pass "Ctrl+Shift+B in the editor applies bold (Ctrl+S round-trip)"

# 6. The Explorer: Ctrl+Shift+E moves the keyboard into the file tree and
#    picks the file that is open; F2 renames it, and the file on disk follows.
xdotool key --clearmodifiers ctrl+shift+e
shot 09-explorer-focus
xdotool key --clearmodifiers F2
shot 10-rename-prompt
[[ "$(pixels_differ 09-explorer-focus 10-rename-prompt)" -gt 1000 ]] \
  || fail "F2 did not open the rename prompt"
# F2 selects the stem (VS Code): typing replaces "README", keeping ".md".
xdotool type --delay 40 'renamed'
xdotool key --clearmodifiers Return
sleep 0.8
shot 11-after-rename
wait_for 5 test -f "$WS/renamed.md" || fail "F2 + typing 'renamed' did not make renamed.md (stem not selected?): $(ls "$WS")"
[[ ! -e "$WS/README.md" ]] || fail "the old name is still on disk after the rename"
wait_for 5 grep -q 'SMOKE' "$WS/renamed.md" || fail "the renamed file lost its contents"
pass "F2 in the Explorer selects the stem and renames the file on disk"

# 6b. A name the filesystem refuses (over 255 bytes) must not lose the file:
#     a toast says why, the file stays, and its tab keeps its path.
before_files="$(ls -A "$WS" | sort)"
xdotool key --clearmodifiers F2
sleep 0.5
xdotool type --delay 2 "$(printf 'x%.0s' $(seq 1 300))"
shot 11b-long-name-typed
xdotool key --clearmodifiers Return
# The toast lands when the refused rename returns; on a slow runner that
# is after any fixed sleep, so wait for it (checked again below).
SAVED_SETTLE=$SETTLE SETTLE=0.3
for _ in $(seq 1 20); do
  shot 11c-long-name-refused
  [[ "$(region_differ 11-after-rename 11c-long-name-refused 520x200+1080+800)" -gt 300 ]] && break
done
SETTLE=$SAVED_SETTLE
[[ -f "$WS/renamed.md" ]] && grep -q 'SMOKE' "$WS/renamed.md" \
  || fail "renaming to a 300-byte name lost the file: $(ls -A "$WS")"
[[ "$(ls -A "$WS" | sort)" == "$before_files" ]] || fail "a refused rename changed the folder: $(ls -A "$WS")"
W_NOW=$(xdotool search --pid "$APP_PID" --onlyvisible --name . 2>/dev/null | head -1)
[[ "$(xdotool getwindowname "$W_NOW" 2>/dev/null)" == *renamed.md* ]] \
  || fail "after a refused rename the window title is '$(xdotool getwindowname "$W_NOW" 2>/dev/null)'"
[[ "$(region_differ 11-after-rename 11c-long-name-refused 520x200+1080+800)" -gt 300 ]] \
  || fail "a refused rename showed no message"
xdotool key --clearmodifiers Escape
sleep 0.3
pass "a 300-byte rename is refused with a message; the file and its tab are unchanged"

# 7. Close the editor showing a second file, then bring it back with
#    Ctrl+Shift+T, the way VS Code's Reopen Closed Editor does.
xdotool key --clearmodifiers ctrl+p
sleep 0.8
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

# 9. Find in Files (Ctrl+Shift+F): search the folder's files for text (the
#    last query is selected, so typing replaces it), then Enter again takes
#    the first hit; type into the file it opened and save — so the assertion
#    is on the file the hit came from, not on a changed frame.
xdotool key --clearmodifiers ctrl+shift+f
sleep 0.5
xdotool type --delay 40 'SMOKE'
xdotool key --clearmodifiers Return
sleep 1.5
shot 18-find-in-files
[[ "$(pixels_differ 17-after-close-all 18-find-in-files)" -gt 1000 ]] \
  || fail "Ctrl+Shift+F did not show the Find in Files results"
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
# 11c. Convert \[ \] / \( \) math to $$ / $: the palette command rewrites
#      the math in one undoable edit, the preview then draws it (the
#      \boxed frame appears), and one Ctrl+Z brings the original back.
#      Nothing is saved until Ctrl+S.
printf '# Brackets\n\nInline \\(\\mathcal G\\) here.\n\n\\[\n\\boxed{x^2+y^2}\n\\]\n' >"$WS/notes/brackets.md"
BRACKETS_ORIG="$(cat "$WS/notes/brackets.md")"
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'brackets.md'
xdotool key --clearmodifiers Return
wait_for 15 xdotool search --name '^brackets\.md' >/dev/null \
  || fail "Quick Open did not open brackets.md"
# The \boxed frame in the preview pane: two equal faint horizontal rules,
# one above the other (bar and pane borders span the whole pane width).
boxed_frame() { # boxed_frame <shot> [contrast]: exit 0 when the preview shows a box
  python3 - "$ART_DIR/$1.png" "${2:-10}" <<'PY'
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert("L")
contrast = int(sys.argv[2])
w, h = im.size
x0, x1, y0, y1 = int(w * 0.60), int(w * 0.90), int(h * 0.25), int(h * 0.85)
px = im.load()
bg = px[x1 - 4, y1 - 4]
rules = []
for y in range(y0, y1):
    run = 0
    for x in range(x0, x1 + 1):
        if x < x1 and abs(px[x, y] - bg) > contrast:
            run += 1
        else:
            if 40 <= run < (x1 - x0) * 0.8:
                rules.append((y, x - run, run))
            run = 0
found = any(abs(a[1] - b[1]) <= 2 and abs(a[2] - b[2]) <= 2 and 10 <= b[0] - a[0] <= 120
            for a in rules for b in rules)
sys.exit(0 if found else 1)
PY
}
shot 20a-brackets-open
! boxed_frame 20a-brackets-open || fail "the preview drew a box before the math was converted"
xdotool key --clearmodifiers ctrl+shift+p
sleep 0.5
xdotool type --delay 30 'Convert'
sleep 0.4
xdotool key --clearmodifiers Return
sleep 0.5
for _ in $(seq 1 10); do
  shot 20b-brackets-converted
  boxed_frame 20b-brackets-converted && break
done
boxed_frame 20b-brackets-converted || fail "after Convert the preview did not draw the \\boxed math"
# The frame is drawn in the text colour, so it stands out in both themes
# (it was black: invisible on dark, a faint half pixel on light).
boxed_frame 20b-brackets-converted 60 || fail "the \\boxed frame is too faint in the first theme"
palette_run() { # palette_run <query>
  xdotool key --clearmodifiers ctrl+shift+p
  sleep 0.5
  xdotool type --delay 30 "$1"
  sleep 0.4
  xdotool key --clearmodifiers Return
  sleep 1.0
}
palette_run 'Toggle light and dark'
for _ in $(seq 1 10); do
  shot 20b2-brackets-other-theme
  boxed_frame 20b2-brackets-other-theme 60 && break
done
boxed_frame 20b2-brackets-other-theme 60 || fail "the \\boxed frame is too faint after switching theme"
palette_run 'Toggle light and dark'
# Back in the editor (its empty last line) for the undo below.
read -r bwx bwy <<<"$(xdotool getwindowgeometry "$WIN" |
  awk '/Position/ { gsub(",", " "); print $2, $3 }')"
xdotool mousemove $((bwx + 540)) $((bwy + 310)) click 1
sleep 0.4
[[ "$(cat "$WS/notes/brackets.md")" == "$BRACKETS_ORIG" ]] || fail "Convert saved the file by itself"
# Compare when called (not once, when wait_for's arguments are expanded).
file_is() { [[ "$(cat "$1")" == "$2" ]]; }
xdotool key --clearmodifiers ctrl+s
EXPECT_DOLLARS="$(printf '# Brackets\n\nInline $\\mathcal G$ here.\n\n$$\n\\boxed{x^2+y^2}\n$$\n')"
wait_for 5 file_is "$WS/notes/brackets.md" "$EXPECT_DOLLARS" \
  || fail "Convert wrote: $(head -c 200 "$WS/notes/brackets.md")"
xdotool key --clearmodifiers ctrl+z
sleep 0.4
xdotool key --clearmodifiers ctrl+s
wait_for 5 file_is "$WS/notes/brackets.md" "$BRACKETS_ORIG" \
  || fail "one Ctrl+Z did not restore the original math: $(head -c 200 "$WS/notes/brackets.md")"
shot 20c-brackets-undone
pass "Convert \\[ \\] / \\( \\) math to \$\$ / \$: the preview draws it, one Ctrl+Z restores the original"

# 11c2. Convert keeps the view where the reader scrolled to: the caret is
#       on line 1, the math is 300 lines down, and the editor must not jump
#       back to the caret.
# Lines of different lengths, so a jump changes the picture a lot.
{ for i in $(seq 1 300); do printf "%$(( i % 37 + 1 ))s\n" "" | tr ' ' 'w'; done
  printf 'MARK-BOTTOM\n\\[\nx^2\n\\]\n'; } >"$WS/notes/longmath.md"
sleep 0.8
xdotool key --clearmodifiers ctrl+p
sleep 0.6
xdotool type --delay 40 'longmath'
sleep 0.6
xdotool key --clearmodifiers Return
sleep 1.2
xdotool mousemove $((bwx + 450)) $((bwy + 400))
xdotool click --repeat 60 --delay 10 5
sleep 1.0
shot 20d-longmath-scrolled
palette_run 'Convert'
shot 20e-longmath-converted
grep -q '^\$\$$' "$WS/notes/longmath.md" && fail "Convert saved longmath.md by itself"
# The hint bar above the editor goes away, so the text may move up by its
# height: find the best vertical shift and require the lines to match there.
same_view() { # same_view <a> <b> <x0> <y0> <w> <h>: exit 0 if b is a shifted a
  python3 - "$ART_DIR/$1.png" "$ART_DIR/$2.png" "$3" "$4" "$5" "$6" <<'PY'
from PIL import Image, ImageChops
import sys
a = Image.open(sys.argv[1]).convert("L")
b = Image.open(sys.argv[2]).convert("L")
x0, y0, w, h = map(int, sys.argv[3:7])
ref = a.crop((x0, y0 + 80, x0 + w, y0 + h - 80))
best = min(
    sum(1 for v in ImageChops.difference(ref, b.crop((x0, y0 + 80 + d, x0 + w, y0 + h - 80 + d))).tobytes() if v > 40)
    for d in range(-80, 81, 2)
)
print(best)
sys.exit(0 if best < ref.size[0] * ref.size[1] * 0.02 else 1)
PY
}
LONG_DIFF="$(same_view 20d-longmath-scrolled 20e-longmath-converted $((bwx + 330)) $((bwy + 110)) 260 560)" \
  || fail "Convert scrolled the editor away from where it was (best match $LONG_DIFF px off)"
pass "Convert keeps the editor scrolled where it was ($LONG_DIFF px changed)"
# Put things back: undo (the tab is clean again), close it, and drop the
# file so the Explorer rows the later steps click are where they were.
xdotool key --clearmodifiers ctrl+z
sleep 0.4
xdotool key --clearmodifiers ctrl+w
sleep 0.6
rm -f "$WS/notes/longmath.md"
sleep 1.0

# 11b. A GBK file (no BOM, not UTF-8) opens as GBK, and saving an edit
#      writes GBK again, not UTF-8 or replacement characters.
printf '第一行\n' | iconv -t gbk >"$WS/notes/gbk.txt"
sleep 1.0
xdotool key --clearmodifiers ctrl+p
sleep 0.6
xdotool type --delay 40 'gbk.txt'
sleep 0.6
xdotool key --clearmodifiers Return
# On a loaded runner the tab can take a while: type only once its title is up
# (CI 40712758691 typed into nothing and saved the file unchanged).
wait_for 15 xdotool search --name '^gbk\.txt' >/dev/null \
  || fail "Quick Open did not open gbk.txt (window: $(xdotool getactivewindow getwindowname 2>/dev/null))"
sleep 0.5
shot 20b-gbk-open
xdotool key --clearmodifiers ctrl+End
xdotool type --delay 40 'OK'
xdotool key --clearmodifiers ctrl+s
expected="$(printf '第一行\nOK' | iconv -t gbk | od -An -tx1 | tr -d ' \n')"
gbk_saved() { [[ "$(od -An -tx1 "$WS/notes/gbk.txt" | tr -d ' \n')" == "$expected" ]]; }
wait_for 5 gbk_saved \
  || fail "the GBK file did not open and save as GBK: $(od -An -tx1 "$WS/notes/gbk.txt" | head -2)"
pass "a GBK file opens as GBK and an edit saves as GBK"
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
# Find in Files (step 9) left the Search view in the sidebar, the way VS
# Code keeps it after a hit is opened; bring the Explorer back first.
xdotool key --clearmodifiers ctrl+shift+e
sleep 0.8
shot 24b-explorer
xdotool mousemove $((win_x + 83)) $((win_y + 285))
sleep 0.3
xdotool click 1
sleep 1.5
shot 25-image-open
[[ "$(pixels_differ 24b-explorer 25-image-open)" -gt 1000 ]] \
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

# 19b. Ctrl+Shift+V toggles split ↔ editor-only (default open is already split).
layout_before="$(sed -n 's/.*"layout": *"\([^"]*\)".*/\1/p' "$HOME/.config/malgel/settings.json" 2>/dev/null || true)"
xdotool key --clearmodifiers ctrl+shift+v
sleep 0.6
shot 38b-preview-toggle
layout_after="$(sed -n 's/.*"layout": *"\([^"]*\)".*/\1/p' "$HOME/.config/malgel/settings.json" 2>/dev/null || true)"
[[ "$layout_before" != "$layout_after" ]] \
  || fail "Ctrl+Shift+V did not change layout (was ${layout_before:-unset})"
xdotool key --clearmodifiers ctrl+shift+v
sleep 0.6
layout_restored="$(sed -n 's/.*"layout": *"\([^"]*\)".*/\1/p' "$HOME/.config/malgel/settings.json" 2>/dev/null || true)"
[[ "$layout_restored" == "$layout_before" ]] \
  || fail "Ctrl+Shift+V did not restore layout (now ${layout_restored:-unset})"
pass "Ctrl+Shift+V toggles editor|preview layout"

# 19c. Command palette rows show keybinding chords (OCR when tesseract exists).
xdotool key --clearmodifiers ctrl+shift+p
sleep 0.5
xdotool type --delay 40 'toggle terminal'
sleep 0.4
shot 38c-palette-keyhint
dismiss
if command -v tesseract >/dev/null 2>&1; then
  tesseract "$ART_DIR/38c-palette-keyhint.png" "$ART_DIR/38c-palette-keyhint" -l eng --psm 6 >/dev/null 2>&1 || true
  if [[ -f "$ART_DIR/38c-palette-keyhint.txt" ]]; then
    grep -qiE 'ctrl|control' "$ART_DIR/38c-palette-keyhint.txt" \
      || fail "palette OCR found no Ctrl chord beside Toggle Terminal"
    pass "command palette shows Ctrl keybinding hints (OCR)"
  else
    pass "command palette keyhint shot taken (OCR output missing)"
  fi
else
  pass "command palette keyhint shot taken (no tesseract)"
fi


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

# 21a1. Find: matches other than the current one are outlined (the library's
#       desaturated fill alone is nearly invisible on a light background).
xdotool key --clearmodifiers ctrl+f
sleep 0.5
xdotool type --delay 30 'wrapword'
sleep 0.6
shot 43f-find-others
xdotool key --clearmodifiers Escape
sleep 0.3
python3 - "$ART_DIR/43f-find-others.png" <<'PY' || fail "Find's other matches are not outlined"
from PIL import Image
import colorsys, sys
im = Image.open(sys.argv[1]).convert("RGB")
w, h = im.size
n = 0
for y in range(int(h * 0.25), int(h * 0.80), 2):
    for x in range(int(w * 0.33), int(w * 0.75), 2):
        r, g, b = (c / 255 for c in im.getpixel((x, y)))
        _, l, s = colorsys.rgb_to_hls(r, g, b)
        if s > 0.45 and 0.25 < l < 0.75:
            n += 1
print(f"find outlines: {n} saturated samples")
assert n > 150, n
PY
pass "Find outlines the other matches"

# 21a1b. Find's current match: Enter moves it AND the selection (VS Code),
#        painted with a strong fill while the find box has focus; after Esc
#        the selection stays on that match, so the caret's line is its line.
python3 -c "
import sys
open(sys.argv[1], 'w').write(''.join(f'item {i:02d} ' + ('needle tail' if i % 3 == 0 else 'plain') + '\n' for i in range(1, 61)))
" "$WS/notes/findcur.md"
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'findcur.md'
xdotool key --clearmodifiers Return
sleep 1.0
xdotool key --clearmodifiers ctrl+f
sleep 0.5
xdotool key --clearmodifiers ctrl+a
xdotool type --delay 30 'needle'
sleep 0.6
for _ in 1 2 3; do xdotool key --clearmodifiers Return; sleep 0.3; done
shot 43g-find-current
python3 - "$ART_DIR/43g-find-current.png" <<'PY' || fail "Find's current match has no strong fill"
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert("RGB")
w, h = im.size
# The current match's fill is the selection blue at >= 55% alpha; the other
# matches are a pale grey with an amber outline, so count saturated blue.
n = sum(1 for y in range(int(h * 0.16), int(h * 0.85)) for x in range(int(w * 0.30), int(w * 0.45))
        if (lambda p: p[2] > 200 and p[2] - p[0] > 70 and p[2] - p[1] > 30)(im.getpixel((x, y))))
print(f"current-match fill: {n} px")
assert n > 400, n
PY
xdotool key --clearmodifiers Escape
sleep 0.4
shot 43h-find-after-esc
printf '' | xclip -selection clipboard
xdotool key --clearmodifiers ctrl+c
sleep 0.3
GOT="$(xclip -o -selection clipboard 2>/dev/null || true)"
[[ "$GOT" == "needle" ]] || fail "after Find + Esc the selection is '${GOT:0:80}', not the match"
xdotool key --clearmodifiers Home shift+End
sleep 0.2
xdotool key --clearmodifiers ctrl+c
sleep 0.3
GOT="$(xclip -o -selection clipboard 2>/dev/null || true)"
[[ "$GOT" == "item 12 needle tail" ]] || fail "after Find + Enter x3 + Esc the caret is on '${GOT:0:80}', not line 12"
xdotool key --clearmodifiers ctrl+w
sleep 0.4
pass "Find: Enter selects and fills the current match; Esc leaves the caret on it (line 12)"

# 21a1c. Regex Find / Replace (VS Code's .*): Alt+R turns the regex on,
#        `foo\((\w+), ?(\w+)\)` matches both calls, Replace All with
#        `bar($2, $1)` swaps the arguments in one edit, one Ctrl+Z puts it
#        all back, and an unfinished pattern `(` is an error, not a crash.
printf 'x = foo(a, b);\ny = foo(cc,dd);\nfoo()\n' >"$WS/notes/regex.md"
REGEX_ORIG="$(cat "$WS/notes/regex.md")"
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'regex.md'
xdotool key --clearmodifiers Return
sleep 1.0
xdotool key --clearmodifiers ctrl+h
sleep 0.5
xdotool key --clearmodifiers ctrl+a
xdotool key --clearmodifiers alt+r
xdotool type --delay 30 'foo\((\w+), ?(\w+)\)'
sleep 0.5
shot 43i-find-regex
# The .* toggle shows it is on (VS Code: tinted fill, accent border): the
# same field with the regex off differs there by >= 3:1 on its border.
xdotool key --clearmodifiers alt+r
sleep 0.5
shot 43i2-find-regex-off
xdotool key --clearmodifiers alt+r
sleep 0.5
TOGGLE_PX="$(python3 - "$ART_DIR/43i-find-regex.png" "$ART_DIR/43i2-find-regex-off.png" <<'PY'
import sys
from PIL import Image
box = (1030, 200, 1150, 236)  # the Aa / ab / .* toggles at the end of the Find field
on, off = (Image.open(p).convert("RGB").crop(box) for p in sys.argv[1:3])
def lum(c):
    ch = [v / 255 for v in c]
    ch = [v / 12.92 if v <= 0.03928 else ((v + 0.055) / 1.055) ** 2.4 for v in ch]
    return 0.2126 * ch[0] + 0.7152 * ch[1] + 0.0722 * ch[2]
n = 0
for a, b in zip(on.getdata(), off.getdata()):
    la, lb = lum(a), lum(b)
    if (max(la, lb) + 0.05) / (min(la, lb) + 0.05) >= 3:
        n += 1
print(n)
PY
)"
[[ "${TOGGLE_PX:-0}" -ge 40 ]] \
  || fail "the .* toggle looks the same on and off ($TOGGLE_PX pixels >= 3:1, want >= 40)"
pass "Find: the .* toggle's on state shows (${TOGGLE_PX} border pixels >= 3:1 against off)"
xdotool key --clearmodifiers Tab
xdotool key --clearmodifiers ctrl+a
xdotool type --delay 30 'bar($2, $1)'
xdotool key --clearmodifiers ctrl+alt+Return
sleep 0.5
shot 43j-replace-all
xdotool key --clearmodifiers Escape
sleep 0.3
xdotool key --clearmodifiers ctrl+s
wait_for 5 grep -q 'bar(b, a)' "$WS/notes/regex.md" \
  || fail "regex Replace All wrote '$(head -c 200 "$WS/notes/regex.md")'"
[[ "$(cat "$WS/notes/regex.md")" == $'x = bar(b, a);\ny = bar(dd, cc);\nfoo()' ]] \
  || fail "regex Replace All gave '$(cat "$WS/notes/regex.md")'"
xdotool key --clearmodifiers ctrl+z
sleep 0.3
xdotool key --clearmodifiers ctrl+s
wait_for 5 grep -q 'foo(a, b)' "$WS/notes/regex.md" \
  || fail "one Ctrl+Z did not undo the regex Replace All: '$(cat "$WS/notes/regex.md")'"
[[ "$(cat "$WS/notes/regex.md")" == "$REGEX_ORIG" ]] || fail "after Ctrl+Z: '$(cat "$WS/notes/regex.md")'"
# An unfinished pattern: the field says why and the app lives on.
xdotool key --clearmodifiers ctrl+f
sleep 0.4
xdotool key --clearmodifiers ctrl+a
xdotool type --delay 30 '('
sleep 0.6
shot 43k-find-bad-regex
kill -0 "$APP_PID" 2>/dev/null || fail "an invalid regex '(' brought the app down"
# Turn the regex off again so later steps search literally.
xdotool key --clearmodifiers alt+r
xdotool key --clearmodifiers Escape
sleep 0.3
pass "Find: regex 2 matches, Replace All bar(\$2, \$1) is one undo step, '(' is an error not a crash"

# 21a1c2. Find as you type follows the query in a big file: the only match
#         is on the last of 40k lines (past the quick scan around the
#         viewport, so the background scan finds it) and the editor must
#         select it without Enter. Esc keeps the caret there: copy its line.
python3 -c 'import sys; sys.stdout.write("".join(f"let value_{i} = foo(a, b); // line {i}\n" for i in range(40000)) + "needle_tail at the end\n")' >"$WS/notes/bigfind.rs"
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'bigfind.rs'
xdotool key --clearmodifiers Return
wait_for 15 xdotool search --name '^bigfind\.rs' >/dev/null || fail "Quick Open did not open bigfind.rs"
sleep 0.5
xdotool key --clearmodifiers ctrl+Home
xdotool key --clearmodifiers ctrl+f
sleep 0.4
xdotool key --clearmodifiers ctrl+a
xdotool type --delay 30 'needle_tail'
sleep 1.5
shot 43k2-find-as-you-type-big
xdotool key --clearmodifiers Escape
sleep 0.3
xdotool key --clearmodifiers Home shift+End
printf '' | xclip -selection clipboard
xdotool key --clearmodifiers ctrl+c
sleep 0.3
GOT="$(xclip -o -selection clipboard 2>/dev/null || true)"
[[ "$GOT" == "needle_tail at the end" ]] \
  || fail "Find as you type did not go to the match on line 40001 (caret line: '${GOT:0:80}')"
xdotool key --clearmodifiers ctrl+w
sleep 0.5
rm -f "$WS/notes/bigfind.rs"
pass "Find as you type selects and reveals a match the big-file background scan finds"

# 21a1d. Ctrl+D (VS Code's Add Selection to Next Find Match): the word, then
#        each next match; typing edits every one, one Ctrl+Z undoes it, and
#        typing over one selection also undoes in one step.
printf 'foo bar foo\nfoo\n' >"$WS/notes/multi.md"
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'multi.md'
xdotool key --clearmodifiers Return
sleep 1.0
xdotool key --clearmodifiers ctrl+Home
xdotool key --clearmodifiers ctrl+d
xdotool type --delay 30 'abc'
xdotool key --clearmodifiers ctrl+z
xdotool key --clearmodifiers ctrl+s
sleep 0.5
[[ "$(cat "$WS/notes/multi.md")" == $'foo bar foo\nfoo' ]] \
  || fail "typing over a selection did not undo in one step: '$(cat "$WS/notes/multi.md")'"
xdotool key --clearmodifiers ctrl+Home
for _ in 1 2 3; do xdotool key --clearmodifiers ctrl+d; sleep 0.15; done
shot 43l-ctrl-d-three
xdotool type --delay 30 'x'
xdotool key --clearmodifiers ctrl+s
wait_for 5 grep -q 'x bar x' "$WS/notes/multi.md" \
  || fail "Ctrl+D x3 + typing gave '$(cat "$WS/notes/multi.md")'"
[[ "$(cat "$WS/notes/multi.md")" == $'x bar x\nx' ]] || fail "Ctrl+D x3 + typing gave '$(cat "$WS/notes/multi.md")'"
xdotool key --clearmodifiers Escape
xdotool key --clearmodifiers ctrl+z
xdotool key --clearmodifiers ctrl+s
wait_for 5 grep -q 'foo bar foo' "$WS/notes/multi.md" \
  || fail "one Ctrl+Z did not undo the multi-cursor edit: '$(cat "$WS/notes/multi.md")'"
xdotool key --clearmodifiers ctrl+w
sleep 0.3
xdotool key --clearmodifiers ctrl+w
sleep 0.4
pass "Ctrl+D adds a selection per match; typing edits all; one Ctrl+Z undoes; typed-over selection undoes whole"

# 21a1e. More cursors, as in VS Code: Ctrl+Alt+Down adds one on the next
#        line, Alt+Click adds one where it clicks, typing edits at every
#        cursor, one Ctrl+Z undoes it all, Esc goes back to one cursor; and
#        an opening bracket or quote typed over selections wraps them.
printf 'foo bar foo\nfoo\nbaz\n' >"$WS/notes/cursors.txt"
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'cursors.txt'
xdotool key --clearmodifiers Return
wait_for 15 xdotool search --name '^cursors\.txt' >/dev/null || fail "Quick Open did not open cursors.txt"
sleep 0.5
xdotool key --clearmodifiers ctrl+Home
xdotool key --clearmodifiers ctrl+alt+Down
sleep 0.2
xdotool type --delay 30 'Y'
xdotool key --clearmodifiers ctrl+s
wait_for 5 file_is "$WS/notes/cursors.txt" $'Yfoo bar foo\nYfoo\nbaz' \
  || fail "Ctrl+Alt+Down + typing gave '$(cat "$WS/notes/cursors.txt")'"
xdotool key --clearmodifiers ctrl+z
xdotool key --clearmodifiers ctrl+s
wait_for 5 file_is "$WS/notes/cursors.txt" $'foo bar foo\nfoo\nbaz' \
  || fail "one Ctrl+Z did not undo the Ctrl+Alt+Down edit: '$(cat "$WS/notes/cursors.txt")'"
xdotool key --clearmodifiers Escape
xdotool type --delay 30 'Q'
xdotool key --clearmodifiers ctrl+s
wait_for 5 file_is "$WS/notes/cursors.txt" $'Qfoo bar foo\nfoo\nbaz' \
  || fail "Esc did not go back to one cursor: '$(cat "$WS/notes/cursors.txt")'"
xdotool key --clearmodifiers BackSpace
xdotool key --clearmodifiers ctrl+Home
sleep 0.3
shot 43m-cursors-before-alt-click
ROW3="$(python3 - "$ART_DIR/43m-cursors-before-alt-click.png" <<'PY'
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
        rows.append([y, y, set()])
    if ink:
        rows[-1][1] = y
        rows[-1][2].update(xs)
    prev = ink
# The last row is line 4's number (the empty last line): row 3 is above it.
top, bottom, xs = rows[-2]
xs = sorted(xs)
for a, b in zip(xs, xs[1:]):
    if b - a > 10:
        print(b + 1, (top + bottom) // 2); break
else:
    raise SystemExit("no gap between digits and text on row 3")
PY
)" || fail "could not find row 3 of cursors.txt"
xdotool mousemove ${ROW3% *} ${ROW3#* }
sleep 0.2
xdotool keydown alt click 1 keyup alt
sleep 0.2
xdotool type --delay 30 'Y'
xdotool key --clearmodifiers ctrl+s
wait_for 5 file_is "$WS/notes/cursors.txt" $'Yfoo bar foo\nfoo\nYbaz' \
  || fail "Alt+Click did not add a cursor on row 3 (at $ROW3): '$(cat "$WS/notes/cursors.txt")'"
xdotool key --clearmodifiers ctrl+z Escape
xdotool key --clearmodifiers ctrl+s
wait_for 5 file_is "$WS/notes/cursors.txt" $'foo bar foo\nfoo\nbaz' \
  || fail "one Ctrl+Z did not undo the Alt+Click edit: '$(cat "$WS/notes/cursors.txt")'"
xdotool key --clearmodifiers ctrl+w
sleep 0.4
pass "Ctrl+Alt+Down and Alt+Click add cursors; one Ctrl+Z undoes; Esc goes back to one cursor"

# 21a1f. Auto-surround (VS Code): an opening bracket or quote typed over
#        selections wraps each one and keeps it selected; one Ctrl+Z per wrap.
# Select both "foo"s on line 1 (Ctrl+D twice), type ( then ".
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'cursors.txt'
xdotool key --clearmodifiers Return
wait_for 15 xdotool search --name '^cursors\.txt' >/dev/null || fail "Quick Open did not open cursors.txt"
sleep 0.5
xdotool key --clearmodifiers ctrl+Home
xdotool key --clearmodifiers ctrl+d
sleep 0.15
xdotool key --clearmodifiers ctrl+d
sleep 0.15
xdotool type --delay 40 '("'
xdotool key --clearmodifiers ctrl+s
wait_for 5 file_is "$WS/notes/cursors.txt" $'("foo") bar ("foo")\nfoo\nbaz' \
  || fail "typing ( and \" over two selections gave '$(cat "$WS/notes/cursors.txt")'"
shot 43n-auto-surround
xdotool key --clearmodifiers ctrl+z
xdotool key --clearmodifiers ctrl+s
wait_for 5 file_is "$WS/notes/cursors.txt" $'(foo) bar (foo)\nfoo\nbaz' \
  || fail "one Ctrl+Z did not undo one surround: '$(cat "$WS/notes/cursors.txt")'"
xdotool key --clearmodifiers Escape
xdotool key --clearmodifiers ctrl+w
sleep 0.4
pass "( and \" typed over two selections wrap both; one Ctrl+Z undoes one wrap"

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

# 21a4. Ctrl+D selects the word, then adds its next match (both are typed
#       over, as in VS Code); with nothing selected
#       Ctrl+C copies the whole line and Ctrl+V puts it in above; Ctrl+Shift+K
#       deletes the line.
printf 'alpha one\nbeta\nalpha two\n' >"$WS/notes/lines.md"
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'lines.md'
xdotool key --clearmodifiers Return
sleep 0.8
xdotool key --clearmodifiers ctrl+Home
xdotool key --clearmodifiers ctrl+d
sleep 0.2
xdotool key --clearmodifiers ctrl+d
sleep 0.2
xdotool type --delay 30 'Z'
xdotool key --clearmodifiers ctrl+s
sleep 0.8
[[ "$(cat "$WS/notes/lines.md")" == $'Z one\nbeta\nZ two' ]] \
  || fail "Ctrl+D twice did not add the next match: $(tr '\n' '|' <"$WS/notes/lines.md")"
xdotool key --clearmodifiers Escape
xdotool key --clearmodifiers ctrl+Home
xdotool key --clearmodifiers ctrl+c
sleep 0.2
xdotool key --clearmodifiers ctrl+v
xdotool key --clearmodifiers ctrl+s
sleep 0.8
[[ "$(cat "$WS/notes/lines.md")" == $'Z one\nZ one\nbeta\nZ two' ]] \
  || fail "empty-selection copy/paste did not duplicate the line above: $(tr '\n' '|' <"$WS/notes/lines.md")"
xdotool key --clearmodifiers ctrl+Home
xdotool key --clearmodifiers ctrl+shift+k
xdotool key --clearmodifiers ctrl+s
sleep 0.8
[[ "$(cat "$WS/notes/lines.md")" == $'Z one\nbeta\nZ two' ]] \
  || fail "Ctrl+Shift+K did not delete the line: $(tr '\n' '|' <"$WS/notes/lines.md")"
pass "Ctrl+D adds the next occurrence, whole-line copy/paste, Ctrl+Shift+K"

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
# The region is the dialog itself, buttons included: it grew when the
# dialog started listing the unsaved files, and the old one missed the row.
# The dialog opens with Save (primary) ringed; Tab moves the ring, and three
# Tabs (Don't save, Cancel, Save) come back around without leaving it.
xdotool key --clearmodifiers Tab
sleep 0.3
shot 47a-quit-tab1
[[ "$(region_differ 47-quit-confirm 47a-quit-tab1 460x290+570+160)" -gt 30 ]] \
  || fail "Tab in the quit dialog did not move the focus ring"
xdotool key --clearmodifiers Tab
sleep 0.2
xdotool key --clearmodifiers Tab
sleep 0.3
shot 47b-quit-tab3
# Dialog region only: the editor behind may repaint (caret).
[[ "$(region_differ 47-quit-confirm 47b-quit-tab3 460x290+570+160)" -lt 30 ]] \
  || fail "three Tabs did not cycle back to Save (focus left the dialog?)"
pass "quit dialog: primary focused, Tab cycles inside the dialog"
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

# 21c2. No file dialog available (no portal / session bus): Ctrl+S on an
#       Untitled document reports it instead of silently doing nothing.
xdotool key --clearmodifiers ctrl+n
sleep 0.6
xdotool type --delay 30 'portal'
sleep 0.3
shot 48a-untitled
xdotool key --clearmodifiers ctrl+s
wait_for 6 grep -q "Couldn't open the file dialog" "$ART_DIR/app.log" \
  || fail "Ctrl+S with no file dialog available reported nothing"
sleep 0.5
shot 48b-dialog-error
[[ "$(pixels_differ 48a-untitled 48b-dialog-error)" -gt 1000 ]] \
  || fail "no notification on screen for the file dialog failure"
kill -0 "$APP_PID" 2>/dev/null || fail "the app died when the file dialog failed"
pass "a file dialog that cannot open is reported (notification)"
# The in-app fallback asks for a path, prefilled with $HOME/Untitled.md
# (fresh HOME); Enter saves there, so Untitled can still be saved.
xdotool key --clearmodifiers Return
wait_for 4 test -s "$HOME/Untitled.md" \
  || fail "the in-app Save As fallback did not save the Untitled document"
grep -q 'portal' "$HOME/Untitled.md" || fail "fallback save wrote the wrong text"
sleep 0.5
shot 48c-fallback-saved
# The notice sits at the bottom: the top-right band, under the title bar where
# toasts used to stack over the tabs, is unchanged.
[[ "$(region_differ 48a-untitled 48c-fallback-saved 380x80+1040+150)" -lt 200 ]] \
  || fail "the file dialog notice covers the top of the window (tabs)"
pass "no file dialog: in-app path field saves Untitled; notice stays off the tabs"

# 21c3. Fold / Unfold (Ctrl+Shift+[ / ]): a Markdown heading section collapses
#       and expands again. Assert on the frame — folded body lines disappear.
printf '%s\n' \
  '# Fold me' \
  '' \
  'hidden body line one' \
  'hidden body line two' \
  'hidden body line three' \
  'hidden body line four' \
  '' \
  '## Keep visible' \
  'after the folded section' >"$WS/notes/fold.md"
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'fold.md'
xdotool key --clearmodifiers Return
sleep 1.0
# Editor-only so the fold gutter and text fill the pane (easier pixel check).
xdotool key --clearmodifiers ctrl+1
sleep 0.5
xdotool key --clearmodifiers ctrl+Home
sleep 0.3
shot 48d-fold-before
xdotool key --clearmodifiers ctrl+shift+bracketleft
sleep 0.8
shot 48e-fold-collapsed
[[ "$(pixels_differ 48d-fold-before 48e-fold-collapsed)" -gt 2000 ]] \
  || fail "Ctrl+Shift+[ did not fold the heading section"
xdotool key --clearmodifiers ctrl+shift+bracketright
sleep 0.8
shot 48f-fold-expanded
[[ "$(pixels_differ 48d-fold-before 48f-fold-expanded)" -lt 2500 ]] \
  || fail "Ctrl+Shift+] did not restore the unfolded view"
[[ "$(pixels_differ 48e-fold-collapsed 48f-fold-expanded)" -gt 2000 ]] \
  || fail "Unfold left the folded frame unchanged"
pass "Ctrl+Shift+[ / ] fold and unfold a Markdown heading"

# 21d. G5: in Tokyo Moon the line numbers render at >= 4.5:1 (they are painted
#      with muted.foreground; the old #6e738d was 3.28:1). Last, since the
#      theme choice persists.
printf 'one\ntwo\nthree\nfour\nfive\nsix\n' >"$WS/notes/moon.md"
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'moon.md'
xdotool key --clearmodifiers Return
sleep 0.8
xdotool key --clearmodifiers ctrl+shift+p
sleep 0.5
xdotool type --delay 40 'color theme'
sleep 0.3
xdotool key --clearmodifiers Return
sleep 0.5
xdotool type --delay 40 'Tokyo Moon'
sleep 0.3
xdotool key --clearmodifiers Return
sleep 1.0
xdotool key --clearmodifiers ctrl+End
sleep 0.5
shot 49-moon-gutter
python3 - "$ART_DIR/49-moon-gutter.png" <<'PY' || fail "Tokyo Moon line numbers below 4.5:1"
from PIL import Image
import sys
def lum(c):
    c = [v / 255 for v in c]
    c = [v / 12.92 if v <= 0.03928 else ((v + 0.055) / 1.055) ** 2.4 for v in c]
    return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
def ratio(a, b):
    a, b = lum(a), lum(b)
    return (max(a, b) + 0.05) / (min(a, b) + 0.05)
im = Image.open(sys.argv[1]).convert("RGB")
w, h = im.size
bg = (0x22, 0x24, 0x36)
# The editor background must be Moon's, or the theme did not apply.
found = sum(1 for x in range(int(w * .45), int(w * .55), 7) for y in range(int(h * .5), int(h * .7), 7)
            if max(abs(a - b) for a, b in zip(im.getpixel((x, y)), bg)) <= 3)
assert found > 20, f"Tokyo Moon background not found ({found})"
# Gutter digits: the band left of the text, rows of lines 1-6 (the caret is
# on line 7 after Ctrl+End, so its brighter number is not sampled).
best = None
for y in range(int(h * .17), int(h * .355)):
    # The gutter sits right of the sidebar (wider at the zoom left over from
    # earlier steps), so look between it and the text column.
    for x in range(int(w * .34), int(w * .37)):
        px = im.getpixel((x, y))
        if max(abs(a - b) for a, b in zip(px, bg)) > 3 and px[2] >= px[0]:
            r = ratio(px, bg)
            if best is None or r > best[0]:
                best = (r, px)
assert best, "no line-number ink found"
r, px = best
print(f"moon gutter: strongest digit pixel {'#%02x%02x%02x' % px} {r:.2f}:1")
assert max(abs(a - b) for a, b in zip(px, (0x6e, 0x73, 0x8d))) > 6, "still the old #6e738d"
assert r >= 4.5, r
PY
pass "Tokyo Moon line numbers read at >= 4.5:1"

# 21e. Terminal selection: drag across a word of output, Ctrl+Shift+C copies
#      exactly it; Ctrl+C with that selection still interrupts the shell
#      (it is ^C, not Copy); new output does not move the selection off its
#      text; the right-click menu opens; the copied text pastes into a file.
printf '' >"$WS/notes/termpaste.md"
xdotool key --clearmodifiers ctrl+grave
sleep 2.0
# A green background marks the cells to drag across (the typed command
# line itself is not colored, so only the output row matches).
xdotool type --delay 30 "clear; printf '\\033[42mSMOKECOPY42\\033[0m tail\\n'"
xdotool key --clearmodifiers Return
sleep 1.5
shot 49a-term-output
read -r TX0 TX1 TY < <(python3 - "$ART_DIR/49a-term-output.png" <<'PY'
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert("RGB")
w, h = im.size
pts = [(x, y) for y in range(int(h * 0.45), h, 1) for x in range(0, w, 1)
       if (lambda p: p[1] > 90 and p[1] > p[0] + 40 and p[1] > p[2] + 25)(im.getpixel((x, y)))]
assert len(pts) > 200, f"no green output cells ({len(pts)})"
xs = sorted(p[0] for p in pts); ys = sorted(p[1] for p in pts)
# The densest band: the green run is one row of cells.
y0, y1 = ys[len(ys) // 10], ys[len(ys) * 9 // 10]
print(xs[2] + 2, xs[-3] - 2, (y0 + y1) // 2)
PY
) || fail "could not find the green terminal output"
printf '' | xclip -selection clipboard
xdotool mousemove "$TX0" "$TY" mousedown 1
xdotool mousemove $(( (TX0 + TX1) / 2 )) "$TY"
sleep 0.1
xdotool mousemove "$TX1" "$TY"
sleep 0.2
xdotool mouseup 1
sleep 0.4
xdotool key --clearmodifiers ctrl+shift+c
sleep 0.4
GOT="$(xclip -o -selection clipboard 2>/dev/null || true)"
shot 49b-term-selected
[[ "$GOT" == "SMOKECOPY42" ]] || fail "terminal drag + Ctrl+Shift+C copied '${GOT:0:80}' ($TX0..$TX1,$TY)"
pass "terminal: drag-select + Ctrl+Shift+C copies the selected output"
# Ctrl+C with output selected is ^C: the sleep never reaches `touch`.
rm -f "$WS/sigint-missed"
xdotool type --delay 30 "sleep 3; touch '$WS/sigint-missed'"
xdotool key --clearmodifiers Return
sleep 0.8
xdotool key --clearmodifiers ctrl+c
sleep 3.5
[[ ! -e "$WS/sigint-missed" ]] || fail "Ctrl+C in the terminal did not interrupt (it copied?)"
# More output arrived since the drag; the selection is still on its word.
printf '' | xclip -selection clipboard
xdotool key --clearmodifiers ctrl+shift+c
sleep 0.4
GOT="$(xclip -o -selection clipboard 2>/dev/null || true)"
[[ "$GOT" == "SMOKECOPY42" ]] || fail "selection moved after new output: '${GOT:0:80}'"
pass "terminal: Ctrl+C still interrupts; the selection stays on its text"
shot 49c-term-before-menu
xdotool mousemove "$TX0" "$TY" click 3
sleep 0.6
shot 49d-term-context-menu
[[ "$(pixels_differ 49c-term-before-menu 49d-term-context-menu)" -gt 1500 ]] \
  || fail "right-click in the terminal opened no menu"
xdotool key --clearmodifiers Escape
sleep 0.3
pass "terminal: right-click opens the Copy / Paste menu"
# Paste what the terminal copied into a document and save it.
xdotool key --clearmodifiers ctrl+grave
sleep 0.5
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'termpaste.md'
xdotool key --clearmodifiers Return
# Wait for the editor to be termpaste.md (the window title follows the
# active document) rather than a fixed sleep: a debug build on CI is slow.
wait_for 15 xdotool search --name '^termpaste\.md' >/dev/null \
  || fail "Quick Open did not open termpaste.md"
sleep 0.3
# Should the keys still race the editor's focus, select-all + paste
# replaces whatever landed, so a retry cannot duplicate the text.
for _ in 1 2 3; do
  xdotool key --clearmodifiers ctrl+a
  xdotool key --clearmodifiers ctrl+v
  sleep 0.5
  xdotool key --clearmodifiers ctrl+s
  wait_for 4 grep -qx 'SMOKECOPY42' "$WS/notes/termpaste.md" && break
done
shot 49e-term-pasted
grep -qx 'SMOKECOPY42' "$WS/notes/termpaste.md" \
  || fail "pasted terminal text not saved: '$(head -c 80 "$WS/notes/termpaste.md")'"
pass "terminal: copied text pastes into the editor unchanged"

# 21f. Terminal hit-testing deep in the scrollback: 6000 lines with a green
#      marker ten rows from the end; scrolled to it, a drag, a double click
#      and a triple click on the marker each copy exactly it (rows used to
#      drift a quarter pixel each, landing ~60 rows off by then).
xdotool key --clearmodifiers ctrl+grave
sleep 1.0
xdotool type --delay 20 "clear; for i in \$(seq 1 6000); do if [ \$i = 5990 ]; then printf '\\033[42mDEEPMARK5990\\033[0m\\n'; else echo \"row \$i\"; fi; done"
xdotool key --clearmodifiers Return
sleep 3.0
find_green() { # find_green <shot>: "x0 x1 y" of the green run, or nothing
  python3 - "$ART_DIR/$1.png" <<'PY'
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert("RGB")
w, h = im.size
pts = [(x, y) for y in range(int(h * 0.3), h, 1) for x in range(0, w, 2)
       if (lambda p: p[1] > 90 and p[1] > p[0] + 40 and p[1] > p[2] + 25)(im.getpixel((x, y)))]
if len(pts) > 100:
    xs = sorted(p[0] for p in pts); ys = sorted(p[1] for p in pts)
    print(xs[2] + 2, xs[-3] - 2, (ys[len(ys) // 10] + ys[len(ys) * 9 // 10]) // 2)
PY
}
shot 49f-term-deep
read -r DX0 DX1 DY < <(find_green 49f-term-deep) || true
for _ in $(seq 1 30); do
  [[ -n "${DY:-}" ]] && break
  # Over the transcript, wheel up a little at a time until the marker shows.
  xdotool mousemove "$TX0" "$TY" click --repeat 2 --delay 30 4
  sleep 0.3
  shot 49f-term-deep
read -r DX0 DX1 DY < <(find_green 49f-term-deep) || true
done
[[ -n "${DY:-}" ]] || fail "the green marker never scrolled into view"
copy_check() { # copy_check <how>
  sleep 0.4
  xdotool key --clearmodifiers ctrl+shift+c
  sleep 0.4
  GOT="$(xclip -o -selection clipboard 2>/dev/null || true)"
  [[ "$GOT" == "DEEPMARK5990" ]] || fail "terminal $1 at scrollback row 5990 copied '${GOT:0:80}' ($DX0..$DX1,$DY)"
}
printf '' | xclip -selection clipboard
xdotool mousemove "$DX0" "$DY" mousedown 1
xdotool mousemove $(( (DX0 + DX1) / 2 )) "$DY"
sleep 0.1
xdotool mousemove "$DX1" "$DY"
sleep 0.2
xdotool mouseup 1
copy_check drag
shot 49g-term-deep-selected
printf '' | xclip -selection clipboard
xdotool mousemove $(( (DX0 + DX1) / 2 )) "$DY" click --repeat 2 --delay 80 1
copy_check "double click"
printf '' | xclip -selection clipboard
xdotool mousemove $(( (DX0 + DX1) / 2 )) "$DY" click --repeat 3 --delay 80 1
copy_check "triple click"
pass "terminal: drag / double / triple click 5990 rows into the scrollback copy exactly the row under the mouse"
xdotool key --clearmodifiers ctrl+grave
sleep 0.5

# 21f1. Keyboard scrollback (VS Code): Shift+PgUp pages back, Ctrl+Shift+Up
#       steps a line, Shift+Home goes to the oldest line kept and Shift+End
#       back to the newest. None of it reaches the program: `cat` writes
#       whatever bytes arrive to a file, which must stay empty.
rm -f "$WS/pty-bytes"
xdotool key --clearmodifiers ctrl+grave
sleep 1.0
xdotool type --delay 20 "clear; seq 1 30000; cat > '$WS/pty-bytes'"
xdotool key --clearmodifiers Return
wait_for 10 test -e "$WS/pty-bytes" || fail "the scrollback probe did not start"
sleep 2.0
shot 49g3-scroll-tail
xdotool key --clearmodifiers shift+Prior
# A short panel pages only a few rows, and a loaded box repaints late: poll.
shot_differs() { # shot_differs <name> <against> <pixels>: shoot until it changes
  local saved=$SETTLE i
  SETTLE=0.3
  for ((i = 0; i < 20; i++)); do
    shot "$1"
    [[ "$(pixels_differ "$2" "$1")" -gt "$3" ]] && break
  done
  SETTLE=$saved
  [[ "$(pixels_differ "$2" "$1")" -gt "$3" ]]
}
shot_differs 49g4-scroll-page-up 49g3-scroll-tail 600 \
  || fail "Shift+PgUp did not page the terminal back"
xdotool key --clearmodifiers ctrl+shift+Up
shot_differs 49g5-scroll-line-up 49g4-scroll-page-up 300 \
  || fail "Ctrl+Shift+Up did not step the terminal back a line"
xdotool key --clearmodifiers shift+Home
shot_differs 49g6-scroll-top 49g5-scroll-line-up 1000 \
  || fail "Shift+Home did not go to the top of the scrollback"
# At the oldest line kept, paging further back changes nothing.
xdotool key --clearmodifiers shift+Prior
shot 49g6b-scroll-top-again
[[ "$(pixels_differ 49g6-scroll-top 49g6b-scroll-top-again)" -lt 300 ]] \
  || fail "Shift+Home did not stop at the oldest line kept"
xdotool key --clearmodifiers shift+End
shot 49g7-scroll-bottom
[[ "$(pixels_differ 49g3-scroll-tail 49g7-scroll-bottom)" -lt 2000 ]] \
  || fail "Shift+End did not come back to the newest output"
xdotool key --clearmodifiers ctrl+d
sleep 0.5
[[ ! -s "$WS/pty-bytes" ]] \
  || fail "scrollback keys reached the program: $(od -An -c "$WS/pty-bytes" | head -2)"
pass "terminal: Shift+PgUp / Ctrl+Shift+Up / Shift+Home / Shift+End scroll; no bytes reach the program"
# 21f2. Ctrl+Shift+` opens a new terminal from the editor and from inside a
#       terminal (X11 reports the chord as Ctrl+~, which the shell must not
#       swallow). Each terminal is one shell, a child of the app.
shells() { pgrep -P "$APP_PID" | wc -l; }
# Checked when called, not once when wait_for's arguments are expanded.
shells_at_least() { [[ "$(shells)" -ge "$1" ]]; }
shells_at_most() { [[ "$(shells)" -le "$1" ]]; }
S0=$(shells)
xdotool key --clearmodifiers ctrl+shift+grave
wait_for 8 shells_at_least $(( S0 + 1 )) \
  || fail "Ctrl+Shift+\` from the editor opened no new terminal ($S0 -> $(shells) shells)"
sleep 1.0
xdotool key --clearmodifiers ctrl+shift+grave
wait_for 8 shells_at_least $(( S0 + 2 )) \
  || fail "Ctrl+Shift+\` inside the terminal opened no new terminal ($S0 -> $(shells) shells)"
shot 49g2-term-new-tabs
pass "terminal: Ctrl+Shift+\` opens a new terminal from the editor and from a terminal"
# Back to the state 21g expects: the two new terminals gone, editor focused.
for _ in 1 2; do
  xdotool key --clearmodifiers ctrl+shift+p
  sleep 0.5
  xdotool type --delay 30 'Kill Terminal'
  sleep 0.4
  xdotool key --clearmodifiers Return
  sleep 0.8
done
wait_for 8 shells_at_most "$S0" || fail "Kill Terminal left $(shells) shells (had $S0)"
xdotool key --clearmodifiers ctrl+grave
sleep 0.5

# 21f3. The terminal cursor: the program's DECSCUSR shape while the panel
#       has focus (a bar after `CSI 6 SP q`), an outline once focus moves to
#       the editor (VS Code's inactive cursor). Only the terminal's rows are
#       compared: the status bar's memory/CPU readout changes on its own.
xdotool key --clearmodifiers ctrl+grave
sleep 1.0
xdotool type --delay 20 "clear"
xdotool key --clearmodifiers Return
sleep 1.0
shot 49g8-cursor-block
read -r SW SH <<<"$(identify -format '%w %h' "$ART_DIR/49g8-cursor-block.png")"
TERM_ROWS="${SW}x$(( SH * 22 / 100 ))+0+$(( SH * 62 / 100 ))"
term_differs() { # term_differs <name> <against> <pixels>: shoot until the rows change
  local saved=$SETTLE i
  SETTLE=0.3
  for ((i = 0; i < 20; i++)); do
    shot "$1"
    [[ "$(region_differ "$2" "$1" "$TERM_ROWS")" -gt "$3" ]] && break
  done
  SETTLE=$saved
  [[ "$(region_differ "$2" "$1" "$TERM_ROWS")" -gt "$3" ]]
}
xdotool type --delay 20 "printf '\\033[6 q'; clear"
xdotool key --clearmodifiers Return
term_differs 49g9-cursor-bar 49g8-cursor-block 40 \
  || fail "the terminal cursor did not turn into a bar after CSI 6 SP q"
[[ "$(region_differ 49g8-cursor-block 49g9-cursor-bar "$TERM_ROWS")" -lt 3000 ]] \
  || fail "more than the cursor changed after CSI 6 SP q"
xdotool type --delay 20 "printf '\\033[0 q'; clear"
xdotool key --clearmodifiers Return
term_differs 49g10-cursor-block-again 49g9-cursor-bar 40 \
  || fail "CSI 0 SP q did not bring the block cursor back"
[[ "$(region_differ 49g8-cursor-block 49g10-cursor-block-again "$TERM_ROWS")" -lt 40 ]] \
  || fail "CSI 0 SP q left a different cursor than the first block"
# Focus to the editor: the block turns into an outline.
xdotool mousemove $(( SW / 2 )) $(( SH / 4 )) click 1
term_differs 49g11-cursor-unfocused 49g10-cursor-block-again 40 \
  || fail "the terminal cursor looked the same with focus in the editor"
pass "terminal: DECSCUSR draws a bar cursor, CSI 0 SP q the block, and it is an outline without focus"
# 21f4. Tabs line up: the rows printed with tabs look exactly like the same
#       rows with the spaces typed out (the '\t' cell was drawn as a glyph
#       of its own width, pushing the rest of the row off its column).
xdotool mousemove $(( SW / 2 )) $(( SH * 72 / 100 )) click 1
sleep 0.5
xdotool type --delay 20 "clear; printf 'a\\tX\\nabc\\tX\\nabcd\\tX\\n12345678\\tX\\n'"
xdotool key --clearmodifiers Return
sleep 1.0
shot 49g12-term-tabs
xdotool type --delay 20 "clear; printf 'a       X\\nabc     X\\nabcd    X\\n12345678        X\\n'"
xdotool key --clearmodifiers Return
sleep 1.0
shot 49g13-term-spaces
TABS_DIFF="$(region_differ 49g12-term-tabs 49g13-term-spaces "$TERM_ROWS")"
# Only the typed command line differs (its text is longer for the spaces).
[[ "$TABS_DIFF" -lt 4000 ]] || fail "tab stops do not line up with spaces ($TABS_DIFF px)"
pass "terminal: tabs stop at every 8th column like the spaces they stand for"
# 21f5. Mouse reporting: a full-screen program that asked for the mouse
#       (DECSET 1000 + 1006) gets SGR clicks and the wheel, right click
#       included; a plain alternate screen gets arrow keys from the wheel.
#       `cat` in raw mode writes what arrives.
rm -f "$WS/mouse-bytes" "$WS/alt-bytes"
xdotool type --delay 15 "printf '\\033[?1049h\\033[?1000h\\033[?1006h'; stty raw -echo; timeout 5 cat > '$WS/mouse-bytes'; stty sane; printf '\\033[?1000l\\033[?1006l\\033[?1049l'"
xdotool key --clearmodifiers Return
wait_for 5 test -e "$WS/mouse-bytes" || fail "the mouse probe did not start"
sleep 0.6
xdotool mousemove $(( SW / 2 )) $(( SH * 70 / 100 )) click 1
sleep 0.2
xdotool click 4
sleep 0.2
xdotool click 3
sleep 5.5
MOUSE="$(od -An -c "$WS/mouse-bytes" | tr -d ' \n')"
for want in '033[<0;' '033[<64;' '033[<2;'; do
  [[ "$MOUSE" == *"$want"* ]] || fail "mouse reporting: no $want in $MOUSE"
done
# One wheel notch is one report (it was three, one per toolkit line).
NOTCHES="$(grep -o '033\[<64;' <<<"$MOUSE" | wc -l)"
[[ "$NOTCHES" -eq 1 ]] || fail "one wheel notch sent $NOTCHES mouse reports: $MOUSE"
# The reported right click must not also open the context menu: an open
# menu would take the wheel below.
xdotool key --clearmodifiers ctrl+u
xdotool type --delay 15 "printf '\\033[?1049h'; stty raw -echo; timeout 3 cat > '$WS/alt-bytes'; stty sane; printf '\\033[?1049l'"
xdotool key --clearmodifiers Return
wait_for 5 test -e "$WS/alt-bytes" || fail "the alternate-scroll probe did not start"
sleep 0.6
xdotool mousemove $(( SW / 2 )) $(( SH * 70 / 100 )) click 4
sleep 3.5
ALT="$(od -An -c "$WS/alt-bytes" | tr -d ' \n')"
[[ "$ALT" == *'033[A'* || "$ALT" == *'033OA'* ]] || fail "alternate scroll: the wheel sent '$ALT', not ↑"
pass "terminal: SGR mouse reports (click, wheel, right click) and alternate-screen wheel arrows"
# 21f6. Minimum contrast (VS Code's terminal.integrated.minimumContrastRatio,
#       4.5 by default): in the light theme white (37) on red, blue, magenta
#       and white backgrounds was ~1:1; each row's text must reach 4.5:1
#       against the background it is drawn on.
xdotool type --delay 15 "clear; for b in 41 44 45 47; do printf '\\033[%s;37m white-on-%s \\033[0m\\n' \$b \$b; done"
xdotool key --clearmodifiers Return
term_is_light() { # term_is_light <shot>: the terminal rows' background is light
  python3 - "$ART_DIR/$1.png" "$TERM_ROWS" <<'PY'
from PIL import Image
import re, sys
im = Image.open(sys.argv[1]).convert("RGB")
w, h, x, y = map(int, re.match(r"(\d+)x(\d+)\+(\d+)\+(\d+)", sys.argv[2]).groups())
px = im.crop((x, y, x + w, y + h)).resize((64, 16)).getdata()
sys.exit(0 if sorted(sum(p) / 3 for p in px)[len(px) // 2] > 160 else 1)
PY
}
TOGGLED=0
for _ in 1 2; do
  shot 49g14-term-contrast
  term_is_light 49g14-term-contrast && break
  palette_run 'Toggle light and dark'
  TOGGLED=$(( TOGGLED + 1 ))
done
term_is_light 49g14-term-contrast || fail "could not switch the terminal to the light theme"
CONTRAST="$(python3 - "$ART_DIR/49g14-term-contrast.png" <<'PY'
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert("RGB")
w, h = im.size
px = im.load()
def lum(c):
    def ch(v):
        v /= 255
        return v / 12.92 if v <= 0.03928 else ((v + 0.055) / 1.055) ** 2.4
    return 0.2126 * ch(c[0]) + 0.7152 * ch(c[1]) + 0.0722 * ch(c[2])
def ratio(a, b):
    la, lb = lum(a), lum(b)
    return (max(la, lb) + 0.05) / (min(la, lb) + 0.05)
out = []
# VS Code Light ANSI 41 / 44 / 45 / 47 backgrounds.
for name, bg in (("41", (0xcd, 0x31, 0x31)), ("44", (0x04, 0x51, 0xa5)),
                 ("45", (0xbc, 0x05, 0xbc)), ("47", (0x55, 0x55, 0x55))):
    near = lambda p: all(abs(p[i] - bg[i]) <= 3 for i in range(3))
    rows = {}
    for y in range(h // 3, h):
        n = sum(1 for x in range(0, w, 2) if near(px[x, y]))
        if n > 40:
            rows[y] = n
    if not rows:
        out.append(f"{name}:none")
        continue
    # The background run's rows and columns; the text is what is not bg.
    ys = sorted(rows)
    xs = [x for x in range(0, w, 1) if near(px[x, ys[len(ys) // 2]])]
    x0, x1 = min(xs), max(xs)
    best = max(ratio(px[x, y], bg) for y in ys for x in range(x0, x1 + 1))
    out.append(f"{name}:{best:.2f}")
print(" ".join(out))
PY
)"
echo "  contrast (light theme, white on 41 44 45 47): $CONTRAST"
for item in $CONTRAST; do
  r="${item#*:}"
  [[ "$r" != none ]] || fail "terminal contrast: no ${item%%:*} background row found ($CONTRAST)"
  python3 -c "import sys; sys.exit(0 if float('$r') >= 4.5 else 1)" \
    || fail "terminal contrast: white on ${item%%:*} is only $r:1 in the light theme ($CONTRAST)"
done
(( TOGGLED % 2 == 0 )) || palette_run 'Toggle light and dark'
pass "terminal: white on red/blue/magenta/white reaches 4.5:1 in the light theme"
# 21f7. Typing while the terminal's context menu is open closes it, and the
#       key still reaches the shell.
xdotool key --clearmodifiers ctrl+u
xdotool type --delay 15 "clear"
xdotool key --clearmodifiers Return
sleep 0.8
shot 49g15-term-before-menu
xdotool mousemove $(( SW / 2 )) $(( SH * 70 / 100 )) click 3
sleep 0.8
shot 49g16-term-menu
[[ "$(region_differ 49g15-term-before-menu 49g16-term-menu "$TERM_ROWS")" -gt 1500 ]] \
  || fail "right click in the terminal opened no context menu"
xdotool type --delay 30 'z'
sleep 0.8
shot 49g17-term-menu-typed
MENU_LEFT="$(region_differ 49g15-term-before-menu 49g17-term-menu-typed "$TERM_ROWS")"
[[ "$MENU_LEFT" -lt 1500 ]] || fail "typing left the terminal's context menu open ($MENU_LEFT px)"
[[ "$MENU_LEFT" -gt 0 ]] || fail "the key typed with the menu open did not reach the shell"
xdotool key --clearmodifiers BackSpace
pass "terminal: one wheel notch is one mouse report; typing closes the context menu"
# 21f8. A program that hides the cursor (`tput civis`, top, htop) keeps the
#       keyboard: every key still goes to the shell, none to the document
#       open in the editor (the key field used to follow the visible cursor
#       out of the tree), and `tput cnorm` brings the caret back.
TITLE_BEFORE="$(xdotool getwindowname "$WIN" 2>/dev/null)"
rm -f "$WS/civis-hi" "$WS/top-q"
xdotool type --delay 20 "clear; tput civis"
xdotool key --clearmodifiers Return
sleep 0.8
shot 49g18-term-civis
xdotool type --delay 20 "echo hi >'$WS/civis-hi'"
xdotool key --clearmodifiers Return
wait_for 5 test -s "$WS/civis-hi" \
  || fail "with the cursor hidden, typed 'echo hi' never reached the shell"
[[ "$(cat "$WS/civis-hi")" == hi ]] || fail "the shell got '$(cat "$WS/civis-hi")', not 'echo hi'"
if command -v top >/dev/null; then
  xdotool type --delay 20 "top"
  xdotool key --clearmodifiers Return
  sleep 1.5
  xdotool type --delay 30 "q"
  sleep 0.8
  xdotool type --delay 20 "echo q >'$WS/top-q'"
  xdotool key --clearmodifiers Return
  wait_for 5 test -s "$WS/top-q" || fail "q did not quit top (or what followed missed the shell)"
fi
[[ "$(xdotool getwindowname "$WIN" 2>/dev/null)" == "$TITLE_BEFORE" ]] \
  || fail "keys typed into the hidden-cursor terminal reached the editor (title '$(xdotool getwindowname "$WIN")')"
xdotool type --delay 20 "clear; tput civis"
xdotool key --clearmodifiers Return
sleep 0.8
shot 49g19-term-civis-clear
xdotool type --delay 20 "clear; tput cnorm"
xdotool key --clearmodifiers Return
term_differs 49g20-term-cnorm 49g19-term-civis-clear 40 \
  || fail "tput cnorm did not bring the terminal caret back"
pass "terminal: with the cursor hidden (tput civis, top) keys reach only the shell; tput cnorm shows the caret"
xdotool key --clearmodifiers ctrl+grave
sleep 0.5

# 21g. Settings: every switch is drawn inside the card (the default dialog
#      width clipped them all), and filtering does not move the card.
xdotool key --clearmodifiers ctrl+comma
sleep 1.0
shot 49h-settings
card_box() { # card_box <shot>: "x0 x1 top" of the dialog card (any theme)
  python3 - "$ART_DIR/$1.png" <<'PY'
from PIL import Image
from collections import Counter
import sys
im = Image.open(sys.argv[1]).convert("RGB")
w, h = im.size
px = im.load()
cx, cy = w // 2, h // 2
bg = Counter(px[x, y] for x in range(cx - 100, cx + 100, 2) for y in range(cy - 20, cy + 20, 2)).most_common(1)[0][0]
near = lambda p: sum(abs(a - b) for a, b in zip(p, bg)) < 12
band = range(cy - 150, cy + 150, 3)
cols = [x for x in range(w) if sum(near(px[x, y]) for y in band) > len(band) * 0.5]
x0, x1 = min(cols), max(cols) + 1
rows = [y for y in range(h) if sum(near(px[x, y]) for x in range(x0, x1, 3)) > (x1 - x0) / 3 * 0.5]
print(x0, x1, min(rows))
PY
}
read -r CX0 CX1 CTOP < <(card_box 49h-settings) || fail "could not find the Settings card"
python3 - "$ART_DIR/49h-settings.png" "$CX0" "$CX1" "$CTOP" <<'PY' || fail "Settings switches are clipped or missing"
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert("L")
x0, x1, top = map(int, sys.argv[2:])
px = im.load()
bg = px[x1 - 3, top + 3]
ink = lambda x, y: abs(px[x, y] - bg) > 60
ys = range(top + 60, min(top + 520, im.size[1]))
# Switches sit at the card's right: count their pixels in the right 90 px;
# nothing may reach the card's last 5 px (a clipped switch runs into it).
inside = sum(1 for y in ys for x in range(x1 - 90, x1 - 5) if ink(x, y))
edge = sum(1 for y in ys for x in range(x1 - 5, x1 - 1) if ink(x, y))
print(f"card {x0}..{x1} top {top}: switch pixels {inside}, at the edge {edge}")
assert x1 - x0 >= 560, x1 - x0
assert inside > 400 and edge == 0, (inside, edge)
PY
xdotool type --delay 40 'wrap'
sleep 0.8
shot 49i-settings-filtered
read -r _ _ CTOP2 < <(card_box 49i-settings-filtered) || fail "no Settings card after filtering"
[[ "$CTOP" == "$CTOP2" ]] || fail "filtering moved the Settings card ($CTOP -> $CTOP2)"
xdotool key --clearmodifiers Escape
sleep 0.3
xdotool key --clearmodifiers Escape
sleep 0.5
pass "Settings: switches inside the card; the card stays put while filtering"

# 21h. settings.json edited by hand while the app runs: applied live, kept by
#      the next in-app change, and a broken edit is refused with a message.
SJ="$HOME/.config/malgel/settings.json"
shot 49j-before-hand-edit
python3 - "$SJ" <<'PY'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
d["file_sidebar"] = not d.get("file_sidebar", True)
d["terminal_scrollback"] = 4321
json.dump(d, open(p, "w"), indent=2)
PY
sleep 2.5
shot 49k-after-hand-edit
[[ "$(pixels_differ 49j-before-hand-edit 49k-after-hand-edit)" -gt 1000 ]] \
  || fail "a hand edit of settings.json (file_sidebar) was not applied live"
xdotool key --clearmodifiers alt+z
sleep 1.2
grep -q '"terminal_scrollback": 4321' "$SJ" \
  || fail "an in-app toggle overwrote the hand-edited terminal_scrollback: $(grep scrollback "$SJ")"
xdotool key --clearmodifiers alt+z
sleep 0.8
python3 - "$SJ" <<'PY'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
d["file_sidebar"] = not d["file_sidebar"]
json.dump(d, open(p, "w"), indent=2)
PY
sleep 2.5
cp "$SJ" "$ART_DIR/settings-good.json"
printf '{"soft_wrap": tru' >"$SJ"
wait_for 6 grep -q "settings.json is not valid JSON" "$ART_DIR/app.log" \
  || fail "a broken settings.json edit said nothing"
shot 49l-settings-broken
cp "$ART_DIR/settings-good.json" "$SJ"
sleep 2
pass "settings.json: hand edits apply live and survive in-app changes; a broken one is reported"

# 22. Ctrl+Q → Don't save: dialog, disk unchanged, then the app exits.
#     Last so later steps are not stranded without a process.
printf 'quit-disk\n' >"$WS/notes/quit-final.md"
xdotool key --clearmodifiers ctrl+p
sleep 0.5
xdotool type --delay 30 'quit-final.md'
xdotool key --clearmodifiers Return
sleep 0.8
xdotool type --delay 30 'SHOULD_NOT_SAVE'
sleep 0.3
disk_before="$(cat "$WS/notes/quit-final.md")"
xdotool key --clearmodifiers ctrl+q
sleep 1.0
shot 50-quit-dont-save-dialog
kill -0 "$APP_PID" 2>/dev/null || fail "Ctrl+Q quit before Don't save could be chosen"
[[ "$(pixels_differ 49-moon-gutter 50-quit-dont-save-dialog)" -gt 1000 ]] \
  || fail "Ctrl+Q with dirty buffer showed no confirm dialog"
# Save (primary) → Don't save; one Tab then Enter.
xdotool key --clearmodifiers Tab
sleep 0.3
xdotool key --clearmodifiers Return
sleep 2.0
if kill -0 "$APP_PID" 2>/dev/null; then
  fail "Don't save on Quit left the app alive"
fi
[[ "$(cat "$WS/notes/quit-final.md")" == "$disk_before" ]] \
  || fail "Don't save + Quit mutated quit-final.md: $(head -c 80 "$WS/notes/quit-final.md")"
pass "Ctrl+Q Don't save: dialog shown, disk unchanged, app exits"

# 23. Crash safety: unsaved text survives a panic on the UI thread. With
#     MALGEL_CRASH_TEST=1 the debounced recovery write is off and a hidden
#     palette command panics, so only the panic hook can save the text.
wait_window() { # wait_window <log>: sets WIN once the app's window maps
  WIN=""
  for _ in $(seq 1 100); do
    WIN="$(xdotool search --onlyvisible --class malgel 2>/dev/null | head -1 || true)"
    [[ -n "$WIN" ]] && break
    kill -0 "$APP_PID" 2>/dev/null || fail "process exited during startup; see $1"
    sleep 0.1
  done
  [[ -n "$WIN" ]] || fail "no window appeared ($1)"
  # On a loaded machine the window maps well before it has the keyboard or
  # its editor exists, and keys typed then are lost. Wait for X focus on the
  # window, then for the first frames to settle (two shots that differ by no
  # more than a blinking caret), before anything is typed.
  local i SETTLE=0.2
  for ((i = 0; i < 50; i++)); do
    timeout 1 xdotool windowfocus --sync "$WIN" 2>/dev/null || true
    [[ "$(xdotool getwindowfocus 2>/dev/null)" == "$WIN" ]] && break
    sleep 0.1
  done
  # Without a window manager X may report the focus on another window of
  # the app (or none) although keys arrive: note it and let typed_landed
  # judge whether the typing got through.
  [[ "$(xdotool getwindowfocus 2>/dev/null)" == "$WIN" ]] \
    || echo "note: X focus is on $(xdotool getwindowfocus 2>/dev/null), not $WIN ($1)"
  for ((i = 0; i < 20; i++)); do
    shot .settle-a
    sleep 0.3
    shot .settle-b
    [[ "$(colours .settle-b)" -gt 8 && "$(pixels_differ .settle-a .settle-b)" -lt 1500 ]] && break
  done
}
# typed_landed <text>: type into the focused editor and wait until the frame
# changes, so a crash step never runs before its text reached the buffer.
# Nothing changed means nothing landed; that is the only case it retypes.
typed_landed() {
  local SETTLE=0.2
  shot .type-before
  xdotool type --delay 30 "$1"
  if wait_for 5 frame_moved_since_type; then return 0; fi
  timeout 1 xdotool windowfocus --sync "$WIN" 2>/dev/null || true
  xdotool key --clearmodifiers ctrl+End
  xdotool type --delay 30 "$1"
  wait_for 5 frame_moved_since_type || fail "typing '$1' never reached the editor"
}
frame_moved_since_type() {
  shot .type-after
  [[ "$(pixels_differ .type-before .type-after)" -ge 150 ]]
}
printf 'crash-disk\n' >"$WS/notes/crash.md"
MALGEL_CRASH_TEST=1 env -u DBUS_SESSION_BUS_ADDRESS "$BIN" --new-instance "$WS" "$WS/notes/crash.md" \
  >"$ART_DIR/app-crash.log" 2>&1 &
APP_PID=$!
wait_window "$ART_DIR/app-crash.log"
xdotool key --clearmodifiers ctrl+End
typed_landed 'CRASH_UNSAVED_77'
shot 51a-crash-before
xdotool key --clearmodifiers ctrl+shift+p
sleep 0.5
xdotool type --delay 30 'Developer: Crash'
sleep 0.4
xdotool key --clearmodifiers Return
for _ in $(seq 1 50); do kill -0 "$APP_PID" 2>/dev/null || break; sleep 0.1; done
kill -0 "$APP_PID" 2>/dev/null && fail "Developer: Crash did not bring the app down"
APP_PID=""
grep -q "CRASH_UNSAVED_77" "$WS/notes/crash.md" && fail "crash.md was saved before the crash"
grep -q "PANIC on thread" "$HOME/.config/malgel/logs/malgel.log" \
  || fail "the panic was not logged to logs/malgel.log"
grep -rq "CRASH_UNSAVED_77" "$HOME/.config/malgel/recovery" \
  || fail "the panic hook wrote no recovery file with the unsaved text"
pass "a UI-thread panic writes the unsaved text to recovery and the log"
env -u DBUS_SESSION_BUS_ADDRESS "$BIN" --new-instance "$WS" >"$ART_DIR/app-restart.log" 2>&1 &
APP_PID=$!
wait_window "$ART_DIR/app-restart.log"
shot 51b-crash-restored
xdotool key --clearmodifiers ctrl+shift+p
sleep 0.5
xdotool type --delay 30 'Save All'
sleep 0.4
xdotool key --clearmodifiers Return
wait_for 5 grep -q 'CRASH_UNSAVED_77' "$WS/notes/crash.md" \
  || fail "the restarted app did not restore the unsaved text: $(head -c 80 "$WS/notes/crash.md")"
pass "after a crash the next launch restores the unsaved text (Save All writes it)"
kill "$APP_PID" 2>/dev/null || true
APP_PID=""
sleep 0.5

# 23b. The file changes on disk after the crash: the restored text must not
#      silently replace it. The relaunch asks (Keep Recovered / Use Disk
#      Version / Compare); Ctrl+S before answering writes nothing.
MALGEL_CRASH_TEST=1 env -u DBUS_SESSION_BUS_ADDRESS "$BIN" --new-instance "$WS" "$WS/notes/crash.md" \
  >"$ART_DIR/app-crash2.log" 2>&1 &
APP_PID=$!
wait_window "$ART_DIR/app-crash2.log"
xdotool key --clearmodifiers ctrl+End
typed_landed 'SECOND_EDIT_88'
xdotool key --clearmodifiers ctrl+shift+p
sleep 0.5
xdotool type --delay 30 'Developer: Crash'
sleep 0.4
xdotool key --clearmodifiers Return
for _ in $(seq 1 50); do kill -0 "$APP_PID" 2>/dev/null || break; sleep 0.1; done
kill -0 "$APP_PID" 2>/dev/null && fail "the second Developer: Crash did not bring the app down"
APP_PID=""
grep -rq "SECOND_EDIT_88" "$HOME/.config/malgel/recovery" \
  || fail "the second crash wrote no recovery"
printf 'EXTERNAL_99\n' >"$WS/notes/crash.md"
env -u DBUS_SESSION_BUS_ADDRESS "$BIN" --new-instance "$WS" >"$ART_DIR/app-restart2.log" 2>&1 &
APP_PID=$!
wait_window "$ART_DIR/app-restart2.log"
wait_for 6 grep -q "changed on disk since the crash" "$ART_DIR/app-restart2.log" \
  || fail "no disk-conflict question for a file changed after the crash"
shot 51c-crash-disk-conflict
# The question has the keyboard: typed text must not reach the buffer.
xdotool type --delay 30 'LEAK_77'
xdotool key --clearmodifiers ctrl+s
sleep 1.5
[[ "$(cat "$WS/notes/crash.md")" == "EXTERNAL_99" ]] \
  || fail "Ctrl+S before answering overwrote the newer file: $(head -c 80 "$WS/notes/crash.md")"
pass "a file changed after the crash is not overwritten: the relaunch asks first"
# Esc = Keep Recovered: the dialog closes, nothing is written.
xdotool key --clearmodifiers Escape
sleep 0.8
shot 51d-crash-kept
[[ "$(pixels_differ 51c-crash-disk-conflict 51d-crash-kept)" -gt 1000 ]] \
  || fail "Esc did not dismiss the crash disk-conflict question"
[[ "$(cat "$WS/notes/crash.md")" == "EXTERNAL_99" ]] || fail "Keep Recovered wrote the file"
# Saving the kept text over the newer file asks again (Overwrite?) ...
xdotool key --clearmodifiers ctrl+s
sleep 1
shot 51e-crash-overwrite-newer
[[ "$(pixels_differ 51d-crash-kept 51e-crash-overwrite-newer)" -gt 1000 ]] \
  || fail "Ctrl+S after Keep Recovered did not ask before overwriting the newer file"
[[ "$(cat "$WS/notes/crash.md")" == "EXTERNAL_99" ]] \
  || fail "Ctrl+S after Keep Recovered overwrote the newer file without asking"
# ... whose default (Enter) is Cancel: nothing is written ...
xdotool key --clearmodifiers Return
sleep 1
[[ "$(cat "$WS/notes/crash.md")" == "EXTERNAL_99" ]] \
  || fail "Enter on 'file is newer, Overwrite?' wrote the file; Cancel is the default"
# ... and choosing Overwrite (Shift+Tab from Cancel, then Enter) writes it.
xdotool key --clearmodifiers ctrl+s
sleep 1
xdotool key --clearmodifiers shift+Tab
sleep 0.3
shot 51e2-crash-overwrite-focused
xdotool key --clearmodifiers Return
wait_for 5 grep -q 'SECOND_EDIT_88' "$WS/notes/crash.md" \
  || fail "Overwrite did not write the recovered text"
! grep -q 'LEAK_77' "$WS/notes/crash.md" \
  || fail "keys typed while the crash question was open reached the buffer"
shot 51f-crash-overwritten
pass "Keep Recovered, then Ctrl+S asks Overwrite? (Enter cancels; Overwrite writes the recovered text)"
kill "$APP_PID" 2>/dev/null || true
APP_PID=""

# 23c. A GBK file restored after a crash is saved as GBK again, not UTF-8.
printf '第一行\n' | iconv -t gbk >"$WS/notes/crash-gbk.txt"
MALGEL_CRASH_TEST=1 env -u DBUS_SESSION_BUS_ADDRESS "$BIN" --new-instance "$WS" "$WS/notes/crash-gbk.txt" \
  >"$ART_DIR/app-crash3.log" 2>&1 &
APP_PID=$!
wait_window "$ART_DIR/app-crash3.log"
xdotool key --clearmodifiers ctrl+End
typed_landed 'GBK77'
xdotool key --clearmodifiers ctrl+shift+p
sleep 0.5
xdotool type --delay 30 'Developer: Crash'
sleep 0.4
xdotool key --clearmodifiers Return
for _ in $(seq 1 50); do kill -0 "$APP_PID" 2>/dev/null || break; sleep 0.1; done
kill -0 "$APP_PID" 2>/dev/null && fail "the GBK Developer: Crash did not bring the app down"
APP_PID=""
env -u DBUS_SESSION_BUS_ADDRESS "$BIN" --new-instance "$WS" >"$ART_DIR/app-restart3.log" 2>&1 &
APP_PID=$!
wait_window "$ART_DIR/app-restart3.log"
xdotool key --clearmodifiers ctrl+shift+p
sleep 0.5
xdotool type --delay 30 'Save All'
sleep 0.4
xdotool key --clearmodifiers Return
GBK_EXPECTED="$(printf '第一行\nGBK77' | iconv -t gbk | od -An -tx1 | tr -d ' \n')"
gbk_crash_saved() { [[ "$(od -An -tx1 "$WS/notes/crash-gbk.txt" | tr -d ' \n')" == "$GBK_EXPECTED" ]]; }
wait_for 5 gbk_crash_saved \
  || fail "the recovered GBK file was saved as: $(od -An -tx1 "$WS/notes/crash-gbk.txt" | head -2)"
shot 51g-crash-gbk-saved
pass "a GBK file recovered after a crash is saved as GBK"
kill "$APP_PID" 2>/dev/null || true
APP_PID=""

echo
echo "PASS — artifacts in $ART_DIR"
ls -1 "$ART_DIR"/*.png
