//! Memory-bounded decode: shrink while decoding so the full-size bitmap is
//! never materialised (the pixer strategy).
//!
//! The expensive path was `image`'s decode: it built the whole source bitmap and
//! then resized it. Here the header is read first, a decode-time reduction is
//! chosen to cover the target, and only the reduced bitmap is held:
//!
//! - **JPEG** uses `libjpeg-turbo-rs`'s scaled IDCT (any of the 16 libjpeg-turbo
//!   factors) so a large JPEG is never decoded at full size.
//! - **PNG** is decoded scanline by scanline and box-downsampled on the fly, so
//!   only the reduced bitmap is allocated.
//! - other formats fall back to `image`'s full decode (small in practice).
//!
//! The reduced bitmap is then handed to `fast_image_resize` for the final
//! quality resize, which is cheap at that size.

use image::{DynamicImage, GrayImage, RgbImage};
use std::io::Cursor;

/// A decoded bitmap and the dimensions it was decoded from.
pub struct Decoded {
    pub image: DynamicImage,
    pub width: u32,
    pub height: u32,
}

/// The inline image types pi accepts, from the file header.
pub fn mime_type(header: &[u8]) -> Option<&'static str> {
    if header.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if header.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if header.len() >= 12 && &header[0..4] == b"RIFF" && &header[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Decode `input` at the smallest reduction whose result still covers
/// `(target_w, target_h)`. Returns `None` on any failure.
pub fn decode(input: &[u8], target_w: u32, target_h: u32) -> Option<Decoded> {
    match mime_type(input)? {
        "image/jpeg" => decode_jpeg(input, target_w, target_h),
        "image/png" => decode_png(input, target_w, target_h),
        _ => decode_full(input),
    }
}

/// Full decode via the `image` crate (GIF/WebP, and any fallback).
fn decode_full(input: &[u8]) -> Option<Decoded> {
    let image = image::ImageReader::new(Cursor::new(input))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    Some(Decoded {
        width: image.width(),
        height: image.height(),
        image,
    })
}

/// The original dimensions without decoding pixels.
pub fn dimensions(input: &[u8]) -> Option<(u32, u32)> {
    match mime_type(input)? {
        "image/jpeg" => {
            let decoder = libjpeg_turbo_rs::Decoder::new(input).ok()?;
            let header = decoder.header();
            Some((header.width as u32, header.height as u32))
        }
        "image/png" => {
            let reader = png::Decoder::new(Cursor::new(input)).read_info().ok()?;
            Some((reader.info().width, reader.info().height))
        }
        _ => {
            let reader = image::ImageReader::new(Cursor::new(input))
                .with_guessed_format()
                .ok()?;
            let dims = reader.into_dimensions().ok()?;
            Some(dims)
        }
    }
}

fn decode_jpeg(input: &[u8], target_w: u32, target_h: u32) -> Option<Decoded> {
    use libjpeg_turbo_rs::{Decoder, PixelFormat, ScalingFactor};
    // libjpeg-turbo supports all 16 scaled-IDCT factors (2/1 .. 1/8). Pick the
    // smallest whose output still covers the target, so a 7000px JPEG decodes at
    // 3/8 (2625) rather than 1/2 (3500): a smaller bitmap, and the final resize
    // only downscales.
    const FACTORS: &[(u32, u32)] = &[
        (1, 8),
        (1, 4),
        (3, 8),
        (1, 2),
        (5, 8),
        (3, 4),
        (7, 8),
        (1, 1),
    ];
    let mut decoder = Decoder::new(input).ok()?;
    let header = decoder.header();
    let (width, height) = (header.width as u32, header.height as u32);
    // Ask for RGB; libjpeg-turbo converts CMYK/YCCK correctly itself.
    decoder.set_output_format(PixelFormat::Rgb);
    let mut scale = ScalingFactor::new(1, 1);
    for (num, denom) in FACTORS {
        let candidate = ScalingFactor::new(*num, *denom);
        if candidate.scale_dim(width as usize) >= target_w as usize
            && candidate.scale_dim(height as usize) >= target_h as usize
        {
            scale = candidate;
            break;
        }
    }
    decoder.set_scale(scale);
    let decoded = decoder.decode_image().ok()?;
    let rgb = RgbImage::from_raw(decoded.width as u32, decoded.height as u32, decoded.data)?;
    Some(Decoded {
        image: DynamicImage::ImageRgb8(rgb),
        width,
        height,
    })
}

fn decode_png(input: &[u8], target_w: u32, target_h: u32) -> Option<Decoded> {
    use png::ColorType;
    let mut reader = png::Decoder::new(Cursor::new(input)).read_info().ok()?;
    {
        let info = reader.info();
        if info.bit_depth != png::BitDepth::Eight {
            return decode_full(input);
        }
    }
    let (width, height, color, channels) = {
        let info = reader.info();
        let channels = match info.color_type {
            ColorType::Grayscale => 1usize,
            ColorType::GrayscaleAlpha => 2,
            ColorType::Rgb => 3,
            ColorType::Rgba => 4,
            // Palette (and 16-bit) go through the `image` crate.
            ColorType::Indexed => return decode_full(input),
        };
        (info.width, info.height, info.color_type, channels)
    };

    // Reduction factor: the largest integer that keeps both axes at or above the
    // target, so the streaming pass only downscales and the final quality resize
    // never upscales. When already within the target, decode at full size.
    let factor = (width / target_w.max(1))
        .min(height / target_h.max(1))
        .max(1);
    if factor == 1 {
        return decode_full(input);
    }
    let out_w = (width / factor).max(1);
    let out_h = (height / factor).max(1);
    // Accumulate one output row at a time: the whole output image stays 8-bit,
    // and only a `factor`-row sum is held (a u32 image would cost 4x the output).
    let mut pixels = vec![0u8; out_w as usize * out_h as usize * channels];
    let mut row_sum = vec![0u32; out_w as usize * channels];
    let mut y = 0u32;
    while let Some(row) = reader.next_row().ok()? {
        if y >= out_h * factor {
            break;
        }
        let oy = (y / factor) as usize;
        let bytes = row.data();
        for x in 0..out_w as usize {
            for c in 0..channels {
                let mut sum = 0u32;
                for fx in 0..factor as usize {
                    let ix = x * factor as usize + fx;
                    if ix < width as usize {
                        sum += bytes[ix * channels + c] as u32;
                    }
                }
                row_sum[x * channels + c] += sum;
            }
        }
        y += 1;
        if y.is_multiple_of(factor) {
            let divisor = factor * factor;
            let row_off = oy * out_w as usize * channels;
            for (i, sum) in row_sum.iter_mut().enumerate() {
                pixels[row_off + i] = (*sum / divisor).min(255) as u8;
                *sum = 0;
            }
        }
    }
    let _ = color;
    let image = match channels {
        1 => DynamicImage::ImageLuma8(GrayImage::from_raw(out_w, out_h, pixels)?),
        2 => {
            let mut rgb = Vec::with_capacity(out_w as usize * out_h as usize * 3);
            for px in pixels.chunks_exact(2) {
                rgb.extend_from_slice(&[px[0], px[0], px[0]]);
            }
            DynamicImage::ImageRgb8(RgbImage::from_raw(out_w, out_h, rgb)?)
        }
        3 => DynamicImage::ImageRgb8(RgbImage::from_raw(out_w, out_h, pixels)?),
        _ => {
            let mut rgb = Vec::with_capacity(out_w as usize * out_h as usize * 3);
            for px in pixels.chunks_exact(4) {
                rgb.extend_from_slice(&px[0..3]);
            }
            DynamicImage::ImageRgb8(RgbImage::from_raw(out_w, out_h, rgb)?)
        }
    };
    Some(Decoded {
        image,
        width,
        height,
    })
}
