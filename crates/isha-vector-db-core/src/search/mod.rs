//! Scoring and selection: the parts of search that no index gets to reimplement.

pub mod metric;
pub mod topk;

pub use metric::{distance_from_score, inv_norm, Metric, Scorer};
pub use topk::{Candidate, TopK};
