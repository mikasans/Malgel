//! Process RSS / wall-clock probes for `MALGEL_PERF=1` runs.
//!
//! Answers where heap and CPU went across startup, first Typst preview, and
//! warm reuse. Silent unless the env var is set.

use std::{
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
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

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}
