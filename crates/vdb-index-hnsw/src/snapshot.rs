//! Saving and restoring a built graph.
//!
//! # What is stored, and what is not
//!
//! The graph's *structure* is stored: levels, neighbour lists, the entry point, and the row each
//! node stands for. The vectors are not. They are already in the segments, and copying tens of
//! megabytes of floats into a second file to save re-reading them would be the largest part of
//! the snapshot for the smallest part of the saving — restoring walks the source anyway, to
//! confirm the graph still describes it.
//!
//! # Validation
//!
//! Restoring checks everything it uses: the metric, the dimension, the node count, that every
//! stored row matches the source row in the same position, and that every neighbour index is in
//! range. That sounds expensive next to trusting the file, and is nothing next to rebuilding —
//! a 50,000-node graph takes 95 seconds to build.
//!
//! Any failure returns `None` and the caller rebuilds. A snapshot is a cache; there is no
//! migration path and no error to report, because nothing has been lost.

use vdb_core::document::RowId;
use vdb_core::search::Metric;

use crate::graph::{Graph, Node};
use crate::params::HnswParams;

/// Layout version, independent of the database's on-disk format.
///
/// A snapshot written by a build with a different layout is discarded rather than migrated,
/// which is the whole reason this number can move freely without a format change.
const SNAPSHOT_VERSION: u16 = 1;

/// Recognises the payload before anything else is read from it.
const MAGIC: u32 = 0x484E_5357; // "HNSW"

/// Refuses a payload claiming more nodes than could possibly be described by its own length.
///
/// Without it a corrupt count would reserve gigabytes before the first neighbour list failed to
/// decode. The cheapest correct bound: every node costs at least a level byte, a row, and a
/// length prefix per layer.
const MIN_BYTES_PER_NODE: usize = 10;

/// Serialise a graph.
pub(crate) fn encode(graph: &Graph, params: &HnswParams) -> Vec<u8> {
    let mut out = Vec::with_capacity(graph.len() * 64);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
    out.extend_from_slice(&metric_code(graph.metric).to_le_bytes());
    out.extend_from_slice(&(graph.dimension as u32).to_le_bytes());
    out.extend_from_slice(&(params.m as u32).to_le_bytes());
    out.extend_from_slice(&(params.ef_construction as u32).to_le_bytes());
    out.extend_from_slice(&params.seed.to_le_bytes());
    out.extend_from_slice(&(graph.len() as u32).to_le_bytes());
    out.extend_from_slice(&(graph.layers.len() as u32).to_le_bytes());
    match graph.entry {
        Some(entry) => {
            out.push(1);
            out.extend_from_slice(&entry.to_le_bytes());
        }
        None => {
            out.push(0);
            out.extend_from_slice(&0u32.to_le_bytes());
        }
    }

    for row in &graph.rows {
        out.extend_from_slice(&row.as_u64().to_le_bytes());
    }
    out.extend_from_slice(&graph.levels);

    for layer in &graph.layers {
        for neighbours in layer {
            out.extend_from_slice(&(neighbours.len() as u32).to_le_bytes());
            for n in neighbours {
                out.extend_from_slice(&n.to_le_bytes());
            }
        }
    }
    out
}

/// What a snapshot says about itself, read before any of it is trusted.
pub(crate) struct Header {
    pub(crate) metric: Metric,
    pub(crate) dimension: usize,
    pub(crate) nodes: usize,
    pub(crate) layers: usize,
    pub(crate) entry: Option<Node>,
    pub(crate) params_match: bool,
    body: usize,
}

/// A little-endian reader that returns `None` rather than panicking at the end of input.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let out = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(out)
    }
    fn u8(&mut self) -> Option<u8> {
        // Bounds-checked, not indexed. This file is the one place in the crate that reads bytes
        // it did not write — the blanket `indexing_slicing` allow the graph modules carry does
        // not apply here, and must not.
        self.take(1).and_then(|b| b.first().copied())
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}

/// Read a snapshot's header, or `None` if it is not one this build can use.
pub(crate) fn decode_header(bytes: &[u8], params: &HnswParams) -> Option<Header> {
    let mut c = Cursor::new(bytes);
    if c.u32()? != MAGIC || c.u16()? != SNAPSHOT_VERSION {
        return None;
    }
    let metric = metric_from(c.u16()?)?;
    let dimension = c.u32()? as usize;
    let m = c.u32()? as usize;
    let ef_construction = c.u32()? as usize;
    let seed = c.u64()?;
    let nodes = c.u32()? as usize;
    let layers = c.u32()? as usize;
    let has_entry = c.u8()?;
    let entry_value = c.u32()?;

    // A count the remaining bytes could not possibly describe is rejected before anything is
    // allocated for it.
    if nodes.checked_mul(MIN_BYTES_PER_NODE)? > bytes.len() {
        return None;
    }
    if layers > usize::from(crate::graph::MAX_LEVEL) + 1 {
        return None;
    }
    let entry = match has_entry {
        0 => None,
        1 if (entry_value as usize) < nodes => Some(entry_value),
        _ => return None,
    };

    Some(Header {
        metric,
        dimension,
        nodes,
        layers,
        entry,
        // A snapshot built with different parameters is structurally fine to read, but it is not
        // the graph this index was asked for: its degree and level distribution are someone
        // else's. Rebuilding is the honest response to being handed the wrong graph.
        params_match: m == params.m
            && ef_construction == params.ef_construction
            && seed == params.seed,
        body: c.at,
    })
}

/// Rebuild a graph from a snapshot, given the rows it must describe.
///
/// `rows` is the source's own order, which the snapshot must match exactly — same length, same
/// row at every position. Anything else means the data changed underneath and the graph no longer
/// describes it.
pub(crate) fn decode(
    bytes: &[u8],
    header: &Header,
    rows: &[(RowId, Vec<f32>, f32)],
) -> Option<Graph> {
    if header.nodes != rows.len() {
        return None;
    }
    let mut c = Cursor::new(bytes);
    c.take(header.body)?;

    let mut graph = Graph {
        dimension: header.dimension,
        metric: Some(header.metric),
        entry: header.entry,
        ..Graph::default()
    };
    graph.rows.reserve(header.nodes);
    graph.vectors.reserve(header.nodes * header.dimension);
    graph.inv_norms.reserve(header.nodes);

    for (row, vector, inv_norm) in rows {
        if c.u64()? != row.as_u64() {
            // The source has changed: rows added, removed, or compacted into a different order.
            return None;
        }
        if vector.len() != header.dimension {
            return None;
        }
        graph.rows.push(*row);
        graph.vectors.extend_from_slice(vector);
        graph.inv_norms.push(*inv_norm);
    }

    let levels = c.take(header.nodes)?;
    graph.levels.extend_from_slice(levels);

    graph.layers.reserve(header.layers);
    for _ in 0..header.layers {
        let mut layer = Vec::with_capacity(header.nodes);
        for _ in 0..header.nodes {
            let count = c.u32()? as usize;
            // A neighbour list longer than the graph has nodes cannot be real, and reserving for
            // it is how a corrupt length turns into an allocation failure.
            if count > header.nodes {
                return None;
            }
            let mut neighbours = Vec::with_capacity(count);
            for _ in 0..count {
                let n = c.u32()?;
                // Checked here, once, so traversal can index directly.
                if n as usize >= header.nodes {
                    return None;
                }
                neighbours.push(n);
            }
            layer.push(neighbours);
        }
        graph.layers.push(layer);
    }

    // Every node must appear on every layer's list, which is the invariant the whole graph
    // indexes on.
    if graph.levels.len() != header.nodes || graph.layers.iter().any(|l| l.len() != header.nodes) {
        return None;
    }
    if graph.entry.is_some() != (header.nodes > 0) {
        return None;
    }
    Some(graph)
}

fn metric_code(metric: Option<Metric>) -> u16 {
    match metric {
        Some(Metric::Cosine) => 1,
        Some(Metric::L2) => 2,
        Some(Metric::Dot) => 3,
        _ => 0,
    }
}

fn metric_from(code: u16) -> Option<Metric> {
    match code {
        1 => Some(Metric::Cosine),
        2 => Some(Metric::L2),
        3 => Some(Metric::Dot),
        _ => None,
    }
}
