//! `isha-vector-db-bench` — measures the engine and emits results a machine can diff.
//!
//! Rule 15 of the engineering rules is "benchmark before making performance claims". This is
//! the thing that makes that rule followable: results go to `benchmarks/results/` as JSON, so a
//! regression shows up as a diff in git history rather than as a vague sense that things got
//! slower.
//!
//! # It refuses to report debug numbers
//!
//! A debug build of this engine is roughly an order of magnitude slower than a release build,
//! and the ratio is not uniform across workloads. Numbers from one are not merely imprecise,
//! they are misleading — and a number that lands in a README is very hard to un-publish. So a
//! debug build prints results with a warning and refuses to write JSON at all.

#![allow(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

mod harness;
mod json;
mod workloads;

use std::process::ExitCode;
use std::time::Duration;

use harness::Measurement;
use workloads::Scale;

const USAGE: &str = "\
isha-vector-db-bench — measure the engine

USAGE
  isha-vector-db-bench [--quick | --standard | --large] [--json <path>]

SCALES
  --quick      5,000 documents at 128 dimensions   (a pull request)
  --standard   50,000 at 384                       (the default; a realistic on-device corpus)
  --large      250,000 at 768                      (where a flat scan starts to hurt)

OPTIONS
  --json <path>   also write machine-readable results, for committing as a baseline

Build with --release. A debug build will run, print a warning, and refuse to write JSON.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    let scale = if args.iter().any(|a| a == "--quick") {
        Scale::quick()
    } else if args.iter().any(|a| a == "--large") {
        Scale::large()
    } else {
        Scale::standard()
    };
    let json_path = args
        .iter()
        .position(|a| a == "--json")
        .and_then(|i| args.get(i + 1))
        .cloned();

    let debug_build = cfg!(debug_assertions);
    if debug_build {
        eprintln!("warning: this is a debug build. The numbers below are not comparable to");
        eprintln!("         anything and must not be quoted. Rebuild with --release.\n");
    }

    println!(
        "scale: {} documents at {} dimensions",
        scale.documents, scale.dimension
    );
    println!("running…\n");

    let measurements = match workloads::run_all(scale) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("benchmark failed: {e}");
            return ExitCode::from(1);
        }
    };

    print_table(&measurements);

    if let Some(path) = json_path {
        if debug_build {
            eprintln!("\nrefusing to write {path}: debug numbers must not become a baseline");
            return ExitCode::from(2);
        }
        let document = json::render(&measurements, scale);
        match std::fs::write(&path, document) {
            Ok(()) => println!("\nwrote {path}"),
            Err(e) => {
                eprintln!("could not write {path}: {e}");
                return ExitCode::from(1);
            }
        }
    }
    ExitCode::SUCCESS
}

fn print_table(measurements: &[Measurement]) {
    println!(
        "{:<32} {:>10} {:>12} {:>10} {:>9} {:>9} {:>9}",
        "workload", "count", "throughput", "total", "p50", "p95", "p99"
    );
    println!("{}", "─".repeat(96));
    for m in measurements {
        // A single-shot workload — a cold open — has no throughput and no distribution, but its
        // total *is* the number. Showing a dash for all four made the most interesting figure in
        // the run invisible.
        let throughput = match m.throughput() {
            Some(t) if m.count > 1 => format!("{t:.0}/s"),
            _ => "—".to_owned(),
        };
        println!(
            "{:<32} {:>10} {:>12} {:>10} {:>9} {:>9} {:>9}",
            m.name,
            m.count,
            throughput,
            duration(Some(m.total)),
            duration(m.percentile(50.0)),
            duration(m.percentile(95.0)),
            duration(m.percentile(99.0)),
        );
        for (key, value) in &m.notes {
            println!("    {key}: {value}");
        }
    }
}

fn duration(d: Option<Duration>) -> String {
    // Sub-microsecond operations exist — a hit in the memtable id map is one — and rounding them
    // to "0µs" hides both the value and the fact that it was measured at all.
    match d {
        None => "—".to_owned(),
        Some(d) if d.as_nanos() < 1_000 => format!("{}ns", d.as_nanos()),
        Some(d) if d.as_nanos() < 1_000_000 => format!("{:.1}µs", d.as_nanos() as f64 / 1000.0),
        Some(d) if d.as_millis() < 1_000 => format!("{:.2}ms", d.as_secs_f64() * 1000.0),
        Some(d) => format!("{:.2}s", d.as_secs_f64()),
    }
}
