//! The header and the implementation must declare the same functions.
//!
//! `cbindgen` would generate the header from the source. This project writes it by hand instead:
//! the header is the document four SDK authors read, and it carries ownership and threading
//! rules that no generator would produce. The cost of hand-writing is drift, so this test pays
//! that cost down — it fails if a function exists on one side and not the other, in either
//! direction.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeSet;

const HEADER: &str = include_str!("../include/vdb.h");
/// Every Rust source in the crate, read at run time.
///
/// A hardcoded list of `include_str!`s is what this used to be, and it had exactly the failure
/// it exists to prevent: adding `filter.rs` made nine new exports invisible to the guard, which
/// then reported them as declared-but-missing. A guard with a manual list of what to guard has
/// a gap the size of the next file someone adds.
fn sources() -> Vec<String> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read src/").flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "rs") {
            out.push(std::fs::read_to_string(&path).expect("read source"));
        }
    }
    assert!(
        out.len() >= 4,
        "expected several sources in src/, found {}",
        out.len()
    );
    out
}

/// Functions the Rust side exports.
fn exported() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for source in sources() {
        let mut lines = source.lines().peekable();
        while let Some(line) = lines.next() {
            if line.trim() != "#[no_mangle]" {
                continue;
            }
            let Some(decl) = lines.peek() else { continue };
            let Some(rest) = decl.split("fn ").nth(1) else {
                continue;
            };
            let Some(name) = rest.split('(').next() else {
                continue;
            };
            out.insert(name.trim().to_owned());
        }
    }
    out
}

/// Functions the header declares.
fn declared() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in HEADER.lines() {
        let line = line.trim();
        // Declarations only: a line mentioning a function inside a comment starts with `*`.
        if line.starts_with('*') || line.starts_with("/*") || !line.contains("vdb_") {
            continue;
        }
        let Some(open) = line.find('(') else { continue };
        let before = &line[..open];
        let Some(name) = before.split_whitespace().last() else {
            continue;
        };
        let name = name.trim_start_matches('*');
        if name.starts_with("vdb_") {
            out.insert(name.to_owned());
        }
    }
    out
}

#[test]
fn every_exported_function_is_declared_in_the_header() {
    let (exported, declared) = (exported(), declared());
    let missing: Vec<&String> = exported.difference(&declared).collect();
    assert!(
        missing.is_empty(),
        "exported but absent from include/vdb.h: {missing:?}\n\
         An SDK author cannot call what the header does not mention."
    );
}

#[test]
fn every_declared_function_is_actually_exported() {
    let declared = declared();
    let exported = exported();
    let phantom: Vec<&String> = declared.difference(&exported).collect();
    assert!(
        phantom.is_empty(),
        "declared in include/vdb.h but not exported: {phantom:?}\n\
         This is the worse direction: it links and then fails at load."
    );
}

#[test]
fn the_surface_is_the_expected_size() {
    // Not a limit, a tripwire. The ABI is meant to be frozen and additive; a jump in this number
    // means someone added a lot at once, which is worth a second look in review.
    let n = exported().len();
    assert!(n >= 20, "the ABI lost functions: {n}");
    assert!(
        n <= 40,
        "the ABI grew to {n} functions; is this still a minimal surface?"
    );
}

#[test]
fn the_header_states_the_rules_an_sdk_author_needs() {
    // Each of these has caused a real bug in some binding somewhere. The header is where they
    // belong, because it is the document that gets read.
    for rule in [
        "vdb_abi_version",
        "NUL",
        "Ownership",
        "Threading",
        "Panics",
        "higher-is-better",
    ] {
        assert!(HEADER.contains(rule), "the header should mention {rule:?}");
    }
}

#[test]
fn status_codes_agree_between_the_header_and_the_crate() {
    for (name, value) in [
        ("VDB_OK", vdb_ffi::VDB_OK),
        ("VDB_NULL_POINTER", vdb_ffi::VDB_NULL_POINTER),
        ("VDB_INTERNAL", vdb_ffi::VDB_INTERNAL),
        ("VDB_INVALID_UTF8", vdb_ffi::VDB_INVALID_UTF8),
        ("VDB_INVALID_ARGUMENT", vdb_ffi::VDB_INVALID_ARGUMENT),
    ] {
        let rendered = if value < 0 {
            format!("#define {name} ({value})")
        } else {
            format!("#define {name} {value}")
        };
        assert!(
            HEADER.contains(&rendered),
            "the header disagrees about {name}; expected {rendered:?}"
        );
    }
}
