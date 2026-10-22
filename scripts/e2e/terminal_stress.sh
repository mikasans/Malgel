#!/usr/bin/env bash
#
# Stress test for the integrated terminal under Xvfb: floods, signals,
# full-screen programs, split panes, resize, kill and shell exit.
#
#     scripts/e2e/terminal_stress.sh
#
# Every scenario leaves an observable result on disk (a marker file the shell
# writes, a file vim saves, a process that must be gone) so pass/fail does not
# depend on pixels. Responsiveness comes from the app's own heartbeat:
# MALGEL_PERF=1 makes the app log `ui_lag max=..ms` once a second — how late
# the UI thread woke for a 10ms timer — and each scenario reports the worst
# second it saw, plus peak RSS from /proc.
#
# Results go to $ART_DIR/results.tsv (scenario, PASS/FAIL, detail) and the
# script exits non-zero if any scenario failed. Requirements match
# gui_smoke.sh (xvfb, xdotool, imagemagick); vim / less / top for the
# full-screen checks. Set E2E_GPU=1 to keep Vulkan.
#
# Knobs: MALGEL_BIN, E2E_ARTIFACTS, E2E_DISPLAY_START (first Xvfb display to
# try, default 99), TERM_SCENARIOS ("1 3 p2 2 5 4 6" subsets), TERM_LAG_LIMIT_MS
# (flood UI stall, default 250), P2_SECS / P2_CPU_LIMIT / P2_LAG_LIMIT_MS
# (the `yes` flood: default 10s, <100% CPU, ui_lag max <250ms),
# INT_SETTLE_LIMIT_MS (^C to the last changed frame, default 200),
# TERM_RETRY (0: no second run of failed scenario groups),
# KILL_LIMIT_MS (Kill Terminal to job gone, default 500).

set -uo pipefail

if [[ -z "${E2E_INNER:-}" ]]; then
  # Outer run: the suite, then (TERM_RETRY=1, the default) one more go at
  # the scenario groups that failed. Timing limits are absolute, and a box
  # shared with builds (load average in every result line) can blow one
  # second of one scenario; a real regression fails both runs.
  art="${E2E_ARTIFACTS:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/target/e2e-terminal}"
  E2E_INNER=1 E2E_ARTIFACTS="$art" timeout --kill-after=10 "${E2E_TIMEOUT:-900}" "$0" "$@"
  rc=$?
  res="$art/results.tsv"
  if ((rc != 1)) || [[ "${TERM_RETRY:-1}" == 0 || ! -f "$res" ]]; then
    exit "$rc"
  fi
  groups=$(awk -F'\t' '$2 != "PASS" { g = $1; sub(/-.*/, "", g); if (g ~ /^s[0-9]$/) print substr(g, 2); else if (g == "p2") print g }' "$res" | sort -u | tr '\n' ' ')
  # Something that cannot be retried (the app died) fails outright.
  if [[ -z "${groups// /}" ]] || awk -F'\t' '$2 != "PASS" && $1 !~ /^(s[0-9]|p2)-/ { bad = 1 } END { exit !bad }' "$res"; then
    exit "$rc"
  fi
  echo "== retrying scenario groups: $groups(load $(cut -d' ' -f1-3 /proc/loadavg))"
  E2E_INNER=1 E2E_ARTIFACTS="$art/retry" TERM_SCENARIOS="$groups" \
    timeout --kill-after=10 "${E2E_TIMEOUT:-900}" "$0" "$@"
  # A scenario passes if its retry passed; both details are kept.
  awk -F'\t' -v OFS='\t' 'NR == FNR { retry[$1] = $2 "\t" $3; next }
    $2 != "PASS" && ($1 in retry) {
      split(retry[$1], r, "\t")
      if (r[1] == "PASS") { print $1, "PASS", "retry: " r[2] " | first run FAIL: " $3; next }
      print $1, $2, $3 " | retry FAIL: " r[2]; next
    }
    { print }' "$art/retry/results.tsv" "$res" >"$res.merged" && mv "$res.merged" "$res"
  echo; echo "== results (after retry)"; cat "$res"
  ! awk -F'\t' '$2 != "PASS" { bad = 1 } END { exit !bad }' "$res"
  exit $?
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
BIN="${MALGEL_BIN:-$ROOT/target/release/malgel}"
[[ -x "$BIN" ]] || BIN="$ROOT/target/debug/malgel"
ART_DIR="${E2E_ARTIFACTS:-$ROOT/target/e2e-terminal}"
# Worst acceptable one-second UI stall during a flood.
LAG_LIMIT_MS="${TERM_LAG_LIMIT_MS:-250}"
ONLY="${TERM_SCENARIOS:-}"
mkdir -p "$ART_DIR"
rm -f "$ART_DIR"/*.png "$ART_DIR"/*.log "$ART_DIR"/results.tsv 2>/dev/null || true
RESULTS="$ART_DIR/results.tsv"

APP_PID=""
XVFB_PID=""
TMP_ROOT="$(mktemp -d -t malgel-term.XXXXXX)"
cleanup() {
  [[ -n "${APP_PID:-}" ]] && kill "$APP_PID" 2>/dev/null
  [[ -n "${XVFB_PID:-}" ]] && kill "$XVFB_PID" 2>/dev/null
  pkill -f "MALGEL_STRESS_$$" 2>/dev/null
  rm -rf "$TMP_ROOT"
  return 0
}
trap cleanup EXIT
die() { echo "FATAL: $*" >&2; exit 2; }

for tool in Xvfb xdotool import; do command -v "$tool" >/dev/null || die "$tool not installed"; done
[[ -x "$BIN" ]] || die "no binary at $BIN"

export HOME="$TMP_ROOT/home"
mkdir -p "$HOME" && mkdir -m 700 "$TMP_ROOT/xdg"
export XDG_CONFIG_HOME="$HOME/.config" XDG_DATA_HOME="$HOME/.local/share" XDG_RUNTIME_DIR="$TMP_ROOT/xdg"
start_xvfb() {
  local n
  local first="${E2E_DISPLAY_START:-99}"
  for n in $(seq "$first" $((first + 60))); do
    [[ -e "/tmp/.X${n}-lock" || -e "/tmp/.X11-unix/X${n}" ]] || break
  done
  export DISPLAY=":$n"
  Xvfb "$DISPLAY" -screen 0 1600x1000x24 +extension GLX +render -nolisten tcp >"$ART_DIR/xvfb.log" 2>&1 &
  XVFB_PID=$!
  for _ in $(seq 1 100); do
    xdotool getdisplaygeometry >/dev/null 2>&1 && return 0
    kill -0 "$XVFB_PID" 2>/dev/null || break
    sleep 0.1
  done
  kill "$XVFB_PID" 2>/dev/null
  return 1
}
start_xvfb || start_xvfb || die "Xvfb did not start"
if [[ "${E2E_GPU:-0}" != "1" ]]; then
  export LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe VK_ICD_FILENAMES="$ART_DIR/no-such-icd.json"
fi

WS="$TMP_ROOT/ws"
M="$TMP_ROOT/m"   # markers the shell writes
mkdir -p "$WS" "$M"
printf '# terminal stress\n' >"$WS/README.md"
# A 40 MB text file for `cat`.
seq 1 4500000 | sed 's/$/ lorem ipsum/' | head -c 40000000 >"$TMP_ROOT/big.txt"
TAG="MALGEL_STRESS_$$"

APP_LOG="$ART_DIR/app.log"
MALGEL_PERF=1 env -u DBUS_SESSION_BUS_ADDRESS "$BIN" --new-instance "$WS" "$WS/README.md" >"$APP_LOG" 2>&1 &
APP_PID=$!
WIN=""
for _ in $(seq 1 300); do
  WIN="$(xdotool search --onlyvisible --class malgel 2>/dev/null | head -1 || true)"
  [[ -z "$WIN" ]] && WIN="$(xdotool search --onlyvisible --name -i malgel 2>/dev/null | head -1 || true)"
  [[ -n "$WIN" ]] && break
  kill -0 "$APP_PID" 2>/dev/null || die "app exited during startup"
  sleep 0.1
done
[[ -n "$WIN" ]] || die "no window"
xdotool windowsize "$WIN" 1600 1000 2>/dev/null
# Fully on screen, so screenshots see the whole panel.
xdotool windowmove "$WIN" 0 0 2>/dev/null
xdotool windowfocus "$WIN" 2>/dev/null
sleep 2

shot() { import -window root "$ART_DIR/$1.png" 2>/dev/null; }
# The terminal output only (default panel: the bottom third, above the status
# bar, whose memory/CPU pulse would count as a change).
term_shot() { import -window root -crop 1600x280+0+680 "$ART_DIR/$1.png" 2>/dev/null; }
alive() { kill -0 "$APP_PID" 2>/dev/null; }
key() { xdotool key --clearmodifiers "$@"; }
# Type a command into the focused terminal composer and press Enter.
run() { xdotool type --delay 12 -- "$1"; key Return; }
wait_for() { # seconds, command...
  local deadline=$((SECONDS + $1)); shift
  until "$@" 2>/dev/null; do
    ((SECONDS >= deadline)) && return 1
    sleep 0.05
  done
}
now_ms() { date +%s%3N; }
rss_mib() { awk '/VmRSS/{printf "%.0f", $2/1024}' "/proc/$APP_PID/status" 2>/dev/null || echo 0; }
log_mark() { wc -l <"$APP_LOG"; }
# Worst ui_lag reported since log line $1.
lag_since() {
  tail -n +"$(($1 + 1))" "$APP_LOG" | sed -n 's/.*ui_lag max=\([0-9.]*\)ms.*/\1/p' |
    sort -g | tail -1 | awk '{printf "%.0f", $1}'
}
# 95th percentile of the per-second worst ui_lag since log line $1: an upper
# bound on how long a keystroke waits for the UI thread.
lag_p95_since() {
  tail -n +"$(($1 + 1))" "$APP_LOG" | sed -n 's/.*ui_lag max=\([0-9.]*\)ms.*/\1/p' | sort -g |
    awk '{v[NR] = $1} END {if (!NR) {print 0; exit} i = int(NR * 0.95 + 0.5); if (i < 1) i = 1; if (i > NR) i = NR; printf "%.0f", v[i]}'
}
PEAK=0
sample_rss() { local r; r=$(rss_mib); ((r > PEAK)) && PEAK=$r; }
watch_rss() { # run a sampler until $1 exists or $2 seconds pass
  local deadline=$((SECONDS + $2))
  while [[ ! -e "$1" ]] && ((SECONDS < deadline)) && alive; do sample_rss; sleep 0.2; done
  [[ -e "$1" ]]
}
FAILS=0
record() { # scenario status detail (the 1-minute load average is appended)
  printf '%s\t%s\t%s load=%s\n' "$1" "$2" "$3" "$(cut -d' ' -f1 /proc/loadavg)" | tee -a "$RESULTS"
  [[ "$2" == PASS ]] || FAILS=$((FAILS + 1))
}
want() { [[ -z "$ONLY" || " $ONLY " == *" $1 "* ]]; }
# Open the command palette and type $1 (Return not pressed).
palette_type() {
  key ctrl+shift+p; sleep 0.5
  xdotool type --delay 25 -- "$1"; sleep 0.5
}
palette() {
  palette_type "$1"
  key Return; sleep 1
}
# Get back to a fresh shell after a failed scenario left something stuck.
recover() {
  key Escape; key ctrl+c; sleep 0.3
  palette "kill terminal"
  palette "new terminal"; sleep 1.5
  run "cd $M"
}
# True when no process matches (patterns are bracketed, e.g. "[y]es", so pgrep
# never matches a command line that merely contains the pattern).
gone() { ! pgrep -f "$1" >/dev/null; }
fresh_marker() { rm -f "$M/$1"; echo "$M/$1"; }

# Open the terminal (Ctrl+`) and wait for the shell.
key ctrl+grave
sleep 2
run "cd $M; export PS1='$ '"
run "echo ready > $M/ready"
wait_for 10 test -e "$M/ready" || die "terminal shell never answered"
shot 00-terminal-open

flood() { # name, command, bytes (for MB/s), timeout
  local name=$1 cmd=$2 bytes=$3 limit=$4 mark t0 t1 lag
  mark=$(fresh_marker "$name.done")
  PEAK=$(rss_mib); local base=$PEAK lm; lm=$(log_mark)
  xdotool type --delay 12 -- "$cmd; touch $mark"
  t0=$(now_ms); key Return
  if watch_rss "$mark" "$limit"; then
    t1=$(now_ms); sleep 1.2
    lag=$(lag_since "$lm")
    local secs mbs
    secs=$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2f", (b-a)/1000}')
    mbs=$(awk -v n="$bytes" -v s="$secs" 'BEGIN{ if (n>0) printf "%.1f", n/1e6/s; else print "-"}')
    local status=PASS
    ((${lag:-0} > LAG_LIMIT_MS)) && status=FAIL
    ((PEAK - base > 400)) && status=FAIL
    record "$name" "$status" "time=${secs}s throughput=${mbs}MB/s ui_lag_max=${lag}ms rss=${base}->${PEAK}MiB"
  else
    lag=$(lag_since "$lm")
    record "$name" FAIL "no completion within ${limit}s (alive=$(alive && echo y || echo n)) ui_lag_max=${lag}ms rss=${base}->${PEAK}MiB"
    shot "$name-stuck"
    alive || die "app died"
    recover
  fi
  shot "$name"
}

# ---- 1. long / fast output -------------------------------------------------
if want 1; then
  flood s1-yes "yes | head -n 2000000" 4000000 120
  flood s1-seq "seq 1 5000000" "$(seq 1 5000000 | wc -c)" 180
  flood s1-cat "cat $TMP_ROOT/big.txt" "$(stat -c %s "$TMP_ROOT/big.txt")" 180
  flood s1-find "find /usr 2>/dev/null | head -n 400000" 0 120

  # Ctrl+C mid-stream: time until the producer is gone and the screen settles.
  lm=$(log_mark)
  run "yes $TAG"
  sleep 2
  pgrep -f "yes $TAG" >/dev/null || echo "warn: yes not running" >&2
  t0=$(now_ms)
  key ctrl+c
  if wait_for 10 gone "[y]es $TAG"; then
    t1=$(now_ms)
    # Screen must stop scrolling: shoot the output continuously; settled =
    # the last change before 0.6s without any (within 5s).
    settled=no; t2=$t0; term_shot s1-int-a
    while (($(now_ms) - t0 < 5000)); do
      term_shot s1-int-b
      d=$(compare -metric AE "$ART_DIR/s1-int-a.png" "$ART_DIR/s1-int-b.png" null: 2>&1 | cut -d' ' -f1)
      if [[ "${d%.*}" -ge 50 ]]; then
        t2=$(now_ms); mv "$ART_DIR/s1-int-b.png" "$ART_DIR/s1-int-a.png"
      elif (($(now_ms) - t2 >= 600)); then
        settled=yes; break
      fi
    done
    mark=$(fresh_marker s1-int.done); run "touch $mark"
    # The app's own measure (MALGEL_PERF): ^C to the last frame that showed
    # new output. The screenshots above only confirm the screen is still;
    # each one costs ~100ms, so their timing is coarse.
    st=$(tail -n +"$((lm + 1))" "$APP_LOG" | sed -n 's/.*term_interrupt_settle \([0-9]*\)ms.*/\1/p' | tail -1)
    if wait_for 10 test -e "$mark" && [[ $settled == yes && -n "$st" ]] && ((st <= ${INT_SETTLE_LIMIT_MS:-200})); then
      record s1-ctrl-c PASS "producer killed $((t1 - t0))ms after ^C; output settled ${st}ms after ^C (screen still by $((t2 - t0))ms); ui_lag_max=$(lag_since "$lm")ms"
    else
      record s1-ctrl-c FAIL "producer killed $((t1 - t0))ms; output settled ${st:-?}ms after ^C; screen still=$settled by $((t2 - t0))ms; shell answered=$([[ -e $mark ]] && echo y || echo n)"
      recover
    fi
  else
    record s1-ctrl-c FAIL "^C did not stop 'yes' within 10s (signal never reached the PTY)"
    pkill -f "yes $TAG"; sleep 1; recover
  fi
fi

# ---- 3. control keys ---------------------------------------------------------
if want 3; then
  rm -f "$M/catd.txt"; mark=$(fresh_marker s3-d.done)
  run "cat > $M/catd.txt"
  sleep 0.5; run "hello-eof"; sleep 0.3
  key ctrl+d; sleep 0.5
  run "touch $mark"
  if wait_for 5 test -e "$mark" && grep -q hello-eof "$M/catd.txt"; then
    record s3-ctrl-d PASS "EOF ended cat; text kept"
  else
    record s3-ctrl-d FAIL "cat still waiting after Ctrl+D (content: $(head -c 60 "$M/catd.txt" 2>/dev/null | tr '\n' ' '))"
    pkill -f "cat > $M/catd.txt"; recover
  fi

  rm -f "$M/jobs.txt"
  run "sleep 101"
  sleep 0.8; key ctrl+z; sleep 0.5
  run "jobs > $M/jobs.txt; kill -9 %1"
  if wait_for 5 grep -qi stopped "$M/jobs.txt"; then
    record s3-ctrl-z PASS "job suspended ($(tr -s ' ' <"$M/jobs.txt" | head -1))"
  else
    record s3-ctrl-z FAIL "Ctrl+Z did not suspend sleep"
    pkill -f "sleep 101"; recover
  fi

  # Typing during a flood: the line typed mid-flood must arrive intact.
  rm -f "$M/typed.txt"; lm=$(log_mark)
  run "seq 1 4000000"
  sleep 0.7
  t0=$(now_ms)
  run "echo typed-during-flood-0123456789 > $M/typed.txt"
  t1=$(now_ms)
  if wait_for 120 grep -q 'typed-during-flood-0123456789$' "$M/typed.txt"; then
    record s3-type-flood PASS "line intact; xdotool type took $((t1 - t0))ms; ui_lag_max=$(lag_since "$lm")ms"
  else
    record s3-type-flood FAIL "typed line lost or mangled: $(cat "$M/typed.txt" 2>/dev/null)"
    recover
  fi
  # Ctrl+C while flooding (key latency under load).
  lm=$(log_mark)
  run "seq 1 100000000 | grep -v $TAG"
  sleep 1.5
  started=$(pgrep -f "grep -v $TAG" >/dev/null && echo y || echo n)
  t0=$(now_ms); key ctrl+c
  if [[ $started == n ]]; then
    record s3-ctrl-c-flood FAIL "flood never started (terminal not accepting commands)"
    recover
  elif wait_for 10 gone "[g]rep -v $TAG"; then
    record s3-ctrl-c-flood PASS "flood producer gone $(($(now_ms) - t0))ms after ^C; ui_lag_max=$(lag_since "$lm")ms"
  else
    record s3-ctrl-c-flood FAIL "^C during flood not delivered"
    pkill -f "grep -v $TAG"; recover
  fi
fi

# ---- 3b. line editing / job control the shell does itself ------------------------
if want 3; then
  # sleep 30 + ^C: back at the prompt fast.
  run "sleep 30"; sleep 0.8
  t0=$(now_ms); key ctrl+c
  mark=$(fresh_marker s3-sleep.done); run "touch $mark"
  if wait_for 5 test -e "$mark"; then
    record s3-sleep-ctrl-c PASS "prompt answered $(($(now_ms) - t0))ms after ^C"
  else
    record s3-sleep-ctrl-c FAIL "sleep 30 not interrupted"; pkill -f "sleep 30"; recover
  fi
  run "sleep 104"; sleep 0.8; key ctrl+backslash
  if wait_for 5 gone "[s]leep 104"; then
    record s3-ctrl-backslash PASS "SIGQUIT delivered"
  else
    record s3-ctrl-backslash FAIL "Ctrl+\\ not delivered"; pkill -f "sleep 104"; recover
  fi
  if command -v python3 >/dev/null; then
    run "python3 -q"; sleep 1.5; key ctrl+d; sleep 0.5
    mark=$(fresh_marker s3-py.done); run "touch $mark"
    if wait_for 5 test -e "$mark"; then
      record s3-python-ctrl-d PASS "Ctrl+D exits python3 -q"
    else
      record s3-python-ctrl-d FAIL "python3 still running after Ctrl+D"; pkill -f "python3 -q"; recover
    fi
  fi
  rm -f "$M/tab.txt"
  xdotool type --delay 20 "ech"; key Tab; sleep 0.3
  run " tab-ok > $M/tab.txt"
  wait_for 5 grep -q tab-ok "$M/tab.txt" && record s3-tab-complete PASS "ech<Tab> completed to echo" \
    || { record s3-tab-complete FAIL "Tab not delivered to the shell"; recover; }
  run "echo hist-ok > $M/hist.txt"; sleep 0.3
  run "rm -f $M/hist.txt"; sleep 0.3
  key Up; key Up; sleep 0.2; key Return
  wait_for 5 test -e "$M/hist.txt" && record s3-history-up PASS "Up Up Enter re-ran the command" \
    || { record s3-history-up FAIL "Up arrow history not delivered"; recover; }
  run "rm -f $M/hist.txt"; sleep 0.3
  key ctrl+r; xdotool type --delay 20 "hist-ok"; key Return
  wait_for 5 test -e "$M/hist.txt" && record s3-ctrl-r PASS "reverse search found and ran it" \
    || { record s3-ctrl-r FAIL "Ctrl+R not delivered"; key ctrl+c; recover; }
  xdotool type --delay 20 "garbage-words"; key ctrl+u
  mark=$(fresh_marker s3-u.done); run "touch $mark"
  wait_for 5 test -e "$mark" && record s3-ctrl-u PASS "Ctrl+U killed the line" \
    || { record s3-ctrl-u FAIL "Ctrl+U not delivered"; key ctrl+c; recover; }
  xdotool type --delay 20 "foo bar"; key ctrl+w ctrl+w
  mark=$(fresh_marker s3-w.done); run "touch $mark"
  wait_for 5 test -e "$mark" && record s3-ctrl-w PASS "Ctrl+W erased words" \
    || { record s3-ctrl-w FAIL "Ctrl+W not delivered"; key ctrl+c; recover; }
  key ctrl+l; sleep 0.3
  mark=$(fresh_marker s3-l.done); run "touch $mark"
  wait_for 5 test -e "$mark" && record s3-ctrl-l PASS "Ctrl+L cleared, shell fine" \
    || { record s3-ctrl-l FAIL "shell confused after Ctrl+L"; recover; }

  # A click anywhere in the output, then typing at once: every character
  # must reach the shell (r25: printables were dropped, only Enter arrived).
  geo=$(xdotool getwindowgeometry "$WIN" | sed -n 's/.*Geometry: \([0-9]*\)x\([0-9]*\).*/\1 \2/p')
  ok=0; spots=0
  for spot in "2 120" "3 200" "1 260"; do
    fx=${spot% *}; dy=${spot#* }; spots=$((spots + 1))
    mark=$(fresh_marker "s3-click-$spots.done")
    xdotool mousemove --window "$WIN" $((${geo% *} * fx / 4)) $((${geo#* } - dy)) click 1
    run "touch $mark"
    wait_for 5 test -e "$mark" && ok=$((ok + 1))
  done
  ((ok == spots)) && record s3-click-then-type PASS "typed text reached the shell after $spots clicks in the output" \
    || { record s3-click-then-type FAIL "only $ok/$spots commands arrived after a click in the output"; recover; }
fi

# ---- P2. `yes` flood (P2_SECS, default 10): CPU, RSS, Ctrl+P latency, ^C ------
cpu_ticks() { awk '{print $14 + $15}' "/proc/$APP_PID/stat"; }
crop() { import -window root -crop 700x160+450+30 "$ART_DIR/$1.png" 2>/dev/null; }
P2_SECS="${P2_SECS:-10}"
if want p2; then
  lm=$(log_mark); r0=$(rss_mib); PEAK=$r0
  run "yes $TAG"; sleep 1
  c0=$(cpu_ticks); t0=$(now_ms)
  for i in $(seq 1 $((P2_SECS * 5 / 2))); do sample_rss; sleep 0.2; done
  plateau=$(rss_mib)
  # Ctrl+P (Quick Open) while flooding: time until the palette area changes.
  crop p2-before; tp=$(now_ms); key ctrl+p; opened=no
  for i in $(seq 1 40); do
    crop p2-after
    d=$(compare -metric AE "$ART_DIR/p2-before.png" "$ART_DIR/p2-after.png" null: 2>&1 | cut -d' ' -f1)
    [[ "${d%.*}" -gt 2000 ]] && { opened=yes; break; }
  done
  tp1=$(now_ms); key Escape; sleep 0.3
  # Back into the terminal (Escape returns focus to the editor).
  for i in $(seq 1 $((P2_SECS * 5 / 2))); do sample_rss; sleep 0.2; done
  c1=$(cpu_ticks); t1=$(now_ms)
  hz=$(getconf CLK_TCK)
  cpu=$(awk -v a="$c0" -v b="$c1" -v t0="$t0" -v t1="$t1" -v hz="$hz" 'BEGIN{printf "%.0f", (b-a)/hz/((t1-t0)/1000)*100}')
  # Back into the terminal: click its output (focus returns to the input).
  geo=$(xdotool getwindowgeometry "$WIN" | sed -n 's/.*Geometry: \([0-9]*\)x\([0-9]*\).*/\1 \2/p')
  xdotool mousemove --window "$WIN" $((${geo% *} / 2)) $((${geo#* } - 120)) click 1; sleep 0.3
  ti=$(now_ms); key ctrl+c
  wait_for 3 gone "[y]es $TAG"
  gone=$(pgrep -f "yes $TAG" >/dev/null && echo n || echo y); tg=$(now_ms)
  mark=$(fresh_marker p2.done); run "touch $mark"; wait_for 5 test -e "$mark"; tm=$(now_ms)
  lag=$(lag_since "$lm"); p95=$(lag_p95_since "$lm")
  status=PASS
  [[ $gone == y && $opened == yes ]] || status=FAIL
  ((cpu >= ${P2_CPU_LIMIT:-100} || lag >= ${P2_LAG_LIMIT_MS:-250})) && status=FAIL
  record "p2-yes-${P2_SECS}s" "$status" "cpu=${cpu}% rss=${r0}->${PEAK}MiB (plateau ${plateau}) ctrl+p=$([[ $opened == yes ]] && echo "$((tp1 - tp))ms" || echo never) ^C->yes-gone=$((tg - ti))ms ^C->prompt=$((tm - ti))ms ui_lag_p95=${p95}ms ui_lag_max=${lag}ms"
  [[ $gone == y ]] || { pkill -f "yes $TAG"; recover; }
fi

# ---- 2. full-screen programs -------------------------------------------------
if want 2; then
  rm -f "$M/vim.txt"; mark=$(fresh_marker s2-vim.done)
  run "vim -u NONE -N $M/vim.txt"
  sleep 1.5; shot s2-vim-open
  key i; sleep 0.2
  xdotool type --delay 20 "vim-ok"
  key Escape; sleep 0.2
  # Resize while vim runs: it must redraw, not wedge.
  xdotool windowsize "$WIN" 1300 850; sleep 0.8
  xdotool windowsize "$WIN" 1600 1000; sleep 0.8
  shot s2-vim-resized
  xdotool type --delay 30 ":wq"; key Return; sleep 0.8
  run "touch $mark"
  if wait_for 5 test -e "$mark" && grep -qx vim-ok "$M/vim.txt"; then
    record s2-vim PASS "insert, Esc, resize, :wq all delivered"
  else
    record s2-vim FAIL "vim.txt=$(head -c 40 "$M/vim.txt" 2>/dev/null | tr '\n' ' ') marker=$([[ -e $mark ]] && echo y || echo n)"
    pkill -f "vim -u NONE -N $M/vim.txt"; recover
  fi

  mark=$(fresh_marker s2-less.done)
  run "seq 1 1000 | less"
  sleep 1; key space; sleep 0.3; key G; sleep 0.3; shot s2-less
  key q; sleep 0.5
  run "touch $mark"
  if wait_for 5 test -e "$mark"; then
    record s2-less PASS "scroll + q delivered, shell back"
  else
    record s2-less FAIL "less did not quit on q"
    pkill -x less; recover
  fi

  mark=$(fresh_marker s2-top.done)
  run "top -d 0.5"
  sleep 2; shot s2-top
  key q; sleep 0.5
  run "touch $mark"
  if wait_for 5 test -e "$mark"; then
    record s2-top PASS "refresh + q delivered, shell back"
  else
    record s2-top FAIL "top did not quit on q"
    pkill -x top; recover
  fi
  # After leaving the alternate screen the composer must own input again.
  mark=$(fresh_marker s2-after.done)
  run "touch $mark"
  wait_for 5 test -e "$mark" && record s2-composer-back PASS "composer works after TUI exit" \
    || { record s2-composer-back FAIL "composer dead after TUI"; recover; }
fi

# ---- 5. resize during output, stty size --------------------------------------
if want 5; then
  mark=$(fresh_marker s5-resize.done); lm=$(log_mark)
  run "seq 1 3000000; touch $mark"
  for i in $(seq 1 10); do
    xdotool windowsize "$WIN" 1200 800; sleep 0.25
    xdotool windowsize "$WIN" 1600 1000; sleep 0.25
  done
  if wait_for 120 test -e "$mark" && alive; then
    lag=$(lag_since "$lm"); st=PASS; ((${lag:-0} > LAG_LIMIT_MS)) && st=FAIL
    record s5-resize-flood "$st" "ui_lag_max=${lag}ms (limit ${LAG_LIMIT_MS}ms)"
  else
    record s5-resize-flood FAIL "flood + resize did not finish (alive=$(alive && echo y || echo n))"
    recover
  fi
  rm -f "$M/size.txt"; run "stty size > $M/size.txt"
  wait_for 5 test -s "$M/size.txt"
  sz=$(cat "$M/size.txt" 2>/dev/null)
  record s5-pty-size "$([[ -n "$sz" && "$sz" != "24 100" ]] && echo PASS || echo FAIL)" "stty size: $(cat "$M/size.txt" 2>/dev/null) (24 100 = never resized)"
fi

# ---- 4. split terminal ---------------------------------------------------------
if want 4; then
  palette "split terminal"
  sleep 2; shot s4-split
  geo=$(xdotool getwindowgeometry "$WIN" | sed -n 's/.*Geometry: \([0-9]*\)x\([0-9]*\).*/\1 \2/p')
  W=${geo% *}; H=${geo#* }
  # Focused pane after split is the new one; the left pane is clicked later.
  run "cd $M; tty > $M/tty_b"
  rm -f "$M/s4a.done" "$M/s4b.done"; lm=$(log_mark)
  run "seq 1 3000000; touch $M/s4b.done"
  # Click the left pane's transcript (focuses its composer).
  xdotool mousemove --window "$WIN" $((W / 4)) $((H - 150)) click 1; sleep 0.4
  run "cd $M; tty > $M/tty_a; seq 1 3000000; touch $M/s4a.done"
  if wait_for 180 test -e "$M/s4a.done" -a -e "$M/s4b.done"; then
    a=$(cat "$M/tty_a" 2>/dev/null); b=$(cat "$M/tty_b" 2>/dev/null)
    if [[ -n "$a" && -n "$b" && "$a" != "$b" ]]; then
      lag=$(lag_since "$lm"); st=PASS; ((${lag:-0} > LAG_LIMIT_MS)) && st=FAIL
      record s4-split-flood "$st" "both panes finished; ttys $a / $b; ui_lag_max=${lag}ms (limit ${LAG_LIMIT_MS}ms)"
    else
      record s4-split-flood FAIL "input went to the wrong pane (ttys '$a' / '$b')"
    fi
  else
    record s4-split-flood FAIL "a pane starved (a=$([[ -e $M/s4a.done ]] && echo y || echo n) b=$([[ -e $M/s4b.done ]] && echo y || echo n))"
  fi
  # Kill one pane while the other streams.
  run "sleep 102"   # left pane (focused)
  xdotool mousemove --window "$WIN" $((W * 3 / 4)) $((H - 150)) click 1; sleep 0.4
  rm -f "$M/s4k.done"
  run "for i in \$(seq 1 40); do echo tick \$i; sleep 0.05; done; touch $M/s4k.done"
  xdotool mousemove --window "$WIN" $((W / 4)) $((H - 150)) click 1; sleep 0.3
  lm=$(log_mark); t0=$(now_ms)
  palette "kill terminal"
  if wait_for 20 test -e "$M/s4k.done" && wait_for 5 gone "[s]leep 102"; then
    record s4-kill-one PASS "other pane kept running; killed job gone; ui_lag_max=$(lag_since "$lm")ms"
  else
    record s4-kill-one FAIL "other pane done=$([[ -e $M/s4k.done ]] && echo y || echo n); sleep 102 alive=$(pgrep -f 'sleep 102' >/dev/null && echo y || echo n)"
    pkill -f 'sleep 102'
  fi
  shot s4-after-kill
fi

# ---- 5b. close while running, shell exit ------------------------------------------
if want 5; then
  key ctrl+grave; sleep 0.3
  # Make sure a terminal is focused/visible.
  pgrep -f "sleep 102" >/dev/null && pkill -f "sleep 102"
  key ctrl+grave; sleep 1
  run "cd $M"
  run "sleep 103"
  sleep 0.8; lm=$(log_mark)
  palette_type "kill terminal"
  t0=$(now_ms); key Return
  if wait_for 5 gone "[s]leep 103"; then
    tk=$(($(now_ms) - t0)); sleep 1
    st=PASS; ((tk > ${KILL_LIMIT_MS:-500})) && st=FAIL
    record s5-kill-running "$st" "foreground job gone ${tk}ms after Kill Terminal (Return in the palette); ui_lag_max=$(lag_since "$lm")ms"
  else
    record s5-kill-running FAIL "job survived Kill"
    pkill -f "sleep 103"
  fi
  key ctrl+grave; sleep 0.5; key ctrl+grave; sleep 1.5
  run "cd $M"; sleep 0.5
  run "echo \$\$ > $M/shellpid"; wait_for 5 test -s "$M/shellpid"
  spid=$(cat "$M/shellpid" 2>/dev/null)
  run "exit"
  if wait_for 5 bash -c "! kill -0 $spid 2>/dev/null"; then
    mark=$(fresh_marker s5-exit.done)
    sleep 0.5
    run "cd $M && touch $mark"
    if wait_for 8 test -e "$mark"; then
      record s5-shell-exit PASS "exit handled; composer restarted a shell"
    else
      record s5-shell-exit FAIL "after exit, composer did not start a new shell"
    fi
  else
    record s5-shell-exit FAIL "shell $spid did not exit"
  fi
  shot s5-exit
fi

# ---- 6. tabs: New, then Split, keeps every session -------------------------------
# VS Code: Split divides only the current tab; the tab New made before it and
# every shell in it survive (an old bug: + then Split lost a session).
if want 6; then
  geo=$(xdotool getwindowgeometry "$WIN" | sed -n 's/.*Geometry: \([0-9]*\)x\([0-9]*\).*/\1 \2/p')
  W=${geo% *}; H=${geo#* }
  rm -f "$M"/s6-p*
  palette "new terminal"; sleep 1.5
  run "echo \$\$ > $M/s6-p1"
  palette "new terminal"; sleep 1.5
  run "echo \$\$ > $M/s6-p2"
  palette "split terminal"; sleep 1.5
  run "echo \$\$ > $M/s6-p3"
  wait_for 5 test -s "$M/s6-p1" -a -s "$M/s6-p2" -a -s "$M/s6-p3"
  p1=$(cat "$M/s6-p1" 2>/dev/null); p2=$(cat "$M/s6-p2" 2>/dev/null); p3=$(cat "$M/s6-p3" 2>/dev/null)
  live=0
  for p in $p1 $p2 $p3; do kill -0 "$p" 2>/dev/null && live=$((live + 1)); done
  distinct=$(printf '%s\n' "$p1" "$p2" "$p3" | sort -u | grep -c .)
  # The split's left pane is the second tab's shell, not the first tab's.
  xdotool mousemove --window "$WIN" $((W / 4)) $((H - 150)) click 1; sleep 0.4
  run "echo \$\$ > $M/s6-p2b"
  wait_for 5 test -s "$M/s6-p2b"
  p2b=$(cat "$M/s6-p2b" 2>/dev/null)
  shot s6-tabs
  if ((live == 3 && distinct == 3)) && [[ "$p2b" == "$p2" ]]; then
    record s6-new-then-split PASS "3 live shells ($p1 / $p2 | $p3); left pane of the split is tab 2's shell"
  else
    record s6-new-then-split FAIL "shells '$p1' '$p2' '$p3' live=$live distinct=$distinct; left pane shell '$p2b' (want '$p2')"
  fi
fi

alive && record app-alive PASS "rss=$(rss_mib)MiB" || record app-alive FAIL "app died"
echo
cat "$RESULTS"
exit $((FAILS > 0))
