//! The storage conformance suite, run against the web backend.
//!
//! The real host is JavaScript driving OPFS and needs a browser. The suite runs here against
//! `test_host`, which implements the same imports in Rust, so everything between the engine and
//! the host interface — path resolution, error-code translation, the listing format, the
//! short-read contract, the buffer-growth loop — is tested in ordinary CI. What the browser is
//! then left to prove is only the part it uniquely can: that OPFS itself behaves the way this
//! interface says a host must.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use vdb_core::storage::Storage;
use vdb_storage_web::{test_host, WebStorage};

#[test]
fn the_web_backend_conforms() {
    let report = vdb_testkit::storage_conformance(&|| {
        let root = test_host::unique_root("conformance");
        // The host requires a parent to exist before a file can be created in it, exactly as
        // OPFS does, so the root has to be made first.
        let storage = WebStorage::open(root.clone());
        storage
            .create_dir_all(&vdb_core::path::DbPath::root())
            .expect("create root");
        Box::new(storage)
    });
    report.assert_ok();
}
