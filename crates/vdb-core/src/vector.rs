//! Vector representation.
//!
//! A [`VectorView`] borrows its data rather than owning it: nothing is copied until the bytes
//! reach the write-ahead log. That matters most at the binding boundary, where a JavaScript
//! `Float32Array`, a Dart `Float32List` and a Kotlin `ByteBuffer` can all be passed straight
//! through without an intermediate allocation per document.
//!
//! Two variants rather than one because the two callers want different things: Rust code has a
//! `&[f32]` and should not have to reinterpret it as bytes, while a binding has raw bytes and
//! should not have to decode them.

use crate::error::{DbError, NonFiniteKind, Result, ValidationError};
use crate::validation::limits;

pub use vdb_format::VectorDType;

/// A borrowed vector.
#[derive(Debug, Clone, Copy)]
pub enum VectorView<'a> {
    /// Native Rust floats.
    F32(&'a [f32]),
    /// Raw bytes in a declared dtype, as they arrive from a binding or a segment file.
    Raw {
        /// How the bytes are to be interpreted.
        dtype: VectorDType,
        /// The bytes themselves, `dimension * dtype.component_size()` long.
        bytes: &'a [u8],
        /// Component count.
        dimension: u32,
    },
}

impl<'a> VectorView<'a> {
    /// A view over native floats.
    pub fn f32(values: &'a [f32]) -> Self {
        Self::F32(values)
    }

    /// A view over raw bytes.
    ///
    /// # Errors
    /// [`ValidationError::InvalidVectorDimension`] if the byte length is not exactly
    /// `dimension * component_size`.
    pub fn raw(dtype: VectorDType, bytes: &'a [u8], dimension: u32) -> Result<Self> {
        let expected = dtype.row_stride(dimension);
        if bytes.len() != expected {
            return Err(ValidationError::InvalidVectorDimension {
                collection: "<input>".to_owned(),
                expected: dimension,
                actual: (bytes.len() / dtype.component_size().max(1)) as u32,
            }
            .into());
        }
        Ok(Self::Raw {
            dtype,
            bytes,
            dimension,
        })
    }

    /// Component type.
    pub fn dtype(&self) -> VectorDType {
        match self {
            Self::F32(_) => VectorDType::F32,
            Self::Raw { dtype, .. } => *dtype,
        }
    }

    /// Component count.
    pub fn dimension(&self) -> u32 {
        match self {
            Self::F32(v) => v.len() as u32,
            Self::Raw { dimension, .. } => *dimension,
        }
    }

    /// Whether the vector has no components.
    pub fn is_empty(&self) -> bool {
        self.dimension() == 0
    }

    /// Bytes this vector occupies when stored.
    pub fn byte_len(&self) -> usize {
        self.dtype().row_stride(self.dimension())
    }

    /// Append the stored representation to `out`.
    ///
    /// The single point where a vector is copied. Everything upstream of the log is borrowed.
    pub fn write_bytes(&self, out: &mut Vec<u8>) {
        match self {
            Self::F32(values) => {
                out.reserve(values.len() * 4);
                for v in *values {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            Self::Raw { bytes, .. } => out.extend_from_slice(bytes),
        }
    }

    /// The stored representation as an owned buffer.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.byte_len());
        self.write_bytes(&mut out);
        out
    }

    /// Components as `f32`, decoding raw bytes if necessary.
    pub fn to_f32(&self) -> Vec<f32> {
        match self {
            Self::F32(v) => v.to_vec(),
            Self::Raw { bytes, .. } => bytes
                .chunks_exact(4)
                .filter_map(|c| <[u8; 4]>::try_from(c).ok())
                .map(f32::from_le_bytes)
                .collect(),
        }
    }

    /// Iterate the components without allocating.
    pub fn iter_f32(&self) -> Box<dyn Iterator<Item = f32> + '_> {
        match self {
            Self::F32(v) => Box::new(v.iter().copied()),
            Self::Raw { bytes, .. } => Box::new(
                bytes
                    .chunks_exact(4)
                    .filter_map(|c| <[u8; 4]>::try_from(c).ok())
                    .map(f32::from_le_bytes),
            ),
        }
    }

    /// The L2 norm.
    pub fn norm(&self) -> f32 {
        self.iter_f32().map(|v| v * v).sum::<f32>().sqrt()
    }

    /// The reciprocal of the L2 norm, cached in the row directory so cosine similarity is a dot
    /// product and two multiplies.
    ///
    /// Zero for a zero-length vector, which is the convention that makes cosine against the zero
    /// vector score 0 rather than NaN. A zero vector has no direction, so no angle to anything
    /// is meaningful; scoring it 0 is the least surprising total answer.
    pub fn inv_norm(&self) -> f32 {
        let n = self.norm();
        if n > 0.0 && n.is_finite() {
            1.0 / n
        } else {
            0.0
        }
    }

    /// Check that every component is finite.
    ///
    /// # Errors
    /// [`ValidationError::InvalidVectorData`] naming the position of the first offending
    /// component. One NaN poisons every distance computed against the row, and the resulting
    /// "search returns nothing sensible" is very hard to trace back to the insert that caused it.
    pub fn check_finite(&self) -> Result<()> {
        for (index, v) in self.iter_f32().enumerate() {
            if v.is_finite() {
                continue;
            }
            let reason = if v.is_nan() {
                NonFiniteKind::Nan
            } else if v > 0.0 {
                NonFiniteKind::PosInf
            } else {
                NonFiniteKind::NegInf
            };
            return Err(ValidationError::InvalidVectorData { reason, index }.into());
        }
        Ok(())
    }

    /// Check the dimension against a collection's.
    ///
    /// # Errors
    /// [`ValidationError::InvalidVectorDimension`] naming the collection and both dimensions.
    pub fn check_dimension(&self, collection: &str, expected: u32) -> Result<()> {
        let actual = self.dimension();
        if actual != expected {
            return Err(ValidationError::InvalidVectorDimension {
                collection: collection.to_owned(),
                expected,
                actual,
            }
            .into());
        }
        Ok(())
    }

    /// Full validation for a vector about to be written.
    ///
    /// # Errors
    /// As [`VectorView::check_dimension`] and [`VectorView::check_finite`].
    pub fn validate(&self, collection: &str, expected: u32) -> Result<()> {
        self.check_dimension(collection, expected)?;
        self.check_finite()
    }
}

/// Check that a dimension is one a collection may be created with.
///
/// # Errors
/// [`ValidationError::InvalidDimension`] for zero or above [`limits::MAX_DIMENSION`].
pub fn check_dimension(dimension: u32) -> Result<()> {
    if dimension == 0 || dimension > limits::MAX_DIMENSION {
        return Err(DbError::Validation(ValidationError::InvalidDimension {
            dimension,
            max: limits::MAX_DIMENSION,
        }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_and_raw_views_agree() {
        let values = [1.0f32, -2.5, 3.25];
        let owned = VectorView::f32(&values).to_bytes();
        let raw = VectorView::raw(VectorDType::F32, &owned, 3).unwrap();

        assert_eq!(raw.dimension(), 3);
        assert_eq!(raw.to_f32(), values.to_vec());
        assert_eq!(raw.to_bytes(), owned);
        assert_eq!(raw.byte_len(), 12);
        assert!((raw.norm() - VectorView::f32(&values).norm()).abs() < 1e-6);
    }

    #[test]
    fn raw_rejects_a_byte_length_that_is_not_a_whole_number_of_components() {
        assert!(VectorView::raw(VectorDType::F32, &[0u8; 11], 3).is_err());
        assert!(VectorView::raw(VectorDType::F32, &[0u8; 13], 3).is_err());
        assert!(VectorView::raw(VectorDType::F32, &[0u8; 12], 3).is_ok());
    }

    #[test]
    fn an_empty_vector_is_representable_but_has_no_direction() {
        let v = VectorView::f32(&[]);
        assert!(v.is_empty());
        assert_eq!(v.dimension(), 0);
        assert_eq!(v.norm(), 0.0);
        assert_eq!(v.inv_norm(), 0.0);
    }

    #[test]
    fn inv_norm_is_the_reciprocal_and_zero_for_the_zero_vector() {
        let v = VectorView::f32(&[3.0, 4.0]);
        assert!((v.norm() - 5.0).abs() < 1e-6);
        assert!((v.inv_norm() - 0.2).abs() < 1e-6);

        // The zero vector must not produce an infinity that then poisons every score.
        let zero = VectorView::f32(&[0.0, 0.0, 0.0]);
        assert_eq!(zero.inv_norm(), 0.0);
        assert!(zero.inv_norm().is_finite());
    }

    #[test]
    fn check_finite_names_the_offending_component() {
        let v = [1.0f32, 2.0, f32::NAN, 4.0];
        match VectorView::f32(&v).check_finite() {
            Err(DbError::Validation(ValidationError::InvalidVectorData { reason, index })) => {
                assert_eq!(reason, NonFiniteKind::Nan);
                assert_eq!(index, 2);
            }
            other => panic!("expected InvalidVectorData, got {other:?}"),
        }
    }

    #[test]
    fn check_finite_distinguishes_the_infinities() {
        for (value, expected) in [
            (f32::INFINITY, NonFiniteKind::PosInf),
            (f32::NEG_INFINITY, NonFiniteKind::NegInf),
        ] {
            let v = [value];
            match VectorView::f32(&v).check_finite() {
                Err(DbError::Validation(ValidationError::InvalidVectorData { reason, .. })) => {
                    assert_eq!(reason, expected);
                }
                other => panic!("expected {expected:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn finite_extremes_are_accepted() {
        let v = [f32::MIN, f32::MAX, 0.0, -0.0, f32::MIN_POSITIVE];
        VectorView::f32(&v).check_finite().unwrap();
    }

    #[test]
    fn dimension_mismatch_names_the_collection_and_both_dimensions() {
        let v = [1.0f32, 2.0];
        match VectorView::f32(&v).check_dimension("products", 768) {
            Err(DbError::Validation(ValidationError::InvalidVectorDimension {
                collection,
                expected,
                actual,
            })) => {
                assert_eq!(collection, "products");
                assert_eq!(expected, 768);
                assert_eq!(actual, 2);
            }
            other => panic!("expected InvalidVectorDimension, got {other:?}"),
        }
    }

    #[test]
    fn dimension_bounds_are_enforced_in_both_directions() {
        assert!(check_dimension(1).is_ok());
        assert!(check_dimension(limits::MAX_DIMENSION).is_ok());
        assert!(check_dimension(0).is_err());
        assert!(check_dimension(limits::MAX_DIMENSION + 1).is_err());
    }

    #[test]
    fn write_bytes_is_little_endian_and_appends() {
        let mut out = vec![0xAAu8];
        VectorView::f32(&[1.0]).write_bytes(&mut out);
        assert_eq!(out, vec![0xAA, 0x00, 0x00, 0x80, 0x3F]);
    }

    #[test]
    fn iter_f32_matches_to_f32_for_both_variants() {
        let values = [0.5f32, -1.5, 2.0, 100.0];
        let bytes = VectorView::f32(&values).to_bytes();
        let raw = VectorView::raw(VectorDType::F32, &bytes, 4).unwrap();
        assert_eq!(raw.iter_f32().collect::<Vec<_>>(), values.to_vec());
        assert_eq!(
            VectorView::f32(&values).iter_f32().collect::<Vec<_>>(),
            values.to_vec()
        );
    }
}
