//! The icon a file draws with, by type — and a colour so types stay
//! distinguishable (VS Code–ish file icons, not monochrome mud).

use std::path::Path;

use gpui_kit::{Hsla, assets::IconName, rgb};

/// The icon for `path`, chosen by its extension (and a few well-known names).
/// Anything unrecognised — a file with no extension, or one we have no icon
/// for — gets a plain file.
pub fn file_icon(path: &Path) -> IconName {
    if let Some(icon) = icon_for_special_name(path) {
        return icon;
    }

    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);

    match extension.as_deref() {
        Some("md" | "markdown" | "mdx" | "mdown" | "mkd" | "mkdn") => IconName::FileText,
        Some(
            "rs" | "py" | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" | "go" | "c"
            | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "cs" | "java" | "kt" | "kts" | "swift"
            | "rb" | "php" | "scala" | "zig" | "lua" | "ex" | "exs" | "erl" | "hs" | "clj" | "cljs"
            | "vue" | "svelte" | "astro" | "dart" | "r" | "nim" | "v" | "ml" | "mli" | "fs" | "fsx"
            | "f90" | "f95" | "jl" | "groovy" | "gradle",
        ) => IconName::FileCode,
        Some("html" | "htm" | "xhtml" | "css" | "scss" | "sass" | "less" | "styl") => {
            IconName::CodeXml
        }
        Some("sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd" | "psm1") => {
            IconName::FileTerminal
        }
        Some(
            "json" | "jsonc" | "json5" | "toml" | "yaml" | "yml" | "xml" | "ini" | "cfg" | "conf"
            | "env" | "properties" | "editorconfig" | "graphql" | "gql" | "prisma" | "proto"
            | "cmake" | "nix" | "tf" | "tfvars" | "hcl",
        ) => IconName::FileBraces,
        Some(
            "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "bmp" | "ico" | "avif" | "tif"
            | "tiff" | "heic" | "heif",
        ) => IconName::FileImage,
        Some("csv" | "tsv" | "ods" | "xls" | "xlsx") => IconName::FileSpreadsheet,
        Some("sql" | "sqlite" | "db" | "mdb") => IconName::Database,
        Some("pdf" | "doc" | "docx" | "odt" | "rtf" | "pages" | "epub") => IconName::FileType,
        // A Typst source shares the icon of what its preview compiles to.
        Some("typ" | "tex" | "latex" | "ltx" | "bib" | "sty" | "cls") => IconName::FileType,
        Some("txt" | "text" | "log" | "rst" | "adoc" | "org" | "nfo") => IconName::FileText,
        Some(
            "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "zst" | "lz4" | "cab"
            | "deb" | "rpm" | "apk",
        ) => IconName::FileArchive,
        Some("mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "wma" | "opus" | "aiff") => {
            IconName::FileMusic
        }
        Some("mp4" | "mkv" | "mov" | "webm" | "avi" | "m4v" | "wmv" | "flv") => {
            IconName::FileVideoCamera
        }
        Some("ttf" | "otf" | "woff" | "woff2" | "eot" | "ttc") => IconName::ALargeSmall,
        Some("lock" | "sum" | "pem" | "crt" | "cer" | "key" | "p12" | "pfx" | "gpg" | "asc") => {
            IconName::FileLock
        }
        Some("wasm" | "bin" | "exe" | "dll" | "so" | "dylib" | "o" | "a" | "lib" | "class") => {
            IconName::Binary
        }
        Some("diff" | "patch") => IconName::FileDiff,
        Some("dockerfile" | "containerfile") => IconName::FileBox,
        Some("mk" | "mak" | "makefile") => IconName::FileCog,
        Some("ipynb") => IconName::FileCode,
        _ => IconName::File,
    }
}

/// Accent colour for `path`'s file-type icon. Fixed hues so types stay
/// distinct in both light and dark themes (theme accents alone collapse to mud).
pub fn file_icon_color(path: &Path) -> Hsla {
    if let Some(color) = color_for_special_name(path) {
        return color;
    }

    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);

    match extension.as_deref() {
        Some(
            "md" | "markdown" | "mdx" | "mdown" | "mkd" | "mkdn" | "txt" | "text" | "log" | "rst"
            | "adoc" | "org" | "nfo",
        ) => rgb(0x519aba).into(), // blue
        Some(
            "rs" | "py" | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" | "go" | "c"
            | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "cs" | "java" | "kt" | "kts" | "swift"
            | "rb" | "php" | "scala" | "zig" | "lua" | "ex" | "exs" | "erl" | "hs" | "clj" | "cljs"
            | "vue" | "svelte" | "astro" | "dart" | "r" | "nim" | "v" | "ml" | "mli" | "fs" | "fsx"
            | "f90" | "f95" | "jl" | "groovy" | "gradle" | "ipynb",
        ) => rgb(0x8bc34a).into(), // green
        Some("html" | "htm" | "xhtml") => rgb(0xe57373).into(), // soft red
        Some("css" | "scss" | "sass" | "less" | "styl") => rgb(0x64b5f6).into(), // light blue
        Some("sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd" | "psm1") => {
            rgb(0x4db6ac).into() // teal
        }
        Some(
            "json" | "jsonc" | "json5" | "toml" | "yaml" | "yml" | "xml" | "ini" | "cfg" | "conf"
            | "env" | "properties" | "editorconfig" | "graphql" | "gql" | "prisma" | "proto"
            | "cmake" | "nix" | "tf" | "tfvars" | "hcl",
        ) => rgb(0xffca28).into(), // amber
        Some(
            "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "bmp" | "ico" | "avif" | "tif"
            | "tiff" | "heic" | "heif",
        ) => rgb(0xec407a).into(), // pink
        Some("csv" | "tsv" | "ods" | "xls" | "xlsx" | "sql" | "sqlite" | "db" | "mdb") => {
            rgb(0x66bb6a).into() // soft green
        }
        Some(
            "pdf" | "doc" | "docx" | "odt" | "rtf" | "pages" | "epub" | "typ" | "tex" | "latex"
            | "ltx" | "bib" | "sty" | "cls",
        ) => rgb(0xef5350).into(), // red
        Some(
            "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "zst" | "lz4" | "cab"
            | "deb" | "rpm" | "apk",
        ) => rgb(0xffa726).into(), // orange
        Some("mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "wma" | "opus" | "aiff") => {
            rgb(0xab47bc).into() // purple
        }
        Some("mp4" | "mkv" | "mov" | "webm" | "avi" | "m4v" | "wmv" | "flv") => {
            rgb(0x7e57c2).into() // deep purple
        }
        Some("ttf" | "otf" | "woff" | "woff2" | "eot" | "ttc") => rgb(0x8d6e63).into(), // brown
        Some("lock" | "sum" | "pem" | "crt" | "cer" | "key" | "p12" | "pfx" | "gpg" | "asc") => {
            rgb(0x78909c).into() // blue-grey
        }
        Some("wasm" | "bin" | "exe" | "dll" | "so" | "dylib" | "o" | "a" | "lib" | "class") => {
            rgb(0x607d8b).into() // slate
        }
        Some("diff" | "patch") => rgb(0x26a69a).into(), // teal-green
        Some("dockerfile" | "containerfile") => rgb(0x29b6f6).into(), // sky
        Some("mk" | "mak" | "makefile") => rgb(0xffca28).into(), // amber
        _ => rgb(0x90a4ae).into(),                      // blue-grey
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

/// Well-known basenames (often extensionless) that still deserve a typed icon.
fn special_basename(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_ascii_lowercase())
}

fn icon_for_special_name(path: &Path) -> Option<IconName> {
    let name = special_basename(path)?;
    Some(match name.as_str() {
        "dockerfile" | "containerfile" | "dockerfile.dev" | "dockerfile.prod" => IconName::FileBox,
        "makefile" | "gnumakefile" | "cmakelists.txt" => IconName::FileCog,
        "cargo.toml"
        | "package.json"
        | "composer.json"
        | "go.mod"
        | "pyproject.toml"
        | "requirements.txt"
        | "gemfile"
        | "podfile"
        | "build.gradle"
        | "build.gradle.kts"
        | "settings.gradle"
        | "settings.gradle.kts" => IconName::FileBraces,
        "cargo.lock" | "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock" | "go.sum"
        | "gemfile.lock" | "poetry.lock" | "composer.lock" | "bun.lockb" => IconName::FileLock,
        ".gitignore"
        | ".gitattributes"
        | ".gitmodules"
        | ".gitkeep"
        | ".dockerignore"
        | ".editorconfig"
        | ".env"
        | ".env.local"
        | ".env.example"
        | ".npmrc"
        | ".nvmrc"
        | ".eslintrc"
        | ".eslintrc.js"
        | ".eslintrc.cjs"
        | ".eslintrc.json"
        | ".prettierrc"
        | ".prettierrc.json"
        | ".prettierrc.yml"
        | "tsconfig.json"
        | "jsconfig.json"
        | "rust-toolchain"
        | "rust-toolchain.toml" => IconName::FileBraces,
        "license" | "license.md" | "license.txt" | "copying" | "notice" | "authors"
        | "contributors" => IconName::FileText,
        "readme" | "readme.md" | "readme.txt" | "changelog" | "changelog.md" | "changes"
        | "history" | "history.md" => IconName::FileText,
        "procfile" => IconName::FileTerminal,
        "vagrantfile" | "rakefile" | "fastfile" | "appfile" | "matchfile" => IconName::FileCode,
        _ if name.starts_with("dockerfile.") => IconName::FileBox,
        _ => return None,
    })
}

fn color_for_special_name(path: &Path) -> Option<Hsla> {
    let name = special_basename(path)?;
    Some(match name.as_str() {
        "dockerfile" | "containerfile" | "dockerfile.dev" | "dockerfile.prod" => {
            rgb(0x29b6f6).into()
        }
        "makefile" | "gnumakefile" | "cmakelists.txt" => rgb(0xffca28).into(),
        "cargo.toml"
        | "package.json"
        | "composer.json"
        | "go.mod"
        | "pyproject.toml"
        | "requirements.txt"
        | "gemfile"
        | "podfile"
        | "build.gradle"
        | "build.gradle.kts"
        | "settings.gradle"
        | "settings.gradle.kts" => rgb(0xffca28).into(),
        "cargo.lock" | "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock" | "go.sum"
        | "gemfile.lock" | "poetry.lock" | "composer.lock" | "bun.lockb" => rgb(0x78909c).into(),
        ".gitignore"
        | ".gitattributes"
        | ".gitmodules"
        | ".gitkeep"
        | ".dockerignore"
        | ".editorconfig"
        | ".env"
        | ".env.local"
        | ".env.example"
        | ".npmrc"
        | ".nvmrc"
        | ".eslintrc"
        | ".eslintrc.js"
        | ".eslintrc.cjs"
        | ".eslintrc.json"
        | ".prettierrc"
        | ".prettierrc.json"
        | ".prettierrc.yml"
        | "tsconfig.json"
        | "jsconfig.json"
        | "rust-toolchain"
        | "rust-toolchain.toml" => rgb(0xffca28).into(),
        "license" | "license.md" | "license.txt" | "copying" | "notice" | "authors"
        | "contributors" | "readme" | "readme.md" | "readme.txt" | "changelog" | "changelog.md"
        | "changes" | "history" | "history.md" => rgb(0x519aba).into(),
        "procfile" => rgb(0x4db6ac).into(),
        "vagrantfile" | "rakefile" | "fastfile" | "appfile" | "matchfile" => rgb(0x8bc34a).into(),
        _ if name.starts_with("dockerfile.") => rgb(0x29b6f6).into(),
        _ => return None,
    })
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
        assert_eq!(file_icon(Path::new("index.html")), IconName::CodeXml);
        assert_eq!(file_icon(Path::new("styles.css")), IconName::CodeXml);
        assert_eq!(file_icon(Path::new("query.sql")), IconName::Database);
        assert_eq!(file_icon(Path::new("bundle.zip")), IconName::FileArchive);
        assert_eq!(file_icon(Path::new("track.mp3")), IconName::FileMusic);
        assert_eq!(file_icon(Path::new("clip.mp4")), IconName::FileVideoCamera);
        assert_eq!(file_icon(Path::new("font.woff2")), IconName::ALargeSmall);
        assert_eq!(file_icon(Path::new("Cargo.lock")), IconName::FileLock);
        assert_eq!(file_icon(Path::new("fix.diff")), IconName::FileDiff);
        assert_eq!(file_icon(Path::new("schema.proto")), IconName::FileBraces);
    }

    #[test]
    fn the_extension_is_matched_regardless_of_case() {
        assert_eq!(file_icon(Path::new("README.MARKDOWN")), IconName::FileText);
        assert_eq!(file_icon(Path::new("Photo.JPG")), IconName::FileImage);
        assert_eq!(file_icon(Path::new("Archive.ZIP")), IconName::FileArchive);
    }

    #[test]
    fn unknown_and_extensionless_files_get_the_plain_file() {
        assert_eq!(file_icon(Path::new("oddname")), IconName::File);
        assert_eq!(file_icon(Path::new("archive.xyz")), IconName::File);
    }

    #[test]
    fn well_known_basenames_get_typed_icons() {
        assert_eq!(file_icon(Path::new("Dockerfile")), IconName::FileBox);
        assert_eq!(file_icon(Path::new("Makefile")), IconName::FileCog);
        assert_eq!(file_icon(Path::new("CMakeLists.txt")), IconName::FileCog);
        assert_eq!(file_icon(Path::new(".gitignore")), IconName::FileBraces);
        assert_eq!(file_icon(Path::new("LICENSE")), IconName::FileText);
        assert_eq!(file_icon(Path::new("dockerfile.prod")), IconName::FileBox);
    }

    #[test]
    fn colours_differ_across_families() {
        let md = file_icon_color(Path::new("a.md"));
        let rs = file_icon_color(Path::new("a.rs"));
        let png = file_icon_color(Path::new("a.png"));
        let toml = file_icon_color(Path::new("a.toml"));
        let html = file_icon_color(Path::new("a.html"));
        let zip = file_icon_color(Path::new("a.zip"));
        assert_ne!(md, rs);
        assert_ne!(rs, png);
        assert_ne!(png, toml);
        assert_ne!(html, rs);
        assert_ne!(zip, png);
        assert_ne!(md, folder_icon_color());
        assert_ne!(
            file_icon_color(Path::new("Dockerfile")),
            folder_icon_color()
        );
    }

    #[test]
    fn lockfiles_prefer_lock_icon_over_extension() {
        // `.lock` extension wins for generic locks; Cargo.lock is also braces
        // via special name — either typed icon is fine; must not be plain File.
        assert_ne!(file_icon(Path::new("yarn.lock")), IconName::File);
        assert_ne!(file_icon(Path::new("app.lock")), IconName::File);
    }
}
