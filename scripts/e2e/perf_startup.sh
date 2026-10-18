#!/usr/bin/env bash
# Cold/warm Typst preview RSS under Xvfb (MALGEL_PERF=1).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${MALGEL_BIN:-$ROOT/target/release/malgel}"
ART="${E2E_ARTIFACTS:-$ROOT/target/e2e-artifacts/perf}"
DISP="${E2E_DISPLAY:-:97}"
FIX="${PERF_FIX:-$ROOT/samples/perf-preview/main.typ}"
mkdir -p "$ART"
[[ -x "$BIN" ]] || { echo "missing $BIN"; exit 1; }
[[ -f "$FIX" ]] || { echo "missing $FIX"; exit 1; }

rss_of() { awk '/VmRSS:/ {print $2}' "/proc/$1/status" 2>/dev/null || echo 0; }

cleanup() {
  [[ -n "${APP_PID:-}" ]] && kill "$APP_PID" 2>/dev/null || true
  [[ -n "${XVFB_PID:-}" ]] && kill "$XVFB_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT

pkill -f "Xvfb $DISP" 2>/dev/null || true
sleep 0.2
Xvfb "$DISP" -screen 0 1280x840x24 -ac >/dev/null 2>&1 &
XVFB_PID=$!
export DISPLAY="$DISP"
export LIBGL_ALWAYS_SOFTWARE=1
export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json
export MALGEL_PERF=1
export RUST_LOG=malgel=info,malgel::typst_live=debug

run_once() {
  local tag="$1"
  export XDG_CONFIG_HOME="$ART/config-$tag"
  export XDG_DATA_HOME="$ART/data-$tag"
  export XDG_CACHE_HOME="$ART/cache-$tag"
  rm -rf "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME"
  mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME"
  local LOG="$ART/$tag.log"
  : >"$LOG"
  echo "=== $tag ==="
  "$BIN" --new-instance "$FIX" >>"$LOG" 2>&1 &
  APP_PID=$!
  local deadline=$((SECONDS + 50))
  local peak=0 r=0
  while kill -0 "$APP_PID" 2>/dev/null && (( SECONDS < deadline )); do
    r=$(rss_of "$APP_PID")
    (( r > peak )) && peak=$r
    if grep -q 'first_typst_preview_pages_applied' "$LOG" 2>/dev/null; then
      sleep 0.8
      break
    fi
    sleep 0.1
  done
  local after_first
  after_first=$(rss_of "$APP_PID")
  echo "${tag}_got_first=$(grep -c first_typst_preview_pages_applied "$LOG" || true) rss_kib=$after_first peak_kib=$peak"

  if [[ "$tag" == cold ]] && command -v xdotool >/dev/null && grep -q first_typst_preview_pages_applied "$LOG"; then
    local WID
    WID=$(xdotool search --name Malgel 2>/dev/null | head -1 || true)
    if [[ -n "${WID:-}" ]]; then
      xdotool windowactivate --sync "$WID" 2>/dev/null || true
      xdotool key --window "$WID" ctrl+1 2>/dev/null || true
      sleep 0.4
      # Edit: force redraw of page 2 area; then a no-op identical recompile via space+backspace
      xdotool type --window "$WID" --delay 20 'x' 2>/dev/null || true
      sleep 1.5
      xdotool key --window "$WID" BackSpace 2>/dev/null || true
      sleep 1.5
      # Identical text again — should hit reuse_sheets if send_live fires
      xdotool type --window "$WID" --delay 20 'y' 2>/dev/null || true
      sleep 0.2
      xdotool key --window "$WID" BackSpace 2>/dev/null || true
      sleep 1.5
    fi
  fi
  local after_warm
  after_warm=$(rss_of "$APP_PID")
  echo "${tag}_rss_after_edits_kib=$after_warm"
  grep -E 'malgel-perf:' "$LOG" | tee "$ART/$tag-perf-lines.txt" >/dev/null
  # Summary lines of interest
  grep -E 'malgel-perf: (process_start|window_|open_window|typst_|sheet_|font_|document_fonts|system_font|first_|warm_|inline_)' "$LOG" || true
  kill "$APP_PID" 2>/dev/null || true
  wait "$APP_PID" 2>/dev/null || true
  APP_PID=
}

run_once cold
run_once warm
echo "DONE $ART"
