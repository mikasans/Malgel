//! Crash safety: what a panic must not take with it.
//!
//! - Every document with unsaved changes registers its latest text here on
//!   each edit (a shared string or a rope clone — no copy). The panic hook
//!   writes them all to the recovery folder before the process goes down,
//!   so the next launch restores them (the session's usual "Recovered
//!   unsaved changes" path) even when the crash came before the debounced
//!   recovery write.
//! - The hook also appends the panic, its thread and a backtrace to the
//!   rotating log ([`crate::logfile`]).
//! - Background work (renders, listings, the terminal's reader) runs under
//!   [`isolate`]: a panic there is logged and that one job is dropped; the
//!   app keeps running.
//! - `MALGEL_CRASH_TEST=1` adds a hidden "Developer: Crash (test recovery)"
//!   command that panics on the UI thread, for tests of all of the above.

use std::{
    collections::BTreeMap,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use gpui_kit::{SharedString, component::input::Rope};

use crate::session::{self, Recovery};

/// The text of an unsaved document as of its last edit.
#[derive(Clone)]
pub enum Snapshot {
    Text(SharedString),
    /// A large document: the editor's rope is the only copy.
    Rope(Rope),
}

#[derive(Clone)]
struct Entry {
    path: Option<PathBuf>,
    base_hash: Option<u64>,
    crlf: bool,
    encoding: crate::document::Encoding,
    text: Snapshot,
}

/// Unsaved documents by recovery id.
#[derive(Default)]
pub struct Registry {
    entries: Mutex<BTreeMap<String, Entry>>,
}

impl Registry {
    pub fn note(
        &self,
        id: &str,
        path: Option<PathBuf>,
        base_hash: Option<u64>,
        crlf: bool,
        encoding: crate::document::Encoding,
        text: Snapshot,
    ) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.insert(
            id.to_string(),
            Entry {
                path,
                base_hash,
                crlf,
                encoding,
                text,
            },
        );
    }

    pub fn forget(&self, id: &str) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.remove(id);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.lock().map(|e| e.len()).unwrap_or(0)
    }

    /// Write every registered document into `dir`. Takes the lock without
    /// waiting (the panic may have come while it was held on this thread);
    /// returns how many were written.
    pub fn flush_into(&self, dir: &Path) -> usize {
        let entries = match self.entries.try_lock() {
            Ok(entries) => entries.clone(),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner().clone(),
            Err(std::sync::TryLockError::WouldBlock) => return 0,
        };
        let mut written = 0;
        for (id, entry) in entries {
            // An Untitled document left empty has nothing to rescue.
            if entry.path.is_none()
                && matches!(&entry.text, Snapshot::Text(text) if text.is_empty())
            {
                continue;
            }
            let recovery = Recovery {
                path: entry.path,
                text: String::new(),
                crlf: entry.crlf,
                base_hash: entry.base_hash,
                encoding: Some(entry.encoding),
            };
            let ok = match &entry.text {
                Snapshot::Text(text) => session::write_recovery_in(
                    dir,
                    &id,
                    &Recovery {
                        text: text.to_string(),
                        ..recovery
                    },
                ),
                Snapshot::Rope(rope) => {
                    session::write_recovery_chunks_in(dir, &id, &recovery, rope.chunks())
                }
            }
            .is_ok();
            written += usize::from(ok);
        }
        written
    }
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}

/// Record the latest text of a document with unsaved changes.
pub fn note_unsaved(
    id: &str,
    path: Option<PathBuf>,
    base_hash: Option<u64>,
    crlf: bool,
    encoding: crate::document::Encoding,
    text: Snapshot,
) {
    registry().note(id, path, base_hash, crlf, encoding, text);
}

/// The document was saved, reverted or closed: nothing to rescue.
pub fn forget(id: &str) {
    registry().forget(id);
}

/// Write every unsaved document to the recovery folder now.
pub fn flush_unsaved() -> usize {
    match session::recovery_dir() {
        Some(dir) => registry().flush_into(&dir),
        None => 0,
    }
}

/// Install the panic hook (once, at startup). The default hook still
/// prints the message afterwards.
pub fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let written = flush_unsaved();
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let backtrace = std::backtrace::Backtrace::force_capture();
        if let Some(log) = crate::logfile::RotatingLog::app() {
            let location = info
                .location()
                .map_or_else(|| "unknown location".to_string(), |l| l.to_string());
            let message = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic".into());
            log.write_line(&format!(
                "[{}] PANIC on thread '{name}' at {location}: {message} — {} — {written} unsaved document(s) written to recovery\n{backtrace}",
                crate::logfile::timestamp(),
                crate::logfile::build_label(),
            ));
        }
        previous(info);
    }));
}

/// Run background work so that a panic in it costs only this job: the
/// panic is logged (by the hook) and `None` comes back.
pub fn isolate<T>(what: &str, work: impl FnOnce() -> T) -> Option<T> {
    match panic::catch_unwind(AssertUnwindSafe(work)) {
        Ok(value) => Some(value),
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic".into());
            log::error!("{what} panicked and was stopped: {message}");
            None
        }
    }
}

/// The hidden crash command is offered (tests of crash recovery only).
pub fn crash_test_enabled() -> bool {
    std::env::var_os("MALGEL_CRASH_TEST").is_some_and(|v| v == "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("malgel-crash-{tag}-{}", session::new_recovery_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read(dir: &Path, id: &str) -> Recovery {
        let json = std::fs::read_to_string(dir.join(format!("{id}.json"))).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn flush_writes_every_unsaved_document_and_forgets_saved_ones() {
        let dir = temp_dir("flush");
        let reg = Registry::default();
        reg.note(
            "a-1",
            Some(PathBuf::from("/tmp/a.md")),
            Some(99),
            false,
            crate::document::Encoding::Gbk,
            Snapshot::Text("typed but not saved".into()),
        );
        let big: String = (0..5_000).map(|i| format!("line {i}\n")).collect();
        reg.note(
            "b-2",
            None,
            None,
            true,
            crate::document::Encoding::Utf8Bom,
            Snapshot::Rope(Rope::from(big.as_str())),
        );
        reg.note(
            "c-3",
            None,
            None,
            false,
            crate::document::Encoding::Utf8,
            Snapshot::Text("saved later".into()),
        );
        reg.forget("c-3");
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.flush_into(&dir), 2);
        let a = read(&dir, "a-1");
        assert_eq!(a.text, "typed but not saved");
        assert_eq!(a.path.as_deref(), Some(Path::new("/tmp/a.md")));
        assert_eq!(a.base_hash, Some(99), "the base travels with the text");
        let b = read(&dir, "b-2");
        assert_eq!(b.text, big);
        assert!(b.crlf);
        assert_eq!(b.encoding, Some(crate::document::Encoding::Utf8Bom));
        assert!(!dir.join("c-3.json").exists());
        // A later edit replaces the snapshot.
        reg.note(
            "a-1",
            None,
            None,
            false,
            crate::document::Encoding::Utf8,
            Snapshot::Text("newer".into()),
        );
        reg.flush_into(&dir);
        assert_eq!(read(&dir, "a-1").text, "newer");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// GBK and UTF-8-with-BOM documents rescued by the panic hook come back
    /// with their encoding, so Ctrl+S writes the same kind of bytes; an
    /// Untitled document left empty is not rescued at all.
    #[test]
    fn recovered_documents_keep_their_encoding_byte_for_byte() {
        use crate::document::{Encoding, LineEnding, decode, encode};
        let dir = temp_dir("encoding");
        let reg = Registry::default();
        let gbk = encode("第一行\n", LineEnding::Lf, Encoding::Gbk).unwrap();
        let bom = encode("héllo\n", LineEnding::Lf, Encoding::Utf8Bom).unwrap();
        for (id, bytes, edit) in [("g-1", &gbk, "第二行"), ("u-2", &bom, "wörld")] {
            let loaded = decode(bytes.clone()).unwrap();
            let edited = format!("{}{edit}\n", loaded.text);
            reg.note(
                id,
                Some(PathBuf::from(format!("/tmp/{id}.txt"))),
                None,
                false,
                loaded.encoding,
                Snapshot::Text(edited.into()),
            );
        }
        reg.note(
            "e-3",
            None,
            None,
            false,
            Encoding::Utf8,
            Snapshot::Text("".into()),
        );
        assert_eq!(reg.flush_into(&dir), 2, "the empty Untitled is skipped");
        assert!(!dir.join("e-3.json").exists());
        for (id, expected) in [
            (
                "g-1",
                encode("第一行\n第二行\n", LineEnding::Lf, Encoding::Gbk).unwrap(),
            ),
            (
                "u-2",
                encode("héllo\nwörld\n", LineEnding::Lf, Encoding::Utf8Bom).unwrap(),
            ),
        ] {
            let recovery = read(&dir, id);
            let saved = encode(&recovery.text, LineEnding::Lf, recovery.encoding.unwrap()).unwrap();
            assert_eq!(saved, expected, "{id}");
        }
        assert_eq!(
            &encode("x", LineEnding::Lf, Encoding::Utf8Bom).unwrap()[..3],
            b"\xEF\xBB\xBF"
        );
        // A recovery written before the encoding was recorded reads as None,
        // so restoring falls back to the file's own encoding.
        let old: Recovery =
            serde_json::from_str(r#"{"path":null,"text":"t","crlf":false}"#).unwrap();
        assert_eq!(old.encoding, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn flush_does_not_wait_on_a_held_lock() {
        // The panic hook may run while this thread holds the lock.
        let dir = temp_dir("held");
        let reg = Registry::default();
        reg.note(
            "x-1",
            None,
            None,
            false,
            crate::document::Encoding::Utf8,
            Snapshot::Text("t".into()),
        );
        let guard = reg.entries.lock().unwrap();
        assert_eq!(reg.flush_into(&dir), 0);
        drop(guard);
        assert_eq!(reg.flush_into(&dir), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn isolate_turns_a_panic_into_none() {
        assert_eq!(isolate("ok job", || 7), Some(7));
        let out: Option<u8> = isolate("bad job", || panic!("boom"));
        assert_eq!(out, None);
        let s = String::from("中");
        let out = isolate("slicing", || s[0..1].len());
        assert_eq!(out, None);
    }
}
