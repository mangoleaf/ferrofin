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

/// Deserializes an optional [`Uuid`] query parameter the way ASP.NET binds a
/// `Guid?`: an **empty or whitespace-only** value is *absent*, not malformed.
///
/// `SimpleTypeModelBinder` returns `null` for a nullable value type whose
/// submitted value is empty, and the `Guid` type converter trims before parsing,
/// so upstream answers `?userId=` and `?userId=%20` exactly as it answers a
/// request with no `userId` at all. serde's own `Option<Uuid>` sees the key and
/// tries to parse `""`, which made both shapes a `400` here against a `200`
/// there — measured on the lane-3 pair against Jellyfin 10.11.8 for
/// `/Items`, `/UserViews`, `/Persons`, `/Years`, `/Devices`, `/Channels`,
/// `/Channels/Items/Latest`, `/LiveTv/Recordings/Folders`,
/// `/LiveTv/Recordings/{id}` and `/Audio/{id}/universal`.
///
/// Every `Option<Uuid>` field of a `Query<…>` struct in this crate binds through
/// here, so the rule has one implementation rather than one per handler. Request
/// **bodies** deliberately do not: `System.Text.Json` rejects `"UserId": ""`
/// upstream too, so a JSON body keeps serde's strict parse.
pub(crate) fn empty_as_none_uuid<'de, D>(deserializer: D) -> Result<Option<Uuid>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    match raw.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(value) => Uuid::parse_str(value)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
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
    use super::{empty_as_none_uuid, parse_csv_uuids, parse_pipe_strings};
    use ferrofin_model::data::BaseItemKind;
    use uuid::Uuid;

    #[derive(Debug, serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct UserIdOnly {
        #[serde(default, deserialize_with = "empty_as_none_uuid")]
        user_id: Option<Uuid>,
    }

    #[test]
    fn empty_user_id_binds_as_absent_like_asp_net() {
        // ASP.NET binds an empty (or whitespace-only) value for a `Guid?` to
        // null, so upstream answers these exactly as it answers a request with
        // no `userId` at all. Measured on the lane-3 pair: every one of these
        // spellings was `200` on Jellyfin 10.11.8 and `400` here.
        for raw in ["", "userId=", "userId=%20", "userId=%20%20"] {
            let bound: UserIdOnly = serde_urlencoded::from_str(raw)
                .unwrap_or_else(|e| panic!("{raw:?} must bind, got {e}"));
            assert_eq!(bound.user_id, None, "{raw:?}");
        }
    }

    #[test]
    fn a_real_user_id_still_binds_and_a_malformed_one_still_fails() {
        let id = Uuid::from_u128(0x1234);
        let bound: UserIdOnly = serde_urlencoded::from_str(&format!("userId={id}")).unwrap();
        assert_eq!(bound.user_id, Some(id));
        // Guid's type converter trims, so a padded id is still that id.
        let padded: UserIdOnly = serde_urlencoded::from_str(&format!("userId=%20{id}%20")).unwrap();
        assert_eq!(padded.user_id, Some(id));
        // A non-empty value that is not a guid stays a bind failure (400).
        assert!(serde_urlencoded::from_str::<UserIdOnly>("userId=not-a-guid").is_err());
    }

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
