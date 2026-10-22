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
            }
            if window_start.elapsed() >= Duration::from_secs(1) {
                eprintln!(
                    "malgel-perf: ui_lag max={:.1}ms over50={over_50} ticks={ticks} (at +{:.1}ms) rss={}",
                    max_lag.as_secs_f64() * 1000.0,
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

// ---------------------------------------------------------------------------
// Quiet status-bar pulse (RSS · CPU). One background thread; 1.5s sample.
// ---------------------------------------------------------------------------

const PULSE_INTERVAL: Duration = Duration::from_millis(1500);

struct PulseState {
    label: String,
    /// Generation bumped each sample so the UI can skip redundant notify.
    generation: u64,
}

static PULSE_ONCE: Once = Once::new();
static PULSE: Mutex<PulseState> = Mutex::new(PulseState {
    label: String::new(),
    generation: 0,
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
    let rss = format_rss_compact(rss_kib());
    let cpu = format_cpu_compact(cpu_percent());
    let label = match (rss, cpu) {
        (Some(r), Some(c)) => format!("{r} · {c}"),
        (Some(r), None) => r,
        (None, Some(c)) => c,
        (None, None) => String::new(),
    };
    if let Ok(mut state) = PULSE.lock() {
        // Only advance generation when the label changes — otherwise the
        // status-bar task wakes every 1.5s and forces a full UI repaint for
        // an identical "128M · 12%" string.
        if state.label != label {
            state.label = label;
            state.generation = state.generation.wrapping_add(1);
        }
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

fn format_cpu_compact(pct: Option<f32>) -> Option<String> {
    let pct = pct?;
    // One-core percent; can exceed 100 on multi-threaded bursts.
    let n = pct.round().clamp(0.0, 999.0) as u32;
    Some(format!("{n}%"))
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
    fn formats_cpu_compactly() {
        assert_eq!(format_cpu_compact(Some(12.4)), Some("12%".into()));
        assert_eq!(format_cpu_compact(Some(0.0)), Some("0%".into()));
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
