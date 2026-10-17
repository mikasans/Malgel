//! User preferences persisted between sessions.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const RECENT_LIMIT: usize = 10;
pub const DEFAULT_FONT_SIZE: f32 = 17.5;
pub const MIN_FONT_SIZE: f32 = 12.;
pub const MAX_FONT_SIZE: f32 = 24.;

/// Where a document opened from a link in the preview goes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkPlacement {
    /// A tab of its own, keeping the linking document open.
    #[default]
    NewTab,
    /// In place of the linking document, like a browser.
    SameTab,
}

/// Light or dark appearance, or follow the system.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

/// Which panes the workspace shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    Editor,
    #[default]
    Split,
    Preview,
}

impl Layout {
    pub fn shows_editor(self) -> bool {
        self != Layout::Preview
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    /// Theme used in light appearance; `None` is the built-in default.
    pub light_theme: Option<String>,
    /// Theme used in dark appearance; `None` is the built-in default.
    pub dark_theme: Option<String>,
    /// Base interface size in pixels; everything scales from it.
    pub font_size: f32,
    pub layout: Layout,
    pub soft_wrap: bool,
    /// Treat single newlines in paragraphs as hard breaks in the preview (GFM breaks).
    pub gfm_breaks: bool,
    pub line_numbers: bool,
    /// Keep the preview scrolled to the part of the document being edited.
    pub scroll_sync: bool,
    pub recent_files: Vec<PathBuf>,

    // Optional features. Off, Malgel is a single-document editor; each one
    // turned on adds to the window.
    /// Open documents in tabs instead of replacing the current one.
    pub tabs: bool,
    /// Show a folder's Markdown files beside the editor.
    pub file_sidebar: bool,
    /// Show the document's headings beside the editor.
    pub outline_sidebar: bool,
    /// Underline unknown words.
    pub spell_check: bool,
    /// Dictionary to check against, like `en_US`; `None` follows the system.
    pub spell_language: Option<String>,
    /// Reopen the documents, folder and window of the last session.
    pub restore_session: bool,
    /// Folder, next to the document, that pasted and dropped images are
    /// saved into.
    pub image_folder: String,
    /// Where documents opened from links go.
    pub open_links_in: LinkPlacement,
    /// Ask before opening a web link in the browser.
    pub confirm_web_links: bool,
    /// Ask before the file tree moves a dragged entry (VS Code's
    /// `explorer.confirmDragAndDrop`).
    pub confirm_drag_and_drop: bool,
    /// Show .git and the other entries VS Code hides by default.
    pub show_hidden_entries: bool,
    /// Center the text column in a wide editor, line numbers with it (VS
    /// Code's Centered Layout). Off: the line numbers sit at the left edge.
    pub centered_layout: bool,

    /// Hide everything but the text, dimming all but the current paragraph.
    /// Lasts until turned off or Malgel quits.
    #[serde(skip)]
    pub focus_mode: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            appearance: Appearance::System,
            light_theme: None,
            dark_theme: None,
            font_size: DEFAULT_FONT_SIZE,
            layout: Layout::Split,
            soft_wrap: true,
            gfm_breaks: false,
            line_numbers: true,
            scroll_sync: true,
            recent_files: Vec::new(),
            tabs: true,
            show_hidden_entries: false,
            centered_layout: false,
            file_sidebar: false,
            outline_sidebar: false,
            spell_check: false,
            spell_language: None,
            restore_session: true,
            image_folder: "assets".to_string(),
            open_links_in: LinkPlacement::NewTab,
            confirm_web_links: true,
            confirm_drag_and_drop: true,
            focus_mode: false,
        }
    }
}

impl Settings {
    /// Load settings, falling back to defaults when the file is missing or
    /// unreadable. Out-of-range values are clamped.
    pub fn load() -> Self {
        let Some(path) = settings_path() else {
            return Self::default();
        };
        // Another editor's settings.json in the same folder (e.g. Zed-style
        // `{"font_size":14,"theme":…}`) must not become Malgel's UI size —
        // serde would keep font_size and fill the rest from Default, so a
        // cold open would show ~80% of [`DEFAULT_FONT_SIZE`] (17.5).
        let mut settings = std::fs::read_to_string(&path)
            .ok()
            .and_then(|json| {
                let value: serde_json::Value = serde_json::from_str(&json).ok()?;
                let obj = value.as_object()?;
                if !obj.contains_key("appearance") && !obj.contains_key("layout") {
                    return None;
                }
                serde_json::from_value::<Settings>(value).ok()
            })
            .unwrap_or_default();
        settings.font_size = clamp_font_size(settings.font_size);
        settings.recent_files.truncate(RECENT_LIMIT);
        let folder = settings.image_folder.trim().trim_matches(['/', '\\']);
        settings.image_folder = if folder.is_empty() || folder.contains("..") {
            "assets".to_string()
        } else {
            folder.to_string()
        };
        settings
    }

    /// Persist settings. Failures are not fatal: preferences simply are not
    /// remembered.
    pub fn save(&self) {
        let Some(path) = settings_path() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Move `path` to the top of the recent files list.
    pub fn push_recent(&mut self, path: &Path) {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.recent_files.retain(|recent| recent != &path);
        self.recent_files.insert(0, path);
        self.recent_files.truncate(RECENT_LIMIT);
    }
}

pub fn clamp_font_size(size: f32) -> f32 {
    if size.is_finite() {
        size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
    } else {
        DEFAULT_FONT_SIZE
    }
}

/// UI zoom as a percentage of [`DEFAULT_FONT_SIZE`], for the status bar.
pub fn zoom_percent(font_size: f32) -> u32 {
    let size = clamp_font_size(font_size);
    ((size / DEFAULT_FONT_SIZE) * 100.).round() as u32
}

fn settings_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("settings.json"))
}

/// Where Malgel keeps its settings, session and recovered documents.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("MALGEL_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("Malgel"))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support/Malgel"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|dir| dir.join("malgel"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_use_defaults() {
        let settings: Settings = serde_json::from_str(r#"{ "layout": "preview" }"#).unwrap();
        assert_eq!(settings.layout, Layout::Preview);
        assert!(settings.soft_wrap);
        assert_eq!(settings.font_size, DEFAULT_FONT_SIZE);
        // Tabs and session restore start on; other optional features start off.
        assert!(settings.tabs && !settings.file_sidebar && !settings.spell_check);
        assert!(settings.restore_session);
        assert_eq!(settings.open_links_in, LinkPlacement::NewTab);
        assert!(settings.confirm_web_links);
    }

    #[test]
    fn recent_files_are_unique_and_bounded() {
        let mut settings = Settings::default();
        for ix in 0..12 {
            settings.push_recent(Path::new(&format!("/tmp/{ix}.md")));
        }
        settings.push_recent(Path::new("/tmp/5.md"));
        assert_eq!(settings.recent_files.len(), RECENT_LIMIT);
        assert_eq!(settings.recent_files[0], PathBuf::from("/tmp/5.md"));
        assert_eq!(
            settings
                .recent_files
                .iter()
                .filter(|path| path.ends_with("5.md"))
                .count(),
            1
        );
    }

    #[test]
    fn font_size_is_clamped() {
        assert_eq!(clamp_font_size(100.), MAX_FONT_SIZE);
        assert_eq!(clamp_font_size(f32::NAN), DEFAULT_FONT_SIZE);
    }

    #[test]
    fn zoom_percent_is_relative_to_default() {
        assert_eq!(zoom_percent(DEFAULT_FONT_SIZE), 100);
        assert_eq!(zoom_percent(DEFAULT_FONT_SIZE + 1.), 106);
        assert_eq!(zoom_percent(DEFAULT_FONT_SIZE - 1.), 94);
    }

    #[test]
    fn foreign_settings_json_does_not_steal_font_size() {
        // Same shape as a non-Malgel settings file that once landed in the
        // inspect HOME and made status read 80% on a fresh window.
        let foreign = r#"{ "theme": "Default Dark", "font_size": 14, "ui_font_size": 14 }"#;
        let value: serde_json::Value = serde_json::from_str(foreign).unwrap();
        let obj = value.as_object().unwrap();
        assert!(!obj.contains_key("appearance") && !obj.contains_key("layout"));
        // load() returns Default when the file is not Malgel's — font stays 17.5.
        assert_eq!(Settings::default().font_size, DEFAULT_FONT_SIZE);
        assert_eq!(zoom_percent(Settings::default().font_size), 100);
    }
}
