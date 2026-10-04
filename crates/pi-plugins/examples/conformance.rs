//! Plugin conformance harness.
//!
//! Loads every `.ts`/`.js`/`.tsx` extension under a directory through the
//! native QuickJS host (permissive policy) and reports how many load and
//! register. Scoped to pi's own examples (the minimal core), not a grab bag of
//! third-party extensions.
//!
//! Usage:
//!   cargo run -p pi-plugins --example conformance -- <dir>

use pi_plugins::{PluginHost, PluginPolicy};
use std::path::{Path, PathBuf};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip dependency and build directories.
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "node_modules" || name == "dist" || name == "test" {
                continue;
            }
            collect(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("ts" | "tsx" | "js" | "mjs" | "jsx")
        ) {
            let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if stem.contains(".test.") || stem.contains(".spec.") {
                continue;
            }
            out.push(path);
        }
    }
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: conformance <dir>");
        std::process::exit(2);
    });
    let mut files = Vec::new();
    collect(Path::new(&dir), &mut files);
    files.sort();

    let mut loaded = 0usize;
    let mut failed = 0usize;
    for file in &files {
        let host = PluginHost::new(PluginPolicy::permissive());
        match host.run_file(file) {
            Ok(calls) => {
                loaded += 1;
                println!("OK    {:3} calls  {}", calls.len(), file.display());
            }
            Err(error) => {
                failed += 1;
                println!("FAIL  {error}");
            }
        }
    }

    println!(
        "\n{} loaded, {} failed, {} total",
        loaded,
        failed,
        files.len()
    );
}
