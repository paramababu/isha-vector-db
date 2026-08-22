//! Opaque handles.
//!
//! Nothing about these types crosses the boundary but their address. That is what lets the
//! engine's layout change — a field added, a lock swapped — without every SDK needing a rebuild,
//! and it is why the header declares them as incomplete types.

use vdb_core::api::{Collection, Database, SearchResponse};
use vdb_core::metadata::Metadata;

use crate::Boundary;

/// Generate the raw-pointer plumbing each handle needs.
///
/// A macro because the four handles differ only in what they wrap, and hand-writing the same
/// null check four times is how one of them ends up missing it.
macro_rules! handle {
    ($name:ident, $inner:ty) => {
        /// An opaque handle. See the header for the lifetime rules.
        #[derive(Debug)]
        pub struct $name($inner);

        impl $name {
            pub(crate) fn into_raw(value: $inner) -> *mut Self {
                Box::into_raw(Box::new(Self(value)))
            }

            /// Reclaim ownership.
            ///
            /// # Safety
            /// The pointer must come from `into_raw` and must not be used again.
            #[allow(dead_code)]
            pub(crate) unsafe fn from_raw(ptr: *mut Self) -> $inner {
                // SAFETY: the caller guarantees the pointer came from `into_raw`.
                unsafe { Box::from_raw(ptr) }.0
            }

            /// Drop the handle.
            ///
            /// # Safety
            /// As `from_raw`.
            #[allow(dead_code)]
            pub(crate) unsafe fn destroy(ptr: *mut Self) {
                // SAFETY: the caller guarantees the pointer came from `into_raw`.
                drop(unsafe { Box::from_raw(ptr) });
            }

            /// Borrow the value, refusing null.
            ///
            /// # Safety
            /// A non-null pointer must be live and must have come from `into_raw`.
            pub(crate) unsafe fn borrow<'a>(ptr: *const Self) -> Result<&'a $inner, Boundary> {
                if ptr.is_null() {
                    return Err(Boundary::Null);
                }
                // SAFETY: checked non-null, and the caller guarantees it is live.
                Ok(unsafe { &(*ptr).0 })
            }

            /// Borrow the value mutably, refusing null.
            ///
            /// # Safety
            /// As `borrow`, and no other reference may be outstanding.
            #[allow(dead_code)]
            pub(crate) unsafe fn borrow_mut<'a>(
                ptr: *mut Self,
            ) -> Result<&'a mut $inner, Boundary> {
                if ptr.is_null() {
                    return Err(Boundary::Null);
                }
                // SAFETY: checked non-null; the caller guarantees exclusivity.
                Ok(unsafe { &mut (*ptr).0 })
            }
        }
    };
}

handle!(VdbDb, Database);
handle!(VdbCollection, Collection);
handle!(VdbMetadata, Metadata);

/// A search result, with its ids materialised so they can be borrowed by pointer.
///
/// Not built from the macro because it needs a second field. Ids are rendered into owned buffers
/// at construction rather than on access: a string id could be borrowed straight from the hit,
/// but an integer id has no stable buffer to point at, and having one kind return a borrowed
/// pointer while the other returns a cached one is the sort of asymmetry that produces a
/// use-after-free in exactly one binding. Uniform is cheaper than clever.
#[derive(Debug)]
pub struct VdbResults {
    response: SearchResponse,
    ids: Vec<Vec<u8>>,
}

impl VdbResults {
    pub(crate) fn into_raw(response: SearchResponse) -> *mut Self {
        let ids = response.hits.iter().map(|h| h.id.to_bytes()).collect();
        Box::into_raw(Box::new(Self { response, ids }))
    }

    /// Drop the handle.
    ///
    /// # Safety
    /// The pointer must come from `into_raw` and must not be used again.
    pub(crate) unsafe fn destroy(ptr: *mut Self) {
        // SAFETY: the caller guarantees the pointer came from `into_raw`.
        drop(unsafe { Box::from_raw(ptr) });
    }
    /// Hits held.
    pub(crate) fn len(&self) -> usize {
        self.response.hits.len()
    }

    /// A hit's score, or 0 for an out-of-range index.
    ///
    /// Out of range returns a value rather than failing, because these accessors are called in a
    /// loop by binding code and an error path per element would be all cost and no benefit — the
    /// caller has already been told how many there are.
    pub(crate) fn score(&self, index: usize) -> f32 {
        self.response.hits.get(index).map_or(0.0, |h| h.score)
    }

    /// A hit's id bytes, borrowed from the result.
    ///
    /// The bytes live in the `SearchResponse` this handle owns, so they stay valid exactly as
    /// long as the handle does — which is what the header promises.
    pub(crate) fn id(&self, index: usize) -> (*const u8, usize) {
        match self.ids.get(index) {
            Some(bytes) => (bytes.as_ptr(), bytes.len()),
            None => (std::ptr::null(), 0),
        }
    }
}
