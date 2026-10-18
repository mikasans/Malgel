//! Searchable Keyboard Shortcuts reference (Help → Keyboard Shortcuts).

use gpui_kit::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    assets::IconName,
    component::{
        ActiveTheme as _, Icon, WindowExt as _,
        button::Button,
        h_flex,
        input::{Input, InputEvent, InputState},
        v_flex,
    },
    div, px,
};

/// One row: command name, when it applies, and the platform chord wording.
pub struct ShortcutRow {
    pub command: &'static str,
    pub when: &'static str,
    pub chord: &'static str,
}

/// Keep roughly in sync with `actions::bind_keys` / menus.
pub const SHORTCUTS: &[ShortcutRow] = &[
    ShortcutRow {
        command: "Toggle Terminal",
        when: "Workspace",
        chord: "Ctrl+`  (macOS also ⌘J)",
    },
    ShortcutRow {
        command: "New Terminal",
        when: "Workspace",
        chord: "Ctrl+Shift+`",
    },
    ShortcutRow {
        command: "Toggle Side Bar",
        when: "Workspace",
        chord: "Ctrl+B  (⌘B)",
    },
    ShortcutRow {
        command: "Bold",
        when: "Editor",
        chord: "Ctrl+Shift+B  (⌘⇧B)",
    },
    ShortcutRow {
        command: "Toggle Word Wrap",
        when: "Workspace",
        chord: "Alt+Z",
    },
    ShortcutRow {
        command: "Find in Files",
        when: "Workspace · search",
        chord: "Ctrl+Shift+F  (⌘⇧F)",
    },
    ShortcutRow {
        command: "Replace in Files",
        when: "Workspace · search",
        chord: "Ctrl+Shift+H  (⌘⇧H)",
    },
    ShortcutRow {
        command: "Find",
        when: "Editor",
        chord: "Ctrl+F  (⌘F)",
    },
    ShortcutRow {
        command: "Command Palette",
        when: "Workspace",
        chord: "Ctrl+Shift+P  (⌘⇧P)",
    },
    ShortcutRow {
        command: "Quick Open",
        when: "Workspace",
        chord: "Ctrl+P  (⌘P)",
    },
    ShortcutRow {
        command: "Toggle Preview Layout",
        when: "Workspace",
        chord: "Ctrl+Shift+V  (⌘⇧V)",
    },
    ShortcutRow {
        command: "Split Editor Right",
        when: "Workspace",
        chord: "Ctrl+\\  (⌘\\)",
    },
    ShortcutRow {
        command: "Fold",
        when: "Editor",
        chord: "Ctrl+Shift+[",
    },
    ShortcutRow {
        command: "Unfold",
        when: "Editor",
        chord: "Ctrl+Shift+]",
    },
    ShortcutRow {
        command: "Toggle Line Comment",
        when: "Workspace",
        chord: "Ctrl+/  (⌘/)",
    },
    ShortcutRow {
        command: "Open Folder",
        when: "Workspace",
        chord: "Ctrl+K Ctrl+O",
    },
    ShortcutRow {
        command: "Toggle Outline",
        when: "Workspace · outline pane",
        chord: "Ctrl+Alt+O  (⌘⌥O)",
    },
    ShortcutRow {
        command: "Go to Symbol",
        when: "Workspace",
        chord: "Ctrl+Shift+O  (⌘⇧O)",
    },
    ShortcutRow {
        command: "Open File",
        when: "Workspace",
        chord: "Ctrl+O  (⌘O)",
    },
    ShortcutRow {
        command: "New File",
        when: "Workspace",
        chord: "Ctrl+N  (⌘N)",
    },
    ShortcutRow {
        command: "Save",
        when: "Workspace",
        chord: "Ctrl+S  (⌘S)",
    },
    ShortcutRow {
        command: "Save All",
        when: "Workspace",
        chord: "Ctrl+K S",
    },
    ShortcutRow {
        command: "Replace",
        when: "Editor",
        chord: "Ctrl+H  (⌘⌥F)",
    },
    ShortcutRow {
        command: "Rename (Explorer)",
        when: "FileTree",
        chord: "F2",
    },
    ShortcutRow {
        command: "Toggle Maximized Panel",
        when: "Workspace · palette maximize",
        chord: "Command Palette",
    },
    ShortcutRow {
        command: "Settings",
        when: "Workspace",
        chord: "Ctrl+,  (⌘,)",
    },
    ShortcutRow {
        command: "Keyboard Shortcuts",
        when: "Workspace",
        chord: "Ctrl+K Ctrl+S",
    },
    ShortcutRow {
        command: "Zoom In",
        when: "Workspace",
        chord: "Ctrl+=",
    },
    ShortcutRow {
        command: "Zoom Out",
        when: "Workspace",
        chord: "Ctrl+-",
    },
    ShortcutRow {
        command: "Actual Size",
        when: "Workspace",
        chord: "Ctrl+0",
    },
    ShortcutRow {
        command: "Go to Line",
        when: "Workspace",
        chord: "Ctrl+G  (⌘G)",
    },
    ShortcutRow {
        command: "Go to Heading",
        when: "Workspace",
        chord: "Ctrl+Shift+O",
    },
];

fn row_matches(row: &ShortcutRow, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    row.command.to_lowercase().contains(&q)
        || row.chord.to_lowercase().contains(&q)
        || row.when.to_lowercase().contains(&q)
}

struct ShortcutsView {
    filter: Entity<InputState>,
    query: String,
}

impl ShortcutsView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Filter shortcuts (e.g. terminal, search)…")
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

impl Render for ShortcutsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = SHORTCUTS
            .iter()
            .filter(|row| row_matches(row, &self.query))
            .collect();
        v_flex()
            .w(px(560.))
            .h(px(440.))
            .gap_2()
            .p_3()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Icon::new(IconName::Keyboard).text_color(cx.theme().muted_foreground))
                    .child(div().flex_1().child(Input::new(&self.filter))),
            )
            .child(
                div()
                    .id("shortcuts-list")
                    .flex_1()
                    .min_h_0()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .p_1()
                    .overflow_y_scroll()
                    .children(rows.into_iter().map(|row| {
                        h_flex()
                            .id(row.command)
                            .w_full()
                            .gap_3()
                            .px_2()
                            .py_1p5()
                            .rounded(cx.theme().radius)
                            .hover(|s| s.bg(cx.theme().list_hover))
                            .justify_between()
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .gap_0p5()
                                    .child(div().text_sm().child(SharedString::from(row.command)))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(SharedString::from(row.when)),
                                    ),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_color(cx.theme().muted_foreground)
                                    .child(SharedString::from(row.chord)),
                            )
                    })),
            )
            .child(
                h_flex().justify_end().child(
                    Button::new("close-shortcuts")
                        .label("Close")
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                ),
            )
    }
}

/// Open the searchable shortcuts dialog.
pub fn open(window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    window.open_dialog(cx, |dialog, window, cx| {
        let view = cx.new(|cx| ShortcutsView::new(window, cx));
        dialog.title("Keyboard Shortcuts").child(view)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_alias_hits_find_in_files_chord_row() {
        assert!(SHORTCUTS.iter().any(|r| r.command == "Find in Files"));
        assert!(row_matches(
            SHORTCUTS
                .iter()
                .find(|r| r.command == "Find in Files")
                .unwrap(),
            "search"
        ));
        assert!(row_matches(
            SHORTCUTS
                .iter()
                .find(|r| r.command == "Toggle Terminal")
                .unwrap(),
            "terminal"
        ));
    }
}
