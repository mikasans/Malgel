#!/usr/bin/env python3
"""Measure one Malgel run under an X server, and gate the medians of several.

    perf_probe.py run --bin BIN --tmp DIR --out run.json [--art DIR]
    perf_probe.py gate --thresholds FILE --out perf.json run1.json run2.json ...
    perf_probe.py compare a/perf.json b/perf.json [--tolerance 0.15]

`run` drives the app the way a person would, through XTest key events, and
times what the screen does in answer by reading the root window back
(XGetImage) every few milliseconds. Everything it needs comes from libX11 and
libXtst through ctypes, so it works on a bare CI image with no python-xlib.

`gate` takes the median of each metric over the runs, writes perf.json and
exits 1 when a blocking threshold is exceeded (2 when a metric is missing).
"""

import argparse
import ctypes
import json
import os
import re
import signal
import statistics
import subprocess
import sys
import time
import zlib
from pathlib import Path

# ---------------------------------------------------------------------------
# X11 through ctypes


class XImage(ctypes.Structure):
    _fields_ = [
        ("width", ctypes.c_int),
        ("height", ctypes.c_int),
        ("xoffset", ctypes.c_int),
        ("format", ctypes.c_int),
        ("data", ctypes.POINTER(ctypes.c_char)),
        ("byte_order", ctypes.c_int),
        ("bitmap_unit", ctypes.c_int),
        ("bitmap_bit_order", ctypes.c_int),
        ("bitmap_pad", ctypes.c_int),
        ("depth", ctypes.c_int),
        ("bytes_per_line", ctypes.c_int),
        ("bits_per_pixel", ctypes.c_int),
    ]


class X:
    def __init__(self, display):
        self.x11 = ctypes.CDLL("libX11.so.6")
        self.xtst = ctypes.CDLL("libXtst.so.6")
        x11 = self.x11
        x11.XOpenDisplay.restype = ctypes.c_void_p
        x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
        x11.XDefaultRootWindow.restype = ctypes.c_ulong
        x11.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
        x11.XGetImage.restype = ctypes.POINTER(XImage)
        x11.XGetImage.argtypes = [
            ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
            ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong, ctypes.c_int,
        ]
        x11.XFree.argtypes = [ctypes.c_void_p]
        x11.XStringToKeysym.restype = ctypes.c_ulong
        x11.XStringToKeysym.argtypes = [ctypes.c_char_p]
        x11.XKeysymToKeycode.restype = ctypes.c_ubyte
        x11.XKeysymToKeycode.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
        x11.XFlush.argtypes = [ctypes.c_void_p]
        x11.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
        x11.XDisplayWidth.argtypes = [ctypes.c_void_p, ctypes.c_int]
        x11.XDisplayHeight.argtypes = [ctypes.c_void_p, ctypes.c_int]
        x11.XCreateSimpleWindow.restype = ctypes.c_ulong
        x11.XCreateSimpleWindow.argtypes = [
            ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
            ctypes.c_uint, ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong, ctypes.c_ulong,
        ]
        x11.XMapWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
        x11.XSetInputFocus.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
        self.xtst.XTestFakeKeyEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
        self.xtst.XTestFakeMotionEvent.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_ulong]
        self.d = x11.XOpenDisplay(display.encode())
        if not self.d:
            raise SystemExit(f"cannot open X display {display}")
        self.root = x11.XDefaultRootWindow(self.d)
        self.width = x11.XDisplayWidth(self.d, 0)
        self.height = x11.XDisplayHeight(self.d, 0)
        # Region (x, y, w, h) that frame hashes look at; whole screen until set.
        self.region = (0, 0, self.width, self.height)

    def frame(self, region=None):
        x, y, w, h = region or self.region
        im = self.x11.XGetImage(self.d, self.root, x, y, w, h, 0xFFFFFFFF, 2)
        if not im:
            return 0
        data = ctypes.string_at(im.contents.data, im.contents.bytes_per_line * im.contents.height)
        self.x11.XFree(im.contents.data)
        self.x11.XFree(im)
        return zlib.crc32(data)

    # Keys that need Shift on a US layout, by the keysym they produce.
    SHIFTED = {
        "~": "asciitilde", "!": "exclam", "@": "at", "#": "numbersign", "$": "dollar",
        "%": "percent", "^": "asciicircum", "&": "ampersand", "*": "asterisk",
        "(": "parenleft", ")": "parenright", "_": "underscore", "+": "plus",
        "{": "braceleft", "}": "braceright", "|": "bar", ":": "colon",
        '"': "quotedbl", "<": "less", ">": "greater", "?": "question",
    }
    PLAIN = {
        " ": "space", "-": "minus", "=": "equal", "[": "bracketleft", "]": "bracketright",
        "\\": "backslash", ";": "semicolon", "'": "apostrophe", ",": "comma",
        ".": "period", "/": "slash", "`": "grave",
    }
    BASE_OF_SHIFTED = {
        "~": "grave", "!": "1", "@": "2", "#": "3", "$": "4", "%": "5", "^": "6",
        "&": "7", "*": "8", "(": "9", ")": "0", "_": "minus", "+": "equal",
        "{": "bracketleft", "}": "bracketright", "|": "backslash", ":": "semicolon",
        '"': "apostrophe", "<": "comma", ">": "period", "?": "slash",
    }

    def keycode(self, name):
        sym = self.x11.XStringToKeysym(name.encode())
        code = self.x11.XKeysymToKeycode(self.d, sym)
        if not code:
            raise SystemExit(f"no keycode for {name!r}")
        return code

    def chord(self, combo):
        """Press a chord like `ctrl+shift+f` or `Return`."""
        parts = combo.split("+")
        mods = {"ctrl": "Control_L", "shift": "Shift_L", "alt": "Alt_L", "super": "Super_L"}
        held = [self.keycode(mods[p]) for p in parts[:-1]]
        key = parts[-1]
        key = {"grave": "grave", "backslash": "backslash"}.get(key, key)
        code = self.keycode(key)
        for m in held:
            self.xtst.XTestFakeKeyEvent(self.d, m, 1, 0)
        self.xtst.XTestFakeKeyEvent(self.d, code, 1, 0)
        self.xtst.XTestFakeKeyEvent(self.d, code, 0, 0)
        for m in reversed(held):
            self.xtst.XTestFakeKeyEvent(self.d, m, 0, 0)
        self.x11.XFlush(self.d)

    def char(self, c):
        if c.isalpha() and c.isupper():
            self.chord("shift+" + c.lower())
        elif c in self.SHIFTED:
            self.chord("shift+" + self.BASE_OF_SHIFTED[c])
        elif c in self.PLAIN:
            self.chord(self.PLAIN[c])
        else:
            self.chord(c)

    def type(self, text, delay=0.012):
        for c in text:
            self.char(c)
            time.sleep(delay)

    def pointer(self, x, y):
        self.xtst.XTestFakeMotionEvent(self.d, 0, x, y, 0)
        self.x11.XFlush(self.d)

    def steal_focus(self):
        """Map a small window of our own and focus it, so Malgel's window
        goes inactive the way it does when another app comes to the front."""
        w = self.x11.XCreateSimpleWindow(self.d, self.root, self.width - 20, self.height - 20, 10, 10, 0, 0, 0)
        self.x11.XMapWindow(self.d, w)
        self.x11.XSync(self.d, 0)
        time.sleep(0.2)
        self.pointer(self.width - 15, self.height - 15)
        self.x11.XSetInputFocus(self.d, w, 1, 0)  # RevertToPointerRoot
        self.x11.XSync(self.d, 0)


# ---------------------------------------------------------------------------
# Screen timing helpers


def now_ms():
    return time.perf_counter() * 1000.0


def wait_change(x, base, timeout_ms, poll=0.003, region=None):
    """Milliseconds-since-epoch of the first frame that differs from `base`."""
    end = now_ms() + timeout_ms
    while now_ms() < end:
        h = x.frame(region)
        if h != base:
            return now_ms(), h
        time.sleep(poll)
    return None, base


def wait_settle(x, quiet_ms, timeout_ms, region=None, poll=0.01):
    """Time of the last change before the screen holds still for `quiet_ms`."""
    start = now_ms()
    last_change = start
    h = x.frame(region)
    while now_ms() - start < timeout_ms:
        time.sleep(poll)
        n = x.frame(region)
        t = now_ms()
        if n != h:
            h = n
            last_change = t
        elif t - last_change >= quiet_ms:
            return last_change
    return None


def percentile(values, pct):
    if not values:
        return None
    s = sorted(values)
    k = (len(s) - 1) * pct / 100.0
    lo, hi = int(k), min(int(k) + 1, len(s) - 1)
    return s[lo] + (s[hi] - s[lo]) * (k - lo)


# ---------------------------------------------------------------------------
# /proc helpers


def proc_status(pid, field):
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith(field + ":"):
                return int(line.split()[1])
    except OSError:
        pass
    return None


def tree_pids(root):
    """`root` and every descendant."""
    children = {}
    for d in Path("/proc").iterdir():
        if not d.name.isdigit():
            continue
        try:
            stat = (d / "stat").read_text()
        except OSError:
            continue
        ppid = int(stat.rsplit(")", 1)[1].split()[1])
        children.setdefault(ppid, []).append(int(d.name))
    out, todo = [], [root]
    while todo:
        p = todo.pop()
        out.append(p)
        todo.extend(children.get(p, []))
    return out


def cpu_ticks(pids):
    total = 0
    for p in pids:
        try:
            f = Path(f"/proc/{p}/stat").read_text().rsplit(")", 1)[1].split()
            total += int(f[11]) + int(f[12])
        except (OSError, IndexError, ValueError):
            pass
    return total


def find_proc(root, needle):
    for p in tree_pids(root):
        try:
            cmd = Path(f"/proc/{p}/cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace")
        except OSError:
            continue
        if needle in cmd:
            return p
    return None


def idle_cpu(pid, seconds):
    tick = os.sysconf("SC_CLK_TCK")
    pids = tree_pids(pid)
    a = cpu_ticks(pids)
    time.sleep(seconds)
    b = cpu_ticks(pids)
    return (b - a) / tick / seconds * 100.0


# ---------------------------------------------------------------------------
# One run


def log(msg):
    print(f"[perf] {msg}", flush=True)


def window_geometry(display):
    env = dict(os.environ, DISPLAY=display)
    for _ in range(100):
        r = subprocess.run(["xdotool", "search", "--onlyvisible", "--name", "Malgel"],
                           capture_output=True, text=True, env=env)
        ids = r.stdout.split()
        if ids:
            wid = ids[0]
            g = subprocess.run(["xdotool", "getwindowgeometry", "--shell", wid],
                               capture_output=True, text=True, env=env).stdout
            vals = dict(line.split("=", 1) for line in g.split())
            return wid, int(vals["X"]), int(vals["Y"]), int(vals["WIDTH"]), int(vals["HEIGHT"])
        time.sleep(0.1)
    return None


def typing_latency(x, keys, region, timeout_ms=1500, gap=0.12):
    """Per-key latency: key event out → first frame that changed."""
    samples, misses = [], 0
    for k in keys:
        time.sleep(gap)
        base = x.frame(region)
        t0 = now_ms()
        x.char(k)
        t1, _ = wait_change(x, base, timeout_ms, region=region)
        if t1 is None:
            misses += 1
        else:
            samples.append(t1 - t0)
    return samples, misses


def shot(art, name):
    """Screenshot for eyeballing a run afterwards; skipped without ImageMagick."""
    try:
        subprocess.run(["import", "-window", "root", str(Path(art) / f"{name}.png")],
                       capture_output=True, timeout=10)
    except (OSError, subprocess.TimeoutExpired):
        pass


def perf_mark_ms(logfile, pattern):
    try:
        text = Path(logfile).read_text(errors="replace")
    except OSError:
        return None
    m = re.search(r"malgel-perf: " + pattern + r"[^\n]*?\+([0-9.]+)ms", text)
    return float(m.group(1)) if m else None


def oneline_scene(args, x, tmp, art, metrics, notes):
    """A 10 MB JSON file on one line, soft wrap on: keys, the wrap toggle
    and memory. VS Code stays usable here; whole-line work per frame used to
    cost seconds per key."""
    ws = tmp / "oneline"
    ws.mkdir(parents=True, exist_ok=True)
    path = ws / "oneline.json"
    if not path.exists():
        item = '{"id":%d,"name":"item-%d","tags":["a","b","c"],"v":%d.5}'
        parts, size, i = [], 1, 0
        while size < 10_000_000:
            parts.append(item % (i, i, i))
            size += len(parts[-1]) + 1
            i += 1
        path.write_text("[" + ",".join(parts) + "]")
    display = os.environ["DISPLAY"]
    applog = art / "oneline-app.log"
    blank = x.frame()
    t_launch = now_ms()
    app = subprocess.Popen(
        [args.bin, "--new-instance", str(ws), str(path)],
        stdout=open(applog, "w"), stderr=subprocess.STDOUT, start_new_session=True,
    )
    try:
        if wait_change(x, blank, 20000, poll=0.005)[0] is None:
            notes.append("oneline: no frame within 20s")
            return
        settled = wait_settle(x, 700, 30000)
        metrics["oneline_open_ms"] = (settled - t_launch) if settled else None
        geo = window_geometry(display)
        if not geo:
            notes.append("oneline: window not found")
            return
        wid, wx, wy, ww, wh = geo
        region = (max(wx, 0), max(wy, 0), min(ww, x.width - wx), max(min(wh, x.height - wy) - 32, 32))
        env = dict(os.environ, DISPLAY=display)
        subprocess.run(["xdotool", "windowfocus", "--sync", wid], env=env, capture_output=True)
        time.sleep(0.5)

        def chords(keys):
            samples = []
            for k in keys:
                time.sleep(0.15)
                base = x.frame(region)
                t0 = now_ms()
                x.chord(k)
                t1, _ = wait_change(x, base, 3000, region=region)
                samples.append((t1 or now_ms()) - t0)
            return samples

        # 32 samples each, the first two dropped as warm-up: with 10, the
        # p95 was the second-worst key and the gate flaked.
        arrows = chords(["Down", "Right", "Right", "Down", "Left", "Down", "Up", "Right"] * 4)[2:]
        typed = chords(list("onelinetypingintoaverylonglinexy"))[2:]
        jumps = chords(["ctrl+End", "ctrl+Home", "ctrl+End", "ctrl+Home"])
        shot(art, "4-oneline-keys")
        log(f"oneline samples: arrows {[round(v) for v in arrows]} typed {[round(v) for v in typed]} jumps {[round(v) for v in jumps]}")
        metrics["oneline_arrow_p95_ms"] = percentile(arrows, 95)
        metrics["oneline_type_p95_ms"] = percentile(typed, 95)
        metrics["oneline_jump_max_ms"] = max(jumps)
        toggles = []
        for _ in range(2):
            time.sleep(0.3)
            t0 = now_ms()
            x.chord("alt+z")
            done = wait_settle(x, 400, 10000, region=region)
            toggles.append((done or now_ms()) - t0)
        metrics["oneline_wrap_toggle_ms"] = max(toggles)
        shot(art, "4-oneline")
        hwm = proc_status(app.pid, "VmHWM")
        metrics["oneline_rss_peak_mib"] = hwm / 1024.0 if hwm else None
        log(f"oneline: open {metrics['oneline_open_ms']} arrows p95 {metrics['oneline_arrow_p95_ms']:.0f} "
            f"type p95 {metrics['oneline_type_p95_ms']:.0f} jumps max {max(jumps):.0f} "
            f"wrap toggle {max(toggles):.0f} rss {metrics['oneline_rss_peak_mib']}")
        if app.poll() is not None:
            notes.append(f"oneline: app exited early with {app.returncode}")
    finally:
        if app.poll() is None:
            app.terminate()
            try:
                app.wait(5)
            except subprocess.TimeoutExpired:
                app.kill()
                app.wait()


def run(args):
    tmp = Path(args.tmp)
    art = Path(args.art or tmp)
    art.mkdir(parents=True, exist_ok=True)
    ws = tmp / "ws"
    ws.mkdir(parents=True, exist_ok=True)
    root = Path(__file__).resolve().parents[2]
    (ws / "notes.md").write_text("# Perf notes\n\nThe editor latency scene types below.\n\n")
    typ = ws / "multi-page.typ"
    typ.write_text((root / "samples/perf-scenes/multi-page.typ").read_text())
    big = tmp / "big.txt"
    if not big.exists() or big.stat().st_size < 20 * 1024 * 1024:
        line = b"line " + b"x" * 74 + b"\n"
        with open(big, "wb") as f:
            f.write(line * (20 * 1024 * 1024 // len(line) + 1))
    display = os.environ["DISPLAY"]
    x = X(display)
    metrics = {}
    notes = []

    applog = art / "app.log"
    blank = x.frame()
    t_launch = now_ms()
    app = subprocess.Popen(
        [args.bin, "--new-instance", str(typ), str(ws / "notes.md")],
        stdout=open(applog, "w"), stderr=subprocess.STDOUT, start_new_session=True,
    )
    try:
        t_first, _ = wait_change(x, blank, 20000, poll=0.002)
        if t_first is None:
            raise SystemExit("no frame within 20s")
        metrics["first_frame_ms"] = t_first - t_launch
        # Settled: the Typst pages are in and the screen has then held still
        # for 700 ms. Waiting for the pages first keeps a still moment
        # between the first paint and the preview from passing as settled.
        settled, h, last_change = None, x.frame(), now_ms()
        typst = None
        while now_ms() - t_launch < 30000:
            time.sleep(0.01)
            n, t = x.frame(), now_ms()
            if n != h:
                h, last_change = n, t
            if typst is None:
                typst = perf_mark_ms(applog, r"first_typst_preview_pages_applied")
            elif t - last_change >= 700:
                settled = last_change
                break
        metrics["typst_done_ms"] = typst
        metrics["settled_ms"] = (settled - t_launch) if settled else None
        log(f"first frame {metrics['first_frame_ms']:.0f}ms, settled {metrics['settled_ms']}")

        geo = window_geometry(display)
        if not geo:
            raise SystemExit("Malgel window not found")
        wid, wx, wy, ww, wh = geo
        # Everything above the status bar: it carries the memory/CPU pulse,
        # which changes on its own clock and would read as a fake response.
        x.region = (max(wx, 0), max(wy, 0), min(ww, x.width - wx), max(min(wh, x.height - wy) - 32, 32))
        env = dict(os.environ, DISPLAY=display)
        subprocess.run(["xdotool", "windowfocus", "--sync", wid], env=env, capture_output=True)
        x.pointer(wx + ww // 3, wy + wh // 2)
        time.sleep(0.5)

        # Editor input latency: Markdown tab (active), end of the file.
        x.chord("ctrl+End")
        time.sleep(0.4)
        samples, misses = typing_latency(x, "thequickbrownfoxjumpsoverthelazydogagain", x.region)
        metrics["editor_input_p50_ms"] = percentile(samples, 50)
        metrics["editor_input_p95_ms"] = percentile(samples, 95)
        if misses:
            notes.append(f"editor: {misses} keys never changed the frame")
        shot(art, "1-editor-typed")
        log(f"editor latency p50 {metrics['editor_input_p50_ms']} p95 {metrics['editor_input_p95_ms']}")

        # Terminal: summon, wait for the shell, then the composer.
        base = x.frame()
        x.chord("ctrl+grave")
        wait_change(x, base, 3000)
        wait_settle(x, 400, 5000)
        time.sleep(0.8)
        samples, misses = typing_latency(x, "echo composerlatencyprobe", x.region)
        metrics["terminal_input_p50_ms"] = percentile(samples, 50)
        metrics["terminal_input_p95_ms"] = percentile(samples, 95)
        if misses:
            notes.append(f"terminal: {misses} keys never changed the frame")
        shot(art, "2-composer-typed")
        x.chord("Return")
        wait_settle(x, 400, 5000)
        log(f"terminal latency p50 {metrics['terminal_input_p50_ms']} p95 {metrics['terminal_input_p95_ms']}")

        def throughput(cmd, done, timeout_s=60):
            if done.exists():
                done.unlink()
            x.type(cmd)
            time.sleep(0.2)
            t0 = now_ms()
            x.chord("Return")
            end = time.time() + timeout_s
            while not done.exists() and time.time() < end:
                time.sleep(0.01)
            if not done.exists():
                return None
            # The shell is done; the panel has caught up once frames stop.
            settled = wait_settle(x, 400, timeout_s * 1000)
            return (settled - t0) if settled else None

        metrics["terminal_seq_ms"] = throughput(f"seq 1 200000; touch {tmp}/seq.done", tmp / "seq.done")
        metrics["terminal_cat20mb_ms"] = throughput(f"cat {big}; touch {tmp}/cat.done", tmp / "cat.done", 120)
        shot(art, "3-after-cat")
        log(f"seq {metrics['terminal_seq_ms']} cat20MB {metrics['terminal_cat20mb_ms']}")

        # Ctrl+C: a foreground sleep with an argument nothing else uses.
        x.type("sleep 37.25")
        x.chord("Return")
        sleeper = None
        end = time.time() + 5
        while sleeper is None and time.time() < end:
            sleeper = find_proc(app.pid, "sleep 37.25")
            time.sleep(0.02)
        if sleeper is None:
            metrics["ctrl_c_to_prompt_ms"] = None
            notes.append("ctrl+c: sleep never started")
        else:
            time.sleep(0.3)
            base = x.frame()
            t0 = now_ms()
            x.chord("ctrl+c")
            gone = None
            limit = args.ctrl_c_timeout_ms
            while now_ms() - t0 < limit:
                if not Path(f"/proc/{sleeper}").exists() or find_proc(app.pid, "sleep 37.25") is None:
                    gone = now_ms()
                    break
                time.sleep(0.005)
            if gone is None:
                metrics["ctrl_c_to_prompt_ms"] = None
                notes.append(f"ctrl+c: sleep still alive after {limit}ms")
                try:
                    os.kill(sleeper, signal.SIGKILL)
                except OSError:
                    pass
            else:
                # The prompt is back once the transcript repaints after the exit.
                t_frame, _ = wait_change(x, base, 1000)
                metrics["ctrl_c_to_prompt_ms"] = max(gone, t_frame or gone) - t0
        shot(art, "4-after-ctrl-c")
        log(f"ctrl+c {metrics['ctrl_c_to_prompt_ms']}")

        # Back to the editor, then idle.
        x.chord("ctrl+grave")
        t_input = time.monotonic()
        time.sleep(2.5)
        # While the caret still blinks (informational: each blink is a
        # full-window repaint, which a software renderer pays for).
        metrics["idle_cpu_focused_blink_pct"] = idle_cpu(app.pid, args.idle_seconds)
        # Focused and idle: past the caret's blink timeout (10 s after the
        # last input, as GTK's gtk-cursor-blink-timeout) it rests on.
        rest = args.focused_settle_seconds - (time.monotonic() - t_input)
        if rest > 0:
            time.sleep(rest)
        metrics["idle_cpu_focused_pct"] = idle_cpu(app.pid, args.idle_seconds)
        x.steal_focus()
        time.sleep(2.5)
        metrics["idle_cpu_pct"] = idle_cpu(app.pid, args.idle_seconds)
        log(f"idle cpu blinking {metrics['idle_cpu_focused_blink_pct']:.1f}% focused {metrics['idle_cpu_focused_pct']:.1f}% background {metrics['idle_cpu_pct']:.1f}%")

        hwm = proc_status(app.pid, "VmHWM")
        rss = proc_status(app.pid, "VmRSS")
        metrics["rss_peak_mib"] = hwm / 1024.0 if hwm else None
        metrics["rss_final_mib"] = rss / 1024.0 if rss else None
        metrics["window_open_ms"] = perf_mark_ms(applog, r"open_window_ok")
        if app.poll() is not None:
            notes.append(f"app exited early with {app.returncode}")
    finally:
        if app.poll() is None:
            app.terminate()
            try:
                app.wait(5)
            except subprocess.TimeoutExpired:
                app.kill()
                app.wait()
    if not args.skip_oneline:
        oneline_scene(args, x, tmp, art, metrics, notes)
    metrics = {k: (round(v, 1) if isinstance(v, float) else v) for k, v in metrics.items()}
    out = {"metrics": metrics, "notes": notes}
    Path(args.out).write_text(json.dumps(out, indent=2))
    log(f"wrote {args.out}")


# ---------------------------------------------------------------------------
# Gate


def gate(args):
    runs = [json.loads(Path(p).read_text()) for p in args.runs]
    thresholds = json.loads(Path(args.thresholds).read_text())
    names = sorted({k for r in runs for k in r["metrics"]})
    medians, per_run = {}, {}
    for k in names:
        vals = [r["metrics"].get(k) for r in runs]
        per_run[k] = vals
        # A run that never saw the event (None) counts as a failure of it,
        # not as a missing sample to drop.
        present = [v for v in vals if v is not None]
        med = statistics.median(present) if len(present) == len(vals) and present else None
        medians[k] = round(med, 1) if med is not None else None
    strict = os.environ.get("PERF_STRICT") == "1"
    checks, failed, soft = [], [], []
    for k, rule in thresholds.items():
        if k.startswith("_"):
            continue
        v = medians.get(k)
        blocking = rule.get("blocking", True) or strict
        ok = v is not None and v <= rule["max"]
        checks.append({"metric": k, "median": v, "max": rule["max"], "ok": ok, "blocking": blocking})
        if not ok:
            (failed if blocking else soft).append(k)
    out = {
        "runs": len(runs),
        "median": medians,
        "per_run": per_run,
        "checks": checks,
        "notes": [n for r in runs for n in r.get("notes", [])],
        "result": "fail" if failed else "pass",
    }
    Path(args.out).write_text(json.dumps(out, indent=2))
    for c in checks:
        mark = "ok  " if c["ok"] else ("FAIL" if c["blocking"] else "warn")
        v = "missing" if c["median"] is None else f"{c['median']:.1f}"
        print(f"  {mark} {c['metric']:<24} median {v:>9}  max {c['max']}{'' if c['blocking'] else '  (non-blocking)'}")
    for n in out["notes"]:
        print(f"  note: {n}")
    if soft:
        print(f"non-blocking over budget: {', '.join(soft)}")
    if failed:
        print(f"PERF GATE FAILED: {', '.join(failed)}")
        return 1
    print("perf gate passed")
    return 0


def compare(args):
    a = json.loads(Path(args.a).read_text())["median"]
    b = json.loads(Path(args.b).read_text())["median"]
    bad = 0
    for k in sorted(set(a) | set(b)):
        va, vb = a.get(k), b.get(k)
        if va is None or vb is None:
            print(f"  {k:<24} {va} vs {vb}  (missing)")
            continue
        base = max(abs(va), 1e-9)
        delta = (vb - va) / base
        # Tiny absolute values (idle CPU ~1%) swing in relative terms on noise.
        within = abs(delta) <= args.tolerance or abs(vb - va) <= args.floor.get(k, 0)
        bad += not within
        print(f"  {'ok  ' if within else 'OUT '} {k:<24} {va:9.1f} -> {vb:9.1f}  {delta*100:+6.1f}%")
    return 1 if bad else 0


def main():
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--bin", required=True)
    r.add_argument("--tmp", required=True)
    r.add_argument("--art")
    r.add_argument("--out", required=True)
    r.add_argument("--idle-seconds", type=float, default=5.0)
    r.add_argument("--focused-settle-seconds", type=float, default=11.0)
    r.add_argument("--ctrl-c-timeout-ms", type=float, default=3000.0)
    r.add_argument("--skip-oneline", action="store_true")
    o = sub.add_parser("oneline", help="only the 10 MB one-line scene")
    o.add_argument("--bin", required=True)
    o.add_argument("--tmp", required=True)
    o.add_argument("--art")
    o.add_argument("--out", required=True)
    g = sub.add_parser("gate")
    g.add_argument("--thresholds", required=True)
    g.add_argument("--out", required=True)
    g.add_argument("runs", nargs="+")
    c = sub.add_parser("compare")
    c.add_argument("a")
    c.add_argument("b")
    c.add_argument("--tolerance", type=float, default=0.15)
    args = p.parse_args()
    if args.cmd == "run":
        run(args)
        return 0
    if args.cmd == "oneline":
        tmp = Path(args.tmp)
        art = Path(args.art or tmp)
        art.mkdir(parents=True, exist_ok=True)
        metrics, notes = {}, []
        oneline_scene(args, X(os.environ["DISPLAY"]), tmp, art, metrics, notes)
        metrics = {k: (round(v, 1) if isinstance(v, float) else v) for k, v in metrics.items()}
        Path(args.out).write_text(json.dumps({"metrics": metrics, "notes": notes}, indent=2))
        return 0
    if args.cmd == "gate":
        return gate(args)
    # Absolute slack per metric for compare, where ±15% of a tiny number is noise.
    args.floor = {"idle_cpu_pct": 2.0, "idle_cpu_focused_pct": 2.0, "idle_cpu_focused_blink_pct": 3.0}
    return compare(args)


if __name__ == "__main__":
    sys.exit(main())
