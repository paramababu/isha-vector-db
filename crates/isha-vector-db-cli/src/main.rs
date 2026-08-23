//! `vdb` — inspect, verify and compact a database from a terminal.
//!
//! Built early, before it is strictly needed, because every debugging session from here will
//! want it. When something goes wrong with a user's database, the first question is always
//! "what is actually in it?", and answering that by writing a throwaway Rust program each time
//! is how a project ends up with no tooling at all.
//!
//! Argument parsing is hand-rolled. Five subcommands and a handful of flags is not enough to
//! justify a dependency, and this crate ships in the same repository as an engine that keeps
//! `isha-vector-db-core` at zero dependencies — the standard should not relax just because it is a binary.

// A command-line tool's entire job is to write to the terminal and exit with a status.
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::exit)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

mod format;
mod run;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run::dispatch(&args) {
        Ok(code) => code,
        Err(e) => {
            // The code is printed alongside the message: it is stable, greppable, and what a
            // user should quote in a bug report.
            eprintln!("error: {e}");
            eprintln!("  code: {}", e.code());
            eprintln!("  what to do: {}", format::advice(&e));
            ExitCode::from(1)
        }
    }
}
