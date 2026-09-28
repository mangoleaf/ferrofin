//! `MergeData` cases. The upstream ones are transliterated from
//! `tests/Jellyfin.Providers.Tests/Manager/MetadataServiceTests.cs`,
//! `tests/Jellyfin.Model.Tests/Entities/ProviderIdsExtensionsTests.cs` and
//! the `RefreshWithProviders_*` cases of
//! `tests/Jellyfin.Providers.Tests/Manager/MetadataServiceRefreshTests.cs`
//! (upstream master `208c278b75`); their expected values are the oracle.
//!
//! Upstream's reflection helper `TestMergeBaseItemData` answers "did the
//! target end up equal to the source value?"; [`merged_equals_new`] is its
//! transliteration.

use chrono::{TimeZone, Utc};
use ferrofin_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::MetadataField;
use rstest::rstest;

use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};

use super::{
    MetadataResult, RefreshAnswers, RefreshMerge, is_valid_provider_id, merge_data,
    merge_provider_ids, merge_refresh, set_provider_ids, settle_sort_name,
};

/// The stored `Type` name for `kind`.
fn type_name(kind: BaseItemKind) -> String {
    kind.stored_type_name().expect("a stored kind").to_owned()
}

/// A string key of a `Data` column value.
fn data_string(data: Option<&str>, key: &str) -> Option<String> {
    super::parse_data(data)
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// The URLs of a `Data` column value's `RemoteTrailers`, in order.
fn trailer_urls_of(data: Option<&str>) -> Vec<String> {
    super::parse_data(data)
        .get("RemoteTrailers")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| e.get("Url")?.as_str().map(str::to_owned))
        .collect()
}

fn item(kind: BaseItemKind) -> BaseItemEntity {
    BaseItemEntity {
        type_: type_name(kind),
        ..BaseItemEntity::default()
    }
}

/// One field of the row, read and written as upstream's reflection helper
/// reads and writes a property.
#[derive(Clone, Copy, Debug)]
enum Field {
    Name,
    OriginalTitle,
    OfficialRating,
    CustomRating,
    Tagline,
    Overview,
    ForcedSortName,
    /// `DisplayOrder`, which Ferrofin keeps in the `Data` blob.
    DisplayOrder,
    Genres,
    Studios,
    Tags,
    ProductionLocations,
    AlbumArtists,
}

impl Field {
    fn get(self, item: &BaseItemEntity) -> Option<String> {
        match self {
            Self::Name => item.name.clone(),
            Self::OriginalTitle => item.original_title.clone(),
            Self::OfficialRating => item.official_rating.clone(),
            Self::CustomRating => item.custom_rating.clone(),
            Self::Tagline => item.tagline.clone(),
            Self::Overview => item.overview.clone(),
            Self::ForcedSortName => item.forced_sort_name.clone(),
            Self::DisplayOrder => data_string(item.data.as_deref(), "DisplayOrder"),
            Self::Genres => item.genres.clone(),
            Self::Studios => item.studios.clone(),
            Self::Tags => item.tags.clone(),
            Self::ProductionLocations => item.production_locations.clone(),
            Self::AlbumArtists => item.album_artists.clone(),
        }
    }

    fn set(self, item: &mut BaseItemEntity, value: Option<&str>) {
        let value = value.map(str::to_owned);
        match self {
            Self::Name => item.name = value,
            Self::OriginalTitle => item.original_title = value,
            Self::OfficialRating => item.official_rating = value,
            Self::CustomRating => item.custom_rating = value,
            Self::Tagline => item.tagline = value,
            Self::Overview => item.overview = value,
            Self::ForcedSortName => item.forced_sort_name = value,
            Self::DisplayOrder => {
                item.data = value.map(|v| serde_json::json!({ "DisplayOrder": v }).to_string());
            }
            Self::Genres => item.genres = value,
            Self::Studios => item.studios = value,
            Self::Tags => item.tags = value,
            Self::ProductionLocations => item.production_locations = value,
            Self::AlbumArtists => item.album_artists = value,
        }
    }
}

/// `TestMergeBaseItemData`: merges a `kind` item carrying `new` into one
/// carrying `old` and answers whether the target now holds `new`.
fn merged_equals_new(
    kind: BaseItemKind,
    field: Field,
    old: Option<&str>,
    new: Option<&str>,
    lock: Option<MetadataField>,
    replace: bool,
) -> bool {
    let mut source = item(kind);
    field.set(&mut source, new);
    let mut target = item(kind);
    field.set(&mut target, old);
    let source = MetadataResult::of(source);
    let mut target = MetadataResult::of(target);
    let locked: Vec<MetadataField> = lock.into_iter().collect();
    merge_data(&source, &mut target, &locked, replace, false);
    field.get(&target.item).as_deref() == new
}

/// `MergeBaseItemData_StringField_ReplacesAppropriately` (on a `Series`, so
/// `DisplayOrder` is reachable).
#[rstest]
#[case::name(Field::Name, Some(MetadataField::Name), false)]
#[case::original_title(Field::OriginalTitle, None, true)]
#[case::official_rating(Field::OfficialRating, Some(MetadataField::OfficialRating), true)]
#[case::custom_rating(Field::CustomRating, None, true)]
#[case::tagline(Field::Tagline, None, true)]
#[case::overview(Field::Overview, Some(MetadataField::Overview), true)]
#[case::display_order(Field::DisplayOrder, None, false)]
#[case::forced_sort_name(Field::ForcedSortName, None, false)]
fn string_field_replaces_appropriately(
    #[case] field: Field,
    #[case] lock: Option<MetadataField>,
    #[case] replaces_with_empty: bool,
) {
    let k = BaseItemKind::Series;
    assert!(!merged_equals_new(
        k,
        field,
        Some("Old"),
        Some("New"),
        None,
        false
    ));
    if let Some(lock) = lock {
        assert!(!merged_equals_new(
            k,
            field,
            Some("Old"),
            Some("New"),
            Some(lock),
            true
        ));
        assert!(!merged_equals_new(
            k,
            field,
            None,
            Some("New"),
            Some(lock),
            false
        ));
        assert!(!merged_equals_new(
            k,
            field,
            Some(""),
            Some("New"),
            Some(lock),
            false
        ));
    }
    assert!(merged_equals_new(
        k,
        field,
        Some("Old"),
        Some("New"),
        None,
        true
    ));
    assert!(merged_equals_new(k, field, None, Some("New"), None, false));
    assert!(merged_equals_new(
        k,
        field,
        Some(""),
        Some("New"),
        None,
        false
    ));
    assert_eq!(
        merged_equals_new(k, field, Some("Old"), Some(""), None, true),
        replaces_with_empty
    );
}

/// `MergeBaseItemData_StringArrayField_ReplacesAppropriately` (on an
/// `Audio`, so `AlbumArtists` is reachable). "Note that arrays are replaced,
/// not merged" — except that a non-replacing merge into a non-empty target
/// unions, which is why the first assertion is `false`.
#[rstest]
#[case::genres(Field::Genres, Some(MetadataField::Genres))]
#[case::studios(Field::Studios, Some(MetadataField::Studios))]
#[case::tags(Field::Tags, Some(MetadataField::Tags))]
#[case::production_locations(Field::ProductionLocations, Some(MetadataField::ProductionLocations))]
#[case::album_artists(Field::AlbumArtists, None)]
fn string_array_field_replaces_appropriately(
    #[case] field: Field,
    #[case] lock: Option<MetadataField>,
) {
    let k = BaseItemKind::Audio;
    assert!(!merged_equals_new(
        k,
        field,
        Some("Old"),
        Some("New"),
        None,
        false
    ));
    if let Some(lock) = lock {
        assert!(!merged_equals_new(
            k,
            field,
            Some("Old"),
            Some("New"),
            Some(lock),
            true
        ));
        assert!(!merged_equals_new(
            k,
            field,
            None,
            Some("New"),
            Some(lock),
            false
        ));
    }
    assert!(merged_equals_new(
        k,
        field,
        Some("Old"),
        Some("New"),
        None,
        true
    ));
    assert!(merged_equals_new(k, field, None, Some("New"), None, false));
    assert!(merged_equals_new(k, field, Some("Old"), None, None, true));
}

/// The union half of the array rule: a fill merge into a non-empty target
/// keeps the target's values first and adds the source's, deduplicated
/// ordinal-ignore-case.
#[rstest]
#[case::studios(Field::Studios)]
#[case::tags(Field::Tags)]
#[case::production_locations(Field::ProductionLocations)]
#[case::album_artists(Field::AlbumArtists)]
fn string_array_fill_unions(#[case] field: Field) {
    let mut source = item(BaseItemKind::Audio);
    field.set(&mut source, Some("B|a"));
    let mut target = item(BaseItemKind::Audio);
    field.set(&mut target, Some("A"));
    let mut target = MetadataResult::of(target);
    merge_data(&MetadataResult::of(source), &mut target, &[], false, false);
    assert_eq!(field.get(&target.item).as_deref(), Some("A|B"));
}

/// Genres are not a union field upstream: a fill merge keeps the target.
#[test]
fn genres_fill_does_not_union() {
    assert!(!merged_equals_new(
        BaseItemKind::Movie,
        Field::Genres,
        Some("Drama"),
        Some("Comedy"),
        None,
        false
    ));
    let mut target = item(BaseItemKind::Movie);
    target.genres = Some("Drama".into());
    let mut source = item(BaseItemKind::Movie);
    source.genres = Some("Comedy".into());
    let mut target = MetadataResult::of(target);
    merge_data(&MetadataResult::of(source), &mut target, &[], false, false);
    assert_eq!(target.item.genres.as_deref(), Some("Drama"));
}

/// A scalar field, set on both sides of a merge.
#[derive(Clone, Copy, Debug)]
enum Scalar {
    IndexNumber,
    ParentIndexNumber,
    ProductionYear,
    CommunityRating,
    CriticRating,
    EndDate,
    PremiereDate,
    /// `Video3DFormat`, kept in `Data`.
    Video3DFormat,
}

impl Scalar {
    /// Sets the field to its "old" (1) or "new" (2) test value, or clears it.
    fn set(self, item: &mut BaseItemEntity, value: Option<i64>) {
        let date = |v: i64| {
            Utc.with_ymd_and_hms(2000 + i32::try_from(v).unwrap(), 1, 1, 0, 0, 0)
                .unwrap()
        };
        match self {
            Self::IndexNumber => item.index_number = value,
            Self::ParentIndexNumber => item.parent_index_number = value,
            Self::ProductionYear => item.production_year = value,
            #[allow(clippy::cast_precision_loss)]
            Self::CommunityRating => item.community_rating = value.map(|v| v as f64),
            #[allow(clippy::cast_precision_loss)]
            Self::CriticRating => item.critic_rating = value.map(|v| v as f64),
            Self::EndDate => item.end_date = value.map(date),
            Self::PremiereDate => item.premiere_date = value.map(date),
            Self::Video3DFormat => {
                item.data = value.map(|v| {
                    let format = if v == 1 {
                        "HalfSideBySide"
                    } else {
                        "FullSideBySide"
                    };
                    serde_json::json!({ "Video3DFormat": format }).to_string()
                });
            }
        }
    }

    fn equal(self, a: &BaseItemEntity, b: &BaseItemEntity) -> bool {
        match self {
            Self::IndexNumber => a.index_number == b.index_number,
            Self::ParentIndexNumber => a.parent_index_number == b.parent_index_number,
            Self::ProductionYear => a.production_year == b.production_year,
            Self::CommunityRating => a.community_rating == b.community_rating,
            Self::CriticRating => a.critic_rating == b.critic_rating,
            Self::EndDate => a.end_date == b.end_date,
            Self::PremiereDate => a.premiere_date == b.premiere_date,
            Self::Video3DFormat => {
                let read = |e: &BaseItemEntity| data_string(e.data.as_deref(), "Video3DFormat");
                read(a) == read(b)
            }
        }
    }
}

fn scalar_merged_equals_new(
    field: Scalar,
    old: Option<i64>,
    new: Option<i64>,
    replace: bool,
) -> bool {
    let mut source = item(BaseItemKind::Movie);
    field.set(&mut source, new);
    let mut target = item(BaseItemKind::Movie);
    field.set(&mut target, old);
    let source = MetadataResult::of(source);
    let mut target = MetadataResult::of(target);
    merge_data(&source, &mut target, &[], replace, false);
    field.equal(&target.item, &source.item)
}

/// `MergeBaseItemData_SimpleField_ReplacesAppropriately` (on a `Movie`, so
/// `Video3DFormat` is reachable).
#[rstest]
#[case::index_number(Scalar::IndexNumber)]
#[case::parent_index_number(Scalar::ParentIndexNumber)]
#[case::production_year(Scalar::ProductionYear)]
#[case::community_rating(Scalar::CommunityRating)]
#[case::critic_rating(Scalar::CriticRating)]
#[case::end_date(Scalar::EndDate)]
#[case::premiere_date(Scalar::PremiereDate)]
#[case::video_3d_format(Scalar::Video3DFormat)]
fn simple_field_replaces_appropriately(#[case] field: Scalar) {
    assert!(!scalar_merged_equals_new(field, Some(1), Some(2), false));
    assert!(scalar_merged_equals_new(field, Some(1), Some(2), true));
    assert!(scalar_merged_equals_new(field, None, Some(2), false));
    // "Video3DFormat - null values do NOT replace existing data".
    let null_replaces = !matches!(field, Scalar::Video3DFormat);
    assert_eq!(
        scalar_merged_equals_new(field, Some(1), None, true),
        null_replaces
    );
}

fn trailers(urls: &[&str]) -> String {
    let entries: Vec<serde_json::Value> = urls
        .iter()
        .map(|u| serde_json::json!({ "Name": format!("Name {u}"), "Url": u }))
        .collect();
    serde_json::json!({ "RemoteTrailers": entries }).to_string()
}

fn trailer_urls(item: &BaseItemEntity) -> Vec<String> {
    trailer_urls_of(item.data.as_deref())
}

fn merge_trailers(old: &[&str], new: &[&str], replace: bool) -> Vec<String> {
    let mut source = item(BaseItemKind::Movie);
    source.data = Some(trailers(new));
    let mut target = item(BaseItemKind::Movie);
    target.data = Some(trailers(old));
    let mut target = MetadataResult::of(target);
    merge_data(
        &MetadataResult::of(source),
        &mut target,
        &[],
        replace,
        false,
    );
    trailer_urls(&target.item)
}

/// `MergeBaseItemData_MergeTrailers_ReplacesAppropriately`, plus the union
/// it implies (`DistinctBy(t => t.Url)`).
#[test]
fn merge_trailers_replaces_appropriately() {
    assert_ne!(merge_trailers(&["URL 1"], &["URL 2"], false), ["URL 2"]);
    assert_eq!(
        merge_trailers(&["URL 1"], &["URL 2"], false),
        ["URL 1", "URL 2"]
    );
    assert_eq!(merge_trailers(&["URL 1"], &["URL 2"], true), ["URL 2"]);
    assert_eq!(merge_trailers(&[], &["URL 2"], false), ["URL 2"]);
    assert!(merge_trailers(&["URL 1"], &[], true).is_empty());
    assert_eq!(merge_trailers(&["URL 1"], &["URL 1"], false), ["URL 1"]);
}

fn ids(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// `MergeBaseItemData_ProviderIds_MergesAppropriately`.
#[test]
fn provider_ids_merge_appropriately() {
    let old = ids(&[("provider 1", "id 1")]);
    // Overwrite.
    let overwrite = ids(&[("provider 1", "id 2")]);
    assert_ne!(merge_provider_ids(&overwrite, &old, false), overwrite);
    assert_eq!(merge_provider_ids(&overwrite, &old, true), overwrite);
    // Merge without overwriting.
    let merged = merge_provider_ids(
        &ids(&[("provider 1", "id 2"), ("provider 2", "id 3")]),
        &old,
        false,
    );
    assert_eq!(
        merged,
        ids(&[("provider 1", "id 1"), ("provider 2", "id 3")])
    );
    // An empty source changes nothing.
    assert_eq!(merge_provider_ids(&[], &old, true), old);
}

/// The id-validity half of the ProviderIds rule: a malformed source id never
/// lands, even when replacing; a malformed stored id is replaced by a valid
/// one even when filling, and dropped when nothing replaces it.
#[test]
fn provider_ids_never_keep_an_invalid_id() {
    let stored = ids(&[("Tmdb", "nm0000123"), ("Imdb", "tt0113375")]);
    assert_eq!(
        merge_provider_ids(&ids(&[("Tmdb", "11")]), &stored, false),
        ids(&[("Tmdb", "11"), ("Imdb", "tt0113375")])
    );
    assert_eq!(
        merge_provider_ids(&ids(&[("Imdb", "https://imdb.com/tt1")]), &stored, true),
        ids(&[("Imdb", "tt0113375")])
    );
    // Keys compare case-insensitively.
    assert_eq!(
        merge_provider_ids(&ids(&[("tmdb", "12")]), &ids(&[("Tmdb", "11")]), true),
        ids(&[("Tmdb", "12")])
    );
}

/// `ProviderIdsExtensionsTests.TrySetProviderId_SurroundingWhitespace_Trimmed`.
#[rstest]
#[case("Imdb", " tt0113375 ")]
#[case(" Imdb", "tt0113375")]
fn set_provider_ids_trims_the_name_and_value(#[case] name: &str, #[case] value: &str) {
    assert_eq!(
        set_provider_ids([(name, value)]),
        ids(&[("Imdb", "tt0113375")])
    );
}

/// `SetProviderIds_ReplacesAll`, `SetProviderIds_ForeignId_Dropped` and
/// `TrySetProviderId_ForeignId_False`: the set is exactly the valid pairs
/// given (what the item had is not kept), a foreign or blank id is dropped;
/// and `TrySetProviderId`'s guards — a name holding `=` is dropped, a known
/// provider takes its canonical spelling, a later pair for the same name
/// replaces an earlier one.
#[test]
fn set_provider_ids_keeps_only_the_valid_pairs_given() {
    assert_eq!(
        set_provider_ids([("Tmdb", "nm0000123"), ("Imdb", "tt0113375"), ("Tvdb", "")]),
        ids(&[("Imdb", "tt0113375")])
    );
    assert_eq!(
        set_provider_ids([
            ("a=b", "1"),
            ("tmdb", "11"),
            ("TMDB", "12"),
            ("  ", "x"),
            ("SomePlugin", "  "),
            ("SomePlugin", "p1"),
        ]),
        ids(&[("Tmdb", "12"), ("SomePlugin", "p1")])
    );
    assert!(set_provider_ids([]).is_empty());
}

/// `ProviderIdsExtensionsTests.IsValidProviderId_ChecksKnownFormats`
/// (`null` is the empty string here).
#[rstest]
#[case("Imdb", "tt0113375", true)]
#[case("Imdb", "nm0000123", true)]
#[case("Imdb", "0113375", true)]
#[case("Imdb", "https://www.imdb.com/title/tt0113375", false)]
#[case("Tmdb", "11", true)]
#[case("Tmdb", "nm0000123", false)]
#[case("Tmdb", "0", false)]
#[case("Tmdb", "-11", false)]
#[case("TmdbCollection", "nm0000123", false)]
#[case("AudioDbArtist", "111239", true)]
#[case("AudioDbArtist", "a3cb23fc-acd3-4ce0-8f36-1e5aa6a18432", false)]
#[case("MusicBrainzArtist", "a3cb23fc-acd3-4ce0-8f36-1e5aa6a18432", true)]
#[case("MusicBrainzArtist", "111239", false)]
#[case("MusicBrainzAlbum", "not-an-mbid", false)]
#[case("Tvdb", "anything-goes", true)]
#[case("SomePlugin", "anything-goes", true)]
#[case("Tmdb", "", false)]
#[case("", "11", false)]
fn is_valid_provider_id_checks_known_formats(
    #[case] name: &str,
    #[case] value: &str,
    #[case] expected: bool,
) {
    assert_eq!(is_valid_provider_id(name, value), expected);
}

fn person(name: &str) -> PeopleEntity {
    PeopleEntity {
        id: String::new(),
        name: name.to_owned(),
        person_type: Some("Actor".into()),
        role: None,
        primary_image_url: None,
        provider_id: None,
    }
}

fn old_people() -> Vec<PeopleEntity> {
    vec![PeopleEntity {
        provider_id: Some(1234),
        ..person("Name 1")
    }]
}

/// `TestMergeBaseItemDataPerson`: whether the target's people end up equal to
/// the source's. (Owned arguments keep the upstream call shapes readable.)
#[allow(clippy::needless_pass_by_value)]
fn merge_people_case(
    old: Option<Vec<PeopleEntity>>,
    new: Option<Vec<PeopleEntity>>,
    lock: Option<MetadataField>,
    replace: bool,
) -> (bool, Option<Vec<PeopleEntity>>) {
    let source = MetadataResult {
        people: new.clone(),
        ..MetadataResult::of(item(BaseItemKind::Movie))
    };
    let mut target = MetadataResult {
        people: old,
        ..MetadataResult::of(item(BaseItemKind::Movie))
    };
    let locked: Vec<MetadataField> = lock.into_iter().collect();
    merge_data(&source, &mut target, &locked, replace, false);
    (target.people == new, target.people)
}

/// `MergeBaseItemData_MergePeople_MergesAppropriately`, over Ferrofin's one
/// person id (upstream's `"Provider 1"` key is the TMDB id here).
#[test]
fn merge_people_merges_appropriately() {
    let overwrite = vec![person("Name 2")];
    let (equal, result) =
        merge_people_case(Some(old_people()), Some(overwrite.clone()), None, false);
    assert!(!equal);
    // People not already in target are not merged into it from source.
    let result = result.expect("people");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].name, "Name 1");

    assert!(merge_people_case(Some(old_people()), Some(overwrite.clone()), None, true).0);
    assert!(merge_people_case(Some(Vec::new()), Some(overwrite.clone()), None, false).0);
    assert!(merge_people_case(None, Some(overwrite.clone()), None, false).0);
    assert!(
        !merge_people_case(
            Some(old_people()),
            Some(overwrite),
            Some(MetadataField::Cast),
            true
        )
        .0
    );

    // Ids merge but don't overwrite the target's.
    let merge_new = vec![PeopleEntity {
        provider_id: Some(5678),
        role: Some("Role".into()),
        ..person("Name 1")
    }];
    let (_, result) = merge_people_case(Some(old_people()), Some(merge_new), None, false);
    let result = result.expect("people");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].provider_id, Some(1234));
    assert_eq!(
        result[0].role.as_deref(),
        Some("Role"),
        "a missing role fills"
    );
    let (_, result) = merge_people_case(
        Some(vec![person("Name 1")]),
        Some(vec![PeopleEntity {
            provider_id: Some(5678),
            ..person("Name 1")
        }]),
        None,
        false,
    );
    assert_eq!(result.expect("people")[0].provider_id, Some(5678));

    // A picture fills when missing but never overwrites.
    let picture = |url: &str| {
        vec![PeopleEntity {
            primary_image_url: Some(url.into()),
            ..person("Name 1")
        }]
    };
    let (_, result) = merge_people_case(Some(old_people()), Some(picture("URL 1")), None, false);
    assert_eq!(
        result.expect("people")[0].primary_image_url.as_deref(),
        Some("URL 1")
    );
    let (_, result) =
        merge_people_case(Some(picture("URL 1")), Some(picture("URL 2")), None, false);
    assert_eq!(
        result.expect("people")[0].primary_image_url.as_deref(),
        Some("URL 1")
    );

    // An empty source can be forced to overwrite a target with data.
    assert!(merge_people_case(Some(old_people()), Some(Vec::new()), None, true).0);
}

/// Names match ignoring case and diacritics; an invalid (non-positive) TMDB
/// person id is dropped on both sides.
#[test]
fn merge_people_matches_names_loosely_and_drops_invalid_ids() {
    let (_, result) = merge_people_case(
        Some(vec![PeopleEntity {
            provider_id: Some(0),
            ..person("Zoë Kravitz")
        }]),
        Some(vec![PeopleEntity {
            provider_id: Some(37_917),
            ..person("zoe kravitz")
        }]),
        None,
        false,
    );
    assert_eq!(result.expect("people")[0].provider_id, Some(37_917));
}

/// `MergeBaseItemData_MergeMetadataSettings_MergesWhenSet`.
#[rstest]
#[case(false, false)]
#[case(true, false)]
#[case(true, true)]
fn merge_metadata_settings_merges_when_set(
    #[case] merge_metadata_settings: bool,
    #[case] default_date: bool,
) {
    let new_date = Utc.with_ymd_and_hms(2026, 9, 24, 0, 0, 0).unwrap();
    let old_date = chrono::DateTime::<Utc>::UNIX_EPOCH;
    let source = BaseItemEntity {
        is_locked: true,
        preferred_metadata_country_code: Some("new".into()),
        preferred_metadata_language: Some("new".into()),
        date_created: (!default_date).then_some(new_date),
        ..item(BaseItemKind::Movie)
    };
    let target = BaseItemEntity {
        is_locked: false,
        preferred_metadata_country_code: Some("old".into()),
        preferred_metadata_language: Some("old".into()),
        date_created: Some(old_date),
        ..item(BaseItemKind::Movie)
    };
    let new_locked = vec![MetadataField::Genres, MetadataField::Cast];
    let old_locked = vec![MetadataField::Genres];
    let source = MetadataResult {
        locked_fields: new_locked.clone(),
        ..MetadataResult::of(source)
    };
    let mut target = MetadataResult {
        locked_fields: old_locked.clone(),
        ..MetadataResult::of(target)
    };
    merge_data(&source, &mut target, &[], true, merge_metadata_settings);
    let t = &target.item;
    if merge_metadata_settings {
        assert_eq!(target.locked_fields, new_locked);
        assert!(t.is_locked);
        assert_eq!(t.preferred_metadata_country_code.as_deref(), Some("new"));
        assert_eq!(t.preferred_metadata_language.as_deref(), Some("new"));
        assert_eq!(
            t.date_created,
            Some(if default_date { old_date } else { new_date })
        );
    } else {
        assert_eq!(target.locked_fields, old_locked);
        assert!(!t.is_locked);
        assert_eq!(t.preferred_metadata_country_code.as_deref(), Some("old"));
        assert_eq!(t.preferred_metadata_language.as_deref(), Some("old"));
        assert_eq!(t.date_created, Some(old_date));
    }
}

/// `if (target is not Audio && target is not Video && target is not Book)`:
/// a provider runtime never replaces a probed one, but does land on a
/// series.
#[rstest]
#[case::movie(BaseItemKind::Movie, false)]
#[case::episode(BaseItemKind::Episode, false)]
#[case::audio(BaseItemKind::Audio, false)]
#[case::audio_book(BaseItemKind::AudioBook, false)]
#[case::book(BaseItemKind::Book, false)]
#[case::series(BaseItemKind::Series, true)]
fn runtime_comes_from_providers_only_for_non_media_kinds(
    #[case] kind: BaseItemKind,
    #[case] takes_provider_runtime: bool,
) {
    let source = BaseItemEntity {
        run_time_ticks: Some(2),
        ..item(kind)
    };
    let mut target = MetadataResult::of(BaseItemEntity {
        run_time_ticks: Some(1),
        ..item(kind)
    });
    merge_data(
        &MetadataResult::of(source.clone()),
        &mut target,
        &[],
        true,
        true,
    );
    assert_eq!(
        target.item.run_time_ticks,
        Some(if takes_provider_runtime { 2 } else { 1 })
    );
    let mut locked = MetadataResult::of(BaseItemEntity {
        run_time_ticks: Some(1),
        ..item(kind)
    });
    merge_data(
        &MetadataResult::of(source),
        &mut locked,
        &[MetadataField::Runtime],
        true,
        true,
    );
    assert_eq!(
        locked.item.run_time_ticks,
        Some(1),
        "a locked runtime never moves"
    );
}

/// A row carrying a value for every field upstream's lock can protect.
fn fully_populated(tag: &str, kind: BaseItemKind) -> MetadataResult {
    MetadataResult {
        item: BaseItemEntity {
            name: Some(format!("{tag} name")),
            genres: Some(format!("{tag} genre")),
            official_rating: Some(format!("{tag} rating")),
            overview: Some(format!("{tag} overview")),
            run_time_ticks: Some(if tag == "old" { 1 } else { 2 }),
            studios: Some(format!("{tag} studio")),
            tags: Some(format!("{tag} tag")),
            production_locations: Some(format!("{tag} location")),
            tagline: Some(format!("{tag} tagline")),
            ..item(kind)
        },
        people: Some(vec![person(&format!("{tag} person"))]),
        provider_ids: Vec::new(),
        locked_fields: Vec::new(),
    }
}

/// Every lockable field, merged in every `replace_data` mode, with and
/// without its lock: a locked field keeps the target's value; an unlocked one
/// follows the mode. `Tagline` is never lock-checked upstream.
#[rstest]
fn locked_fields_hold_in_every_mode(
    #[values(
        MetadataField::Name,
        MetadataField::Genres,
        MetadataField::OfficialRating,
        MetadataField::Overview,
        MetadataField::Cast,
        MetadataField::Runtime,
        MetadataField::Studios,
        MetadataField::Tags,
        MetadataField::ProductionLocations
    )]
    field: MetadataField,
    #[values(false, true)] replace: bool,
    #[values(false, true)] locked: bool,
) {
    // A `Series` owns no runtime of its own, so `Runtime` is observable.
    let kind = BaseItemKind::Series;
    let source = fully_populated("new", kind);
    let old = fully_populated("old", kind);
    let mut target = old.clone();
    let locks: Vec<MetadataField> = if locked { vec![field] } else { Vec::new() };
    merge_data(&source, &mut target, &locks, replace, false);
    let pick = |r: &MetadataResult| -> String {
        let i = &r.item;
        match field {
            MetadataField::Name => i.name.clone().unwrap_or_default(),
            MetadataField::Genres => i.genres.clone().unwrap_or_default(),
            MetadataField::OfficialRating => i.official_rating.clone().unwrap_or_default(),
            MetadataField::Overview => i.overview.clone().unwrap_or_default(),
            MetadataField::Cast => format!("{:?}", r.people),
            MetadataField::Runtime => format!("{:?}", i.run_time_ticks),
            MetadataField::Studios => i.studios.clone().unwrap_or_default(),
            MetadataField::Tags => i.tags.clone().unwrap_or_default(),
            MetadataField::ProductionLocations => {
                i.production_locations.clone().unwrap_or_default()
            }
        }
    };
    let expected = if locked || !replace {
        match (locked, field) {
            // A fill merge into a non-empty target unions the union fields.
            (
                false,
                MetadataField::Studios | MetadataField::Tags | MetadataField::ProductionLocations,
            ) => format!("{}|{}", pick(&old), pick(&source)),
            // …and enriches (but never adds to) the target's people.
            _ => pick(&old),
        }
    } else {
        pick(&source)
    };
    assert_eq!(
        pick(&target),
        expected,
        "{field:?} replace={replace} locked={locked}"
    );
    // A lock on one field never protects another.
    assert_eq!(
        target.item.tagline.as_deref(),
        Some(if replace {
            "new tagline"
        } else {
            "old tagline"
        })
    );
}

/// `Data` merges key by key: the upstream-owned keys follow their rules,
/// resolver keys (`VideoType`) come from the source when it has them, and a
/// key only the target holds is kept — in every mode.
#[test]
fn data_merges_key_wise() {
    let source = BaseItemEntity {
        data: Some(r#"{"VideoType":"Iso","Status":"Ended"}"#.into()),
        ..item(BaseItemKind::Series)
    };
    let target = BaseItemEntity {
        data: Some(
            r#"{"VideoType":"VideoFile","Status":"Continuing","AirTime":"20:00","LinkedChildren":[]}"#.into(),
        ),
        ..item(BaseItemKind::Series)
    };
    let merged = |replace: bool| {
        let mut t = MetadataResult::of(target.clone());
        merge_data(
            &MetadataResult::of(source.clone()),
            &mut t,
            &[],
            replace,
            false,
        );
        super::parse_data(t.item.data.as_deref())
    };
    let filled = merged(false);
    assert_eq!(filled["VideoType"], "VideoFile", "fill keeps the target's");
    assert_eq!(filled["Status"], "Continuing");
    assert_eq!(filled["AirTime"], "20:00");
    assert!(filled.contains_key("LinkedChildren"));
    let replaced = merged(true);
    assert_eq!(replaced["VideoType"], "Iso");
    assert_eq!(replaced["Status"], "Ended");
    assert!(
        !replaced.contains_key("AirTime"),
        "replace clears an upstream property the source did not return"
    );
    assert!(
        replaced.contains_key("LinkedChildren"),
        "an unrelated key is never dropped"
    );
}

/// A merge that changes nothing leaves the blob byte-identical (no
/// reserialization, so key order and spacing survive).
#[test]
fn an_unchanged_blob_is_not_reserialized() {
    let blob = r#"{ "Status": "Ended",  "AirTime": "20:00" }"#;
    let target = BaseItemEntity {
        data: Some(blob.into()),
        ..item(BaseItemKind::Series)
    };
    let mut t = MetadataResult::of(target.clone());
    merge_data(&MetadataResult::of(target), &mut t, &[], true, false);
    assert_eq!(t.item.data.as_deref(), Some(blob));
}

/// Upstream's Default scan in two calls (`RefreshWithProviders` `:897-915`):
/// stored values fill the provider result, then the result replaces the row.
/// Provider values win; fields no provider returned keep the stored values;
/// the union fields keep both.
#[test]
fn default_mode_is_fill_then_replace() {
    let stored = MetadataResult::of(BaseItemEntity {
        name: Some("Stored".into()),
        overview: Some("Stored overview".into()),
        premiere_date: Some(Utc.with_ymd_and_hms(2011, 4, 17, 0, 0, 0).unwrap()),
        community_rating: Some(8.1),
        studios: Some("HBO".into()),
        data: Some(r#"{"RemoteTrailers":[{"Url":"a"}]}"#.into()),
        ..item(BaseItemKind::Episode)
    });
    let mut provider = MetadataResult::of(BaseItemEntity {
        overview: Some("Provider overview".into()),
        studios: Some("Sky".into()),
        data: Some(r#"{"RemoteTrailers":[{"Url":"b"}]}"#.into()),
        ..item(BaseItemKind::Episode)
    });
    merge_data(&stored, &mut provider, &[], false, false);
    let mut row = stored.clone();
    merge_data(&provider, &mut row, &[], true, true);
    let r = &row.item;
    assert_eq!(r.name.as_deref(), Some("Stored"));
    assert_eq!(r.overview.as_deref(), Some("Provider overview"));
    assert_eq!(r.premiere_date, stored.item.premiere_date);
    assert_eq!(r.community_rating, Some(8.1));
    assert_eq!(r.studios.as_deref(), Some("Sky|HBO"));
    assert_eq!(trailer_urls(r), ["b", "a"]);
}

/// Per-kind overrides: `Artists` unions on audio, an episode takes a
/// provider's explicit season only on the settings-carrying merge, and a
/// book takes its series name.
#[test]
fn kind_specific_rules() {
    let mut target = MetadataResult::of(BaseItemEntity {
        artists: Some("A".into()),
        album: Some("Old".into()),
        ..item(BaseItemKind::Audio)
    });
    let source = MetadataResult::of(BaseItemEntity {
        artists: Some("B".into()),
        album: Some("New".into()),
        ..item(BaseItemKind::Audio)
    });
    merge_data(&source, &mut target, &[], false, false);
    assert_eq!(target.item.artists.as_deref(), Some("A|B"));
    assert_eq!(target.item.album.as_deref(), Some("Old"));

    let episode = |season| {
        MetadataResult::of(BaseItemEntity {
            parent_index_number: season,
            ..item(BaseItemKind::Episode)
        })
    };
    let mut target = episode(Some(1));
    merge_data(&episode(Some(2)), &mut target, &[], false, false);
    assert_eq!(target.item.parent_index_number, Some(1));
    merge_data(&episode(Some(2)), &mut target, &[], false, true);
    assert_eq!(target.item.parent_index_number, Some(2));

    let mut book = MetadataResult::of(BaseItemEntity {
        series_name: Some("Folder".into()),
        ..item(BaseItemKind::Book)
    });
    let source = MetadataResult::of(BaseItemEntity {
        series_name: Some("Discworld".into()),
        ..item(BaseItemKind::Book)
    });
    merge_data(&source, &mut book, &[], true, false);
    assert_eq!(book.item.series_name.as_deref(), Some("Discworld"));
}

/// The dashboard's three refresh choices, as `RefreshWithProviders`
/// (`MetadataService.cs:895-918`) makes its two `MergeData` calls for each:
/// the stored values first fill the provider result (skipped under
/// `RemoveOldMetadata`), then the result merges onto the item under the
/// item's `LockedFields`, replacing unless the mode is a fill-missing
/// `FullRefresh`.
#[derive(Clone, Copy, Debug)]
enum RefreshChoice {
    /// "Scan for new and updated files" when the providers run (a first or
    /// required refresh): `Default`, `shouldReplace = true`.
    Default,
    /// "Search for missing metadata": `FullRefresh`, no `ReplaceAllMetadata`.
    SearchMissing,
    /// "Replace all metadata": `FullRefresh` + `ReplaceAllMetadata` +
    /// `RemoveOldMetadata`.
    ReplaceAll,
}

/// `stored` after a refresh whose providers answered `provider`.
fn refresh(
    stored: &BaseItemEntity,
    provider: &BaseItemEntity,
    choice: RefreshChoice,
    locked: &[MetadataField],
) -> BaseItemEntity {
    let mut temp = MetadataResult::of(provider.clone());
    let current = MetadataResult::of(stored.clone());
    if !matches!(choice, RefreshChoice::ReplaceAll) {
        merge_data(&current, &mut temp, &[], false, false);
    }
    let replace = !matches!(choice, RefreshChoice::SearchMissing);
    let mut target = current;
    merge_data(&temp, &mut target, locked, replace, true);
    target.item
}

/// Phase 3L: a user's edit made without `LockData` against a provider that
/// says otherwise, in each refresh mode, with and without a lock on the
/// edited field. Only a lock protects it from a replacing merge; the
/// fill-missing "Search for missing metadata" keeps it either way.
#[rstest]
#[case::default_unlocked(RefreshChoice::Default, false, "Provider overview")]
#[case::default_locked(RefreshChoice::Default, true, "My overview")]
#[case::search_missing_unlocked(RefreshChoice::SearchMissing, false, "My overview")]
#[case::search_missing_locked(RefreshChoice::SearchMissing, true, "My overview")]
#[case::replace_all_unlocked(RefreshChoice::ReplaceAll, false, "Provider overview")]
#[case::replace_all_locked(RefreshChoice::ReplaceAll, true, "My overview")]
fn an_edited_overview_follows_the_mode_unless_locked(
    #[case] choice: RefreshChoice,
    #[case] lock_overview: bool,
    #[case] overview: &str,
) {
    let stored = BaseItemEntity {
        name: Some("Stored name".into()),
        overview: Some("My overview".into()),
        tagline: None,
        ..item(BaseItemKind::Movie)
    };
    let provider = BaseItemEntity {
        name: Some("Provider name".into()),
        overview: Some("Provider overview".into()),
        tagline: Some("Provider tagline".into()),
        ..item(BaseItemKind::Movie)
    };
    let locks: Vec<MetadataField> = if lock_overview {
        vec![MetadataField::Overview]
    } else {
        Vec::new()
    };
    let saved = refresh(&stored, &provider, choice, &locks);
    assert_eq!(saved.overview.as_deref(), Some(overview), "{choice:?}");
    // The unlocked fields on the same item follow the mode regardless.
    let name = match choice {
        RefreshChoice::SearchMissing => "Stored name",
        RefreshChoice::Default | RefreshChoice::ReplaceAll => "Provider name",
    };
    assert_eq!(saved.name.as_deref(), Some(name), "{choice:?}");
    assert_eq!(saved.tagline.as_deref(), Some("Provider tagline"));
}

/// "Replace all metadata" clears what the providers did not return — but
/// never a locked field.
#[test]
fn replace_all_clears_unlocked_fields_the_providers_left_empty_but_not_locked_ones() {
    let stored = BaseItemEntity {
        overview: Some("My overview".into()),
        tagline: Some("My tagline".into()),
        genres: Some("Drama".into()),
        ..item(BaseItemKind::Movie)
    };
    let provider = item(BaseItemKind::Movie);
    let saved = refresh(
        &stored,
        &provider,
        RefreshChoice::ReplaceAll,
        &[MetadataField::Overview, MetadataField::Genres],
    );
    assert_eq!(saved.overview.as_deref(), Some("My overview"));
    assert_eq!(saved.genres.as_deref(), Some("Drama"));
    assert_eq!(saved.tagline, None, "unlocked and not returned: cleared");
}

/// `RefreshWithProviders`' two switches (`MetadataService.cs:895-917`) for
/// the dashboard's three choices, an Identify, and the passes where nothing
/// answered or every remote provider failed — the one rule both the scan and
/// the single-item refresh merge by.
// A table of eight cases, one per line before rustfmt spreads them out.
#[allow(clippy::too_many_lines)]
#[test]
fn refresh_merge_follows_refresh_with_providers() {
    use MetadataRefreshMode::{Default, FullRefresh, ValidationOnly};
    // (case, mode, replace all, remove old, any answered, failed, remote
    // answered) → (keep existing, replace)
    let cases = [
        // "Scan for new and updated files": replace, the stored values fill.
        (
            "scan", Default, false, false, true, false, false, true, true,
        ),
        // "Search for missing metadata": fill only.
        (
            "search missing",
            FullRefresh,
            false,
            false,
            true,
            false,
            false,
            true,
            false,
        ),
        // "Replace all metadata" / Identify (`RemoveOldMetadata`): replace and
        // clear.
        (
            "replace all",
            FullRefresh,
            true,
            true,
            true,
            false,
            true,
            false,
            true,
        ),
        // …where nothing answered: nothing is erased.
        (
            "nothing answered",
            FullRefresh,
            true,
            true,
            false,
            false,
            false,
            true,
            true,
        ),
        // …where a provider failed and no remote one answered: kept.
        (
            "all failed",
            FullRefresh,
            true,
            true,
            true,
            true,
            false,
            true,
            true,
        ),
        // …where one failed but another answered: cleared.
        (
            "partly failed",
            FullRefresh,
            true,
            true,
            true,
            true,
            true,
            false,
            true,
        ),
        // Replace all without `RemoveOldMetadata`: the stored values fill.
        (
            "replace keeping",
            FullRefresh,
            true,
            false,
            true,
            false,
            true,
            true,
            true,
        ),
        // `ValidationOnly`: no replace.
        (
            "validation only",
            ValidationOnly,
            false,
            false,
            true,
            false,
            false,
            true,
            false,
        ),
    ];
    for (case, mode, replace_all, remove_old, any, failed, remote, keep_existing, replace) in cases
    {
        let options = MetadataRefreshOptions {
            metadata_refresh_mode: mode,
            image_refresh_mode: mode,
            replace_all_metadata: replace_all,
            remove_old_metadata: remove_old,
            ..MetadataRefreshOptions::default()
        };
        let answers = RefreshAnswers {
            any,
            failed,
            remote,
            local_locked: false,
        };
        assert_eq!(
            RefreshMerge::of(&options, answers),
            RefreshMerge {
                keep_existing,
                replace
            },
            "{case}"
        );
        // An NFO's `<lockdata>` (`isLocalLocked`) always replaces.
        let local_locked = RefreshAnswers {
            local_locked: true,
            ..answers
        };
        assert!(RefreshMerge::of(&options, local_locked).replace, "{case}");
    }
}

/// [`merge_refresh`] is the two calls in order: under "Search for missing
/// metadata" a stored value stays and a gap is filled; under an Identify's
/// replace a value no provider returned is cleared — never a locked one.
#[test]
fn merge_refresh_runs_both_calls_by_the_rule() {
    let stored = MetadataResult::of(BaseItemEntity {
        overview: Some("Mine".into()),
        tagline: Some("My tagline".into()),
        ..item(BaseItemKind::BoxSet)
    });
    let answer = || {
        MetadataResult::of(BaseItemEntity {
            name: Some("Provider".into()),
            overview: Some("Theirs".into()),
            community_rating: Some(7.0),
            ..item(BaseItemKind::BoxSet)
        })
    };
    let fill = merge_refresh(
        &stored,
        answer(),
        &[],
        RefreshMerge {
            keep_existing: true,
            replace: false,
        },
    );
    assert_eq!(fill.item.overview.as_deref(), Some("Mine"));
    assert_eq!(fill.item.community_rating, Some(7.0));
    let cleared = merge_refresh(
        &stored,
        answer(),
        &[MetadataField::Overview],
        RefreshMerge {
            keep_existing: false,
            replace: true,
        },
    );
    assert_eq!(cleared.item.overview.as_deref(), Some("Mine"), "locked");
    assert_eq!(cleared.item.tagline, None, "not returned: cleared");
    assert_eq!(cleared.item.name.as_deref(), Some("Provider"));
}

// --- MetadataServiceRefreshTests.cs `RefreshWithProviders_*`, transliterated ---
//
// `RefreshWithProviders_ForeignProviderId_ReplacedInLookupInfo` exercises
// `MergeNewData` (the lookup info handed to the next provider), which the
// scan's music pass ports: its transliteration sits beside that port in
// `ferrofin-core`'s `library_scan/music.rs`.

/// What one provider's `GetMetadata` did.
enum Answer {
    /// It threw (`RefreshResult.Failures++`).
    Threw,
    /// `HasMetadata = true` with this result.
    Found(Box<MetadataResult>),
}

/// A provider that answered with `result`.
fn found(result: MetadataResult) -> Answer {
    Answer::Found(Box::new(result))
}

/// A movie row carrying `name`.
fn movie(name: &str) -> BaseItemEntity {
    BaseItemEntity {
        name: Some(name.into()),
        ..item(BaseItemKind::Movie)
    }
}

/// `RefreshWithProviders` (`MetadataService.cs:761-928`) as upstream's
/// `TestMetadataService` drives it: a local provider, then the remote ones,
/// then the merge onto `existing`. The provider loop is the harness; the
/// rules under test are Ferrofin's — each answer taken into `temp` by
/// [`merge_data`] (a local one with `mergeMetadataSettings`, a remote one
/// without, both `replaceData = false`), then [`RefreshMerge::of`] and
/// [`merge_refresh`]. Returns `RefreshResult.Failures` and the item.
fn refresh_with_providers(
    existing: MetadataResult,
    options: &MetadataRefreshOptions,
    local: Option<MetadataResult>,
    remote: Vec<Answer>,
) -> (usize, MetadataResult) {
    let mut temp = MetadataResult::of(item(BaseItemKind::Movie));
    let mut answers = RefreshAnswers::default();
    if let Some(local) = local {
        merge_data(&local, &mut temp, &[], false, true);
        answers.any = true;
    }
    let mut failures = 0;
    if options.replace_all_metadata
        || matches!(
            options.metadata_refresh_mode,
            MetadataRefreshMode::Default | MetadataRefreshMode::FullRefresh
        )
    {
        for answer in remote {
            match answer {
                Answer::Threw => failures += 1,
                Answer::Found(result) => {
                    merge_data(&result, &mut temp, &[], false, false);
                    answers.remote = true;
                    answers.any = true;
                }
            }
        }
    }
    answers.failed = failures > 0;
    // `if (refreshResult.UpdateType > ItemUpdateType.None)`: nothing
    // answered, nothing merged.
    if !answers.any {
        return (failures, existing);
    }
    let locked = existing.locked_fields.clone();
    let merged = merge_refresh(&existing, temp, &locked, RefreshMerge::of(options, answers));
    (failures, merged)
}

/// "Replace all metadata" (or Identify): `FullRefresh` with
/// `ReplaceAllMetadata` and, when `remove_old`, `RemoveOldMetadata`.
fn replace_all(remove_old: bool) -> MetadataRefreshOptions {
    MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_metadata: true,
        remove_old_metadata: remove_old,
        ..MetadataRefreshOptions::default()
    }
}

/// `RefreshWithProviders_ReplaceAllMetadata_ErasesOldDataWhenAProviderAnswers`
/// (`[InlineData(false)]`, `[InlineData(true)]`): a provider failing does not
/// downgrade a `RemoveOldMetadata` replace to a merge when another one
/// answered — the old overview is erased.
#[rstest]
#[case(false)]
#[case(true)]
fn refresh_with_providers_replace_all_metadata_erases_old_data_when_a_provider_answers(
    #[case] all_providers_succeed: bool,
) {
    let existing = MetadataResult::of(BaseItemEntity {
        overview: Some("existing overview".into()),
        ..movie("Test Movie")
    });
    let failing = if all_providers_succeed {
        found(MetadataResult::of(item(BaseItemKind::Movie)))
    } else {
        Answer::Threw
    };
    let succeeding = found(MetadataResult::of(BaseItemEntity {
        tagline: Some("new tagline".into()),
        ..movie("Test Movie")
    }));
    let (failures, merged) = refresh_with_providers(
        existing,
        &replace_all(true),
        None,
        vec![failing, succeeding],
    );
    assert_eq!(failures, usize::from(!all_providers_succeed));
    assert_eq!(merged.item.tagline.as_deref(), Some("new tagline"));
    assert_eq!(merged.item.overview, None);
}

/// `RefreshWithProviders_ReplaceAllMetadata_KeepsExistingDataWhenEveryRemoteProviderFails`:
/// only the local provider answered, so erasing the overview would lose it
/// for good — it is kept, and the local answer still lands.
#[test]
fn refresh_with_providers_replace_all_metadata_keeps_existing_data_when_every_remote_provider_fails()
 {
    let existing = MetadataResult::of(BaseItemEntity {
        overview: Some("existing overview".into()),
        ..movie("Test Movie")
    });
    let local = MetadataResult::of(BaseItemEntity {
        tagline: Some("new tagline".into()),
        ..movie("Test Movie")
    });
    let (failures, merged) = refresh_with_providers(
        existing,
        &replace_all(true),
        Some(local),
        vec![Answer::Threw],
    );
    assert_eq!(failures, 1);
    assert_eq!(merged.item.tagline.as_deref(), Some("new tagline"));
    assert_eq!(merged.item.overview.as_deref(), Some("existing overview"));
}

/// `RefreshWithProviders_ForeignProviderId_NotStored`: an IMDb person id
/// filed under `Tmdb` is dropped; the valid IMDb id is stored.
#[test]
fn refresh_with_providers_foreign_provider_id_not_stored() {
    let answer = MetadataResult {
        provider_ids: ids(&[("Tmdb", "nm0000123"), ("Imdb", "tt0113375")]),
        ..MetadataResult::of(movie("Test Movie"))
    };
    let (_, merged) = refresh_with_providers(
        MetadataResult::of(movie("Test Movie")),
        &replace_all(false),
        None,
        vec![found(answer)],
    );
    assert!(
        !merged
            .provider_ids
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("Tmdb")),
        "{:?}",
        merged.provider_ids
    );
    assert_eq!(merged.provider_ids, ids(&[("Imdb", "tt0113375")]));
}

/// `RefreshWithProviders_ForeignPersonProviderId_NotStored`
/// (`[InlineData(true)]`, `[InlineData(false)]` for `ReplaceAllMetadata`):
/// the merged credit keeps no TMDB id that cannot be one.
///
/// Ferrofin's credit carries one person id, TMDB's, as a number, so
/// upstream's `nm0000123` has no form here; the ids `IsValidProviderId(Tmdb,
/// …)` rejects that it can hold stand in for it (`#[values]`: zero and
/// `-11`, as in `IsValidProviderId_ChecksKnownFormats`, and one past
/// `int.MaxValue`). The case's other half — the credit keeps its valid IMDb
/// id — has no counterpart: a credit carries no IMDb id.
#[rstest]
#[case(true)]
#[case(false)]
fn refresh_with_providers_foreign_person_provider_id_not_stored(
    #[case] replace_all_metadata: bool,
    #[values(0, -11, i64::from(i32::MAX) + 1)] foreign_id: i64,
) {
    let actor = || PeopleEntity {
        name: "Some Actor".into(),
        person_type: Some("Actor".into()),
        ..PeopleEntity::default()
    };
    let existing = MetadataResult {
        people: Some(vec![actor()]),
        ..MetadataResult::of(movie("Test Movie"))
    };
    let answer = MetadataResult {
        people: Some(vec![PeopleEntity {
            provider_id: Some(foreign_id),
            ..actor()
        }]),
        ..MetadataResult::of(movie("Test Movie"))
    };
    let options = MetadataRefreshOptions {
        replace_all_metadata,
        ..replace_all(false)
    };
    let (_, merged) = refresh_with_providers(existing, &options, None, vec![found(answer)]);
    let people = merged.people.expect("people");
    assert_eq!(people.len(), 1, "{people:?}");
    assert_eq!(people[0].name, "Some Actor");
    assert_eq!(people[0].provider_id, None);
}

/// The sort key a refresh settles, by kind: a forced one wins, an episode, a
/// season and a track sort by number, a person by its name verbatim
/// (`Person.EnableAlphaNumericSorting => false`), anything else by the
/// alphanumeric pipeline.
#[test]
fn settle_sort_name_follows_create_sort_name_per_kind() {
    let settled = |mut row: BaseItemEntity| {
        settle_sort_name(&mut row);
        row.sort_name
    };
    assert_eq!(
        settled(BaseItemEntity {
            name: Some("The Matrix Collection".into()),
            ..item(BaseItemKind::BoxSet)
        })
        .as_deref(),
        Some("matrix collection")
    );
    assert_eq!(
        settled(BaseItemEntity {
            name: Some("The Rock".into()),
            ..item(BaseItemKind::Person)
        })
        .as_deref(),
        Some("The Rock")
    );
    assert_eq!(
        settled(BaseItemEntity {
            name: Some("Pilot".into()),
            parent_index_number: Some(1),
            index_number: Some(2),
            ..item(BaseItemKind::Episode)
        })
        .as_deref(),
        Some("001 - 0002 - Pilot")
    );
    assert_eq!(
        settled(BaseItemEntity {
            name: Some("Season 3".into()),
            index_number: Some(3),
            ..item(BaseItemKind::Season)
        })
        .as_deref(),
        Some("0003")
    );
    assert_eq!(
        settled(BaseItemEntity {
            name: Some("Song".into()),
            index_number: Some(4),
            ..item(BaseItemKind::Audio)
        })
        .as_deref(),
        Some("0004 - Song")
    );
    assert_eq!(
        settled(BaseItemEntity {
            name: Some("Anything".into()),
            forced_sort_name: Some("The Zed".into()),
            ..item(BaseItemKind::Movie)
        })
        .as_deref(),
        Some("zed")
    );
}
