//! Keyboard focus for alert dialogs: the dialog opens with its primary
//! button focused and ringed, and Tab / Shift+Tab cycle through the buttons
//! without leaving the dialog.
//!
//! gpui-component's buttons keep their focus handles private, so each button
//! is wrapped in an element with a handle of ours; the button itself leaves
//! the tab order and the wrapper draws the ring.
//!
//! The dialog binds Enter to its OK handler (the primary button). The other
//! buttons' wrappers answer that Enter themselves, so Enter presses the button
//! that has the keyboard — Tab to Don't Save, Enter must not save.

use std::rc::Rc;

use gpui_kit::{
    App, ClickEvent, FocusHandle, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window,
    base::{FocusTrapElement as _, actions::Confirm},
    component::{ActiveTheme as _, button::Button},
    div, px,
};

/// How many non-primary buttons an alert can have.
const SECONDARY_BUTTONS: usize = 3;

/// The focus handles of one alert's buttons (one alert shows at a time, so
/// an owner keeps a single pair and reuses it).
pub struct DialogFocus {
    trap: FocusHandle,
    primary: FocusHandle,
    secondary: [FocusHandle; SECONDARY_BUTTONS],
}

impl DialogFocus {
    pub fn new(cx: &mut App) -> Self {
        Self {
            trap: cx.focus_handle(),
            primary: cx.focus_handle().tab_stop(true),
            secondary: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
        }
    }

    /// Focus the primary button once the dialog has taken focus.
    pub fn focus_primary_soon(&self, window: &mut Window, cx: &mut App) {
        let primary = self.primary.clone();
        window.defer(cx, move |window, cx| primary.focus(window, cx));
    }

    /// The primary button, in the tab order through our handle and ringed
    /// while it has focus. Enter still reaches the dialog's own OK handler.
    pub fn primary(&self, button: Button, window: &Window, cx: &App) -> impl IntoElement {
        let ring = if self.primary.is_focused(window) {
            cx.theme().ring
        } else {
            gpui_kit::transparent_black()
        };
        let primary = self.primary.clone();
        let ours: Vec<FocusHandle> = std::iter::once(self.trap.clone())
            .chain(self.secondary.iter().cloned())
            .collect();
        div()
            .track_focus(&self.primary)
            .rounded(cx.theme().radius + px(2.))
            .border_2()
            .border_color(ring)
            .child(button.tab_stop(false))
            // While the alert shows, the keyboard belongs to it: focus taken
            // before it was laid out (lost), or pulled away by a late
            // editor focus (a tab finishing loading at startup), comes back
            // to the primary button.
            .child(
                gpui_kit::canvas(
                    move |_, window, cx| {
                        let inside = primary.is_focused(window)
                            || ours.iter().any(|handle| handle.is_focused(window));
                        if !inside {
                            primary.focus(window, cx);
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_0(),
            )
    }

    /// Non-primary button number `ix` (0-based, at most three per dialog):
    /// in the tab order through our handle, ringed while focused, and pressed
    /// by Enter or Space while it has the keyboard, as well as by a click.
    /// `press` replaces the button's own `on_click`.
    pub fn secondary(
        &self,
        ix: usize,
        button: Button,
        press: impl Fn(&mut Window, &mut App) + 'static,
        window: &Window,
        cx: &App,
    ) -> impl IntoElement {
        let handle = &self.secondary[ix.min(SECONDARY_BUTTONS - 1)];
        let ring = if handle.is_focused(window) {
            cx.theme().ring
        } else {
            gpui_kit::transparent_black()
        };
        let press = Rc::new(press);
        div()
            .id(("alert-secondary", ix))
            .track_focus(handle)
            .rounded(cx.theme().radius + px(2.))
            .border_2()
            .border_color(ring)
            // Enter: the dialog's Confirm binding fires before any key
            // listener, so answer it here, below the dialog's OK handler.
            .on_action({
                let press = press.clone();
                move |_: &Confirm, window, cx| press(window, cx)
            })
            // Space: the focused wrapper's native keyboard click. A mouse
            // click lands on the button below, so skip it here.
            .on_click({
                let press = press.clone();
                move |event: &ClickEvent, window, cx| {
                    if event.is_keyboard() {
                        press(window, cx)
                    }
                }
            })
            .child(
                button
                    .tab_stop(false)
                    .on_click(move |_, window, cx| press(window, cx)),
            )
    }

    /// Keep Tab inside `footer`: past the last button it wraps to the first.
    pub fn trap(&self, footer: impl IntoElement) -> impl IntoElement {
        div()
            .id("alert-buttons")
            .focus_trap("alert-buttons-trap", &self.trap)
            .child(footer)
    }
}
