//! Input validation, and the one table of limits everything enforces.
//!
//! Validation lives in one module so no write path can validate a subset by accident, and so
//! the limits are documented in a single place rather than rediscovered from error messages.

pub mod limits;

use crate::error::{NameRejection, Result, ValidationError};

/// Check a collection name.
///
/// This is a **security control**, not just naming hygiene: the name becomes a path component
/// on every platform, so it is rejected here, before any storage backend sees it. `DbPath` then
/// refuses traversal a second time. Two independent checks, because one of them will eventually
/// be bypassed by a code path nobody thought about.
///
/// # Errors
/// [`ValidationError::InvalidCollectionName`], naming why.
pub fn check_collection_name(name: &str) -> Result<()> {
    let reject = |reason| {
        Err(ValidationError::InvalidCollectionName {
            name: name.to_owned(),
            reason,
        }
        .into())
    };
    if name.is_empty() {
        return reject(NameRejection::Empty);
    }
    if name.len() > limits::MAX_COLLECTION_NAME_LEN {
        return reject(NameRejection::TooLong);
    }
    if name == "." || name == ".." {
        return reject(NameRejection::Reserved);
    }
    if !name.bytes().all(limits::is_valid_name_byte) {
        return reject(NameRejection::IllegalCharacter);
    }
    Ok(())
}

/// Check a `top_k`.
///
/// # Errors
/// [`ValidationError::TopKOutOfRange`] for zero or above [`limits::MAX_TOP_K`].
pub fn check_top_k(top_k: usize) -> Result<()> {
    if top_k == 0 || top_k > limits::MAX_TOP_K {
        return Err(ValidationError::TopKOutOfRange {
            requested: top_k,
            max: limits::MAX_TOP_K,
        }
        .into());
    }
    Ok(())
}

/// Check a batch's size.
///
/// # Errors
/// [`ValidationError::BatchTooLarge`] above [`limits::MAX_BATCH_OPS`].
pub fn check_batch_size(ops: usize) -> Result<()> {
    if ops > limits::MAX_BATCH_OPS {
        return Err(ValidationError::BatchTooLarge {
            ops,
            max: limits::MAX_BATCH_OPS,
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DbError;

    #[test]
    fn ordinary_names_are_accepted() {
        for name in ["products", "a", "my-collection", "my_collection_2", "A1"] {
            check_collection_name(name).unwrap_or_else(|e| panic!("{name:?} rejected: {e}"));
        }
    }

    /// The security-relevant cases. Every one of these would become a path component.
    #[test]
    fn names_that_could_escape_the_database_directory_are_rejected() {
        for bad in [
            "..", ".", "../etc", "a/b", "a\\b", "a\0b", "/abs", "C:", "~",
        ] {
            let err = check_collection_name(bad).unwrap_err();
            assert!(
                matches!(
                    err,
                    DbError::Validation(ValidationError::InvalidCollectionName { .. })
                ),
                "{bad:?} gave {err:?}"
            );
        }
    }

    #[test]
    fn empty_and_overlong_names_are_rejected() {
        assert!(matches!(
            check_collection_name(""),
            Err(DbError::Validation(
                ValidationError::InvalidCollectionName {
                    reason: NameRejection::Empty,
                    ..
                }
            ))
        ));
        let at_limit = "x".repeat(limits::MAX_COLLECTION_NAME_LEN);
        check_collection_name(&at_limit).unwrap();
        let over = "x".repeat(limits::MAX_COLLECTION_NAME_LEN + 1);
        assert!(matches!(
            check_collection_name(&over),
            Err(DbError::Validation(
                ValidationError::InvalidCollectionName {
                    reason: NameRejection::TooLong,
                    ..
                }
            ))
        ));
    }

    /// Non-ASCII is rejected even though it is harmless-looking: case folding and Unicode
    /// normalisation differ between APFS, ext4 and NTFS, so two names that compare equal in the
    /// engine could be one file or two depending on the device.
    #[test]
    fn non_ascii_names_are_rejected() {
        for bad in ["café", "日本語", "emoji-🧭"] {
            assert!(
                check_collection_name(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn top_k_bounds_are_enforced_in_both_directions() {
        assert!(check_top_k(1).is_ok());
        assert!(check_top_k(limits::MAX_TOP_K).is_ok());
        assert!(matches!(
            check_top_k(0),
            Err(DbError::Validation(ValidationError::TopKOutOfRange {
                requested: 0,
                ..
            }))
        ));
        assert!(check_top_k(limits::MAX_TOP_K + 1).is_err());
    }

    #[test]
    fn batch_size_bounds_are_enforced() {
        assert!(
            check_batch_size(0).is_ok(),
            "an empty batch is legal, if pointless"
        );
        assert!(check_batch_size(limits::MAX_BATCH_OPS).is_ok());
        assert!(check_batch_size(limits::MAX_BATCH_OPS + 1).is_err());
    }
}
