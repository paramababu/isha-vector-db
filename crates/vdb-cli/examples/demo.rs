//! Builds a small database so the CLI can be tried by hand.
//!
//! ```text
//! cargo run -p vdb-cli --example demo -- /tmp/vdb-demo
//! cargo run -p vdb-cli -- stats /tmp/vdb-demo
//! ```

#![allow(clippy::print_stdout, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use vdb_core::api::{CollectionSpec, Database, DatabaseConfig};
use vdb_core::clock::ManualClock;
use vdb_core::document::DocumentInput;
use vdb_core::metadata::{Metadata, Value};
use vdb_core::vector::VectorView;
use vdb_core::Metric;
use vdb_storage_os::OsStorage;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/vdb-demo".to_owned());
    let _ = std::fs::remove_dir_all(&path);

    let db = Database::open(
        Arc::new(OsStorage::open(&path).unwrap()),
        DatabaseConfig::default(),
        Arc::new(ManualClock::default()),
    )
    .unwrap();
    let c = db
        .create_collection(CollectionSpec::new("products", 4, Metric::Cosine))
        .unwrap();
    for i in 0..1000i64 {
        let mut meta = Metadata::new();
        meta.insert("index", Value::I64(i));
        meta.insert(
            "category",
            Value::Str(if i % 3 == 0 { "tools" } else { "toys" }.into()),
        );
        c.insert(
            DocumentInput::new(
                format!("doc-{i:04}"),
                VectorView::f32(&[i as f32, 1.0, 2.0, 3.0]),
            )
            .with_metadata(meta),
        )
        .unwrap();
    }
    c.flush().unwrap();
    // Delete most of them, so there is something for `vdb compact` to reclaim.
    for i in 0..600 {
        c.delete(format!("doc-{i:04}")).unwrap();
    }
    c.flush().unwrap();
    db.close().unwrap();

    println!("created a demo database at {path}");
    println!("try:  cargo run -p vdb-cli -- stats {path}");
}
