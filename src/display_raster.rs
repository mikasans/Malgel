//! Display-side raster compression for cached preview bitmaps.
//!
//! JPEG XL is not a GPUI `ImageFormat`, so live Typst sheets and Mermaid sharp
//! views keep compressed JPEG (lossy, ~quality 82) in the `Arc<Image>` cache
//! instead of PNG. Display fidelity is traded for RAM; GPU upload still expands
//! to RGBA. Opaque pages composite onto white before encode.

use std::io::Cursor;

use gpui_kit::ImageFormat;
use image::ColorType;
use image::codecs::jpeg::JpegEncoder;

/// JPEG quality for preview / live-sheet caches (fidelity OK to trade for RAM).
const DISPLAY_JPEG_QUALITY: u8 = 82;

/// Encode an RGBA pixmap for display cache. Prefers JPEG; falls back to PNG.
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
    if let Some(jpeg) = encode_jpeg_from_rgba(width, height, &rgba[..expected]) {
        return Some((ImageFormat::Jpeg, jpeg));
    }
    // PNG fallback keeps alpha if JPEG somehow fails.
    let png = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(
        width,
        height,
        rgba[..expected].to_vec(),
    )?;
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(png)
        .write_to(&mut out, image::ImageFormat::Png)
        .ok()?;
    Some((ImageFormat::Png, out.into_inner()))
}

/// Encode RGB (or RGBA composited on white) as JPEG.
fn encode_jpeg_from_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut rgb = Vec::with_capacity((width as usize) * (height as usize) * 3);
    for px in rgba.chunks_exact(4) {
        let a = px[3] as u16;
        if a == 255 {
            rgb.extend_from_slice(&px[..3]);
        } else if a == 0 {
            rgb.extend_from_slice(&[255, 255, 255]);
        } else {
            // Composite onto white: out = src*a + 255*(1-a)
            let inv = 255 - a;
            rgb.push(((px[0] as u16 * a + 255 * inv) / 255) as u8);
            rgb.push(((px[1] as u16 * a + 255 * inv) / 255) as u8);
            rgb.push(((px[2] as u16 * a + 255 * inv) / 255) as u8);
        }
    }
    let mut out = Cursor::new(Vec::new());
    let mut enc = JpegEncoder::new_with_quality(&mut out, DISPLAY_JPEG_QUALITY);
    enc.encode(&rgb, width, height, ColorType::Rgb8.into())
        .ok()?;
    Some(out.into_inner())
}

/// Re-encode a decoded thumbnail as JPEG for the image viewer display path.
pub fn encode_dynamic_jpeg(img: &image::DynamicImage) -> Option<Vec<u8>> {
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

    #[test]
    fn jpeg_beats_png_on_photoish_rgba() {
        let w = 256u32;
        let h = 256u32;
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for (i, px) in rgba.chunks_exact_mut(4).enumerate() {
            let x = (i as u32) % w;
            let y = (i as u32) / w;
            px[0] = (x % 256) as u8;
            px[1] = (y % 256) as u8;
            px[2] = ((x + y) % 256) as u8;
            px[3] = 255;
        }
        let (fmt, jpeg) = encode_rgba(w, h, &rgba).expect("encode");
        assert_eq!(fmt, ImageFormat::Jpeg);
        let png = {
            let img = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(w, h, rgba).unwrap();
            let mut out = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut out, image::ImageFormat::Png)
                .unwrap();
            out.into_inner()
        };
        assert!(
            jpeg.len() < png.len(),
            "jpeg {} should be smaller than png {}",
            jpeg.len(),
            png.len()
        );
        assert!(jpeg.starts_with(&[0xFF, 0xD8]), "JPEG SOI marker");
    }
}
