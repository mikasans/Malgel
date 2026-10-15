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
}

fn recovery_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("recovery"))
}

fn recovery_path(id: &str) -> Option<PathBuf> {
    // Ids are generated here; refuse anything that could leave the folder.
    if id.is_empty() || !id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-') {
        return None;
    }
    recovery_dir().map(|dir| dir.join(format!("{id}.json")))
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
    let path = recovery_path(id).ok_or_else(|| std::io::Error::other("no config folder"))?;
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
    let path = recovery_path(id).ok_or_else(|| std::io::Error::other("no config folder"))?;
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

/// Modification time and length of a file, to notice changes made by other
/// programs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskStamp {
    pub modified: Option<SystemTime>,
    pub len: u64,
}

impl DiskStamp {
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
