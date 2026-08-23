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

/// Symbols that are exported but deliberately not part of the C ABI.
///
/// The list is spelled out here, in the guard, rather than inferred from a `cfg` attribute in
/// the source. Inferring it would make the exemption invisible at review time and easy to widen
/// by accident; naming each symbol means adding one is a deliberate edit to the file whose whole
/// job is to stop the ABI drifting.
///
/// Everything on this list must be genuinely outside the contract in `include/vdb.h` — a
/// calling-convention detail of one embedder, not database functionality. `vdb_abi_version()`
/// does not cover any of it.
const NOT_PART_OF_THE_C_ABI: &[(&str, &str)] = &[
    (
        "vdb_wasm_alloc",
        "WebAssembly only: JavaScript cannot allocate in this module's linear memory, so the \
         module must hand it a region. A C caller has its own allocator and never needs this.",
    ),
    (
        "vdb_wasm_free",
        "WebAssembly only: the counterpart to vdb_wasm_alloc.",
    ),
];

#[test]
fn every_exported_function_is_declared_in_the_header() {
    let (exported, declared) = (exported(), declared());
    let exempt: BTreeSet<String> = NOT_PART_OF_THE_C_ABI
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect();
    let missing: Vec<&String> = exported
        .difference(&declared)
        .filter(|name| !exempt.contains(*name))
        .collect();
    assert!(
        missing.is_empty(),
        "exported but absent from include/vdb.h: {missing:?}\n\
         An SDK author cannot call what the header does not mention.\n\
         If a symbol is genuinely not part of the C ABI, add it to NOT_PART_OF_THE_C_ABI with \
         a reason."
    );
}

/// The exemption list must not rot.
///
/// An entry for a symbol that no longer exists is a stale exemption, and a stale exemption is
/// how a real export slips through later under a name someone already blessed.
#[test]
fn every_exemption_names_a_symbol_that_exists() {
    // The wasm-only exports are compiled out on this target, so their absence from `exported()`
    // proves nothing. Look for them in the source instead.
    let sources = sources().join("\n");
    for (name, reason) in NOT_PART_OF_THE_C_ABI {
        assert!(
            sources.contains(name),
            "{name} is exempted from the header but no longer exported; remove the exemption"
        );
        assert!(
            reason.len() > 30,
            "{name} needs a real reason for being outside the C ABI"
        );
    }
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
        // Raised from 40 when filters and maintenance landed. Raising it should be a
        // deliberate act with a reason attached: the tripwire does not stop growth, it
        // stops growth nobody noticed.
        n <= 45,
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
        ("VDB_OK", isha_vector_db_ffi::VDB_OK),
        ("VDB_NULL_POINTER", isha_vector_db_ffi::VDB_NULL_POINTER),
        ("VDB_INTERNAL", isha_vector_db_ffi::VDB_INTERNAL),
        ("VDB_INVALID_UTF8", isha_vector_db_ffi::VDB_INVALID_UTF8),
        (
            "VDB_INVALID_ARGUMENT",
            isha_vector_db_ffi::VDB_INVALID_ARGUMENT,
        ),
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
