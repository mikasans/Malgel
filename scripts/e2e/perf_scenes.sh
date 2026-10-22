#!/usr/bin/env bash
#
# Performance regression gate: runs Malgel under its own Xvfb N times (3 by
# default), measures each run with perf_probe.py, and writes the medians to
# perf.json. Exits non-zero when a blocking threshold in perf_thresholds.json
# is exceeded.
#
#     scripts/e2e/perf_scenes.sh                 # release binary, 3 runs
#     PERF_RUNS=5 scripts/e2e/perf_scenes.sh
#     PERF_THRESHOLDS=my.json scripts/e2e/perf_scenes.sh
#     PERF_STRICT=1 ...                          # non-blocking checks block too
#     E2E_GPU=1 ...                              # Vulkan (lavapipe when found)
#
# Metrics (see scripts/e2e/README.md): first frame / settled, Typst 12-page
# completion, RSS peak/final, editor and terminal-composer input latency
# p50/p95, terminal throughput (seq 1 200000, cat 20MB), Ctrl+C-to-prompt,
# idle CPU (window in the background; with focus is reported too).
#
# The run owns everything it touches: a display number nobody holds, its own
# Xvfb (killed by PID, never by name), and a throwaway HOME/XDG per run.

set -euo pipefail
if [[ -z "${E2E_INNER:-}" ]]; then
  E2E_INNER=1 exec timeout --kill-after=10 "${E2E_TIMEOUT:-900}" "$0" "$@"
fi
trap 'echo "FAIL: line $LINENO: $BASH_COMMAND (exit $?)" >&2' ERR

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
BIN="${MALGEL_BIN:-$ROOT/target/release/malgel}"
ART="${E2E_ARTIFACTS:-$ROOT/target/e2e-artifacts/perf-scenes}"
RUNS="${PERF_RUNS:-3}"
THRESHOLDS="${PERF_THRESHOLDS:-$ROOT/scripts/e2e/perf_thresholds.json}"
[[ -x "$BIN" ]] || { echo "missing $BIN — cargo build --release first" >&2; exit 2; }
for tool in Xvfb xdotool python3; do
  command -v "$tool" >/dev/null || { echo "$tool not installed" >&2; exit 2; }
done
mkdir -p "$ART"
rm -f "$ART"/run-*.json "$ART"/perf.json

TMP_ROOT="$(mktemp -d -t malgel-perf.XXXXXX)"
XVFB_PID=""
cleanup() {
  [[ -n "$XVFB_PID" ]] && kill "$XVFB_PID" 2>/dev/null || true
  [[ -n "$XVFB_PID" ]] && wait "$XVFB_PID" 2>/dev/null || true
  rm -rf "$TMP_ROOT"
  return 0
}
trap cleanup EXIT

start_xvfb() {
  local n
  if [[ -n "${E2E_DISPLAY:-}" ]]; then
    DISP="$E2E_DISPLAY"
  else
    for n in $(seq 170 240); do
      [[ -e "/tmp/.X${n}-lock" || -e "/tmp/.X11-unix/X${n}" ]] || break
    done
    DISP=":$n"
  fi
  Xvfb "$DISP" -screen 0 1440x900x24 +extension GLX +render -nolisten tcp \
    >"$ART/xvfb.log" 2>&1 &
  XVFB_PID=$!
  for _ in $(seq 1 100); do
    DISPLAY="$DISP" xdotool getdisplaygeometry >/dev/null 2>&1 && return 0
    kill -0 "$XVFB_PID" 2>/dev/null || break
    sleep 0.1
  done
  kill "$XVFB_PID" 2>/dev/null || true
  XVFB_PID=""
  return 1
}
start_xvfb || start_xvfb || { cat "$ART/xvfb.log" >&2; echo "Xvfb did not start" >&2; exit 2; }
export DISPLAY="$DISP"
echo "display $DISPLAY (Xvfb pid $XVFB_PID)"

if [[ "${E2E_GPU:-0}" == "1" ]]; then
  # Vulkan: prefer Mesa's lavapipe where it is installed. The file name
  # differs by distro (lvp_icd.json on Debian, lvp_icd.x86_64.json on Fedora).
  if [[ -z "${VK_ICD_FILENAMES:-}" ]]; then
    for icd in /usr/share/vulkan/icd.d/lvp_icd*.json /etc/vulkan/icd.d/lvp_icd*.json; do
      [[ -e "$icd" ]] && { export VK_ICD_FILENAMES="$icd"; break; }
    done
  fi
  echo "renderer: Vulkan (VK_ICD_FILENAMES=${VK_ICD_FILENAMES:-system default})"
else
  # Software GL: Xvfb has no DRI3, so Vulkan present would not paint.
  export LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe
  export VK_ICD_FILENAMES="$TMP_ROOT/no-such-icd.json"
  echo "renderer: software GL (llvmpipe)"
fi
export MALGEL_PERF=1
export RUST_LOG="${RUST_LOG:-malgel=info}"

# The render-budget heuristic still has to keep the 12-page sample under the
# consent line and the 22-page one over it.
python3 - <<'PY'
from pathlib import Path
def estimate(src):
    return max(src.count("#pagebreak") + 1, max(len(src) // 2500, 1)) * 7.7
em = estimate(Path("samples/perf-scenes/multi-page.typ").read_text())
eh = estimate(Path("samples/perf-scenes/heavy-budget.typ").read_text())
assert em < 150 <= eh, (em, eh)
print(f"budget estimates ok ({em:.0f} / {eh:.0f} MiB)")
PY

declare -a RUN_FILES=()
for i in $(seq 1 "$RUNS"); do
  echo "=== run $i/$RUNS ==="
  RUN_TMP="$TMP_ROOT/run-$i"
  mkdir -p "$RUN_TMP/home" "$ART/run-$i"
  mkdir -m 700 "$RUN_TMP/xdg"
  HOME="$RUN_TMP/home" XDG_CONFIG_HOME="$RUN_TMP/home/.config" \
    XDG_DATA_HOME="$RUN_TMP/home/.local/share" XDG_CACHE_HOME="$RUN_TMP/home/.cache" \
    XDG_RUNTIME_DIR="$RUN_TMP/xdg" \
    python3 "$ROOT/scripts/e2e/perf_probe.py" run --bin "$BIN" --tmp "$RUN_TMP" \
      --art "$ART/run-$i" --out "$ART/run-$i.json"
  RUN_FILES+=("$ART/run-$i.json")
done

echo "=== gate ($THRESHOLDS) ==="
status=0
python3 "$ROOT/scripts/e2e/perf_probe.py" gate --thresholds "$THRESHOLDS" \
  --out "$ART/perf.json" "${RUN_FILES[@]}" || status=$?
echo "perf.json: $ART/perf.json"
exit "$status"
