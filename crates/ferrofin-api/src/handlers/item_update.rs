//! `ItemUpdateController` / `ItemRefreshController` — item writes.
//!
//! Ports the portable item-write surface:
//!
//! - `POST /Items/{itemId}` — applies an edited [`BaseItemDto`] onto the stored
//!   item row and persists it.
//! - `POST /Items/{itemId}/ContentType` — sets (or clears) the configured
//!   content-type override for the item's folder in the server configuration.
//! - `POST /Items/{itemId}/Refresh` — queues a metadata/image refresh for the
//!   item at high priority: a folder's refresh is a scan of its subtree, a
//!   file item's a scan of its own path, both with the request's options; an
//!   item with no file of its own refreshes through the provider manager.
//! - `GET /Items/{itemId}/MetadataEditor` — the reference data (parental ratings,
//!   countries, cultures, external-id descriptors, content-type options) a client
//!   needs to render the item's metadata editor.
//!
//! The edit is applied to the addressed row, its `LockedFields` and external
//! ids, then cascaded as the C# `UpdateItem` does: a series' name, rating and
//! tag edits onto its seasons and their episodes, a season's onto its
//! episodes, an album's onto its tracks (each child's own `LockedFields`
//! honoured), and a change of `LockData` onto every descendant of a folder.
//!
//! TODO(parity, open work item): two parts of `UpdateItem` are not ported
//! yet — the `request.People` write (`_libraryManager.UpdatePeople`) and the
//! `FullRefresh`/`ReplaceAllMetadata` refresh queued when a series'
//! `DisplayOrder` changes (`ItemUpdateController.cs:83-86,120-132`).

use crate::extract::Query;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::routing::post;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::{MetadataEditorInfo, NameGuidPair, NameValuePair};
use ferrofin_model::entities::MetadataField;
use ferrofin_traits::library::ScanTarget;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions, RefreshPriority};

use crate::handlers::refresh_scan_path;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::extract::JsonBody;
use crate::state::AppState;

/// `POST /Items/{itemId}` — applies an edited item and persists it.
///
/// Port of `ItemUpdateController.UpdateItem` (scalar/collection subset). A
/// missing item is a `404`; on success the row is saved and the handler returns
/// `204`.
#[utoipa::path(
    post,
    path = "/Items/{itemId}",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 204, description = "Item updated"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
pub(crate) async fn update_item(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(item_id): Path<Uuid>,
    JsonBody(request): JsonBody<Box<UpdateItemRequest>>,
) -> Result<StatusCode, ApiError> {
    let mut item = state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    // `var isLockedChanged = item.IsLocked != (request.LockData ?? false)`.
    let lock_changed = item.is_locked != request.lock_data.unwrap_or(false);
    let current_tags = split_list(item.tags.as_deref());
    // `item.IsLocked = request.LockData ?? false` (`ItemUpdateController.cs:
    // 418`) — exactly, inside `apply_update`. Editing a field never locks the
    // row: the scan merges onto the stored row and honours `LockedFields`,
    // so an unlocked edit survives a rescan that runs no provider.
    apply_update(&mut item, &request);
    state
        .library
        .update_items(std::slice::from_ref(&item), None)
        .await?;
    // External ids live in their own table (`BaseItemProviders`), so they are a
    // second write rather than a column on the row. C# strips empty values and
    // then ASSIGNS the dictionary, which is why this replaces the set instead of
    // merging into it.
    if let Some(ids) = &request.provider_ids {
        let mut pairs: Vec<(String, String)> = ids
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // A HashMap has no order; sort so the write (and its test) is deterministic.
        pairs.sort();
        state
            .library
            .update_item_provider_ids(item_id, &pairs)
            .await?;
    }
    // `if (request.LockedFields is not null) item.LockedFields =
    // request.LockedFields;` — its own table too (`BaseItemMetadataFields`);
    // an absent key leaves the stored set alone.
    if let Some(fields) = &request.locked_fields {
        state
            .library
            .update_item_locked_fields(item_id, fields)
            .await?;
    }
    cascade_to_children(&state, &item, &request, &current_tags).await?;
    // `if (isLockedChanged && item.IsFolder)`: every descendant takes the
    // new lock (`ItemUpdateController.cs:104-113`).
    if lock_changed && item.is_folder {
        let mut descendants = recursive_children(&state, item_id).await?;
        for child in &mut descendants {
            child.is_locked = item.is_locked;
        }
        state.library.update_items(&descendants, None).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The editable subset of a `BaseItemDto` the metadata editor `POST`s.
///
/// jellyfin-web's editor sends the whole item, but only these fields are applied;
/// modelling just them (unknown fields are ignored) keeps the write path focused.
/// Crucially, its number inputs serialize as **strings** (`"ProductionYear": "2010"`)
/// and cleared dates as `""`, which Jellyfin's C# binder coerces but strict serde
/// rejects (a `422`). The numeric/date fields therefore use tolerant deserializers
/// that accept a string, a number, or an empty value.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct UpdateItemRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    forced_sort_name: Option<String>,
    #[serde(default)]
    original_title: Option<String>,
    #[serde(default, deserialize_with = "opt_f32")]
    critic_rating: Option<f32>,
    #[serde(default, deserialize_with = "opt_f32")]
    community_rating: Option<f32>,
    #[serde(default, deserialize_with = "opt_i32")]
    index_number: Option<i32>,
    #[serde(default, deserialize_with = "opt_i32")]
    parent_index_number: Option<i32>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    genres: Option<Vec<String>>,
    #[serde(default)]
    taglines: Option<Vec<String>>,
    #[serde(default)]
    studios: Option<Vec<NameGuidPair>>,
    #[serde(default, deserialize_with = "opt_date")]
    date_created: Option<DateTime<Utc>>,
    #[serde(default)]
    series_name: Option<String>,
    #[serde(default, deserialize_with = "opt_date")]
    end_date: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "opt_date")]
    premiere_date: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "opt_i32")]
    production_year: Option<i32>,
    #[serde(default)]
    official_rating: Option<String>,
    #[serde(default)]
    custom_rating: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    production_locations: Option<Vec<String>>,
    #[serde(default)]
    preferred_metadata_country_code: Option<String>,
    #[serde(default)]
    preferred_metadata_language: Option<String>,
    #[serde(default)]
    lock_data: Option<bool>,
    /// The fields locked against provider updates. An absent key leaves the
    /// stored set alone (`if (request.LockedFields is not null)`).
    #[serde(default, deserialize_with = "opt_metadata_fields")]
    locked_fields: Option<Vec<i32>>,
    #[serde(default)]
    album: Option<String>,
    #[serde(default)]
    artist_items: Option<Vec<NameGuidPair>>,
    #[serde(default)]
    album_artists: Option<Vec<NameGuidPair>>,
    /// The item's external ids. C# assigns the whole dictionary
    /// (`item.ProviderIds = request.ProviderIds`), so a key the client dropped
    /// is REMOVED, not merged — and pairs with an empty value are stripped
    /// first (`ItemUpdateController.UpdateItem`, v10.11.8 lines 402-410).
    ///
    /// An ABSENT key leaves the stored ids alone. The vendored contract types
    /// `BaseItemDto.ProviderIds` as `nullable: true`, so a body without it is a
    /// legal request; upstream's own model binder happens to reject one with a
    /// 400 because its C# dictionary is non-nullable, and copying that quirk
    /// would reject bodies Ferrofin's contract accepts.
    #[serde(default)]
    provider_ids: Option<std::collections::HashMap<String, String>>,
}

/// Deserializes an optional `i32` that may arrive as a number, a numeric string,
/// or an empty string (`""` → `None`).
///
/// The two halves are both ports, not conveniences. Reading a number out of a
/// string is `JsonNumberHandling.AllowReadingFromString`, which
/// `JsonDefaults.Options` sets for the whole API (v10.11.8
/// src/Jellyfin.Extensions/Json/JsonDefaults.cs:33) — jellyfin-web's track
/// pickers really do post `"AudioStreamIndex": "1"`. Reading `""` as a cleared
/// field is `JsonNullableStructConverter<TStruct>.Read`, which returns `null`
/// for an empty string before deserializing (JsonNullableStructConverter.cs:18).
///
/// Everything else is an ERROR, exactly as `Deserialize<int>` throws and the
/// model binder answers `400`. Swallowing a string that is not a number into
/// `None` was a real divergence: `{"MaxStreamingBitrate":"nope"}` on
/// `POST /Items/{itemId}/PlaybackInfo` measured Ferrofin `200` against
/// Jellyfin `400`.
///
/// # Errors
///
/// Fails when the value is a non-empty string that is not an integer, or a
/// number that is not a 32-bit integer (`Deserialize<int>` rejects `3.0` too).
pub(crate) fn opt_i32<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i32>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Lenient {
        Number(i32),
        Text(String),
        Null,
    }
    match Lenient::deserialize(d)? {
        Lenient::Number(n) => Ok(Some(n)),
        Lenient::Text(s) if s.trim().is_empty() => Ok(None),
        Lenient::Text(s) => s
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| serde::de::Error::custom(format!("expected an integer, got {s:?}"))),
        Lenient::Null => Ok(None),
    }
}

/// Deserializes an optional `f32` that may arrive as a number or a numeric string.
///
/// The `f32` twin of [`opt_i32`], with the same two ports behind it and the
/// same refusal to turn an unparseable string into a silent `None`.
///
/// # Errors
///
/// Fails when the value is a non-empty string that is not a number.
fn opt_f32<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f32>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Lenient {
        Number(f32),
        Text(String),
        Null,
    }
    match Lenient::deserialize(d)? {
        Lenient::Number(n) => Ok(Some(n)),
        Lenient::Text(s) if s.trim().is_empty() => Ok(None),
        Lenient::Text(s) => s
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| serde::de::Error::custom(format!("expected a number, got {s:?}"))),
        Lenient::Null => Ok(None),
    }
}

/// Deserializes `LockedFields` the way `JsonStringEnumConverter` reads an
/// enum, into the stored `MetadataField` values (`MetadataField.cs`: `Cast`
/// = 0 … `OfficialRating` = 8): a member name (case-insensitively), an
/// integer, or an integer in a string. Like a C# enum, an integer names a
/// value even when no member has it; it is stored as sent and skipped on
/// read.
///
/// # Errors
///
/// Fails on a string that is neither a member name nor an integer.
fn opt_metadata_fields<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Vec<i32>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Lenient {
        Number(i32),
        Name(String),
    }
    let Some(values) = Option::<Vec<Lenient>>::deserialize(d)? else {
        return Ok(None);
    };
    values
        .into_iter()
        .map(|value| match value {
            Lenient::Number(n) => Ok(n),
            Lenient::Name(name) => {
                let name = name.trim();
                if let Ok(n) = name.parse::<i32>() {
                    return Ok(n);
                }
                MetadataField::ALL
                    .iter()
                    .find(|field| format!("{field:?}").eq_ignore_ascii_case(name))
                    .map(|field| ferrofin_db::enums::metadata_field::to_i32(*field))
                    .ok_or_else(|| {
                        serde::de::Error::custom(format!("not a MetadataField: {name:?}"))
                    })
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// Deserializes an optional timestamp the way Jellyfin reads one: a cleared
/// field (`null` or `""`) is `None`, and a bare date — what jellyfin-web's
/// metadata editor sends for a date the user changed — is midnight UTC.
fn opt_date<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
    ferrofin_model::json::datetime::option::deserialize(d)
}

/// Applies the editable fields of `request` onto `item`. Mirrors the scalar and
/// collection assignments of C# `ItemUpdateController.UpdateItem`; the
/// series/season/album child cascades are [`cascade_to_children`].
fn apply_update(item: &mut BaseItemEntity, request: &UpdateItemRequest) {
    item.name.clone_from(&request.name);
    item.forced_sort_name.clone_from(&request.forced_sort_name);
    item.original_title = non_empty(request.original_title.as_deref());
    item.critic_rating = request.critic_rating.map(f64::from);
    item.community_rating = request.community_rating.map(f64::from);
    item.index_number = request.index_number.map(i64::from);
    item.parent_index_number = request.parent_index_number.map(i64::from);
    item.overview.clone_from(&request.overview);

    if let Some(genres) = &request.genres {
        item.genres = Some(join_distinct(genres));
    }
    if let Some(taglines) = &request.taglines {
        item.tagline = taglines.first().cloned();
    }
    if let Some(studios) = &request.studios {
        let names: Vec<String> = studios
            .iter()
            .filter_map(|s| s.name.clone())
            .collect::<Vec<_>>();
        item.studios = Some(join_distinct(&names));
    }
    if let Some(created) = request.date_created {
        item.date_created = Some(created);
    }
    if let Some(series_name) = &request.series_name {
        item.series_name = Some(series_name.clone());
    }

    item.end_date = request.end_date;
    item.premiere_date = request.premiere_date;
    item.production_year = request.production_year.map(i64::from);
    item.official_rating = non_empty(request.official_rating.as_deref());
    item.custom_rating.clone_from(&request.custom_rating);

    if let Some(tags) = &request.tags {
        item.tags = Some(join_distinct(tags));
    }
    if let Some(locations) = &request.production_locations {
        item.production_locations = Some(join_distinct(locations));
    }

    item.preferred_metadata_country_code
        .clone_from(&request.preferred_metadata_country_code);
    item.preferred_metadata_language
        .clone_from(&request.preferred_metadata_language);
    item.is_locked = request.lock_data.unwrap_or(false);

    if let Some(album) = &request.album {
        item.album = Some(album.clone());
    }
    if let Some(artist_items) = &request.artist_items {
        let names: Vec<String> = artist_items
            .iter()
            .filter_map(|a| a.name.clone())
            .collect::<Vec<_>>();
        item.artists = Some(join_distinct(&names));
    }
    if let Some(album_artists) = &request.album_artists {
        let names: Vec<String> = album_artists
            .iter()
            .filter_map(|a| a.name.clone())
            .collect::<Vec<_>>();
        item.album_artists = Some(join_distinct(&names));
    }
}

/// `Folder.GetRecursiveChildren()` (`Folder.cs:1627-1682`): every physical
/// descendant, plus the folder's own linked children (a box set's or
/// playlist's members) — `AddChildrenToList(includeLinkedChildren: true)`
/// includes those for the first folder only, and does not descend into them.
async fn recursive_children(
    state: &AppState,
    folder: Uuid,
) -> Result<Vec<BaseItemEntity>, ApiError> {
    let mut all = state
        .library
        .get_item_list(&InternalItemsQuery {
            ancestor_ids: vec![folder],
            recursive: true,
            ..InternalItemsQuery::default()
        })
        .await?;
    // A non-recursive, non-physical `parent_id` browse merges the folder's
    // `LinkedChildren` into its direct children.
    let direct = state
        .library
        .get_item_list(&InternalItemsQuery {
            parent_id: folder,
            ..InternalItemsQuery::default()
        })
        .await?;
    for child in direct {
        if !all
            .iter()
            .any(|known| known.id.eq_ignore_ascii_case(&child.id))
        {
            all.push(child);
        }
    }
    Ok(all)
}

/// The values of a `|`-joined list column.
fn split_list(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split('|')
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// `a.Except(b)` — ordinal, and distinct like every LINQ set operator.
fn except(a: &[String], b: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in a {
        if !b.contains(value) && !out.contains(value) {
            out.push(value.clone());
        }
    }
    out
}

/// The tag edit the cascade applies to each child: which tags the request
/// added to the item and which it removed. No `Tags` in the request changes
/// nothing (`removedTags = []; addedTags = []`).
struct TagEdit {
    /// `newTags.Except(currentTags)`.
    added: Vec<String>,
    /// `currentTags.Except(newTags)`.
    removed: Vec<String>,
}

impl TagEdit {
    fn of(current: &[String], request: &UpdateItemRequest) -> Self {
        let Some(tags) = &request.tags else {
            return Self {
                added: Vec::new(),
                removed: Vec::new(),
            };
        };
        // `request.Tags.Select(t => t.Trim()).Distinct(OrdinalIgnoreCase)`,
        // as the item's own column stores it.
        let new = split_list(Some(&join_distinct(tags)));
        Self {
            added: except(&new, current),
            removed: except(current, &new),
        }
    }

    /// `child.Tags.Concat(addedTags).Except(removedTags)
    /// .Distinct(StringComparer.OrdinalIgnoreCase)`.
    fn apply(&self, child: &mut BaseItemEntity) {
        let mut tags = split_list(child.tags.as_deref());
        tags.extend(self.added.iter().cloned());
        let kept = except(&tags, &self.removed);
        let joined = join_distinct(&kept);
        child.tags = (!joined.is_empty() || child.tags.is_some()).then_some(joined);
    }
}

/// The children an edit cascades onto: `Children.OfType<kind>()` (a direct,
/// physical child of `parent`), or every direct child when `kind` is `None`.
async fn children_of(
    state: &AppState,
    parent: Uuid,
    kind: Option<BaseItemKind>,
) -> Result<Vec<BaseItemEntity>, ApiError> {
    Ok(state
        .library
        .get_item_list(&InternalItemsQuery {
            parent_id: parent,
            physical_children_only: true,
            include_item_types: kind.into_iter().collect(),
            ..InternalItemsQuery::default()
        })
        .await?)
}

/// The rating and tag half of the cascade on one child, skipping a field in
/// the child's own `LockedFields` (`ItemUpdateController.cs:321-381`):
/// `OfficialRating` unless locked, `CustomRating` always, the tag edit
/// unless `Tags` is locked.
fn cascade_onto(
    child: &mut BaseItemEntity,
    locked: &[MetadataField],
    official_rating: Option<&String>,
    custom_rating: Option<&String>,
    tags: &TagEdit,
) {
    if !locked.contains(&MetadataField::OfficialRating) {
        child.official_rating = official_rating.cloned();
    }
    child.custom_rating = custom_rating.cloned();
    if !locked.contains(&MetadataField::Tags) {
        tags.apply(child);
    }
}

/// Port of the child walks in `ItemUpdateController.UpdateItem`
/// (`ItemUpdateController.cs:315-381`): a `Series` passes its name, rating
/// and tag edits to its seasons and their episodes, a `Season` its rating
/// and tags to its episodes, a `MusicAlbum` its rating and tags to its
/// children. Each child's `LockedFields` shields its `OfficialRating` and
/// `Tags`; `SeriesName` and `CustomRating` are always written.
async fn cascade_to_children(
    state: &AppState,
    item: &BaseItemEntity,
    request: &UpdateItemRequest,
    current_tags: &[String],
) -> Result<(), ApiError> {
    let kind = BaseItemKind::from_stored_type_name(&item.type_);
    if !matches!(
        kind,
        Some(BaseItemKind::Series | BaseItemKind::Season | BaseItemKind::MusicAlbum)
    ) {
        return Ok(());
    }
    let Ok(item_id) = Uuid::parse_str(&item.id) else {
        return Ok(());
    };
    let tags = TagEdit::of(current_tags, request);
    // `request.OfficialRating = string.IsNullOrWhiteSpace(...) ? null : ...`
    // — the same value the item itself took.
    let official_rating = item.official_rating.as_ref();
    let custom_rating = request.custom_rating.as_ref();
    // The direct children the walk visits, then (for a series) the seasons'
    // episodes.
    let mut children = match kind {
        Some(BaseItemKind::Series) => {
            children_of(state, item_id, Some(BaseItemKind::Season)).await?
        }
        Some(BaseItemKind::Season) => {
            children_of(state, item_id, Some(BaseItemKind::Episode)).await?
        }
        _ => children_of(state, item_id, None).await?,
    };
    if kind == Some(BaseItemKind::Series) {
        let mut episodes = Vec::new();
        for season in &children {
            if let Ok(season_id) = Uuid::parse_str(&season.id) {
                episodes.extend(children_of(state, season_id, Some(BaseItemKind::Episode)).await?);
            }
        }
        children.extend(episodes);
    }
    if children.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = children
        .iter()
        .filter_map(|c| Uuid::parse_str(&c.id).ok())
        .collect();
    let locks = state.library.get_locked_fields_batch(&ids).await?;
    for child in &mut children {
        if kind == Some(BaseItemKind::Series) {
            // `season.SeriesName = rseries.Name` / `ep.SeriesName = ...`.
            child.series_name.clone_from(&item.name);
        }
        let locked = Uuid::parse_str(&child.id)
            .ok()
            .and_then(|id| locks.get(&id))
            .map_or(&[][..], Vec::as_slice);
        cascade_onto(child, locked, official_rating, custom_rating, &tags);
    }
    state.library.update_items(&children, None).await?;
    Ok(())
}

/// Returns the trimmed value, or [`None`] when it is blank — mirrors the C#
/// `string.IsNullOrWhiteSpace(x) ? null : x` guards.
fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// Joins values with `|` after a case-insensitive de-duplication, matching how
/// `ferrofin-db` stores the `Genres`/`Studios`/`Artists`/`Tags` columns and C#'s
/// `Distinct(StringComparer.OrdinalIgnoreCase)`.
fn join_distinct(values: &[String]) -> String {
    let mut seen: Vec<String> = Vec::with_capacity(values.len());
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if !seen.iter().any(|s| s.eq_ignore_ascii_case(value)) {
            seen.push(value.to_owned());
        }
    }
    seen.join("|")
}

/// Query parameters for `POST /Items/{itemId}/ContentType`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentTypeQuery {
    /// The content type to set; absent/blank clears the override.
    #[serde(default)]
    content_type: Option<String>,
}

/// `POST /Items/{itemId}/ContentType` — sets the folder content-type override.
///
/// Port of `ItemUpdateController.UpdateItemContentType`. A missing item is a
/// `404`; on success the server configuration's `ContentTypes` list is rewritten
/// (dropping any prior entry for this folder, adding the new one when non-blank)
/// and persisted, returning `204`.
#[utoipa::path(
    post,
    path = "/Items/{itemId}/ContentType",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 204, description = "Content type updated"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn update_item_content_type(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(item_id): Path<Uuid>,
    Query(query): Query<ContentTypeQuery>,
) -> Result<StatusCode, ApiError> {
    let item = state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    let folder = containing_folder_path(item.path.as_deref());

    let mut configuration = (*state.config.configuration().await?).clone();
    configuration.content_types.retain(|pair| {
        pair.name
            .as_deref()
            .is_some_and(|name| !name.is_empty() && !name.eq_ignore_ascii_case(&folder))
    });
    if let Some(content_type) = query
        .content_type
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        configuration.content_types.push(NameValuePair {
            name: Some(folder),
            value: Some(content_type.to_owned()),
        });
    }
    state.config.update_configuration(&configuration).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The containing-folder path of a file path (its parent directory), or an empty
/// string when unknown. Mirrors C# `BaseItem.ContainingFolderPath`.
fn containing_folder_path(path: Option<&str>) -> String {
    let Some(path) = path else {
        return String::new();
    };
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rfind(['/', '\\']) {
        Some(idx) => trimmed[..idx].to_owned(),
        None => String::new(),
    }
}

/// Query parameters for `POST /Items/{itemId}/Refresh`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefreshQuery {
    /// The metadata refresh mode (defaults to `None`).
    #[serde(default)]
    metadata_refresh_mode: Option<RefreshMode>,
    /// The image refresh mode (defaults to `None`).
    #[serde(default)]
    image_refresh_mode: Option<RefreshMode>,
    /// Whether to replace all metadata (only for a full refresh).
    #[serde(default)]
    replace_all_metadata: Option<bool>,
    /// Whether to replace all images (only for a full refresh).
    #[serde(default)]
    replace_all_images: Option<bool>,
    /// Whether to regenerate trickplay images (only for a full refresh).
    #[serde(default)]
    regenerate_trickplay: Option<bool>,
}

/// How `POST /Items/{itemId}/Refresh` refreshes an item — upstream's
/// `ProviderManager.RefreshItem` and `RefreshArtist`
/// (`ProviderManager.cs:1211-1290`).
struct RefreshRoute {
    /// The folders validated, or the file refreshed, with the request's
    /// options, as a scan.
    scan: Option<ScanTarget>,
    /// The item's own refresh through the provider manager (an item with no
    /// file of its own).
    item: bool,
}

impl RefreshRoute {
    /// A scan of `target`, which refreshes the item too.
    fn scan(target: ScanTarget) -> Self {
        Self {
            scan: Some(target),
            item: false,
        }
    }

    /// The item's own refresh only.
    fn item_only() -> Self {
        Self {
            scan: None,
            item: true,
        }
    }
}

/// Routes an item's refresh. A music artist refreshes as
/// [`artist_refresh_route`] says; everything else as
/// [`refresh_scan_target`] says, and the item refreshes itself through the
/// provider manager when no scan covers it.
async fn refresh_route(
    state: &AppState,
    item: &BaseItemEntity,
    item_id: Uuid,
) -> Result<RefreshRoute, ApiError> {
    if BaseItemKind::from_stored_type_name(&item.type_) == Some(BaseItemKind::MusicArtist) {
        return artist_refresh_route(state, item, item_id).await;
    }
    Ok(match refresh_scan_target(item, item_id) {
        Some(target) => RefreshRoute::scan(target),
        None => RefreshRoute::item_only(),
    })
}

/// The scan an item's refresh is (`ProviderManager.RefreshItem`), if any. A
/// CollectionFolder refreshes its physical folders
/// (`RefreshCollectionFolderChildren`), the root every library, and any
/// other folder validates its own children — its subtree, not its whole
/// library — which the scan refreshes along with the folder itself.
///
/// A file item (a movie, an episode, a track, a book, a photo, a video…)
/// refreshes itself (`RefreshSingleItem`) through the same metadata service
/// the scan runs: here the scan of its one path, so it gets the scan's
/// refresh decision, merge, field locks, probe, NFO and every metadata
/// provider, and nothing else of its library is walked.
///
/// No scan covers an item upstream validates no children for and that has
/// no file of its own: a box set's members and a playlist's entries are
/// linked, not physical (`BoxSet.GetNonCachedChildren` returns none,
/// `Playlist.ValidateChildrenInternal` is a no-op), a view's folder holds
/// none, a folder with no path of its own — a virtual season, whose episodes
/// sit in the series folder — is not `IsFileProtocol`, so
/// `ValidateChildrenInternal2` validates and refreshes no child
/// (`Folder.cs:430-436,780-812`), and a person, genre, studio or channel
/// item has no file. Those refresh through the provider manager.
pub(crate) fn refresh_scan_target(item: &BaseItemEntity, item_id: Uuid) -> Option<ScanTarget> {
    if !item.is_folder {
        return refresh_scan_path(item).map(|path| ScanTarget::Items(vec![path]));
    }
    match BaseItemKind::from_stored_type_name(&item.type_) {
        Some(BaseItemKind::CollectionFolder) => Some(ScanTarget::Library(item_id)),
        Some(BaseItemKind::AggregateFolder | BaseItemKind::UserRootFolder) => Some(ScanTarget::All),
        Some(
            BaseItemKind::BoxSet
            | BaseItemKind::Playlist
            | BaseItemKind::ManualPlaylistsFolder
            | BaseItemKind::PlaylistsFolder
            | BaseItemKind::UserView,
        ) => None,
        _ => item
            .path
            .clone()
            .filter(|p| !p.is_empty())
            .map(|path| ScanTarget::Paths(vec![path])),
    }
}

/// `ProviderManager.RefreshArtist` (`ProviderManager.cs:1258-1290`): the
/// albums credited to the artist (`ArtistIds`) each name their
/// `MusicArtist` — the nearest artist folder above them
/// (`MusicAlbum.GetMusicArtist`) — and those artist folders validate their
/// CHILDREN with the request's options (`ValidateChildren` refreshes a
/// folder's children, never the folder itself, `Folder.cs:831-870`); then
/// the artist refreshes itself (`item.RefreshMetadata`).
///
/// A folder-backed artist refreshes itself in the same scan
/// ([`ScanTarget::Artist`]'s `path`); its own folder is validated only when
/// one of its credited albums sits under it. An artist known only by name
/// has no folder (`MusicArtist.ValidateChildrenInternal` returns on
/// `IsAccessedByName`): the scanner refreshes it after its albums' folders
/// (a `path` of `None`), its music providers under the request's options —
/// through the scan queue's priority lane when there is no folder to
/// validate, so it runs inside a running scan and never beside one. An album
/// with no artist folder above it names the by-name artist, whose validation
/// is that no-op, so it is not scanned — as upstream.
async fn artist_refresh_route(
    state: &AppState,
    item: &BaseItemEntity,
    item_id: Uuid,
) -> Result<RefreshRoute, ApiError> {
    let own_folder = item
        .path
        .clone()
        .filter(|p| !p.is_empty())
        .filter(|_| item.top_parent_id.as_deref().is_some_and(|t| !t.is_empty()));
    let albums = state
        .library
        .get_item_list(&InternalItemsQuery {
            include_item_types: vec![BaseItemKind::MusicAlbum],
            artist_ids: vec![item_id],
            ..InternalItemsQuery::default()
        })
        .await?;
    let mut folders: Vec<String> = Vec::new();
    for album in &albums {
        if let Some(path) = artist_folder_above(state, album).await?
            && !folders.contains(&path)
        {
            folders.push(path);
        }
    }
    Ok(RefreshRoute::scan(ScanTarget::Artist {
        id: item_id,
        path: own_folder,
        folders,
    }))
}

/// The path of the nearest folder-backed `MusicArtist` above `album` (the
/// parent chain `MusicAlbum.GetMusicArtist` walks), if any.
async fn artist_folder_above(
    state: &AppState,
    album: &BaseItemEntity,
) -> Result<Option<String>, ApiError> {
    let mut seen: Vec<String> = vec![album.id.clone()];
    let mut next = album.parent_id.clone();
    while let Some(parent_id) = next.take() {
        let Ok(parent_uuid) = Uuid::parse_str(&parent_id) else {
            break;
        };
        if seen.contains(&parent_id) {
            break;
        }
        seen.push(parent_id);
        let Some(parent) = state.library.get_item_by_id(parent_uuid).await? else {
            break;
        };
        match BaseItemKind::from_stored_type_name(&parent.type_) {
            Some(BaseItemKind::MusicArtist) => {
                return Ok(parent.path.filter(|p| !p.is_empty()));
            }
            Some(
                BaseItemKind::CollectionFolder
                | BaseItemKind::AggregateFolder
                | BaseItemKind::UserRootFolder,
            ) => break,
            _ => next = parent.parent_id,
        }
    }
    Ok(None)
}

/// The wire spelling of the refresh-mode query enum. Mirrors the vendored
/// contract's `MetadataRefreshMode` (PascalCase), mapped onto the service-layer
/// [`MetadataRefreshMode`].
#[derive(Debug, Clone, Copy, serde::Deserialize)]
enum RefreshMode {
    /// Do not refresh.
    None,
    /// Validate only what is present.
    ValidationOnly,
    /// Fetch missing metadata only.
    Default,
    /// Fetch all metadata.
    FullRefresh,
}

impl From<RefreshMode> for MetadataRefreshMode {
    fn from(mode: RefreshMode) -> Self {
        match mode {
            RefreshMode::None => Self::None,
            RefreshMode::ValidationOnly => Self::ValidationOnly,
            RefreshMode::Default => Self::Default,
            RefreshMode::FullRefresh => Self::FullRefresh,
        }
    }
}

/// `POST /Items/{itemId}/Refresh` — queues a metadata/image refresh.
///
/// Port of `ItemRefreshController.RefreshItem`. A missing item is a `404`; on
/// success the refresh is queued at high priority and the handler returns `204`.
#[utoipa::path(
    post,
    path = "/Items/{itemId}/Refresh",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 204, description = "Refresh queued"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn refresh_item(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(item_id): Path<Uuid>,
    Query(query): Query<RefreshQuery>,
) -> Result<StatusCode, ApiError> {
    let item = state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;

    let metadata_refresh_mode = query
        .metadata_refresh_mode
        .map_or(MetadataRefreshMode::None, MetadataRefreshMode::from);
    let image_refresh_mode = query
        .image_refresh_mode
        .map_or(MetadataRefreshMode::None, MetadataRefreshMode::from);

    // Every refresh runs with the request's options: both modes (`None`
    // when omitted, `ItemRefreshController.cs:64-65`), the replace flags,
    // `ForceSave` and `RemoveOldMetadata` as the controller sets them
    // (`:76-89`), and `RegenerateTrickplay` (which the scan carries but does
    // not act on yet: see the `TrickplayProvider` work item in the scanner).
    let options = MetadataRefreshOptions::for_item_refresh(
        metadata_refresh_mode,
        image_refresh_mode,
        query.replace_all_metadata.unwrap_or(false),
        query.replace_all_images.unwrap_or(false),
        query.regenerate_trickplay.unwrap_or(false),
    );
    // Refreshing a folder (a library's CollectionFolder, a series, a season,
    // an album…) means "validate its children" — `folder.ValidateChildren(…,
    // options)` — so it drives the filesystem scan over that folder's subtree,
    // and every item in it refreshes with those options. A file item's
    // refresh is the scan of its own path: the scan's refresh decision and
    // merge, with the probe (upstream probes inside `RefreshMetadata`, so a
    // "Search for missing metadata" re-probes the file and a Default refresh
    // of an unchanged one does not), the NFO and every metadata provider.
    let route = refresh_route(&state, &item, item_id).await?;
    if let Some(target) = route.scan {
        state.library.queue_refresh_scan(target, &options).await?;
    }
    if !route.item {
        return Ok(StatusCode::NO_CONTENT);
    }

    // An item with no file of its own — a box set, a playlist, a view, a
    // virtual season, a person, a by-name artist — refreshes through the
    // provider queue: the enqueue spawns the refresh and this request 204s
    // immediately, like the C# queued refresh.
    state
        .providers
        .queue_refresh(item_id, &options, RefreshPriority::High)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Registers the item-refresh + content-type routes.
///
/// The bare `POST /Items/{itemId}` route is registered by
/// [`crate::handlers::items::register`] so it shares one `MethodRouter` with the
/// `GET`/`DELETE` handlers (axum rejects a duplicate method+path across two
/// `route` calls).
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/Items/{itemId}/ContentType",
            post(update_item_content_type),
        )
        .route("/Items/{itemId}/Refresh", post(refresh_item))
        .route("/Items/{itemId}/MetadataEditor", get(get_metadata_editor))
}

/// `GET /Items/{itemId}/MetadataEditor` — the item's metadata-editor descriptor.
///
/// Port of `ItemUpdateController.GetMetadataEditorInfo`: resolves the item (`404`
/// when absent), then assembles the reference data — parental ratings, countries,
/// cultures (deduped by display name, name-ordered case-insensitively via the
/// shared [`crate::handlers::localization::distinct_ordered_cultures`]), the item's external-id
/// descriptors, and the per-item content-type options.
///
/// Port note — content type: C# refines `ContentType`/`ContentTypeOptions` from
/// the folder's inherited vs configured collection type
/// (`GetInheritedContentType`/`GetConfiguredContentType`), which need the
/// un-ported collection-folder tree. The portable seam always offers the full
/// per-item option set with an unset `ContentType`; the descriptor shape and the
/// reference lists are already the final ones.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/MetadataEditor",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Metadata editor returned", body = MetadataEditorInfo),
        (status = 404, description = "Item not found"),
    ),
    tag = "ferrofin"
)]
async fn get_metadata_editor(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(item_id): Path<Uuid>,
) -> Result<Json<MetadataEditorInfo>, ApiError> {
    if state.library.get_item_by_id(item_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("item {item_id}")));
    }

    let external_id_infos = state.providers.get_external_id_infos(item_id).await?;

    // Dedupe cultures by display name (case-insensitively) and order by it, as in
    // C#'s `DistinctBy(...).OrderBy(c => c.DisplayName)`. Shared with
    // `GET /Localization/Cultures` so both lists come back in the same order.
    let cultures =
        crate::handlers::localization::distinct_ordered_cultures(state.localization.get_cultures());

    let info = MetadataEditorInfo {
        parental_rating_options: state.localization.get_parental_ratings(),
        countries: state.localization.get_countries(),
        cultures,
        external_id_infos,
        content_type: None,
        // Jellyfin's GetMetadataEditorInfo only populates ContentTypeOptions for a collection-folder
        // whose content type is configurable; a plain library item (e.g. a Movie) gets an empty list.
        // Ferrofin doesn't model the configurable-content-type folder tree here, so a plain item — the
        // common case this endpoint serves — matches Jellyfin's empty array.
        content_type_options: Vec::new(),
    };
    Ok(Json(info))
}

#[cfg(test)]
mod tests {
    use super::{UpdateItemRequest, containing_folder_path, join_distinct, non_empty};

    /// A string the number cannot be read from is a REFUSAL, not a silent
    /// `None`.
    ///
    /// `JsonNumberHandling.AllowReadingFromString` reads `"2010"`; it does not
    /// make `"nope"` null — `Deserialize<int>` throws and the model binder
    /// answers 400. Measured on the parity pair before this fix:
    /// `POST /Items/{itemId}/PlaybackInfo` with
    /// `{"MaxStreamingBitrate":"nope"}` was Ferrofin 200 / Jellyfin 400,
    /// because `opt_i32` swallowed the parse failure.
    #[test]
    fn a_number_that_is_not_a_number_is_refused_not_swallowed() {
        let one = |body: &str| {
            serde_json::from_str::<UpdateItemRequest>(&format!(
                r#"{{"Id":"x","Type":"Movie","Name":"n",{body}}}"#
            ))
        };
        // The two ported leniencies still hold.
        assert_eq!(
            one(r#""ProductionYear":"2010""#)
                .expect("a numeric string reads")
                .production_year,
            Some(2010)
        );
        assert_eq!(
            one(r#""ProductionYear":"""#)
                .expect("an empty string is a cleared field")
                .production_year,
            None
        );
        assert_eq!(
            one(r#""ProductionYear":null"#)
                .expect("null is a cleared field")
                .production_year,
            None
        );
        assert_eq!(
            one(r#""CommunityRating":"8.5""#)
                .expect("a numeric string reads")
                .community_rating,
            Some(8.5)
        );
        // Everything else is an error, which the shared body binder turns into
        // the 400 ValidationProblemDetails Jellyfin answers.
        for body in [
            r#""ProductionYear":"nope""#,
            r#""ProductionYear":"20 10""#,
            // `Deserialize<int>` rejects a fractional number too.
            r#""ProductionYear":2010.5"#,
            r#""IndexNumber":"one""#,
            r#""CommunityRating":"great""#,
        ] {
            assert!(
                one(body).is_err(),
                "{body} must not bind: .NET throws and the binder answers 400"
            );
        }
    }

    #[test]
    fn update_request_accepts_editor_string_numbers_and_empty_dates() {
        // The metadata editor sends number inputs as strings and cleared dates as
        // "" — strict serde would 422; these must parse (was the save bug).
        let json = r#"{
            "Id": "ignored", "Type": "Movie", "Name": "Inception",
            "ProductionYear": "2010", "CommunityRating": "8.5", "IndexNumber": "1",
            "PremiereDate": "2010-07-16T00:00:00.0000000Z", "EndDate": "",
            "Genres": ["Action"], "Studios": [{"Name": "WB"}], "LockData": false
        }"#;
        let req: UpdateItemRequest = serde_json::from_str(json).expect("lenient parse");
        assert_eq!(req.production_year, Some(2010));
        assert_eq!(req.community_rating, Some(8.5));
        assert_eq!(req.index_number, Some(1));
        assert!(req.premiere_date.is_some());
        assert!(
            req.end_date.is_none(),
            "empty date string → None, not an error"
        );
        assert_eq!(req.name.as_deref(), Some("Inception"));
    }

    /// `JsonStringEnumConverter` reads an enum from its name (any case) or
    /// its integer value; anything else is refused.
    #[test]
    fn locked_fields_accept_names_and_integer_values() {
        let req: UpdateItemRequest =
            serde_json::from_str(r#"{"LockedFields": ["Overview", 8, "cast", "7", 0, 42]}"#)
                .expect("names and numbers");
        // Overview 6, OfficialRating 8, Cast 0, Runtime 7; 42 is kept as sent.
        assert_eq!(req.locked_fields, Some(vec![6, 8, 0, 7, 0, 42]));
        let req: UpdateItemRequest =
            serde_json::from_str(r#"{"LockedFields": null}"#).expect("null");
        assert_eq!(req.locked_fields, None);
        assert!(
            serde_json::from_str::<UpdateItemRequest>(r#"{"LockedFields": ["Plot"]}"#).is_err()
        );
    }

    #[test]
    fn update_request_accepts_native_number_types_too() {
        let req: UpdateItemRequest =
            serde_json::from_str(r#"{"ProductionYear": 1999, "CommunityRating": 7}"#)
                .expect("numbers parse");
        assert_eq!(req.production_year, Some(1999));
        assert_eq!(req.community_rating, Some(7.0));
    }

    #[test]
    fn join_distinct_dedups_case_insensitively() {
        let values = vec![
            "Action".to_owned(),
            "action".to_owned(),
            " Sci-Fi ".to_owned(),
            String::new(),
        ];
        assert_eq!(join_distinct(&values), "Action|Sci-Fi");
    }

    #[test]
    fn non_empty_trims_and_blanks_to_none() {
        assert_eq!(non_empty(Some("  x ")), Some("x".to_owned()));
        assert_eq!(non_empty(Some("   ")), None);
        assert_eq!(non_empty(None), None);
    }

    #[test]
    fn containing_folder_is_parent_dir() {
        assert_eq!(
            containing_folder_path(Some("/media/movies/Blade/Blade.mkv")),
            "/media/movies/Blade"
        );
        assert_eq!(containing_folder_path(Some("Blade.mkv")), "");
        assert_eq!(containing_folder_path(None), "");
    }
}
