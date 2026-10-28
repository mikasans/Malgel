//! A size-rotated log file: `malgel.log`, rolled to `malgel.log.1` …
//! `.N` when it passes a size, the oldest deleted. Warnings, errors and
//! crash reports land here so a user can send them; stderr still gets
//! everything.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::Mutex,
};

/// Roll the log past this size (5 MiB).
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// Rolled files kept beside the live one (`.1` … `.3`).
pub const KEEP_ROLLED: usize = 3;

/// An append-only log that rotates by size.
pub struct RotatingLog {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    state: Mutex<Option<(File, u64)>>,
}

impl RotatingLog {
    pub fn new(path: PathBuf, max_bytes: u64, keep: usize) -> Self {
        Self {
            path,
            max_bytes: max_bytes.max(1),
            keep,
            state: Mutex::new(None),
        }
    }

    /// The app's log in the config folder (`logs/malgel.log`).
    pub fn app() -> Option<&'static RotatingLog> {
        static LOG: std::sync::OnceLock<Option<RotatingLog>> = std::sync::OnceLock::new();
        LOG.get_or_init(|| {
            let dir = crate::settings::config_dir()?.join("logs");
            Some(RotatingLog::new(
                dir.join("malgel.log"),
                MAX_LOG_BYTES,
                KEEP_ROLLED,
            ))
        })
        .as_ref()
    }

    fn rolled(&self, n: usize) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(format!(".{n}"));
        PathBuf::from(name)
    }

    /// Shift `.N-1` → `.N` … live → `.1`; what falls off the end is deleted.
    fn rotate(&self) {
        if self.keep == 0 {
            let _ = fs::remove_file(&self.path);
            return;
        }
        let _ = fs::remove_file(self.rolled(self.keep));
        for n in (1..self.keep).rev() {
            let _ = fs::rename(self.rolled(n), self.rolled(n + 1));
        }
        let _ = fs::rename(&self.path, self.rolled(1));
    }

    fn open(&self) -> Option<(File, u64)> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).ok()?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok()?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        Some((file, len))
    }

    /// Append one line (a newline is added). Never panics; a log that
    /// cannot be written is skipped. `try_lock` so a panic while logging
    /// (the hook logs too) cannot deadlock.
    pub fn write_line(&self, line: &str) {
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };
        if state.is_none() {
            *state = self.open();
        }
        let needed = line.len() as u64 + 1;
        if let Some((_, len)) = state.as_ref()
            && *len > 0
            && *len + needed > self.max_bytes
        {
            *state = None;
            self.rotate();
            *state = self.open();
        }
        if let Some((file, len)) = state.as_mut() {
            // One line longer than the whole budget is cut, not written
            // whole: the log stays within its size.
            let max = self.max_bytes.saturating_sub(1) as usize;
            let mut cut = line.len().min(max);
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            if writeln!(file, "{}", &line[..cut]).is_ok() {
                *len += cut as u64 + 1;
            }
        }
    }
}

/// Seconds since the Unix epoch, for log lines (no date crate here).
pub fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!("t={secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "malgel-log-{tag}-{}",
            crate::session::new_recovery_id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rotates_by_size_and_keeps_n_files() {
        let dir = temp_dir("rot");
        let log = RotatingLog::new(dir.join("m.log"), 100, 3);
        // 30 lines of 20 bytes: 600 bytes through a 100-byte log.
        for i in 0..30 {
            log.write_line(&format!("line {i:02} xxxxxxxxxx"));
        }
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["m.log", "m.log.1", "m.log.2", "m.log.3"]);
        let total: u64 = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        assert!(total <= 4 * 100, "{total} bytes");
        for n in ["m.log", "m.log.1", "m.log.2", "m.log.3"] {
            assert!(fs::metadata(dir.join(n)).unwrap().len() <= 100, "{n}");
        }
        // The newest line is live, the oldest ones were deleted.
        let live = fs::read_to_string(dir.join("m.log")).unwrap();
        assert!(live.ends_with("line 29 xxxxxxxxxx\n"), "{live}");
        let all: String = ["m.log.3", "m.log.2", "m.log.1", "m.log"]
            .iter()
            .map(|n| fs::read_to_string(dir.join(n)).unwrap())
            .collect();
        assert!(!all.contains("line 00"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_oversized_line_is_cut_at_a_char_boundary() {
        let dir = temp_dir("cut");
        let log = RotatingLog::new(dir.join("m.log"), 10, 1);
        log.write_line("标题标题标题标题");
        let live = fs::read_to_string(dir.join("m.log")).unwrap();
        assert!(live.len() <= 10, "{}", live.len());
        assert!(live.starts_with("标题"));
        let _ = fs::remove_dir_all(dir);
    }
}
