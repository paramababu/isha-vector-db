//! What gets measured.
//!
//! Every workload uses a seeded generator, so two runs on the same machine measure the same
//! work and a difference between them is a change in the code rather than in the data.

use std::sync::Arc;

use vdb_core::api::{
    Collection, CollectionSpec, CompactOptions, Database, DatabaseConfig, SearchRequest,
};
use vdb_core::clock::ManualClock;
use vdb_core::document::DocumentInput;
use vdb_core::filter::Filter;
use vdb_core::metadata::{Metadata, Value};
use vdb_core::persistence::Durability;
use vdb_core::vector::VectorView;
use vdb_core::{Metric, Result, WriteBatch};
use vdb_storage_os::OsStorage;
use vdb_testkit::Rng;

use crate::harness::{directory_bytes, peak_rss_bytes, sampled, timed, Measurement};

/// How much work to do.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Scale {
    pub documents: usize,
    pub dimension: u32,
    pub queries: usize,
}

impl Scale {
    /// Small enough for a pull request, large enough to be more than noise.
    pub(crate) fn quick() -> Self {
        Self {
            documents: 5_000,
            dimension: 128,
            queries: 300,
        }
    }

    /// The shape of a real on-device workload: a sentence-embedding dimension and a corpus size
    /// where a brute-force scan is still the right choice.
    pub(crate) fn standard() -> Self {
        Self {
            documents: 50_000,
            dimension: 384,
            queries: 500,
        }
    }

    /// Where a flat scan starts to hurt, which is the number that says when an approximate index
    /// becomes worth building.
    pub(crate) fn large() -> Self {
        Self {
            documents: 250_000,
            dimension: 768,
            queries: 200,
        }
    }

    pub(crate) fn name(&self) -> String {
        format!("{}docs-{}d", self.documents, self.dimension)
    }
}

/// A scratch database directory, removed when the run ends.
pub(crate) struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("vdb-bench-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Open with the accelerated index, which is what every shipped SDK does — so the numbers
/// describe what a user gets, not what an auditor's default build gets.
fn open(path: &std::path::Path, durability: Durability) -> Result<Database> {
    Database::open_with_index(
        Arc::new(OsStorage::open(path)?),
        DatabaseConfig::default().durability(durability),
        Arc::new(ManualClock::default()),
        Arc::new(vdb_index_flat::FlatIndex::new()),
    )
}

/// Open with the core's reference scan, for measuring what the SIMD kernels actually buy.
fn open_reference(path: &std::path::Path) -> Result<Database> {
    Database::open(
        Arc::new(OsStorage::open(path)?),
        DatabaseConfig::default().durability(Durability::Batch),
        Arc::new(ManualClock::default()),
    )
}

/// Deterministic corpus: clustered rather than uniform, because uniform random vectors in high
/// dimensions are all nearly equidistant, which makes every ranking a coin flip and hides real
/// differences in the scan.
fn corpus(scale: Scale, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng::new(seed);
    let clusters: Vec<Vec<f32>> = (0..16)
        .map(|_| rng.vector(scale.dimension as usize))
        .collect();
    (0..scale.documents)
        .map(|i| {
            let centre = clusters
                .get(i % clusters.len())
                .cloned()
                .unwrap_or_default();
            centre
                .iter()
                .map(|c| c + rng.next_gaussian() * 0.3)
                .collect()
        })
        .collect()
}

fn metadata_for(i: usize) -> Metadata {
    let mut m = Metadata::new();
    m.insert("index", Value::I64(i as i64));
    // A field with 1-in-10 selectivity, so the filtered-search benchmark measures something.
    m.insert("bucket", Value::I64((i % 10) as i64));
    m.insert(
        "category",
        Value::Str(if i % 3 == 0 { "a".into() } else { "b".into() }),
    );
    m
}

/// How many fields the wide-metadata corpus carries. Comfortably above
/// `INDEX_MIN_ENTRIES`, and representative of a document with real attributes rather than the
/// three the main corpus uses.
/// Overridable at compile time (`VDB_BENCH_WIDE_FIELDS=12 cargo build`) so the crossover can be
/// re-measured without editing this file. Parsed at runtime because `from_str_radix` is not
/// const until Rust 1.82 and the MSRV is 1.78.
fn wide_fields() -> usize {
    option_env!("VDB_BENCH_WIDE_FIELDS")
        .and_then(|s| s.parse().ok())
        .unwrap_or(16)
}

/// Metadata wide enough to trigger the offset table, with the probe fields at the two extremes
/// of key order so a linear walk and a binary search give visibly different answers.
fn wide_metadata_for(i: usize) -> Metadata {
    let mut m = Metadata::new();
    // `aaa_first` sorts before everything, `zzz_last` after: the best and worst case for a walk.
    m.insert("aaa_first", Value::I64((i % 10) as i64));
    for f in 0..wide_fields().saturating_sub(2) {
        m.insert(
            format!("field_{f:02}"),
            Value::Str(format!("value-{}-{}", f, i % 7)),
        );
    }
    m.insert("zzz_last", Value::I64((i % 10) as i64));
    m
}

/// The measurement that justifies the offset table, or does not.
///
/// Both probes match nothing, so each is a pure lookup over every row: no distances, no top-k
/// churn. Under a linear walk `zzz_last` pays for every key in the record and `aaa_first` pays
/// for none, so the gap between them *is* the walk cost. Under a binary search both are
/// `log2(n)` probes and the gap should collapse. Comparing the gap — not the absolute times —
/// is what makes this robust to the thermal drift that moves every number in a run.
fn wide_metadata_probe(scale: Scale, vectors: &[Vec<f32>]) -> Result<Vec<Measurement>> {
    let scratch = Scratch::new("wide");
    let db = open(scratch.path(), Durability::Batch)?;
    let c = db.create_collection(CollectionSpec::new("wide", scale.dimension, Metric::Cosine))?;
    for chunk in (0..vectors.len()).collect::<Vec<_>>().chunks(1000) {
        let mut batch = WriteBatch::with_capacity(chunk.len());
        for &i in chunk {
            let Some(v) = vectors.get(i) else { continue };
            batch.upsert(
                DocumentInput::new(format!("doc-{i:07}"), VectorView::f32(v))
                    .with_metadata(wide_metadata_for(i)),
            );
        }
        c.write_batch(batch)?;
    }
    c.flush()?;

    let mut out = Vec::new();
    for (label, field) in [
        ("wide_first_key", "aaa_first"),
        ("wide_last_key", "zzz_last"),
    ] {
        let filter = Filter::eq(field, Value::I64(-1));
        let mut m = one_filtered_run(&c, vectors, scale, label, &filter)?;
        m.note("fields_per_doc", wide_fields() as u64);
        out.push(m);
    }
    db.close()?;
    Ok(out)
}

/// Flat scan against the graph index: latency, and the recall that latency buys.
///
/// The comparison only means something with both numbers. A graph index that is ten times faster
/// at 60% recall is not ten times better, and reporting the speed alone would be the kind of
/// benchmark this project exists not to publish.
fn hnsw_against_flat(scale: Scale, vectors: &[Vec<f32>]) -> Result<Vec<Measurement>> {
    let mut out = Vec::new();
    let flat_dir = Scratch::new("cmp-flat");
    let hnsw_dir = Scratch::new("cmp-hnsw");

    let mut truth: Vec<Vec<vdb_core::document::DocId>> = Vec::new();
    let mut approx: Vec<Vec<vdb_core::document::DocId>> = Vec::new();

    for (label, dir, use_hnsw) in [
        ("flat", flat_dir.path(), false),
        ("hnsw", hnsw_dir.path(), true),
    ] {
        let storage = Arc::new(OsStorage::open(dir.to_str().unwrap_or("."))?);
        let clock = Arc::new(ManualClock::new(1_700_000_000_000));
        let config = DatabaseConfig::default().durability(Durability::Batch);
        let db = if use_hnsw {
            Database::open_with_index(
                storage,
                config,
                clock,
                Arc::new(vdb_index_hnsw::HnswIndex::new()),
            )?
        } else {
            Database::open_with_index(
                storage,
                config,
                clock,
                Arc::new(vdb_index_flat::FlatIndex::new()),
            )?
        };
        let c = db.create_collection(CollectionSpec::new(
            "bench",
            scale.dimension,
            Metric::Cosine,
        ))?;
        for chunk in (0..vectors.len()).collect::<Vec<_>>().chunks(1000) {
            let mut batch = WriteBatch::with_capacity(chunk.len());
            for &i in chunk {
                let Some(v) = vectors.get(i) else { continue };
                batch.upsert(DocumentInput::new(
                    format!("doc-{i:07}"),
                    VectorView::f32(v),
                ));
            }
            c.write_batch(batch)?;
        }
        c.flush()?;

        // Building the graph is a real cost and is measured separately, not hidden inside the
        // first query's latency.
        if use_hnsw {
            let (mut m, r) = timed("hnsw_build", "graph", 1, || {
                c.search(&SearchRequest::new(
                    VectorView::f32(vectors.first().map_or(&[][..], Vec::as_slice)),
                    1,
                ))
            });
            r?;
            m.note("documents", scale.documents as u64);
            m.note("dimension", scale.dimension);
            out.push(m);
        }

        let mut error = None;
        let mut m = sampled(format!("search_k10_{label}"), "query", scale.queries, |i| {
            let Some(base) = vectors.get(i * 7 % vectors.len()) else {
                return;
            };
            if let Err(e) = c.search(&SearchRequest::new(VectorView::f32(base), 10)) {
                error.get_or_insert(e);
            }
        });
        if let Some(e) = error {
            return Err(e);
        }
        m.note("dimension", scale.dimension);
        m.note("index", label);
        out.push(m);

        // Collect answers for the recall comparison, from queries that are not corpus members.
        let mut answers = Vec::new();
        for i in 0..50usize {
            let Some(base) = vectors.get(i * 37 % vectors.len()) else {
                continue;
            };
            let query: Vec<f32> = base.iter().map(|x| x * 0.9 + 0.05).collect();
            let hits = c.search(&SearchRequest::new(VectorView::f32(&query), 10))?;
            answers.push(hits.hits.iter().map(|h| h.id.clone()).collect());
        }
        if use_hnsw {
            approx = answers;
        } else {
            truth = answers;
        }
        db.close()?;
    }

    let mut overlap = 0usize;
    let mut total = 0usize;
    for (t, a) in truth.iter().zip(approx.iter()) {
        overlap += a.iter().filter(|id| t.contains(id)).count();
        total += t.len();
    }
    let mut recall = Measurement::new("hnsw_recall_at_10", "ratio");
    recall.count = (overlap * 1000).checked_div(total).unwrap_or(0) as u64;
    recall.note(
        "scale",
        "per mille against the flat scan on the same corpus",
    );
    recall.note(
        "ef_search",
        vdb_index_hnsw::HnswParams::default().ef_search as u64,
    );
    out.push(recall);
    Ok(out)
}

/// Run everything and return the measurements.
pub(crate) fn run_all(scale: Scale) -> Result<Vec<Measurement>> {
    let mut out = Vec::new();
    let vectors = corpus(scale, 0x5EED);

    let scratch = Scratch::new("main");
    let db = open(scratch.path(), Durability::Batch)?;
    let collection = db.create_collection(CollectionSpec::new(
        "bench",
        scale.dimension,
        Metric::Cosine,
    ))?;

    out.push(insert_one_at_a_time(&collection, &vectors, scale)?);
    out.push(flush_to_disk(&collection, scratch.path())?);
    out.extend(search_latency(&collection, &vectors, scale)?);
    out.extend(filter_selectivity_sweep(&collection, &vectors, scale)?);
    out.extend(wide_metadata_probe(scale, &vectors)?);
    out.push(get_by_id(&collection, scale)?);
    db.close()?;

    out.push(scalar_search_for_comparison(
        scratch.path(),
        &vectors,
        scale,
    )?);
    out.push(cold_open(scratch.path(), scale)?);
    out.push(storage_footprint(scratch.path(), scale));

    out.push(batch_insert(scale, &vectors)?);
    out.push(recovery_after_unclean_shutdown(scale, &vectors)?);
    out.push(compaction(scale, &vectors)?);
    out.extend(hnsw_against_flat(scale, &vectors)?);

    if let Some(rss) = peak_rss_bytes() {
        let mut m = Measurement::new("peak_memory", "bytes");
        m.count = rss;
        m.note(
            "caveat",
            "peak for the whole process, not a steady-state reading",
        );
        out.push(m);
    }
    let mut kernel = Measurement::new("kernel_backend", "info");
    kernel.note("backend", vdb_index_flat::FlatIndex::new().backend().name());
    out.push(kernel);
    Ok(out)
}

fn insert_one_at_a_time(c: &Collection, vectors: &[Vec<f32>], scale: Scale) -> Result<Measurement> {
    let mut error = None;
    let mut m = sampled("insert_single", "doc", scale.documents, |i| {
        let Some(v) = vectors.get(i) else { return };
        let doc = DocumentInput::new(format!("doc-{i:07}"), VectorView::f32(v))
            .with_metadata(metadata_for(i));
        if let Err(e) = c.upsert(doc) {
            error.get_or_insert(e);
        }
    });
    if let Some(e) = error {
        return Err(e);
    }
    m.note("durability", "batch");
    m.note("dimension", scale.dimension);
    Ok(m)
}

fn flush_to_disk(c: &Collection, path: &std::path::Path) -> Result<Measurement> {
    let count = c.count()?;
    let (mut m, result) = timed("flush", "doc", count, || c.flush());
    result?;
    m.note("bytes_on_disk", directory_bytes(path));
    Ok(m)
}

fn search_latency(c: &Collection, vectors: &[Vec<f32>], scale: Scale) -> Result<Vec<Measurement>> {
    let mut out = Vec::new();
    for k in [1usize, 10, 100] {
        let mut error = None;
        let mut m = sampled(format!("search_k{k}"), "query", scale.queries, |i| {
            // Query with a real corpus vector perturbed slightly, which is what a nearest-
            // neighbour lookup actually looks like — not a random point in space.
            let Some(base) = vectors.get(i * 7 % vectors.len()) else {
                return;
            };
            let query: Vec<f32> = base.iter().map(|x| x * 1.01).collect();
            match c.search(&SearchRequest::new(VectorView::f32(&query), k)) {
                Ok(r) => debug_assert!(!r.is_empty()),
                Err(e) => {
                    error.get_or_insert(e);
                }
            }
        });
        if let Some(e) = error {
            return Err(e);
        }
        m.note("documents", c.count()?);
        m.note("dimension", scale.dimension);
        out.push(m);
    }
    let mut kernel = Measurement::new("kernel_backend", "info");
    kernel.note("backend", vdb_index_flat::FlatIndex::new().backend().name());
    out.push(kernel);
    Ok(out)
}

/// Filtered search across a range of selectivities.
///
/// One filtered figure cannot separate two costs that move in opposite directions: a filter
/// removes distance computations and adds a metadata lookup per candidate. Sweeping selectivity
/// separates them — at 100% nothing is skipped, so the difference from an unfiltered scan is
/// the lookup cost alone; at 1% almost everything is skipped, so what remains is mostly lookup
/// too. If the two ends agree, the lookup dominates and the field-offset-table work is
/// justified. If the low end is much faster, the scan does.
fn filter_selectivity_sweep(
    c: &Collection,
    vectors: &[Vec<f32>],
    scale: Scale,
) -> Result<Vec<Measurement>> {
    let mut out = Vec::new();
    // `bucket` holds 0..10, so `bucket < n` selects roughly n * 10%.
    for (label, threshold) in [("1pct", 1i64), ("10pct", 1), ("50pct", 5), ("100pct", 10)] {
        let filter = if label == "1pct" {
            // A value no document has: nothing matches, so every row costs a lookup and no
            // distance at all. The purest measurement of lookup cost there is.
            Filter::eq("bucket", Value::I64(999))
        } else {
            Filter::lt("bucket", Value::I64(threshold))
        };
        out.push(one_filtered_run(c, vectors, scale, label, &filter)?);
    }
    // Does *walking* keys cost anything, or is the cost fixed per-row plumbing?
    //
    // Metadata is stored with keys sorted, so `bucket` is found on the first comparison and
    // `index` on the third. If a field offset table would help — the fix the filter docs claim
    // is needed — these two must differ measurably. If they do not, the walk is not the cost
    // and the documented fix is the wrong one.
    for (label, field) in [("first_key", "bucket"), ("last_key", "index")] {
        let filter = Filter::eq(field, Value::I64(-1)); // matches nothing: pure lookup cost
        out.push(one_filtered_run(c, vectors, scale, label, &filter)?);
    }
    Ok(out)
}

fn one_filtered_run(
    c: &Collection,
    vectors: &[Vec<f32>],
    scale: Scale,
    label: &str,
    filter: &Filter,
) -> Result<Measurement> {
    let mut error = None;
    let mut m = sampled(
        format!("search_k10_filter_{label}"),
        "query",
        scale.queries,
        |i| {
            let Some(base) = vectors.get(i * 7 % vectors.len()) else {
                return;
            };
            let request = SearchRequest::new(VectorView::f32(base), 10).with_filter(filter);
            if let Err(e) = c.search(&request) {
                error.get_or_insert(e);
            }
        },
    );
    if let Some(e) = error {
        return Err(e);
    }
    let sample = c.search(
        &SearchRequest::new(
            VectorView::f32(vectors.first().map_or(&[][..], Vec::as_slice)),
            10,
        )
        .with_filter(filter),
    )?;
    m.note("scored", sample.stats.considered);
    m.note("skipped", sample.stats.skipped);
    m.note("dimension", scale.dimension);
    Ok(m)
}

#[allow(dead_code)]
fn filtered_search(c: &Collection, vectors: &[Vec<f32>], scale: Scale) -> Result<Measurement> {
    // One bucket in ten, so roughly 10% of documents pass.
    let filter = Filter::eq("bucket", Value::I64(3));
    let mut error = None;
    let mut m = sampled("search_k10_filtered_10pct", "query", scale.queries, |i| {
        let Some(base) = vectors.get(i * 7 % vectors.len()) else {
            return;
        };
        let request = SearchRequest::new(VectorView::f32(base), 10).with_filter(&filter);
        if let Err(e) = c.search(&request) {
            error.get_or_insert(e);
        }
    });
    if let Some(e) = error {
        return Err(e);
    }
    // Selectivity is recorded, not assumed: a filter benchmark whose selectivity drifted would
    // otherwise look like a performance change.
    let sample = c.search(
        &SearchRequest::new(
            VectorView::f32(vectors.first().map_or(&[][..], Vec::as_slice)),
            10,
        )
        .with_filter(&filter),
    )?;
    m.note("considered", sample.stats.considered);
    m.note("skipped", sample.stats.skipped);
    m.note("dimension", scale.dimension);
    Ok(m)
}

fn get_by_id(c: &Collection, scale: Scale) -> Result<Measurement> {
    let mut error = None;
    let m = sampled("get_by_id", "lookup", scale.queries.min(1000), |i| {
        let id = vdb_core::DocId::from(format!("doc-{:07}", i % scale.documents));
        if let Err(e) = c.get(&id) {
            error.get_or_insert(e);
        }
    });
    if let Some(e) = error {
        return Err(e);
    }
    Ok(m)
}

/// The same search against the core's reference scan, so the SIMD speedup is a measured ratio
/// rather than a claim.
fn scalar_search_for_comparison(
    path: &std::path::Path,
    vectors: &[Vec<f32>],
    scale: Scale,
) -> Result<Measurement> {
    let db = open_reference(path)?;
    let c = db.open_collection("bench")?;
    let mut error = None;
    let mut m = sampled("search_k10_scalar_reference", "query", scale.queries, |i| {
        let Some(base) = vectors.get(i * 7 % vectors.len()) else {
            return;
        };
        let query: Vec<f32> = base.iter().map(|x| x * 1.01).collect();
        if let Err(e) = c.search(&SearchRequest::new(VectorView::f32(&query), 10)) {
            error.get_or_insert(e);
        }
    });
    if let Some(e) = error {
        return Err(e);
    }
    m.note("kernel", "scalar (vdb-core ExactScan)");
    m.note("dimension", scale.dimension);
    db.close()?;
    Ok(m)
}

fn cold_open(path: &std::path::Path, scale: Scale) -> Result<Measurement> {
    // The number a mobile application feels on launch: how long before the first query can run.
    let (mut m, db) = timed("cold_open", "database", 1, || open(path, Durability::Batch));
    let db = db?;
    let count = db.open_collection("bench")?.count()?;
    m.note("documents", count);
    m.note("dimension", scale.dimension);
    db.close()?;
    Ok(m)
}

fn storage_footprint(path: &std::path::Path, scale: Scale) -> Measurement {
    let bytes = directory_bytes(path);
    let raw = (scale.documents as u64) * u64::from(scale.dimension) * 4;
    let mut m = Measurement::new("storage_footprint", "bytes");
    m.count = bytes;
    m.note("raw_vector_bytes", raw);
    // The number that surprises people on a phone: what the database costs beyond the vectors.
    m.note(
        "amplification",
        format!(
            "{:.3}x",
            if raw > 0 {
                bytes as f64 / raw as f64
            } else {
                0.0
            }
        ),
    );
    m.note("bytes_per_document", bytes / scale.documents.max(1) as u64);
    m
}

fn batch_insert(scale: Scale, vectors: &[Vec<f32>]) -> Result<Measurement> {
    let scratch = Scratch::new("batch");
    let db = open(scratch.path(), Durability::Batch)?;
    let c = db.create_collection(CollectionSpec::new(
        "bench",
        scale.dimension,
        Metric::Cosine,
    ))?;

    const BATCH: usize = 1000;
    let (mut m, result) = timed("insert_batched_1000", "doc", scale.documents as u64, || {
        for chunk in (0..scale.documents).collect::<Vec<_>>().chunks(BATCH) {
            let mut batch = WriteBatch::with_capacity(chunk.len());
            for &i in chunk {
                let Some(v) = vectors.get(i) else { continue };
                batch.upsert(
                    DocumentInput::new(format!("doc-{i:07}"), VectorView::f32(v))
                        .with_metadata(metadata_for(i)),
                );
            }
            c.write_batch(batch)?;
        }
        c.flush()
    });
    result?;
    m.note("batch_size", BATCH);
    m.note("durability", "batch");
    db.close()?;
    Ok(m)
}

fn recovery_after_unclean_shutdown(scale: Scale, vectors: &[Vec<f32>]) -> Result<Measurement> {
    // Written and never flushed, then reopened: everything has to come back out of the log.
    // This is the number an application pays after the operating system kills it.
    let unflushed = scale.documents.min(20_000);
    let scratch = Scratch::new("recovery");
    {
        let db = open(scratch.path(), Durability::Batch)?;
        let c = db.create_collection(CollectionSpec::new(
            "bench",
            scale.dimension,
            Metric::Cosine,
        ))?;
        for i in 0..unflushed {
            let Some(v) = vectors.get(i) else { continue };
            c.upsert(DocumentInput::new(
                format!("doc-{i:07}"),
                VectorView::f32(v),
            ))?;
        }
        // Deliberately no flush and no close.
        std::mem::drop(c);
        std::mem::drop(db);
    }

    let (mut m, db) = timed("recovery_open", "database", 1, || {
        open(scratch.path(), Durability::Batch)
    });
    let db = db?;
    let recovered = db.open_collection("bench")?.count()?;
    m.note("documents_replayed", recovered);
    m.note("wal_bytes", directory_bytes(scratch.path()));
    if recovered as usize != unflushed {
        return Err(vdb_core::internal_error!(
            "recovery lost documents: expected {unflushed}, got {recovered}"
        ));
    }
    db.close()?;
    Ok(m)
}

fn compaction(scale: Scale, vectors: &[Vec<f32>]) -> Result<Measurement> {
    let n = scale.documents.min(20_000);
    let scratch = Scratch::new("compact");
    let db = open(scratch.path(), Durability::Batch)?;
    let c = db.create_collection(CollectionSpec::new(
        "bench",
        scale.dimension,
        Metric::Cosine,
    ))?;
    for i in 0..n {
        let Some(v) = vectors.get(i) else { continue };
        c.upsert(DocumentInput::new(
            format!("doc-{i:07}"),
            VectorView::f32(v),
        ))?;
    }
    c.flush()?;
    for i in 0..(n * 7 / 10) {
        c.delete(format!("doc-{i:07}"))?;
    }
    c.flush()?;

    let before = directory_bytes(scratch.path());
    let (mut m, report) = timed("compaction_70pct_dead", "row", n as u64, || {
        c.compact(CompactOptions::default())
    });
    let report = report?;
    let after = directory_bytes(scratch.path());
    m.note("rows_reclaimed", report.rows_reclaimed);
    m.note("bytes_before", before);
    m.note("bytes_after", after);
    m.note("bytes_reclaimed", before.saturating_sub(after));
    db.close()?;
    Ok(m)
}
