//! Guide rows → synthetic [`BaseItemEntity`] rows for the DTO service.
//!
//! Jellyfin stores channels and programmes as `BaseItems` and projects them
//! through `DtoService`; Ferrofin keeps its guide cache in its own tables and
//! instead builds the equivalent entity row in memory here — the projection
//! then runs through the very same `DtoService::get_base_item_dtos` path every
//! other item takes, which is what makes the channel/programme DTOs carry
//! `UserData`, the image maps, `SortName`, `CanDelete` and the rest for free.
//!
//! The field values are ports of `Jellyfin.LiveTv.Guide.GuideManager.GetChannel`
//! / `GetProgram` (what the C# scan writes onto the items) and
//! `MediaBrowser.Controller.LiveTv.LiveTvChannel.CreateSortName`.

use chrono::{DateTime, Utc};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::querying::ItemFields;
use ferrofin_traits::options::DtoOptions;
use uuid::Uuid;

/// The stored `BaseItems.Type` name of a Live TV channel.
pub const CHANNEL_TYPE_NAME: &str = "MediaBrowser.Controller.LiveTv.LiveTvChannel";
/// The stored `BaseItems.Type` name of a Live TV programme.
pub const PROGRAM_TYPE_NAME: &str = "MediaBrowser.Controller.LiveTv.LiveTvProgram";

/// The stored `BaseItems.Type` name of a DVR recording.
///
/// Jellyfin's recordings are ordinary library `Video` items scanned out of the
/// recordings folder (`LiveTvManager.GetEmbyRecordingsAsync` queries
/// `BaseItemKind.Video`), so a recording DTO's `Type` is `"Video"` — not
/// `"Recording"`, which is a timer-side concept.
pub const RECORDING_TYPE_NAME: &str = "MediaBrowser.Controller.Entities.Video";

/// One `FerrofinLiveTvChannels` row, as the query paths read it.
#[derive(Debug, Clone, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct ChannelRow {
    /// The channel's `Guid`, hyphenated.
    pub id: String,
    /// The tuner `tvg-id` (empty when the M3U carried none).
    pub tvg_id: String,
    /// The TUNER's own id for this entry (`hdhr_10.1`, `m3u_{md5}{md5}`) —
    /// C# `ChannelInfo.Id`, which `GuideManager.GetChannel` stores as the
    /// item's `ExternalId` and which the internal GUID is derived from.
    #[sqlx(default)]
    pub external_id: String,
    /// The display name.
    pub name: String,
    /// The channel number, if any.
    pub number: Option<String>,
    /// `"Tv"` or `"Radio"`.
    pub channel_type: String,
    /// When the channel first appeared in the lineup.
    pub date_created: Option<String>,
    /// Whether the guide airs a movie on this channel (upstream
    /// `LiveTvChannel.IsMovie`, aggregated from the programmes at refresh).
    #[sqlx(default)]
    pub is_movie: bool,
    /// Whether the guide airs a series episode on this channel.
    #[sqlx(default)]
    pub is_series: bool,
    /// Whether the guide airs a kids' programme on this channel (upstream
    /// models this as the channel's "Kids" tag).
    #[sqlx(default)]
    pub is_kids: bool,
}

/// One `FerrofinLiveTvPrograms` row joined to its channel, as the query paths
/// read it.
#[derive(Debug, Clone, Default, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
#[allow(clippy::struct_excessive_bools)] // the upstream ProgramInfo flags
pub struct ProgramRow {
    /// The programme's `Guid`, hyphenated.
    pub id: String,
    /// The owning channel's `Guid`, hyphenated.
    pub channel_id: String,
    /// The airing start (DB text form).
    pub start_date: String,
    /// The airing end (DB text form), if known.
    pub end_date: Option<String>,
    /// The programme title.
    pub title: String,
    /// The episode title, if any.
    pub episode_title: Option<String>,
    /// The description, if any.
    pub overview: Option<String>,
    /// The category list as a JSON array, if any.
    pub genres: Option<String>,
    /// The production year, if any.
    pub production_year: Option<i32>,
    /// The rating string, if any.
    pub official_rating: Option<String>,
    /// Whether the airing is premiering.
    pub is_premiere: bool,
    /// Whether the airing is a repeat.
    pub is_repeat: bool,
    /// Whether the programme is a movie.
    pub is_movie: bool,
    /// Whether the programme is a series episode.
    pub is_series: bool,
    /// Whether the programme is news.
    pub is_news: bool,
    /// Whether the programme is for kids.
    pub is_kids: bool,
    /// Whether the programme is sports.
    pub is_sports: bool,
    /// Whether the airing is live.
    pub is_live: bool,
    /// The listing's own programme id (`{channelId}_{start:O}`), if any.
    pub external_id: Option<String>,
    /// The listing's series id (title MD5), if any.
    pub external_series_id: Option<String>,
    /// The season number, if any.
    pub season_number: Option<i32>,
    /// The episode number, if any.
    pub episode_number: Option<i32>,
    /// When the programme row was first inserted.
    pub date_created: Option<String>,
    /// The owning channel's name.
    pub channel_name: String,
    /// The owning channel's number, if any.
    pub channel_number: Option<String>,
    /// The owning channel's type (`"Tv"`/`"Radio"`).
    pub channel_media_kind: String,
}

/// The per-channel user-data the favourite filters/sorting read, keyed by the
/// stored channel id: `(IsFavorite, Rating)`.
pub type ChannelUserData = std::collections::HashMap<String, (bool, Option<f64>)>;

/// Applies `GetInternalChannels`' filters to the loaded lineup.
///
/// The type filter matches the stored `ChannelType`. The kind flags follow
/// upstream's split (`BaseItemRepository.TranslateQuery` over channel items
/// `GuideManager.RefreshChannels` aggregated): `IsMovie`/`IsSeries` match the
/// channel's aggregated columns, `IsKids` matches the aggregated "Kids" tag,
/// while `IsNews`/`IsSports` translate to the "News"/"Sports" *tags* — which
/// the guide refresh never writes onto a channel — so a `true` value matches
/// nothing and a `false` one everything. `IsDisliked` is accepted and
/// dropped, as upstream's `GetInternalChannels` drops it. The favourite/like
/// filters read `user_data` (`UserItemData.MinLikeValue` upstream is 6.5 on
/// the 0–10 rating scale).
pub fn filter_channel_rows(
    rows: &mut Vec<ChannelRow>,
    query: &ferrofin_traits::stubs::LiveTvChannelQuery,
    user_data: &ChannelUserData,
) {
    if let Some(channel_type) = query.channel_type {
        let want = match channel_type {
            ferrofin_model::live_tv::ChannelType::Radio => "Radio",
            ferrofin_model::live_tv::ChannelType::Tv => "Tv",
        };
        rows.retain(|r| r.channel_type == want);
    }
    if let Some(want) = query.is_movie {
        rows.retain(|r| r.is_movie == want);
    }
    if let Some(want) = query.is_series {
        rows.retain(|r| r.is_series == want);
    }
    if let Some(want) = query.is_kids {
        rows.retain(|r| r.is_kids == want);
    }
    if query.is_news == Some(true) || query.is_sports == Some(true) {
        rows.clear();
    }
    if let Some(want) = query.is_favorite {
        rows.retain(|r| user_data.get(&r.id).is_some_and(|d| d.0) == want);
    }
    if let Some(want) = query.is_liked {
        rows.retain(|r| {
            user_data
                .get(&r.id)
                .and_then(|d| d.1)
                .is_some_and(|x| x >= 6.5)
                == want
        });
    }
}

/// Sorts the lineup the way `GetInternalChannels` builds its order-by list:
/// each requested column takes the single requested order, favourite sorting
/// prepends `(IsFavoriteOrLiked, Desc)`, and `(SortName, Asc)` is appended
/// unless already requested.
///
/// Sort keys the channel cache has no column for keep the lineup order (the
/// appended `SortName` still breaks ties); `IsFavoriteOrLiked` sorts on the
/// `IsFavorite` bit, exactly like the item repository's `translate_query`.
pub fn sort_channel_rows(
    rows: &mut [ChannelRow],
    query: &ferrofin_traits::stubs::LiveTvChannelQuery,
    user_data: &ChannelUserData,
) {
    use ferrofin_model::dto::SortOrder;
    use ferrofin_model::live_tv::ItemSortBy;

    let requested_order = query.sort_order.unwrap_or(SortOrder::Ascending);
    let mut order: Vec<(ItemSortBy, SortOrder)> = query
        .sort_by
        .iter()
        .map(|s| (*s, requested_order))
        .collect();
    if query.enable_favorite_sorting {
        order.insert(0, (ItemSortBy::IsFavoriteOrLiked, SortOrder::Descending));
    }
    if !order.iter().any(|(s, _)| *s == ItemSortBy::SortName) {
        order.push((ItemSortBy::SortName, SortOrder::Ascending));
    }
    let is_favorite = |row: &ChannelRow| user_data.get(&row.id).is_some_and(|d| d.0);
    rows.sort_by(|a, b| {
        for (sort, direction) in &order {
            let ordering = match sort {
                ItemSortBy::SortName | ItemSortBy::Default => {
                    channel_sort_name(a.number.as_deref(), &a.name)
                        .to_lowercase()
                        .cmp(&channel_sort_name(b.number.as_deref(), &b.name).to_lowercase())
                }
                // Upstream maps `ItemSortBy.Name` to `CleanName` — the plain
                // display name, not the number-padded channel sort key.
                ItemSortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                ItemSortBy::IsFavoriteOrLiked => is_favorite(a).cmp(&is_favorite(b)),
                ItemSortBy::DateCreated => a.date_created.cmp(&b.date_created),
                _ => std::cmp::Ordering::Equal,
            };
            let ordering = match direction {
                SortOrder::Ascending => ordering,
                SortOrder::Descending => ordering.reverse(),
            };
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
        }
        std::cmp::Ordering::Equal
    });
}

/// `LiveTvChannel.MediaType`: radio channels are audio, everything else video.
#[must_use]
pub fn channel_media_type(channel_type: &str) -> &'static str {
    if channel_type == "Radio" {
        "Audio"
    } else {
        "Video"
    }
}

/// Port of `LiveTvChannel.CreateSortName`: a parseable number pads to
/// `00000.0` form (`"1"` → `"00001.0-Name"`), an unparseable one is used
/// verbatim (`"A1"` → `"A1-Name"`).
#[must_use]
pub fn channel_sort_name(number: Option<&str>, name: &str) -> String {
    let number = number.unwrap_or("");
    // C# `double.TryParse(Number, InvariantCulture, …)` trims whitespace.
    match number.trim().parse::<f64>() {
        Ok(n) => format!("{n:07.1}-{name}"),
        Err(_) => format!("{number}-{name}"),
    }
}

/// Builds the synthetic `BaseItems` row a channel projects from.
///
/// Port of `GuideManager.GetChannel`'s item shape: `LiveTvChannel` type, no
/// path (the tuner URL is resolved at stream time, not stored on the item),
/// the `CreateSortName` sort key, and the first-seen `DateCreated`.
///
/// `CleanName` and `PresentationUniqueKey` are deliberately left unset: the
/// item store derives both at write time (C# `SaveItem` likewise stamps
/// `CleanName = GetCleanValue(item.Name)` itself), so a value set here would be
/// one nothing reads.
#[must_use]
pub fn channel_entity(
    row: &ChannelRow,
    parse_dt: fn(&str) -> Option<DateTime<Utc>>,
    live_tv_view_id: Option<Uuid>,
) -> BaseItemEntity {
    // `item.ParentId = parentFolderId` where the parent is
    // `GetInternalLiveTvFolder()` — the Live TV `UserView` row. It is also the
    // row's `TopParentId`, which is what puts a channel inside the recursive
    // user universe (`scope_to_user_libraries` treats a Live TV view as
    // standing for itself).
    let parent = live_tv_view_id.map(db_guid);
    BaseItemEntity {
        id: row.id.clone(),
        type_: CHANNEL_TYPE_NAME.to_owned(),
        name: Some(row.name.clone()),
        media_type: Some(channel_media_type(&row.channel_type).to_owned()),
        sort_name: Some(channel_sort_name(row.number.as_deref(), &row.name)),
        // `ExternalId` is `channelInfo.Id` — the TUNER's id (`hdhr_10.1`), not
        // the listing's `tvg-id` (v10.11.8 GuideManager.cs:425). It is what
        // `GetInternalChannelId` hashed to mint this row's GUID, so the two
        // must agree or a re-scan cannot recognise its own channel.
        //
        // No parity probe can catch this one: `ExternalId` is a `BaseItems`
        // column that is not a `BaseItemDto` property in the vendored 10.11.8
        // contract, so it never reaches the wire and the body diff can never
        // see it. The unit test below is its only guard — do not delete it.
        external_id: Some(if row.external_id.is_empty() {
            row.tvg_id.clone()
        } else {
            row.external_id.clone()
        }),
        external_service_id: Some("Emby".to_owned()),
        // `LiveTvChannel.GetBlockUnratedType() => UnratedItem.LiveTvChannel`.
        unrated_type: Some("LiveTvChannel".to_owned()),
        date_created: row.date_created.as_deref().and_then(parse_dt),
        parent_id: parent.clone(),
        top_parent_id: parent,
        is_folder: false,
        ..BaseItemEntity::default()
    }
}

/// Applies to an ALREADY-STORED channel item the bounded set of fields
/// `GuideManager.GetChannel` assigns, returning the row to write back when one
/// of them actually changed — the C#'s `forceUpdate` — and `None` when nothing
/// did.
///
/// Upstream (v10.11.8 GuideManager.cs:375-468) loads the item from the database
/// and assigns exactly `Tags`, `ParentId`, `ChannelType`, `ServiceName`,
/// `ExternalId`, `Number`, `Name` and the primary image, then persists with
/// `UpdateItemAsync` — so every OTHER property of the item survives a guide
/// refresh, and the write happens at all only when `isNew || forceUpdate`.
///
/// Ferrofin's `save_items` is a full-column upsert, so that guarantee has to
/// come from starting at the STORED row rather than at a fresh
/// [`channel_entity`]. Starting at the fresh one is a silent 24-hourly revert:
/// a channel item is a real item now, `POST /Items/{channelId}` writes
/// `Overview`, `Genres`, `Tags`, `CustomRating`, `OfficialRating`, `IsLocked`
/// and `Data` onto it, and the next `RefreshGuide` would put every one of them
/// back to `NULL`.
///
/// `Number`/`ChannelType` are not `BaseItems` columns here — they reach the
/// wire through `LiveTvManager::add_channel_info` from
/// `FerrofinLiveTvChannels`, which the same refresh rewrites — and a channel
/// row carries no image path, so those two upstream assignments have no column
/// to land on.
#[must_use]
pub fn channel_item_update(
    stored: &BaseItemEntity,
    fresh: &BaseItemEntity,
) -> Option<BaseItemEntity> {
    let mut item = stored.clone();
    item.type_.clone_from(&fresh.type_);
    item.name.clone_from(&fresh.name);
    item.media_type.clone_from(&fresh.media_type);
    item.external_id.clone_from(&fresh.external_id);
    item.external_service_id
        .clone_from(&fresh.external_service_id);
    item.unrated_type.clone_from(&fresh.unrated_type);
    item.parent_id.clone_from(&fresh.parent_id);
    item.top_parent_id.clone_from(&fresh.top_parent_id);
    item.is_folder = fresh.is_folder;
    // `SortName` is a lazy property upstream, not a stored assignment:
    // `ForcedSortName` when the metadata editor set one, else
    // `CreateSortName()` off Number/Name. Re-deriving it unconditionally would
    // overwrite a user's forced sort key on every refresh.
    if stored
        .forced_sort_name
        .as_deref()
        .unwrap_or_default()
        .is_empty()
    {
        item.sort_name.clone_from(&fresh.sort_name);
    }
    // `DateCreated` is stamped only on a NEW item, so the stored one stands.
    (item != *stored).then_some(item)
}

/// One `FerrofinLiveTvRecordings` row, as the recording query paths read it.
#[derive(Debug, Clone, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
#[allow(clippy::struct_excessive_bools)] // the upstream TimerInfo flags
pub struct RecordingRow {
    /// The recording's `Guid`, hyphenated.
    pub id: String,
    /// The recorded channel's `Guid`, hyphenated.
    pub channel_id: String,
    /// The firing timer's id — the key `/LiveTv/LiveRecordings/{id}/stream`
    /// takes while the capture is in progress.
    pub timer_id: Option<String>,
    /// The series timer that scheduled it, if any.
    pub series_timer_id: Option<String>,
    /// The programme's name.
    pub name: String,
    /// The programme description.
    pub overview: Option<String>,
    /// The airing start (DB text form).
    pub start_date: String,
    /// The airing end (DB text form), if known.
    pub end_date: Option<String>,
    /// The `RecordingStatus` name.
    pub status: String,
    /// The captured file, once there is one.
    pub path: Option<String>,
    /// When the capture started (DB text form).
    pub date_created: Option<String>,
    /// The episode's own title, when the programme is an episode.
    pub episode_title: Option<String>,
    /// The production year, when known.
    pub production_year: Option<i32>,
    /// The season number, when known.
    pub season_number: Option<i32>,
    /// The episode number, when known.
    pub episode_number: Option<i32>,
    /// The guide programme the timer was created for.
    pub program_id: Option<String>,
    /// The listing provider's own programme id.
    pub external_program_id: Option<String>,
    /// Seconds of recording before the programme's start.
    pub pre_padding_seconds: i32,
    /// Seconds of recording after the programme's end.
    pub post_padding_seconds: i32,
    /// Whether the programme is a movie.
    pub is_movie: bool,
    /// Whether the programme is a series episode.
    pub is_series: bool,
    /// Whether the programme is news.
    pub is_news: bool,
    /// Whether the programme is for kids.
    pub is_kids: bool,
    /// Whether the programme is sport.
    pub is_sports: bool,
    /// Whether the programme is live.
    pub is_live: bool,
    /// Whether the airing is a repeat.
    pub is_repeat: bool,
    /// Whether the airing is a premiere.
    pub is_premiere: bool,
    /// The owning channel's display name (joined).
    pub channel_name: Option<String>,
}

/// Builds the synthetic `BaseItems` row a recording projects from.
///
/// Jellyfin scans its recordings folder and projects the resulting `Video`
/// items; Ferrofin keeps the recordings in its own table (the library-folder
/// end state is a follow-up — see the module docs on
/// [`crate::dvr`]) and builds the equivalent row here, so a recording DTO
/// carries the same `UserData`, image maps, `SortName` and `MediaSources` as
/// any other video.
#[must_use]
pub fn recording_entity(
    row: &RecordingRow,
    parse_dt: fn(&str) -> Option<DateTime<Utc>>,
) -> BaseItemEntity {
    let start_date = parse_dt(&row.start_date);
    let end_date = row.end_date.as_deref().and_then(parse_dt);
    let run_time_ticks = match (start_date, end_date) {
        (Some(start), Some(end)) => Some((end - start).num_milliseconds() * 10_000),
        _ => None,
    };
    BaseItemEntity {
        id: row.id.clone(),
        type_: RECORDING_TYPE_NAME.to_owned(),
        name: Some(row.name.clone()),
        overview: row.overview.clone(),
        episode_title: row.episode_title.clone(),
        channel_id: Some(row.channel_id.clone()),
        path: row.path.clone(),
        start_date,
        end_date,
        run_time_ticks,
        production_year: row.production_year.map(i64::from),
        index_number: row.episode_number.map(i64::from),
        parent_index_number: row.season_number.map(i64::from),
        sort_name: Some(item_sort_name(&row.name)),
        media_type: Some("Video".to_owned()),
        date_created: row.date_created.as_deref().and_then(parse_dt),
        is_movie: row.is_movie,
        is_series: row.is_series,
        is_repeat: row.is_repeat,
        is_folder: false,
        ..BaseItemEntity::default()
    }
}

/// Builds the synthetic `BaseItems` row a programme projects from.
///
/// Port of `GuideManager.GetProgram`'s item shape: the flag-derived `Tags`
/// list (in upstream's exact order), pipe-joined `Genres`,
/// `RunTimeTicks = end - start`, the channel as both `ChannelId` and
/// `ParentId`, and `MediaType` left `"Unknown"` (the C# override is commented
/// out upstream, so lists show `"Unknown"` until the `ChannelInfo` post-pass
/// substitutes the channel's own type).
#[must_use]
pub fn program_entity(
    row: &ProgramRow,
    parse_dt: fn(&str) -> Option<DateTime<Utc>>,
) -> BaseItemEntity {
    let start_date = parse_dt(&row.start_date);
    let end_date = row.end_date.as_deref().and_then(parse_dt);
    let run_time_ticks = match (start_date, end_date) {
        // `RunTimeTicks = (info.EndDate - info.StartDate).Ticks`.
        (Some(start), Some(end)) => Some((end - start).num_milliseconds() * 10_000),
        _ => None,
    };
    let genres: Vec<String> = row
        .genres
        .as_deref()
        .and_then(|g| serde_json::from_str(g).ok())
        .unwrap_or_default();
    let is_series = row.is_series;
    BaseItemEntity {
        id: row.id.clone(),
        type_: PROGRAM_TYPE_NAME.to_owned(),
        name: Some(row.title.clone()),
        overview: row.overview.clone(),
        episode_title: row.episode_title.clone(),
        channel_id: Some(row.channel_id.clone()),
        parent_id: Some(row.channel_id.clone()),
        start_date,
        end_date,
        run_time_ticks,
        production_year: row.production_year.map(i64::from),
        official_rating: row.official_rating.clone(),
        genres: join_multi(&genres),
        tags: join_multi(&program_tags(row)),
        is_movie: row.is_movie,
        is_series,
        is_repeat: row.is_repeat,
        index_number: row.episode_number.map(i64::from),
        parent_index_number: row.season_number.map(i64::from),
        sort_name: Some(item_sort_name(&row.title)),
        external_id: row.external_id.clone(),
        external_series_id: row.external_series_id.clone(),
        // `SeriesName = info.Name` for an episode (not projected on the DTO,
        // kept for fidelity of the synthetic row).
        series_name: (is_series || row.episode_title.is_some()).then(|| row.title.clone()),
        media_type: Some("Unknown".to_owned()),
        date_created: row.date_created.as_deref().and_then(parse_dt),
        is_folder: false,
        ..BaseItemEntity::default()
    }
}

/// The flag-derived tag list, in `GuideManager.GetProgram`'s exact order.
#[must_use]
pub fn program_tags(row: &ProgramRow) -> Vec<String> {
    let mut tags = Vec::new();
    let mut push = |on: bool, tag: &str| {
        if on {
            tags.push(tag.to_owned());
        }
    };
    push(row.is_live, "Live");
    push(row.is_premiere, "Premiere");
    push(row.is_news, "News");
    push(row.is_sports, "Sports");
    push(row.is_kids, "Kids");
    push(row.is_repeat, "Repeat");
    push(row.is_movie, "Movie");
    push(row.is_series, "Series");
    tags
}

/// Strips the four list-path fields Jellyfin's `LiveTvManager.RemoveFields`
/// removes before projecting a channel/programme page:
/// `CanDelete`/`CanDownload`/`DisplayPreferencesId`/`Etag`.
pub fn remove_fields(options: &mut DtoOptions) {
    options.fields.retain(|f| {
        !matches!(
            f,
            ItemFields::CanDelete
                | ItemFields::CanDownload
                | ItemFields::DisplayPreferencesId
                | ItemFields::Etag
        )
    });
}

/// Pipe-joins a name list into the entity's multi-value column form, or `None`
/// when empty (the `BaseItems` storage convention `split_multi` reads back).
fn join_multi(values: &[String]) -> Option<String> {
    if values.is_empty() {
        None
    } else {
        Some(values.join("|"))
    }
}

/// A `Uuid` in the stored `BaseItems` column form (hyphenated uppercase).
#[must_use]
pub fn db_guid(id: Uuid) -> String {
    ferrofin_db::store::guid_to_db(id)
}

/// The alphanumeric sort key for a guide item's name.
///
/// `BaseItem.CreateSortName` + `ModifySortChunks`, shared with the scanner in
/// [`ferrofin_util::sort_name`]: digit runs pad to ten, so "Parity Show 02"
/// sorts as "parity show 0000000002". Guide rows never pass through the scanner,
/// so the key is derived here instead of read from a stored column.
#[must_use]
pub fn item_sort_name(name: &str) -> String {
    ferrofin_util::sort_name::create_sort_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Option<DateTime<Utc>> {
        s.parse().ok()
    }

    #[test]
    fn sort_name_pads_numeric_channel_numbers_like_upstream() {
        // C# string.Format("{0:00000.0}", n) + "-" + name.
        assert_eq!(
            channel_sort_name(Some("1"), "Parity One"),
            "00001.0-Parity One"
        );
        assert_eq!(channel_sort_name(Some("8.5"), "Eight"), "00008.5-Eight");
        assert_eq!(channel_sort_name(Some("123456"), "Big"), "123456.0-Big");
        // An unparseable number is used verbatim.
        assert_eq!(channel_sort_name(Some("A1"), "Alpha"), "A1-Alpha");
        assert_eq!(channel_sort_name(None, "Bare"), "-Bare");
    }

    #[test]
    fn program_tags_follow_get_program_order() {
        let row = ProgramRow {
            id: String::new(),
            channel_id: String::new(),
            start_date: String::new(),
            end_date: None,
            title: String::new(),
            episode_title: None,
            overview: None,
            genres: None,
            production_year: None,
            official_rating: None,
            is_premiere: true,
            is_repeat: true,
            is_movie: false,
            is_series: true,
            is_news: true,
            is_kids: false,
            is_sports: false,
            is_live: true,
            external_id: None,
            external_series_id: None,
            season_number: None,
            episode_number: None,
            date_created: None,
            channel_name: String::new(),
            channel_number: None,
            channel_media_kind: "Tv".to_owned(),
        };
        assert_eq!(
            program_tags(&row),
            ["Live", "Premiere", "News", "Repeat", "Series"]
        );
    }

    #[test]
    fn program_entity_derives_runtime_and_channel_parent() {
        let row = ProgramRow {
            id: "AAAAAAAA-0000-0000-0000-000000000001".to_owned(),
            channel_id: "BBBBBBBB-0000-0000-0000-000000000002".to_owned(),
            start_date: "2026-08-23T18:00:00Z".to_owned(),
            end_date: Some("2026-08-23T19:00:00Z".to_owned()),
            title: "News at Six".to_owned(),
            episode_title: None,
            overview: Some("Hour 0".to_owned()),
            genres: Some(r#"["News"]"#.to_owned()),
            production_year: None,
            official_rating: None,
            is_premiere: false,
            is_repeat: false,
            is_movie: false,
            is_series: false,
            is_news: true,
            is_kids: false,
            is_sports: false,
            is_live: false,
            external_id: Some("parity1_20260823180000".to_owned()),
            external_series_id: None,
            season_number: None,
            episode_number: None,
            date_created: None,
            channel_name: "Parity One".to_owned(),
            channel_number: Some("1".to_owned()),
            channel_media_kind: "Tv".to_owned(),
        };
        let entity = program_entity(&row, parse);
        assert_eq!(entity.type_, PROGRAM_TYPE_NAME);
        assert_eq!(entity.run_time_ticks, Some(36_000_000_000));
        assert_eq!(entity.channel_id.as_deref(), Some(row.channel_id.as_str()));
        assert_eq!(entity.parent_id.as_deref(), Some(row.channel_id.as_str()));
        assert_eq!(entity.genres.as_deref(), Some("News"));
        assert_eq!(entity.tags.as_deref(), Some("News"));
        assert_eq!(entity.media_type.as_deref(), Some("Unknown"));
        assert!(!entity.is_folder);
    }

    #[test]
    fn channel_entity_has_no_path_and_the_emby_service_id() {
        let view = Uuid::from_u128(0x2b2b_ca16_aacc_8a14_d53a_11bb_829e_afa5);
        let row = ChannelRow {
            id: "CCCCCCCC-0000-0000-0000-000000000003".to_owned(),
            tvg_id: "parity1".to_owned(),
            external_id: "hdhr_10.1".to_owned(),
            name: "Parity One".to_owned(),
            number: Some("1".to_owned()),
            channel_type: "Tv".to_owned(),
            date_created: None,
            is_movie: false,
            is_series: false,
            is_kids: false,
        };
        let entity = channel_entity(&row, parse, Some(view));
        assert_eq!(entity.type_, CHANNEL_TYPE_NAME);
        assert_eq!(entity.path, None); // upstream channel items carry no path
        assert_eq!(entity.media_type.as_deref(), Some("Video"));
        assert_eq!(entity.sort_name.as_deref(), Some("00001.0-Parity One"));
        assert_eq!(entity.external_service_id.as_deref(), Some("Emby"));
        assert_eq!(entity.unrated_type.as_deref(), Some("LiveTvChannel"));
        // `ExternalId` is the TUNER's id, which is what the GUID was hashed
        // from — never the listing's tvg-id.
        assert_eq!(entity.external_id.as_deref(), Some("hdhr_10.1"));
        // ParentId == TopParentId == the Live TV UserView row.
        assert_eq!(entity.parent_id.as_deref(), Some(db_guid(view).as_str()));
        assert_eq!(
            entity.top_parent_id.as_deref(),
            Some(db_guid(view).as_str())
        );
    }

    /// A refresh assigns only the properties `GuideManager.GetChannel` assigns,
    /// and writes at all only when one of them changed.
    #[test]
    fn a_channel_update_is_bounded_and_only_written_when_something_changed() {
        let view = Uuid::from_u128(0x2b2b_ca16_aacc_8a14_d53a_11bb_829e_afa5);
        let row = ChannelRow {
            id: "CCCCCCCC-0000-0000-0000-000000000003".to_owned(),
            tvg_id: "parity1".to_owned(),
            external_id: "hdhr_10.1".to_owned(),
            name: "Parity One".to_owned(),
            number: Some("1".to_owned()),
            channel_type: "Tv".to_owned(),
            date_created: None,
            is_movie: false,
            is_series: false,
            is_kids: false,
        };
        let fresh = channel_entity(&row, parse, Some(view));

        // An unchanged lineup is not a write: the C# persists only on
        // `isNew || forceUpdate`.
        assert_eq!(channel_item_update(&fresh, &fresh), None);

        // Everything the metadata editor owns survives the refresh…
        let mut stored = fresh.clone();
        stored.overview = Some("Edited".to_owned());
        stored.genres = Some("News".to_owned());
        stored.tags = Some("Favourite".to_owned());
        stored.custom_rating = Some("PG".to_owned());
        stored.official_rating = Some("TV-14".to_owned());
        stored.is_locked = true;
        stored.data = Some("{}".to_owned());
        stored.date_created = Some(
            DateTime::parse_from_rfc3339("2019-01-02T03:04:05Z")
                .expect("date")
                .with_timezone(&Utc),
        );
        // …and the edit alone is still not a reason to write.
        assert_eq!(channel_item_update(&stored, &fresh), None);

        // A renamed channel IS a write, and carries the edit through.
        let renamed = ChannelRow {
            name: "Parity One HD".to_owned(),
            ..row.clone()
        };
        let fresh = channel_entity(&renamed, parse, Some(view));
        let updated = channel_item_update(&stored, &fresh).expect("the rename is a write");
        assert_eq!(updated.name.as_deref(), Some("Parity One HD"));
        assert_eq!(updated.sort_name, fresh.sort_name);
        assert_eq!(updated.overview.as_deref(), Some("Edited"));
        assert_eq!(updated.genres.as_deref(), Some("News"));
        assert_eq!(updated.tags.as_deref(), Some("Favourite"));
        assert_eq!(updated.custom_rating.as_deref(), Some("PG"));
        assert_eq!(updated.official_rating.as_deref(), Some("TV-14"));
        assert!(updated.is_locked);
        assert_eq!(updated.data.as_deref(), Some("{}"));
        // `DateCreated` is stamped on a NEW item only.
        assert_eq!(updated.date_created, stored.date_created);

        // A user-forced sort key is not a property the refresh owns.
        let mut forced = stored.clone();
        forced.forced_sort_name = Some("zzz".to_owned());
        forced.sort_name = Some("zzz".to_owned());
        let updated = channel_item_update(&forced, &fresh).expect("the rename is still a write");
        assert_eq!(updated.sort_name.as_deref(), Some("zzz"));
    }

    #[test]
    fn item_sort_name_pads_digit_runs_and_strips_articles() {
        // C# ModifySortChunks pads digit runs to 10 (the oracle programme
        // "Parity Show 02 on parity1" sorts as this exact key).
        assert_eq!(
            item_sort_name("Parity Show 02 on parity1"),
            "parity show 0000000002 on parity0000000001"
        );
        assert_eq!(item_sort_name("The 4400"), "0000004400");
        // SortRemoveCharacters and SortReplaceCharacters.
        assert_eq!(item_sort_name("Mr. & Mrs-Smith"), "mr   mrssmith");
        assert_eq!(item_sort_name("A Team"), "team");
        // `ModifySortChunks` ends with `RemoveDiacritics()`, so an accented
        // title sorts as ASCII and interleaves with the rest of the library
        // instead of landing after it under SQLite's BINARY collation.
        assert_eq!(item_sort_name("Café Größe"), "cafe grosse");
    }

    #[test]
    fn a_recording_projects_as_a_video_item() {
        let row = RecordingRow {
            id: "11111111-2222-3333-4444-555555555555".to_owned(),
            channel_id: "66666666-7777-8888-9999-000000000000".to_owned(),
            timer_id: Some("t1".to_owned()),
            series_timer_id: None,
            name: "The Late Show".to_owned(),
            overview: Some("Tonight".to_owned()),
            start_date: "2026-08-23T17:00:00Z".to_owned(),
            end_date: Some("2026-08-23T18:00:00Z".to_owned()),
            status: "Completed".to_owned(),
            path: Some("/data/livetv/recordings/Other/The Late Show/x.ts".to_owned()),
            date_created: Some("2026-08-23T17:00:00Z".to_owned()),
            episode_title: Some("Pilot".to_owned()),
            production_year: Some(2026),
            season_number: Some(1),
            episode_number: Some(2),
            program_id: None,
            external_program_id: None,
            pre_padding_seconds: 0,
            post_padding_seconds: 0,
            is_movie: false,
            is_series: true,
            is_news: false,
            is_kids: false,
            is_sports: false,
            is_live: false,
            is_repeat: false,
            is_premiere: false,
            channel_name: Some("Parity One".to_owned()),
        };
        let entity = recording_entity(&row, parse);

        // Jellyfin's recordings are library `Video` items scanned out of the
        // recordings folder — not a `Recording` type of their own.
        assert_eq!(entity.type_, RECORDING_TYPE_NAME);
        assert_eq!(entity.media_type.as_deref(), Some("Video"));
        assert_eq!(entity.name.as_deref(), Some("The Late Show"));
        assert_eq!(entity.path, row.path);
        assert!(!entity.is_folder);
        assert_eq!(entity.index_number, Some(2));
        assert_eq!(entity.parent_index_number, Some(1));
        assert_eq!(entity.production_year, Some(2026));
        // One hour, in ticks.
        assert_eq!(entity.run_time_ticks, Some(36_000_000_000));
        assert_eq!(entity.sort_name.as_deref(), Some("late show"));
    }

    #[test]
    fn remove_fields_strips_exactly_the_four_list_fields() {
        let mut options = DtoOptions::default();
        let before = options.fields.len();
        remove_fields(&mut options);
        assert_eq!(options.fields.len(), before - 4);
        for f in [
            ItemFields::CanDelete,
            ItemFields::CanDownload,
            ItemFields::DisplayPreferencesId,
            ItemFields::Etag,
        ] {
            assert!(!options.contains_field(f));
        }
    }
}
