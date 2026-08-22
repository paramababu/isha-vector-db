//! Graph construction.

// Indices into the graph's own arrays are used directly rather than through `get`.
//
// This crate is not in the same risk class as `vdb-format`, which parses bytes an attacker may
// have written and must never panic on them. Every index here is a node number this crate
// allocated, into a vector this crate sized, and the invariant — every per-node array has
// exactly one entry per node, on every layer — is asserted in `Graph::push_node` under
// `debug_assert`. Node numbers that arrive from outside, such as a neighbour read during
// traversal, are still bounds-checked at the point they enter.
//
// The alternative is a fallible lookup in the innermost loop of a search that runs tens of
// millions of times per build, written to handle a case that would be a bug in this file.
#![allow(clippy::indexing_slicing)]

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use vdb_core::error::Result;
use vdb_core::index::VectorSource;
use vdb_core::search::Metric;

use crate::score::GraphScorer;

use crate::graph::{level_for, Graph, Node};
use crate::params::HnswParams;

/// Which nodes a traversal has already seen.
///
/// A `Vec<bool>` allocated per call is the obvious implementation and was the largest single
/// cost in building a graph: `search_layer` runs once per node per layer, so a 50,000-node build
/// allocated and zeroed a 50 KB array 50,000 times. Measured on a 50,000 x 384 corpus, replacing
/// it took the build from 185s to 95s and the query from 913us to 476us — queries benefit too,
/// because a search allocated the same array on every call.
///
/// Stamping with a generation counter instead means the buffer is allocated once and reset by
/// incrementing an integer.
#[derive(Debug, Default)]
pub(crate) struct Visited {
    stamp: Vec<u32>,
    generation: u32,
}

impl Visited {
    /// Start a fresh traversal over `len` nodes.
    pub(crate) fn begin(&mut self, len: usize) {
        if self.stamp.len() < len {
            self.stamp.resize(len, 0);
        }
        // On wraparound every stale stamp could collide with the new generation, so the buffer is
        // cleared once — every four billion traversals — rather than risk a node being treated as
        // already visited and silently skipped.
        self.generation = match self.generation.checked_add(1) {
            Some(next) => next,
            None => {
                self.stamp.iter_mut().for_each(|s| *s = 0);
                1
            }
        };
    }

    /// Mark `node` seen, returning whether it had already been seen this traversal.
    pub(crate) fn seen(&mut self, node: usize) -> bool {
        match self.stamp.get_mut(node) {
            Some(slot) if *slot == self.generation => true,
            Some(slot) => {
                *slot = self.generation;
                false
            }
            None => true,
        }
    }
}

/// A candidate ordered by score, highest first.
///
/// Every score in this crate is higher-is-better, matching the engine's contract, so "nearest"
/// means "greatest score" throughout. Getting this backwards is the classic HNSW bug: the graph
/// still builds, the search still runs, and recall is quietly terrible.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Candidate {
    pub(crate) score: f32,
    pub(crate) node: Node,
}

impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // Ties break on node index so the ordering is total and the result deterministic. A
        // partial order here would make `BinaryHeap` produce a different graph on different
        // runs from identical input.
        self.score
            .partial_cmp(&other.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| other.node.cmp(&self.node))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// `Candidate` inverted, for a min-heap of the worst-so-far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Worst(pub(crate) Candidate);

impl Ord for Worst {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.cmp(&self.0)
    }
}

impl PartialOrd for Worst {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Build a graph over every row the source holds.
///
/// Rows are taken in the source's own stable order, which together with the hashed level
/// assignment makes the whole structure a pure function of the data.
pub(crate) fn build(
    source: &dyn VectorSource,
    metric: Metric,
    params: &HnswParams,
) -> Result<Graph> {
    let dimension = source.dimension() as usize;
    let mut graph = Graph {
        dimension,
        metric: Some(metric),
        ..Graph::default()
    };
    graph.vectors.reserve(source.len() * dimension);

    let mut visited = Visited::default();
    let mut pending: Vec<(vdb_core::document::RowId, Vec<f32>, f32)> =
        Vec::with_capacity(source.len());
    source.for_each(&mut |row, bytes, inv_norm| {
        pending.push((row, decode(bytes, dimension), inv_norm));
        Ok(())
    })?;

    for (row, vector, inv_norm) in pending {
        let level = level_for(row, params);
        let node = graph.push_node(row, &vector, inv_norm, level);
        insert(&mut graph, node, metric, params, &mut visited);
    }
    Ok(graph)
}

/// Decode a row's raw bytes into floats.
fn decode(bytes: &[u8], dimension: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(dimension);
    for chunk in bytes.chunks_exact(4) {
        // `chunks_exact` yields exactly four bytes, so the conversion cannot fail.
        let arr: [u8; 4] = chunk.try_into().unwrap_or([0; 4]);
        out.push(f32::from_le_bytes(arr));
    }
    out.resize(dimension, 0.0);
    out
}

/// Link a newly pushed node into the graph.
fn insert(
    graph: &mut Graph,
    node: Node,
    metric: Metric,
    params: &HnswParams,
    visited: &mut Visited,
) {
    let Some(entry) = graph.entry else {
        graph.entry = Some(node);
        return;
    };

    let level = graph.levels[node as usize] as usize;
    let entry_level = graph.levels[entry as usize] as usize;
    let query = graph.vector(node).to_vec();
    let scorer = GraphScorer::new(metric, &query);

    // Descend the layers this node does not belong to, greedily, to find a good entry point.
    let mut current = entry;
    let mut current_score = scorer.score(graph.vector(current), graph.inv_norms[current as usize]);
    for layer in (level + 1..=entry_level).rev() {
        loop {
            let mut improved = false;
            for &n in graph.neighbours(layer, current) {
                let s = scorer.score(graph.vector(n), graph.inv_norms[n as usize]);
                if s > current_score {
                    current = n;
                    current_score = s;
                    improved = true;
                }
            }
            if !improved {
                break;
            }
        }
    }

    // Then connect at every layer this node does belong to.
    //
    // The whole found set is carried into the next layer down, not just its best member. A
    // single entry point makes each layer's search a narrow funnel: one unlucky greedy step near
    // the top costs candidates all the way to the bottom, and the graph ends up stitched
    // together from whatever that one path happened to see.
    let mut entries = vec![current];
    for layer in (0..=level.min(entry_level)).rev() {
        let found = search_layer(
            graph,
            &scorer,
            &entries,
            layer,
            params.ef_construction,
            visited,
        );
        let max = if layer == 0 { params.m0() } else { params.m };
        let chosen = select_neighbours(graph, &found, max, metric);

        for &neighbour in &chosen {
            connect(graph, layer, node, neighbour);
            connect(graph, layer, neighbour, node);
            // Adding an edge can push a neighbour over its degree cap, so it is pruned back
            // immediately. Letting degrees grow unbounded is what turns a graph search into a
            // scan.
            prune(graph, layer, neighbour, max, metric);
        }
        if !found.is_empty() {
            entries = found.iter().map(|c| c.node).collect();
        }
    }

    if level > entry_level {
        graph.entry = Some(node);
    }
}

fn connect(graph: &mut Graph, layer: usize, from: Node, to: Node) {
    if from == to {
        return;
    }
    if let Some(list) = graph
        .layers
        .get_mut(layer)
        .and_then(|l| l.get_mut(from as usize))
    {
        if !list.contains(&to) {
            list.push(to);
        }
    }
}

/// Trim a node's neighbour list back to `max`.
///
/// This uses the same diversity heuristic as [`select_neighbours`], and that is not a detail.
/// Keeping simply the `max` closest neighbours looks obviously right and fragments the graph:
/// on clustered data every node's nearest neighbours are all inside its own cluster, so the
/// links that bridge clusters are exactly the ones truncation discards. Measured here, plain
/// proximity pruning left 167 of 2000 nodes reachable from the entry point and recall at 0.46,
/// with every node still showing a full complement of 32 neighbours — the graph looked healthy
/// by every count and was not.
fn prune(graph: &mut Graph, layer: usize, node: Node, max: usize, metric: Metric) {
    if graph.neighbours(layer, node).len() <= max {
        return;
    }
    let query = graph.vector(node).to_vec();
    let scorer = GraphScorer::new(metric, &query);
    let mut scored: Vec<Candidate> = graph
        .neighbours(layer, node)
        .iter()
        .map(|&n| Candidate {
            score: scorer.score(graph.vector(n), graph.inv_norms[n as usize]),
            node: n,
        })
        .collect();
    scored.sort_by(|a, b| b.cmp(a));
    let kept = select_neighbours(graph, &scored, max, metric);
    if let Some(list) = graph
        .layers
        .get_mut(layer)
        .and_then(|l| l.get_mut(node as usize))
    {
        *list = kept;
    }
}

/// Pick which of `candidates` to keep as neighbours.
///
/// The paper's heuristic, not a plain top-`max`: a candidate is kept only if it is closer to the
/// new node than to any already-kept neighbour. That drops candidates clustered in one direction
/// in favour of ones that open up new parts of the space, which is what keeps the graph navigable
/// rather than merely dense.
fn select_neighbours(
    graph: &Graph,
    candidates: &[Candidate],
    max: usize,
    metric: Metric,
) -> Vec<Node> {
    let mut kept: Vec<Node> = Vec::with_capacity(max);
    for candidate in candidates {
        if kept.len() >= max {
            break;
        }
        // Borrowed, not copied. This runs once per candidate for every node inserted, so a
        // `to_vec` here allocates a whole vector per comparison; removing it took the build of a
        // 5,000-vector corpus from 5.32s to 4.49s.
        let to_candidate = GraphScorer::new(metric, graph.vector(candidate.node));
        let dominated = kept.iter().any(|&k| {
            to_candidate.score(graph.vector(k), graph.inv_norms[k as usize]) > candidate.score
        });
        if !dominated {
            kept.push(candidate.node);
        }
    }
    // If the heuristic was too strict to fill the quota, top up by plain proximity rather than
    // leaving a node under-connected.
    if kept.len() < max {
        for candidate in candidates {
            if kept.len() >= max {
                break;
            }
            if !kept.contains(&candidate.node) {
                kept.push(candidate.node);
            }
        }
    }
    kept
}

/// The beam search one layer, returning up to `ef` candidates, best first.
pub(crate) fn search_layer(
    graph: &Graph,
    scorer: &GraphScorer<'_>,
    entries: &[Node],
    layer: usize,
    ef: usize,
    visited: &mut Visited,
) -> Vec<Candidate> {
    visited.begin(graph.len());
    // Frontier: best-first, what to expand next. Results: a min-heap of the best `ef` so far,
    // so the weakest is cheap to find and evict.
    let mut frontier: BinaryHeap<Candidate> = BinaryHeap::new();
    let mut results: BinaryHeap<Worst> = BinaryHeap::new();

    for &entry in entries {
        if entry as usize >= graph.len() || visited.seen(entry as usize) {
            continue;
        }
        let c = Candidate {
            score: scorer.score(graph.vector(entry), graph.inv_norms[entry as usize]),
            node: entry,
        };
        frontier.push(c);
        results.push(Worst(c));
    }

    while let Some(current) = frontier.pop() {
        // Everything left in the frontier is worse than the worst kept result, so no expansion
        // can improve the answer. This is the check that makes the search sublinear.
        if let Some(Worst(worst)) = results.peek() {
            if results.len() >= ef && current.score < worst.score {
                break;
            }
        }
        for &n in graph.neighbours(layer, current.node) {
            let i = n as usize;
            if i >= graph.len() || visited.seen(i) {
                continue;
            }
            let c = Candidate {
                score: scorer.score(graph.vector(n), graph.inv_norms[i]),
                node: n,
            };
            let worst_kept = results.peek().map(|w| w.0.score);
            if results.len() < ef || worst_kept.is_some_and(|w| c.score > w) {
                frontier.push(c);
                results.push(Worst(c));
                if results.len() > ef {
                    results.pop();
                }
            }
        }
    }

    let mut out: Vec<Candidate> = results.into_iter().map(|w| w.0).collect();
    out.sort_by(|a, b| b.cmp(a));
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod diagnostics {
    use super::*;
    use vdb_core::document::RowId;
    use vdb_core::index::RowVisitor;
    use vdb_core::search::inv_norm;

    #[derive(Debug)]
    struct Rows {
        dimension: u32,
        rows: Vec<(RowId, Vec<u8>, f32)>,
    }

    impl VectorSource for Rows {
        fn dimension(&self) -> u32 {
            self.dimension
        }
        fn len(&self) -> usize {
            self.rows.len()
        }
        fn for_each(&self, visit: &mut RowVisitor<'_>) -> Result<()> {
            for (row, bytes, norm) in &self.rows {
                visit(*row, bytes, *norm)?;
            }
            Ok(())
        }
        fn vector(&self, row: RowId) -> Option<(&[u8], f32)> {
            self.rows
                .iter()
                .find(|(r, _, _)| *r == row)
                .map(|(_, b, n)| (b.as_slice(), *n))
        }
    }

    /// Every node must be reachable from the entry point by following layer-0 edges.
    ///
    /// This is the test that would have caught the worst bug in this crate. Pruning a node's
    /// neighbours down to the closest `max` looks obviously correct, and leaves every count
    /// looking healthy — full degree on every node, the right number of layers, the right
    /// population per layer. What it silently destroys is the links *between* clusters, because
    /// on clustered data a node's nearest neighbours are all its own cluster-mates. The graph
    /// fragmented into islands, 167 of 2000 nodes were reachable, and the only outward symptom
    /// was mediocre recall — which is exactly the symptom everyone attributes to tuning.
    #[test]
    fn every_node_is_reachable_from_the_entry_point() {
        let n = 2000;
        let d = 64;
        let mut seed = 12345u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as f32 / (1u32 << 31) as f32) - 0.5
        };
        // Clustered, deliberately: uniform noise hides this failure completely, because in high
        // dimensions every point is roughly equidistant and there are no bridges to sever.
        let centres: Vec<Vec<f32>> = (0..12)
            .map(|_| (0..d).map(|_| next() * 2.0).collect())
            .collect();
        let rows: Vec<(RowId, Vec<u8>, f32)> = (0..n)
            .map(|i| {
                let c: &Vec<f32> = &centres[i % 12];
                let v: Vec<f32> = c.iter().map(|x| x + next() * 0.35).collect();
                let bytes = v.iter().flat_map(|x| x.to_le_bytes()).collect();
                (RowId::new(0, i as u32), bytes, inv_norm(&v))
            })
            .collect();
        let source = Rows {
            dimension: d as u32,
            rows,
        };
        let g = build(&source, Metric::Cosine, &HnswParams::default()).unwrap();

        let entry = g.entry.expect("a non-empty graph has an entry point");
        let mut seen = vec![false; g.len()];
        seen[entry as usize] = true;
        let mut stack = vec![entry];
        let mut reached = 0usize;
        while let Some(node) = stack.pop() {
            reached += 1;
            for &m in g.neighbours(0, node) {
                if !seen[m as usize] {
                    seen[m as usize] = true;
                    stack.push(m);
                }
            }
        }
        assert_eq!(
            reached,
            g.len(),
            "only {reached} of {} nodes are reachable from the entry point; the graph has \
             fragmented and recall will be capped no matter how wide the beam",
            g.len()
        );
    }

    /// A full-width beam must give exactly the brute-force answer.
    ///
    /// Separates "the graph is badly built" from "the search is wrong": with the beam covering
    /// every node, traversal quality cannot matter, so any disagreement here is a scoring or
    /// bookkeeping bug rather than a tuning problem.
    #[test]
    fn a_full_width_beam_matches_brute_force() {
        let d = 16;
        let n = 300;
        let mut seed = 999u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as f32 / (1u32 << 31) as f32) - 0.5
        };
        let rows: Vec<(RowId, Vec<u8>, f32)> = (0..n)
            .map(|i| {
                let v: Vec<f32> = (0..d).map(|_| next()).collect();
                let bytes = v.iter().flat_map(|x| x.to_le_bytes()).collect();
                (RowId::new(0, i as u32), bytes, inv_norm(&v))
            })
            .collect();
        let source = Rows {
            dimension: d as u32,
            rows,
        };

        for metric in [Metric::Cosine, Metric::L2, Metric::Dot] {
            let g = build(&source, metric, &HnswParams::default()).unwrap();
            let query = g.vector(3).to_vec();
            let scorer = GraphScorer::new(metric, &query);
            let found = search_layer(
                &g,
                &scorer,
                &[g.entry.unwrap()],
                0,
                g.len(),
                &mut Visited::default(),
            );

            let mut brute: Vec<Candidate> = (0..g.len() as u32)
                .map(|node| Candidate {
                    score: scorer.score(g.vector(node), g.inv_norms[node as usize]),
                    node,
                })
                .collect();
            brute.sort_by(|a, b| b.cmp(a));

            let got: Vec<u32> = found.iter().take(10).map(|c| c.node).collect();
            let want: Vec<u32> = brute.iter().take(10).map(|c| c.node).collect();
            assert_eq!(got, want, "{metric:?}");
        }
    }
}
