//! Themes bundled with Malgel and the application-wide preferences global.

use gpui_kit::{
    App, Global, SharedString, Window,
    component::{Theme, ThemeMode, ThemeRegistry},
    px,
};

use crate::settings::{Appearance, Settings, clamp_font_size};

/// Theme sets shipped in the binary, from the GPUI Kit theme collection.
const BUNDLED_THEMES: &[&str] = &[
    include_str!("../themes/ayu.json"),
    include_str!("../themes/catppuccin.json"),
    include_str!("../themes/everforest.json"),
    include_str!("../themes/flexoki.json"),
    include_str!("../themes/gruvbox.json"),
    include_str!("../themes/macos-classic.json"),
    include_str!("../themes/solarized.json"),
    include_str!("../themes/tokyonight.json"),
];

/// The user's preferences, shared by every window.
pub struct AppSettings(pub Settings);

impl Global for AppSettings {}

impl AppSettings {
    pub fn get(cx: &App) -> &Settings {
        &cx.global::<AppSettings>().0
    }

    /// Change the settings, persist them, and re-apply the appearance.
    pub fn update(cx: &mut App, f: impl FnOnce(&mut Settings)) {
        let mut settings = Self::get(cx).clone();
        f(&mut settings);
        settings.font_size = clamp_font_size(settings.font_size);
        if &settings == Self::get(cx) {
            return;
        }
        let appearance_changed = {
            let current = Self::get(cx);
            current.appearance != settings.appearance
                || current.light_theme != settings.light_theme
                || current.dark_theme != settings.dark_theme
                || current.font_size != settings.font_size
        };
        settings.save();
        cx.set_global(AppSettings(settings));
        if appearance_changed {
            apply(None, cx);
        }
    }
}

pub fn init(settings: Settings, cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    for theme_set in BUNDLED_THEMES {
        if let Err(err) = registry.load_themes_from_str(theme_set) {
            eprintln!("malgel: could not load a bundled theme: {err}");
        }
    }
    cx.set_global(AppSettings(settings));
    apply(None, cx);
}

/// Apply the preferred themes, appearance and interface size.
///
/// `window` supplies the system appearance when the preference is to follow
/// it; without one the application-wide appearance is used.
pub fn apply(window: Option<&mut Window>, cx: &mut App) {
    let settings = AppSettings::get(cx).clone();
    let registry = ThemeRegistry::global(cx);
    let pick = |name: &Option<String>, mode: ThemeMode| {
        name.as_deref()
            .and_then(|name| registry.themes().get(name))
            .filter(|theme| theme.mode == mode)
            .cloned()
            .unwrap_or_else(|| registry.default_themes()[&mode].clone())
    };
    let light = pick(&settings.light_theme, ThemeMode::Light);
    let dark = pick(&settings.dark_theme, ThemeMode::Dark);

    let mode = match settings.appearance {
        Appearance::Light => ThemeMode::Light,
        Appearance::Dark => ThemeMode::Dark,
        Appearance::System => window
            .as_ref()
            .map(|window| window.appearance())
            .unwrap_or_else(|| cx.window_appearance())
            .into(),
    };

    let default_dark = dark.name == registry.default_themes()[&ThemeMode::Dark].name;
    Theme::update(cx, |theme| {
        theme.light_theme = light;
        theme.dark_theme = dark;
    });
    Theme::change(mode, None, cx);
    if mode.is_dark() && default_dark {
        Theme::update(cx, tune_default_dark);
    }
    Theme::update(cx, |theme| {
        theme.font_size = px(settings.font_size);
        // Toasts bottom-right above the status bar, as VS Code shows them:
        // top-right they covered the tab bar's + and the preview header.
        theme.notification.placement = gpui_kit::Anchor::BottomRight;
        theme.notification.margins.bottom = px(STATUS_BAR_CLEARANCE);
    });
}

/// Room below a bottom-right toast: the status bar plus a gap.
const STATUS_BAR_CLEARANCE: f32 = 40.;

/// The bundled dark theme's selection (~1.4:1 on its background) and current
/// line (~1.1:1) are too faint to see a long selection's edge or the caret's
/// line: use VS Code Dark+'s selection blue and a clearer line band.
fn tune_default_dark(theme: &mut Theme) {
    theme.selection = gpui_kit::rgb(DARK_SELECTION).into();
    let mut highlight = (*theme.highlight_theme).clone();
    highlight.style.editor_active_line = Some(gpui_kit::rgb(DARK_ACTIVE_LINE).into());
    theme.highlight_theme = std::sync::Arc::new(highlight);
}

/// VS Code Dark+'s `editor.selectionBackground`.
const DARK_SELECTION: u32 = 0x264f78;
/// On the near-black editor: about 1.25:1 against the background.
const DARK_ACTIVE_LINE: u32 = 0x232323;

/// Names of the registered themes for `mode`, sorted for display.
pub fn theme_names(mode: ThemeMode, cx: &App) -> Vec<SharedString> {
    ThemeRegistry::global(cx)
        .sorted_themes()
        .into_iter()
        .filter(|theme| theme.mode == mode)
        .map(|theme| theme.name.clone())
        .collect()
}

/// Remember `name` as the theme for its mode and switch to that mode.
pub fn select_theme(name: &SharedString, cx: &mut App) {
    let Some(mode) = ThemeRegistry::global(cx)
        .themes()
        .get(name)
        .map(|theme| theme.mode)
    else {
        return;
    };
    // Following the system stays in effect when the theme suits the current
    // appearance; otherwise switching to the theme means switching modes.
    let follows_system =
        AppSettings::get(cx).appearance == Appearance::System && Theme::global(cx).mode == mode;
    AppSettings::update(cx, |settings| {
        if mode.is_dark() {
            settings.dark_theme = Some(name.to_string());
        } else {
            settings.light_theme = Some(name.to_string());
        }
        if !follows_system {
            settings.appearance = if mode.is_dark() {
                Appearance::Dark
            } else {
                Appearance::Light
            };
        }
    });
}

#[cfg(test)]
mod dark_tuning_tests {
    fn luminance(rgb: u32) -> f64 {
        let ch = |c: u32| {
            let c = c as f64 / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(rgb >> 16 & 0xff) + 0.7152 * ch(rgb >> 8 & 0xff) + 0.0722 * ch(rgb & 0xff)
    }
    fn contrast(a: u32, b: u32) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    fn hex(color: &str) -> u32 {
        u32::from_str_radix(&color.trim_start_matches('#')[..6], 16).unwrap()
    }

    /// G5: the gutter's line numbers are painted with the theme's
    /// `muted.foreground` (gpui-base ignores `editor.line_number`), so that
    /// is the color that must read at WCAG AA against the editor background,
    /// in every bundled theme.
    #[test]
    fn rendered_line_numbers_meet_aa_contrast_in_every_theme() {
        let mut checked = 0;
        let mut failures = Vec::new();
        for source in super::BUNDLED_THEMES {
            let family: serde_json::Value = serde_json::from_str(source).unwrap();
            for theme in family["themes"].as_array().unwrap() {
                let colors = &theme["colors"];
                let numbers = hex(colors["muted.foreground"].as_str().unwrap());
                let background = theme["highlight"]["editor.background"]
                    .as_str()
                    .or(colors["background"].as_str())
                    .map(hex)
                    .unwrap();
                let ratio = contrast(numbers, background);
                if ratio < 4.5 {
                    failures.push(format!("{} {ratio:.2}:1", theme["name"]));
                }
                assert!(
                    theme["highlight"].get("editor.line_number").is_none(),
                    "{}: editor.line_number is never rendered; drop it",
                    theme["name"]
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 19, "every bundled theme is checked");
        assert!(
            failures.is_empty(),
            "line numbers under 4.5:1: {failures:?}"
        );
    }

    /// The default dark editor background is neutral-950 (#0A0A0A).
    #[test]
    fn default_dark_selection_and_current_line_stand_out() {
        assert_eq!(super::DARK_SELECTION, 0x264f78, "VS Code Dark+ selection");
        assert!(contrast(super::DARK_SELECTION, 0x0a0a0a) > 2.0);
        let line = contrast(super::DARK_ACTIVE_LINE, 0x0a0a0a);
        assert!(line > 1.2, "current line {line:.2}:1 is too faint");
        // Selection must still read over the current-line band.
        assert!(contrast(super::DARK_SELECTION, super::DARK_ACTIVE_LINE) > 1.5);
    }
}
