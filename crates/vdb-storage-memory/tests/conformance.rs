//! `MemoryStorage` is the reference backend, so it must pass the shared suite exactly.

use vdb_core::storage::Storage;
use vdb_storage_memory::MemoryStorage;
use vdb_testkit::storage_conformance;

#[test]
fn memory_storage_is_conformant() {
    let report = storage_conformance(&|| Box::new(MemoryStorage::new()) as Box<dyn Storage>);
    report.assert_ok();
    assert!(
        report.passed.len() >= 25,
        "suite shrank unexpectedly: {}",
        report.passed.len()
    );
}
