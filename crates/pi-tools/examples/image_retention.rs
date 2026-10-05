//! Does one image read return its memory, or does the unit stay large?
//!
//! Runs a `read` of a 2100x2100 PNG, drops the result, then samples RSS (from
//! `/proc/self/status`) before, right after, after idling, and after
//! `malloc_trim(0)`. A long-lived unit that ever read an image must not sit at
//! the resize high-water mark forever.
//!
//! Usage: cargo run --release -p pi-tools --example image_retention

use pi_tools::{ReadTool, Tool, ToolContext};

#[cfg(target_os = "linux")]
fn rss_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            if let Some(value) = rest.split_whitespace().next() {
                return value.parse().unwrap_or(0);
            }
        }
    }
    0
}

#[cfg(not(target_os = "linux"))]
fn rss_kb() -> u64 {
    0
}

fn report(label: &str) {
    println!("{label:<28} rss {:>6.1} MB", rss_kb() as f64 / 1024.0);
}

#[cfg(target_os = "linux")]
fn hwm_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            if let Some(value) = rest.split_whitespace().next() {
                return value.parse().unwrap_or(0);
            }
        }
    }
    0
}

#[cfg(not(target_os = "linux"))]
fn hwm_kb() -> u64 {
    0
}

#[cfg(unix)]
fn trim() {
    // glibc keeps freed arenas; release them back to the OS.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(not(unix))]
fn trim() {}

fn main() {
    let dir = std::env::temp_dir().join(format!("pi-image-retention-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let source = std::env::args().nth(1);
    if let Some(path) = &source {
        std::fs::copy(path, dir.join("shot.png")).expect("copy image");
    } else {
        std::fs::write(dir.join("shot.png"), pi_image::screenshot_png(2100, 2100))
            .expect("write image");
    }
    std::fs::copy(dir.join("shot.png"), dir.join("shot.in")).expect("copy");
    let ctx = ToolContext::new(&dir);

    report("baseline (idle)");
    let result = ReadTool.run(&serde_json::json!({ "path": "shot.png" }), &ctx);
    assert!(!result.is_error, "{}", result.content);
    println!(
        "image: {} attachment(s), {} bytes base64",
        result.images.len(),
        result.images.first().map(|i| i.data.len()).unwrap_or(0)
    );
    report("right after read");
    println!("{:<28} hwm {:>6.1} MB", "", hwm_kb() as f64 / 1024.0);
    drop(result);
    report("after drop");
    std::thread::sleep(std::time::Duration::from_secs(3));
    report("after 3s idle");
    trim();
    report("after malloc_trim");

    let _ = std::fs::remove_dir_all(&dir);
}
