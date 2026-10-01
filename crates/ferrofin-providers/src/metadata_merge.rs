//! The metadata merge — port of `MetadataService.MergeData` /
//! `MergeBaseItemData` (`MediaBrowser.Providers/Manager/MetadataService.cs`,
//! upstream master `208c278b75`, `:1141-1515`) and the per-kind overrides in
//! the `*MetadataService` classes.
//!
//! One function, [`merge_data`], decides for every provider-owned field
//! whether `source` replaces `target`, fills it only where it is empty, or is
//! unioned with it. Upstream's refresh modes are the two calls
//! `RefreshWithProviders` (`:897-915`) makes with it:
//!
//! - **Default** (a library scan): `merge_data(existing → provider, fill)`
//!   then `merge_data(provider → item, replace)`; provider values win and
//!   the stored values fill whatever the providers did not return.
//! - **`FullRefresh`** ("Search for missing metadata"): the second call with
//!   `replace_data = false` — stored values stay, providers fill gaps.
//! - **`ReplaceAll`** ("Replace all metadata" with `RemoveOldMetadata`): only
//!   the second call with `replace_data = true` — the provider result
//!   replaces the row and clears what it did not return.
//!
//! [`BaseItemEntity`] keeps several of upstream's properties in the `Data`
//! JSON blob (`RemoteTrailers`, `Status`, `AirDays`, …); those keys get their
//! upstream rule here too, and every other `Data` key is merged key-wise
//! (never replaced wholesale).
//!
//! `HomePageUrl` lives in `Data`. A credit's `SortOrder` rides on its
//! [`PeopleEntity`] and is merged by [`merge_people`] (stored as
//! `PeopleBaseItemMap.SortOrder`). `LockedFields` lives in its own table
//! (`BaseItemMetadataFields`), so it rides on [`MetadataResult`].
//!
//! It lives here, beside the providers, as upstream's lives in
//! `MediaBrowser.Providers`: the library scan (`ferrofin-core`) and the
//! single-item refresh (`provider_manager`) both depend on this crate, so both
//! can merge through the one rule set.

use std::collections::{HashMap, HashSet};

use ferrofin_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::MetadataField;
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
use serde_json::{Map, Value};

/// A `Data` column value's JSON object; `NULL`, empty and malformed payloads
/// (and non-object JSON) are an empty object.
fn parse_data(data: Option<&str>) -> Map<String, Value> {
    match data.and_then(|d| serde_json::from_str::<Value>(d).ok()) {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Upstream's `MetadataResult<T>`: an item plus the parts of a provider
/// result that are not columns of its row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetadataResult {
    /// The item row.
    pub item: BaseItemEntity,
    /// The credited people, when the result carries any (`null` upstream
    /// means "no people", which a later merge may fill).
    pub people: Option<Vec<PeopleEntity>>,
    /// The external ids (`BaseItemProviders`), as `(provider, value)` pairs.
    pub provider_ids: Vec<(String, String)>,
    /// The item's `LockedFields` (`BaseItemMetadataFields`), which the
    /// metadata-settings half of the merge unions.
    pub locked_fields: Vec<MetadataField>,
}

impl MetadataResult {
    /// A result holding only an item row.
    #[must_use]
    pub fn of(item: BaseItemEntity) -> Self {
        Self {
            item,
            people: None,
            provider_ids: Vec::new(),
            locked_fields: Vec::new(),
        }
    }
}

/// Merges `source` into `target` — port of `MergeData`.
///
/// `replace_data` picks replace (`true`) or fill-missing (`false`) for every
/// scalar field, and replace or union for the list fields that union
/// (`Studios`, `Tags`, `ProductionLocations`, `AlbumArtists`, `Artists`,
/// `RemoteTrailers`). A field in `locked_fields` is never touched (upstream
/// checks exactly `Name`, `Genres`, `OfficialRating`, `Overview`, `Cast`,
/// `Runtime`, `Studios`, `Tags` and `ProductionLocations`).
///
/// `merge_metadata_settings` additionally carries `IsLocked`,
/// `LockedFields` (a union), `DateCreated`, `DateModified` and the preferred
/// metadata language/country — upstream passes it on the provider → item
/// merge only.
pub fn merge_data(
    source: &MetadataResult,
    target: &mut MetadataResult,
    locked_fields: &[MetadataField],
    replace_data: bool,
    merge_metadata_settings: bool,
) {
    let unlocked = |field: MetadataField| !locked_fields.contains(&field);
    merge_base_item(
        &source.item,
        &mut target.item,
        &unlocked,
        replace_data,
        merge_metadata_settings,
    );
    if unlocked(MetadataField::Cast) {
        merge_people_field(source.people.as_deref(), &mut target.people, replace_data);
    }
    if merge_metadata_settings {
        // `if (target.LockedFields.Length == 0) target.LockedFields =
        // source.LockedFields; else target.LockedFields =
        // target.LockedFields.Concat(source.LockedFields).Distinct()`
        // (`MetadataService.cs:1372-1379`).
        for field in &source.locked_fields {
            if !target.locked_fields.contains(field) {
                target.locked_fields.push(*field);
            }
        }
    }
    target.provider_ids =
        merge_provider_ids(&source.provider_ids, &target.provider_ids, replace_data);
    merge_kind_specific(
        &source.item,
        &mut target.item,
        replace_data,
        merge_metadata_settings,
    );
}

/// Every field a `LockedFields` set can name (`MetadataField.cs`). What a
/// caller treats as locked when an item's stored set could not be read, so a
/// read failure never lets a write overwrite a field the user locked.
pub const ALL_LOCKABLE_FIELDS: &[MetadataField] = &MetadataField::ALL;

/// What one refresh pass's providers did, as the merge of
/// `RefreshWithProviders` (`MetadataService.cs:895-917`) reads it.
// Independent facts about the pass, each read by its own rule.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefreshAnswers {
    /// `refreshResult.UpdateType > None`: a pre-refresh custom provider (the
    /// probe), a local reader or a remote provider returned something.
    pub any: bool,
    /// `refreshResult.Failures > 0`: a provider failed.
    pub failed: bool,
    /// `hasRemoteMetadata`: a remote provider answered with metadata.
    pub remote: bool,
    /// `isLocalLocked`: the local metadata carried `<lockdata>`.
    pub local_locked: bool,
}

/// The two `MergeData` calls `RefreshWithProviders` makes with a pass's
/// provider result (`MetadataService.cs:895-917`): whether the stored values
/// fill the result first, and whether the result then replaces the stored
/// values or only fills what they lack. The library scan and the single-item
/// refresh both merge by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshMerge {
    /// "Add existing metadata to provider result if it does not exist
    /// there": the stored values fill the provider result before it is
    /// merged back. Off for `RemoveOldMetadata`, unless a provider failed and
    /// no remote provider answered (erasing is only safe when something
    /// replaces the old values) — and never off when nothing answered, where
    /// upstream merges nothing at all, so the row stays as it was.
    pub keep_existing: bool,
    /// `shouldReplace` (or `isLocalLocked`): the provider result replaces the
    /// stored values. Off for "Search for missing metadata" (`FullRefresh`
    /// without `ReplaceAllMetadata`), where it only fills what is empty.
    pub replace: bool,
}

impl RefreshMerge {
    /// The Default refresh ("Scan for new and updated files"): the provider
    /// result replaces, the stored values fill what it lacks.
    pub const DEFAULT: Self = Self {
        keep_existing: true,
        replace: true,
    };

    /// The merge a pass refreshing with `options` runs, given what its
    /// providers did.
    #[must_use]
    pub fn of(options: &MetadataRefreshOptions, answers: RefreshAnswers) -> Self {
        let mode = options.metadata_refresh_mode;
        let fetches = matches!(
            mode,
            MetadataRefreshMode::Default | MetadataRefreshMode::FullRefresh
        );
        Self {
            keep_existing: !options.remove_old_metadata
                || !answers.any
                || (answers.failed && !answers.remote),
            replace: answers.local_locked
                || (fetches && options.replace_all_metadata)
                || (mode == MetadataRefreshMode::Default && !options.replace_all_metadata),
        }
    }
}

/// `RefreshWithProviders`' merge of a pass's provider result `temp` onto the
/// `stored` item, by `merge`: the stored values fill `temp` first when
/// `merge.keep_existing` (`MergeData(metadata, temp, [], false, false)`),
/// then `temp` is merged onto the stored item with its `locked_fields`
/// (`MergeData(temp, metadata, item.LockedFields, shouldReplace, true)`).
///
/// `temp` is what the providers returned, starting from what upstream's
/// `temp` starts with (the item's `ParentIndexNumber` and preferred metadata
/// language and country, `:790-795`) where the caller has them.
#[must_use]
pub fn merge_refresh(
    stored: &MetadataResult,
    mut temp: MetadataResult,
    locked_fields: &[MetadataField],
    merge: RefreshMerge,
) -> MetadataResult {
    if merge.keep_existing {
        merge_data(stored, &mut temp, &[], false, false);
    }
    let mut target = stored.clone();
    merge_data(&temp, &mut target, locked_fields, merge.replace, true);
    target
}

/// `BaseItem.SortName` as upstream saves it (`BaseItem.cs:540-561`,
/// `AfterMetadataRefresh` resetting the cache): the `ForcedSortName`'s key
/// when there is one, else the per-kind `CreateSortName` of the saved name —
/// derived last, so an episode's or track's key embeds its final numbers.
/// Upstream persists no other explicit sort key. Every refresh settles the
/// row it saves with it, so a row the library scan saves and one the
/// single-item refresh saves carry the same key.
pub fn settle_sort_name(row: &mut BaseItemEntity) {
    if let Some(forced) = row.forced_sort_name.clone().filter(|f| !f.is_empty()) {
        row.sort_name = Some(forced_sort_name(row, &forced));
    } else if let Some(name) = row.name.clone() {
        row.sort_name = Some(derived_sort_name(row, &name));
    }
}

/// `EnableAlphaNumericSorting` (`BaseItem.cs`, overridden to `false` by
/// `Person.cs` only): whether `CreateSortName` runs the alphanumeric
/// pipeline, or keeps the name verbatim apart from `TrimStart()`.
fn alpha_numeric_sorting(kind: Option<BaseItemKind>) -> bool {
    kind != Some(BaseItemKind::Person)
}

/// The `SortName` a non-empty `ForcedSortName` yields for `entity` — 12.0's
/// `GetSortName(ForcedSortName, EnableAlphaNumericSorting, config)`, so a
/// `Person` keeps it verbatim and every other kind cleans it like a derived
/// key. The per-kind `CreateSortName` overrides in [`derived_sort_name`] do not
/// apply to the forced branch.
#[must_use]
pub fn forced_sort_name(entity: &BaseItemEntity, forced: &str) -> String {
    let kind = BaseItemKind::from_stored_type_name(&entity.type_);
    ferrofin_util::sort_name::get_sort_name(forced, alpha_numeric_sorting(kind))
}

/// The sort name a row derives from `title`, honouring the per-kind
/// `CreateSortName` overrides (episodes and seasons sort by number, tracks by
/// disc and track number, a person by the name verbatim, everything else by
/// the name pipeline).
#[must_use]
pub fn derived_sort_name(entity: &BaseItemEntity, title: &str) -> String {
    match entity.type_.rsplit('.').next().unwrap_or(&entity.type_) {
        "Episode" => episode_sort_name(entity.parent_index_number, entity.index_number, title),
        "Season" => season_sort_name(entity.index_number, title),
        "Audio" | "AudioBook" => {
            audio_sort_name(entity.parent_index_number, entity.index_number, title)
        }
        _ => ferrofin_util::sort_name::get_sort_name(
            title,
            alpha_numeric_sorting(BaseItemKind::from_stored_type_name(&entity.type_)),
        ),
    }
}

/// Port of C# `Episode.CreateSortName`: the zero-padded season/episode numbers
/// ahead of the title (`001 - 0004 - The Title`).
///
/// This override REPLACES the generic name-derived sort name — an episode must
/// sort by its position in the season, never alphabetically by title. Clients
/// build their play queue from the season's episodes in `SortName` order, so a
/// title-derived sort name scrambles the queue: "next episode" points at the
/// wrong item, and at the alphabetically-last episode there is no next at all
/// (a dead Next button and no autoplay).
#[must_use]
pub fn episode_sort_name(parent_index: Option<i64>, index: Option<i64>, name: &str) -> String {
    let season = parent_index.map_or_else(String::new, |n| format!("{n:03} - "));
    let episode = index.map_or_else(String::new, |n| format!("{n:04} - "));
    format!("{season}{episode}{name}")
}

/// Port of C# `Season.CreateSortName`: the zero-padded season number, or the
/// name when the season has no number (so `Specials` (0000) sorts first).
#[must_use]
pub fn season_sort_name(index: Option<i64>, name: &str) -> String {
    index.map_or_else(
        || ferrofin_util::sort_name::create_sort_name(name),
        |n| format!("{n:04}"),
    )
}

/// Port of C# `Audio.CreateSortName` (v10.11.8
/// `MediaBrowser.Controller/Entities/Audio/Audio.cs`):
/// `ParentIndexNumber.ToString("0000 - ") + IndexNumber.ToString("0000 - ") + Name`,
/// each prefix omitted when its number is absent, and the **raw** name appended
/// — `Audio` overrides `CreateSortName` outright, so the alphanumeric
/// lowercase/pad pipeline never runs on a track.
///
/// A track stored with the alphanumeric key instead sorts in a different place
/// than Jellyfin puts it, which reorders every album and every search-hint page
/// that contains one.
#[must_use]
pub fn audio_sort_name(parent_index: Option<i64>, index: Option<i64>, name: &str) -> String {
    let disc = parent_index.map_or_else(String::new, |n| format!("{n:04} - "));
    let track = index.map_or_else(String::new, |n| format!("{n:04} - "));
    format!("{disc}{track}{name}")
}

/// Whether a string field is empty in upstream's `string.IsNullOrEmpty`
/// sense.
fn is_empty(value: Option<&str>) -> bool {
    value.is_none_or(str::is_empty)
}

/// `string.IsNullOrWhiteSpace`.
fn is_blank(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.trim().is_empty())
}

/// `if (replaceData || string.IsNullOrEmpty(target.X)) target.X = source.X;`
fn text(source: Option<&String>, target: &mut Option<String>, replace: bool) {
    if replace || is_empty(target.as_deref()) {
        target.clone_from(&source.cloned());
    }
}

/// `if (replaceData || !target.X.HasValue) target.X = source.X;`
fn value<T: Clone>(source: Option<&T>, target: &mut Option<T>, replace: bool) {
    if replace || target.is_none() {
        *target = source.cloned();
    }
}

/// The values of a `|`-joined list column, as upstream's string array.
fn list(value: Option<&str>) -> Vec<&str> {
    value
        .unwrap_or_default()
        .split('|')
        .filter(|v| !v.is_empty())
        .collect()
}

/// `target.Concat(source).Distinct(StringComparer.OrdinalIgnoreCase)`.
fn union(target: Option<&str>, source: Option<&str>) -> Option<String> {
    let mut seen = HashSet::new();
    let values: Vec<&str> = list(target)
        .into_iter()
        .chain(list(source))
        .filter(|v| seen.insert(ferrofin_util::string_extensions::upper_invariant(v)))
        .collect();
    (!values.is_empty()).then(|| values.join("|"))
}

/// The replace-or-union rule of `Studios`, `Tags`, `ProductionLocations`
/// (`:1271-1305`): replace (possibly with nothing) on `replaceData` or an
/// empty target, otherwise union.
fn replace_or_union(source: Option<&String>, target: &mut Option<String>, replace: bool) {
    if replace || list(target.as_deref()).is_empty() {
        target.clone_from(&source.cloned());
    } else {
        *target = union(target.as_deref(), source.map(String::as_str));
    }
}

/// Whether upstream's item class for `kind` derives from `Audio`, `Video` or
/// `Book` — the classes whose runtime comes from their own media, never
/// from a provider (`MergeBaseItemData` `:1260-1269`).
fn owns_its_runtime(kind: Option<BaseItemKind>) -> bool {
    matches!(
        kind,
        Some(
            BaseItemKind::Audio
                | BaseItemKind::AudioBook
                | BaseItemKind::Book
                | BaseItemKind::Movie
                | BaseItemKind::Episode
                | BaseItemKind::Trailer
                | BaseItemKind::MusicVideo
                | BaseItemKind::Video
        )
    )
}

/// `IHasAlbumArtist` (`Audio` and its `AudioBook`, `MusicAlbum`,
/// `MusicVideo`).
fn has_album_artist(kind: Option<BaseItemKind>) -> bool {
    matches!(
        kind,
        Some(
            BaseItemKind::Audio
                | BaseItemKind::AudioBook
                | BaseItemKind::MusicAlbum
                | BaseItemKind::MusicVideo
        )
    )
}

/// `MergeBaseItemData` over the row's columns and its `Data` blob.
fn merge_base_item(
    source: &BaseItemEntity,
    target: &mut BaseItemEntity,
    unlocked: &dyn Fn(MetadataField) -> bool,
    replace: bool,
    merge_metadata_settings: bool,
) {
    // "Safeguard against incoming data having an empty name."
    if unlocked(MetadataField::Name)
        && (replace || is_empty(target.name.as_deref()))
        && !is_blank(source.name.as_deref())
    {
        target.name.clone_from(&source.name);
    }
    text(
        source.original_title.as_ref(),
        &mut target.original_title,
        replace,
    );
    text(
        source.original_language.as_ref(),
        &mut target.original_language,
        replace,
    );
    value(
        source.community_rating.as_ref(),
        &mut target.community_rating,
        replace,
    );
    value(source.end_date.as_ref(), &mut target.end_date, replace);
    if unlocked(MetadataField::Genres) && (replace || list(target.genres.as_deref()).is_empty()) {
        target.genres.clone_from(&source.genres);
    }
    value(
        source.index_number.as_ref(),
        &mut target.index_number,
        replace,
    );
    if unlocked(MetadataField::OfficialRating) {
        text(
            source.official_rating.as_ref(),
            &mut target.official_rating,
            replace,
        );
    }
    text(
        source.custom_rating.as_ref(),
        &mut target.custom_rating,
        replace,
    );
    text(source.tagline.as_ref(), &mut target.tagline, replace);
    if unlocked(MetadataField::Overview) {
        text(source.overview.as_ref(), &mut target.overview, replace);
    }
    value(
        source.parent_index_number.as_ref(),
        &mut target.parent_index_number,
        replace,
    );
    value(
        source.premiere_date.as_ref(),
        &mut target.premiere_date,
        replace,
    );
    value(
        source.production_year.as_ref(),
        &mut target.production_year,
        replace,
    );
    if unlocked(MetadataField::Runtime)
        && (replace || target.run_time_ticks.is_none())
        && !owns_its_runtime(BaseItemKind::from_stored_type_name(&target.type_))
    {
        target.run_time_ticks = source.run_time_ticks;
    }
    merge_union_lists(source, target, unlocked, replace);
    value(
        source.critic_rating.as_ref(),
        &mut target.critic_rating,
        replace,
    );
    if (replace || is_empty(target.forced_sort_name.as_deref()))
        && !is_empty(source.forced_sort_name.as_deref())
    {
        target.forced_sort_name.clone_from(&source.forced_sort_name);
    }
    // `RemoteTrailers`, `Video3DFormat` (`MergeVideoInfo`), `DisplayOrder`
    // (`MergeDisplayOrder`) and the per-kind blob properties live in `Data`.
    target.data = merge_data_blob(source.data.as_deref(), target.data.as_deref(), replace);
    if merge_metadata_settings {
        merge_settings(source, target, replace);
    }
}

/// The `mergeMetadataSettings` half of `MergeBaseItemData` (`:1365-1402`).
fn merge_settings(source: &BaseItemEntity, target: &mut BaseItemEntity, replace: bool) {
    if replace || !target.is_locked {
        target.is_locked = target.is_locked || source.is_locked;
    }
    // `DateTime.MinValue` is upstream's "not set"; here it is `None`.
    if source.date_created.is_some() {
        target.date_created = source.date_created;
    }
    if replace || source.date_modified.is_some() {
        target.date_modified = source.date_modified;
    }
    text(
        source.preferred_metadata_country_code.as_ref(),
        &mut target.preferred_metadata_country_code,
        replace,
    );
    text(
        source.preferred_metadata_language.as_ref(),
        &mut target.preferred_metadata_language,
        replace,
    );
}

/// The list fields that union on a fill merge — `Studios`, `Tags`,
/// `ProductionLocations` (`:1271-1305`) and `MergeAlbumArtist`
/// (`:1488-1502`).
fn merge_union_lists(
    source: &BaseItemEntity,
    target: &mut BaseItemEntity,
    unlocked: &dyn Fn(MetadataField) -> bool,
    replace: bool,
) {
    if unlocked(MetadataField::Studios) {
        replace_or_union(source.studios.as_ref(), &mut target.studios, replace);
    }
    if unlocked(MetadataField::Tags) {
        replace_or_union(source.tags.as_ref(), &mut target.tags, replace);
    }
    if unlocked(MetadataField::ProductionLocations) {
        replace_or_union(
            source.production_locations.as_ref(),
            &mut target.production_locations,
            replace,
        );
    }
    // `MergeAlbumArtist` (`:1488-1502`).
    let kind = BaseItemKind::from_stored_type_name(&target.type_);
    if has_album_artist(kind)
        && has_album_artist(BaseItemKind::from_stored_type_name(&source.type_))
    {
        if replace || list(target.album_artists.as_deref()).is_empty() {
            target.album_artists.clone_from(&source.album_artists);
        } else if !list(source.album_artists.as_deref()).is_empty() {
            target.album_artists = union(
                target.album_artists.as_deref(),
                source.album_artists.as_deref(),
            );
        }
    }
}

/// The per-kind `MergeData` overrides that touch columns: `Artists` and
/// `Album` (`AudioMetadataService`, `AlbumMetadataService`,
/// `MusicVideoMetadataService`, `AudioBookMetadataService`), a book's
/// `SeriesName` (`BookMetadataService`) and an episode's season number
/// (`EpisodeMetadataService`). Their blob-held properties are in
/// [`DATA_RULES`].
fn merge_kind_specific(
    source: &BaseItemEntity,
    target: &mut BaseItemEntity,
    replace: bool,
    merge_metadata_settings: bool,
) {
    match BaseItemKind::from_stored_type_name(&target.type_) {
        Some(BaseItemKind::Audio | BaseItemKind::MusicAlbum | BaseItemKind::MusicVideo) => {
            if replace || list(target.artists.as_deref()).is_empty() {
                target.artists.clone_from(&source.artists);
            } else {
                target.artists = union(target.artists.as_deref(), source.artists.as_deref());
            }
            if !matches!(
                BaseItemKind::from_stored_type_name(&target.type_),
                Some(BaseItemKind::MusicAlbum)
            ) {
                text(source.album.as_ref(), &mut target.album, replace);
            }
        }
        Some(BaseItemKind::AudioBook) => {
            if replace || list(target.artists.as_deref()).is_empty() {
                target.artists.clone_from(&source.artists);
            }
            text(source.album.as_ref(), &mut target.album, replace);
        }
        Some(BaseItemKind::Book) => {
            text(
                source.series_name.as_ref(),
                &mut target.series_name,
                replace,
            );
        }
        // "Episode season numbers can be set from path parsing before local
        // metadata is merged. When a provider supplies an explicit season,
        // prefer it" — only on the merges that carry settings.
        Some(BaseItemKind::Episode)
            if merge_metadata_settings && source.parent_index_number.is_some() =>
        {
            target.parent_index_number = source.parent_index_number;
        }
        _ => {}
    }
}

/// How one `Data`-blob property merges.
#[derive(Clone, Copy)]
enum DataRule {
    /// `if (replaceData || target empty) target.X = source.X` — replace may
    /// clear it.
    Plain,
    /// `RemoteTrailers`: replace, or union by `Url`.
    Trailers,
    /// `DisplayOrder`: only a non-blank source value is taken.
    NonBlankSource,
    /// `Video3DFormat`: only a source that has a value is taken.
    PresentSource,
}

/// The upstream properties Ferrofin keeps in `Data`, with their merge rule.
/// Every other key is file- or custom-provider-derived and merges key-wise
/// (see [`merge_data_blob`]).
const DATA_RULES: &[(&str, DataRule)] = &[
    ("RemoteTrailers", DataRule::Trailers),
    ("HomePageUrl", DataRule::Plain),
    ("DisplayOrder", DataRule::NonBlankSource),
    ("Video3DFormat", DataRule::PresentSource),
    // `SeriesMetadataService.MergeData`.
    ("AirTime", DataRule::Plain),
    ("Status", DataRule::Plain),
    ("AirDays", DataRule::Plain),
    // `EpisodeMetadataService.MergeData`.
    ("AirsBeforeSeasonNumber", DataRule::Plain),
    ("AirsAfterSeasonNumber", DataRule::Plain),
    ("AirsBeforeEpisodeNumber", DataRule::Plain),
    ("IndexNumberEnd", DataRule::Plain),
    // `MovieMetadataService.MergeData`.
    ("CollectionName", DataRule::Plain),
    // `TrailerMetadataService.MergeData`.
    ("TrailerTypes", DataRule::Plain),
];

/// Whether a blob value is "empty" for the fill rules: absent, `null`, `""`
/// or `[]`.
fn blob_empty(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        Some(_) => false,
    }
}

/// The union of two `RemoteTrailers` arrays, deduplicated by `Url`
/// (`DistinctBy(t => t.Url)`, first occurrence kept).
fn union_trailers(target: &Value, source: Option<&Value>) -> Value {
    let mut seen = HashSet::new();
    let entries = target
        .as_array()
        .into_iter()
        .flatten()
        .chain(source.and_then(Value::as_array).into_iter().flatten())
        .filter(|entry| seen.insert(entry.get("Url").and_then(Value::as_str).map(str::to_owned)))
        .cloned()
        .collect();
    Value::Array(entries)
}

/// Merges two `Data` column values key by key; returns `target` unchanged
/// (byte for byte) when no key changes, so an unchanged row is not
/// reserialized.
fn merge_data_blob(source: Option<&str>, target: Option<&str>, replace: bool) -> Option<String> {
    let src = parse_data(source);
    let mut out = parse_data(target);
    let before = out.clone();
    for (key, rule) in DATA_RULES {
        let incoming = src.get(*key);
        let current = out.get(*key);
        let next: Option<Value> = match rule {
            DataRule::Plain => {
                if replace || blob_empty(current) {
                    incoming.cloned()
                } else {
                    continue;
                }
            }
            DataRule::Trailers => {
                if replace || blob_empty(current) {
                    incoming.cloned()
                } else {
                    Some(union_trailers(current.unwrap_or(&Value::Null), incoming))
                }
            }
            DataRule::NonBlankSource => {
                let take = (replace || blob_empty(current))
                    && incoming
                        .and_then(Value::as_str)
                        .is_some_and(|v| !v.trim().is_empty());
                if !take {
                    continue;
                }
                incoming.cloned()
            }
            DataRule::PresentSource => {
                if blob_empty(incoming) || !(replace || blob_empty(current)) {
                    continue;
                }
                incoming.cloned()
            }
        };
        match next {
            Some(v) => {
                out.insert((*key).to_owned(), v);
            }
            None => {
                out.remove(*key);
            }
        }
    }
    // Every other key (`VideoType`, EXIF fields, …) is set on the item by the
    // resolver or a custom provider, not by `MergeData`: the source's value is
    // taken when it has one (fill-only when not replacing), and a key only
    // the target holds is kept.
    for (key, v) in &src {
        if DATA_RULES.iter().any(|(k, _)| k == key) {
            continue;
        }
        if replace || blob_empty(out.get(key)) {
            out.insert(key.clone(), v.clone());
        }
    }
    if out == before {
        return target.map(str::to_owned);
    }
    if out.is_empty() && target.is_none() {
        return None;
    }
    serde_json::to_string(&Value::Object(out))
        .ok()
        .or_else(|| target.map(str::to_owned))
}

/// The people part of `MergeBaseItemData` (`:1235-1249`): replace on
/// `replaceData` or an empty target, else enrich the target's people from
/// the source's (`MergePeople`) without adding anyone.
fn merge_people_field(
    source: Option<&[PeopleEntity]>,
    target: &mut Option<Vec<PeopleEntity>>,
    replace: bool,
) {
    let source: Option<Vec<PeopleEntity>> = source.map(remove_invalid_provider_ids);
    if let Some(people) = target.as_mut() {
        *people = remove_invalid_provider_ids(people);
    }
    if replace || target.as_ref().is_none_or(Vec::is_empty) {
        *target = source;
    } else if let (Some(source), Some(target)) = (source, target.as_mut())
        && !source.is_empty()
    {
        merge_people(&source, target);
    }
}

/// `RemoveInvalidProviderIds` over Ferrofin's one person id (the TMDB id):
/// kept only when `IsValidProviderId(Tmdb, id)` holds — a positive number
/// that fits an `int` ([`is_positive_number`]).
fn remove_invalid_provider_ids(people: &[PeopleEntity]) -> Vec<PeopleEntity> {
    people
        .iter()
        .cloned()
        .map(|mut p| {
            if p.provider_id
                .is_some_and(|id| !i32::try_from(id).is_ok_and(|id| id > 0))
            {
                p.provider_id = None;
            }
            p
        })
        .collect()
}

/// The name key `MergePeople` groups by: diacritics removed, compared
/// ordinal-ignore-case.
fn person_key(name: &str) -> String {
    ferrofin_util::string_extensions::upper_invariant(
        &ferrofin_util::string_extensions::remove_diacritics(name),
    )
}

/// The credits a refresh pass saves — `metadata.People` after
/// `RefreshWithProviders`, which `SaveItemAsync` writes whole when it is not
/// null (`MetadataService.cs:320-324`) and leaves the stored credits alone
/// when it is. `None` means "keep what is stored"; `Some` replaces it, an
/// empty list clearing it.
///
/// - `local`: the first local reader that found metadata, its credits in
///   `temp` (`MergeData(localItem, temp, [], false, true)`, `:850`). An NFO
///   always yields a list, possibly empty (`BaseNfoParser` calls
///   `ResetPeople`); `None` when no local reader found anything.
/// - `answers`: each remote provider that answered, in the order it ran,
///   with its `People` — `None` for a null one. What a provider yields is its
///   own: TMDB and OMDb only ever `AddPerson`, so an answer of theirs that
///   credits nobody is null; the TVDB plugin calls `ResetPeople` on every
///   answer (`TvdbSeriesProvider.cs:228`, `TvdbEpisodeProvider.cs:225`,
///   `TvdbMovieProvider.cs:363`, jellyfin-plugin-tvdb `5c4592f`), so its
///   answer is a list, empty when it credits nobody. `ExecuteRemoteProviders`
///   folds each into `temp` by `MergeData(result, temp, [], false, false)`
///   (`:1003`, people half `:1235-1249`): when `temp`'s credits are null or
///   empty they become the answer's, even a null one; otherwise a non-empty
///   answer enriches them through `MergePeople` (a missing TMDB person id,
///   image, role or sort order) and adds nobody.
/// - `keep_existing`: [`RefreshMerge::keep_existing`]. "Add existing metadata
///   to provider result" (`MergeData(metadata, temp, [], false, false)`,
///   `:905`) finds `metadata.People` null — `RefreshMetadata` never loads the
///   stored credits (`:145-148`) — so it turns an empty `temp.People` into
///   null: outside "Replace all metadata" an empty result keeps the stored
///   credits.
///
/// Every person id `IsValidProviderId` rejects is dropped from both sides
/// first (`RemoveInvalidProviderIds`).
#[must_use]
pub fn pass_credits(
    local: Option<Vec<PeopleEntity>>,
    answers: &[Option<Vec<PeopleEntity>>],
    keep_existing: bool,
) -> Option<Vec<PeopleEntity>> {
    let mut temp = local;
    if let Some(people) = temp.as_mut() {
        *people = remove_invalid_provider_ids(people);
    }
    for answer in answers {
        merge_people_field(answer.as_deref(), &mut temp, false);
    }
    if keep_existing {
        temp.filter(|people| !people.is_empty())
    } else {
        temp
    }
}

/// `MergePeople` (`:1429-1470`): for each target person, the same-named
/// source person at the same position (or the first one) fills the
/// target's missing provider id, image, role and sort order. Nobody is
/// added. People pair by name alone, whatever their type: a director who
/// also acts can take the actor credit's role, as upstream's lookup does.
///
/// The source is keyed once (`source.ToLookup(…)`), so a cast of `T` people
/// merged from one of `S` costs `O(T + S)` name keys.
pub fn merge_people(source: &[PeopleEntity], target: &mut [PeopleEntity]) {
    let mut by_name: HashMap<String, Vec<&PeopleEntity>> = HashMap::with_capacity(source.len());
    for person in source {
        by_name
            .entry(person_key(&person.name))
            .or_default()
            .push(person);
    }
    let mut positions: HashMap<String, usize> = HashMap::new();
    for person in target.iter_mut() {
        let key = person_key(&person.name);
        let Some(same_named) = by_name.get(&key) else {
            continue;
        };
        let index = positions.entry(key).or_insert(0);
        let in_source = same_named.get(*index).or_else(|| same_named.first());
        *index += 1;
        let Some(in_source) = in_source else {
            continue;
        };
        if person.provider_id.is_none() {
            person.provider_id = in_source.provider_id;
        }
        if is_blank(person.primary_image_url.as_deref()) {
            person
                .primary_image_url
                .clone_from(&in_source.primary_image_url);
        }
        if !is_blank(in_source.role.as_deref()) && is_blank(person.role.as_deref()) {
            person.role.clone_from(&in_source.role);
        }
        if in_source.sort_order.is_some() && person.sort_order.is_none() {
            person.sort_order = in_source.sort_order;
        }
    }
}

/// The ProviderIds rule of `MergeBaseItemData` (`:1307-1334`): a source id
/// that is valid for its provider replaces the target's on `replaceData`, or
/// when the target has none or an unusable one; then every invalid id left
/// on the target is dropped. Keys compare ordinal-ignore-case, as the
/// upstream dictionary does.
#[must_use]
pub fn merge_provider_ids(
    source: &[(String, String)],
    target: &[(String, String)],
    replace: bool,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = target.to_vec();
    for (key, value) in source {
        if !is_valid_provider_id(key, value) {
            continue;
        }
        match out.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
            Some(existing) => {
                if replace || !is_valid_provider_id(&existing.0, &existing.1) {
                    existing.1.clone_from(value);
                }
            }
            None => out.push((key.clone(), value.clone())),
        }
    }
    out.retain(|(k, v)| is_valid_provider_id(k, v));
    out
}

/// `ProviderIdsExtensions.SetProviderIds` (`ProviderIdsExtensions.cs:
/// 227-241`): the id set `ids` becomes, each pair through `TrySetProviderId`
/// (`:155-189`) — a blank name or value, or a name holding `=` (it could not
/// be read back from the database), is dropped; name and value are trimmed;
/// an id that cannot belong to its provider ([`is_valid_provider_id`]) is
/// dropped; a known provider's name takes its canonical spelling
/// (`MetadataProvider`); a later pair for the same name (ignoring case)
/// replaces an earlier one.
#[must_use]
pub fn set_provider_ids<'a>(
    ids: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (name, value) in ids {
        if name.trim().is_empty() || value.trim().is_empty() || name.contains('=') {
            continue;
        }
        let (name, value) = (name.trim(), value.trim());
        if !is_valid_provider_id(name, value) {
            continue;
        }
        let name = ferrofin_model::entities_media::MetadataProvider::all()
            .iter()
            .map(|p| p.as_name())
            .find(|known| known.eq_ignore_ascii_case(name))
            .unwrap_or(name);
        match out.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
            Some(existing) => value.clone_into(&mut existing.1),
            None => out.push((name.to_owned(), value.to_owned())),
        }
    }
    out
}

/// `ProviderIdsExtensions.IsValidProviderId`: a blank name or value is never
/// valid; a provider with a known id shape must match it (IMDb ids, positive
/// TMDB/AudioDb numbers, MusicBrainz MBIDs); any other provider accepts any
/// value.
#[must_use]
pub fn is_valid_provider_id(name: &str, value: &str) -> bool {
    if name.trim().is_empty() || value.trim().is_empty() {
        return false;
    }
    let is = |known: &str| name.eq_ignore_ascii_case(known);
    if is("Imdb") {
        is_imdb_id(value)
    } else if is("Tmdb") || is("TmdbCollection") || is("AudioDbArtist") || is("AudioDbAlbum") {
        is_positive_number(value)
    } else if is("MusicBrainzAlbum")
        || is("MusicBrainzAlbumArtist")
        || is("MusicBrainzArtist")
        || is("MusicBrainzReleaseGroup")
        || is("MusicBrainzRecording")
        || is("MusicBrainzTrack")
    {
        is_guid(value)
    } else {
        true
    }
}

/// `IsGuid` (`ProviderIdsExtensions.cs:280-281`): whether [`parse_guid`]
/// reads `value`.
fn is_guid(value: &str) -> bool {
    parse_guid(value).is_some()
}

/// .NET's `Guid.TryParse(value, out var guid)` (dotnet/runtime
/// `System.Private.CoreLib/src/System/Guid.cs`, `TryParseGuid`): the GUID
/// `value` names, in any form .NET reads. Surrounding whitespace is trimmed;
/// then the first character picks the form: `(` the `P` form, `{` the `B`
/// form when a dash follows at index 9 and the `X` form otherwise, anything
/// else the `D` form when a dash is at index 8 and the `N` form otherwise.
///
/// `IsGuid` validates the MusicBrainz ids with it, and
/// `MusicBrainzQueryExtensions.ParseMusicBrainzId` turns one into the `Guid`
/// its lookups send (`Guid.ToString()`, the lowercase `D` form). A
/// MusicBrainz id reaches both from an audio tag, an NFO or a Jellyfin-written
/// `BaseItemProviders` row, so every form upstream accepts is read here too —
/// and `uuid`'s own extra (`urn:uuid:`) is not.
#[must_use]
pub fn parse_guid(value: &str) -> Option<uuid::Uuid> {
    let s: Vec<char> = value.trim().chars().collect();
    // `if (guidString.Length < 32)`: the shortest form, `N`.
    if s.len() < 32 {
        return None;
    }
    match s[0] {
        '(' => guid_wrapped(&s, ')'),
        '{' if s[9] == '-' => guid_wrapped(&s, '}'),
        '{' => guid_x(&s),
        _ if s[8] == '-' => guid_d(&s),
        _ => guid_n(&s),
    }
}

/// `TryParseExactN`: 32 hex digits.
fn guid_n(s: &[char]) -> Option<uuid::Uuid> {
    if s.len() != 32 || !s.iter().all(char::is_ascii_hexdigit) {
        return None;
    }
    let hex: String = s.iter().collect();
    u128::from_str_radix(&hex, 16)
        .ok()
        .map(uuid::Uuid::from_u128)
}

/// `TryParseExactB` / `TryParseExactP`: a `D` form inside braces or
/// parentheses, 38 characters in all.
fn guid_wrapped(s: &[char], close: char) -> Option<uuid::Uuid> {
    if s.len() == 38 && s[37] == close {
        guid_d(&s[1..37])
    } else {
        None
    }
}

/// `TryParseExactD`, its compat parsing included: 36 characters, dashes at
/// 8, 13, 18 and 23; the first four groups read by [`guid_hex`], so each may
/// open with `+` and then `0x` inside its fixed width ("a four digit component
/// could be `1234` or `0x34` or `+0x4` or `+234`"), the last two strictly
/// hex.
fn guid_d(text: &[char]) -> Option<uuid::Uuid> {
    if text.len() != 36 || [8, 13, 18, 23].iter().any(|&at| text[at] != '-') {
        return None;
    }
    let first = guid_hex(&text[0..8])?;
    let second = guid_hex(&text[9..13])?;
    let third = guid_hex(&text[14..18])?;
    let fourth = guid_hex(&text[19..23])?;
    if !text[24..].iter().all(char::is_ascii_hexdigit) {
        return None;
    }
    let last: String = text[24..].iter().collect();
    let last = u64::from_str_radix(&last, 16).ok()?.to_be_bytes();
    // A four-digit group holds at most 0xFFFF; `+`/`0x` only shorten it.
    let [_, _, hi, lo] = fourth.to_be_bytes();
    Some(uuid::Uuid::from_fields(
        first,
        u16::try_from(second).ok()?,
        u16::try_from(third).ok()?,
        &[hi, lo, last[2], last[3], last[4], last[5], last[6], last[7]],
    ))
}

/// `TryParseExactX`: `{0xdddddddd,0xdddd,0xdddd,{0xdd,0xdd,0xdd,0xdd,0xdd,
/// 0xdd,0xdd,0xdd}}`, whitespace anywhere in it ignored. Each component
/// follows its own `0x` (either case) and is non-empty; [`guid_hex`] reads
/// it (so it may repeat the `+`/`0x` openers), with at most eight
/// significant digits, and a byte component is at most `0xFF`. The two short
/// components are only checked against 32 bits and keep their low 16 (the
/// runtime's "parsed as 32-bits and only considered to overflow if they'd
/// overflow 32 bits").
fn guid_x(text: &[char]) -> Option<uuid::Uuid> {
    let text: String = text.iter().filter(|ch| !ch.is_whitespace()).collect();
    let inner = text.strip_prefix('{')?.strip_suffix("}}")?;
    let parts: Vec<&str> = inner.split(',').collect();
    let [first, second, third, bytes @ ..] = parts.as_slice() else {
        return None;
    };
    let component = |part: &str| -> Option<u32> {
        let digits = part
            .strip_prefix("0x")
            .or_else(|| part.strip_prefix("0X"))
            .filter(|d| !d.is_empty())?;
        guid_hex(&digits.chars().collect::<Vec<char>>())
    };
    if bytes.len() != 8 {
        return None;
    }
    let mut tail = [0u8; 8];
    for (index, (part, byte)) in bytes.iter().zip(tail.iter_mut()).enumerate() {
        let part = if index == 0 {
            part.strip_prefix('{')?
        } else {
            part
        };
        *byte = u8::try_from(component(part)?).ok()?;
    }
    let low16 = |v: u32| {
        let [_, _, hi, lo] = v.to_be_bytes();
        u16::from_be_bytes([hi, lo])
    };
    Some(uuid::Uuid::from_fields(
        component(first)?,
        low16(component(second)?),
        low16(component(third)?),
        &tail,
    ))
}

/// `Guid.TryParseHex`: an optional `+`, then an optional `0x`/`0X`, then hex
/// digits (none reads as zero). `None` for any other character, or for more
/// than eight significant digits (its `overflow`).
fn guid_hex(s: &[char]) -> Option<u32> {
    let s = s.strip_prefix(&['+']).unwrap_or(s);
    let s = match s {
        ['0', 'x' | 'X', rest @ ..] => rest,
        _ => s,
    };
    let significant = s.iter().skip_while(|&&c| c == '0');
    let mut value: u32 = 0;
    let mut digits = 0;
    for c in significant {
        value = value.wrapping_mul(16).wrapping_add(c.to_digit(16)?);
        digits += 1;
    }
    (digits <= 8).then_some(value)
}

/// `int.TryParse(value, NumberStyles.None, …) && id > 0`: digits only, no
/// sign or whitespace, fitting an `int`.
fn is_positive_number(value: &str) -> bool {
    value.bytes().all(|b| b.is_ascii_digit()) && value.parse::<i32>().is_ok_and(|id| id > 0)
}

/// `^(tt|nm|co|ev|ch|ni)?[0-9]+$`, case-insensitive.
fn is_imdb_id(value: &str) -> bool {
    let digits = match value.get(..2) {
        Some(prefix)
            if ["tt", "nm", "co", "ev", "ch", "ni"]
                .iter()
                .any(|p| prefix.eq_ignore_ascii_case(p)) =>
        {
            &value[2..]
        }
        _ => value,
    };
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests;
