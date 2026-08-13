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

/// Splits a comma-delimited value case-insensitively, and silently drops tokens
/// that don't parse instead of erroring.
///
/// This mirrors ASP.NET's enum model binding, which Jellyfin relies on: tokens
/// bind case-insensitively (`Enum.Parse(…, ignoreCase: true)` — jellyfin-web
/// really does send `Filters=IsUnPlayed` and `VideoTypes=Bluray`), and an
/// unrecognized token is logged and dropped rather than failing the request
/// (`CommaDelimitedCollectionModelBinder`). Used for the enum-set parameters
/// (fields, filters, …); identifier-bearing params (ids) stay strict.
pub(crate) fn parse_csv_enums_lenient<T>(raw: Option<&str>) -> Vec<T>
where
    T: for<'de> Deserialize<'de>,
{
    let Some(raw) = raw else {
        return Vec::new();
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(deserialize_enum_token_ci)
        .collect()
}

/// Deserializes one enum token, retrying case-insensitively on a miss.
///
/// The retry recovers the type's accepted spellings from serde's own
/// "unknown variant `X`, expected one of `A`, `B`" error message — the only
/// way to enumerate a derived enum's variants without adding a reflection
/// dependency or hand-maintained variant lists. The message shape is pinned by
/// the tests below, so a serde format change fails loudly here, not in an API
/// handler.
fn deserialize_enum_token_ci<T>(token: &str) -> Option<T>
where
    T: for<'de> Deserialize<'de>,
{
    let de: StrDeserializer<'_, ValueError> = token.into_deserializer();
    match T::deserialize(de) {
        Ok(v) => Some(v),
        Err(err) => {
            // "unknown variant `x`, expected one of `A`, `B`" — every
            // backtick-quoted word after "expected" is an accepted spelling.
            let msg = err.to_string();
            let expected = msg.split("expected").nth(1)?;
            let canonical = expected
                .split('`')
                .skip(1)
                .step_by(2)
                .find(|variant| variant.eq_ignore_ascii_case(token))?;
            let de: StrDeserializer<'_, ValueError> = canonical.into_deserializer();
            T::deserialize(de).ok()
        }
    }
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
    use super::{parse_csv_uuids, parse_pipe_strings};
    use ferrofin_model::data::BaseItemKind;
    use uuid::Uuid;

    #[test]
    fn csv_enums_lenient_skips_unknown() {
        use super::parse_csv_enums_lenient;
        // A deprecated/unknown token is dropped, the valid ones survive.
        let kinds: Vec<BaseItemKind> = parse_csv_enums_lenient(Some("Movie,BasicSyncInfo,Series"));
        assert_eq!(kinds, vec![BaseItemKind::Movie, BaseItemKind::Series]);
    }

    #[test]
    fn csv_enums_lenient_binds_case_insensitively() {
        use super::parse_csv_enums_lenient;
        use ferrofin_model::entities::VideoType;
        use ferrofin_model::querying::ItemFilter;
        // jellyfin-web's filter dialog literally sends `IsUnPlayed` (sic) and
        // `Bluray`; ASP.NET's `Enum.Parse(…, ignoreCase: true)` binds both.
        let filters: Vec<ItemFilter> = parse_csv_enums_lenient(Some("IsUnPlayed,IsFavorite"));
        assert_eq!(
            filters,
            vec![ItemFilter::IsUnplayed, ItemFilter::IsFavorite]
        );
        let types: Vec<VideoType> = parse_csv_enums_lenient(Some("Bluray,dvd"));
        assert_eq!(types, vec![VideoType::BluRay, VideoType::Dvd]);
        // This retry parses serde's "unknown variant …, expected one of …"
        // message to enumerate the accepted spellings; if serde ever changes
        // that shape, this test is the loud failure.
        let kinds: Vec<BaseItemKind> = parse_csv_enums_lenient(Some("movie"));
        assert_eq!(kinds, vec![BaseItemKind::Movie]);
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
