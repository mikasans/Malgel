//! Keyboard focus for alert dialogs: the dialog opens with its primary
//! button focused and ringed, and Tab / Shift+Tab cycle through the buttons
//! without leaving the dialog.
//!
//! gpui-component's buttons keep their focus handles private, so the primary
//! button is wrapped in an element with a handle of ours; the button itself
//! leaves the tab order and the wrapper draws the ring.

use gpui_kit::{
    App, FocusHandle, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _,
    Window,
    base::FocusTrapElement as _,
    component::{ActiveTheme as _, button::Button},
    div, px,
};

/// The focus handles of one alert's buttons (one alert shows at a time, so
/// an owner keeps a single pair and reuses it).
pub struct DialogFocus {
    trap: FocusHandle,
    primary: FocusHandle,
}

impl DialogFocus {
    pub fn new(cx: &mut App) -> Self {
        Self {
            trap: cx.focus_handle(),
            primary: cx.focus_handle().tab_stop(true),
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
        div()
            .track_focus(&self.primary)
            .rounded(cx.theme().radius + px(2.))
            .border_2()
            .border_color(ring)
            .child(button.tab_stop(false))
    }

    /// Keep Tab inside `footer`: past the last button it wraps to the first.
    pub fn trap(&self, footer: impl IntoElement) -> impl IntoElement {
        div()
            .id("alert-buttons")
            .focus_trap("alert-buttons-trap", &self.trap)
            .child(footer)
    }
}
