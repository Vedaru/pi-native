//! `pi-gate` — the CI/dev measurement gates, ported from the former
//! `scripts/*.py` harnesses (VED-409).
//!
//! One subcommand per gate, so a broken gate fails with the same `cargo`
//! ergonomics as the rest of the workspace and the repo stays zero-Python:
//!
//! ```text
//! pi-gate mem-bench     measure idle RSS of pi-node / pi-rust / pipelets
//! pi-gate mem-gate      fail on an RSS regression or an absolute ceiling
//! pi-gate stress-gate   fail if `pipelets --stress` blows peak RSS or CPU
//! pi-gate swarm-stress  run N units at once and measure aggregate RSS
//! pi-gate headless      fail if the worker links a TUI/clipboard/GUI crate
//! pi-gate package       build + pack a release tarball (was package.sh)
//! ```
//!
//! The memory primitives are Linux-native (`/proc/<pid>/status` for RSS,
//! `/proc/<pid>/stat` for CPU, `getrusage` for a child's high-water mark), with
//! a `ps` fallback for the idle benchmark on macOS/BSD.

use clap::{Parser, Subcommand};

mod headless_gate;
mod mem_bench;
mod mem_gate;
mod package;
mod stress_gate;
mod swarm_stress;
mod util;

#[derive(Parser)]
#[command(
    name = "pi-gate",
    version,
    about = "CI/dev measurement gates for pipelets (memory, stress, swarm, headless, package)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Canonical runtime-memory benchmark (was scripts/mem_bench.py).
    #[command(name = "mem-bench")]
    MemBench(mem_bench::Args),
    /// Memory regression gate (was scripts/mem_gate.py).
    #[command(name = "mem-gate")]
    MemGate(mem_gate::Args),
    /// Pressure gate for the native harness (was scripts/stress_gate.py).
    #[command(name = "stress-gate")]
    StressGate(stress_gate::Args),
    /// Swarm stress: N units at once (was scripts/swarm_stress.py).
    #[command(name = "swarm-stress")]
    SwarmStress(swarm_stress::Args),
    /// Headless-worker dependency gate (was scripts/headless_gate.py).
    Headless(headless_gate::Args),
    /// Build a release binary and pack a tarball (was scripts/package.sh).
    Package(package::Args),
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::MemBench(args) => mem_bench::run(args),
        Command::MemGate(args) => mem_gate::run(args),
        Command::StressGate(args) => stress_gate::run(args),
        Command::SwarmStress(args) => swarm_stress::run(args),
        Command::Headless(args) => headless_gate::run(args),
        Command::Package(args) => package::run(args),
    };
    std::process::ExitCode::from(code as u8)
}
