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
///
/// Extension and magic bytes are cross-checked so a mislabeled or damaged
/// file fails here (visible in the tab) instead of hanging on a blank viewer.
/// SVG is validated with `resvg` so dimensions are real and bad XML is caught.
pub fn load_image(path: &Path, bytes: Vec<u8>) -> Result<(Arc<Image>, f32, f32), String> {
    let by_ext = image_format(path);
    let sniffed = crate::images::sniff_format(&bytes);
    let format = resolve_image_format(path, by_ext, sniffed)?;

    if format == ImageFormat::Svg {
        let (width, height) = svg_dimensions(&bytes)?;
        let image = Arc::new(Image::from_bytes(ImageFormat::Svg, bytes));
        return Ok((image, width, height));
    }

    let (width, height) = probe_dimensions(&bytes, format).ok_or_else(|| {
        format!(
            "Couldn\u{2019}t read image dimensions ({})",
            format_label(format)
        )
    })?;
    // A photo of tens of megapixels would be decoded whole onto the GPU;
    // past what any screen shows, keep a copy at display resolution instead.
    // The layout still uses the full size, so zoom and fit are unchanged.
    let (format, bytes) = if width * height > MAX_DECODED_PIXELS {
        downscaled_display(&bytes, format).unwrap_or((format, bytes))
    } else {
        (format, bytes)
    };
    let image = Arc::new(Image::from_bytes(format, bytes));
    Ok((image, width, height))
}

fn format_label(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "PNG",
        ImageFormat::Jpeg => "JPEG",
        ImageFormat::Webp => "WebP",
        ImageFormat::Gif => "GIF",
        ImageFormat::Svg => "SVG",
        ImageFormat::Bmp => "BMP",
        ImageFormat::Tiff => "TIFF",
        ImageFormat::Ico => "ICO",
        _ => "image",
    }
}

fn sniff_to_format(sniffed: &str) -> Option<ImageFormat> {
    Some(match sniffed {
        "png" => ImageFormat::Png,
        "jpg" => ImageFormat::Jpeg,
        "webp" => ImageFormat::Webp,
        "gif" => ImageFormat::Gif,
        "svg" => ImageFormat::Svg,
        "bmp" => ImageFormat::Bmp,
        _ => return None,
    })
}

fn resolve_image_format(
    path: &Path,
    by_ext: Option<ImageFormat>,
    sniffed: Option<&'static str>,
) -> Result<ImageFormat, String> {
    let by_sniff = sniffed.and_then(sniff_to_format);
    match (by_ext, by_sniff, sniffed) {
        // Prefer magic bytes when they name a format we can show — a
        // `.svg` that is really a PNG still opens, and a real SVG opens
        // even when the extension was wrong.
        (_, Some(format), _) => Ok(format),
        (Some(ImageFormat::Svg), None, _) => {
            // Extension says SVG but the head has no `<svg` — still try
            // parsing; usvg will return a clear error if it is not SVG.
            Ok(ImageFormat::Svg)
        }
        (Some(format), None, _) => Err(format!(
            "File header does not look like {} (extension \u{201c}.{}\u{201d})",
            format_label(format),
            path.extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("?")
        )),
        (None, None, _) => Err(format!(
            "unknown image format ({})",
            path.extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("no extension")
        )),
    }
}

fn svg_dimensions(bytes: &[u8]) -> Result<(f32, f32), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        "SVG is not valid UTF-8 text".to_string()
    })?;
    let options = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_str(text, &options).map_err(|err| {
        format!("Couldn\u{2019}t parse SVG. {err}")
    })?;
    let size = tree.size();
    let (width, height) = (size.width(), size.height());
    if !(width.is_finite() && height.is_finite() && width > 0. && height > 0.) {
        return Err("SVG has no usable width/height".into());
    }
    Ok((width, height))
}

/// Pixels decoded for display at most: 4096 × 4096, 64 MB as RGBA.
const MAX_DECODED_PIXELS: f32 = 4096. * 4096.;

/// `bytes` decoded and scaled to fit 4096 × 4096, as lossless WebP (or
/// JPEG/PNG fallback) for the display cache. Full-size bitmap lives only
/// while this runs.
fn downscaled_display(bytes: &[u8], format: ImageFormat) -> Option<(ImageFormat, Vec<u8>)> {
    let fmt = match format {
        ImageFormat::Png => image::ImageFormat::Png,
        ImageFormat::Jpeg => image::ImageFormat::Jpeg,
        ImageFormat::Webp => image::ImageFormat::WebP,
        ImageFormat::Bmp => image::ImageFormat::Bmp,
        ImageFormat::Tiff => image::ImageFormat::Tiff,
        // An animated GIF would lose its frames.
        _ => return None,
    };
    let decoded = image::load_from_memory_with_format(bytes, fmt).ok()?;
    // `thumbnail` samples in the image's own pixel type; `resize` would
    // convert a 16-bit picture to floats first, several times its size.
    let (w, h) = (decoded.width(), decoded.height());
    let scale = (4096. / w.max(h) as f64).min(1.);
    let (tw, th) = (
        ((w as f64 * scale) as u32).max(1),
        ((h as f64 * scale) as u32).max(1),
    );
    let small = image::DynamicImage::ImageRgba8(match decoded {
        image::DynamicImage::ImageRgb8(rgb) => {
            image::DynamicImage::ImageRgb8(image::imageops::thumbnail(&rgb, tw, th)).into_rgba8()
        }
        image::DynamicImage::ImageRgba8(rgba) => image::imageops::thumbnail(&rgba, tw, th),
        other => other.thumbnail_exact(tw, th).into_rgba8(),
    });
    if let Some(encoded) = crate::display_raster::encode_dynamic_display(&small) {
        return Some(encoded);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    small.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some((ImageFormat::Png, out.into_inner()))
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

    /// The picture's own size in logical pixels.
    pub fn natural_size(&self) -> (f32, f32) {
        (self.width, self.height)
    }

    /// The factor the picture is drawn at.
    pub fn zoom(&self) -> f32 {
        self.zoom
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
        v_flex()
            .id("image-pages-scroll")
            // The pane's size, never the picture's: without `min_w_0` the
            // scroll area grew to a wide image's width, so the viewport it
            // measured (and the fit computed from it) was the image's own,
            // and a large picture opened over-zoomed and off-centre.
            .size_full()
            .min_w_0()
            .min_h_0()
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
                    .min_w_0()
                    .w_full()
                    .overflow_hidden()
                    // Measured here, outside the scroll area: an absolutely
                    // placed child of a scroll area is sized by what it
                    // scrolls, so a picture wider than the pane used to be
                    // fitted to its own width.
                    .on_prepaint({
                        let entity = cx.entity();
                        move |bounds, _, cx| {
                            // Notifying mid-frame can be lost; update once
                            // it's done.
                            cx.defer(move |cx| {
                                entity.update(cx, |this, cx| {
                                    if this.viewport != bounds.size {
                                        this.viewport = bounds.size;
                                        cx.notify();
                                    }
                                });
                            });
                        }
                    })
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .child(self.render_content(cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_huge_picture_is_kept_at_display_resolution() {
        let big = image::RgbImage::new(8192, 64);
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(big)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let small = super::downscaled_display(png.get_ref(), gpui_kit::ImageFormat::Png)
            .unwrap()
            .1;
        let decoded = image::load_from_memory(&small).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (4096, 32));
    }

    #[test]
    fn a_huge_16_bit_picture_is_scaled_too() {
        let big = image::ImageBuffer::<image::Rgb<u16>, _>::new(8192, 64);
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb16(big)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let small = super::downscaled_display(png.get_ref(), gpui_kit::ImageFormat::Png)
            .unwrap()
            .1;
        let decoded = image::load_from_memory(&small).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (4096, 32));
    }

    #[test]
    fn svg_opens_with_real_dimensions_and_rejects_garbage() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="40"><rect width="120" height="40"/></svg>"#;
        let (image, width, height) = load_image(Path::new("figure.svg"), svg.to_vec()).unwrap();
        assert_eq!(image.format(), ImageFormat::Svg);
        assert!((width - 120.).abs() < 0.01, "got {width}");
        assert!((height - 40.).abs() < 0.01, "got {height}");

        // XML prologue + viewBox (no width/height) still sniffs and sizes.
        let fancy = br#"<?xml version="1.0"?><svg viewBox="0 0 10 5" xmlns="http://www.w3.org/2000/svg"></svg>"#;
        let (_, w, h) = load_image(Path::new("x.SVG"), fancy.to_vec()).unwrap();
        assert!((w - 10.).abs() < 0.01 && (h - 5.).abs() < 0.01, "{w}x{h}");

        let err = load_image(Path::new("broken.svg"), b"not svg at all".to_vec()).unwrap_err();
        assert!(
            err.contains("parse") || err.contains("SVG") || err.contains("header"),
            "{err}"
        );

        // Extension says PNG but bytes are SVG — open as SVG via magic bytes.
        let (_, w, h) = load_image(Path::new("misnamed.png"), svg.to_vec()).unwrap();
        assert!((w - 120.).abs() < 0.01 && (h - 40.).abs() < 0.01);

        // Extension says PNG but bytes are garbage.
        let err = load_image(Path::new("bad.png"), b"hello".to_vec()).unwrap_err();
        assert!(err.contains("header") || err.contains("PNG"), "{err}");
    }

    #[test]
    fn a_wide_rgba_plot_reports_its_pixel_size() {
        // A matplotlib figure: 4050x1560 RGBA at 300 dpi. Its size must be
        // read from the file, so Fit Page can scale it into the pane.
        let mut png = Vec::new();
        image::RgbaImage::new(4050, 1560)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let (_, width, height) = load_image(Path::new("plot.png"), png).unwrap();
        assert_eq!((width, height), (4050., 1560.));
        // In a 1280-wide pane it opens well under 100%, whole and centred.
        let zoom = fit_zoom(FitMode::Page, width, height, viewport(1280., 760.));
        assert!(zoom < 0.32, "got {zoom}");
    }

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
