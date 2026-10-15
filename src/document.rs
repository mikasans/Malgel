//! The document being edited: where it lives and how it is read and written.

use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
};

/// Line ending convention detected when a file is opened, restored on save.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn detect(text: &str) -> Self {
        match text.find('\n') {
            Some(ix) if ix > 0 && text.as_bytes()[ix - 1] == b'\r' => LineEnding::CrLf,
            _ => LineEnding::Lf,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::CrLf => "CRLF",
        }
    }

    /// The other convention, for switching a document between the two the way
    /// clicking the EOL item in VS Code's status bar does.
    pub fn toggled(self) -> Self {
        match self {
            LineEnding::Lf => LineEnding::CrLf,
            LineEnding::CrLf => LineEnding::Lf,
        }
    }
}

/// Text read from disk, normalized to `\n` line endings for editing.
pub struct LoadedText {
    pub text: String,
    pub line_ending: LineEnding,
}

/// Read a UTF-8 text file. A byte-order mark is dropped.
pub fn load(path: &Path) -> io::Result<LoadedText> {
    let bytes = fs::read(path)?;
    let text = String::from_utf8(bytes).map_err(|err| {
        // Distinguish a genuinely binary file from text that merely has a
        // stray invalid byte, so the message tells the user what happened.
        let bytes = err.as_bytes();
        let message = if looks_binary(bytes) {
            "This looks like a binary file, which Malgel can’t show as text."
        } else {
            "The file isn’t valid UTF-8 text."
        };
        io::Error::new(io::ErrorKind::InvalidData, message)
    })?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let line_ending = LineEnding::detect(text);
    let text = match line_ending {
        LineEnding::CrLf => text.replace("\r\n", "\n"),
        LineEnding::Lf => text.to_string(),
    };
    Ok(LoadedText { text, line_ending })
}

/// Whether `bytes` look like binary data rather than text: a NUL byte, or a
/// large share of control bytes in the sample. Used only to pick a clearer
/// error message; it never decides how a file is opened.
pub fn looks_binary(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(8192)];
    if sample.is_empty() {
        return false;
    }
    if sample.contains(&0) {
        return true;
    }
    let control = sample
        .iter()
        .filter(|&&byte| byte < 0x09 || (0x0e..0x20).contains(&byte))
        .count();
    control * 100 / sample.len() > 10
}

/// Write `text` to `path` atomically: a temporary sibling file is written and
/// flushed, then renamed over the target, so a crash never leaves a
/// half-written document behind.
pub fn save(path: &Path, text: &str, line_ending: LineEnding) -> io::Result<()> {
    let contents = match line_ending {
        LineEnding::CrLf => text.replace('\n', "\r\n"),
        LineEnding::Lf => text.to_string(),
    };

    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Missing file name."))?;
    let temp = dir.join(format!(
        ".{}.malgel-{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));

    let result = (|| {
        let mut file = fs::File::create(&temp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        // Keep the permissions of the file being replaced.
        if let Ok(metadata) = fs::metadata(path) {
            fs::set_permissions(&temp, metadata.permissions())?;
        }
        fs::rename(&temp, path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Whether `path` looks like a Markdown document.
pub fn is_markdown_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "md" | "markdown" | "mdown" | "mkd" | "mkdn" | "mdx" | "txt"
            )
        })
}

/// Whether `path` is a PDF document.
#[allow(dead_code)]
/// The tree-sitter language tag for a path's extension, for rendering
/// non-markdown files as highlighted code in the preview.
pub fn language_tag(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust",
        "py" => "python",
        "toml" => "toml",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        "sh" | "bash" | "zsh" => "bash",
        "yml" | "yaml" => "yaml",
        "json" => "json",
        "html" | "htm" => "html",
        "css" => "css",
        "kt" | "kts" => "kotlin",
        "zig" => "zig",
        "lua" => "lua",
        "php" => "php",
        "cs" => "csharp",
        "scala" => "scala",
        "ex" | "exs" => "elixir",
        "swift" => "swift",
        "mk" | "makefile" | "mak" => "make",
        "cmake" => "cmake",
        "proto" => "proto",
        "diff" | "patch" => "diff",
        "md" | "markdown" => "markdown",
        _ => return None,
    })
}

/// Whether `path` names an image the app can preview.
pub fn is_image_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some(ext) if matches!(
            ext.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tif" | "tiff" | "ico" | "svg"
        )
    )
}

pub fn is_pdf_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

/// Whether `path` is a Typst source file.
///
/// A Typst document is not read as text: its preview is the PDF the Typst
/// compiler makes of it, so opening one compiles the file and shows the
/// result. Malgel does not ship the `typst` command, so the preview can only
/// ever be as fresh as the last time Malgel compiled it — it is refreshed
/// when the file is opened and whenever it changes on disk.
pub fn is_typst_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("typ"))
}

/// The name shown for a document in the title bar and dialogs.
pub fn display_name(path: Option<&PathBuf>) -> String {
    path.and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Untitled".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_line_endings() {
        assert_eq!(LineEnding::detect("a\r\nb"), LineEnding::CrLf);
        assert_eq!(LineEnding::detect("a\nb\r\n"), LineEnding::Lf);
        assert_eq!(LineEnding::detect("no newline"), LineEnding::Lf);
    }

    /// Clicking the status bar's EOL item flips the document between the two
    /// conventions, and the flipped ending is what gets written.
    #[test]
    fn toggling_the_line_ending_changes_what_is_saved() {
        let dir = std::env::temp_dir().join(format!("malgel-eol-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("doc.md");

        let lf = LineEnding::Lf;
        assert_eq!(lf.toggled(), LineEnding::CrLf);
        assert_eq!(lf.toggled().toggled(), LineEnding::Lf);

        save(&path, "a\nb\n", lf.toggled()).unwrap();
        assert_eq!(load(&path).unwrap().line_ending, LineEnding::CrLf);
        save(&path, "a\nb\n", lf.toggled().toggled()).unwrap();
        assert_eq!(load(&path).unwrap().line_ending, LineEnding::Lf);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn round_trips_crlf_and_bom() {
        let dir = std::env::temp_dir().join(format!("malgel-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("doc.md");
        fs::write(&path, "\u{feff}# Title\r\n\r\nBody\r\n").unwrap();

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.text, "# Title\n\nBody\n");
        assert_eq!(loaded.line_ending, LineEnding::CrLf);

        save(&path, "# Title\n\nEdited\n", loaded.line_ending).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# Title\r\n\r\nEdited\r\n"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn recognizes_markdown_extensions() {
        assert!(is_markdown_path(Path::new("README.md")));
        assert!(is_markdown_path(Path::new("notes.MARKDOWN")));
        assert!(!is_markdown_path(Path::new("image.png")));
        assert_eq!(display_name(None), "Untitled");
    }

    #[test]
    fn recognizes_pdf_extensions() {
        assert!(is_pdf_path(Path::new("document.pdf")));
        assert!(is_pdf_path(Path::new("REPORT.PDF")));
        assert!(is_pdf_path(Path::new("/a/b/c/doc.Pdf")));
        assert!(!is_pdf_path(Path::new("document.md")));
        assert!(!is_pdf_path(Path::new("image.png")));
        assert!(!is_pdf_path(Path::new("pdf")));
    }

    #[test]
    fn recognizes_typst_extensions() {
        assert!(is_typst_path(Path::new("paper.typ")));
        assert!(is_typst_path(Path::new("THESIS.TYP")));
        assert!(!is_typst_path(Path::new("paper.typo")));
        assert!(!is_typst_path(Path::new("notes.md")));
        assert!(!is_typst_path(Path::new("typ")));
    }
}
