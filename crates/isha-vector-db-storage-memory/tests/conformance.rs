//! `MemoryStorage` is the reference backend, so it must pass the shared suite exactly.

use isha_vector_db_core::storage::Storage;
use isha_vector_db_storage_memory::MemoryStorage;
use isha_vector_db_testkit::storage_conformance;

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
