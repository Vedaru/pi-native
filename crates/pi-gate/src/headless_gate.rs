//! Headless-worker dependency gate — a Rust port of
//! `scripts/headless_gate.py` (VED-409).
//!
//! A swarm unit draws no terminal and needs no clipboard or GUI. It may decode
//! images: the `read` tool attaches them to the model, which is agent input, not
//! rendering.
//!
//! Names are matched exactly, not as substrings: `webpki-roots` is a TLS
//! dependency, not the `webp` image codec, and this gate must not confuse them.

use clap::Parser;
use std::collections::BTreeSet;
use std::process::Command;

/// Crates whose only reason to exist is interactive or graphical output. A
/// headless worker links none of them.
pub const FORBIDDEN: [&str; 29] = [
    // Terminal / TUI renderers
    "crossterm",
    "ratatui",
    "termion",
    "termwiz",
    "tui",
    "tuirealm",
    "notcurses",
    "ncurses",
    "pancurses",
    "cursive",
    "tui-input",
    "pi-tui",
    // Clipboards
    "arboard",
    "clipboard",
    "copypasta",
    "cli-clipboard",
    "x11-clipboard",
    // Terminal image renderers (display-only; image *decoding* is allowed,
    // because `read` attaches images to the model)
    "viuer",
    "sixel",
    "kitty",
    // GUI toolkits / GPU
    "egui",
    "eframe",
    "iced",
    "slint",
    "gtk",
    "gtk4",
    "wgpu",
    "winit",
    "sdl2",
];

#[derive(Parser, Debug)]
#[command(about = "Headless-worker dependency gate (no TUI/clipboard/GUI crates)")]
pub struct Args {
    /// Package to check.
    #[arg(short, long, default_value = "pipelets")]
    package: String,
}

/// Parse `cargo tree --prefix none` output into the set of crate names.
///
/// `cargo tree --prefix none` prints one crate per line: `name vX.Y.Z`,
/// optionally followed by `(*)` (already shown) or `(proc-macro)`.
pub fn dependency_names(tree_output: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for line in tree_output.lines() {
        let line = line.trim();
        // Name = leading `[A-Za-z0-9_-]` run.
        let name_end = line
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
            .unwrap_or(line.len());
        if name_end == 0 {
            continue;
        }
        let name = &line[..name_end];
        // Must be followed by whitespace and `v<digit>`.
        let rest = line[name_end..].trim_start();
        let Some(version) = rest.strip_prefix('v') else {
            continue;
        };
        if !version.starts_with(|ch: char| ch.is_ascii_digit()) {
            continue;
        }
        names.insert(name.to_string());
    }
    names
}

fn tree_for(package: &str) -> Result<String, String> {
    let output = Command::new("cargo")
        .args(["tree", "-p", package, "-e", "normal", "--prefix", "none"])
        .output()
        .map_err(|error| format!("failed to run cargo tree: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn run(args: Args) -> i32 {
    let tree = match tree_for(&args.package) {
        Ok(tree) => tree,
        Err(error) => {
            eprint!("{error}");
            eprintln!("cargo tree failed for package {:?}", args.package);
            return 1;
        }
    };
    let names = dependency_names(&tree);
    let found: Vec<&str> = FORBIDDEN
        .iter()
        .copied()
        .filter(|name| names.contains(*name))
        .collect();
    if !found.is_empty() {
        eprintln!(
            "✗ {} depends on UI-only crates: {}",
            args.package,
            found.join(", ")
        );
        eprintln!(
            "  A headless swarm worker must not link a TUI, clipboard, or GUI. \
             Image decoding is allowed (agent input); rendering is not. Move a \
             display dependency to a UI crate that depends on core, not into the worker."
        );
        return 1;
    }

    println!(
        "✓ {} is headless ({} crates, no UI-only deps)",
        args.package,
        names.len()
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_and_ignores_annotations() {
        // `cargo tree --prefix none` prints bare `name vX.Y.Z` lines.
        let tree = "\
pipelets v0.0.1 (/path/to/crates/pi-cli)
serde v1.0.0
pi-tui v0.0.1
syn v2.0.0 (proc-macro)
webpki-roots v0.26.0
";
        let names = dependency_names(tree);
        assert!(names.contains("pipelets"));
        assert!(names.contains("serde"));
        assert!(names.contains("pi-tui"));
        assert!(names.contains("syn"));
        assert!(names.contains("webpki-roots"));
        // Exact match: `webpki-roots` must not read as the `webp` codec.
        assert!(!names.contains("webp"));
        // A bare path or version-less line is not a crate.
        assert!(dependency_names("/usr/lib/libfoo.so\n").is_empty());
    }
}
