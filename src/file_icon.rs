//! The icon a file draws with, by type.
//!
//! VS Code gives every file type its own icon; Malgel drew one `FileText` for
//! everything. The bundled GPUI Kit icon set already carries a `file-*` family,
//! so telling the types apart costs no new dependency and no new asset: the
//! file tree, the tab strip and the Quick Open list all share this mapping.

use std::path::Path;

use gpui_kit::assets::IconName;

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
}
