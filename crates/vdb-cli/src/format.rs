//! Turning engine types into terminal output.

use vdb_core::error::Recoverability;
use vdb_core::DbError;

/// What a user should actually do about an error.
///
/// Derived from [`Recoverability`] rather than matched per-variant, so a new error variant gets
/// sensible advice without anyone remembering to add it here.
pub(crate) fn advice(e: &DbError) -> &'static str {
    // A few codes deserve better than their recoverability class. "The database cannot be used
    // in this state" is true of a missing path and completely unhelpful, and these two are by
    // far the most common things a person running this tool will hit.
    match e.code() {
        vdb_core::ErrorCode::DATABASE_NOT_FOUND => {
            return "check the path; this command does not create a database"
        }
        vdb_core::ErrorCode::DATABASE_ALREADY_OPEN => {
            return "another process has it open; `stats`, `inspect` and `verify` work anyway"
        }
        _ => {}
    }
    match e.recoverability() {
        Recoverability::UserError => "check the arguments; the database was not modified",
        Recoverability::Retryable => {
            "transient — check that nothing else has the database open, then retry"
        }
        Recoverability::NeedsRepair => {
            "run `vdb verify --full <path>` for the full picture before changing anything"
        }
        Recoverability::Fatal => "the database cannot be used in this state",
        _ => "see the message above",
    }
}

/// Bytes, at human scale.
pub(crate) fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS.get(unit).copied().unwrap_or("B"))
    }
}

/// Thousands separators, so a seven-digit row count is readable at a glance.
pub(crate) fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A label and value, aligned into a column.
pub(crate) fn field(label: &str, value: impl std::fmt::Display) {
    println!("  {label:<22} {value}");
}

/// A section heading.
pub(crate) fn heading(text: &str) {
    println!("\n{text}");
    println!("{}", "─".repeat(text.chars().count()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_scale_to_readable_units() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(1024 * 1024 * 3), "3.0 MiB");
        // The scale stops at TiB rather than growing units forever; an absurd value should
        // still render rather than panic.
        assert!(bytes(u64::MAX).ends_with(" TiB"), "{}", bytes(u64::MAX));
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(1_234_567), "1,234,567");
    }

    #[test]
    fn the_common_cli_errors_get_specific_advice() {
        use vdb_core::error::LifecycleError;
        let missing: DbError = LifecycleError::DatabaseNotFound {
            path: "/tmp/x".into(),
        }
        .into();
        assert!(advice(&missing).contains("path"), "{}", advice(&missing));

        let busy: DbError = LifecycleError::DatabaseAlreadyOpen {
            path: "/tmp/x".into(),
            holder: None,
        }
        .into();
        assert!(advice(&busy).contains("verify"), "{}", advice(&busy));
    }

    #[test]
    fn every_recoverability_has_advice() {
        use vdb_core::error::{LifecycleError, ValidationError};
        let cases: Vec<DbError> = vec![
            ValidationError::TopKOutOfRange {
                requested: 0,
                max: 10,
            }
            .into(),
            LifecycleError::DatabaseClosed.into(),
            vdb_core::error::CorruptionError::MissingSegment {
                collection: "c".into(),
                segment: 1,
            }
            .into(),
            DbError::Cancelled,
        ];
        for e in cases {
            assert!(!advice(&e).is_empty(), "{e:?}");
        }
    }
}
