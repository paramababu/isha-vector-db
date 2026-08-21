//! Paths *inside* a database, and the validation that keeps them there.
//!
//! The engine never handles a host filesystem path. A [`DbPath`] is always relative to a database
//! root that only the [`Storage`](crate::storage::Storage) implementation knows about. This is a
//! security control as much as a tidiness one: because a `DbPath` cannot be constructed with a
//! `..` component, path traversal is prevented by the type, not by a check someone might forget.

use core::fmt;

use crate::error::{DbError, Result, ValidationError};

/// Longest single path component we will produce or accept.
pub const MAX_COMPONENT_LEN: usize = 255;
/// Longest whole path, chosen to stay clear of platform `PATH_MAX` once a root is prepended.
pub const MAX_PATH_LEN: usize = 1024;

/// Why a path or component was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathRejection {
    /// The path or a component was empty.
    Empty,
    /// A component was `.` or `..`.
    RelativeComponent,
    /// A component contained `/`, `\`, or a NUL byte.
    IllegalCharacter,
    /// The path was absolute.
    Absolute,
    /// A component exceeded [`MAX_COMPONENT_LEN`].
    ComponentTooLong,
    /// The whole path exceeded [`MAX_PATH_LEN`].
    TooLong,
}

impl fmt::Display for PathRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Empty => "empty",
            Self::RelativeComponent => "contains a `.` or `..` component",
            Self::IllegalCharacter => "contains a path separator or NUL byte",
            Self::Absolute => "is absolute",
            Self::ComponentTooLong => "has a component longer than 255 bytes",
            Self::TooLong => "is longer than 1024 bytes",
        };
        f.write_str(s)
    }
}

/// A validated, root-relative, `/`-separated path within a database directory.
///
/// Construction is the only place validation happens; every `DbPath` in existence is safe to
/// join onto a root.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DbPath(String);

impl DbPath {
    /// The database root itself.
    pub fn root() -> Self {
        Self(String::new())
    }

    /// Build a path from already-separated components.
    ///
    /// # Errors
    /// [`ValidationError::InvalidPath`] if any component is empty, relative, over-long, or
    /// contains a separator or NUL.
    pub fn from_components<I, S>(components: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut out = String::new();
        for c in components {
            let c = c.as_ref();
            check_component(c)?;
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(c);
        }
        if out.len() > MAX_PATH_LEN {
            return Err(reject(&out, PathRejection::TooLong));
        }
        Ok(Self(out))
    }

    /// Parse a `/`-separated relative path, validating every component.
    ///
    /// # Errors
    /// [`ValidationError::InvalidPath`] on any illegal component, or if the path is absolute.
    pub fn parse(s: &str) -> Result<Self> {
        if s.is_empty() {
            return Ok(Self::root());
        }
        if s.starts_with('/') || s.starts_with('\\') {
            return Err(reject(s, PathRejection::Absolute));
        }
        Self::from_components(s.split('/'))
    }

    /// Append one component, returning the new path.
    ///
    /// # Errors
    /// [`ValidationError::InvalidPath`] if the component is illegal or the result is too long.
    pub fn join(&self, component: &str) -> Result<Self> {
        check_component(component)?;
        let mut out = String::with_capacity(self.0.len() + 1 + component.len());
        out.push_str(&self.0);
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(component);
        if out.len() > MAX_PATH_LEN {
            return Err(reject(&out, PathRejection::TooLong));
        }
        Ok(Self(out))
    }

    /// The path as a `/`-separated string, empty for the root.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the database root.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterate the individual components, yielding nothing for the root.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|c| !c.is_empty())
    }

    /// The final component, or `None` for the root.
    pub fn file_name(&self) -> Option<&str> {
        self.components().last()
    }

    /// The containing directory, or `None` for the root.
    pub fn parent(&self) -> Option<Self> {
        let idx = self.0.rfind('/')?;
        Some(Self(self.0[..idx].to_owned()))
    }
}

fn check_component(c: &str) -> Result<()> {
    if c.is_empty() {
        return Err(reject(c, PathRejection::Empty));
    }
    if c == "." || c == ".." {
        return Err(reject(c, PathRejection::RelativeComponent));
    }
    if c.len() > MAX_COMPONENT_LEN {
        return Err(reject(c, PathRejection::ComponentTooLong));
    }
    if c.bytes().any(|b| b == b'/' || b == b'\\' || b == 0) {
        return Err(reject(c, PathRejection::IllegalCharacter));
    }
    Ok(())
}

fn reject(path: &str, reason: PathRejection) -> DbError {
    DbError::Validation(ValidationError::InvalidPath {
        path: path.to_owned(),
        reason,
    })
}

impl fmt::Display for DbPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            f.write_str("<root>")
        } else {
            f.write_str(&self.0)
        }
    }
}

impl fmt::Debug for DbPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DbPath({:?})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_empty_and_has_no_components() {
        let r = DbPath::root();
        assert!(r.is_root());
        assert_eq!(r.as_str(), "");
        assert_eq!(r.components().count(), 0);
        assert_eq!(r.file_name(), None);
        assert_eq!(r.parent(), None);
        assert_eq!(r.to_string(), "<root>");
    }

    #[test]
    fn joins_and_parses_round_trip() {
        let p = DbPath::root()
            .join("collections")
            .unwrap()
            .join("products")
            .unwrap();
        assert_eq!(p.as_str(), "collections/products");
        assert_eq!(DbPath::parse("collections/products").unwrap(), p);
        assert_eq!(p.file_name(), Some("products"));
        assert_eq!(p.parent().unwrap().as_str(), "collections");
    }

    /// The security-relevant case: traversal must be impossible to construct at all.
    #[test]
    fn rejects_traversal_in_every_form() {
        for bad in ["..", "../etc", "a/../../b", "./x", "a/./b"] {
            let err = DbPath::parse(bad).unwrap_err();
            assert!(
                matches!(
                    err,
                    DbError::Validation(ValidationError::InvalidPath {
                        reason: PathRejection::RelativeComponent,
                        ..
                    })
                ),
                "expected traversal rejection for {bad:?}, got {err:?}"
            );
        }
        assert!(DbPath::root().join("..").is_err());
    }

    #[test]
    fn rejects_absolute_separators_and_nul() {
        assert!(DbPath::parse("/abs").is_err());
        assert!(DbPath::parse("\\abs").is_err());
        assert!(DbPath::root().join("a/b").is_err());
        assert!(DbPath::root().join("a\\b").is_err());
        assert!(DbPath::root().join("a\0b").is_err());
        assert!(DbPath::parse("a//b").is_err()); // empty component
    }

    #[test]
    fn enforces_length_limits() {
        let long = "x".repeat(MAX_COMPONENT_LEN);
        assert!(DbPath::root().join(&long).is_ok());
        let too_long = "x".repeat(MAX_COMPONENT_LEN + 1);
        assert!(DbPath::root().join(&too_long).is_err());

        let mut p = DbPath::root();
        let mut hit_limit = false;
        for _ in 0..10 {
            match p.join(&long) {
                Ok(next) => p = next,
                Err(_) => {
                    hit_limit = true;
                    break;
                }
            }
        }
        assert!(hit_limit, "MAX_PATH_LEN should stop repeated joins");
    }
}
