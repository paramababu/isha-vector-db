//! Timing, percentiles, and honest reporting.

use std::time::{Duration, Instant};

/// A completed measurement.
#[derive(Debug, Clone)]
pub(crate) struct Measurement {
    pub name: String,
    /// What the number counts — documents, queries, bytes.
    pub unit: &'static str,
    /// How many of them.
    pub count: u64,
    /// Total wall time.
    pub total: Duration,
    /// Per-operation latencies, when the workload measured them individually.
    pub latencies: Vec<Duration>,
    /// Extra facts worth recording alongside the timing.
    pub notes: Vec<(String, String)>,
}

impl Measurement {
    pub(crate) fn new(name: impl Into<String>, unit: &'static str) -> Self {
        Self {
            name: name.into(),
            unit,
            count: 0,
            total: Duration::ZERO,
            latencies: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Operations per second, or `None` when the run was too short to divide by.
    pub(crate) fn throughput(&self) -> Option<f64> {
        let secs = self.total.as_secs_f64();
        if secs > 0.0 && self.count > 0 {
            Some(self.count as f64 / secs)
        } else {
            None
        }
    }

    /// A latency percentile, `p` in 0..=100.
    ///
    /// Percentiles rather than a mean, because a mean hides exactly the thing that matters for
    /// an interactive application: the slow tail. A search averaging 5 ms but spending 80 ms at
    /// the 99th percentile drops frames, and the average will never say so.
    ///
    /// **Nearest-rank, inclusive**: the smallest sample at or above which `p` percent of samples
    /// fall, `index = ceil(p/100 × n) − 1`. Stated because percentile conventions genuinely
    /// differ — nearest-rank, linear interpolation and the `(n−1)`-scaled variant give three
    /// different answers for the same data — and a benchmark whose convention is undocumented
    /// cannot be compared with anything, including its own past runs after someone tidies it.
    pub(crate) fn percentile(&self, p: f64) -> Option<Duration> {
        if self.latencies.is_empty() {
            return None;
        }
        let mut sorted = self.latencies.clone();
        sorted.sort_unstable();
        let n = sorted.len();
        let rank = (p / 100.0 * n as f64).ceil().max(1.0) as usize;
        sorted.get(rank.min(n) - 1).copied()
    }

    pub(crate) fn note(
        &mut self,
        key: impl Into<String>,
        value: impl std::fmt::Display,
    ) -> &mut Self {
        self.notes.push((key.into(), value.to_string()));
        self
    }
}

/// Time a whole workload as one figure.
pub(crate) fn timed<T>(
    name: impl Into<String>,
    unit: &'static str,
    count: u64,
    body: impl FnOnce() -> T,
) -> (Measurement, T) {
    let start = Instant::now();
    let value = body();
    let mut m = Measurement::new(name, unit);
    m.count = count;
    m.total = start.elapsed();
    (m, value)
}

/// Time each iteration separately, so the distribution is visible.
pub(crate) fn sampled(
    name: impl Into<String>,
    unit: &'static str,
    iterations: usize,
    mut body: impl FnMut(usize),
) -> Measurement {
    // A short warm-up, so the first samples are not measuring page faults and cold caches. Not
    // hidden: it is recorded in the output, because a warm-up changes what the number means.
    let warmup = (iterations / 20).clamp(1, 50);
    for i in 0..warmup {
        body(i);
    }

    let mut latencies = Vec::with_capacity(iterations);
    let start = Instant::now();
    for i in 0..iterations {
        let t = Instant::now();
        body(i);
        latencies.push(t.elapsed());
    }
    let total = start.elapsed();

    let mut m = Measurement::new(name, unit);
    m.count = iterations as u64;
    m.total = total;
    m.latencies = latencies;
    m.note("warmup_iterations", warmup);
    m
}

/// Peak resident memory for this process, in bytes.
///
/// `ru_maxrss` is a **peak**, not a current reading, and it never goes down — so it is
/// meaningful for "how much did this workload need at its worst" and meaningless for "how much
/// is in use now". Reported as such.
///
/// The unit differs by platform, which is a classic source of results that are wrong by a
/// factor of 1024: Linux reports kilobytes, Darwin reports bytes.
pub(crate) fn peak_rss_bytes() -> Option<u64> {
    #[cfg(unix)]
    {
        // SAFETY: `getrusage` writes into a fully-initialised struct we own, and RUSAGE_SELF is
        // a valid `who` value.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
        if rc != 0 {
            return None;
        }
        let raw = usage.ru_maxrss as u64;
        if cfg!(target_vendor = "apple") {
            Some(raw)
        } else {
            Some(raw.saturating_mul(1024))
        }
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Total bytes on disk under a directory.
pub(crate) fn directory_bytes(path: &std::path::Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total += directory_bytes(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_pick_the_right_samples() {
        let mut m = Measurement::new("t", "op");
        m.latencies = (1..=100).map(Duration::from_millis).collect();
        // Nearest-rank, inclusive: index = ceil(p/100 × n) − 1.
        assert_eq!(m.percentile(0.0), Some(Duration::from_millis(1)));
        assert_eq!(m.percentile(50.0), Some(Duration::from_millis(50)));
        assert_eq!(m.percentile(95.0), Some(Duration::from_millis(95)));
        assert_eq!(m.percentile(99.0), Some(Duration::from_millis(99)));
        assert_eq!(m.percentile(100.0), Some(Duration::from_millis(100)));
    }

    #[test]
    fn percentiles_are_sane_for_tiny_samples() {
        let mut m = Measurement::new("t", "op");
        m.latencies = vec![Duration::from_millis(7)];
        for p in [0.0, 50.0, 99.0, 100.0] {
            assert_eq!(m.percentile(p), Some(Duration::from_millis(7)), "p{p}");
        }
        m.latencies = vec![Duration::from_millis(1), Duration::from_millis(2)];
        assert_eq!(m.percentile(50.0), Some(Duration::from_millis(1)));
        assert_eq!(m.percentile(51.0), Some(Duration::from_millis(2)));
    }

    #[test]
    fn a_measurement_with_no_samples_has_no_percentiles() {
        assert_eq!(Measurement::new("t", "op").percentile(50.0), None);
        assert_eq!(Measurement::new("t", "op").throughput(), None);
    }

    #[test]
    fn throughput_needs_both_a_count_and_a_duration() {
        let mut m = Measurement::new("t", "doc");
        m.count = 1000;
        m.total = Duration::from_secs(2);
        assert_eq!(m.throughput(), Some(500.0));
    }

    /// On unix `getrusage` should always answer; elsewhere `None` is the honest result and there
    /// is nothing to assert.
    #[cfg(unix)]
    #[test]
    fn peak_rss_is_reported_in_the_right_unit() {
        let bytes = peak_rss_bytes().expect("getrusage should work on unix");
        // A running test process uses more than a megabyte and less than a terabyte. The bound
        // is loose on purpose: its job is to catch the platform unit being wrong by 1024x, which
        // is the mistake this function exists to avoid.
        assert!(
            bytes > 1_000_000,
            "suspiciously small: {bytes} — kilobytes read as bytes?"
        );
        assert!(
            bytes < 1 << 40,
            "suspiciously large: {bytes} — bytes read as kilobytes?"
        );
    }
}
