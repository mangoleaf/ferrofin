//! Port of `GuidExtensions.cs` — nil-GUID predicates over `uuid::Uuid`.

use uuid::Uuid;

/// Determines whether the GUID is the default (all-zero / nil) value.
#[must_use]
pub fn is_empty(guid: &Uuid) -> bool {
    guid.is_nil()
}

/// Determines whether the GUID is `None` or the default (nil) value.
#[must_use]
pub fn is_null_or_empty(guid: Option<&Uuid>) -> bool {
    match guid {
        None => true,
        Some(g) => is_empty(g),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nil_is_empty() {
        assert!(is_empty(&Uuid::nil()));
    }

    #[test]
    fn non_nil_is_not_empty() {
        assert!(!is_empty(&Uuid::from_u128(1)));
    }

    #[test]
    fn null_or_empty() {
        assert!(is_null_or_empty(None));
        assert!(is_null_or_empty(Some(&Uuid::nil())));
        assert!(!is_null_or_empty(Some(&Uuid::from_u128(1))));
    }
}
