//! What survives quitting and crashing: the last session (open documents,
//! folder and window) and unsaved changes written aside while editing.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    document::{self, LineEnding},
    settings::config_dir,
};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub documents: Vec<SessionDocument>,
    /// Index of the document that was showing.
    pub active: usize,
    /// Folder shown in the file sidebar.
    pub folder: Option<PathBuf>,
    pub window: Option<WindowPlacement>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionDocument {
    pub path: Option<PathBuf>,
    /// Caret position, a byte offset.
    pub cursor: usize,
    /// Recovery file holding unsaved changes, if there were any.
    pub recovery: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowPlacement {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
}

impl Session {
    /// The last session, or `None` on first run.
    pub fn load() -> Option<Self> {
        let json = std::fs::read_to_string(session_path()?).ok()?;
        Some(serde_json::from_str(&json).unwrap_or_default())
    }

    pub fn save(&self) {
        let Some(path) = session_path() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = document::save(&path, &json, LineEnding::Lf);
        }
    }
}

fn session_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("session.json"))
}

/// Unsaved changes to one document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recovery {
    /// Where the document is saved, if it ever was.
    pub path: Option<PathBuf>,
    pub text: String,
    pub crlf: bool,
    /// `content_hash` of the file's text as last loaded or saved — what the
    /// unsaved changes were made on top of. On restore, a file whose text
    /// no longer hashes to this was changed on disk after the crash, and
    /// the recovered text must not silently replace it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_hash: Option<u64>,
}

/// Whether restoring `recovery` over the file now on disk (whose text
/// hashes to `disk_hash`, `None` when it cannot be read) would replace
/// changes made after the recovered edits began.
pub fn recovery_conflicts(recovery: &Recovery, disk_hash: Option<u64>) -> bool {
    match (recovery.base_hash, disk_hash) {
        (Some(base), Some(disk)) => base != disk,
        // No base recorded (older recovery) or no file: nothing to compare.
        _ => false,
    }
}

pub fn recovery_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("recovery"))
}

fn recovery_path(id: &str) -> Option<PathBuf> {
    recovery_file(&recovery_dir()?, id)
}

fn recovery_file(dir: &Path, id: &str) -> Option<PathBuf> {
    // Ids are generated here; refuse anything that could leave the folder.
    if id.is_empty() || !id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-') {
        return None;
    }
    Some(dir.join(format!("{id}.json")))
}

/// A new, unique recovery id.
pub fn new_recovery_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!(
        "{nanos:x}-{:x}-{:x}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

pub fn write_recovery(id: &str, recovery: &Recovery) -> std::io::Result<()> {
    let dir = recovery_dir().ok_or_else(|| std::io::Error::other("no config folder"))?;
    write_recovery_in(&dir, id, recovery)
}

pub fn write_recovery_in(dir: &Path, id: &str, recovery: &Recovery) -> std::io::Result<()> {
    let path = recovery_file(dir, id).ok_or_else(|| std::io::Error::other("bad recovery id"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string(recovery).map_err(std::io::Error::other)?;
    document::save(&path, &json, LineEnding::Lf)
}

/// Like `write_recovery`, with the text given as chunks (a rope's) that are
/// escaped into the file one by one, so a large document is never held as
/// one string. `recovery.text` is ignored.
pub fn write_recovery_chunks<'a>(
    id: &str,
    recovery: &Recovery,
    chunks: impl IntoIterator<Item = &'a str>,
) -> std::io::Result<()> {
    let dir = recovery_dir().ok_or_else(|| std::io::Error::other("no config folder"))?;
    write_recovery_chunks_in(&dir, id, recovery, chunks)
}

pub fn write_recovery_chunks_in<'a>(
    dir: &Path,
    id: &str,
    recovery: &Recovery,
    chunks: impl IntoIterator<Item = &'a str>,
) -> std::io::Result<()> {
    let path = recovery_file(dir, id).ok_or_else(|| std::io::Error::other("bad recovery id"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = path.with_extension("partial");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&partial)?);
    recovery_json(&mut out, recovery, chunks)?;
    out.into_inner()
        .map_err(|err| err.into_error())?
        .sync_all()?;
    std::fs::rename(partial, path)
}

fn recovery_json<'a>(
    out: &mut impl std::io::Write,
    recovery: &Recovery,
    chunks: impl IntoIterator<Item = &'a str>,
) -> std::io::Result<()> {
    let path_json = serde_json::to_string(&recovery.path).map_err(std::io::Error::other)?;
    write!(
        out,
        "{{\"path\":{path_json},\"crlf\":{},\"text\":\"",
        recovery.crlf
    )?;
    for chunk in chunks {
        let quoted = serde_json::to_string(chunk).map_err(std::io::Error::other)?;
        out.write_all(&quoted.as_bytes()[1..quoted.len() - 1])?;
    }
    out.write_all(b"\"}")
}

pub fn read_recovery(id: &str) -> Option<Recovery> {
    let json = std::fs::read_to_string(recovery_path(id)?).ok()?;
    serde_json::from_str(&json).ok()
}

pub fn remove_recovery(id: &str) {
    if let Some(path) = recovery_path(id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Recovery files no session document refers to: left by a crash before
/// the session was written, or by another window.
pub fn orphaned_recoveries(known: &HashSet<String>) -> Vec<(String, Recovery)> {
    let Some(dir) = recovery_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(String, Recovery)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let id = path.file_stem()?.to_str()?.to_string();
            (path.extension()? == "json" && !known.contains(&id))
                .then(|| read_recovery(&id).map(|recovery| (id, recovery)))
                .flatten()
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Limits on the recovery folder, applied at startup.
pub struct RecoveryLimits {
    /// Unreferenced recovery files older than this are deleted.
    pub max_age: std::time::Duration,
    /// At most this many recovery files are kept…
    pub max_files: usize,
    /// …taking at most this many bytes together.
    pub max_bytes: u64,
}

impl RecoveryLimits {
    pub const DEFAULT: RecoveryLimits = RecoveryLimits {
        max_age: std::time::Duration::from_secs(90 * 24 * 3600),
        max_files: 100,
        max_bytes: 512 * 1024 * 1024,
    };
}

/// Clean the recovery folder: half-written `.partial` files and anything
/// that is not a recovery go first, then unreferenced recoveries past
/// `max_age`, then the oldest unreferenced ones until the count and size
/// fit. Recoveries the last session refers to (`keep`) are never deleted.
/// Returns the ids removed.
pub fn prune_recoveries(
    dir: &Path,
    keep: &HashSet<String>,
    limits: &RecoveryLimits,
    now: SystemTime,
) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    // (modified, id, bytes, path) of recoveries that may go.
    let mut candidates = Vec::new();
    let mut kept_files = 0usize;
    let mut kept_bytes = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(now);
        let age = now.duration_since(modified).unwrap_or_default();
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let is_recovery = path.extension().is_some_and(|e| e == "json");
        if !is_recovery {
            // A write cut short (`.partial`) is useless once it is not
            // being written any more.
            if age > std::time::Duration::from_secs(3600) {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }
        if keep.contains(&stem) {
            kept_files += 1;
            kept_bytes += meta.len();
            continue;
        }
        if age > limits.max_age {
            if std::fs::remove_file(&path).is_ok() {
                removed.push(stem);
            }
            continue;
        }
        candidates.push((modified, stem, meta.len(), path));
    }
    // Newest first; drop from the end (oldest) while over the limits.
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0));
    let mut files = kept_files + candidates.len();
    let mut bytes = kept_bytes + candidates.iter().map(|c| c.2).sum::<u64>();
    while (files > limits.max_files || bytes > limits.max_bytes)
        && let Some((_, id, len, path)) = candidates.pop()
    {
        if std::fs::remove_file(&path).is_ok() {
            removed.push(id);
        }
        files -= 1;
        bytes -= len;
    }
    removed
}

/// Modification time and length of a file, to notice changes made by other
/// programs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskStamp {
    pub modified: Option<SystemTime>,
    pub len: u64,
}

impl DiskStamp {
    /// A stamp no file has: the document's idea of the file is out of date
    /// and the next check or save must compare contents.
    pub const UNKNOWN: DiskStamp = DiskStamp {
        modified: None,
        len: u64::MAX,
    };

    pub fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streamed_recovery_reads_back_as_the_same_text() {
        let text = "line \"one\"\n\ttab \\ 标题\u{1}";
        let recovery = Recovery {
            path: Some(PathBuf::from("/tmp/a b.md")),
            text: String::new(),
            crlf: true,
            base_hash: None,
        };
        let mut out = Vec::new();
        let chunks = ["line \"one\"\n", "\ttab \\ ", "标题\u{1}"];
        recovery_json(&mut out, &recovery, chunks).unwrap();
        let back: Recovery = serde_json::from_slice(&out).unwrap();
        assert_eq!(back.text, text);
        assert_eq!(back.path, recovery.path);
        assert!(back.crlf);
    }

    #[test]
    fn a_recovery_conflicts_only_when_the_file_changed_after_its_base() {
        let rec = |base| Recovery {
            path: Some(PathBuf::from("/tmp/x.md")),
            text: "recovered".into(),
            crlf: false,
            base_hash: base,
        };
        assert!(recovery_conflicts(&rec(Some(1)), Some(2)), "disk moved on");
        assert!(
            !recovery_conflicts(&rec(Some(1)), Some(1)),
            "disk as it was"
        );
        assert!(
            !recovery_conflicts(&rec(None), Some(2)),
            "old recovery: as before"
        );
        assert!(
            !recovery_conflicts(&rec(Some(1)), None),
            "file gone: as before"
        );
        // The base survives the round trip through JSON; an old file
        // without it still reads.
        let json = serde_json::to_string(&rec(Some(u64::MAX))).unwrap();
        assert_eq!(
            serde_json::from_str::<Recovery>(&json).unwrap().base_hash,
            Some(u64::MAX)
        );
        let old: Recovery =
            serde_json::from_str(r#"{"path":null,"text":"t","crlf":false}"#).unwrap();
        assert_eq!(old.base_hash, None);
    }

    #[test]
    fn recovery_folder_is_capped_by_age_count_and_size() {
        let dir = std::env::temp_dir().join(format!("malgel-prune-{}", new_recovery_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rec = |text: &str| Recovery {
            path: None,
            text: text.into(),
            crlf: false,
            base_hash: None,
        };
        for i in 0..6 {
            write_recovery_in(&dir, &format!("r{i}"), &rec(&"x".repeat(100))).unwrap();
            // Distinct mtimes, r0 oldest.
            let t = SystemTime::now() - std::time::Duration::from_secs(600 - i * 60);
            std::fs::File::options()
                .write(true)
                .open(dir.join(format!("r{i}.json")))
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        std::fs::write(dir.join("r9.partial"), "half").unwrap();
        let old = SystemTime::now() - std::time::Duration::from_secs(7200);
        std::fs::File::options()
            .write(true)
            .open(dir.join("r9.partial"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let keep = HashSet::from(["r0".to_string()]);
        let now = SystemTime::now();
        // Age: older than 8 minutes goes (r0 is kept: the session uses it).
        let limits = RecoveryLimits {
            max_age: std::time::Duration::from_secs(8 * 60),
            max_files: 100,
            max_bytes: u64::MAX,
        };
        let mut gone = prune_recoveries(&dir, &keep, &limits, now);
        gone.sort();
        assert_eq!(gone, ["r1", "r2"]);
        assert!(!dir.join("r9.partial").exists());
        assert!(dir.join("r0.json").exists());
        // Count: 4 left (r0, r3, r4, r5), cap 3 → the oldest unkept (r3).
        let limits = RecoveryLimits {
            max_files: 3,
            ..limits
        };
        assert_eq!(prune_recoveries(&dir, &keep, &limits, now), ["r3"]);
        // Size: room for two files → r4 goes, r5 (newest) and r0 stay.
        let one = std::fs::metadata(dir.join("r5.json")).unwrap().len();
        let limits = RecoveryLimits {
            max_bytes: one * 2,
            ..limits
        };
        assert_eq!(prune_recoveries(&dir, &keep, &limits, now), ["r4"]);
        assert!(dir.join("r0.json").exists() && dir.join("r5.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn round_trips_sessions_and_recovery() {
        let dir = std::env::temp_dir().join(format!("malgel-session-{}", new_recovery_id()));
        // SAFETY: tests touching the config folder run in this one test.
        unsafe { std::env::set_var("MALGEL_CONFIG_DIR", &dir) };

        assert_eq!(Session::load(), None);
        let session = Session {
            documents: vec![SessionDocument {
                path: Some(PathBuf::from("/tmp/a.md")),
                cursor: 12,
                recovery: None,
            }],
            active: 0,
            folder: Some(PathBuf::from("/tmp")),
            window: Some(WindowPlacement {
                x: 10.,
                y: 20.,
                width: 800.,
                height: 600.,
                maximized: false,
            }),
        };
        session.save();
        assert_eq!(Session::load(), Some(session));

        let id = new_recovery_id();
        assert_ne!(id, new_recovery_id());
        let recovery = Recovery {
            path: None,
            text: "unsaved".into(),
            crlf: false,
            base_hash: Some(7),
        };
        write_recovery(&id, &recovery).unwrap();
        assert_eq!(read_recovery(&id), Some(recovery.clone()));
        assert_eq!(
            orphaned_recoveries(&HashSet::new()),
            vec![(id.clone(), recovery)]
        );
        assert!(orphaned_recoveries(&HashSet::from([id.clone()])).is_empty());
        remove_recovery(&id);
        assert_eq!(read_recovery(&id), None);
        assert!(recovery_path("../escape").is_none());

        let _ = std::fs::remove_dir_all(dir);
    }
}
