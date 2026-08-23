//! A dense bitmap with a maintained population count.
//!
//! This is the live/tombstone set. Deletes clear a bit; a scan tests one. The population count
//! is maintained incrementally because `count()` is called on every `Collection::count()` and
//! every search that needs to size a result buffer, and a full popcount scan there would be
//! O(rows) for a number we already know.
//!
//! Dense rather than compressed (roaring) on purpose: at one bit per row a million documents
//! costs 125 KB, the access pattern in a brute-force scan is sequential, and a compressed
//! representation would add a decode step to the hottest loop in the engine. Compression can
//! come later for collections that are mostly deleted, behind the same API.

/// A fixed-length bit set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    words: Vec<u64>,
    len: usize,
    ones: usize,
}

const BITS: usize = 64;

impl Bitmap {
    /// A bitmap of `len` bits, all clear.
    pub fn new(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(BITS)],
            len,
            ones: 0,
        }
    }

    /// A bitmap of `len` bits, all set. The usual starting state for a freshly written segment.
    pub fn all_set(len: usize) -> Self {
        let mut b = Self {
            words: vec![u64::MAX; len.div_ceil(BITS)],
            len,
            ones: len,
        };
        b.clear_tail();
        b
    }

    /// Number of bits.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the bitmap holds no bits at all.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of set bits.
    pub fn count(&self) -> usize {
        self.ones
    }

    /// Whether bit `i` is set. Out-of-range indices read as clear rather than panicking:
    /// a stale row id from a snapshot taken before a truncation must not take the process down.
    pub fn get(&self, i: usize) -> bool {
        if i >= self.len {
            return false;
        }
        match self.words.get(i / BITS) {
            Some(w) => w & (1u64 << (i % BITS)) != 0,
            None => false,
        }
    }

    /// Set bit `i`, returning whether it changed. Out-of-range indices are ignored.
    pub fn set(&mut self, i: usize) -> bool {
        self.assign(i, true)
    }

    /// Clear bit `i`, returning whether it changed. Out-of-range indices are ignored.
    pub fn clear(&mut self, i: usize) -> bool {
        self.assign(i, false)
    }

    fn assign(&mut self, i: usize, value: bool) -> bool {
        if i >= self.len {
            return false;
        }
        let Some(word) = self.words.get_mut(i / BITS) else {
            return false;
        };
        let mask = 1u64 << (i % BITS);
        let was = *word & mask != 0;
        if was == value {
            return false;
        }
        if value {
            *word |= mask;
            self.ones += 1;
        } else {
            *word &= !mask;
            self.ones -= 1;
        }
        true
    }

    /// Grow to `new_len` bits, with the new bits clear. Shrinking is not supported.
    pub fn grow(&mut self, new_len: usize) {
        if new_len <= self.len {
            return;
        }
        self.words.resize(new_len.div_ceil(BITS), 0);
        self.len = new_len;
    }

    /// Clear every bit.
    pub fn clear_all(&mut self) {
        self.words.fill(0);
        self.ones = 0;
    }

    /// Iterate the indices of set bits, in ascending order.
    ///
    /// Word-at-a-time with `trailing_zeros`, so a mostly-deleted collection skips whole 64-row
    /// runs instead of testing each row.
    pub fn iter_set(&self) -> SetBits<'_> {
        SetBits {
            words: &self.words,
            word_idx: 0,
            current: self.words.first().copied().unwrap_or(0),
        }
    }

    /// The raw words, for serialization. Always little-endian on disk.
    pub fn as_words(&self) -> &[u64] {
        &self.words
    }

    /// Rebuild from raw words, e.g. after reading a `.del` file.
    ///
    /// Bits beyond `len` in the final word are cleared rather than trusted, so a corrupt tail
    /// cannot inflate the population count.
    pub fn from_words(words: Vec<u64>, len: usize) -> Self {
        let mut b = Self {
            words,
            len,
            ones: 0,
        };
        b.words.resize(len.div_ceil(BITS), 0);
        b.clear_tail();
        b.ones = b.words.iter().map(|w| w.count_ones() as usize).sum();
        b
    }

    /// Zero the unused high bits of the final word, keeping `count()` honest.
    fn clear_tail(&mut self) {
        let used = self.len % BITS;
        if used != 0 {
            if let Some(last) = self.words.last_mut() {
                *last &= (1u64 << used) - 1;
            }
        }
    }
}

/// Iterator over the indices of set bits. See [`Bitmap::iter_set`].
#[derive(Debug)]
pub struct SetBits<'a> {
    words: &'a [u64],
    word_idx: usize,
    current: u64,
}

impl Iterator for SetBits<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        loop {
            if self.current != 0 {
                let bit = self.current.trailing_zeros() as usize;
                self.current &= self.current - 1;
                return Some(self.word_idx * BITS + bit);
            }
            self.word_idx += 1;
            self.current = *self.words.get(self.word_idx)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_empty_and_all_set_is_full() {
        let b = Bitmap::new(100);
        assert_eq!(b.len(), 100);
        assert_eq!(b.count(), 0);
        assert!(!b.get(0));

        let b = Bitmap::all_set(100);
        assert_eq!(b.count(), 100);
        assert!(b.get(99));
        assert!(!b.get(100));
    }

    /// The tail bits of the final word must not be counted, or `count()` over-reports.
    #[test]
    fn all_set_ignores_bits_past_the_end() {
        for len in [0, 1, 63, 64, 65, 127, 128, 129] {
            let b = Bitmap::all_set(len);
            assert_eq!(b.count(), len, "len {len}");
            assert_eq!(b.iter_set().count(), len, "len {len}");
        }
    }

    #[test]
    fn set_and_clear_track_the_population_count() {
        let mut b = Bitmap::new(200);
        assert!(b.set(5));
        assert!(!b.set(5), "setting twice should report no change");
        assert_eq!(b.count(), 1);
        assert!(b.set(199));
        assert_eq!(b.count(), 2);
        assert!(b.clear(5));
        assert!(!b.clear(5));
        assert_eq!(b.count(), 1);
        assert!(b.get(199));
    }

    /// A stale row id from an older snapshot must be inert, not fatal.
    #[test]
    fn out_of_range_access_is_inert() {
        let mut b = Bitmap::new(10);
        assert!(!b.get(10));
        assert!(!b.get(usize::MAX));
        assert!(!b.set(10));
        assert!(!b.clear(10));
        assert_eq!(b.count(), 0);
    }

    #[test]
    fn iter_set_yields_ascending_indices() {
        let mut b = Bitmap::new(300);
        let expected = [0usize, 1, 63, 64, 65, 127, 200, 299];
        for &i in &expected {
            b.set(i);
        }
        let got: Vec<usize> = b.iter_set().collect();
        assert_eq!(got, expected);
        assert_eq!(b.count(), expected.len());
    }

    #[test]
    fn iter_set_on_empty_and_zero_length() {
        assert_eq!(Bitmap::new(0).iter_set().count(), 0);
        assert_eq!(Bitmap::new(1000).iter_set().count(), 0);
    }

    #[test]
    fn grow_preserves_bits_and_adds_clear_ones() {
        let mut b = Bitmap::all_set(10);
        b.grow(100);
        assert_eq!(b.len(), 100);
        assert_eq!(b.count(), 10);
        assert!(b.get(9));
        assert!(!b.get(10));
        b.grow(5); // shrinking is a no-op
        assert_eq!(b.len(), 100);
    }

    #[test]
    fn word_round_trip_preserves_everything() {
        let mut b = Bitmap::new(150);
        for i in (0..150).step_by(7) {
            b.set(i);
        }
        let restored = Bitmap::from_words(b.as_words().to_vec(), b.len());
        assert_eq!(restored, b);
        assert_eq!(restored.count(), b.count());
    }

    /// A corrupt `.del` file with junk in the tail must not inflate the count.
    #[test]
    fn from_words_sanitises_a_corrupt_tail() {
        let b = Bitmap::from_words(vec![u64::MAX], 10);
        assert_eq!(b.count(), 10);
        assert!(!b.get(10));
        assert_eq!(
            b.iter_set().collect::<Vec<_>>(),
            (0..10).collect::<Vec<_>>()
        );
    }

    #[test]
    fn from_words_resizes_a_short_or_long_word_vector() {
        let short = Bitmap::from_words(vec![], 130);
        assert_eq!(short.len(), 130);
        assert_eq!(short.count(), 0);

        let long = Bitmap::from_words(vec![u64::MAX; 8], 64);
        assert_eq!(long.count(), 64);
        assert_eq!(long.as_words().len(), 1);
    }

    #[test]
    fn clear_all_resets_the_count() {
        let mut b = Bitmap::all_set(70);
        b.clear_all();
        assert_eq!(b.count(), 0);
        assert_eq!(b.iter_set().count(), 0);
        assert_eq!(b.len(), 70);
    }
}
