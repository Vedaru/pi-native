//! Native image resize/encode pipeline.
//!
//! Mirrors pi's `image-resize-core.ts`: keep an image if it is already within
//! the dimension and encoded-size limits; otherwise scale to fit and pick the
//! smallest encoding that fits, lowering JPEG quality and then dimensions until
//! it does. Implemented with the pure-Rust `image` crate, so there is no photon
//! WASM and no worker process.
//!
//! EXIF orientation is read and applied before resizing, as pi does.

use base64::Engine as _;
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageFormat, ImageReader};
use std::io::Cursor;

/// Default max encoded (base64) size: 4.5 MB, below Anthropic's 5 MB limit.
pub const DEFAULT_MAX_BYTES: usize = (4.5 * 1024.0 * 1024.0) as usize;

#[derive(Debug, Clone, Copy)]
pub struct ImageLimits {
    pub max_width: u32,
    pub max_height: u32,
    pub max_bytes: usize,
    pub jpeg_quality: u8,
}

impl Default for ImageLimits {
    fn default() -> Self {
        Self {
            max_width: 2000,
            max_height: 2000,
            max_bytes: DEFAULT_MAX_BYTES,
            jpeg_quality: 80,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResizedImage {
    pub data_base64: String,
    pub mime_type: String,
    pub original_width: u32,
    pub original_height: u32,
    pub width: u32,
    pub height: u32,
    pub was_resized: bool,
}

fn base64_len(bytes: usize) -> usize {
    bytes.div_ceil(3) * 4
}

fn encode(img: &DynamicImage, format: ImageFormat, quality: u8) -> Option<Vec<u8>> {
    match format {
        ImageFormat::Jpeg => {
            let mut buffer = Vec::new();
            let rgb = img.to_rgb8();
            JpegEncoder::new_with_quality(&mut buffer, quality)
                .encode_image(&rgb)
                .ok()?;
            Some(buffer)
        }
        other => {
            let mut buffer = Vec::new();
            img.write_to(&mut Cursor::new(&mut buffer), other).ok()?;
            Some(buffer)
        }
    }
}

struct Candidate {
    bytes: Vec<u8>,
    mime: &'static str,
}

/// Try PNG and several JPEG qualities, returning the encodings that fit.
/// JPEG quality steps in pi's order: the configured quality, then 85/70/55/40.
fn quality_steps(configured: u8) -> Vec<u8> {
    let mut steps = Vec::new();
    for quality in [configured, 85, 70, 55, 40] {
        if !steps.contains(&quality) {
            steps.push(quality);
        }
    }
    steps
}

/// The first encoding under the limit in pi's preference order: PNG, then JPEG
/// at the configured quality and lower steps. Returns the first that fits rather
/// than the smallest, matching pi (higher quality wins when it fits).
fn first_fitting(
    img: &DynamicImage,
    configured_quality: u8,
    max_bytes: usize,
) -> Option<Candidate> {
    if let Some(bytes) = encode(img, ImageFormat::Png, configured_quality) {
        if base64_len(bytes.len()) < max_bytes {
            return Some(Candidate {
                bytes,
                mime: "image/png",
            });
        }
    }
    for quality in quality_steps(configured_quality) {
        if let Some(bytes) = encode(img, ImageFormat::Jpeg, quality) {
            if base64_len(bytes.len()) < max_bytes {
                return Some(Candidate {
                    bytes,
                    mime: "image/jpeg",
                });
            }
        }
    }
    None
}

/// Apply an EXIF orientation (1-8) to an image. Unknown values are a no-op.
pub fn apply_orientation(image: DynamicImage, orientation: u16) -> DynamicImage {
    match orientation {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.rotate90().fliph(),
        6 => image.rotate90(),
        7 => image.rotate90().flipv(),
        8 => image.rotate270(),
        _ => image,
    }
}

/// Read the EXIF orientation tag, if present.
pub fn read_orientation(bytes: &[u8]) -> Option<u16> {
    let mut cursor = Cursor::new(bytes);
    let exif = exif::Reader::new().read_from_container(&mut cursor).ok()?;
    exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?
        .value
        .get_uint(0)
        .map(|value| value as u16)
}

/// Scale dimensions to fit the limits, preserving aspect ratio.
fn fit(width: u32, height: u32, limits: &ImageLimits) -> (u32, u32) {
    let mut target_width = width;
    let mut target_height = height;
    if target_width > limits.max_width {
        target_height =
            ((target_height as u64 * limits.max_width as u64) / target_width as u64).max(1) as u32;
        target_width = limits.max_width;
    }
    if target_height > limits.max_height {
        target_width =
            ((target_width as u64 * limits.max_height as u64) / target_height as u64).max(1) as u32;
        target_height = limits.max_height;
    }
    (target_width.max(1), target_height.max(1))
}

/// Resize `input` to fit `limits`, returning the chosen encoding or `None` if it
/// cannot be brought under the byte limit.
pub fn resize_image(input: &[u8], limits: &ImageLimits) -> Option<ResizedImage> {
    let decoded = ImageReader::new(Cursor::new(input))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    // EXIF orientation is metadata; apply it so the pixels match what the user
    // sees before any dimension math.
    let decoded = match read_orientation(input) {
        Some(orientation) => apply_orientation(decoded, orientation),
        None => decoded,
    };
    let original_width = decoded.width();
    let original_height = decoded.height();

    // Already within every limit: return the original bytes unchanged.
    if original_width <= limits.max_width
        && original_height <= limits.max_height
        && base64_len(input.len()) < limits.max_bytes
    {
        return Some(ResizedImage {
            data_base64: base64::engine::general_purpose::STANDARD.encode(input),
            mime_type: guess_mime(input).to_string(),
            original_width,
            original_height,
            width: original_width,
            height: original_height,
            was_resized: false,
        });
    }

    let (mut target_width, mut target_height) = fit(original_width, original_height, limits);
    let mut scaled: Option<DynamicImage> = None;

    loop {
        let current = scaled.get_or_insert_with(|| {
            if target_width == original_width && target_height == original_height {
                decoded.clone()
            } else {
                decoded.resize_exact(
                    target_width,
                    target_height,
                    image::imageops::FilterType::Lanczos3,
                )
            }
        });

        if let Some(best) = first_fitting(current, limits.jpeg_quality, limits.max_bytes) {
            return Some(ResizedImage {
                data_base64: base64::engine::general_purpose::STANDARD.encode(&best.bytes),
                mime_type: best.mime.to_string(),
                original_width,
                original_height,
                width: target_width,
                height: target_height,
                was_resized: true,
            });
        }

        if target_width <= 1 && target_height <= 1 {
            return None;
        }
        // pi shrinks by 25% per retry.
        target_width = (target_width * 3 / 4).max(1);
        target_height = (target_height * 3 / 4).max(1);
        scaled = None;
    }
}

fn guess_mime(bytes: &[u8]) -> &'static str {
    match image::guess_format(bytes) {
        Ok(ImageFormat::Png) => "image/png",
        Ok(ImageFormat::Jpeg) => "image/jpeg",
        Ok(ImageFormat::Gif) => "image/gif",
        Ok(ImageFormat::WebP) => "image/webp",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
#[path = "../tests/unit/lib.rs"]
mod tests;
