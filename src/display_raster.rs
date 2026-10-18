//! Display-side raster compression for cached preview bitmaps.
//!
//! JPEG XL and AVIF are not GPUI `ImageFormat`s. Among what GPUI can upload
//! (PNG / JPEG / WebP / …), **lossless WebP** wins memory + encode/decode for
//! Typst sheets and Mermaid views: flat white + text/diagrams compress far
//! smaller than JPEG q82, and the pure-Rust encoder is fast. Lossy WebP needs
//! libwebp (not in-tree); JPEG remains a fallback if WebP encode fails.
//! Opaque pages composite onto white before RGB encode paths.

use std::io::Cursor;

use gpui_kit::ImageFormat;
use image::ColorType;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::webp::WebPEncoder;

/// JPEG quality if WebP encode fails (legacy / fallback).
const DISPLAY_JPEG_QUALITY: u8 = 82;

/// Encode an RGBA pixmap for display cache. Prefers lossless WebP; then JPEG;
/// then PNG.
pub fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<(ImageFormat, Vec<u8>)> {
    if width == 0 || height == 0 {
        return None;
    }
    let expected = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if rgba.len() < expected {
        return None;
    }
    let slice = &rgba[..expected];
    if let Some(webp) = encode_webp_lossless_rgba(width, height, slice) {
        return Some((ImageFormat::Webp, webp));
    }
    if let Some(jpeg) = encode_jpeg_from_rgba(width, height, slice) {
        return Some((ImageFormat::Jpeg, jpeg));
    }
    // PNG fallback keeps alpha if both preferred codecs fail.
    let png = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(width, height, slice.to_vec())?;
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(png)
        .write_to(&mut out, image::ImageFormat::Png)
        .ok()?;
    Some((ImageFormat::Png, out.into_inner()))
}

/// Lossless WebP (VP8L) — best RAM for document/diagram display caches under
/// GPUI. Keeps alpha; Typst/Mermaid sheets are mostly opaque anyway.
fn encode_webp_lossless_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    WebPEncoder::new_lossless(&mut out)
        .encode(rgba, width, height, image::ExtendedColorType::Rgba8)
        .ok()?;
    Some(out.into_inner())
}

/// Encode RGB (or RGBA composited on white) as JPEG.
fn encode_jpeg_from_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut rgb = Vec::with_capacity((width as usize) * (height as usize) * 3);
    for &[r, g, b, a] in rgba.as_chunks::<4>().0 {
        let a = a as u16;
        if a == 255 {
            rgb.extend_from_slice(&[r, g, b]);
        } else if a == 0 {
            rgb.extend_from_slice(&[255, 255, 255]);
        } else {
            // Composite onto white: out = src*a + 255*(1-a)
            let inv = 255 - a;
            rgb.push(((r as u16 * a + 255 * inv) / 255) as u8);
            rgb.push(((g as u16 * a + 255 * inv) / 255) as u8);
            rgb.push(((b as u16 * a + 255 * inv) / 255) as u8);
        }
    }
    let mut out = Cursor::new(Vec::new());
    let mut enc = JpegEncoder::new_with_quality(&mut out, DISPLAY_JPEG_QUALITY);
    enc.encode(&rgb, width, height, ColorType::Rgb8.into())
        .ok()?;
    Some(out.into_inner())
}

/// Re-encode a decoded thumbnail for the image viewer display path.
pub fn encode_dynamic_display(img: &image::DynamicImage) -> Option<(ImageFormat, Vec<u8>)> {
    let rgba = img.to_rgba8();
    encode_rgba(rgba.width(), rgba.height(), rgba.as_raw())
}

/// Backward-compatible JPEG-only helper (tests / call sites that only want bytes).
#[cfg(test)]
fn encode_dynamic_jpeg(img: &image::DynamicImage) -> Option<Vec<u8>> {
    let rgb = img.to_rgb8();
    let mut out = Cursor::new(Vec::new());
    let mut enc = JpegEncoder::new_with_quality(&mut out, DISPLAY_JPEG_QUALITY);
    enc.encode(
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        ColorType::Rgb8.into(),
    )
    .ok()?;
    Some(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_rgba(w: u32, h: u32) -> Vec<u8> {
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x = (i as u32) % w;
            let y = (i as u32) / w;
            px[0] = (x % 256) as u8;
            px[1] = (y % 256) as u8;
            px[2] = ((x + y) % 256) as u8;
            px[3] = 255;
        }
        rgba
    }

    #[test]
    fn webp_beats_jpeg_and_png_on_pageish_rgba() {
        let w = 256u32;
        let h = 256u32;
        // Mostly white + ink runs (Typst/Mermaid-like).
        let mut rgba = vec![255u8; (w * h * 4) as usize];
        for y in 16..h - 16 {
            for x in 16..w - 16 {
                if (y % 20) < 10 && (x / 5) % 2 == 0 {
                    let i = ((y * w + x) * 4) as usize;
                    rgba[i] = 30;
                    rgba[i + 1] = 30;
                    rgba[i + 2] = 30;
                }
            }
        }
        let (fmt, webp) = encode_rgba(w, h, &rgba).expect("encode");
        assert_eq!(fmt, ImageFormat::Webp);
        assert!(webp.starts_with(b"RIFF") && webp[8..12] == *b"WEBP");

        let jpeg = encode_jpeg_from_rgba(w, h, &rgba).expect("jpeg");
        let png = {
            let img =
                image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(w, h, rgba.clone()).unwrap();
            let mut out = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut out, image::ImageFormat::Png)
                .unwrap();
            out.into_inner()
        };
        assert!(
            webp.len() < jpeg.len() && webp.len() < png.len(),
            "webp {} should beat jpeg {} and png {}",
            webp.len(),
            jpeg.len(),
            png.len()
        );
    }

    #[test]
    fn webp_preferred_on_photoish_rgba() {
        let w = 128u32;
        let h = 128u32;
        let rgba = sample_rgba(w, h);
        let (fmt, bytes) = encode_rgba(w, h, &rgba).expect("encode");
        assert_eq!(fmt, ImageFormat::Webp);
        assert!(bytes.starts_with(b"RIFF"));
        let _ = encode_dynamic_jpeg(&image::DynamicImage::ImageRgba8(
            image::ImageBuffer::from_raw(w, h, rgba).unwrap(),
        ));
    }
}
