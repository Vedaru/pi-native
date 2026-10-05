//! Build a release binary and pack it into a distributable tarball — a Rust
//! port of `scripts/package.sh`, replacing the Python one-liner that parsed
//! `cargo metadata` for the `pipelets` version (VED-409).
//!
//! ```text
//! pi-gate package                    # host target
//! pi-gate package --target <triple>  # cross/static target (e.g. *-musl)
//! ```
//!
//! Output: `dist/pipelets-<version>-<target>.tar.gz` (+ `.sha256`).

use clap::Parser;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

#[derive(Parser, Debug)]
#[command(about = "Build a release binary and pack a distributable tarball")]
pub struct Args {
    /// Cross/static target triple (e.g. x86_64-unknown-linux-musl).
    #[arg(long)]
    target: Option<String>,
}

pub fn run(args: Args) -> i32 {
    let root = crate::util::project_root();
    if let Err(error) = std::env::set_current_dir(&root) {
        eprintln!("failed to enter project root {}: {error}", root.display());
        return 1;
    }

    let (out_dir, suffix) = match &args.target {
        Some(target) => {
            if !run_cargo(&["build", "--release", "--target", target]) {
                return 1;
            }
            (
                PathBuf::from("target").join(target).join("release"),
                target.clone(),
            )
        }
        None => {
            if !run_cargo(&["build", "--release"]) {
                return 1;
            }
            ("target/release".into(), host_target())
        }
    };

    let version = match package_version() {
        Ok(version) => version,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };

    let name = format!("pipelets-{version}-{suffix}");
    let dist = PathBuf::from("dist");
    let staging = dist.join(&name);
    if let Err(error) = std::fs::remove_dir_all(&staging) {
        if error.kind() != std::io::ErrorKind::NotFound {
            eprintln!("failed to clear {}: {error}", staging.display());
            return 1;
        }
    }
    if let Err(error) = std::fs::create_dir_all(&staging) {
        eprintln!("failed to create {}: {error}", staging.display());
        return 1;
    }

    for (source, dest) in [
        (out_dir.join("pipelets"), staging.join("pipelets")),
        (PathBuf::from("README.md"), staging.join("README.md")),
        (PathBuf::from("LICENSE"), staging.join("LICENSE")),
    ] {
        if let Err(error) = std::fs::copy(&source, &dest) {
            eprintln!(
                "failed to copy {} -> {}: {error}",
                source.display(),
                dest.display()
            );
            return 1;
        }
    }

    let tarball = dist.join(format!("{name}.tar.gz"));
    if !run_command(
        Command::new("tar")
            .arg("-C")
            .arg(&dist)
            .arg("-czf")
            .arg(&tarball)
            .arg(&name),
    ) {
        return 1;
    }

    // `sha256sum <tarball> > <tarball>.sha256`, run inside `dist` so the hash
    // file records the bare file name, exactly like package.sh.
    let checksum_path = dist.join(format!("{name}.tar.gz.sha256"));
    let checksum = match crate::util::sha256_file(&tarball) {
        Some(hash) => hash,
        None => {
            eprintln!("failed to hash {}", tarball.display());
            return 1;
        }
    };
    if let Err(error) = std::fs::write(&checksum_path, format!("{checksum}  {name}.tar.gz\n")) {
        eprintln!("failed to write {}: {error}", checksum_path.display());
        return 1;
    }

    println!("packaged dist/{name}.tar.gz");
    run_command(Command::new(staging.join("pipelets")).arg("--version"));
    0
}

/// Query `cargo metadata` for the `pipelets` package version. Replaces the
/// Python `json.load(sys.stdin)` one-liner.
fn package_version() -> Result<String, String> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .map_err(|error| format!("failed to run cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let metadata: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("cargo metadata is not valid JSON: {error}"))?;
    metadata
        .get("packages")
        .and_then(Value::as_array)
        .and_then(|packages| {
            packages
                .iter()
                .find(|package| package.get("name").and_then(Value::as_str) == Some("pipelets"))
        })
        .and_then(|package| package.get("version").and_then(Value::as_str))
        .map(str::to_string)
        .ok_or_else(|| "cargo metadata has no `pipelets` package".to_string())
}

fn host_target() -> String {
    let output = Command::new("rustc").args(["-vV"]).output();
    if let Ok(output) = output {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(host) = line.strip_prefix("host: ") {
                return host.trim().to_string();
            }
        }
    }
    String::new()
}

fn run_cargo(args: &[&str]) -> bool {
    run_command(Command::new("cargo").args(args))
}

fn run_command(command: &mut Command) -> bool {
    match command.status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!("command failed with {status}: {command:?}");
            false
        }
        Err(error) => {
            eprintln!("failed to run command {command:?}: {error}");
            false
        }
    }
}
