//! Delimited query-parameter parsing shared by the item handlers.
//!
//! Jellyfin binds multi-value query parameters with `CommaDelimited` /
//! `PipeDelimited` model binders (e.g. `includeItemTypes=Movie,Series`,
//! `genres=Action|Sci-Fi`). axum has no equivalent, so the handlers accept the
//! raw [`String`] and split it here, parsing enum tokens through their
//! `serde::Deserialize` (PascalCase) impls so the accepted spellings match the
//! vendored contract exactly.
//!
//! The same module also carries the JSON-*body* twin of those binders —
//! [`de_comma_delimited`] / [`de_pipe_delimited`], a port of
//! `JsonDelimitedCollectionConverter<T>` — for the request DTOs whose
//! collection properties upstream decorates with the delimited converter
//! factories (e.g. `GetProgramsDto`).

use std::fmt;
use std::marker::PhantomData;

use serde::de::value::{Error as ValueError, StrDeserializer};
use serde::de::{self, DeserializeOwned, IntoDeserializer, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
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

/// Deserializes a **comma**-delimited JSON collection field.
///
/// Port of `JsonDelimitedCollectionConverter<T>.Read` (v10.11.8
/// `src/Jellyfin.Extensions/Json/Converters/JsonDelimitedCollectionConverter.cs`)
/// as reached through `JsonCommaDelimitedCollectionConverterFactory`: when the
/// JSON token is a **string** it is split on the delimiter with
/// `StringSplitOptions.RemoveEmptyEntries`, each entry is `Trim()`ed and
/// converted, and an entry that fails to convert is *silently dropped*
/// (`catch (FormatException) { /* Ignore unconvertible inputs */ }`). Any other
/// token falls through to a strict array deserialize.
///
/// Jellyfin decorates seven `GetProgramsDto` properties with this factory, so
/// `{"ChannelIds":"<id>,<id>"}` is as valid as `{"ChannelIds":["<id>","<id>"]}`.
pub(crate) fn de_comma_delimited<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    deserializer.deserialize_option(DelimitedOption {
        delimiter: ',',
        marker: PhantomData,
    })
}

/// Deserializes a **pipe**-delimited JSON collection field.
///
/// Same converter as [`de_comma_delimited`], reached through
/// `JsonPipeDelimitedCollectionConverterFactory` — which upstream applies to
/// `GetProgramsDto.Genres` only. Keeping the two apart matters: `"News,Sport"`
/// is *one* genre here, exactly as upstream sees it.
pub(crate) fn de_pipe_delimited<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    deserializer.deserialize_option(DelimitedOption {
        delimiter: '|',
        marker: PhantomData,
    })
}

/// The `Option` layer of the delimited-collection converters: `null` and a
/// missing property both collapse to `None`, as upstream's nullable
/// `IReadOnlyList<T>?` does.
struct DelimitedOption<T> {
    /// The character the string form is split on.
    delimiter: char,
    /// Ties the visitor to the element type without owning one.
    marker: PhantomData<fn() -> T>,
}

impl<'de, T> Visitor<'de> for DelimitedOption<T>
where
    T: DeserializeOwned,
{
    type Value = Option<Vec<T>>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "an array, or a {:?}-delimited string, or null",
            self.delimiter
        )
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer
            .deserialize_any(DelimitedValue {
                delimiter: self.delimiter,
                marker: PhantomData,
            })
            .map(Some)
    }
}

/// The value layer: the string arm splits leniently, the array arm stays strict.
struct DelimitedValue<T> {
    /// The character the string form is split on.
    delimiter: char,
    /// Ties the visitor to the element type without owning one.
    marker: PhantomData<fn() -> T>,
}

impl<'de, T> Visitor<'de> for DelimitedValue<T>
where
    T: DeserializeOwned,
{
    type Value = Vec<T>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "an array or a {:?}-delimited string",
            self.delimiter
        )
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        // `Split(Delimiter, RemoveEmptyEntries)` + `Trim()` + convert, dropping
        // whatever the `TypeConverter` refuses. `deserialize_enum_token_ci` is
        // the same token parser the query-string path uses, so a spelling that
        // binds on `GET /LiveTv/Programs` binds identically on the `POST` form;
        // for `Guid` and `string` elements it degrades to a plain parse, which
        // is what `GuidConverter`/`StringConverter` do upstream.
        Ok(value
            .split(self.delimiter)
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .filter_map(deserialize_enum_token_ci)
            .collect())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        // `JsonSerializer.Deserialize<T[]>` — strict: an unknown member errors.
        let mut values = Vec::new();
        while let Some(value) = seq.next_element::<T>()? {
            values.push(value);
        }
        Ok(values)
    }
}

/// Applies the `locationTypes` / `excludeLocationTypes` query parameters to a
/// query's `IsVirtualItem`, exactly as `ItemsController.GetItems` does.
///
/// Port of v10.11.8 Jellyfin.Api/Controllers/ItemsController.cs:437-447:
///
/// ```text
/// if (excludeLocationTypes.Any(t => t == LocationType.Virtual)) { query.IsVirtualItem = false; }
/// if (locationTypes.Length > 0 && locationTypes.Length < 4)
///     { query.IsVirtualItem = locationTypes.Contains(LocationType.Virtual); }
/// ```
///
/// The `< 4` guard is upstream's: asking for ALL four location types is asking
/// for no filter at all, so it must not collapse into
/// `IsVirtualItem = Contains(Virtual)` and quietly become a virtual-only page.
/// `LocationType` is the only thing either parameter can express here — the
/// three non-`Virtual` values are indistinguishable in storage, and upstream
/// reads them the same way.
///
/// Both parameters were unported, which is why
/// `/Items?includeItemTypes=LiveTvProgram&locationTypes=FileSystem` returned the
/// whole guide where Jellyfin returns nothing: an airing is `IsVirtualItem = 1`.
pub(crate) fn apply_location_types(
    is_virtual_item: &mut Option<bool>,
    location_types: Option<&str>,
    exclude_location_types: Option<&str>,
) {
    use ferrofin_model::entities::LocationType;
    let excluded: Vec<LocationType> = parse_csv_enums_lenient(exclude_location_types);
    if excluded.contains(&LocationType::Virtual) {
        *is_virtual_item = Some(false);
    }
    let requested: Vec<LocationType> = parse_csv_enums_lenient(location_types);
    if !requested.is_empty() && requested.len() < 4 {
        *is_virtual_item = Some(requested.contains(&LocationType::Virtual));
    }
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

    /// A stand-in for a request DTO property carrying the *comma* factory.
    #[derive(Debug, serde::Deserialize)]
    struct CommaHolder {
        #[serde(default, deserialize_with = "super::de_comma_delimited")]
        ids: Option<Vec<Uuid>>,
        #[serde(default, deserialize_with = "super::de_comma_delimited")]
        kinds: Option<Vec<BaseItemKind>>,
    }

    /// A stand-in for a property carrying the *pipe* factory.
    #[derive(Debug, serde::Deserialize)]
    struct PipeHolder {
        #[serde(default, deserialize_with = "super::de_pipe_delimited")]
        genres: Option<Vec<String>>,
    }

    #[test]
    fn delimited_json_accepts_both_the_string_and_the_array_form() {
        let id = Uuid::from_u128(11);
        let from_str: CommaHolder =
            serde_json::from_str(&format!(r#"{{"ids":"{id},{id}","kinds":"Movie,Series"}}"#))
                .expect("binds");
        assert_eq!(from_str.ids, Some(vec![id, id]));
        assert_eq!(
            from_str.kinds,
            Some(vec![BaseItemKind::Movie, BaseItemKind::Series])
        );
        let from_arr: CommaHolder = serde_json::from_str(&format!(
            r#"{{"ids":["{id}","{id}"],"kinds":["Movie","Series"]}}"#
        ))
        .expect("binds");
        assert_eq!(from_arr.ids, from_str.ids);
        assert_eq!(from_arr.kinds, from_str.kinds);
    }

    #[test]
    fn delimited_json_string_form_drops_unconvertible_entries() {
        // `catch (FormatException) { /* Ignore unconvertible inputs */ }`.
        let id = Uuid::from_u128(12);
        let holder: CommaHolder =
            serde_json::from_str(&format!(r#"{{"ids":"zzz,{id}","kinds":"Nope,Movie"}}"#))
                .expect("binds");
        assert_eq!(holder.ids, Some(vec![id]));
        assert_eq!(holder.kinds, Some(vec![BaseItemKind::Movie]));
        // Case-insensitively, exactly as the query-string binder does.
        let cased: CommaHolder = serde_json::from_str(r#"{"kinds":"movie"}"#).expect("binds");
        assert_eq!(cased.kinds, Some(vec![BaseItemKind::Movie]));
    }

    #[test]
    fn delimited_json_string_form_removes_empty_entries_and_trims() {
        let holder: CommaHolder =
            serde_json::from_str(r#"{"kinds":" Movie , ,Series,"}"#).expect("binds");
        assert_eq!(
            holder.kinds,
            Some(vec![BaseItemKind::Movie, BaseItemKind::Series])
        );
        let empty: CommaHolder = serde_json::from_str(r#"{"kinds":",,"}"#).expect("binds");
        assert_eq!(empty.kinds, Some(Vec::new()));
    }

    #[test]
    fn delimited_json_array_form_is_strict() {
        // The array arm is `JsonSerializer.Deserialize<T[]>`, which errors.
        assert!(serde_json::from_str::<CommaHolder>(r#"{"kinds":["Nope"]}"#).is_err());
        assert!(serde_json::from_str::<CommaHolder>(r#"{"ids":["zzz"]}"#).is_err());
    }

    #[test]
    fn delimited_json_null_and_absent_are_none() {
        let absent: CommaHolder = serde_json::from_str("{}").expect("binds");
        assert!(absent.ids.is_none() && absent.kinds.is_none());
        let nulled: CommaHolder =
            serde_json::from_str(r#"{"ids":null,"kinds":null}"#).expect("binds");
        assert!(nulled.ids.is_none() && nulled.kinds.is_none());
    }

    #[test]
    fn pipe_delimited_json_splits_on_the_pipe_only() {
        let piped: PipeHolder = serde_json::from_str(r#"{"genres":"News|Sport"}"#).expect("binds");
        assert_eq!(
            piped.genres,
            Some(vec!["News".to_owned(), "Sport".to_owned()])
        );
        // A comma is just a character in a genre name for the pipe converter.
        let commas: PipeHolder = serde_json::from_str(r#"{"genres":"News,Sport"}"#).expect("binds");
        assert_eq!(commas.genres, Some(vec!["News,Sport".to_owned()]));
    }

    #[test]
    fn pipe_strings_split_and_trim() {
        assert_eq!(
            parse_pipe_strings(Some("Action | Sci-Fi |")),
            vec!["Action".to_owned(), "Sci-Fi".to_owned()]
        );
        assert!(parse_pipe_strings(None).is_empty());
    }

    /// `locationTypes`/`excludeLocationTypes` are the ONLY things that set
    /// `IsVirtualItem` on `GET /Items` (v10.11.8
    /// Jellyfin.Api/Controllers/ItemsController.cs:437-447). Both were unported,
    /// which is why `?includeItemTypes=LiveTvProgram&locationTypes=FileSystem`
    /// returned the whole guide where Jellyfin returns nothing — an airing is
    /// `IsVirtualItem = 1`.
    #[test]
    fn location_types_drive_is_virtual_item() {
        let apply = |loc: Option<&str>, excl: Option<&str>| {
            let mut v = None;
            super::apply_location_types(&mut v, loc, excl);
            v
        };
        // Neither parameter: untouched.
        assert_eq!(apply(None, None), None);
        // `locationTypes=Virtual` asks for virtual items ONLY.
        assert_eq!(apply(Some("Virtual"), None), Some(true));
        // Any other subset asks for non-virtual ones.
        assert_eq!(apply(Some("FileSystem"), None), Some(false));
        assert_eq!(apply(Some("FileSystem,Remote,Offline"), None), Some(false));
        // …but ALL FOUR is no filter at all — the `< 4` guard, which stops the
        // "everything" request collapsing into a virtual-only page.
        assert_eq!(apply(Some("FileSystem,Remote,Virtual,Offline"), None), None);
        // `excludeLocationTypes` only reacts to Virtual.
        assert_eq!(apply(None, Some("Virtual")), Some(false));
        assert_eq!(apply(None, Some("Remote")), None);
        // `locationTypes` is applied SECOND, so it wins over the exclusion —
        // the order upstream evaluates them in.
        assert_eq!(apply(Some("Virtual"), Some("Virtual")), Some(true));
        // Unknown tokens are dropped by the lenient binder, not 400'd.
        assert_eq!(apply(Some("Nonsense"), None), None);
    }
}
