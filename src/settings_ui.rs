//! Settings dialog with a VS Code-like search filter (probe #11).

use gpui_kit::{
    App, AppContext as _, Context, Entity, FontWeight, IntoElement, ParentElement as _, Render,
    Styled as _, Window,
    assets::IconName,
    component::{
        ActiveTheme as _, Icon, Sizable as _, WindowExt as _,
        button::Button,
        h_flex,
        input::{Input, InputEvent, InputState},
        notification::Notification,
        radio::RadioGroup,
        scroll::ScrollableElement as _,
        switch::Switch,
        v_flex,
    },
    div, px,
};

use crate::{default_app, settings::LinkPlacement, themes::AppSettings};

struct SettingRow {
    id: &'static str,
    title: &'static str,
    description: &'static str,
    keywords: &'static [&'static str],
    get: fn(&crate::settings::Settings) -> bool,
    set: fn(&mut crate::settings::Settings, bool),
    section: &'static str,
}

const ROWS: &[SettingRow] = &[
    SettingRow {
        id: "tabs",
        title: "Tabs",
        description: "Open documents side by side in tabs instead of one at a time.",
        keywords: &["tabs", "tab"],
        get: |s| s.tabs,
        set: |s, on| s.tabs = on,
        section: "WORKSPACE",
    },
    SettingRow {
        id: "file-sidebar",
        title: "File sidebar",
        description: "Browse a folder’s Markdown files beside the editor.",
        keywords: &["file", "sidebar", "explorer"],
        get: |s| s.file_sidebar,
        set: |s, on| s.file_sidebar = on,
        section: "WORKSPACE",
    },
    SettingRow {
        id: "outline",
        title: "Outline",
        description: "List the document’s headings beside the editor.",
        keywords: &["outline", "symbol", "headings"],
        get: |s| s.outline_sidebar,
        set: |s, on| s.outline_sidebar = on,
        section: "WORKSPACE",
    },
    SettingRow {
        id: "problems",
        title: "Problems",
        description: "List diagnostics and spell issues in a panel (VS Code Problems).",
        keywords: &["problems", "diagnostics"],
        get: |s| s.problems_panel,
        set: |s, on| s.problems_panel = on,
        section: "WORKSPACE",
    },
    SettingRow {
        id: "minimap",
        title: "Minimap",
        description: "Show a code overview strip beside the editor.",
        keywords: &["minimap", "overview"],
        get: |s| s.minimap,
        set: |s, on| s.minimap = on,
        section: "WORKSPACE",
    },
    SettingRow {
        id: "restore-session",
        title: "Reopen last session",
        description: "Start with the documents, folder and window you had open.",
        keywords: &["session", "restore", "reopen"],
        get: |s| s.restore_session,
        set: |s, on| s.restore_session = on,
        section: "WORKSPACE",
    },
    SettingRow {
        id: "spell-check",
        title: "Check spelling",
        description: "Underline unknown words; right-click one for suggestions.",
        keywords: &["spell", "spelling"],
        get: |s| s.spell_check,
        set: |s, on| s.spell_check = on,
        section: "WRITING",
    },
    SettingRow {
        id: "scroll-sync",
        title: "Sync scrolling",
        description: "Keep the preview on the part of the document being edited.",
        keywords: &["scroll", "sync"],
        get: |s| s.scroll_sync,
        set: |s, on| s.scroll_sync = on,
        section: "WRITING",
    },
    SettingRow {
        id: "soft-wrap",
        title: "Wrap lines",
        description: "Wrap long lines in the editor instead of scrolling sideways.",
        keywords: &["wrap", "word wrap", "soft wrap", "softwrap"],
        get: |s| s.soft_wrap,
        set: |s, on| s.soft_wrap = on,
        section: "WRITING",
    },
    SettingRow {
        id: "auto-save",
        title: "Auto Save",
        description: "Automatically save dirty editors after a short delay.",
        keywords: &["autosave", "auto save", "auto-save", "save"],
        get: |s| s.auto_save,
        set: |s, on| s.auto_save = on,
        section: "WRITING",
    },
    SettingRow {
        id: "gfm-breaks",
        title: "Preview soft line breaks",
        description: "Show single newlines in a paragraph as line breaks in the preview (GFM breaks).",
        keywords: &["gfm", "breaks", "preview"],
        get: |s| s.gfm_breaks,
        set: |s, on| s.gfm_breaks = on,
        section: "WRITING",
    },
    SettingRow {
        id: "line-numbers",
        title: "Line numbers",
        description: "Number the lines in the editor.",
        keywords: &["line", "numbers", "gutter"],
        get: |s| s.line_numbers,
        set: |s, on| s.line_numbers = on,
        section: "WRITING",
    },
    SettingRow {
        id: "confirm-web-links",
        title: "Ask before opening web links",
        description: "Show where a link leads before it opens in your browser.",
        keywords: &["links", "web", "confirm"],
        get: |s| s.confirm_web_links,
        set: |s, on| s.confirm_web_links = on,
        section: "LINKS",
    },
    SettingRow {
        id: "confirm-drag-and-drop",
        title: "Ask before moving files dragged in the sidebar",
        description: "Confirm where a dragged file lands before it moves; holding Ctrl (Option on macOS) copies instead.",
        keywords: &["drag", "drop", "confirm", "move"],
        get: |s| s.confirm_drag_and_drop,
        set: |s, on| s.confirm_drag_and_drop = on,
        section: "LINKS",
    },
];

fn row_matches(row: &SettingRow, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    let q = q.replace(['-', '_', ' '], "");
    let hay = format!(
        "{} {} {} {}",
        row.id,
        row.title,
        row.description,
        row.keywords.join(" ")
    )
    .to_lowercase()
    .replace(['-', '_', ' '], "");
    hay.contains(&q)
        || row.keywords.iter().any(|k| {
            k.to_lowercase().replace(['-', '_', ' '], "").contains(&q)
                || q.contains(&k.to_lowercase().replace(['-', '_', ' '], ""))
        })
}

struct SettingsView {
    filter: Entity<InputState>,
    query: String,
}

impl SettingsView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Filter settings (e.g. wrap, autosave)…")
        });
        cx.subscribe_in(&filter, window, |this, input, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                this.query = input.read(cx).value().to_string();
                cx.notify();
            }
        })
        .detach();
        Self {
            filter,
            query: String::new(),
        }
    }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = AppSettings::get(cx).clone();
        let default_status = default_app::status(cx);
        let matched: Vec<&SettingRow> = ROWS
            .iter()
            .filter(|row| row_matches(row, &self.query))
            .collect();
        let show_links_radio = {
            let q = self
                .query
                .trim()
                .to_lowercase()
                .replace(['-', '_', ' '], "");
            q.is_empty()
                || "openlinkeddocumentsinnewtabsametablinks".contains(q.as_str())
                || q.contains("link")
        };
        let show_files = {
            let q = self
                .query
                .trim()
                .to_lowercase()
                .replace(['-', '_', ' '], "");
            q.is_empty()
                || "defaultappformarkdownfiles".contains(q.as_str())
                || q.contains("default")
                || q.contains("markdown")
                || q.contains("file")
        };

        let mut body = v_flex()
            .gap_0()
            .w(px(520.))
            .max_h(px(480.))
            .overflow_y_scrollbar();
        let mut last_section = "";
        for row in &matched {
            if row.section != last_section {
                last_section = row.section;
                body = body.child(
                    div()
                        .pt_3()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(cx.theme().muted_foreground)
                        .child(row.section),
                );
            }
            let checked = (row.get)(&settings);
            let set = row.set;
            let id = row.id;
            body = body.child(
                h_flex()
                    .gap_4()
                    .py_2()
                    .justify_between()
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(row.title),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(row.description),
                            ),
                    )
                    .child(
                        Switch::new(id)
                            .checked(checked)
                            .on_click(move |checked, _, cx| {
                                let checked = *checked;
                                AppSettings::update(cx, |settings| set(settings, checked));
                            }),
                    ),
            );
        }

        if show_links_radio {
            if last_section != "LINKS" {
                body = body.child(
                    div()
                        .pt_3()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(cx.theme().muted_foreground)
                        .child("LINKS"),
                );
            }
            body = body.child(
                h_flex()
                    .gap_4()
                    .py_2()
                    .justify_between()
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child("Open linked documents in"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(if cfg!(target_os = "macos") {
                                        "Command-click a link to do the opposite."
                                    } else {
                                        "Ctrl-click a link to do the opposite."
                                    }),
                            ),
                    )
                    .child(
                        RadioGroup::horizontal("open-links-in")
                            .flex_none()
                            .child("A new tab")
                            .child("The same tab")
                            .selected_index(Some(match settings.open_links_in {
                                LinkPlacement::NewTab => 0,
                                LinkPlacement::SameTab => 1,
                            }))
                            .on_click(|ix, _, cx| {
                                let placement = if *ix == 0 {
                                    LinkPlacement::NewTab
                                } else {
                                    LinkPlacement::SameTab
                                };
                                AppSettings::update(cx, |settings| {
                                    settings.open_links_in = placement
                                });
                            }),
                    ),
            );
        }

        if show_files {
            body = body
                .child(
                    div()
                        .pt_3()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(cx.theme().muted_foreground)
                        .child("FILES"),
                )
                .child(default_app_row(default_status, cx));
        }

        if matched.is_empty() && !show_links_radio && !show_files {
            body = body.child(
                div()
                    .p_4()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("No matching settings"),
            );
        }

        v_flex()
            .w(px(560.))
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Icon::new(IconName::Search).text_color(cx.theme().muted_foreground))
                    .child(div().flex_1().child(Input::new(&self.filter))),
            )
            .child(body)
    }
}

fn default_app_row(status: default_app::Status, cx: &App) -> impl IntoElement {
    let is_default = status == default_app::Status::Default;
    let description = if is_default {
        "Markdown files you open from your desktop open in Malgel."
    } else if default_app::CHOSEN_IN_SYSTEM_SETTINGS {
        "Choose Malgel for Markdown files in Windows Settings."
    } else {
        "Open Markdown files from your desktop in Malgel."
    };
    let action = if is_default {
        h_flex()
            .gap_1()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(Icon::new(IconName::Check).small())
            .child("Default")
            .into_any_element()
    } else {
        let label = if default_app::CHOSEN_IN_SYSTEM_SETTINGS {
            "Open Settings…"
        } else {
            "Make Default"
        };
        Button::new("make-default")
            .label(label)
            .small()
            .outline()
            .on_click(|_, window, cx| {
                let note = match default_app::make_default(cx) {
                    Ok(default_app::Outcome::Done) => {
                        Notification::success("Malgel now opens Markdown files.")
                    }
                    Ok(default_app::Outcome::SettingsOpened) => {
                        Notification::info("In Settings, set Malgel as the default for .md files.")
                    }
                    Err(message) => Notification::error(message),
                };
                window.push_notification(note, cx);
                window.refresh();
            })
            .into_any_element()
    };
    h_flex()
        .gap_4()
        .py_2()
        .justify_between()
        .child(
            v_flex()
                .gap_0p5()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child("Default app for Markdown"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(description),
                ),
        )
        .child(action)
}

/// Open the filterable Settings dialog.
pub fn open(window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    default_app::refresh(cx);
    window.open_dialog(cx, |dialog, window, cx| {
        let view = cx.new(|cx| SettingsView::new(window, cx));
        dialog.title("Settings").child(view)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_matches_wrap_and_autosave() {
        let wrap = ROWS.iter().find(|r| r.id == "soft-wrap").unwrap();
        let auto = ROWS.iter().find(|r| r.id == "auto-save").unwrap();
        assert!(row_matches(wrap, "wrap"));
        assert!(row_matches(auto, "autosave"));
        assert!(row_matches(auto, "auto save"));
        assert!(!row_matches(wrap, "autosavezzz"));
    }
}
