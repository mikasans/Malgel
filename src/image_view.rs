//! Image viewing support: opens PNG/JPEG/WebP/GIF/BMP/TIFF/ICO/SVG files
//! with the same zoom controls the PDF viewer has — fit width, fit page,
//! actual size, a zoom percentage menu, the mouse wheel and trackpad pinch.
//! The gestures are the ones `zoom` documents and the diagram viewer shares:
//! the wheel zooms, Command/Control with the wheel zooms on a trackpad, and a
//! double click swaps between the fit and a closer look.

use std::{path::Path, path::PathBuf, sync::Arc};

use gpui_kit::{
    Anchor, App, Context, FocusHandle, Focusable, Image, ImageFormat, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _,
    PinchEvent, Pixels, Point, Render, ScrollDelta, ScrollHandle, ScrollWheelEvent, Size,
    StatefulInteractiveElement as _, Styled as _, Window,
    assets::IconName,
    base::Disableable as _,
    component::{
        ActiveTheme as _, ElementExt as _, Icon, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        menu::{DropdownMenu as _, PopupMenuItem},
        scroll::ScrollableElement as _,
        v_flex,
    },
    div, img, px,
};

use crate::{pdf_view::FitMode, zoom};

/// Space left around the image when a fit mode sizes it.
const MARGIN: f32 = 24.;

/// Map an image extension to the gpui format it decodes as.
pub fn image_format(path: &Path) -> Option<ImageFormat> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "webp" => ImageFormat::Webp,
        "gif" => ImageFormat::Gif,
        "svg" => ImageFormat::Svg,
        "bmp" => ImageFormat::Bmp,
        "tif" | "tiff" => ImageFormat::Tiff,
        "ico" => ImageFormat::Ico,
        _ => return None,
    })
}

pub struct ImageView {
    title: String,
    image: Arc<Image>,
    width: f32,
    height: f32,
    fit: FitMode,
    /// The factor actually used, resolved from `fit` each frame.
    zoom: f32,
    /// The user's factor, used when `fit` is [`FitMode::Custom`].
    custom_zoom: f32,
    /// The scroll area's size, measured while it paints.
    viewport: Size<Pixels>,
    /// The image's scroll position, so a drag can move it.
    scroll: ScrollHandle,
    /// Where a drag started, and the scroll offset it started from.
    drag: Option<(Point<Pixels>, Point<Pixels>)>,
    focus_handle: FocusHandle,
}

/// Decode `bytes` as the image `path` names; returns the image with its
/// natural size in logical pixels.
pub fn load_image(path: &Path, bytes: Vec<u8>) -> Result<(Arc<Image>, f32, f32), String> {
    let format = image_format(path).ok_or_else(|| {
        format!(
            "unknown image format ({})",
            path.extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("no extension")
        )
    })?;
    let (width, height) = probe_dimensions(&bytes, format).unwrap_or((1024., 768.));
    let image = Arc::new(Image::from_bytes(format, bytes));
    Ok((image, width, height))
}

/// Read an image's pixel dimensions without keeping the decoded bitmap.
///
/// A format whose decoder is not compiled in (or a damaged header) has no
/// size to report; the container then falls back to a 4:3 frame.
fn probe_dimensions(bytes: &[u8], format: ImageFormat) -> Option<(f32, f32)> {
    let fmt = match format {
        ImageFormat::Png => image::ImageFormat::Png,
        ImageFormat::Jpeg => image::ImageFormat::Jpeg,
        ImageFormat::Webp => image::ImageFormat::WebP,
        ImageFormat::Gif => image::ImageFormat::Gif,
        ImageFormat::Bmp => image::ImageFormat::Bmp,
        ImageFormat::Tiff => image::ImageFormat::Tiff,
        ImageFormat::Ico => image::ImageFormat::Ico,
        // SVG has no fixed pixel size; the container fits it.
        _ => return None,
    };
    let reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), fmt);
    let (width, height) = reader.into_dimensions().ok()?;
    Some((width as f32, height as f32))
}

/// The zoom a fit mode resolves to for a viewport of the given size.
///
/// Fitting never enlarges: an image smaller than the viewport keeps its own
/// size, the way Preview's "Zoom to Fit" does.
fn fit_zoom(mode: FitMode, image_width: f32, image_height: f32, viewport: Size<Pixels>) -> f32 {
    let avail_width = (viewport.width.as_f32() - 2. * MARGIN).max(1.);
    let avail_height = (viewport.height.as_f32() - 2. * MARGIN).max(1.);
    let fit = match mode {
        FitMode::Page => (avail_width / image_width).min(avail_height / image_height),
        _ => avail_width / image_width,
    };
    fit.min(1.).clamp(zoom::MIN, zoom::MAX)
}

impl ImageView {
    pub fn new(
        path: Option<PathBuf>,
        image: Arc<Image>,
        width: f32,
        height: f32,
        cx: &mut Context<Self>,
    ) -> Self {
        let title = path
            .as_deref()
            .and_then(|p| p.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "Image".into());
        Self {
            title,
            image,
            width,
            height,
            // A screenshot is usually taller than the window, so open at a
            // fit rather than showing its top-left corner.
            fit: FitMode::Page,
            zoom: zoom::DEFAULT,
            custom_zoom: zoom::DEFAULT,
            viewport: gpui_kit::size(px(0.), px(0.)),
            scroll: ScrollHandle::new(),
            drag: None,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn zoom_in(&mut self, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom * zoom::BUTTON_STEP, cx);
    }

    pub fn zoom_out(&mut self, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom / zoom::BUTTON_STEP, cx);
    }

    pub fn reset_zoom(&mut self, cx: &mut Context<Self>) {
        self.set_zoom(zoom::DEFAULT, cx);
    }

    /// Set an explicit zoom factor, leaving whichever fit mode was in effect.
    pub fn set_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        let clamped = zoom.clamp(zoom::MIN, zoom::MAX);
        if self.fit != FitMode::Custom || (self.custom_zoom - clamped).abs() > f32::EPSILON {
            self.fit = FitMode::Custom;
            self.custom_zoom = clamped;
            self.zoom = clamped;
            cx.notify();
        }
    }

    /// Scale the image to the width of the view.
    pub fn fit_width(&mut self, cx: &mut Context<Self>) {
        if self.fit != FitMode::Width {
            self.fit = FitMode::Width;
            cx.notify();
        }
    }

    /// Scale the whole image so it fits the view.
    pub fn fit_page(&mut self, cx: &mut Context<Self>) {
        if self.fit != FitMode::Page {
            self.fit = FitMode::Page;
            cx.notify();
        }
    }

    /// The zoom factor the current mode resolves to for `viewport`.
    fn resolved_zoom(&self, viewport: Size<Pixels>) -> f32 {
        if viewport.width.as_f32() <= 1. || viewport.height.as_f32() <= 1. {
            // Not measured yet (first frame): keep the last factor.
            return self.zoom.clamp(zoom::MIN, zoom::MAX);
        }
        match self.fit {
            FitMode::Custom => self.custom_zoom.clamp(zoom::MIN, zoom::MAX),
            FitMode::Width | FitMode::Page => fit_zoom(self.fit, self.width, self.height, viewport),
        }
    }

    /// Command or Control plus the wheel zooms, as in the PDF viewer and in
    /// VS Code. On its own the wheel is left to the scroll area behind this
    /// element, so a zoomed-in image scrolls rather than resizing — the same
    /// gesture as everywhere else in the app.
    fn on_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.modifiers.secondary() {
            return;
        }
        let factor = match event.delta {
            ScrollDelta::Lines(lines) => zoom::WHEEL_STEP.powf(lines.y.clamp(-1., 1.)),
            ScrollDelta::Pixels(delta) => (f32::from(delta.y) / 200.).exp(),
        };
        self.set_zoom(self.zoom * factor, cx);
        // The scroll area behind this element would otherwise scroll the
        // image at the same time as the zoom resizes it.
        cx.stop_propagation();
    }

    /// Double-clicking swaps between the fitted view and a closer look, the
    /// way a double-click does in the diagram viewer: from a fit it doubles
    /// what the fit resolved to, and from a zoom the user set it goes back to
    /// fitting the page.
    fn on_double_click(&mut self, cx: &mut Context<Self>) {
        if self.fit == FitMode::Custom {
            self.fit_page(cx);
        } else {
            self.set_zoom(self.zoom * 2., cx);
        }
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        // Only the left button gets here; the wiring asks for it.
        if event.click_count == 2 {
            self.drag = None;
            self.on_double_click(cx);
            return;
        }
        self.drag = Some((event.position, self.scroll.offset()));
    }

    /// Dragging moves the image, the way it moves a diagram in its own
    /// window: once the picture is bigger than the pane, the wheel alone
    /// leaves the rest of it out of reach.
    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, _cx: &mut Context<Self>) {
        let Some((start, offset)) = self.drag else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            return;
        }
        self.scroll.set_offset(offset + (event.position - start));
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _cx: &mut Context<Self>) {
        self.drag = None;
    }

    fn on_pinch(&mut self, event: &PinchEvent, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom * (1. + event.delta), cx);
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let zoom_pct = zoom::percent(self.zoom);
        let dims = format!("{} × {} px", self.width as u32, self.height as u32);

        let menu_entity = cx.entity();
        h_flex()
            .id("image-toolbar")
            .h_9()
            .px_3()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .items_center()
            .justify_between()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Icon::new(IconName::Image)
                            .small()
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .child(self.title.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(dims),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        Button::new("image-fit-width")
                            .ghost()
                            .small()
                            .compact()
                            .label("Fit width")
                            .tooltip("Fit the image width")
                            .toggled(self.fit == FitMode::Width)
                            .on_click(cx.listener(|this, _, _, cx| this.fit_width(cx))),
                    )
                    .child(
                        Button::new("image-fit-page")
                            .ghost()
                            .small()
                            .compact()
                            .label("Fit page")
                            .tooltip("Fit the whole image")
                            .toggled(self.fit == FitMode::Page)
                            .on_click(cx.listener(|this, _, _, cx| this.fit_page(cx))),
                    )
                    .child(
                        Button::new("image-zoom-out")
                            .ghost()
                            .small()
                            .compact()
                            .icon(Icon::new(IconName::ZoomOut))
                            .tooltip("Zoom out")
                            .disabled(self.zoom <= zoom::MIN)
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_out(cx))),
                    )
                    .child(
                        Button::new("image-zoom-menu")
                            .ghost()
                            .small()
                            .compact()
                            .label(zoom_pct)
                            .tooltip("Zoom")
                            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                                let mut menu = menu;
                                for &(factor, label) in zoom::PRESETS {
                                    let entity = menu_entity.clone();
                                    menu = menu.item(PopupMenuItem::new(label).on_click(
                                        move |_, _, cx| {
                                            entity.update(cx, |this, cx| this.set_zoom(factor, cx));
                                        },
                                    ));
                                }
                                let fit_width = menu_entity.clone();
                                let fit_page = menu_entity.clone();
                                menu.item(PopupMenuItem::new("Fit width").on_click(
                                    move |_, _, cx| {
                                        fit_width.update(cx, |this, cx| this.fit_width(cx));
                                    },
                                ))
                                .item(
                                    PopupMenuItem::new("Fit page").on_click(move |_, _, cx| {
                                        fit_page.update(cx, |this, cx| this.fit_page(cx));
                                    }),
                                )
                            }),
                    )
                    .child(
                        Button::new("image-zoom-in")
                            .ghost()
                            .small()
                            .compact()
                            .icon(Icon::new(IconName::ZoomIn))
                            .tooltip("Zoom in")
                            .disabled(self.zoom >= zoom::MAX)
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_in(cx))),
                    ),
            )
    }

    fn render_content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let image_width = self.width * self.zoom;
        let image_height = self.height * self.zoom;
        // Match the PDF viewer: the scrolled content is at least a viewport
        // big, so a zoomed-in image stays reachable on every side, and the
        // zoom handlers cover the whole view even around a small image.
        let content_width = px(image_width.max(self.viewport.width.as_f32()));
        let content_height = px(image_height.max(self.viewport.height.as_f32()));
        let entity = cx.entity();

        v_flex()
            .id("image-pages-scroll")
            .flex_1()
            .min_h_0()
            .w_full()
            .bg(theme.background)
            .child(
                v_flex()
                    .id("image-pages")
                    .w(content_width)
                    .h(content_height)
                    .flex_shrink_0()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .id("image-card")
                            .flex_shrink_0()
                            .w(px(image_width))
                            .h(px(image_height))
                            .overflow_hidden()
                            .child(
                                img(self.image.clone())
                                    .id("image-canvas")
                                    .w(px(image_width))
                                    .h(px(image_height)),
                            ),
                    )
                    // Inside the scroll area rather than on it: handlers run
                    // innermost first, so zooming can keep the scroll area
                    // from acting on the same wheel.
                    .on_scroll_wheel(cx.listener(Self::on_scroll))
                    .on_pinch(
                        cx.listener(|this, event: &PinchEvent, _, cx| this.on_pinch(event, cx)),
                    ),
            )
            .track_scroll(&self.scroll)
            .on_prepaint(move |bounds, _, cx| {
                // Notifying mid-frame can be lost; update once it's done.
                cx.defer(move |cx| {
                    entity.update(cx, |this, cx| {
                        if this.viewport != bounds.size {
                            this.viewport = bounds.size;
                            cx.notify();
                        }
                    });
                });
            })
            .overflow_scrollbar()
    }
}

impl Focusable for ImageView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ImageView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Resolve the fit mode against the live viewport before drawing the
        // toolbar, so the percentage shown matches the image below it.
        self.zoom = self.resolved_zoom(self.viewport);

        v_flex()
            .id("image-view")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|this, _: &crate::actions::ZoomIn, _, cx| this.zoom_in(cx)))
            .on_action(cx.listener(|this, _: &crate::actions::ZoomOut, _, cx| this.zoom_out(cx)))
            .on_action(
                cx.listener(|this, _: &crate::actions::ZoomReset, _, cx| this.reset_zoom(cx)),
            )
            .child(self.render_toolbar(cx))
            // Double-click fits the page or zooms back out, the way the
            // diagram viewer does. On the content, not the whole view, so a
            // double-click on a toolbar button stays a double click on a
            // button.
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .child(self.render_content(cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport(width: f32, height: f32) -> Size<Pixels> {
        gpui_kit::size(px(width), px(height))
    }

    #[test]
    fn fit_page_scales_a_large_image_down() {
        // 2000x1000 in a 1000x800 view: the width is the binding constraint,
        // (1000 - 2 * 24) / 2000.
        let zoom = fit_zoom(FitMode::Page, 2000., 1000., viewport(1000., 800.));
        assert!((zoom - 0.476).abs() < 0.005, "got {zoom}");
    }

    #[test]
    fn fit_modes_never_enlarge_a_small_image() {
        assert_eq!(
            fit_zoom(FitMode::Page, 100., 100., viewport(1000., 800.)),
            1.
        );
        assert_eq!(
            fit_zoom(FitMode::Width, 100., 100., viewport(1000., 800.)),
            1.
        );
    }

    #[test]
    fn fit_width_ignores_the_height() {
        let zoom = fit_zoom(FitMode::Width, 2000., 4000., viewport(1024., 200.));
        assert!((zoom - 0.488).abs() < 0.005, "got {zoom}");
    }

    #[test]
    fn fit_zoom_is_clamped_to_the_zoom_limits() {
        // A panorama far wider than any window sits at the minimum, not below.
        assert_eq!(
            fit_zoom(FitMode::Width, 100_000., 1000., viewport(1000., 800.)),
            zoom::MIN
        );
    }
}
