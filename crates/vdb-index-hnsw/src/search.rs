//! Answering a query against a built graph.

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

use vdb_core::error::Result;
use vdb_core::index::SearchCtx;
use vdb_core::search::TopK;

use crate::score::GraphScorer;

use crate::build::{search_layer, Visited};
use crate::graph::Graph;
use crate::params::HnswParams;

/// How many nodes are scored between budget checks.
///
/// The same idea as the exact scan's stride: checking a cancellation flag per comparison costs
/// more than the comparison.
const BUDGET_STRIDE: u64 = 256;

/// Run a query.
///
/// `graph` is `None` only if the lock could not be taken, which cannot happen given the recovery
/// in [`HnswIndex::read`](crate::HnswIndex); it is threaded through as an `Option` so a future
/// change there cannot turn into a panic here.
pub(crate) fn search(
    params: &HnswParams,
    graph: Option<&Graph>,
    ctx: &SearchCtx<'_>,
    out: &mut TopK,
) -> Result<()> {
    if ctx.top_k == 0 {
        return Ok(());
    }
    let Some(graph) = graph else {
        return fallback(ctx, out);
    };
    // An empty or stale graph is not an error: `prepare` may not have run, or the source may
    // have changed under a concurrent search. Scanning gives the right answer either way, which
    // is what matters — being approximate is a licence to return slightly worse results, not
    // wrong ones.
    if graph.len() == 0 || graph.metric != Some(ctx.metric) || graph.dimension != ctx.query.len() {
        return fallback(ctx, out);
    }
    let Some(entry) = graph.entry else {
        return fallback(ctx, out);
    };

    let scorer = GraphScorer::new(ctx.metric, ctx.query);

    // Descend from the top layer greedily: each layer above zero is a coarse map that gets the
    // search into the right neighbourhood cheaply.
    let mut current = entry;
    let mut current_score = scorer.score(graph.vector(current), graph.inv_norms[current as usize]);
    let mut charged = 0u64;
    for layer in (1..graph.layers.len()).rev() {
        loop {
            let mut improved = false;
            for &n in graph.neighbours(layer, current) {
                let s = scorer.score(graph.vector(n), graph.inv_norms[n as usize]);
                charged += 1;
                if s > current_score {
                    current = n;
                    current_score = s;
                    improved = true;
                }
            }
            if charged >= BUDGET_STRIDE {
                ctx.budget.charge(charged)?;
                charged = 0;
            }
            if !improved {
                break;
            }
        }
    }

    // Then the real search at layer 0.
    //
    // `ef` is raised to at least `top_k`: a beam narrower than the number of results requested
    // cannot fill it, and a caller asking for 100 results with the default ef of 64 would
    // otherwise get 64 and no explanation.
    let ef = ctx
        .params
        .ef_search
        .unwrap_or(params.ef_search)
        .max(ctx.top_k);
    // A filter rejects some of what the beam finds, so the beam has to be wider to still return
    // `top_k` results. This widening is bounded: an unbounded one would turn a selective filter
    // into a full scan with extra steps, and the engine can already do a full scan properly.
    let ef = if ctx.filter.is_some() {
        ef.saturating_mul(4).min(graph.len().max(1))
    } else {
        ef
    };

    let mut visited = Visited::default();
    let found = search_layer(graph, &scorer, &[current], 0, ef, &mut visited);
    ctx.budget.charge(charged + found.len() as u64)?;

    // Liveness and the filter are applied to what the graph *found*, not to what it traverses.
    // Refusing to traverse rejected rows would disconnect the graph, and recall would collapse
    // precisely when a filter is most selective.
    let mut admitted = Vec::with_capacity(found.len().min(ctx.top_k * 2));
    for candidate in &found {
        let Some(&row) = graph.rows.get(candidate.node as usize) else {
            continue;
        };
        if ctx.admits(row)? {
            admitted.push((row, candidate.score));
        }
    }

    // If the graph could not produce enough admitted rows, scan instead.
    //
    // This is the case where a graph index is simply the wrong tool, and it is not rare: when a
    // filter correlates with position in the vector space — a category that happens to cluster,
    // a tenant whose documents are all about one subject — the beam lands in a neighbourhood
    // containing almost nothing that qualifies. Widening the beam does not fix it, because the
    // matches are not nearby at all.
    //
    // Returning eight results when ten exist is a wrong answer, not an approximate one, so the
    // exact scan takes over. It costs a scan in exactly the situation where a scan was always
    // going to be needed. `found.len() < graph.len()` avoids scanning when the beam already
    // covered everything and the shortfall is simply that fewer rows qualify.
    if admitted.len() < ctx.top_k && found.len() < graph.len() {
        return fallback(ctx, out);
    }

    for (row, score) in admitted {
        out.offer(row, score);
    }
    Ok(())
}

/// Answer exactly, by scanning.
///
/// Used when there is no usable graph, and when the graph could not produce enough rows that
/// pass the filter. It is slower but never wrong, which is the correct way round for a fallback:
/// a missing or unhelpful index should cost time, not results.
fn fallback(ctx: &SearchCtx<'_>, out: &mut TopK) -> Result<()> {
    use vdb_core::index::VectorIndex;
    vdb_core::index::ExactScan::new().search(ctx, out)
}
