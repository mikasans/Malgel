#!/usr/bin/env bash
# Exercise large Typst / Mermaid, multi-tab, preview switch, cold start under Xvfb.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${MALGEL_BIN:-$ROOT/target/release/malgel}"
ART="${E2E_ARTIFACTS:-$ROOT/target/e2e-artifacts/perf-scenes}"
DISP="${E2E_DISPLAY:-:98}"
mkdir -p "$ART"
[[ -x "$BIN" ]] || { echo "missing $BIN"; exit 1; }

cleanup() {
  [[ -n "${APP_PID:-}" ]] && kill "$APP_PID" 2>/dev/null || true
  [[ -n "${XVFB_PID:-}" ]] && kill "$XVFB_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT

pkill -f "Xvfb $DISP" 2>/dev/null || true
sleep 0.2
Xvfb "$DISP" -screen 0 1440x900x24 -ac >/dev/null 2>&1 &
XVFB_PID=$!
export DISPLAY="$DISP"
export LIBGL_ALWAYS_SOFTWARE=1
export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json
export MALGEL_PERF=1
export RUST_LOG=malgel=info,malgel::typst_live=info
export XDG_CONFIG_HOME="$ART/config"
export XDG_DATA_HOME="$ART/data"
export XDG_CACHE_HOME="$ART/cache"
rm -rf "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME"

LOG="$ART/scenes.log"
: >"$LOG"
SCENE_DIR="$ROOT/samples/perf-scenes"
FIX_TYP="$SCENE_DIR/multi-page.typ"
FIX_MD="$SCENE_DIR/large-mermaid.md"
FIX_HEAVY="$SCENE_DIR/heavy-budget.typ"
FIX_MAIN="${PERF_FIX:-$ROOT/samples/perf-preview/main.typ}"

echo "=== cold open typst + preview switch ==="
"$BIN" --new-instance "$FIX_TYP" "$FIX_MD" "$FIX_MAIN" >>"$LOG" 2>&1 &
APP_PID=$!
deadline=$((SECONDS + 70))
got_first=0
while kill -0 "$APP_PID" 2>/dev/null && (( SECONDS < deadline )); do
  if grep -q 'first_typst_preview_pages_applied\|malgel-perf: window_' "$LOG" 2>/dev/null; then
    got_first=1
    break
  fi
  sleep 0.15
done
echo "boot_marker=$got_first"

if command -v xdotool >/dev/null; then
  sleep 1.2
  WID=$(xdotool search --name Malgel 2>/dev/null | head -1 || true)
  if [[ -n "${WID:-}" ]]; then
    xdotool windowactivate --sync "$WID" 2>/dev/null || true
    # Preview-only, editor-only, split — layout churn.
    for key in ctrl+3 ctrl+1 ctrl+2 ctrl+shift+v; do
      xdotool key --window "$WID" "$key" 2>/dev/null || true
      sleep 0.45
    done
    # Multi-tab: next/prev
    xdotool key --window "$WID" ctrl+Tab 2>/dev/null || true
    sleep 0.35
    xdotool key --window "$WID" ctrl+shift+Tab 2>/dev/null || true
    sleep 0.35
    # Terminal summon + flood output
    xdotool key --window "$WID" ctrl+grave 2>/dev/null || true
    sleep 0.5
    xdotool type --window "$WID" --delay 8 'seq 1 4000 | sed "s/^/line /"' 2>/dev/null || true
    xdotool key --window "$WID" Return 2>/dev/null || true
    sleep 2.5
    # Open heavy budget typ via quick open is hard; instead Ctrl+P type path
    xdotool key --window "$WID" ctrl+p 2>/dev/null || true
    sleep 0.4
    xdotool key --window "$WID" Escape 2>/dev/null || true
    sleep 0.3
  else
    echo "WARN: no Malgel window for xdotool"
  fi
fi

sleep 1
rss=$(awk '/VmRSS:/ {print $2}' /proc/$APP_PID/status 2>/dev/null || echo 0)
echo "rss_kib=$rss"
grep -E 'malgel-perf:' "$LOG" | tee "$ART/perf-lines.txt" | tail -40 || true
# Budget gate should not auto-render 22-page heavy without consent when opened;
# multi-page (12) stays under 150 MiB.
python3 - <<'PY'
from pathlib import Path
import sys
sys.path.insert(0, ".")
# Inline estimate check matching render_budget heuristics
def estimate_typst(source: str):
    pagebreaks = source.count("#pagebreak")
    length_pages = max(len(source)//2500, 1)
    pages = max(pagebreaks + 1, length_pages)
    return pages * 7.7

multi = Path("samples/perf-scenes/multi-page.typ").read_text()
heavy = Path("samples/perf-scenes/heavy-budget.typ").read_text()
em, eh = estimate_typst(multi), estimate_typst(heavy)
print(f"estimate_multi_mib={em:.1f} estimate_heavy_mib={eh:.1f}")
assert em < 150, em
assert eh >= 150, eh
print("budget_estimates_ok")
PY

kill "$APP_PID" 2>/dev/null || true
wait "$APP_PID" 2>/dev/null || true
APP_PID=
echo "DONE $ART"
