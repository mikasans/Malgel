//! Process RSS / wall-clock probes for `MALGEL_PERF=1` runs, plus a quiet
//! status-bar pulse (memory · CPU) sampled every ~1.5s on one background thread.

use std::{
    sync::{
        Mutex, Once, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

static ENABLED: AtomicBool = AtomicBool::new(false);
static START: OnceLock<Instant> = OnceLock::new();

/// Call once near process entry. Later [`mark`] lines are deltas from here.
pub fn init() {
    let on = std::env::var_os("MALGEL_PERF").is_some_and(|v| v != "0");
    ENABLED.store(on, Ordering::Relaxed);
    let _ = START.set(Instant::now());
    if on {
        mark("process_start");
    }
    // Status-bar pulse is always on; cheap and one thread for the process.
    ensure_pulse();
}

/// Linux VmRSS in KiB, or `None` when unavailable.
pub fn rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kib);
        }
    }
    None
}

fn since_ms() -> f64 {
    START
        .get()
        .map(|t0| t0.elapsed().as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

fn rss_label() -> String {
    rss_kib()
        .map(|k| format!("{:.1}MiB", k as f64 / 1024.0))
        .unwrap_or_else(|| "?".into())
}

/// Emit `malgel-perf: <label> +X.Xms rss=YMiB` when enabled.
pub fn mark(label: &str) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "malgel-perf: {label} +{:.1}ms rss={}",
        since_ms(),
        rss_label()
    );
}

/// Timed span that logs duration and RSS on drop when enabled.
pub struct Span {
    label: &'static str,
    started: Instant,
    detail: Option<String>,
}

impl Span {
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            started: Instant::now(),
            detail: None,
        }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if !ENABLED.load(Ordering::Relaxed) {
            return;
        }
        let ms = self.started.elapsed().as_secs_f64() * 1000.0;
        match &self.detail {
            Some(d) => eprintln!(
                "malgel-perf: {} {ms:.1}ms (at +{:.1}ms) rss={} ({d})",
                self.label,
                since_ms(),
                rss_label()
            ),
            None => eprintln!(
                "malgel-perf: {} {ms:.1}ms (at +{:.1}ms) rss={}",
                self.label,
                since_ms(),
                rss_label()
            ),
        }
    }
}

/// Longest wait of the UI thread for a terminal's lock since the last
/// ui_lag line, in microseconds.
static TERM_LOCK_WAIT_MAX_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn note_term_lock_wait(wait: Duration) {
    if ENABLED.load(Ordering::Relaxed) {
        TERM_LOCK_WAIT_MAX_US.fetch_max(wait.as_micros() as u64, Ordering::Relaxed);
    }
}

/// `MALGEL_PERF=1` only: a foreground heartbeat that measures how late the
/// main thread wakes for a 10ms timer, logged once a second as
/// `malgel-perf: ui_lag max=..ms over50=N ticks=N`. A stalled UI thread (a
/// flooded terminal parsing on it, a blocking PTY write) shows up as a large
/// `max` — the number the terminal stress script asserts on.
pub fn start_ui_lag_probe(cx: &mut gpui_kit::App) {
    if !enabled() {
        return;
    }
    cx.spawn(async move |cx| {
        const TICK: Duration = Duration::from_millis(10);
        let mut window_start = Instant::now();
        let mut max_lag = Duration::ZERO;
        let mut over_50 = 0u32;
        let mut ticks = 0u32;
        loop {
            let before = Instant::now();
            cx.background_executor().timer(TICK).await;
            // Resumes on the main thread: lateness is time the UI was busy.
            let lag = before.elapsed().saturating_sub(TICK);
            ticks += 1;
            max_lag = max_lag.max(lag);
            if lag > Duration::from_millis(50) {
                over_50 += 1;
                if lag > Duration::from_millis(200) {
                    eprintln!(
                        "malgel-perf: stall {:.0}ms ending at +{:.1}ms",
                        lag.as_secs_f64() * 1000.0,
                        since_ms()
                    );
                }
            }
            if window_start.elapsed() >= Duration::from_secs(1) {
                let lock_us = TERM_LOCK_WAIT_MAX_US.swap(0, Ordering::Relaxed);
                eprintln!(
                    "malgel-perf: ui_lag max={:.1}ms over50={over_50} ticks={ticks} frames={} term_paints={} \
                     term_lock_wait={:.1}ms (at +{:.1}ms) rss={}",
                    max_lag.as_secs_f64() * 1000.0,
                    FRAMES.swap(0, Ordering::Relaxed),
                    TERM_PAINTS.swap(0, Ordering::Relaxed),
                    lock_us as f64 / 1000.0,
                    since_ms(),
                    rss_label()
                );
                window_start = Instant::now();
                max_lag = Duration::ZERO;
                over_50 = 0;
                ticks = 0;
            }
        }
    })
    .detach();
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Hand memory the allocator holds but no longer uses back to the system,
/// after a burst such as a folder search (its file list and the text of
/// every file it read) — otherwise RSS stays at the burst's high-water mark.
pub fn release_free_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: glibc's malloc_trim only returns free heap pages to the
        // kernel; it touches no live allocation.
        unsafe {
            malloc_trim(0);
        }
    }
}

// Workspace renders since the last ui_lag line.
static FRAMES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
// Terminal paints (new output shown) since the last ui_lag line.
static TERM_PAINTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn note_term_paint() {
    if ENABLED.load(Ordering::Relaxed) {
        TERM_PAINTS.fetch_add(1, Ordering::Relaxed);
    }
}

/// `MALGEL_PERF_INJECT_SLEEP_MS=N` (only with `MALGEL_PERF=1`): stall every
/// workspace frame by N ms. A deliberate regression, so the perf gate
/// (scripts/e2e/perf_scenes.sh) can be shown to trip; never set otherwise.
/// Also counts frames for the ui_lag line.
pub fn injected_frame_delay() {
    if ENABLED.load(Ordering::Relaxed) {
        FRAMES.fetch_add(1, Ordering::Relaxed);
    }
    static DELAY: OnceLock<Option<Duration>> = OnceLock::new();
    let delay = DELAY.get_or_init(|| {
        if !enabled() {
            return None;
        }
        parse_inject_ms(std::env::var("MALGEL_PERF_INJECT_SLEEP_MS").ok().as_deref())
    });
    if let Some(delay) = delay {
        thread::sleep(*delay);
    }
}

fn parse_inject_ms(value: Option<&str>) -> Option<Duration> {
    let ms: u64 = value?.trim().parse().ok()?;
    (ms > 0).then(|| Duration::from_millis(ms.min(5_000)))
}

// ---------------------------------------------------------------------------
// Quiet status-bar pulse (RSS · CPU). One background thread; 1.5s sample.
// ---------------------------------------------------------------------------

const PULSE_INTERVAL: Duration = Duration::from_millis(1500);

struct PulseState {
    label: String,
    /// Generation bumped each sample so the UI can skip redundant notify.
    generation: u64,
    /// The CPU step and RSS the label shows, kept while a new sample stays
    /// near them (see [`hold_cpu`], [`hold_rss`]).
    cpu_shown: Option<u32>,
    rss_shown: Option<u64>,
}

static PULSE_ONCE: Once = Once::new();
static PULSE: Mutex<PulseState> = Mutex::new(PulseState {
    label: String::new(),
    generation: 0,
    cpu_shown: None,
    rss_shown: None,
});

/// Ensure the quiet sampler thread is running. Safe to call many times.
pub fn ensure_pulse() {
    PULSE_ONCE.call_once(|| {
        // Seed immediately so the first status-bar paint is not blank.
        refresh_pulse();
        let _ = thread::Builder::new()
            .name("malgel-pulse".into())
            .spawn(|| {
                loop {
                    thread::sleep(PULSE_INTERVAL);
                    refresh_pulse();
                }
            });
    });
}

/// Compact label for the status bar, e.g. `128M · 12%`. Empty before first sample.
pub fn pulse_label() -> String {
    ensure_pulse();
    PULSE.lock().map(|s| s.label.clone()).unwrap_or_default()
}

/// `(label, generation)` — generation advances every sample.
pub fn pulse_snapshot() -> (String, u64) {
    ensure_pulse();
    PULSE
        .lock()
        .map(|s| (s.label.clone(), s.generation))
        .unwrap_or_default()
}

fn refresh_pulse() {
    let (rss_kib, cpu_pct) = (rss_kib(), cpu_percent());
    let Ok(mut state) = PULSE.lock() else {
        return;
    };
    state.rss_shown = rss_kib.map(|kib| hold_rss(state.rss_shown, kib));
    state.cpu_shown = cpu_pct.map(|pct| hold_cpu(state.cpu_shown, pct));
    let rss = format_rss_compact(state.rss_shown);
    let cpu = state.cpu_shown.map(|n| format!("{n}%"));
    let label = match (rss, cpu) {
        (Some(r), Some(c)) => format!("{r} · {c}"),
        (Some(r), None) => r,
        (None, Some(c)) => c,
        (None, None) => String::new(),
    };
    // Only advance generation when the label changes — otherwise the
    // status-bar task wakes every 1.5s and forces a full UI repaint for
    // an identical "128M · 12%" string.
    if state.label != label {
        state.label = label;
        state.generation = state.generation.wrapping_add(1);
    }
}

/// The CPU figure to show for a sample of `pct`: the 5% step it rounds to,
/// unless the figure shown is less than a whole step away. Without the
/// hold an idle window sitting near 2.5% flipped between "0%" and "5%"
/// every sample, and each flip was a full repaint, which on a software
/// renderer is itself the CPU being measured.
fn hold_cpu(shown: Option<u32>, pct: f32) -> u32 {
    match shown {
        Some(shown) if (pct - shown as f32).abs() < 5.0 => shown,
        _ => ((pct / 5.0).round() * 5.0).clamp(0.0, 995.0) as u32,
    }
}

/// The RSS to show for a sample of `kib`: the one shown until it moves by
/// 2 MiB, so allocator noise does not repaint the window.
fn hold_rss(shown: Option<u64>, kib: u64) -> u64 {
    match shown {
        Some(shown) if shown.abs_diff(kib) < 2 * 1024 => shown,
        _ => kib,
    }
}

fn format_rss_compact(kib: Option<u64>) -> Option<String> {
    let kib = kib?;
    if kib >= 1024 * 1024 {
        Some(format!("{:.1}G", kib as f64 / (1024.0 * 1024.0)))
    } else if kib >= 1024 {
        let mib = (kib as f64 / 1024.0).round();
        Some(format!("{mib:.0}M"))
    } else {
        Some(format!("{kib}K"))
    }
}

/// Process CPU percent of one core since the previous call (or `None` first time).
fn cpu_percent() -> Option<f32> {
    static PREV: Mutex<Option<(u64, Instant)>> = Mutex::new(None);
    let ticks = process_cpu_ticks()?;
    let now = Instant::now();
    let mut prev = PREV.lock().ok()?;
    let result = if let Some((prev_ticks, prev_at)) = *prev {
        let dt = now.duration_since(prev_at).as_secs_f32();
        if dt < 0.05 {
            None
        } else {
            // Linux CLK_TCK is almost always 100; avoid a libc dependency.
            let tick_hz = 100.0_f32;
            let delta = ticks.saturating_sub(prev_ticks) as f32;
            Some((delta / tick_hz) / dt * 100.0)
        }
    } else {
        None
    };
    *prev = Some((ticks, now));
    result
}

/// `utime + stime` from `/proc/self/stat` (fields 14 and 15, 1-based).
fn process_cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Comm can contain spaces/parens; find the last `)` then split the rest.
    let after = stat.rsplit(')').next()?;
    let parts: Vec<&str> = after.split_whitespace().collect();
    // After `)`: state(1) … utime is index 11, stime index 12 (0-based in this slice).
    if parts.len() < 13 {
        return None;
    }
    let utime: u64 = parts[11].parse().ok()?;
    let stime: u64 = parts[12].parse().ok()?;
    Some(utime + stime)
}

#[cfg(test)]
mod pulse_tests {
    use super::*;

    #[test]
    fn formats_rss_compactly() {
        assert_eq!(format_rss_compact(Some(512)), Some("512K".into()));
        assert_eq!(format_rss_compact(Some(128 * 1024)), Some("128M".into()));
        assert_eq!(format_rss_compact(Some(1536 * 1024)), Some("1.5G".into()));
    }

    #[test]
    fn injected_delay_parses_only_positive_millis() {
        assert_eq!(parse_inject_ms(None), None);
        assert_eq!(parse_inject_ms(Some("0")), None);
        assert_eq!(parse_inject_ms(Some("x")), None);
        assert_eq!(
            parse_inject_ms(Some(" 120 ")),
            Some(Duration::from_millis(120))
        );
        assert_eq!(
            parse_inject_ms(Some("99999")),
            Some(Duration::from_millis(5_000))
        );
    }

    #[test]
    fn cpu_is_shown_in_steps_of_five_and_held_near_the_last_step() {
        // One-core percent in steps of 5.
        assert_eq!(hold_cpu(None, 12.4), 10);
        assert_eq!(hold_cpu(None, 13.0), 15);
        assert_eq!(hold_cpu(None, 1.9), 0);
        // Idle noise around 2.5% no longer flips the label (and repaints).
        assert_eq!(hold_cpu(Some(5), 2.0), 5);
        assert_eq!(hold_cpu(Some(5), 0.4), 5);
        assert_eq!(hold_cpu(Some(0), 3.9), 0);
        // A real change moves it.
        assert_eq!(hold_cpu(Some(5), 12.4), 10);
        assert_eq!(hold_cpu(Some(0), 5.0), 5);
        assert_eq!(hold_cpu(Some(20), 0.0), 0);
    }

    #[test]
    fn rss_is_held_until_it_moves_two_mebibytes() {
        assert_eq!(hold_rss(None, 200_000), 200_000);
        assert_eq!(hold_rss(Some(200_000), 201_000), 200_000);
        assert_eq!(hold_rss(Some(200_000), 199_000), 200_000);
        assert_eq!(hold_rss(Some(200_000), 203_000), 203_000);
    }

    // RSS comes from /proc/self/status, so the pulse label is only populated on Linux.
    #[cfg(target_os = "linux")]
    #[test]
    fn pulse_sampler_produces_a_label() {
        ensure_pulse();
        std::thread::sleep(Duration::from_millis(50));
        refresh_pulse();
        std::thread::sleep(Duration::from_millis(200));
        refresh_pulse();
        let label = pulse_label();
        assert!(
            label.contains('M') || label.contains('K') || label.contains('G'),
            "expected RSS in label, got {label:?}"
        );
    }

    #[test]
    fn pulse_generation_stays_put_when_label_unchanged() {
        ensure_pulse();
        let pinned = "128M · 0%".to_string();
        if let Ok(mut state) = PULSE.lock() {
            state.label = pinned.clone();
        }
        let (_, before) = pulse_snapshot();
        // Same label re-applied: generation must not advance (mirrors refresh_pulse).
        if let Ok(mut state) = PULSE.lock()
            && state.label != pinned
        {
            state.label = pinned.clone();
            state.generation = state.generation.wrapping_add(1);
        }
        let (label, after) = pulse_snapshot();
        assert_eq!(label, pinned);
        assert_eq!(after, before, "identical label must not bump generation");
    }
}
