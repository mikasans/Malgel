//! The icon a file draws with, by type — and a colour so types stay
//! distinguishable (VS Code–ish file icons, not monochrome mud).

use std::path::Path;

use gpui_kit::{Hsla, assets::IconName, rgb};

/// The icon for `path`, chosen by its extension. Anything unrecognised — a
/// file with no extension, or one we have no icon for — gets a plain file.
pub fn file_icon(path: &Path) -> IconName {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);

    match extension.as_deref() {
        Some("md" | "markdown" | "mdx") => IconName::FileText,
        Some(
            "rs" | "py" | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "go" | "c" | "h" | "cpp"
            | "cc" | "hpp" | "cs" | "java" | "kt" | "kts" | "swift" | "rb" | "php" | "scala"
            | "zig" | "lua" | "ex" | "exs" | "erl" | "hs" | "clj" | "vue" | "svelte" | "dart" | "r",
        ) => IconName::FileCode,
        Some("sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd") => IconName::FileTerminal,
        Some("json" | "jsonc" | "toml" | "yaml" | "yml" | "xml" | "ini" | "cfg" | "conf") => {
            IconName::FileBraces
        }
        Some("png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "bmp" | "ico" | "avif") => {
            IconName::FileImage
        }
        Some("csv" | "tsv") => IconName::FileSpreadsheet,
        Some("pdf" | "doc" | "docx" | "odt" | "rtf" | "pages") => IconName::FileType,
        // A Typst source shares the icon of what its preview compiles to.
        Some("typ") => IconName::FileType,
        Some("txt" | "text" | "log") => IconName::FileText,
        _ => IconName::File,
    }
}

/// Accent colour for `path`'s file-type icon. Fixed hues so types stay
/// distinct in both light and dark themes (theme accents alone collapse to mud).
pub fn file_icon_color(path: &Path) -> Hsla {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);

    match extension.as_deref() {
        Some("md" | "markdown" | "mdx" | "txt" | "text" | "log") => rgb(0x519aba).into(), // blue
        Some(
            "rs" | "py" | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "go" | "c" | "h" | "cpp"
            | "cc" | "hpp" | "cs" | "java" | "kt" | "kts" | "swift" | "rb" | "php" | "scala"
            | "zig" | "lua" | "ex" | "exs" | "erl" | "hs" | "clj" | "vue" | "svelte" | "dart" | "r",
        ) => rgb(0x8bc34a).into(), // green
        Some("sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd") => rgb(0x4db6ac).into(), // teal
        Some("json" | "jsonc" | "toml" | "yaml" | "yml" | "xml" | "ini" | "cfg" | "conf") => {
            rgb(0xffca28).into() // amber
        }
        Some("png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "bmp" | "ico" | "avif") => {
            rgb(0xec407a).into() // pink
        }
        Some("csv" | "tsv") => rgb(0x66bb6a).into(), // soft green
        Some("pdf" | "doc" | "docx" | "odt" | "rtf" | "pages" | "typ") => rgb(0xef5350).into(), // red
        _ => rgb(0x90a4ae).into(), // blue-grey
    }
}

/// Folder chevron / folder glyph colour.
pub fn folder_icon_color() -> Hsla {
    rgb(0xffb74d).into() // warm amber, VS Code–ish folders
}

/// Activity-bar landmark colours (Explorer / Search / SCM / Extensions).
pub fn activity_icon_color(kind: ActivityIcon) -> Hsla {
    match kind {
        ActivityIcon::Explorer => rgb(0x42a5f5).into(),
        ActivityIcon::Search => rgb(0xffca28).into(),
        ActivityIcon::SourceControl => rgb(0xab47bc).into(),
        ActivityIcon::Extensions => rgb(0x26c6da).into(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityIcon {
    Explorer,
    Search,
    SourceControl,
    Extensions,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_family_gets_its_own_icon() {
        assert_eq!(file_icon(Path::new("notes.md")), IconName::FileText);
        assert_eq!(file_icon(Path::new("main.rs")), IconName::FileCode);
        assert_eq!(file_icon(Path::new("run.sh")), IconName::FileTerminal);
        assert_eq!(file_icon(Path::new("Cargo.toml")), IconName::FileBraces);
        assert_eq!(file_icon(Path::new("logo.png")), IconName::FileImage);
        assert_eq!(file_icon(Path::new("data.csv")), IconName::FileSpreadsheet);
        assert_eq!(file_icon(Path::new("paper.pdf")), IconName::FileType);
        assert_eq!(file_icon(Path::new("paper.typ")), IconName::FileType);
    }

    #[test]
    fn the_extension_is_matched_regardless_of_case() {
        assert_eq!(file_icon(Path::new("README.MARKDOWN")), IconName::FileText);
        assert_eq!(file_icon(Path::new("Photo.JPG")), IconName::FileImage);
    }

    #[test]
    fn unknown_and_extensionless_files_get_the_plain_file() {
        assert_eq!(file_icon(Path::new("Makefile")), IconName::File);
        assert_eq!(file_icon(Path::new("archive.xyz")), IconName::File);
    }

    #[test]
    fn colours_differ_across_families() {
        let md = file_icon_color(Path::new("a.md"));
        let rs = file_icon_color(Path::new("a.rs"));
        let png = file_icon_color(Path::new("a.png"));
        let toml = file_icon_color(Path::new("a.toml"));
        assert_ne!(md, rs);
        assert_ne!(rs, png);
        assert_ne!(png, toml);
        assert_ne!(md, folder_icon_color());
    }
}
