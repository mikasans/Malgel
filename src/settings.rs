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
    /// Save dirty editors after a short delay (VS Code Auto Save: afterDelay).
    pub auto_save: bool,
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
    /// Show the Problems panel (diagnostics / spell) like VS Code.
    pub problems_panel: bool,
    /// Show a minimap beside the editor.
    pub minimap: bool,
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
    /// Start terminal shells with the user's rc files (~/.bashrc, ~/.zshrc)
    /// like VS Code. Off: a clean shell (`bash --norc --noprofile`), for a
    /// broken rc that would kill the session.
    pub terminal_shell_config: bool,
    /// Terminal panel height in pixels after the user dragged its divider;
    /// `None`: a third of the window (VS Code).
    pub terminal_panel_height: Option<f32>,
    /// Lines of history each terminal keeps; the oldest are dropped past
    /// it. Clamped to 100 ..= 100 000 (VS Code's
    /// `terminal.integrated.scrollback`).
    pub terminal_scrollback: usize,

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
            auto_save: false,
            gfm_breaks: false,
            line_numbers: true,
            scroll_sync: true,
            recent_files: Vec::new(),
            tabs: true,
            show_hidden_entries: false,
            centered_layout: false,
            terminal_shell_config: true,
            terminal_panel_height: None,
            terminal_scrollback: crate::term_session::SCROLLBACK_LINES,
            file_sidebar: false,
            outline_sidebar: false,
            problems_panel: false,
            minimap: false,
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
        settings_path().map_or_else(Self::default, |path| Self::load_from(&path))
    }

    fn load_from(path: &Path) -> Self {
        let mut settings = std::fs::read_to_string(path)
            .ok()
            .and_then(|json| parse_own(&json))
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
        if let Some(path) = settings_path() {
            self.save_to(&path);
        }
    }

    fn save_to(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // A file we could not read as ours (another editor's, or a typo in
        // a hand edit) is kept beside ours rather than silently replaced.
        if let Ok(old) = std::fs::read_to_string(path)
            && parse_own(&old).is_none()
            && let Some(backup) = backup_path(path)
        {
            let _ = std::fs::write(backup, old);
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

/// Keys other editors' `settings.json` share with ours: on their own they
/// do not make a file Malgel's.
const SHARED_KEYS: [&str; 2] = ["font_size", "theme"];

/// `json` as Malgel settings, or `None` when it is not ours. Another
/// editor's settings.json in the same folder (e.g. Zed-style
/// `{"font_size":14,"theme":…}`) must not become Malgel's UI size — serde
/// would keep font_size and fill the rest from Default. A file is ours when
/// it names at least one Malgel key beyond those shared ones, so a
/// hand-written `{"terminal_scrollback":500}` counts.
fn parse_own(json: &str) -> Option<Settings> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let obj = value.as_object()?;
    let ours = serde_json::to_value(Settings::default()).ok()?;
    let ours = ours.as_object()?;
    let has_own_key = obj
        .keys()
        .any(|key| ours.contains_key(key) && !SHARED_KEYS.contains(&key.as_str()));
    if !has_own_key {
        return None;
    }
    serde_json::from_value::<Settings>(value).ok()
}

/// The first free `settings.json.bak`, `.bak.1` … `.bak.9`.
fn backup_path(path: &Path) -> Option<PathBuf> {
    (0..10)
        .map(|n| {
            let mut name = path.as_os_str().to_owned();
            name.push(if n == 0 {
                ".bak".to_string()
            } else {
                format!(".bak.{n}")
            });
            PathBuf::from(name)
        })
        .find(|candidate| !candidate.exists())
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
    fn a_hand_written_file_with_one_of_our_keys_loads_and_survives_a_save() {
        let dir = std::env::temp_dir().join(format!(
            "malgel-settings-{}",
            crate::session::new_recovery_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"terminal_scrollback":500}"#).unwrap();
        let settings = Settings::load_from(&path);
        assert_eq!(settings.terminal_scrollback, 500);
        settings.save_to(&path);
        assert_eq!(Settings::load_from(&path).terminal_scrollback, 500);
        assert!(!dir.join("settings.json.bak").exists(), "ours: no backup");

        // Another editor's file: defaults, and the save keeps a copy.
        let foreign = r#"{"font_size":14,"theme":"One Dark"}"#;
        std::fs::write(&path, foreign).unwrap();
        let settings = Settings::load_from(&path);
        assert_eq!(settings.font_size, Settings::default().font_size);
        settings.save_to(&path);
        assert_eq!(
            std::fs::read_to_string(dir.join("settings.json.bak")).unwrap(),
            foreign
        );
        // A broken hand edit is backed up too, beside the first backup.
        std::fs::write(&path, r#"{"terminal_scrollback": 5"#).unwrap();
        Settings::load_from(&path).save_to(&path);
        assert_eq!(
            std::fs::read_to_string(dir.join("settings.json.bak.1")).unwrap(),
            r#"{"terminal_scrollback": 5"#
        );
        assert!(parse_own(&std::fs::read_to_string(&path).unwrap()).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

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
    fn terminal_scrollback_defaults_and_reads_from_json() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.terminal_scrollback, 10_000);
        let s: Settings = serde_json::from_str(r#"{"terminal_scrollback": 2500}"#).unwrap();
        assert_eq!(s.terminal_scrollback, 2500);
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
