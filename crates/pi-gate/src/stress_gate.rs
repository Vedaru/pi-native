//! Pressure gate for the native harness — a Rust port of
//! `scripts/stress_gate.py` (VED-328, VED-409).
//!
//! Runs `pipelets --stress N` (a deterministic in-process workload) and fails
//! if peak RSS exceeds a ceiling or the run does not finish in time. Unlike the
//! idle benchmark, this measures the harness under load.
//!
//! `ru_maxrss` of the child process is the high-water mark, so it is not
//! sampling dependent.

use crate::util::sleep;
use clap::Parser;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(about = "Pressure gate for the native harness (peak RSS / CPU under load)")]
pub struct Args {
    #[arg(long, default_value = "target/release/pipelets")]
    binary: PathBuf,
    #[arg(long, default_value_t = 50000)]
    turns: usize,
    /// One or more tools to exercise.
    #[arg(long, num_args = 1.., default_values_t = vec!["ls".to_string()])]
    tools: Vec<String>,
    /// Run the agent session loop (compaction) instead of direct tool calls.
    #[arg(long)]
    session: bool,
    /// `--stress-context-tokens` for `--session` (large = no compaction, so
    /// retained results grow).
    #[arg(long)]
    context_tokens: Option<usize>,
    #[arg(long, default_value_t = 150.0)]
    max_mb: f64,
    #[arg(long, default_value_t = 60.0)]
    timeout: f64,
    /// Max CPU seconds per tool run.
    #[arg(long, default_value_t = 5.0)]
    max_cpu: f64,
    /// Max CPU seconds while idle.
    #[arg(long, default_value_t = 0.2)]
    max_idle_cpu: f64,
    #[arg(long)]
    skip_idle: bool,
}

/// Child CPU usage deltas from `getrusage(RUSAGE_CHILDREN)`.
#[derive(Clone, Copy, Debug, Default)]
struct ChildUsage {
    /// Child high-water RSS in KB (Linux: `ru_maxrss` is KB; macOS: bytes).
    max_rss_kb: i64,
    user_time: f64,
    sys_time: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TimeVal {
    tv_sec: i64,
    tv_usec: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RUsage {
    ru_utime: TimeVal,
    ru_stime: TimeVal,
    ru_maxrss: i64,
    _rest: [i64; 14],
}

extern "C" {
    fn getrusage(who: i32, usage: *mut RUsage) -> i32;
}

const RUSAGE_CHILDREN: i32 = -1;

fn child_usage() -> ChildUsage {
    let mut usage = RUsage {
        ru_utime: TimeVal {
            tv_sec: 0,
            tv_usec: 0,
        },
        ru_stime: TimeVal {
            tv_sec: 0,
            tv_usec: 0,
        },
        ru_maxrss: 0,
        _rest: [0; 14],
    };
    let ok = unsafe { getrusage(RUSAGE_CHILDREN, &mut usage) } == 0;
    if !ok {
        return ChildUsage::default();
    }
    let seconds = |value: TimeVal| value.tv_sec as f64 + value.tv_usec as f64 / 1_000_000.0;
    ChildUsage {
        max_rss_kb: usage.ru_maxrss,
        user_time: seconds(usage.ru_utime),
        sys_time: seconds(usage.ru_stime),
    }
}

pub fn run(args: Args) -> i32 {
    if !args.binary.exists() {
        println!(
            "NOTICE: {} is not built; stress gate skipped.",
            args.binary.display()
        );
        return 0;
    }

    let mut failed = false;

    for tool in &args.tools {
        let mut command = Command::new(&args.binary);
        command
            .arg("--stress")
            .arg(args.turns.to_string())
            .arg("--stress-tool")
            .arg(tool);
        if args.session {
            command.arg("--stress-session");
        }
        if let Some(context_tokens) = args.context_tokens {
            command
                .arg("--stress-context-tokens")
                .arg(context_tokens.to_string());
        }

        let before = child_usage();
        // Capture to temp files (not pipes): the polling loop below never reads
        // the pipes, so a full pipe buffer would deadlock the child. The output
        // is a few lines, but correctness should not depend on that.
        let stdout_path =
            std::env::temp_dir().join(format!("pi-gate-stress-{}-{tool}.out", std::process::id()));
        let stderr_path = stdout_path.with_extension("err");
        let stdout_file = match std::fs::File::create(&stdout_path) {
            Ok(file) => file,
            Err(error) => {
                println!("FAIL[{tool}]: cannot create output file: {error}");
                failed = true;
                continue;
            }
        };
        let stderr_file = match std::fs::File::create(&stderr_path) {
            Ok(file) => file,
            Err(error) => {
                println!("FAIL[{tool}]: cannot create error file: {error}");
                failed = true;
                continue;
            }
        };
        let mut child = match command
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file))
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                println!("FAIL[{tool}]: failed to spawn: {error}");
                failed = true;
                continue;
            }
        };

        // Enforce the timeout without a blocking wait.
        let deadline = std::time::Instant::now() + Duration::from_secs_f64(args.timeout);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break None;
                    }
                    sleep(0.01);
                }
                Err(error) => {
                    println!("FAIL[{tool}]: failed to wait: {error}");
                    failed = true;
                    break None;
                }
            }
        };
        let after = child_usage();
        let cpu = (after.user_time - before.user_time) + (after.sys_time - before.sys_time);

        let Some(status) = status else {
            println!(
                "FAIL[{tool}]: did not finish within {:.0}s ({} turns).",
                args.timeout, args.turns
            );
            failed = true;
            let _ = std::fs::remove_file(&stdout_path);
            let _ = std::fs::remove_file(&stderr_path);
            continue;
        };

        if !status.success() {
            println!("FAIL[{tool}]: exited non-zero.");
            if let Ok(stderr) = std::fs::read_to_string(&stderr_path) {
                eprint!("{stderr}");
            }
            failed = true;
            let _ = std::fs::remove_file(&stdout_path);
            let _ = std::fs::remove_file(&stderr_path);
            continue;
        }

        let stdout = std::fs::read_to_string(&stdout_path)
            .unwrap_or_default()
            .trim()
            .to_string();
        let _ = std::fs::remove_file(&stdout_path);
        let _ = std::fs::remove_file(&stderr_path);
        let mut peak_mb: Option<f64> = None;
        for text in stdout.lines() {
            if !text.starts_with("peak RSS:") {
                continue;
            }
            // Parse defensively: a malformed line should fail only this tool,
            // not abort the whole gate.
            match parse_peak(text) {
                Some(value) => peak_mb = Some(value),
                None => println!("WARN[{tool}]: malformed 'peak RSS:' line: {text:?}"),
            }
        }
        if peak_mb.is_none() {
            // RUSAGE_CHILDREN.ru_maxrss is a monotonic high-water mark across
            // ALL previously reaped children, so using it directly lets a large
            // earlier tool mask a later regression. Use this child's own delta
            // instead: its contribution to the high-water mark.
            println!("WARN[{tool}]: no 'peak RSS:' reported; using per-child RSS delta.");
            peak_mb = Some((after.max_rss_kb - before.max_rss_kb).max(0) as f64 / 1024.0);
        }
        let peak_mb = peak_mb.unwrap_or(0.0);
        let head = stdout.lines().next().unwrap_or("");
        println!("{tool}: {head} | peak {peak_mb:.1} MB | cpu {cpu:.2}s");

        if cpu > args.max_cpu {
            println!("FAIL[{tool}]: CPU {cpu:.2}s over {:.1}s.", args.max_cpu);
            failed = true;
        }
        if peak_mb > args.max_mb {
            println!("FAIL[{tool}]: peak RSS over {:.0} MB.", args.max_mb);
            failed = true;
        }
    }

    // Idle CPU must be ~0: no busy-wait or polling.
    if !args.skip_idle {
        let before = child_usage();
        let mut child = Command::new(&args.binary)
            .arg("--rpc")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn idle child");
        sleep(3.0);
        let _ = child.kill();
        let _ = child.wait();
        let after = child_usage();
        let idle_cpu = (after.user_time - before.user_time) + (after.sys_time - before.sys_time);
        println!(
            "idle cpu over 3s: {idle_cpu:.3}s (ceiling {:.2}s)",
            args.max_idle_cpu
        );
        if idle_cpu > args.max_idle_cpu {
            println!("FAIL: idle CPU too high (busy-wait?).");
            failed = true;
        }
    }

    if failed {
        println!("\nFAIL: stress gate failed.");
        return 1;
    }
    println!("\nPASS: stress gate passed.");
    0
}

/// Parse `peak RSS: 12.3 MB` into `12.3`. Mirrors the Python regex
/// `peak RSS:\s*([0-9]*\.?[0-9]+)`.
pub fn parse_peak(line: &str) -> Option<f64> {
    let rest = line.strip_prefix("peak RSS:")?.trim_start();
    let number: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    if number.is_empty() || number == "." {
        return None;
    }
    number.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_peak_lines() {
        assert_eq!(parse_peak("peak RSS: 12.3 MB"), Some(12.3));
        assert_eq!(parse_peak("peak RSS:5 MB"), Some(5.0));
        assert_eq!(parse_peak("peak RSS: .5 MB"), Some(0.5));
        assert_eq!(parse_peak("peak RSS: n/a"), None);
    }
}
