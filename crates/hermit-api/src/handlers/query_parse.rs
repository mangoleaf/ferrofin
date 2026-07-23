//! Delimited query-parameter parsing shared by the item handlers.
//!
//! Jellyfin binds multi-value query parameters with `CommaDelimited` /
//! `PipeDelimited` model binders (e.g. `includeItemTypes=Movie,Series`,
//! `genres=Action|Sci-Fi`). axum has no equivalent, so the handlers accept the
//! raw [`String`] and split it here, parsing enum tokens through their
//! `serde::Deserialize` (PascalCase) impls so the accepted spellings match the
//! vendored contract exactly.

use serde::Deserialize;
use serde::de::IntoDeserializer;
use serde::de::value::{Error as ValueError, StrDeserializer};
use uuid::Uuid;

use crate::error::ApiError;

/// Splits a comma-delimited value and deserializes each token into `T` via its
/// `serde` (PascalCase) representation, skipping empty tokens.
///
/// An empty/absent input yields an empty [`Vec`]; an unrecognized token is a
/// `400` naming the offending value.
pub(crate) fn parse_csv_enums<T>(raw: Option<&str>) -> Result<Vec<T>, ApiError>
where
    T: for<'de> Deserialize<'de>,
{
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|token| {
            let de: StrDeserializer<'_, ValueError> = token.into_deserializer();
            T::deserialize(de).map_err(|_| ApiError::BadRequest(format!("invalid value {token:?}")))
        })
        .collect()
}

/// Splits a comma-delimited value into [`Uuid`]s, skipping empty tokens.
///
/// A malformed id is a `400`.
pub(crate) fn parse_csv_uuids(raw: Option<&str>) -> Result<Vec<Uuid>, ApiError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|token| {
            Uuid::parse_str(token)
                .map_err(|_| ApiError::BadRequest(format!("invalid id {token:?}")))
        })
        .collect()
}

/// Splits a pipe-delimited value into owned strings, trimming and dropping empty
/// tokens. Mirrors Jellyfin's `PipeDelimitedCollectionModelBinder`.
pub(crate) fn parse_pipe_strings(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    raw.split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{parse_csv_enums, parse_csv_uuids, parse_pipe_strings};
    use hermit_model::data::BaseItemKind;
    use uuid::Uuid;

    #[test]
    fn csv_enums_parse_pascal_case_tokens() {
        let kinds: Vec<BaseItemKind> = parse_csv_enums(Some("Movie, Series")).expect("valid kinds");
        assert_eq!(kinds, vec![BaseItemKind::Movie, BaseItemKind::Series]);
    }

    #[test]
    fn csv_enums_reject_unknown_token() {
        let err = parse_csv_enums::<BaseItemKind>(Some("Nope")).unwrap_err();
        assert!(matches!(err, crate::error::ApiError::BadRequest(_)));
    }

    #[test]
    fn csv_enums_empty_is_empty() {
        let kinds: Vec<BaseItemKind> = parse_csv_enums(None).expect("empty");
        assert!(kinds.is_empty());
        let kinds: Vec<BaseItemKind> = parse_csv_enums(Some("")).expect("empty");
        assert!(kinds.is_empty());
    }

    #[test]
    fn csv_uuids_parse_and_reject() {
        let id = Uuid::from_u128(7);
        let ids = parse_csv_uuids(Some(&format!("{id},{id}"))).expect("valid");
        assert_eq!(ids, vec![id, id]);
        assert!(parse_csv_uuids(Some("not-a-uuid")).is_err());
    }

    #[test]
    fn pipe_strings_split_and_trim() {
        assert_eq!(
            parse_pipe_strings(Some("Action | Sci-Fi |")),
            vec!["Action".to_owned(), "Sci-Fi".to_owned()]
        );
        assert!(parse_pipe_strings(None).is_empty());
    }
}
