/// Image encoding: rescale S2 SR values → u8, encode to PNG / JPEG.
use crate::composite::SceneTile;
use anyhow::Result;
use bytes::Bytes;
use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba, RgbaImage};
use std::io::Cursor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Png,
    Jpeg,
    WebP,
}

impl OutputFormat {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "png" => Some(OutputFormat::Png),
            "jpg" | "jpeg" => Some(OutputFormat::Jpeg),
            "webp" => Some(OutputFormat::WebP),
            _ => None,
        }
    }

    pub fn content_type(&self) -> &'static str {
        match self {
            OutputFormat::Png => "image/png",
            OutputFormat::Jpeg => "image/jpeg",
            OutputFormat::WebP => "image/webp",
        }
    }

    pub fn image_format(&self) -> ImageFormat {
        match self {
            OutputFormat::Png => ImageFormat::Png,
            OutputFormat::Jpeg => ImageFormat::Jpeg,
            OutputFormat::WebP => ImageFormat::WebP,
        }
    }
}

/// Rescale a u16 value from [lo, hi] → [0, 255], clamped.
#[inline]
fn rescale_u16(v: u16, lo: f64, hi: f64) -> u8 {
    let range = (hi - lo).max(1.0);
    let scaled = (v as f64 - lo) / range * 255.0;
    scaled.clamp(0.0, 255.0).round() as u8
}

/// Rescale a float value from [lo, hi] → [0, 255], clamped.
#[inline]
fn rescale_f32(v: f32, lo: f64, hi: f64) -> u8 {
    let range = (hi - lo).max(1e-9);
    let scaled = (v as f64 - lo) / range * 255.0;
    scaled.clamp(0.0, 255.0).round() as u8
}

/// Encode a composited tile to image bytes.
///
/// `rescale` — [lo, hi] value range mapped to [0, 255].
///             For spectral tiles: S2 SR units (e.g. [0, 3000]).
///             For NDVI tiles: NDVI float range (e.g. [-1, 1] or [0, 1]).
/// `format`  — PNG (with alpha) or JPEG (no alpha, transparency → black)
pub fn encode_tile(
    tile: &SceneTile,
    rescale: [f64; 2],
    format: OutputFormat,
) -> Result<Bytes> {
    let size = tile.size() as u32;
    let [lo, hi] = rescale;

    let mut img: RgbaImage = ImageBuffer::new(size, size);

    for row in 0..size as usize {
        for col in 0..size as usize {
            // Unfilled pixels render as opaque black rather than transparent.
            // Transparency is reserved for tiles with no data at all (Ok(None) path),
            // which covers areas fully outside the tileset extent.
            let alpha = 255u8;

            let (r, g, b) = if let Some(ndvi) = &tile.ndvi {
                // NDVI: single f32 channel → grayscale
                let v = rescale_f32(ndvi[[row, col]], lo, hi);
                (v, v, v)
            } else {
                match tile.bands() {
                    1 => {
                        let v = rescale_u16(tile.data[[0, row, col]], lo, hi);
                        (v, v, v)
                    }
                    3 => (
                        rescale_u16(tile.data[[0, row, col]], lo, hi),
                        rescale_u16(tile.data[[1, row, col]], lo, hi),
                        rescale_u16(tile.data[[2, row, col]], lo, hi),
                    ),
                    _ => {
                        let v = rescale_u16(tile.data[[0, row, col]], lo, hi);
                        (v, v, v)
                    }
                }
            };

            img.put_pixel(col as u32, row as u32, Rgba([r, g, b, alpha]));
        }
    }

    let mut buf: Vec<u8> = Vec::new();
    DynamicImage::ImageRgba8(img).write_to(&mut Cursor::new(&mut buf), format.image_format())?;
    Ok(Bytes::from(buf))
}

/// Return a 256×256 fully transparent PNG (used for out-of-extent tiles).
pub fn empty_tile_png() -> Bytes {
    let img: RgbaImage = ImageBuffer::new(256, 256);
    let mut buf: Vec<u8> = Vec::new();
    DynamicImage::ImageRgba8(img)
        .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
        .expect("empty PNG failed");
    Bytes::from(buf)
}
