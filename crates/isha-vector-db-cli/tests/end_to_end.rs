//! The CLI, driven against a real database.
//!
//! Building the tool and never running it is how a debugging aid turns out to be broken exactly
//! when it is needed. These tests create a database, run each command as a subprocess, and check
//! both the exit code and that the output actually says something.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use isha_vector_db_core::api::{CollectionSpec, Database, DatabaseConfig};
use isha_vector_db_core::clock::ManualClock;
use isha_vector_db_core::document::DocumentInput;
use isha_vector_db_core::metadata::{Metadata, Value};
use isha_vector_db_core::vector::VectorView;
use isha_vector_db_core::Metric;
use isha_vector_db_storage_os::OsStorage;

#[derive(Debug)]
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "isha-vector-db-cli-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn str(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A database with a flushed collection and some tombstones, so compaction has work to do.
fn seed(dir: &TempDir) {
    let db = Database::open(
        Arc::new(OsStorage::open(&dir.0).unwrap()),
        DatabaseConfig::default(),
        Arc::new(ManualClock::default()),
    )
    .unwrap();
    let c = db
        .create_collection(CollectionSpec::new("products", 4, Metric::Cosine))
        .unwrap();
    for i in 0..20 {
        let mut meta = Metadata::new();
        meta.insert("index", Value::I64(i));
        meta.insert("category", Value::Str("tools".into()));
        c.insert(
            DocumentInput::new(
                format!("doc-{i:02}"),
                VectorView::f32(&[i as f32, 1.0, 2.0, 3.0]),
            )
            .with_metadata(meta)
            .with_content(b"the source text"),
        )
        .unwrap();
    }
    c.flush().unwrap();
    for i in 0..15 {
        c.delete(format!("doc-{i:02}")).unwrap();
    }
    c.flush().unwrap();
    db.close().unwrap();
}

fn vdb(args: &[&str]) -> (i32, String) {
    let exe = env!("CARGO_BIN_EXE_isha-vector-db");
    let out = Command::new(exe).args(args).output().expect("run vdb");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), text)
}

#[test]
fn version_reports_the_on_disk_format() {
    let (code, out) = vdb(&["version"]);
    assert_eq!(code, 0);
    // Derived, not hardcoded: a version bump should not need this test edited, but it must
    // still fail if the CLI stops reporting the range at all.
    assert!(
        out.contains(&format!(
            "on-disk format: v{}",
            isha_vector_db_format::FORMAT_VERSION
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "reads formats: v{}..=v{}",
            isha_vector_db_format::MIN_READABLE_VERSION,
            isha_vector_db_format::FORMAT_VERSION
        )),
        "{out}"
    );
}

#[test]
fn no_arguments_prints_usage() {
    let (code, out) = vdb(&[]);
    assert_eq!(code, 2, "usage should not be a success");
    assert!(out.contains("USAGE"), "{out}");
    assert!(out.contains("compact"), "{out}");
}

#[test]
fn stats_reports_what_is_in_the_database() {
    let dir = TempDir::new("stats");
    seed(&dir);
    let (code, out) = vdb(&["stats", &dir.str()]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("products"), "{out}");
    assert!(out.contains("live documents"), "{out}");
    assert!(out.contains("cosine"), "{out}");
    assert!(
        out.contains("tombstoned rows"),
        "the dead rows should be surfaced: {out}"
    );
    assert!(
        out.contains("isha-vector-db compact"),
        "and the fix suggested: {out}"
    );
}

#[test]
fn inspect_reports_layout_and_amplification() {
    let dir = TempDir::new("inspect");
    seed(&dir);
    let (code, out) = vdb(&["inspect", &dir.str()]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("bytes per vector"), "{out}");
    assert!(out.contains("storage amplification"), "{out}");
}

#[test]
fn verify_passes_on_a_healthy_database_and_warns_about_dead_rows() {
    let dir = TempDir::new("verify");
    seed(&dir);
    let (code, out) = vdb(&["verify", &dir.str()]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("no problems found"), "{out}");
    assert!(out.contains("Warnings"), "75% dead should warn: {out}");

    let (code, out) = vdb(&["verify", &dir.str(), "--full"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("Full"), "{out}");
}

/// A damaged database must exit with a distinct code, so a script can tell "this database is
/// broken" from "the tool could not run".
#[test]
fn verify_exits_with_its_own_code_on_damage() {
    let dir = TempDir::new("damaged");
    seed(&dir);

    let segments = dir.0.join("collections/products/segments");
    let victim = std::fs::read_dir(&segments)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "vec"))
        .expect("a vector file");
    let mut bytes = std::fs::read(&victim).unwrap();
    let n = bytes.len();
    bytes[n - 8] ^= 0xFF;
    std::fs::write(&victim, bytes).unwrap();

    let (code, out) = vdb(&["verify", &dir.str()]);
    assert_eq!(code, 3, "damage needs its own exit code: {out}");
    assert!(out.contains("Errors"), "{out}");
    assert!(out.contains("checksum"), "{out}");

    // And a quick check does not read the bytes, so it still passes — which is the difference
    // between the levels, stated where a user can see it.
    let (code, _) = vdb(&["verify", &dir.str(), "--quick"]);
    assert_eq!(code, 0);
}

#[test]
fn compact_reclaims_space_and_says_how_much() {
    let dir = TempDir::new("compact");
    seed(&dir);

    let (code, out) = vdb(&["compact", &dir.str()]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("rows reclaimed"), "{out}");
    assert!(
        out.contains("15"),
        "fifteen dead rows should be reclaimed: {out}"
    );

    // The database still works afterwards, and there is nothing left to do.
    let (code, out) = vdb(&["verify", &dir.str(), "--full"]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = vdb(&["compact", &dir.str()]);
    assert_eq!(code, 0);
    assert!(out.contains("nothing to do"), "{out}");
}

#[test]
fn compact_rejects_a_nonsensical_threshold() {
    let dir = TempDir::new("threshold");
    seed(&dir);
    let (code, out) = vdb(&["compact", &dir.str(), "--min-dead", "banana"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("--min-dead"), "{out}");

    let (code, _) = vdb(&["compact", &dir.str(), "--min-dead", "5"]);
    assert_eq!(code, 1, "a ratio above 1 is meaningless");
}

#[test]
fn get_fetches_a_document_and_distinguishes_absence() {
    let dir = TempDir::new("get");
    seed(&dir);

    let (code, out) = vdb(&["get", &dir.str(), "products", "doc-19"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("doc-19"), "{out}");
    assert!(out.contains("4 dims"), "{out}");
    assert!(out.contains("category"), "metadata should be shown: {out}");
    assert!(
        out.contains("15 bytes"),
        "content length should be shown: {out}"
    );

    // A deleted document is absent, with its own exit code.
    let (code, out) = vdb(&["get", &dir.str(), "products", "doc-00"]);
    assert_eq!(code, 4, "{out}");
    assert!(out.contains("not found"), "{out}");
}

#[test]
fn a_missing_database_is_an_error_with_advice() {
    let dir = TempDir::new("empty");
    let (code, out) = vdb(&["stats", &format!("{}/nothing-here", dir.str())]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("code: VDB-"),
        "the error code should be quotable: {out}"
    );
    assert!(out.contains("what to do"), "{out}");
}

/// Reading a database an application has open must work — that is the whole point of the CLI
/// defaulting to read-only.
#[cfg(unix)]
#[test]
fn inspection_works_while_an_application_holds_the_database() {
    let dir = TempDir::new("concurrent");
    seed(&dir);
    let db = Database::open(
        Arc::new(OsStorage::open(&dir.0).unwrap()),
        DatabaseConfig::default(),
        Arc::new(ManualClock::default()),
    )
    .unwrap();

    let (code, out) = vdb(&["stats", &dir.str()]);
    assert_eq!(code, 0, "read-only inspection should not be blocked: {out}");
    let (code, _) = vdb(&["verify", &dir.str()]);
    assert_eq!(code, 0);

    // Compaction writes, so it must be refused while the lock is held.
    let (code, out) = vdb(&["compact", &dir.str()]);
    assert_eq!(code, 1, "{out}");
    assert!(out.to_lowercase().contains("already open"), "{out}");

    db.close().unwrap();
}
