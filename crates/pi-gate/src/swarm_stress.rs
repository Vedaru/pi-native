//! Swarm stress — a Rust port of `scripts/swarm_stress.py` (VED-409).
//!
//! Runs N units at once and measures aggregate and per-unit RSS/CPU. A swarm
//! multiplies both memory and CPU, so the interesting number is the aggregate.
//!
//! - `idle`: N `pipelets --rpc` units sitting idle (the swarm floor).
//! - `busy`: N units each running a stress session (load).

use crate::util::{cpu_seconds, rss_bytes, sleep};
use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(about = "Swarm stress: N units at once, aggregate and per-unit RSS")]
pub struct Args {
    #[arg(long, default_value = "target/release/pipelets")]
    binary: PathBuf,
    #[arg(long, default_value_t = 8)]
    units: usize,
    #[arg(long, value_enum, default_value_t = Mode::Idle)]
    mode: Mode,
    #[arg(long, default_value_t = 5000)]
    turns: usize,
    #[arg(long, default_value_t = 2.0)]
    settle: f64,
    #[arg(long, default_value_t = 200.0)]
    max_total_mb: f64,
    #[arg(long, default_value_t = 25.0)]
    max_avg_mb: f64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    Idle,
    Busy,
}

pub fn run(args: Args) -> i32 {
    if !args.binary.exists() {
        println!(
            "NOTICE: {} is not built; swarm stress skipped.",
            args.binary.display()
        );
        return 0;
    }

    let argv: Vec<String> = match args.mode {
        Mode::Idle => vec![args.binary.display().to_string(), "--rpc".to_string()],
        Mode::Busy => vec![
            args.binary.display().to_string(),
            "--stress".to_string(),
            args.turns.to_string(),
            "--stress-tool".to_string(),
            "read".to_string(),
            "--stress-session".to_string(),
        ],
    };

    let mut children = Vec::new();
    for _ in 0..args.units {
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        children.push(command.spawn().expect("spawn swarm unit"));
    }

    let mut peak_rss: Vec<f64> = vec![0.0; children.len()];
    // Highest CPU seconds seen per unit, to confirm a busy swarm is working.
    let mut peak_cpu: Vec<f64> = vec![0.0; children.len()];

    // Idle units settle; busy units start working immediately, so sample from
    // the first moment or we miss their peak.
    if args.mode == Mode::Idle {
        sleep(args.settle);
    }
    let sampling_seconds = match args.mode {
        Mode::Busy => 120.0,
        Mode::Idle => 3.0,
    };
    let deadline = Instant::now() + Duration::from_secs_f64(sampling_seconds);
    while Instant::now() < deadline {
        let mut alive = 0;
        for (index, child) in children.iter_mut().enumerate() {
            if child.try_wait().ok().flatten().is_some() {
                continue;
            }
            if let Some(rss) = rss_bytes(child.id()) {
                peak_rss[index] = peak_rss[index].max(rss as f64 / 1_048_576.0);
            }
            if let Some(cpu) = cpu_seconds(child.id()) {
                peak_cpu[index] = peak_cpu[index].max(cpu);
            }
            alive += 1;
        }
        if args.mode == Mode::Busy && alive == 0 {
            break;
        }
        sleep(0.05);
    }

    // Kill any stragglers (whole group, like the Python harness).
    for child in &mut children {
        kill_group(child);
    }

    let total_peak: f64 = peak_rss.iter().sum();
    let avg_peak = total_peak / children.len().max(1) as f64;
    println!(
        "swarm[{}]: {} units | avg peak {avg_peak:.1} MB | total peak {total_peak:.1} MB",
        args.mode_str(),
        args.units
    );
    if args.mode == Mode::Busy {
        let cpu_total: f64 = peak_cpu.iter().sum();
        println!("swarm[{}]: total unit CPU {cpu_total:.2}s", args.mode_str());
    }

    let mut failed = false;
    if avg_peak > args.max_avg_mb {
        println!("FAIL: average unit over {:.0} MB.", args.max_avg_mb);
        failed = true;
    }
    if total_peak > args.max_total_mb {
        println!("FAIL: total over {:.0} MB.", args.max_total_mb);
        failed = true;
    }

    if failed {
        println!("\nFAIL: swarm gate failed.");
        return 1;
    }
    println!("\nPASS: swarm gate passed.");
    0
}

impl Args {
    fn mode_str(&self) -> &'static str {
        match self.mode {
            Mode::Idle => "idle",
            Mode::Busy => "busy",
        }
    }
}

/// Kill the child's whole process group, then reap it.
fn kill_group(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        unsafe {
            libc_kill(-(child.id() as i32), 9);
        }
        let _ = child.kill();
    }
    let _ = child.wait();
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}
