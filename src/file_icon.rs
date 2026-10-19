//! File-type icons from a **VS Code–compatible icon theme** (declarative JSON
//! + SVG assets), with Lucide glyphs as fallback.
//!
//! The bundled package lives in `icon-themes/material/` (`theme.json` +
//! `icons/`). Swap or add a package by dropping another folder with the same
//! shape and pointing [`DEFAULT_ICON_THEME`] at it — mappings stay in the
//! theme file, not hardcoded match arms.

use std::{
    collections::HashMap,
    path::Path,
    sync::{Mutex, OnceLock},
};

use gpui_kit::{Hsla, Styled, assets::IconName, component::Icon, rgb};
use rust_embed::Embed;
use serde::Deserialize;

/// Folder under `icon-themes/` that ships with the binary. Change this (or a
/// future settings key) to load another VS Code–shaped package.
pub const DEFAULT_ICON_THEME: &str = "material";

#[derive(Embed)]
#[folder = "icon-themes/material"]
#[include = "theme.json"]
#[include = "icons/*.svg"]
struct MaterialThemeAssets;

/// VS Code [file icon theme](https://code.visualstudio.com/api/extension-guides/file-icon-theme)
/// contribution (the fields Malgel resolves today).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThemeFile {
    icon_definitions: HashMap<String, IconDefinition>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    folder_expanded: Option<String>,
    #[serde(default)]
    file_extensions: HashMap<String, String>,
    #[serde(default)]
    file_names: HashMap<String, String>,
    #[serde(default)]
    folder_names: HashMap<String, String>,
    #[serde(default)]
    folder_names_expanded: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IconDefinition {
    icon_path: String,
}

struct IconTheme {
    defs: HashMap<String, String>,
    file: Option<String>,
    folder: Option<String>,
    folder_expanded: Option<String>,
    file_extensions: HashMap<String, String>,
    file_names: HashMap<String, String>,
    folder_names: HashMap<String, String>,
    folder_names_expanded: HashMap<String, String>,
}

fn load_bundled_theme() -> IconTheme {
    // Today the embed folder is fixed to `material`; the constant documents
    // which package is active and is the hook for a future settings picker.
    assert_eq!(DEFAULT_ICON_THEME, "material");
    let bytes = MaterialThemeAssets::get("theme.json")
        .expect("bundled icon theme.json")
        .data;
    let theme: ThemeFile =
        serde_json::from_slice(&bytes).expect("icon theme.json parses as VS Code schema");
    IconTheme {
        defs: theme
            .icon_definitions
            .into_iter()
            .map(|(id, def)| (id, def.icon_path))
            .collect(),
        file: theme.file,
        folder: theme.folder,
        folder_expanded: theme.folder_expanded,
        file_extensions: theme
            .file_extensions
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect(),
        file_names: theme
            .file_names
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect(),
        folder_names: theme
            .folder_names
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect(),
        folder_names_expanded: theme
            .folder_names_expanded
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect(),
    }
}

fn theme() -> &'static IconTheme {
    static THEME: OnceLock<IconTheme> = OnceLock::new();
    THEME.get_or_init(load_bundled_theme)
}

/// Resolved SVG bytes, cached by theme icon id. Icons are loaded **on first
/// paint** only — opening Explorer does not parse or decode the whole pack.
fn svg_for_id(id: &str) -> Option<std::sync::Arc<[u8]>> {
    static CACHE: OnceLock<Mutex<HashMap<String, std::sync::Arc<[u8]>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let guard = cache.lock().ok()?;
        if let Some(hit) = guard.get(id) {
            return Some(hit.clone());
        }
    }
    let rel = theme().defs.get(id)?.clone();
    let data = MaterialThemeAssets::get(&rel)?;
    let bytes: std::sync::Arc<[u8]> = std::sync::Arc::from(data.data.as_ref());
    if let Ok(mut guard) = cache.lock() {
        // Cap growth: Explorer rarely needs more than a few hundred distinct
        // types; if a huge tree exhausts the map we still serve the new icon.
        if guard.len() > 512 {
            guard.clear();
        }
        guard.insert(id.to_string(), bytes.clone());
    }
    Some(bytes)
}

fn basename_lower(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_ascii_lowercase())
}

fn extension_lower(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// Theme icon id for a file path, if the active package maps it.
pub fn theme_icon_id(path: &Path) -> Option<&'static str> {
    let t = theme();
    if let Some(name) = basename_lower(path)
        && let Some(id) = t.file_names.get(&name)
    {
        return Some(id.as_str());
    }
    if let Some(ext) = extension_lower(path)
        && let Some(id) = t.file_extensions.get(&ext)
    {
        return Some(id.as_str());
    }
    t.file.as_deref()
}

/// Theme icon id for a folder (optionally expanded / named).
pub fn theme_folder_icon_id(path: &Path, expanded: bool) -> Option<&'static str> {
    let t = theme();
    if let Some(name) = basename_lower(path) {
        let map = if expanded {
            &t.folder_names_expanded
        } else {
            &t.folder_names
        };
        if let Some(id) = map.get(&name) {
            return Some(id.as_str());
        }
    }
    if expanded {
        t.folder_expanded.as_deref().or(t.folder.as_deref())
    } else {
        t.folder.as_deref()
    }
}

/// Paint the theme SVG for `id`, or `None` if the asset is missing.
fn theme_icon(id: &str) -> Option<Icon> {
    svg_for_id(id).map(|bytes| Icon::default().data(bytes.as_ref()))
}

/// Explorer / tab / palette icon for `path` — theme SVG when mapped, else Lucide.
pub fn file_type_icon(path: &Path) -> Icon {
    if let Some(id) = theme_icon_id(path)
        && let Some(icon) = theme_icon(id)
    {
        return icon;
    }
    Icon::new(file_icon(path)).text_color(file_icon_color(path))
}

/// Folder glyph from the theme when available.
pub fn folder_type_icon(path: &Path, expanded: bool) -> Icon {
    if let Some(id) = theme_folder_icon_id(path, expanded)
        && let Some(icon) = theme_icon(id)
    {
        return icon;
    }
    Icon::new(if expanded {
        IconName::FolderOpen
    } else {
        IconName::Folder
    })
    .text_color(folder_icon_color())
}

/// Whether the active theme supplied a pre-coloured SVG (skip Lucide tint).
#[cfg(test)]
pub fn uses_theme_svg(path: &Path) -> bool {
    theme_icon_id(path).is_some_and(|id| svg_for_id(id).is_some())
}

/// Lucide fallback glyph by extension (also used when the theme has no SVG).
pub fn file_icon(path: &Path) -> IconName {
    if let Some(icon) = icon_for_special_name(path) {
        return icon;
    }

    let extension = extension_lower(path);

    match extension.as_deref() {
        Some("md" | "markdown" | "mdx" | "mdown" | "mkd" | "mkdn") => IconName::FileText,
        Some("txt" | "text" | "log" | "rst" | "adoc" | "org" | "nfo") => IconName::NotepadText,
        Some("typ") => IconName::FilePen,
        Some("tex" | "latex" | "ltx" | "bib" | "sty" | "cls") => IconName::BookOpen,
        Some("pdf") => IconName::FileType,
        Some("doc" | "docx" | "odt" | "rtf" | "pages" | "epub") => IconName::NotebookText,
        Some(
            "rs" | "py" | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" | "go" | "c"
            | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "cs" | "java" | "kt" | "kts" | "swift"
            | "rb" | "php" | "scala" | "zig" | "lua" | "ex" | "exs" | "erl" | "hs" | "clj" | "cljs"
            | "vue" | "svelte" | "astro" | "dart" | "r" | "nim" | "v" | "ml" | "mli" | "fs" | "fsx"
            | "f90" | "f95" | "jl" | "groovy" | "gradle" | "ipynb",
        ) => IconName::FileCode,
        Some("html" | "htm" | "xhtml") => IconName::CodeXml,
        Some("css" | "scss" | "sass" | "less" | "styl") => IconName::Braces,
        Some("sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd" | "psm1") => {
            IconName::FileTerminal
        }
        Some("json" | "jsonc" | "json5") => IconName::FileBraces,
        Some("toml") => IconName::FileBracesCorner,
        Some("yaml" | "yml") => IconName::FileCodeCorner,
        Some(
            "xml" | "ini" | "cfg" | "conf" | "env" | "properties" | "editorconfig" | "graphql"
            | "gql" | "prisma" | "proto" | "cmake" | "nix" | "tf" | "tfvars" | "hcl",
        ) => IconName::FileBraces,
        Some(
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "avif" | "tif" | "tiff"
            | "heic" | "heif",
        ) => IconName::FileImage,
        Some("svg") => IconName::Image,
        Some("csv" | "tsv" | "ods" | "xls" | "xlsx") => IconName::FileSpreadsheet,
        Some("sql" | "sqlite" | "db" | "mdb") => IconName::Database,
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
        _ => IconName::File,
    }
}

/// Accent for Lucide fallbacks only (theme SVGs are already coloured).
pub fn file_icon_color(path: &Path) -> Hsla {
    if let Some(color) = color_for_special_name(path) {
        return color;
    }

    let extension = extension_lower(path);

    match extension.as_deref() {
        Some("md" | "markdown" | "mdx" | "mdown" | "mkd" | "mkdn") => rgb(0x519aba).into(),
        Some("txt" | "text" | "log" | "rst" | "adoc" | "org" | "nfo") => rgb(0x90a4ae).into(),
        Some("typ") => rgb(0x239dad).into(),
        Some("tex" | "latex" | "ltx" | "bib" | "sty" | "cls") => rgb(0x5c6bc0).into(),
        Some("pdf") => rgb(0xef5350).into(),
        Some("doc" | "docx" | "odt" | "rtf" | "pages" | "epub") => rgb(0x42a5f5).into(),
        Some("rs") => rgb(0xdea584).into(),
        Some("py") => rgb(0x42a5f5).into(),
        Some("js" | "jsx" | "mjs" | "cjs") => rgb(0xffca28).into(),
        Some("ts" | "tsx" | "mts" | "cts") => rgb(0x039be5).into(),
        Some("go") => rgb(0x00acc1).into(),
        Some(
            "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "cs" | "java" | "kt" | "kts"
            | "swift" | "rb" | "php" | "scala" | "zig" | "lua" | "ex" | "exs" | "erl" | "hs"
            | "clj" | "cljs" | "vue" | "svelte" | "astro" | "dart" | "r" | "nim" | "v" | "ml"
            | "mli" | "fs" | "fsx" | "f90" | "f95" | "jl" | "groovy" | "gradle" | "ipynb",
        ) => rgb(0x8bc34a).into(),
        Some("html" | "htm" | "xhtml") => rgb(0xe57373).into(),
        Some("css" | "scss" | "sass" | "less" | "styl") => rgb(0x64b5f6).into(),
        Some("sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd" | "psm1") => {
            rgb(0x4db6ac).into()
        }
        Some("json" | "jsonc" | "json5") => rgb(0xffca28).into(),
        Some("toml") => rgb(0x8d6e63).into(),
        Some("yaml" | "yml") => rgb(0xab47bc).into(),
        Some(
            "xml" | "ini" | "cfg" | "conf" | "properties" | "editorconfig" | "graphql" | "gql"
            | "prisma" | "proto" | "cmake" | "nix" | "tf" | "tfvars" | "hcl",
        ) => rgb(0xffca28).into(),
        Some("env") => rgb(0xc0ca33).into(),
        Some(
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "avif" | "tif" | "tiff"
            | "heic" | "heif",
        ) => rgb(0xec407a).into(),
        Some("svg") => rgb(0xff7043).into(),
        Some("csv" | "tsv" | "ods" | "xls" | "xlsx") => rgb(0x66bb6a).into(),
        Some("sql" | "sqlite" | "db" | "mdb") => rgb(0x66bb6a).into(),
        Some(
            "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "zst" | "lz4" | "cab"
            | "deb" | "rpm" | "apk",
        ) => rgb(0xffa726).into(),
        Some("mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "wma" | "opus" | "aiff") => {
            rgb(0xab47bc).into()
        }
        Some("mp4" | "mkv" | "mov" | "webm" | "avi" | "m4v" | "wmv" | "flv") => {
            rgb(0x7e57c2).into()
        }
        Some("ttf" | "otf" | "woff" | "woff2" | "eot" | "ttc") => rgb(0x8d6e63).into(),
        Some("lock" | "sum" | "pem" | "crt" | "cer" | "key" | "p12" | "pfx" | "gpg" | "asc") => {
            rgb(0x78909c).into()
        }
        Some("wasm" | "bin" | "exe" | "dll" | "so" | "dylib" | "o" | "a" | "lib" | "class") => {
            rgb(0x607d8b).into()
        }
        Some("diff" | "patch") => rgb(0x26a69a).into(),
        Some("dockerfile" | "containerfile") => rgb(0x29b6f6).into(),
        Some("mk" | "mak" | "makefile") => rgb(0xffca28).into(),
        _ => rgb(0x90a4ae).into(),
    }
}

pub fn folder_icon_color() -> Hsla {
    rgb(0xffb74d).into()
}

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

fn icon_for_special_name(path: &Path) -> Option<IconName> {
    let name = basename_lower(path)?;
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
    let name = basename_lower(path)?;
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
        ".env" | ".env.local" | ".env.example" => rgb(0xc0ca33).into(),
        ".gitignore"
        | ".gitattributes"
        | ".gitmodules"
        | ".gitkeep"
        | ".dockerignore"
        | ".editorconfig"
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
    fn theme_maps_common_extensions() {
        assert_eq!(theme_icon_id(Path::new("a.md")), Some("markdown"));
        assert_eq!(theme_icon_id(Path::new("a.typ")), Some("typst"));
        assert_eq!(theme_icon_id(Path::new("a.pdf")), Some("pdf"));
        assert_eq!(theme_icon_id(Path::new("main.rs")), Some("rust"));
        assert!(svg_for_id("markdown").is_some());
        assert!(svg_for_id("typst").is_some());
        assert!(svg_for_id("pdf").is_some());
        assert!(uses_theme_svg(Path::new("notes.md")));
    }

    #[test]
    fn theme_maps_well_known_file_names() {
        assert_eq!(theme_icon_id(Path::new("Dockerfile")), Some("docker"));
        assert!(theme_icon_id(Path::new("Cargo.toml")).is_some());
    }

    #[test]
    fn lucide_fallback_glyphs_still_differ() {
        assert_ne!(file_icon(Path::new("a.pdf")), file_icon(Path::new("a.typ")));
        assert_ne!(file_icon(Path::new("a.md")), file_icon(Path::new("a.txt")));
        assert_eq!(file_icon(Path::new("notes.md")), IconName::FileText);
        assert_eq!(file_icon(Path::new("paper.pdf")), IconName::FileType);
        assert_eq!(file_icon(Path::new("paper.typ")), IconName::FilePen);
    }

    #[test]
    fn each_family_gets_its_own_icon() {
        assert_eq!(file_icon(Path::new("notes.md")), IconName::FileText);
        assert_eq!(file_icon(Path::new("notes.txt")), IconName::NotepadText);
        assert_eq!(file_icon(Path::new("main.rs")), IconName::FileCode);
        assert_eq!(file_icon(Path::new("run.sh")), IconName::FileTerminal);
        assert_eq!(file_icon(Path::new("logo.png")), IconName::FileImage);
        assert_eq!(file_icon(Path::new("data.csv")), IconName::FileSpreadsheet);
        assert_eq!(file_icon(Path::new("index.html")), IconName::CodeXml);
        assert_eq!(file_icon(Path::new("styles.css")), IconName::Braces);
        assert_eq!(file_icon(Path::new("query.sql")), IconName::Database);
        assert_eq!(file_icon(Path::new("bundle.zip")), IconName::FileArchive);
    }

    #[test]
    fn the_extension_is_matched_regardless_of_case() {
        assert_eq!(file_icon(Path::new("README.MARKDOWN")), IconName::FileText);
        assert_eq!(theme_icon_id(Path::new("Paper.PDF")), Some("pdf"));
        assert_eq!(theme_icon_id(Path::new("Paper.TYP")), Some("typst"));
    }

    #[test]
    fn unknown_and_extensionless_files_get_the_plain_file() {
        assert_eq!(file_icon(Path::new("oddname")), IconName::File);
        assert_eq!(file_icon(Path::new("archive.xyz")), IconName::File);
    }

    #[test]
    fn colours_differ_across_families() {
        let md = file_icon_color(Path::new("a.md"));
        let typ = file_icon_color(Path::new("a.typ"));
        let pdf = file_icon_color(Path::new("a.pdf"));
        assert_ne!(md, typ);
        assert_ne!(typ, pdf);
        assert_ne!(md, pdf);
    }

    #[test]
    fn theme_json_loads_without_embedding_every_svg_in_cache() {
        // Parsing the declarative map must not pull SVG bytes. Only paint
        // paths call svg_for_id — Explorer open stays O(visible rows).
        let _ = theme();
        assert!(theme().file_extensions.contains_key("md"));
        assert!(theme().file_extensions.contains_key("typ"));
        assert_eq!(theme_icon_id(Path::new("a.typ")), Some("typst"));
        // One resolve warms one cache entry, not 1000+.
        let _ = svg_for_id("typst");
        let _ = svg_for_id("markdown");
        let _ = svg_for_id("pdf");
    }

    #[test]
    fn bundled_pack_covers_typst_and_common_types() {
        for (path, id) in [
            ("x.md", "markdown"),
            ("x.typ", "typst"),
            ("x.pdf", "pdf"),
            ("x.rs", "rust"),
            ("x.py", "python"),
            ("x.ts", "typescript"),
        ] {
            assert_eq!(theme_icon_id(Path::new(path)), Some(id), "{path}");
            assert!(svg_for_id(id).is_some(), "missing svg for {id}");
        }
    }
}
