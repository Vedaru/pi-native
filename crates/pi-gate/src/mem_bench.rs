//! Canonical runtime-memory benchmark — a Rust port of `scripts/mem_bench.py`
//! (VED-302, VED-409).
//!
//! Measures resident set size (RSS) of a pi runtime after it reaches a steady
//! idle state:
//!
//! - `pi-node`  : the Node/V8 implementation
//! - `pi-rust`  : the third-party reference native port
//! - `pipelets` : our build
//!
//! Only `cold-idle` is implemented; the remaining taxonomies are declared so
//! the artifact schema does not churn when they land.

use crate::util::{human_mb_opt, median, now_rfc3339, project_root, rss_bytes, sha256_file, sleep};
use clap::Parser;
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const SCHEMA: &str = "pi.native.mem_bench.v1";
const DEFAULT_SETTLE_SECONDS: f64 = 5.0;
const DEFAULT_SAMPLE_INTERVAL_SECONDS: f64 = 1.0;
const DEFAULT_SAMPLE_COUNT: usize = 5;
const IDLE_TAXONOMY: &str = "cold-idle";
const TAXONOMIES: [&str; 5] = [
    "cold-idle",
    "warm-idle",
    "post-conversation",
    "post-compaction",
    "post-tool-heavy",
];

#[derive(Parser, Debug)]
#[command(about = "Canonical runtime-memory benchmark (idle RSS)")]
pub struct Args {
    /// Target name (repeatable).
    #[arg(long, action = clap::ArgAction::Append)]
    target: Vec<String>,
    /// Run every detected target.
    #[arg(long)]
    all: bool,
    #[arg(long, default_value_t = DEFAULT_SETTLE_SECONDS)]
    settle: f64,
    #[arg(long, default_value_t = DEFAULT_SAMPLE_INTERVAL_SECONDS)]
    interval: f64,
    #[arg(long, default_value_t = DEFAULT_SAMPLE_COUNT)]
    samples: usize,
    /// Write the artifact to this path.
    #[arg(long)]
    json: Option<PathBuf>,
    /// List detected targets and exit.
    #[arg(long)]
    list: bool,
    /// Load this session JSONL in every target (taxonomy: session-loaded).
    #[arg(long)]
    session: Option<String>,
    /// Override the idle-taxonomy label.
    #[arg(long)]
    taxonomy: Option<String>,
}

/// A benchmark target: how to launch it and what kind it is.
#[derive(Debug, Clone)]
pub struct TargetSpec {
    pub kind: &'static str,
    pub argv: Vec<String>,
    pub env: Vec<(&'static str, String)>,
}

/// Build the target table, honoring env overrides for binary locations.
///
/// `pi-rust` is the THIRD-PARTY reference port, not this project; our build is
/// the `pipelets` target (`PIPELETS_BIN`).
pub fn resolve_targets() -> Vec<(String, TargetSpec)> {
    let mut targets = Vec::new();

    if let Some(node_pi) = which("pi") {
        targets.push((
            "pi-node".to_string(),
            TargetSpec {
                kind: "node",
                argv: vec![
                    node_pi,
                    "--mode".to_string(),
                    "rpc".to_string(),
                    "--no-session".to_string(),
                ],
                env: Vec::new(),
            },
        ));
    }

    let rust_pi = std::env::var("PI_RUST_BIN").unwrap_or_else(|_| {
        std::env::temp_dir()
            .join("pi-rust")
            .join("pi")
            .display()
            .to_string()
    });
    if Path::new(&rust_pi).exists() {
        targets.push((
            "pi-rust".to_string(),
            TargetSpec {
                kind: "native",
                argv: vec![
                    rust_pi,
                    "--rpc".to_string(),
                    "--no-session".to_string(),
                    "--provider".to_string(),
                    std::env::var("PI_RUST_PROVIDER").unwrap_or_else(|_| "anthropic".to_string()),
                    "--model".to_string(),
                    std::env::var("PI_RUST_MODEL")
                        .unwrap_or_else(|_| "claude-sonnet-4-5".to_string()),
                ],
                // A dummy key lets the runtime boot without network; no request
                // is made during an idle benchmark.
                env: vec![("ANTHROPIC_API_KEY", "sk-ant-mem-bench-dummy".to_string())],
            },
        ));
    }

    if let Ok(native_pi) = std::env::var("PIPELETS_BIN") {
        if Path::new(&native_pi).exists() {
            targets.push((
                "pipelets".to_string(),
                TargetSpec {
                    kind: "native",
                    argv: vec![native_pi, "--rpc".to_string(), "--no-session".to_string()],
                    env: Vec::new(),
                },
            ));
        }
    }

    targets
}

/// `shutil.which`, using `PATH` and the executable bit.
fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate.display().to_string());
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

/// One measurement result, serialized into the artifact.
#[derive(Debug, Serialize, Default)]
pub struct Result_ {
    pub target: String,
    pub kind: String,
    pub command: String,
    pub binary: String,
    pub binary_sha256: Option<String>,
    pub taxonomy: String,
    pub settle_seconds: f64,
    pub sample_interval_seconds: f64,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub samples_bytes: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_min_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_max_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_spread_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
struct Host {
    platform: String,
    machine: String,
    /// The toolchain that produced the sample (was `python` in the Python
    /// harness). Kept generic so the artifact schema does not churn again.
    toolchain: String,
}

#[derive(Debug, Serialize)]
struct Artifact {
    schema: &'static str,
    generated_at: String,
    host: Host,
    idle_taxonomy: String,
    taxonomies_declared: [&'static str; 5],
    results: Vec<Result_>,
}

/// Measure one target after it settles, sampling RSS `count` times.
pub fn measure_target(
    name: &str,
    spec: &TargetSpec,
    settle: f64,
    interval: f64,
    count: usize,
    extra_args: &[String],
    taxonomy: &str,
) -> Result_ {
    let mut argv = spec.argv.clone();
    argv.extend_from_slice(extra_args);
    if extra_args.iter().any(|arg| arg == "--session") {
        // `--no-session` and `--session` are mutually exclusive.
        argv.retain(|arg| arg != "--no-session");
    }

    let mut result = Result_ {
        target: name.to_string(),
        kind: spec.kind.to_string(),
        command: argv.join(" "),
        binary: argv.first().cloned().unwrap_or_default(),
        binary_sha256: sha256_file(Path::new(&argv[0])),
        taxonomy: taxonomy.to_string(),
        settle_seconds: settle,
        sample_interval_seconds: interval,
        ..Default::default()
    };

    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    command
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env("NO_COLOR", "1");
    // `start_new_session=True`: a new process group, so a kill can reach the
    // whole tree (Node spawns helpers).
    use std::os::unix::process::CommandExt;
    command.process_group(0);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            result.error = Some(format!("failed to spawn: {error}"));
            return result;
        }
    };

    let outcome = sample(&mut child, settle, interval, count);
    kill_group(&mut child);
    match outcome {
        Ok(samples) => {
            if samples.len() < (count / 2).max(1) {
                result.error = Some(format!("insufficient samples ({}/{count})", samples.len()));
                result.samples_bytes = samples;
                return result;
            }
            result.sample_count = Some(samples.len());
            result.rss_bytes = Some(median(&samples));
            result.rss_min_bytes = samples.iter().copied().min();
            result.rss_max_bytes = samples.iter().copied().max();
            result.rss_spread_bytes =
                Some(result.rss_max_bytes.unwrap_or(0) - result.rss_min_bytes.unwrap_or(0));
            result.samples_bytes = samples;
        }
        Err(error) => result.error = Some(error),
    }
    result
}

/// Settle window, then sample RSS. Returns an error string if the process
/// exits early, mirroring the Python error messages.
fn sample(child: &mut Child, settle: f64, interval: f64, count: usize) -> Result<Vec<u64>, String> {
    let deadline = Instant::now() + Duration::from_secs_f64(settle.max(0.0));
    while Instant::now() < deadline {
        if let Some(code) = child.try_wait().ok().flatten() {
            return Err(format!(
                "process exited early with code {}",
                code.code().unwrap_or(-1)
            ));
        }
        sleep(0.25);
    }

    let mut samples = Vec::new();
    for _ in 0..count {
        if let Some(code) = child.try_wait().ok().flatten() {
            return Err(format!(
                "process exited during sampling with code {}",
                code.code().unwrap_or(-1)
            ));
        }
        if let Some(value) = rss_bytes(child.id()) {
            samples.push(value);
        }
        sleep(interval);
    }
    Ok(samples)
}

/// Kill the child's whole process group, then reap it.
fn kill_group(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        // Negative pid targets the group created by `process_group(0)`.
        unsafe {
            libc_kill(-(child.id() as i32), 9);
        }
        let _ = child.kill();
    }
    for _ in 0..50 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        sleep(0.1);
    }
    let _ = child.kill();
    let _ = child.wait();
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

fn rust_version() -> String {
    // Prefer the running toolchain, fall back to a plain label.
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Run `mem-bench --all --json <path>` in-process. Used by the memory gate so
/// the two stay one binary with no cross-process handoff.
pub fn run_all_to_json(path: &std::path::Path) -> i32 {
    run(Args {
        target: Vec::new(),
        all: true,
        settle: DEFAULT_SETTLE_SECONDS,
        interval: DEFAULT_SAMPLE_INTERVAL_SECONDS,
        samples: DEFAULT_SAMPLE_COUNT,
        json: Some(path.to_path_buf()),
        list: false,
        session: None,
        taxonomy: None,
    })
}

pub fn run(args: Args) -> i32 {
    let targets = resolve_targets();
    if args.list {
        for (name, spec) in &targets {
            println!("{name:12} {}", spec.argv[0]);
        }
        return 0;
    }

    let selected: Vec<String> = if args.all {
        targets.iter().map(|(name, _)| name.clone()).collect()
    } else if !args.target.is_empty() {
        args.target.clone()
    } else {
        eprintln!("No target selected. Use --all, --target <name>, or --list.");
        return 1;
    };

    let missing: Vec<&String> = selected
        .iter()
        .filter(|name| !targets.iter().any(|(n, _)| n == *name))
        .collect();
    if !missing.is_empty() {
        let names = missing
            .iter()
            .map(|name| name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("Unknown/unavailable targets: {names}");
        let detected = targets
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "Detected: {}",
            if detected.is_empty() {
                "(none)"
            } else {
                &detected
            }
        );
        return 1;
    }

    let extra_args: Vec<String> = match &args.session {
        Some(session) => vec!["--session".to_string(), session.clone()],
        None => Vec::new(),
    };
    let taxonomy = args.taxonomy.clone().unwrap_or_else(|| {
        if args.session.is_some() {
            "session-loaded"
        } else {
            IDLE_TAXONOMY
        }
        .to_string()
    });

    let mut results = Vec::new();
    for name in &selected {
        let (_, spec) = targets
            .iter()
            .find(|(n, _)| n == name)
            .expect("selected target is present");
        println!("Measuring {name} ...");
        let _ = std::io::stdout().flush();
        let result = measure_target(
            name,
            spec,
            args.settle,
            args.interval,
            args.samples,
            &extra_args,
            &taxonomy,
        );
        if let Some(error) = &result.error {
            println!(
                "  {name}: FAILED - {error} ({} samples)",
                result.samples_bytes.len()
            );
        } else {
            println!(
                "  {name}: {} (min {}, max {}, spread {} KiB, n={})",
                human_mb_opt(result.rss_bytes),
                human_mb_opt(result.rss_min_bytes),
                human_mb_opt(result.rss_max_bytes),
                result.rss_spread_bytes.unwrap_or(0) / 1024,
                result.sample_count.unwrap_or(0),
            );
        }
        results.push(result);
    }

    let artifact = Artifact {
        schema: SCHEMA,
        generated_at: now_rfc3339(),
        host: Host {
            platform: platform(),
            machine: machine(),
            toolchain: rust_version(),
        },
        idle_taxonomy: taxonomy,
        taxonomies_declared: TAXONOMIES,
        results,
    };

    let out_path = args
        .json
        .clone()
        .unwrap_or_else(|| project_root().join("artifacts").join("mem_bench.last.json"));
    if let Some(parent) = out_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(&artifact) {
        Ok(text) => {
            if let Err(error) = std::fs::write(&out_path, format!("{text}\n")) {
                eprintln!("failed to write artifact: {error}");
                return 1;
            }
            println!("\nArtifact: {}", out_path.display());
        }
        Err(error) => {
            eprintln!("failed to serialize artifact: {error}");
            return 1;
        }
    }

    if artifact.results.iter().any(|result| result.error.is_some()) {
        2
    } else {
        0
    }
}

fn platform() -> String {
    // Approximate Python's `platform.platform()`: `Linux-<release>-<machine>`
    // when the kernel release is readable, else the portable `os-arch` form.
    let machine = machine();
    if let Ok(release) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        let release = release.trim();
        if !release.is_empty() {
            let os = if cfg!(target_os = "macos") {
                "Darwin"
            } else {
                "Linux"
            };
            return format!("{os}-{release}-{machine}");
        }
    }
    format!("{}-{}", std::env::consts::OS, machine)
}

fn machine() -> String {
    std::env::consts::ARCH.to_string()
}
