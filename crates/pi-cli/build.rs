//! Embeds the git revision and target triple so `--version` identifies the build.

fn main() {
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|sha| !sha.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=PIPELETS_GIT_SHA={sha}");
    println!("cargo:rustc-env=PIPELETS_TARGET={target}");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}
