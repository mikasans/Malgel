//! User-facing path presentation.
//!
//! Windows `canonicalize` and some Win32 APIs use the extended-length
//! verbatim prefix `\\?\`. That form is useful for filesystem calls that
//! need paths longer than `MAX_PATH`, but Explorer, the clipboard, and
//! `ShellExecute` / `ParseDisplayName` expect the ordinary `D:\…` shape.
//! Strip the prefix only when showing or handing a path to those APIs —
//! keep the original `PathBuf` for internal reads and writes.

use std::path::{Path, PathBuf};

/// Absolute path text for the clipboard / toasts: no Win32 `\\?\` prefix.
pub fn path_for_clipboard(path: &Path) -> String {
    strip_verbatim_prefix(path)
        .to_string_lossy()
        .into_owned()
}

/// The same path without a Win32 extended-length / verbatim prefix, so
/// Explorer and ShellExecute can open it. Unchanged on other platforms
/// (and when the path never had the prefix).
pub fn path_for_shell(path: &Path) -> PathBuf {
    strip_verbatim_prefix(path)
}

/// `\\?\C:\foo` → `C:\foo`, `\\?\UNC\server\share` → `\\server\share`.
///
/// Also accepts the forward-slash forms Rust sometimes prints under tests.
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(rest) = strip_verbatim_str(&raw) {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

fn strip_verbatim_str(raw: &str) -> Option<String> {
    // UNC via the verbatim prefix: \\?\UNC\server\share → \\server\share
    for prefix in [r"\\?\UNC\", r"//?/UNC/"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            return Some(format!(r"\\{rest}"));
        }
    }
    for prefix in [r"\\?\", r"//?/"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn strips_win32_drive_and_unc_prefixes() {
        assert_eq!(
            path_for_clipboard(Path::new(r"\\?\D:\work\leo\docs\glint.svg")),
            r"D:\work\leo\docs\glint.svg"
        );
        assert_eq!(
            path_for_clipboard(Path::new(r"\\?\UNC\server\share\file.md")),
            r"\\server\share\file.md"
        );
        assert_eq!(
            path_for_clipboard(Path::new(r"D:\already\normal.svg")),
            r"D:\already\normal.svg"
        );
        // Forward-slash verbatim form sometimes seen in cross-platform tests.
        assert_eq!(
            path_for_clipboard(Path::new("//?/C:/Users/a/b.md")),
            "C:/Users/a/b.md"
        );
        // POSIX absolute paths stay as they are.
        assert_eq!(
            path_for_clipboard(Path::new("/work/notes/a.md")),
            "/work/notes/a.md"
        );
    }

    #[test]
    fn shell_path_matches_clipboard_form() {
        let verbatim = Path::new(r"\\?\C:\Users\me\file.svg");
        assert_eq!(
            path_for_shell(verbatim).to_string_lossy(),
            r"C:\Users\me\file.svg"
        );
    }
}
