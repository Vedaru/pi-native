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
use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageFormat};
use std::io::Cursor;

pub mod decode;

/// Default max encoded (base64) size: 4.5 MB, below Anthropic's 5 MB limit.
pub const DEFAULT_MAX_BYTES: usize = (4.5 * 1024.0 * 1024.0) as usize;

/// Caps how many image decodes may run at once in one process.
///
/// A single decode+resize peaks at tens of MB; the gateway hosts many units in
/// one process, so several units reading an image at the same time add up. The
/// gate serialises them so aggregate image memory stays bounded. Override with
/// `PIPELETS_IMAGE_CONCURRENCY`.
struct DecodeGate {
    active: std::sync::Mutex<usize>,
    ready: std::sync::Condvar,
    max: usize,
}

impl DecodeGate {
    fn acquire(self: &'static DecodeGate) -> DecodePermit {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        while *active >= self.max {
            active = self
                .ready
                .wait(active)
                .unwrap_or_else(|error| error.into_inner());
        }
        *active += 1;
        DecodePermit(self)
    }

    fn release(&self) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *active = active.saturating_sub(1);
        self.ready.notify_one();
    }
}

/// Holds a decode slot until dropped.
struct DecodePermit(&'static DecodeGate);

impl Drop for DecodePermit {
    fn drop(&mut self) {
        self.0.release();
    }
}

fn decode_gate() -> &'static DecodeGate {
    static GATE: std::sync::OnceLock<DecodeGate> = std::sync::OnceLock::new();
    GATE.get_or_init(|| DecodeGate {
        active: std::sync::Mutex::new(0),
        ready: std::sync::Condvar::new(),
        max: std::env::var("PIPELETS_IMAGE_CONCURRENCY")
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|&max| max > 0)
            .unwrap_or(2),
    })
}

/// Hand freed image memory back to the OS.
///
/// glibc keeps freed arenas, so after a decode the process can sit at the
/// transient high-water mark at idle. `malloc_trim(0)` returns what it can;
/// measured on a 2100x2100 read: 11.5 MB -> 4.7 MB.
#[cfg(all(unix, target_env = "gnu"))]
fn release_free_memory() {
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(not(all(unix, target_env = "gnu")))]
fn release_free_memory() {}

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

fn encode_png(img: &DynamicImage) -> Option<Vec<u8>> {
    let mut buffer = Vec::new();
    img.write_to(&mut Cursor::new(&mut buffer), ImageFormat::Png)
        .ok()?;
    Some(buffer)
}

fn encode_jpeg(rgb: &image::RgbImage, quality: u8) -> Option<Vec<u8>> {
    let mut buffer = Vec::new();
    JpegEncoder::new_with_quality(&mut buffer, quality)
        .encode_image(rgb)
        .ok()?;
    Some(buffer)
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
///
/// The RGB conversion JPEG needs is done **once** and reused across every
/// quality step; re-deriving it per step dominated both CPU and peak memory
/// (each conversion allocates a full frame).
fn first_fitting(
    img: &DynamicImage,
    configured_quality: u8,
    max_bytes: usize,
) -> Option<Candidate> {
    if let Some(bytes) = encode_png(img) {
        if base64_len(bytes.len()) < max_bytes {
            return Some(Candidate {
                bytes,
                mime: "image/png",
            });
        }
    }
    // Reuse the RGB view when the image already is RGB8; only convert otherwise.
    // (`to_rgb8()` on an RGB8 image is a full-frame copy — 12 MB at 2000x2000.)
    let converted;
    let rgb: &image::RgbImage = match img {
        DynamicImage::ImageRgb8(rgb) => rgb,
        other => {
            converted = other.to_rgb8();
            &converted
        }
    };
    for quality in quality_steps(configured_quality) {
        if let Some(bytes) = encode_jpeg(rgb, quality) {
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

/// Encode a solid-colour RGB PNG of the given size.
///
/// Used by the `--stress` image workload and image tests to build a fixture of
/// a known size without shipping a binary file.
pub fn solid_png(width: u32, height: u32) -> Vec<u8> {
    let image = DynamicImage::new_rgb8(width, height);
    let mut out = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .expect("encode png");
    out
}

/// Encode a screenshot-like RGB PNG: a smooth gradient plus light noise, so the
/// encoded size is in the range a real screenshot produces (a solid image
/// compresses to almost nothing and would understate the per-session cost).
pub fn screenshot_png(width: u32, height: u32) -> Vec<u8> {
    let mut image = image::RgbImage::new(width, height);
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let gradient = (x * 255 / width.max(1)) as u8;
        let noise = ((seed >> 24) & 0x1f) as u8;
        *pixel = image::Rgb([
            gradient.wrapping_add(noise),
            (y * 255 / height.max(1)) as u8,
            ((x ^ y) & 0x7f) as u8,
        ]);
    }
    let mut out = Vec::new();
    DynamicImage::ImageRgb8(image)
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .expect("encode png");
    out
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

/// An empty `DynamicImage` with the same pixel layout as `img`.
fn empty_like(img: &DynamicImage, width: u32, height: u32) -> DynamicImage {
    match img {
        DynamicImage::ImageLuma8(_) => DynamicImage::new_luma8(width, height),
        DynamicImage::ImageLumaA8(_) => DynamicImage::new_luma_a8(width, height),
        DynamicImage::ImageRgb8(_) => DynamicImage::new_rgb8(width, height),
        DynamicImage::ImageRgba8(_) => DynamicImage::new_rgba8(width, height),
        DynamicImage::ImageLuma16(_) => DynamicImage::new_luma16(width, height),
        DynamicImage::ImageLumaA16(_) => DynamicImage::new_luma_a16(width, height),
        DynamicImage::ImageRgb16(_) => DynamicImage::new_rgb16(width, height),
        DynamicImage::ImageRgba16(_) => DynamicImage::new_rgba16(width, height),
        DynamicImage::ImageRgb32F(_) => DynamicImage::new_rgb32f(width, height),
        DynamicImage::ImageRgba32F(_) => DynamicImage::new_rgba32f(width, height),
        _ => DynamicImage::new_rgba8(width, height),
    }
}

/// Resize `img` to `(tw, th)` with `fast_image_resize`.
///
/// `fast_image_resize` is SIMD-accelerated and row-streamed: it allocates only
/// the destination plus a small convolution buffer, so peak memory is
/// independent of the source size. (`image`'s own Lanczos3 builds a full
/// `source_width x target_height` RGBA-f32 transient — 288 MB for a 9000x9000
/// image — which is what made large images expensive.)
///
/// Falls back to the `image` crate for pixel types the resizer rejects.
fn resample(img: &DynamicImage, tw: u32, th: u32) -> DynamicImage {
    let mut dst = empty_like(img, tw, th);
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
    let mut resizer = Resizer::new();
    match resizer.resize(img, &mut dst, &options) {
        Ok(()) => dst,
        Err(_) => img.resize_exact(tw, th, image::imageops::FilterType::Lanczos3),
    }
}

/// Scale dimensions to fit the limits, preserving aspect ratio.
fn fit(width: u32, height: u32, limits: &ImageLimits) -> (u32, u32) {
    let mut target_width = width;
    let mut target_height = height;
    // pi rounds the scaled axis (`Math.round`), not floors it.
    let round_div = |value: u64, by: u64| ((value + by / 2) / by).max(1) as u32;
    if target_width > limits.max_width {
        target_height = round_div(
            target_height as u64 * limits.max_width as u64,
            target_width as u64,
        );
        target_width = limits.max_width;
    }
    if target_height > limits.max_height {
        target_width = round_div(
            target_width as u64 * limits.max_height as u64,
            target_height as u64,
        );
        target_height = limits.max_height;
    }
    (target_width.max(1), target_height.max(1))
}

/// Resize `input` to fit `limits`, returning the chosen encoding or `None` if it
/// cannot be brought under the byte limit.
pub fn resize_image(input: &[u8], limits: &ImageLimits) -> Option<ResizedImage> {
    // One decode at a time per slot: bounds aggregate memory across units.
    let _permit = decode_gate().acquire();
    let result = resize_image_inner(input, limits);
    // glibc keeps the freed transient otherwise; return it before idling.
    release_free_memory();
    result
}

fn resize_image_inner(input: &[u8], limits: &ImageLimits) -> Option<ResizedImage> {
    // Header first: raw dimensions without decoding pixels.
    let (raw_width, raw_height) = decode::dimensions(input)?;
    // EXIF orientation is metadata; pi reports the *oriented* dimensions and
    // resizes after rotating. A 90/270 rotation swaps width and height.
    let orientation = read_orientation(input);
    let rotated = matches!(orientation, Some(5..=8));
    let (original_width, original_height) = if rotated {
        (raw_height, raw_width)
    } else {
        (raw_width, raw_height)
    };

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
    // Decode at the smallest reduction that still covers the target, in the
    // pixels' own (un-rotated) orientation, so the full-size source bitmap is
    // never materialised (shrink while decoding).
    let (decode_w, decode_h) = if rotated {
        (target_height, target_width)
    } else {
        (target_width, target_height)
    };
    let decoded = decode::decode(input, decode_w, decode_h)?;
    let decoded = match orientation {
        Some(orientation) => apply_orientation(decoded.image, orientation),
        None => decoded.image,
    };
    let mut scaled: Option<DynamicImage> = None;

    loop {
        let current = scaled.get_or_insert_with(|| {
            if target_width == original_width && target_height == original_height {
                decoded.clone()
            } else {
                resample(&decoded, target_width, target_height)
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
