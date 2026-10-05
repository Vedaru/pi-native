//! Resize an image and report the result.
//!
//! Usage: cargo run -p pi-image --example resize -- <input> [max_width] [max_height] [max_bytes]

use pi_image::{resize_image, ImageLimits};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .get(1)
        .expect("usage: resize <input> [max_w] [max_h] [max_bytes]");
    let mut limits = ImageLimits::default();
    if let Some(value) = args.get(2).and_then(|v| v.parse().ok()) {
        limits.max_width = value;
    }
    if let Some(value) = args.get(3).and_then(|v| v.parse().ok()) {
        limits.max_height = value;
    }
    if let Some(value) = args.get(4).and_then(|v| v.parse().ok()) {
        limits.max_bytes = value;
    }

    let input = std::fs::read(path).expect("read input");
    let started = std::time::Instant::now();
    let result = resize_image(&input, &limits).expect("resize");
    let elapsed = started.elapsed();

    println!(
        "{}x{} -> {}x{} ({}) in {:?}; base64 {} bytes; was_resized={}",
        result.original_width,
        result.original_height,
        result.width,
        result.height,
        result.mime_type,
        elapsed,
        result.data_base64.len(),
        result.was_resized,
    );
}
