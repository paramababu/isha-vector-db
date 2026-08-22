//! Search requests and results.

use crate::document::{DocId, Document, Include};
use crate::index::{IndexKind, SearchParams};
use crate::search::Metric;
use crate::vector::VectorView;

/// A similarity query.
#[derive(Debug, Clone)]
pub struct SearchRequest<'a> {
    /// The query vector.
    pub vector: VectorView<'a>,
    /// How many results to return.
    pub top_k: usize,
    /// Override the collection's metric.
    ///
    /// Rarely wanted, and worth knowing the cost: the cached reciprocal norms in the row
    /// directory were computed for the collection's own metric, so an override that needs
    /// different precomputation gains nothing from them.
    pub metric: Option<Metric>,
    /// Discard results scoring below this. Inclusive.
    ///
    /// In *score* space, where higher is better — so for `L2`, whose scores are negated squared
    /// distances, a threshold of `-100.0` keeps everything within a squared distance of 100.
    pub min_score: Option<f32>,
    /// Which parts of each matching document to return.
    pub include: Include,
    /// Per-index knobs, ignored by indexes they do not apply to.
    pub params: SearchParams,
}

impl<'a> SearchRequest<'a> {
    /// A query for the `top_k` nearest documents.
    pub fn new(vector: VectorView<'a>, top_k: usize) -> Self {
        Self {
            vector,
            top_k,
            metric: None,
            min_score: None,
            include: Include::default(),
            params: SearchParams::default(),
        }
    }

    /// Score with a metric other than the collection's.
    #[must_use]
    pub fn with_metric(mut self, metric: Metric) -> Self {
        self.metric = Some(metric);
        self
    }

    /// Discard anything scoring below `min_score`.
    #[must_use]
    pub fn with_min_score(mut self, min_score: f32) -> Self {
        self.min_score = Some(min_score);
        self
    }

    /// Choose what to return for each hit.
    #[must_use]
    pub fn with_include(mut self, include: Include) -> Self {
        self.include = include;
        self
    }

    /// Set per-index knobs.
    #[must_use]
    pub fn with_params(mut self, params: SearchParams) -> Self {
        self.params = params;
        self
    }
}

/// One matching document.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Hit {
    /// The document's id.
    pub id: DocId,
    /// Its score. **Always higher-is-better**, whatever the metric.
    pub score: f32,
    /// The metric-native distance, where the metric defines one.
    ///
    /// `None` for the inner product, which is a similarity with no corresponding distance.
    /// Inventing one would be worse than admitting it.
    pub distance: Option<f32>,
    /// The document itself, as far as [`SearchRequest::include`] asked for it.
    pub document: Option<Document>,
}

/// What a search did, beyond its results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SearchStats {
    /// Which index answered.
    pub index_kind: IndexKind,
    /// Whether the results are ground truth.
    ///
    /// Surfaced so an approximate result is never mistaken for an exact one — the distinction
    /// matters enormously when someone is debugging why a document they expected is missing.
    pub exact: bool,
    /// Rows offered to the selector, including those rejected by a threshold.
    pub considered: u64,
    /// Rows the index actually examined.
    pub scanned: u64,
    /// Rows skipped because they were deleted or filtered out.
    pub skipped: u64,
}

/// The result of a search.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SearchResponse {
    /// Matches, best first; ties broken by ascending id.
    pub hits: Vec<Hit>,
    /// What the search did.
    pub stats: SearchStats,
}

impl SearchResponse {
    /// How many matches were returned.
    pub fn len(&self) -> usize {
        self.hits.len()
    }

    /// Whether nothing matched.
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    /// Just the ids, best first.
    pub fn ids(&self) -> Vec<DocId> {
        self.hits.iter().map(|h| h.id.clone()).collect()
    }
}
