//! Time, injected.
//!
//! The core cannot call `SystemTime::now()` — it performs no I/O and reads no ambient state, and
//! CI enforces that. Timestamps arrive through a [`Clock`] the embedder supplies, which has two
//! benefits beyond purity: every test runs against a clock it controls, so no assertion depends
//! on wall-clock timing; and a platform whose clock needs care (a browser tab throttled in the
//! background, a device whose user just changed the date) can supply one that behaves.
//!
//! Timestamps are recorded for humans, never for correctness. Ordering comes from the manifest
//! sequence and the log sequence, both monotonic counters, so a clock that jumps backwards
//! cannot corrupt anything — it only makes a `created_at` look odd.

use core::fmt::Debug;
use core::sync::atomic::{AtomicU64, Ordering};

/// A source of wall-clock time in milliseconds since the Unix epoch.
pub trait Clock: Debug + Send + Sync {
    /// The current time.
    fn now_ms(&self) -> u64;
}

/// A clock that returns whatever it was last set to.
///
/// The default in tests, so a golden fixture or an assertion on `created_at` is reproducible.
#[derive(Debug)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    /// A clock reading `now_ms`.
    pub const fn new(now_ms: u64) -> Self {
        Self(AtomicU64::new(now_ms))
    }

    /// Move the clock to an absolute time.
    pub fn set(&self, now_ms: u64) {
        self.0.store(now_ms, Ordering::SeqCst);
    }

    /// Move the clock forward.
    pub fn advance(&self, millis: u64) {
        self.0.fetch_add(millis, Ordering::SeqCst);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        // An arbitrary fixed instant, so a database created in a test has a plausible-looking
        // timestamp rather than 1970.
        Self::new(1_700_000_000_000)
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
