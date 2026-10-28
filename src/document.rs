//! The document being edited: where it lives and how it is read and written.

use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;

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

/// How a text file is encoded on disk; saved back the same way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Encoding {
    #[default]
    Utf8,
    /// UTF-8 starting with the EF BB BF byte-order mark, which saving keeps.
    Utf8Bom,
    /// UTF-16 with a byte-order mark, little or big endian.
    Utf16Le,
    Utf16Be,
    Gbk,
    Gb18030,
    Big5,
    ShiftJis,
    /// ISO 8859-1 as browsers read it (windows-1252), which maps every byte
    /// to a char and back.
    Latin1,
}

impl Encoding {
    /// The encodings the status bar offers for Reopen / Save with Encoding.
    pub const CHOICES: [Encoding; 9] = [
        Encoding::Utf8,
        Encoding::Utf8Bom,
        Encoding::Utf16Le,
        Encoding::Utf16Be,
        Encoding::Gbk,
        Encoding::Gb18030,
        Encoding::Big5,
        Encoding::ShiftJis,
        Encoding::Latin1,
    ];

    /// What the status bar shows (always, UTF-8 included, as VS Code does).
    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf8Bom => "UTF-8 with BOM",
            Encoding::Utf16Le => "UTF-16 LE",
            Encoding::Utf16Be => "UTF-16 BE",
            Encoding::Gbk => "GBK",
            Encoding::Gb18030 => "GB18030",
            Encoding::Big5 => "Big5",
            Encoding::ShiftJis => "Shift JIS",
            Encoding::Latin1 => "Latin-1",
        }
    }

    /// The encoding a byte-order mark at the start of `bytes` names.
    pub fn from_bom(bytes: &[u8]) -> Self {
        match bytes {
            [0xEF, 0xBB, 0xBF, ..] => Encoding::Utf8Bom,
            [0xFF, 0xFE, ..] => Encoding::Utf16Le,
            [0xFE, 0xFF, ..] => Encoding::Utf16Be,
            _ => Encoding::Utf8,
        }
    }

    fn legacy(self) -> Option<&'static encoding_rs::Encoding> {
        Some(match self {
            Encoding::Gbk => encoding_rs::GBK,
            Encoding::Gb18030 => encoding_rs::GB18030,
            Encoding::Big5 => encoding_rs::BIG5,
            Encoding::ShiftJis => encoding_rs::SHIFT_JIS,
            Encoding::Latin1 => encoding_rs::WINDOWS_1252,
            _ => return None,
        })
    }

    fn from_legacy(encoding: &'static encoding_rs::Encoding) -> Option<Self> {
        Self::CHOICES
            .into_iter()
            .find(|choice| choice.legacy() == Some(encoding))
    }

    fn bom(self) -> &'static [u8] {
        match self {
            Encoding::Utf8Bom => &[0xEF, 0xBB, 0xBF],
            Encoding::Utf16Le => &[0xFF, 0xFE],
            Encoding::Utf16Be => &[0xFE, 0xFF],
            _ => &[],
        }
    }
}

/// Text read from disk, normalized to `\n` line endings for editing.
pub struct LoadedText {
    pub text: String,
    pub line_ending: LineEnding,
    pub encoding: Encoding,
    /// Some bytes were not valid in `encoding` and read as U+FFFD; saving
    /// would replace them.
    pub lossy: bool,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// UTF-16 `body` (no byte-order mark); invalid units become U+FFFD when
/// `lossy`, otherwise they are an error.
fn decode_utf16(body: &[u8], encoding: Encoding, lossy: bool) -> io::Result<(String, bool)> {
    if !lossy && !body.len().is_multiple_of(2) {
        return Err(invalid("The file isn’t valid UTF-16 text."));
    }
    let (pairs, rest) = body.as_chunks::<2>();
    let units = pairs.iter().map(|&pair| match encoding {
        Encoding::Utf16Be => u16::from_be_bytes(pair),
        _ => u16::from_le_bytes(pair),
    });
    let mut replaced = !rest.is_empty();
    let mut text = String::with_capacity(body.len() / 2);
    for unit in char::decode_utf16(units) {
        match unit {
            Ok(c) => text.push(c),
            Err(_) if lossy => {
                replaced = true;
                text.push(char::REPLACEMENT_CHARACTER);
            }
            Err(_) => return Err(invalid("The file isn’t valid UTF-16 text.")),
        }
    }
    if !rest.is_empty() {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    Ok((text, replaced))
}

/// `bytes` in a legacy encoding; invalid sequences read as U+FFFD.
fn decode_legacy(bytes: &[u8], encoding: &'static encoding_rs::Encoding) -> (String, bool) {
    let (text, had_errors) = encoding.decode_without_bom_handling(bytes);
    (text.into_owned(), had_errors)
}

/// Read a text file: UTF-8 (a byte-order mark is remembered), UTF-16 with a
/// byte-order mark, or, when the bytes are not UTF-8, the legacy encoding
/// they confidently look like (GBK, Big5, Shift JIS, Latin-1...). Otherwise
/// the invalid bytes read as U+FFFD and [`LoadedText::lossy`] is set.
pub fn load(path: &Path) -> io::Result<LoadedText> {
    decode(fs::read(path)?)
}

/// [`load`] on bytes already read.
pub fn decode(bytes: Vec<u8>) -> io::Result<LoadedText> {
    decode_as(bytes, None)
}

/// [`decode`], or with `forced`, read in that encoding whatever the bytes
/// look like (Reopen with Encoding); what does not fit reads as U+FFFD.
pub fn decode_as(mut bytes: Vec<u8>, forced: Option<Encoding>) -> io::Result<LoadedText> {
    let bom = Encoding::from_bom(&bytes);
    let encoding = forced.unwrap_or(bom);
    // A byte-order mark is dropped from the text when the encoding it names
    // is the one used; a forced encoding reads it like any other bytes.
    let bom_len = if encoding == bom { bom.bom().len() } else { 0 };
    let (text, encoding, lossy) = match encoding {
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let (text, lossy) = decode_utf16(&bytes[bom_len..], encoding, forced.is_some())?;
            (text, encoding, lossy)
        }
        Encoding::Utf8 | Encoding::Utf8Bom => {
            // Edit the bytes read from disk in place: copying them would
            // briefly hold a large file twice.
            bytes.drain(..bom_len);
            let utf8 = if bom_len > 0 {
                Encoding::Utf8Bom
            } else {
                Encoding::Utf8
            };
            match String::from_utf8(bytes) {
                Ok(text) => (text, if forced.is_some() { encoding } else { utf8 }, false),
                Err(err) => {
                    let bytes = err.into_bytes();
                    if forced.is_some() {
                        (String::from_utf8_lossy(&bytes).into_owned(), encoding, true)
                    } else if looks_binary(&bytes) {
                        return Err(invalid(
                            "This looks like a binary file, which Malgel can’t show as text.",
                        ));
                    } else {
                        let mut detector = chardetng::EncodingDetector::new();
                        detector.feed(&bytes, true);
                        let (guess, confident) = detector.guess_assess(None, false);
                        match Encoding::from_legacy(guess).filter(|_| confident) {
                            Some(legacy) => {
                                let (text, lossy) = decode_legacy(&bytes, guess);
                                (text, legacy, lossy)
                            }
                            None => (String::from_utf8_lossy(&bytes).into_owned(), utf8, true),
                        }
                    }
                }
            }
        }
        legacy => {
            let (text, lossy) = decode_legacy(&bytes, legacy.legacy().expect("a legacy encoding"));
            (text, legacy, lossy)
        }
    };
    let mut text = text;
    let line_ending = LineEnding::detect(&text);
    // The editor holds `\n` endings only. A file whose first line ends in
    // LF can still hold CRLF further down (mixed endings): those become `\n`
    // too, and saving writes the detected ending everywhere (VS Code).
    if text.contains("\r\n") {
        text = text.replace("\r\n", "\n");
    }
    Ok(LoadedText {
        text,
        line_ending,
        encoding,
        lossy,
    })
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
    save_encoded(path, text, line_ending, Encoding::Utf8)
}

/// Write `text` to `path` in `encoding`; UTF-16 gets its byte-order mark.
pub fn save_encoded(
    path: &Path,
    text: &str,
    line_ending: LineEnding,
    encoding: Encoding,
) -> io::Result<()> {
    let contents = encode(text, line_ending, encoding)?;

    // Symlinks and hard links must be written through in place: a temp+rename
    // would replace the link with a new regular file.
    let meta = fs::symlink_metadata(path).ok();
    let write_in_place = meta.as_ref().is_some_and(|m| {
        if m.file_type().is_symlink() {
            return true;
        }
        // Hard links: rewrite in place so we do not break the other names.
        #[cfg(unix)]
        {
            m.nlink() > 1
        }
        #[cfg(not(unix))]
        {
            false
        }
    });
    if path.exists() {
        let perms = meta
            .as_ref()
            .map(|m| m.permissions())
            .or_else(|| fs::metadata(path).ok().map(|m| m.permissions()));
        if perms.is_some_and(|p| p.readonly()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "no write permission",
            ));
        }
    }
    if write_in_place {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)?;
        file.write_all(&contents)?;
        file.sync_all()?;
        return Ok(());
    }

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
        file.write_all(&contents)?;
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

/// The bytes [`save_encoded`] writes for `text`: line endings expanded,
/// a byte-order mark for UTF-8 with BOM and UTF-16. Text a legacy encoding
/// cannot hold is an error rather than silently changed.
pub fn encode(text: &str, line_ending: LineEnding, encoding: Encoding) -> io::Result<Vec<u8>> {
    let contents = match line_ending {
        LineEnding::CrLf => text.replace('\n', "\r\n"),
        LineEnding::Lf => text.to_string(),
    };
    let bom = encoding.bom().iter().copied();
    Ok(match encoding {
        Encoding::Utf8 => contents.into_bytes(),
        Encoding::Utf8Bom => bom.chain(contents.into_bytes()).collect(),
        Encoding::Utf16Le => bom
            .chain(contents.encode_utf16().flat_map(u16::to_le_bytes))
            .collect(),
        Encoding::Utf16Be => bom
            .chain(contents.encode_utf16().flat_map(u16::to_be_bytes))
            .collect(),
        legacy => {
            let (bytes, _, unmappable) = legacy
                .legacy()
                .expect("a legacy encoding")
                .encode(&contents);
            if unmappable {
                let what = first_unmappable(text, legacy)
                    .map_or_else(|| "Some characters".to_string(), |u| u.describe());
                return Err(invalid(format!(
                    "{what} can’t be written in {}. Use Save with Encoding → UTF-8 to keep it.",
                    legacy.label()
                )));
            }
            bytes.into_owned()
        }
    })
}

/// The first character a legacy encoding cannot hold, and where it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unmappable {
    pub ch: char,
    /// 1-based, counted in `\n`-separated lines.
    pub line: usize,
    /// 1-based, in characters.
    pub column: usize,
}

impl Unmappable {
    /// `“😀” (U+1F600) on line 12`, for a toast.
    pub fn describe(&self) -> String {
        format!(
            "“{}” (U+{:04X}) on line {}",
            self.ch, self.ch as u32, self.line
        )
    }
}

/// Where `text` first stops fitting `encoding`, or `None` when all of it
/// fits (UTF-8 and UTF-16 hold everything). Lines are tried whole first, so
/// a large document costs one encode per line, not per character.
pub fn first_unmappable(text: &str, encoding: Encoding) -> Option<Unmappable> {
    let legacy = encoding.legacy()?;
    let fits = |s: &str| !legacy.encode(s).2;
    text.split('\n').enumerate().find_map(|(index, line)| {
        if fits(line) {
            return None;
        }
        let mut buf = [0u8; 4];
        line.chars().enumerate().find_map(|(column, ch)| {
            (!fits(ch.encode_utf8(&mut buf))).then_some(Unmappable {
                ch,
                line: index + 1,
                column: column + 1,
            })
        })
    })
}

/// Whether `path` looks like a Markdown document.
pub fn is_markdown_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "md" | "markdown" | "mdown" | "mkd" | "mkdn" | "mdx"
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
        // Highlighted from typst's own syntax tree instead (typst_live).
        "typ" => "text",
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
/// A Typst source opens in the editor with a live preview of its pages
/// beside it, compiled as it is typed (see `typst_live`).
pub fn is_typst_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("typ"))
}

/// How Malgel opens `path`: which viewer / editor surface to use.
///
/// Kept in one place so Explorer icons, Quick Open and `load_path` agree on
/// the kind (PDF ≠ Typst ≠ Markdown ≠ image ≠ plain/code text).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // used by tests; load_path still branches on path helpers
pub enum OpenRenderer {
    Markdown,
    Typst,
    Pdf,
    Image,
    /// Source / config / unknown text — editor only (no Markdown preview).
    Text,
}

/// The renderer Malgel uses when the user casually opens `path`.
#[allow(dead_code)] // covered by unit tests; call sites use path helpers today
pub fn open_renderer(path: &Path) -> OpenRenderer {
    if is_pdf_path(path) {
        OpenRenderer::Pdf
    } else if is_image_path(path) {
        OpenRenderer::Image
    } else if is_typst_path(path) {
        OpenRenderer::Typst
    } else if is_markdown_path(path) {
        OpenRenderer::Markdown
    } else {
        OpenRenderer::Text
    }
}

/// The language a file is shown as in the status bar, the way VS Code names
/// the language mode: `Rust` for `main.rs`, `Plain Text` for a file it has
/// no mode for. A document that has never been saved is Markdown, Malgel's
/// own kind of document.
pub fn language_name(path: Option<&Path>) -> &'static str {
    let Some(path) = path else {
        return "Markdown";
    };
    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return "Plain Text";
    };
    match ext.to_ascii_lowercase().as_str() {
        "md" | "markdown" | "mdown" | "mkd" | "mkdn" => "Markdown",
        "mdx" => "MDX",
        "txt" => "Plain Text",
        "typ" => "Typst",
        "pdf" => "PDF",
        "csv" => "CSV",
        "tsv" => "TSV",
        "xml" => "XML",
        "svg" => "SVG",
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tif" | "tiff" | "ico" => "Image",
        _ => match language_tag(path) {
            Some("rust") => "Rust",
            Some("python") => "Python",
            Some("toml") => "TOML",
            Some("javascript") => "JavaScript",
            Some("typescript") => "TypeScript",
            Some("tsx") => "TypeScript JSX",
            Some("go") => "Go",
            Some("c") => "C",
            Some("cpp") => "C++",
            Some("java") => "Java",
            Some("ruby") => "Ruby",
            Some("bash") => "Shell Script",
            Some("yaml") => "YAML",
            Some("json") => "JSON",
            Some("html") => "HTML",
            Some("css") => "CSS",
            Some("kotlin") => "Kotlin",
            Some("zig") => "Zig",
            Some("lua") => "Lua",
            Some("php") => "PHP",
            Some("csharp") => "C#",
            Some("scala") => "Scala",
            Some("elixir") => "Elixir",
            Some("swift") => "Swift",
            Some("make") => "Makefile",
            Some("cmake") => "CMake",
            Some("proto") => "Protocol Buffers",
            Some("diff") => "Diff",
            _ => "Plain Text",
        },
    }
}

/// Whether a document at `path` gets the rendered preview beside it.
/// Markdown does (and a document never saved, which is Markdown); source
/// code and other text does not, the way VS Code opens a `.rs` file in an
/// editor alone.
pub fn has_markdown_preview(path: Option<&Path>) -> bool {
    path.is_none_or(is_markdown_path)
}

/// How a text is indented, as VS Code's status bar reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Indent {
    pub hard_tabs: bool,
    pub size: usize,
}

impl Indent {
    /// Malgel's editor default when a text gives nothing to go by.
    pub const DEFAULT: Indent = Indent {
        hard_tabs: false,
        size: 2,
    };

    pub fn label(self) -> String {
        if self.hard_tabs {
            format!("Tab Size: {}", self.size)
        } else {
            format!("Spaces: {}", self.size)
        }
    }
}

/// Guess a text's indentation the way VS Code does: tabs if more lines open
/// with a tab than with spaces, otherwise the most common step between the
/// indentation of neighbouring lines. Only the first lines are read, so a
/// huge file costs no more than a small one.
#[cfg(test)]
pub fn detect_indent(text: &str) -> Indent {
    detect_indent_or(text, Indent::DEFAULT)
}

/// The indentation a file of this kind uses when its text gives nothing to
/// go by, after VS Code's per-language defaults: four spaces for the C
/// family, Python and the like, tabs where the language wants them (Go,
/// Makefiles), two spaces for Markdown, JSON, YAML and the web.
pub fn default_indent(path: Option<&Path>) -> Indent {
    let tabs = Indent {
        hard_tabs: true,
        size: 4,
    };
    let four = Indent {
        hard_tabs: false,
        size: 4,
    };
    if path
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("makefile") || name == "GNUmakefile")
    {
        return tabs;
    }
    match language_name(path) {
        "Go" | "Makefile" => tabs,
        "Rust" | "Python" | "Java" | "C" | "C++" | "C#" | "PHP" | "Kotlin" | "Swift" | "Scala"
        | "Shell Script" | "Plain Text" | "Lua" | "Zig" | "CMake" => four,
        _ => Indent::DEFAULT,
    }
}

/// [`detect_indent`] with the language's own default for text that shows no
/// indentation.
pub fn detect_indent_for(text: &str, path: Option<&Path>) -> Indent {
    detect_indent_or(text, default_indent(path))
}

fn detect_indent_or(text: &str, fallback: Indent) -> Indent {
    const MAX_LINES: usize = 10_000;
    let mut tab_lines = 0usize;
    let mut space_lines = 0usize;
    let mut steps = [0usize; 9];
    let mut previous = 0usize;
    for line in text.lines().take(MAX_LINES) {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with('\t') {
            tab_lines += 1;
            continue;
        }
        let spaces = line.len() - line.trim_start_matches(' ').len();
        if spaces > 0 {
            space_lines += 1;
        }
        let step = spaces.abs_diff(previous);
        if (2..=8).contains(&step) {
            steps[step] += 1;
        }
        previous = spaces;
    }
    if tab_lines > space_lines {
        return Indent {
            hard_tabs: true,
            size: if fallback.hard_tabs { fallback.size } else { 4 },
        };
    }
    // Ties go to the smaller step: 2 and 4 both explain a 4-space nest.
    let best = (2..=8).max_by_key(|&step| (steps[step], std::cmp::Reverse(step)));
    match best {
        Some(size) if steps[size] > 0 => Indent {
            hard_tabs: false,
            size,
        },
        _ => fallback,
    }
}

/// Past this size a text file is not read until the user says how to open
/// it, as VS Code asks only for a file too big to hold comfortably. Smaller
/// large files open straight away with costly features off (see
/// `DocumentView::reduce`).
pub const PROMPT_TEXT_BYTES: u64 = 512 * 1024 * 1024;
/// Past this size a PDF or picture asks first too: it is read whole.
pub const PROMPT_MEDIA_BYTES: u64 = 256 * 1024 * 1024;

/// Whether a file can be opened straight away, judged from its size and its
/// first few kilobytes only; nothing else is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenCheck {
    Fine,
    /// Larger than the threshold for its kind; the size in bytes.
    Large(u64),
    /// Text by its name, but its first bytes hold NUL characters, and it is
    /// still valid UTF-8, so it can be opened as text if the user wants.
    BinaryText,
    /// Text by its name but not text: it cannot be opened in the editor.
    Binary,
}

/// Past this size a file with NUL bytes is not read whole to see whether it
/// is UTF-8 after all; it is treated as binary.
const BINARY_VALIDATE_BYTES: u64 = 16 * 1024 * 1024;

pub fn open_check(path: &Path) -> OpenCheck {
    let Ok(metadata) = fs::metadata(path) else {
        return OpenCheck::Fine;
    };
    let size = metadata.len();
    // PDF / image are opaque viewers. Typst is a text source (live pages
    // beside the editor), so it uses the text size / binary sniff path.
    let media = is_pdf_path(path) || is_image_path(path);
    if media {
        return if size > PROMPT_MEDIA_BYTES {
            OpenCheck::Large(size)
        } else {
            OpenCheck::Fine
        };
    }
    if size > PROMPT_TEXT_BYTES {
        return OpenCheck::Large(size);
    }
    let mut head = [0u8; 8192];
    let read = fs::File::open(path)
        .and_then(|mut file| std::io::Read::read(&mut file, &mut head))
        .unwrap_or(0);
    let head = &head[..read];
    // UTF-16 holds NUL bytes in every ASCII character; its byte-order mark
    // says it is text.
    if matches!(
        Encoding::from_bom(head),
        Encoding::Utf16Le | Encoding::Utf16Be
    ) || !head.contains(&0)
    {
        return OpenCheck::Fine;
    }
    if size <= BINARY_VALIDATE_BYTES
        && fs::read(path).is_ok_and(|bytes| std::str::from_utf8(&bytes).is_ok())
    {
        OpenCheck::BinaryText
    } else {
        OpenCheck::Binary
    }
}

/// The name shown for a document in the title bar and dialogs.
pub fn display_name(path: Option<&PathBuf>) -> String {
    path.and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Untitled".to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn utf16_with_a_bom_opens_and_saves_byte_for_byte() {
        use super::{Encoding, OpenCheck, load, open_check, save_encoded};
        let dir = std::env::temp_dir().join(format!("malgel-u16-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, bytes, encoding) in [
            (
                "le.md",
                &b"\xff\xfeh\x00i\x00\r\x00\n\x00"[..],
                Encoding::Utf16Le,
            ),
            ("be.md", &b"\xfe\xff\x00h\x00i\x00\n"[..], Encoding::Utf16Be),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(open_check(&path), OpenCheck::Fine, "{name}");
            let loaded = load(&path).unwrap();
            assert_eq!(loaded.text, "hi\n");
            assert_eq!(loaded.encoding, encoding);
            save_encoded(&path, &loaded.text, loaded.line_ending, loaded.encoding).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "{name} round trip");
        }
        // A picture under a text name cannot be opened; UTF-8 with NULs can.
        let png = dir.join("x.md");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\xff\xd8").unwrap();
        assert_eq!(open_check(&png), OpenCheck::Binary);
        let nul = dir.join("nul.md");
        std::fs::write(&nul, b"text\x00more").unwrap();
        assert_eq!(open_check(&nul), OpenCheck::BinaryText);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn gbk_and_latin1_files_open_and_save_in_their_own_encoding() {
        use super::{Encoding, decode, decode_as, encode};
        // printf '第一行\n' | iconv -t gbk
        let gbk = b"\xb5\xda\xd2\xbb\xd0\xd0\n".to_vec();
        let loaded = decode(gbk.clone()).unwrap();
        assert_eq!(loaded.text, "第一行\n");
        assert_eq!(loaded.encoding, Encoding::Gbk);
        assert!(!loaded.lossy);
        // An unchanged save writes the same bytes; an edit stays GBK.
        assert_eq!(
            encode(&loaded.text, loaded.line_ending, loaded.encoding).unwrap(),
            gbk
        );
        let edited = encode("第一行\n第二行\n", loaded.line_ending, loaded.encoding).unwrap();
        assert_eq!(
            edited,
            b"\xb5\xda\xd2\xbb\xd0\xd0\n\xb5\xda\xb6\xfe\xd0\xd0\n"
        );
        // A char GBK cannot hold is refused, not written as something else.
        assert!(encode("emoji 🎉", loaded.line_ending, Encoding::Gbk).is_err());
        // Latin-1: every byte is a char and back.
        let latin1: Vec<u8> = b"caf\xe9 na\xefve \xa9 2026\r\n".to_vec();
        let loaded = decode(latin1.clone()).unwrap();
        assert_eq!(loaded.text, "café naïve © 2026\n");
        assert_eq!(loaded.encoding, Encoding::Latin1);
        assert_eq!(
            encode(&loaded.text, loaded.line_ending, loaded.encoding).unwrap(),
            latin1
        );
        let all: Vec<u8> = (0x20..=0xffu8).collect();
        let loaded = decode_as(all.clone(), Some(Encoding::Latin1)).unwrap();
        assert_eq!(
            encode(&loaded.text, loaded.line_ending, Encoding::Latin1).unwrap(),
            all
        );
        // Reopen with Encoding reads the same bytes another way.
        let as_utf8 = decode_as(gbk, Some(Encoding::Utf8)).unwrap();
        assert!(as_utf8.lossy && as_utf8.text.contains('\u{fffd}'));
        assert_eq!(as_utf8.encoding, Encoding::Utf8);
    }

    #[test]
    fn text_in_no_confident_encoding_opens_with_replacement_chars() {
        let loaded = super::decode(b"ok \xff\xfe\xfd ok\n".to_vec()).unwrap();
        assert!(loaded.lossy);
        assert_eq!(loaded.encoding, super::Encoding::Utf8);
        assert!(loaded.text.starts_with("ok ") && loaded.text.contains('\u{fffd}'));
    }

    #[test]
    fn a_utf8_bom_survives_an_edit_and_save() {
        use super::{Encoding, load, save_encoded};
        let dir = std::env::temp_dir().join(format!("malgel-bom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bom.txt");
        std::fs::write(&path, b"\xef\xbb\xbfhello\n").unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.text, "hello\n");
        assert_eq!(loaded.encoding, Encoding::Utf8Bom);
        let edited = format!("{}world\n", loaded.text);
        save_encoded(&path, &edited, loaded.line_ending, loaded.encoding).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"\xef\xbb\xbfhello\nworld\n");
        // Every encoding's status bar label is shown, UTF-8 included.
        assert_eq!(Encoding::Utf8.label(), "UTF-8");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn language_names_follow_the_extension() {
        use std::path::Path;
        assert_eq!(super::language_name(None), "Markdown");
        assert_eq!(super::language_name(Some(Path::new("a/main.rs"))), "Rust");
        assert_eq!(super::language_name(Some(Path::new("x.JSON"))), "JSON");
        assert_eq!(
            super::language_name(Some(Path::new("notes.md"))),
            "Markdown"
        );
        assert_eq!(super::language_name(Some(Path::new("p.pdf"))), "PDF");
        assert_eq!(super::language_name(Some(Path::new("p.typ"))), "Typst");
        assert_eq!(
            super::language_name(Some(Path::new("LICENSE"))),
            "Plain Text"
        );
        assert_eq!(
            super::language_name(Some(Path::new("a.weird"))),
            "Plain Text"
        );
        assert!(super::has_markdown_preview(None));
        assert!(super::has_markdown_preview(Some(Path::new("a.md"))));
        assert!(!super::has_markdown_preview(Some(Path::new("a.rs"))));
    }

    #[test]
    fn large_and_binary_files_are_caught_before_reading() {
        use super::{OpenCheck, PROMPT_TEXT_BYTES, open_check};
        let dir = std::env::temp_dir().join(format!("malgel-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("a.md");
        std::fs::write(&small, "# hi\n").unwrap();
        assert_eq!(open_check(&small), OpenCheck::Fine);
        let binary = dir.join("b.md");
        std::fs::write(&binary, [b'#', 0, 1, 2]).unwrap();
        assert_eq!(open_check(&binary), OpenCheck::BinaryText);
        std::fs::write(&binary, [b'#', 0, 0xff, 2]).unwrap();
        assert_eq!(open_check(&binary), OpenCheck::Binary);
        let big = dir.join("c.md");
        let file = std::fs::File::create(&big).unwrap();
        file.set_len(PROMPT_TEXT_BYTES + 1).unwrap();
        assert_eq!(open_check(&big), OpenCheck::Large(PROMPT_TEXT_BYTES + 1));
        // A sparse PDF under the media threshold opens without asking.
        let pdf = dir.join("d.pdf");
        std::fs::File::create(&pdf)
            .unwrap()
            .set_len(17 * 1024 * 1024)
            .unwrap();
        assert_eq!(open_check(&pdf), OpenCheck::Fine);
        // Past the media threshold a PDF asks first too.
        let huge_pdf = dir.join("e.pdf");
        std::fs::File::create(&huge_pdf)
            .unwrap()
            .set_len(super::PROMPT_MEDIA_BYTES + 1)
            .unwrap();
        assert_eq!(
            open_check(&huge_pdf),
            OpenCheck::Large(super::PROMPT_MEDIA_BYTES + 1)
        );
        assert_eq!(open_check(&dir.join("missing.md")), OpenCheck::Fine);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn indentation_falls_back_to_the_language_default() {
        use super::{Indent, detect_indent_for};
        use std::path::Path;
        let four = Indent {
            hard_tabs: false,
            size: 4,
        };
        let tabs = Indent {
            hard_tabs: true,
            size: 4,
        };
        assert_eq!(
            detect_indent_for("fn main() {}\n", Some(Path::new("a.rs"))),
            four
        );
        assert_eq!(detect_indent_for("x = 1\n", Some(Path::new("a.py"))), four);
        assert_eq!(
            detect_indent_for("package a\n", Some(Path::new("a.go"))),
            tabs
        );
        assert_eq!(
            detect_indent_for("all:\n", Some(Path::new("Makefile"))),
            tabs
        );
        assert_eq!(
            detect_indent_for("{}\n", Some(Path::new("a.json"))),
            Indent::DEFAULT
        );
        assert_eq!(detect_indent_for("# a\n", None), Indent::DEFAULT);
        // Evidence in the text wins over the default.
        assert_eq!(
            detect_indent_for("fn a() {\n  b();\n}\n", Some(Path::new("a.rs"))),
            Indent {
                hard_tabs: false,
                size: 2
            }
        );
        assert!(!super::has_markdown_preview(Some(Path::new("notes.txt"))));
        assert!(!super::is_markdown_path(Path::new("notes.txt")));
    }

    #[test]
    fn indentation_is_detected() {
        use super::{Indent, detect_indent};
        assert_eq!(detect_indent("a\n"), Indent::DEFAULT);
        assert_eq!(
            detect_indent("fn a() {\n    b();\n    if x {\n        c();\n    }\n}\n"),
            Indent {
                hard_tabs: false,
                size: 4
            }
        );
        assert_eq!(
            detect_indent("- a\n  - b\n    - c\n"),
            Indent {
                hard_tabs: false,
                size: 2
            }
        );
        assert_eq!(
            detect_indent("a:\n\tb\n\tc\n"),
            Indent {
                hard_tabs: true,
                size: 4
            }
        );
        assert_eq!(
            Indent {
                hard_tabs: true,
                size: 4
            }
            .label(),
            "Tab Size: 4"
        );
    }

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
    fn mixed_line_endings_load_as_lf_only() {
        // Found by the decode fuzz test: LF first, then CRLF.
        let loaded = decode(b"\n\r\n".to_vec()).unwrap();
        assert_eq!(loaded.text, "\n\n");
        assert_eq!(loaded.line_ending, LineEnding::Lf);
        let loaded = decode(b"a\r\nb\nc\r\n".to_vec()).unwrap();
        assert_eq!(loaded.text, "a\nb\nc\n");
        assert_eq!(loaded.line_ending, LineEnding::CrLf);
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

    #[test]
    fn open_renderer_matches_common_kinds() {
        use super::OpenRenderer::*;
        assert_eq!(open_renderer(Path::new("a.md")), Markdown);
        assert_eq!(open_renderer(Path::new("a.typ")), Typst);
        assert_eq!(open_renderer(Path::new("a.pdf")), Pdf);
        assert_eq!(open_renderer(Path::new("a.png")), Image);
        assert_eq!(open_renderer(Path::new("a.rs")), Text);
        assert_eq!(open_renderer(Path::new("a.unknown")), Text);
    }

    #[test]
    fn typst_uses_text_open_check_not_media_threshold() {
        // A small .typ must not be treated as opaque media (no binary skip).
        let dir = std::env::temp_dir().join(format!("malgel-typ-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let typ = dir.join("paper.typ");
        std::fs::write(&typ, "= Hello\n").unwrap();
        assert_eq!(open_check(&typ), OpenCheck::Fine);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn readonly_file_is_not_clobbered_by_save() {
        let dir = std::env::temp_dir().join(format!("malgel-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("locked.md");
        std::fs::write(&path, "safe\n").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).unwrap();
        let err = save(&path, "clobber\n", LineEnding::Lf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("no write permission"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "safe\n");
        #[allow(clippy::permissions_set_readonly_false)]
        {
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_readonly(false);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn saving_through_a_symlink_keeps_the_link() {
        let dir = std::env::temp_dir().join(format!("malgel-sym-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("real.md");
        let link = dir.join("link.md");
        std::fs::write(&target, "old\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        save(&link, "new\n", LineEnding::Lf).unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
