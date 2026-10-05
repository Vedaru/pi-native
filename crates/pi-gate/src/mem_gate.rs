//! Memory regression gate — a Rust port of `scripts/mem_gate.py` (VED-409),
//! including the noise-floor logic and its tests (VED-367).
//!
//! Two independent checks, both meaningful in CI:
//!
//! 1. **Absolute**: each target's RSS vs the committed baseline
//!    (`artifacts/mem_bench.json`), only for `(target, taxonomy)` keys present
//!    in both.
//! 2. **Ratio**: each native target's RSS vs the Node target's RSS for the same
//!    taxonomy. Environment-independent, so it works on a CI runner whose
//!    absolute numbers differ from a developer machine.
//!
//! Exits 0 when no benchmark target is installed (CI without `pi`), but says so
//! loudly instead of passing silently.

use crate::mem_bench;
use crate::util::{human_mb, project_root};
use clap::Parser;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const DEFAULT_THRESHOLD: f64 = 0.10;
pub const DEFAULT_RATIO_THRESHOLD: f64 = 0.60;
/// Below this absolute regression (MB) the relative check is ignored: for a
/// tiny baseline (single-digit MB) a few hundred KB of run-to-run noise exceeds
/// the percentage threshold and would false-fail CI.
pub const DEFAULT_MIN_ABSOLUTE_DELTA_MB: f64 = 2.0;
const NODE_TARGET: &str = "pi-node";
const NATIVE_TARGETS: [&str; 2] = ["pi-rust", "pipelets"];
/// Our build. The ceiling applies only to this; the reference is for comparison.
const OUR_TARGET: &str = "pipelets";

#[derive(Parser, Debug)]
#[command(about = "Memory regression gate (absolute baseline, native/node ratio, ceiling)")]
pub struct Args {
    #[arg(long, default_value_t = DEFAULT_THRESHOLD)]
    threshold: f64,
    #[arg(long, default_value_t = DEFAULT_RATIO_THRESHOLD)]
    ratio_threshold: f64,
    /// Ignore absolute regressions smaller than this many MB (noise floor).
    #[arg(long, default_value_t = DEFAULT_MIN_ABSOLUTE_DELTA_MB)]
    pub min_absolute_delta_mb: f64,
    /// Evaluate this artifact instead of running the benchmark.
    #[arg(long)]
    pub from_json: Option<PathBuf>,
    /// Skip the absolute baseline check (use on runners whose memory profile differs).
    #[arg(long)]
    no_absolute: bool,
    /// Fail if any native target exceeds this many MB (environment-independent ceiling).
    #[arg(long)]
    max_native_mb: Option<f64>,
}

/// A `(target, taxonomy) -> rss_bytes` index, ignoring zero/missing RSS.
pub fn index_results(artifact: Option<&Value>) -> BTreeMap<(String, String), u64> {
    let mut indexed = BTreeMap::new();
    let Some(artifact) = artifact else {
        return indexed;
    };
    let Some(results) = artifact.get("results").and_then(Value::as_array) else {
        return indexed;
    };
    for result in results {
        let target = result
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let taxonomy = result
            .get("taxonomy")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(rss) = result.get("rss_bytes").and_then(Value::as_u64) {
            if rss > 0 {
                indexed.insert((target, taxonomy), rss);
            }
        }
    }
    indexed
}

/// A regression fails only when it is over the relative threshold *and* at
/// least the absolute noise floor. On a tiny baseline a few hundred KB of
/// measurement noise exceeds the percentage threshold and must not fail CI.
pub fn absolute_regression_fails(
    rss: u64,
    baseline_rss: u64,
    threshold: f64,
    min_delta_mb: f64,
) -> bool {
    if baseline_rss == 0 {
        return false;
    }
    let delta = (rss as f64 - baseline_rss as f64) / baseline_rss as f64;
    if delta <= threshold {
        return false;
    }
    (rss as f64 - baseline_rss as f64) / 1_048_576.0 >= min_delta_mb
}

fn load_json(path: &std::path::Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn run(args: Args) -> i32 {
    let root = project_root();
    let baseline =
        index_results(load_json(&root.join("artifacts").join("mem_bench.json")).as_ref());

    let current_artifact: Value = if let Some(path) = &args.from_json {
        // A missing or malformed artifact is a hard error: silently treating it
        // as "no targets" would let CI pass when the file was never produced.
        if !path.exists() {
            println!("FAIL: --from-json file not found: {}", path.display());
            return 1;
        }
        match load_json(path) {
            Some(value) => value,
            None => {
                println!(
                    "FAIL: --from-json file is not valid JSON: {}",
                    path.display()
                );
                return 1;
            }
        }
    } else {
        let current_path = root.join("artifacts").join("mem_bench.current.json");
        if !run_benchmark(&current_path) {
            println!("FAIL: memory benchmark did not produce a usable artifact.");
            return 1;
        }
        match load_json(&current_path) {
            Some(value) => value,
            None => {
                println!("FAIL: memory benchmark did not produce a usable artifact.");
                return 1;
            }
        }
    };

    let current = index_results(Some(&current_artifact));
    if current.is_empty() {
        println!(
            "NOTICE: no benchmark targets available in this environment; the memory gate \
             did not run. Install `pi` (and optionally the reference native binary) to enable it."
        );
        return 0;
    }

    let mut failed = false;

    // 1. Absolute regression against the committed baseline.
    println!("Absolute (vs committed baseline):");
    let mut overlaps = 0;
    if args.no_absolute {
        println!("  (skipped by --no-absolute)");
    } else {
        for ((target, taxonomy), rss) in &current {
            let Some(base_rss) = baseline.get(&(target.clone(), taxonomy.clone())) else {
                continue;
            };
            overlaps += 1;
            let base_rss = *base_rss;
            let rss = *rss;
            let delta = (rss as f64 - base_rss as f64) / base_rss as f64;
            let abs_delta_mb = (rss as f64 - base_rss as f64) / 1_048_576.0;
            println!(
                "  {target} [{taxonomy}]: {} -> {} ({:+.1}%, {:+.2} MB)",
                human_mb(base_rss),
                human_mb(rss),
                delta * 100.0,
                abs_delta_mb
            );
            if absolute_regression_fails(rss, base_rss, args.threshold, args.min_absolute_delta_mb)
            {
                println!("    FAIL: over {:.0}%", args.threshold * 100.0);
                failed = true;
            } else if delta > args.threshold {
                println!(
                    "    ok: over {:.0}% but only {abs_delta_mb:+.2} MB (< {:.1} MB noise floor)",
                    args.threshold * 100.0,
                    args.min_absolute_delta_mb
                );
            }
        }
        if overlaps == 0 {
            println!("  (no comparable baseline keys in this environment; skipped)");
        }
    }

    // 2. Ratio of native targets to the Node target, same taxonomy.
    println!(
        "Ratio (native <= {:.0}% of {NODE_TARGET}):",
        args.ratio_threshold * 100.0
    );
    let mut ratios = 0;
    for ((target, taxonomy), rss) in &current {
        if !NATIVE_TARGETS.contains(&target.as_str()) {
            continue;
        }
        let Some(node_rss) = current.get(&(NODE_TARGET.to_string(), taxonomy.clone())) else {
            continue;
        };
        if *node_rss == 0 {
            continue;
        }
        ratios += 1;
        let ratio = *rss as f64 / *node_rss as f64;
        println!(
            "  {target}/{NODE_TARGET} [{taxonomy}]: {ratio:.2} ({}/{})",
            human_mb(*rss),
            human_mb(*node_rss)
        );
        if ratio > args.ratio_threshold {
            println!("    FAIL: over {:.0}%", args.ratio_threshold * 100.0);
            failed = true;
        }
    }
    if ratios == 0 {
        println!("  (no native target alongside {NODE_TARGET} in this environment; skipped)");
    }

    // 3. Environment-independent absolute ceiling for native targets.
    if let Some(max_native_mb) = args.max_native_mb {
        println!("Ceiling (native <= {max_native_mb:.0} MB):");
        let mut ceilings = 0;
        let ceiling_bytes = (max_native_mb * 1_048_576.0) as u64;
        for ((target, taxonomy), rss) in &current {
            if target != OUR_TARGET {
                continue;
            }
            ceilings += 1;
            println!("  {target} [{taxonomy}]: {}", human_mb(*rss));
            if *rss > ceiling_bytes {
                println!("    FAIL: over {max_native_mb:.0} MB");
                failed = true;
            }
        }
        if ceilings == 0 {
            println!("  (no native target in this environment; skipped)");
        }
    }

    if failed {
        println!("\nFAIL: memory gate failed.");
        return 1;
    }
    println!("\nPASS: memory gate passed.");
    0
}

/// Run `mem-bench --all --json <path>` in-process.
fn run_benchmark(current_path: &std::path::Path) -> bool {
    mem_bench::run_all_to_json(current_path) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MB: u64 = 1024 * 1024;

    #[test]
    fn tiny_relative_regression_below_floor_passes() {
        // Reproduces the observed false failure: 3.9 MB -> 4.4 MB is ~+13%, but
        // only +0.5 MB of noise, so it must not fail (VED-367).
        assert!(!absolute_regression_fails(4_400_000, 4_090_000, 0.10, 2.0));
    }

    #[test]
    fn regression_at_or_above_floor_fails() {
        // 3.9 MB -> 6.5 MB is +~66% and +2.6 MB: a real regression.
        assert!(absolute_regression_fails(
            6 * MB + MB / 2,
            3 * MB + MB * 9 / 10,
            0.10,
            2.0
        ));
    }

    #[test]
    fn relative_regression_under_threshold_passes() {
        // +5% is under the 10% threshold even with a large absolute delta.
        assert!(!absolute_regression_fails(105 * MB, 100 * MB, 0.10, 2.0));
    }

    #[test]
    fn shrink_is_never_a_regression() {
        assert!(!absolute_regression_fails(3 * MB, 4 * MB, 0.10, 2.0));
    }

    #[test]
    fn floor_of_zero_matches_old_relative_only_behavior() {
        // With no floor, any relative regression fails (back-compat escape).
        // 4.09 MB -> 4.60 MB is +~12.5% and only +0.5 MB.
        assert!(absolute_regression_fails(4_600_000, 4_090_000, 0.10, 0.0));
    }

    #[test]
    fn default_floor_is_two_mb() {
        assert_eq!(DEFAULT_MIN_ABSOLUTE_DELTA_MB, 2.0);
    }

    #[test]
    fn zero_and_missing_rss_are_ignored() {
        let artifact = json!({
            "results": [
                {"target": "pipelets", "taxonomy": "cold-idle", "rss_bytes": 0},
                {"target": "pi-node", "taxonomy": "cold-idle"},
                {"target": "pipelets", "taxonomy": "warm-idle", "rss_bytes": 123}
            ]
        });
        let indexed = index_results(Some(&artifact));
        let mut expected = BTreeMap::new();
        expected.insert(("pipelets".to_string(), "warm-idle".to_string()), 123);
        assert_eq!(indexed, expected);
    }
}
