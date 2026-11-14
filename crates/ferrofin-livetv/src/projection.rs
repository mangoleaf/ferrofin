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

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::entities::ImageType;
use ferrofin_model::querying::ItemFields;
use ferrofin_traits::options::{DtoOptions, ItemImageInfo};
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
    /// `ChannelInfo.ImageUrl`: the tuner's logo, or the matched guide
    /// channel's. Read only by the guide refresh's item mirror.
    #[sqlx(default)]
    pub image_url: Option<String>,
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
    /// The listing's showing-identity key (`ProgramInfo.ShowId`), if any.
    pub show_id: Option<String>,
    /// The owning channel's name.
    pub channel_name: String,
    /// The owning channel's number, if any.
    pub channel_number: Option<String>,
    /// The owning channel's type (`"Tv"`/`"Radio"`).
    pub channel_media_kind: String,
}

/// One channel's user data for the requesting user: its row under the
/// channel's own id key. `UserDataManager.SaveUserData` writes the same values
/// under every key of the item, so this row answers what upstream reads from
/// the first row by key (`IsFavorite`, `PlayCount`), from any row (`Played`)
/// and from the newest (`LastPlayedDate`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChannelUserDataRow {
    /// `UserData.IsFavorite`.
    pub is_favorite: bool,
    /// `UserData.Rating`, on the 0–10 scale.
    pub rating: Option<f64>,
    /// `UserData.PlayCount`.
    pub play_count: i32,
    /// `UserData.Played`.
    pub played: bool,
    /// `UserData.LastPlayedDate` as stored. Every writer stores one text
    /// format, so it orders as SQLite orders the column upstream.
    pub last_played_date: Option<String>,
}

/// The requesting user's channel user data, keyed by the stored channel id.
pub type ChannelUserData = std::collections::HashMap<String, ChannelUserDataRow>;

/// Applies `GetInternalChannels`' filters to the loaded lineup.
///
/// The type filter matches the stored `ChannelType`. The kind flags are
/// applied by the read itself ([`crate::guide_repository`]'s `channel_rows`,
/// from the stored channel items). `IsDisliked` is accepted and
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
            ferrofin_model::live_tv::ChannelType::Unrecognized(_) => {
                rows.clear();
                return;
            }
        };
        rows.retain(|r| r.channel_type == want);
    }
    if let Some(want) = query.is_favorite {
        rows.retain(|r| user_data.get(&r.id).is_some_and(|d| d.is_favorite) == want);
    }
    if let Some(want) = query.is_liked {
        rows.retain(|r| {
            user_data
                .get(&r.id)
                .and_then(|d| d.rating)
                .is_some_and(|x| x >= 6.5)
                == want
        });
    }
}

/// Sorts the lineup the way `GetInternalChannels` builds its order-by list and
/// `BaseItemRepository.ApplyOrder` applies it: each requested key but
/// `Default` (which `ApplyOrder` drops) takes the single requested order,
/// favourite sorting prepends `(IsFavoriteOrLiked, Desc)`, and
/// `(SortName, Asc)` is appended unless already requested. A
/// `SeriesDatePlayed` key hands the whole order to
/// `ApplySeriesDatePlayedOrder`: the series' played date, which no channel
/// has, then `SortName` ascending.
///
/// Each key compares as `OrderMapper.MapOrderByField` maps it for a channel
/// item. The user-data keys read `user_data`, where a channel without a row
/// is `NULL` for `DatePlayed` and `PlayCount` (first ascending, last
/// descending), unplayed for `IsPlayed`/`IsUnplayed` (through
/// `BuildIsPlayedFilter`) and no favourite. `Random` draws a fresh order on
/// every call. `AirTime` and the keys `OrderMapper` has no arm for fall back
/// to `SortName`. The keys over columns a channel item never fills
/// (`RunTimeTicks`, `PremiereDate`, the ratings, the studios, …) or fills
/// alike (`IsFolder`) tie. `IsFavoriteOrLiked` sorts on the `IsFavorite`
/// bit, exactly like the item repository's `translate_query`.
pub fn sort_channel_rows(
    rows: &mut Vec<ChannelRow>,
    query: &ferrofin_traits::stubs::LiveTvChannelQuery,
    user_data: &ChannelUserData,
) {
    use ferrofin_model::dto::SortOrder;
    use ferrofin_model::live_tv::ItemSortBy;
    use std::cmp::Ordering;

    let requested_order = query.sort_order.unwrap_or(SortOrder::Ascending);
    let mut order: Vec<(ItemSortBy, SortOrder)> = query
        .sort_by
        .iter()
        .filter(|s| **s != ItemSortBy::Default)
        .map(|s| (*s, requested_order))
        .collect();
    if query.enable_favorite_sorting {
        order.insert(0, (ItemSortBy::IsFavoriteOrLiked, SortOrder::Descending));
    }
    if !order.iter().any(|(s, _)| *s == ItemSortBy::SortName) {
        order.push((ItemSortBy::SortName, SortOrder::Ascending));
    }
    if order
        .iter()
        .any(|(s, _)| *s == ItemSortBy::SeriesDatePlayed)
    {
        order = vec![(ItemSortBy::SortName, SortOrder::Ascending)];
    }
    // A shuffled rank per row is `ORDER BY RANDOM()`'s per-row draw.
    let mut random: Vec<usize> = Vec::new();
    if order.iter().any(|(s, _)| *s == ItemSortBy::Random) {
        random = (0..rows.len()).collect();
        ferrofin_util::shuffle_extensions::shuffle(&mut random);
    }
    // Each row's keys are built once: rebuilding the padded sort name on both
    // sides of every comparison dominated the sort of a large lineup.
    let mut keyed: Vec<KeyedChannel<'_>> = rows
        .drain(..)
        .enumerate()
        .map(|(index, row)| KeyedChannel {
            sort_name: channel_sort_name(row.number.as_deref(), &row.name).to_lowercase(),
            name: row.name.to_lowercase(),
            user_data: user_data.get(&row.id),
            random: random.get(index).copied().unwrap_or_default(),
            row,
        })
        .collect();
    keyed.sort_by(|a, b| {
        for (sort, direction) in &order {
            let ordering = match sort {
                // Upstream maps `ItemSortBy.Name` to `CleanName` — the plain
                // display name, not the number-padded channel sort key.
                ItemSortBy::Name => a.name.cmp(&b.name),
                ItemSortBy::IsFavoriteOrLiked => a.is_favorite().cmp(&b.is_favorite()),
                ItemSortBy::DateCreated => a.row.date_created.cmp(&b.row.date_created),
                ItemSortBy::DatePlayed => a.last_played().cmp(&b.last_played()),
                ItemSortBy::PlayCount => a.play_count().cmp(&b.play_count()),
                ItemSortBy::IsPlayed => a.played().cmp(&b.played()),
                ItemSortBy::IsUnplayed => b.played().cmp(&a.played()),
                ItemSortBy::Random => a.random.cmp(&b.random),
                ItemSortBy::Album
                | ItemSortBy::AlbumArtist
                | ItemSortBy::Artist
                | ItemSortBy::OfficialRating
                | ItemSortBy::PremiereDate
                | ItemSortBy::StartDate
                | ItemSortBy::Runtime
                | ItemSortBy::CommunityRating
                | ItemSortBy::ProductionYear
                | ItemSortBy::CriticRating
                | ItemSortBy::IsFolder
                | ItemSortBy::SeriesSortName
                | ItemSortBy::VideoBitRate
                | ItemSortBy::Studio
                | ItemSortBy::DateLastContentAdded
                | ItemSortBy::ParentIndexNumber
                | ItemSortBy::IndexNumber => Ordering::Equal,
                // `SortName`, `AirTime`'s arm and `OrderMapper`'s
                // `_ => e.SortName` (`AiredEpisodeOrder`, an unknown value).
                _ => a.sort_name.cmp(&b.sort_name),
            };
            let ordering = match direction {
                SortOrder::Ascending => ordering,
                SortOrder::Descending | SortOrder::Unrecognized(_) => ordering.reverse(),
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    });
    rows.extend(keyed.into_iter().map(|keyed| keyed.row));
}

/// Whether [`sort_channel_rows`] reads `user_data` for one of `sort_by`'s
/// keys, so the caller must load it.
#[must_use]
pub fn sort_reads_user_data(sort_by: &[ferrofin_model::live_tv::ItemSortBy]) -> bool {
    use ferrofin_model::live_tv::ItemSortBy;
    sort_by.iter().any(|by| {
        matches!(
            by,
            ItemSortBy::IsFavoriteOrLiked
                | ItemSortBy::DatePlayed
                | ItemSortBy::PlayCount
                | ItemSortBy::IsPlayed
                | ItemSortBy::IsUnplayed
        )
    })
}

/// A lineup row with the keys [`sort_channel_rows`] sorts it on.
struct KeyedChannel<'a> {
    sort_name: String,
    name: String,
    user_data: Option<&'a ChannelUserDataRow>,
    random: usize,
    row: ChannelRow,
}

impl KeyedChannel<'_> {
    fn is_favorite(&self) -> bool {
        self.user_data.is_some_and(|d| d.is_favorite)
    }

    fn played(&self) -> bool {
        self.user_data.is_some_and(|d| d.played)
    }

    fn play_count(&self) -> Option<i32> {
        self.user_data.map(|d| d.play_count)
    }

    fn last_played(&self) -> Option<&str> {
        self.user_data.and_then(|d| d.last_played_date.as_deref())
    }
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

/// Port of Jellyfin 12's `LiveTvChannel.CreateSortName`: a parseable number
/// pads to `0000000000.00000` form (`"1"` → `"0000000001.00000-Name"`), so
/// six-digit numbers and two-decimal subchannels sort numerically; an
/// unparseable one is used verbatim (`"A1"` → `"A1-Name"`).
#[must_use]
pub fn channel_sort_name(number: Option<&str>, name: &str) -> String {
    let number = number.unwrap_or("");
    // C# `double.TryParse(Number, InvariantCulture, …)` trims whitespace.
    match number.trim().parse::<f64>() {
        // .NET's custom format keeps the sign outside the zero padding
        // (`-1` → `-0000000001.00000`), where Rust's width counts it; a number
        // wider than the pattern keeps all its digits in both.
        Ok(n) if n.is_finite() => {
            let sign = if n < 0.0 { "-" } else { "" };
            format!("{sign}{:016.5}-{name}", n.abs())
        }
        // Rust also parses `inf`/`nan`, which have no padded form; keep the
        // text (.NET rejects `inf`, and formats a parsed `NaN`/`Infinity` as
        // that word).
        _ => format!("{number}-{name}"),
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
/// `FerrofinLiveTvChannels`, which the same refresh rewrites. The primary
/// image is an image row, written by [`channel_images_update`]'s caller.
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

/// The tag `RefreshChannelsInternal` adds to a channel with a kids airing,
/// and `GetProgram` to a kids airing.
const KIDS_TAG: &str = "Kids";

impl ProgramRow {
    /// The listing's own `ProgramInfo.IsSeries`, which the channel aggregate
    /// ORs (`GuideManager.cs:276`). The stored `IsSeries` is `GetProgram`'s
    /// widened `isSeries || EpisodeTitle` (`:546`). The listing's flag is an
    /// episode number on an XMLTV airing (`Episode is not null`; a movie has
    /// neither), and a series id on a Schedules Direct one, which sets it only
    /// for a series. An airing with only a sub-title is no series here.
    #[must_use]
    pub fn listed_as_series(&self) -> bool {
        self.is_series && (self.episode_number.is_some() || self.external_series_id.is_some())
    }
}

/// What `GuideManager.RefreshChannelsInternal` aggregates over one channel's
/// programmes (`isMovie |= program.IsMovie`, …, `GuideManager.cs:275-279`) and
/// stores on the channel item. `IsNews` and `IsSports` are not `BaseItems`
/// columns and are left out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelAggregate {
    /// Whether any airing is a movie.
    pub is_movie: bool,
    /// Whether any airing is a series episode as the listing marks it
    /// ([`ProgramRow::listed_as_series`]).
    pub is_series: bool,
    /// Whether any airing is for kids.
    pub is_kids: bool,
}

impl ChannelAggregate {
    /// The aggregate of one channel's guide `rows`; none at all is all false.
    #[must_use]
    pub fn of(rows: &[ProgramRow]) -> Self {
        rows.iter().fold(Self::default(), |seen, row| Self {
            is_movie: seen.is_movie || row.is_movie,
            is_series: seen.is_series || row.listed_as_series(),
            is_kids: seen.is_kids || row.is_kids,
        })
    }
}

/// Whether a stored `Tags` column holds a tag value, the only kind `ItemValues`
/// indexes (`item_values_of` skips blank ones): Jellyfin stores `''` for an
/// item with none (`BaseItemMapper.cs:334` over an empty list), which carries
/// no tag, and neither does `|` or a blank.
#[must_use]
pub fn carries_tags(tags: Option<&str>) -> bool {
    tags.is_some_and(|tags| tags.split('|').any(|tag| !tag.trim().is_empty()))
}

/// Applies a channel's [`ChannelAggregate`] to its STORED item, as
/// `RefreshChannelsInternal` does before `UpdateToRepositoryAsync`
/// (`GuideManager.cs:306-316`): `IsMovie` and `IsSeries` take the aggregate,
/// and a kids airing adds the "Kids" tag the way `AddTag` does (never removing
/// it, and not again in another case). Every other column, the editor's tags
/// included, is the stored row's. Returns the row to write back, or `None`
/// when nothing changed.
#[must_use]
pub fn channel_aggregate_update(
    stored: &BaseItemEntity,
    aggregate: ChannelAggregate,
) -> Option<BaseItemEntity> {
    let mut item = stored.clone();
    item.is_movie = aggregate.is_movie;
    item.is_series = aggregate.is_series;
    if aggregate.is_kids {
        let mut tags: Vec<String> = item
            .tags
            .as_deref()
            .unwrap_or_default()
            .split('|')
            .filter(|tag| !tag.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if !tags
            .iter()
            .any(|tag| crate::mapping::eq_ordinal_ignore_case(tag, KIDS_TAG))
        {
            tags.push(KIDS_TAG.to_owned());
            item.tags = join_multi(&tags);
        }
    }
    (item != *stored).then_some(item)
}

/// Whether a tuner's or guide's artwork value may become an image row: an
/// absolute `http` or `https` URL.
///
/// Upstream's `SetImagePath` also accepts a local file path here; Ferrofin keeps
/// only URLs. Jellyfin bug not ported; see the `images` row of
/// `handlers::VERIFIED_SUBSYSTEMS`. Every source's artwork (M3U, XMLTV, Schedules
/// Direct; an HDHomeRun lineup has none) reaches an image row only through
/// [`channel_images_update`] and [`program_images_update`], which apply this.
pub(crate) fn is_artwork_url(value: &str) -> bool {
    crate::fetch::has_scheme(value, &["http", "https"])
}

/// A remote image row for `url`: upstream's `new ItemImageInfo { Path = url,
/// Type = type }`. The image routes serve it through the remote-image cache.
fn remote_image(image_type: ImageType, url: &str) -> ItemImageInfo {
    ItemImageInfo {
        path: url.to_owned(),
        image_type,
        ..ItemImageInfo::default()
    }
}

/// `item.SetImage(image, 0)`: the first image of its type is replaced, or the
/// image is added when the item has none of that type.
fn set_first_image(images: &mut Vec<ItemImageInfo>, image: ItemImageInfo) {
    match images.iter().position(|i| i.image_type == image.image_type) {
        Some(index) => images[index] = image,
        None => images.push(image),
    }
}

/// The channel's image list after `GuideManager.GetChannel`'s Primary rule,
/// or `None` when it leaves the images as they are.
///
/// Upstream (v12.2 GuideManager.cs:460-477) sets the Primary to the
/// channel's `ImageUrl` when the channel has no Primary, or when the URL is
/// not blank and differs (ordinally) from the Primary's path. Upstream's
/// `UpdateImagesAsync` then downloads it into a local file, whose path never
/// equals the URL again, so every refresh re-sets and re-downloads every
/// logo. Ferrofin keeps the row at the URL (the image routes serve it from the
/// remote-image cache), so the comparison holds and an unchanged logo is
/// left alone. A Primary that is a local file (an adopted Jellyfin channel's
/// downloaded logo, or one set in the metadata editor) never equals the URL,
/// so it is replaced by the tuner's or guide's logo, as upstream replaces it
/// on every refresh.
///
/// A logo that is not a URL (`is_artwork_url`) sets nothing. A Primary an
/// earlier version stored from that same value is removed: the image routes
/// no longer serve it, and the old version can store it again beside this one
/// during a rolling update, so it goes on a refresh, not once.
#[must_use]
pub fn channel_images_update(
    current: &[ItemImageInfo],
    image_url: Option<&str>,
) -> Option<Vec<ItemImageInfo>> {
    let url = image_url.filter(|url| !url.trim().is_empty())?;
    let primary = current
        .iter()
        .position(|i| i.image_type == ImageType::Primary);
    let stored = primary.filter(|&index| current[index].path == url);
    match (is_artwork_url(url), stored) {
        (true, Some(_)) | (false, None) => None,
        (true, None) => {
            let mut images = current.to_vec();
            set_first_image(&mut images, remote_image(ImageType::Primary, url));
            Some(images)
        }
        (false, Some(index)) => {
            let mut images = current.to_vec();
            images.remove(index);
            Some(images)
        }
    }
}

/// The artwork a listing supplies for one programme: `ProgramInfo.ImageUrl`
/// (the `<icon>`), `ThumbImageUrl` (the first `still` `<image>`) and
/// `BackdropImageUrl` (the first `backdrop` `<image>`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProgramImages {
    /// The Primary image URL.
    pub primary: Option<String>,
    /// The Thumb image URL.
    pub thumb: Option<String>,
    /// The Backdrop image URL.
    pub backdrop: Option<String>,
}

impl ProgramImages {
    /// The listing's URLs, in `UpdateImages` order, with a value that is no URL
    /// ([`is_artwork_url`]) read as none.
    fn urls(&self) -> [(ImageType, Option<&str>); 3] {
        [
            (ImageType::Primary, &self.primary),
            (ImageType::Thumb, &self.thumb),
            (ImageType::Backdrop, &self.backdrop),
        ]
        .map(|(image_type, url)| (image_type, url.as_deref().filter(|url| is_artwork_url(url))))
    }

    /// How many of the listing's values are neither blank nor a URL.
    pub(crate) fn refused(&self) -> usize {
        [&self.primary, &self.thumb, &self.backdrop]
            .into_iter()
            .flatten()
            .filter(|value| !value.trim().is_empty() && !is_artwork_url(value))
            .count()
    }
}

/// The programme's image list after `GuideManager.UpdateImages`, or `None`
/// when nothing is to be written.
///
/// Port of `UpdateImage` (v12.2 GuideManager.cs:694-748) for Primary, Thumb,
/// Logo and Backdrop in that order: the first image of a type is left alone
/// when its path equals the listing's URL ignoring case, replaced when the
/// listing has a URL, and removed when it has none. XMLTV has no logo URL, so
/// a programme's Logo goes too. A removal alone does not mark the programme
/// updated upstream (`UpdateImage` returns `false`), so it is only written
/// when the programme's other fields changed (`item_updated`) — then the item,
/// removal included, is saved. A listing value that is not a URL
/// (`is_artwork_url`) counts as none.
#[must_use]
pub fn program_images_update(
    current: &[ItemImageInfo],
    listing: &ProgramImages,
    item_updated: bool,
) -> Option<Vec<ItemImageInfo>> {
    let mut images = current.to_vec();
    let mut updated = false;
    let mut removed = false;
    let [primary, thumb, backdrop] = listing.urls();
    for (image_type, url) in [primary, thumb, (ImageType::Logo, None), backdrop] {
        let first = images.iter().position(|i| i.image_type == image_type);
        let same = first
            .zip(url)
            .is_some_and(|(index, url)| images[index].path.eq_ignore_ascii_case(url));
        if same {
            continue;
        }
        match (url, first) {
            (Some(url), _) => {
                set_first_image(&mut images, remote_image(image_type, url));
                updated = true;
            }
            (None, Some(index)) => {
                images.remove(index);
                removed = true;
            }
            (None, None) => {}
        }
    }
    (updated || (removed && item_updated)).then_some(images)
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
///
/// `live_tv_view_id` is the row's `TopParentId`, NOT its `ParentId`: upstream
/// stores a programme with `CreateItems(newPrograms, currentChannel, …)`
/// (GuideManager.cs:277) so the channel is the parent, while
/// `BaseItemRepository` derives `TopParentId` from `item.TopParent?.Id` — the
/// walk up through the channel to the Live TV `UserView`. That column is what
/// puts a programme inside the recursive user universe (`scope_to_user_libraries`
/// drops a NULL-`TopParentId` row from every unscoped user query), so without it
/// `GET /Items?recursive=true&includeItemTypes=LiveTvProgram` is empty and
/// `GET /Items/{programId}` is a 404 for an id `GET /LiveTv/Programs` just
/// handed the client.
#[must_use]
pub fn program_entity(
    row: &ProgramRow,
    parse_dt: fn(&str) -> Option<DateTime<Utc>>,
    live_tv_view_id: Option<Uuid>,
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
        // `GetProgram` assigns `ExternalSeriesId` only from a non-blank
        // listing id (`GuideManager.cs:540-545`).
        external_series_id: row
            .external_series_id
            .clone()
            .filter(|id| !id.trim().is_empty()),
        // `SeriesName = info.Name` for an episode (not projected on the DTO,
        // kept for fidelity of the synthetic row).
        series_name: (is_series || row.episode_title.is_some()).then(|| row.title.clone()),
        media_type: Some("Unknown".to_owned()),
        date_created: row.date_created.as_deref().and_then(parse_dt),
        show_id: row.show_id.clone(),
        top_parent_id: live_tv_view_id.map(db_guid),
        // `LiveTvProgram.GetBlockUnratedType() => UnratedItem.LiveTvProgram`
        // (v10.11.8 MediaBrowser.Controller/LiveTv/LiveTvProgram.cs:210-213).
        unrated_type: Some("LiveTvProgram".to_owned()),
        // `LiveTvProgram`'s constructor sets `IsVirtualItem = true`
        // (LiveTvProgram.cs:26-29): an airing has no file behind it. The column
        // is load-bearing on the wire — `?locationTypes=FileSystem` returns no
        // programmes on the oracle because of it.
        is_virtual_item: true,
        // `item.Width`/`item.Height` are non-nullable `int`s upstream, so a
        // programme stores 0/0 unless the listing declares HD
        // (`if (info.IsHD ?? false) { Width = 1280; Height = 720; }`,
        // GuideManager.cs:566-570). `XmlTvListingsProvider.GetProgramInfo` never
        // sets `IsHD`, so an XMLTV guide always takes the 0/0 branch; the DTO
        // layer only projects a dimension greater than zero, so neither reaches
        // the wire.
        width: Some(0),
        height: Some(0),
        is_folder: false,
        ..BaseItemEntity::default()
    }
}

/// Applies to an ALREADY-STORED programme item the bounded set of fields
/// `GuideManager.GetProgram` assigns, returning the row to write back when one
/// of them actually changed — the C#'s `isUpdated` — and `None` when nothing
/// did.
///
/// The sibling of [`channel_item_update`], and for the same reason: upstream
/// (v10.11.8 GuideManager.cs:473-640) LOADS the existing `LiveTvProgram` out of
/// `existingPrograms` and assigns only the listing's own fields onto it, then
/// persists through `UpdateItemsAsync`. Ferrofin's `save_items` is a full-column
/// upsert that sets every column from `excluded.`, so rebuilding the row from
/// [`program_entity`] and saving it would revert, on the guide timer, everything
/// the metadata editor wrote onto a programme item — which is a real item now
/// that `POST /Items/{programId}` can write to.
///
/// `ForcedSortName` is left alone and `SortName` re-derived only when no forced
/// key is set, because upstream's `SortName` is a lazy property, not a stored
/// assignment. `DateCreated` is stamped on a NEW item only.
#[must_use]
pub fn program_item_update(
    stored: &BaseItemEntity,
    fresh: &BaseItemEntity,
) -> Option<BaseItemEntity> {
    let mut item = stored.clone();
    item.type_.clone_from(&fresh.type_);
    item.name.clone_from(&fresh.name);
    item.overview.clone_from(&fresh.overview);
    item.episode_title.clone_from(&fresh.episode_title);
    item.channel_id.clone_from(&fresh.channel_id);
    item.parent_id.clone_from(&fresh.parent_id);
    item.top_parent_id.clone_from(&fresh.top_parent_id);
    item.start_date = fresh.start_date;
    item.end_date = fresh.end_date;
    item.run_time_ticks = fresh.run_time_ticks;
    item.production_year = fresh.production_year;
    item.index_number = fresh.index_number;
    item.parent_index_number = fresh.parent_index_number;
    item.premiere_date = fresh.premiere_date;
    item.genres.clone_from(&fresh.genres);
    item.tags.clone_from(&fresh.tags);
    item.is_movie = fresh.is_movie;
    item.is_series = fresh.is_series;
    item.is_repeat = fresh.is_repeat;
    item.official_rating.clone_from(&fresh.official_rating);
    item.external_id.clone_from(&fresh.external_id);
    // `if (!IsNullOrWhiteSpace(seriesId) && !Equals(item.ExternalSeriesId,
    // seriesId, OrdinalIgnoreCase))` (`GuideManager.cs:540-545`): a listing
    // with no series id keeps the stored one, and one differing only in case
    // keeps its spelling.
    if let Some(series) = fresh.external_series_id.as_deref()
        && !stored
            .external_series_id
            .as_deref()
            .is_some_and(|stored| crate::mapping::eq_ordinal_ignore_case(stored, series))
    {
        item.external_series_id = Some(series.to_owned());
    }
    item.series_name.clone_from(&fresh.series_name);
    item.show_id.clone_from(&fresh.show_id);
    item.media_type.clone_from(&fresh.media_type);
    item.unrated_type.clone_from(&fresh.unrated_type);
    item.is_virtual_item = fresh.is_virtual_item;
    item.width = fresh.width;
    item.height = fresh.height;
    item.is_folder = fresh.is_folder;
    if stored
        .forced_sort_name
        .as_deref()
        .unwrap_or_default()
        .is_empty()
    {
        item.sort_name.clone_from(&fresh.sort_name);
    }
    (item != *stored).then_some(item)
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
    push(row.is_kids, KIDS_TAG);
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
/// when empty (the `BaseItems` storage convention `split_multi` reads back),
/// each name once: `BaseItemMapper.Map` joins `Genres` and `Tags` after a
/// `Distinct(StringComparer.OrdinalIgnoreCase)` (`BaseItemMapper.cs:306,334`),
/// the first spelling kept.
fn join_multi(values: &[String]) -> Option<String> {
    let mut seen = HashSet::with_capacity(values.len());
    let distinct: Vec<&str> = values
        .iter()
        .map(String::as_str)
        .filter(|value| seen.insert(crate::mapping::ordinal_ignore_case_key(value)))
        .collect();
    if distinct.is_empty() {
        None
    } else {
        Some(distinct.join("|"))
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
    use ferrofin_model::dto::SortOrder;
    use ferrofin_model::live_tv::ItemSortBy;

    fn parse(s: &str) -> Option<DateTime<Utc>> {
        s.parse().ok()
    }

    fn image(image_type: ImageType, path: &str) -> ItemImageInfo {
        remote_image(image_type, path)
    }

    /// `GuideManager.GetChannel`'s Primary rule.
    #[test]
    fn a_channel_takes_its_logo_when_it_has_none_or_it_moved() {
        let logo = image(ImageType::Primary, "http://img/a.png");
        // No Primary: the URL becomes it.
        assert_eq!(
            channel_images_update(&[], Some("http://img/a.png")),
            Some(vec![logo.clone()])
        );
        // The same URL (ordinal): nothing to write.
        assert_eq!(
            channel_images_update(std::slice::from_ref(&logo), Some("http://img/a.png")),
            None
        );
        // A different URL, even by case only, replaces it and keeps the rest.
        let backdrop = image(ImageType::Backdrop, "/meta/b.jpg");
        assert_eq!(
            channel_images_update(&[logo.clone(), backdrop.clone()], Some("http://img/A.png")),
            Some(vec![
                image(ImageType::Primary, "http://img/A.png"),
                backdrop
            ])
        );
        // No URL, or a blank one, never removes the logo it has.
        assert_eq!(
            channel_images_update(std::slice::from_ref(&logo), None),
            None
        );
        assert_eq!(channel_images_update(&[logo], Some("  ")), None);
        assert_eq!(channel_images_update(&[], None), None);
    }

    /// Artwork is an absolute `http` or `https` URL and nothing else.
    #[rstest::rstest]
    #[case::absolute_path("/var/lib/art.png", false)]
    #[case::relative_path("art/logo.png", false)]
    #[case::parent_path("../art.png", false)]
    #[case::windows_path(r"C:\art\logo.png", false)]
    #[case::file_url("file:///var/lib/art.png", false)]
    #[case::file_url_without_authority("file:/var/lib/art.png", false)]
    #[case::another_scheme("ftp://img/a.png", false)]
    #[case::starts_like_http("httpfoo/a.png", false)]
    #[case::scheme_without_slashes("http:/a.png", false)]
    #[case::padded(" http://img/a.png", false)]
    #[case::scheme_relative("//img/a.png", false)]
    #[case::blank("  ", false)]
    #[case::http("http://img/a.png", true)]
    #[case::https("https://img/a.png", true)]
    #[case::any_case("HTTPS://IMG/A.PNG", true)]
    fn artwork_is_a_url(#[case] value: &str, #[case] expected: bool) {
        assert_eq!(is_artwork_url(value), expected);
    }

    /// A logo that is no URL sets nothing, and takes the row an earlier version
    /// stored from it, but no other.
    #[test]
    fn a_channel_logo_that_is_no_url_sets_nothing() {
        let refused = "/var/lib/art.png";
        assert_eq!(channel_images_update(&[], Some(refused)), None);
        // A logo from the metadata editor, or a URL, stays.
        for path in ["/meta/library/primary.png", "http://img/a.png"] {
            let primary = image(ImageType::Primary, path);
            assert_eq!(channel_images_update(&[primary], Some(refused)), None);
        }
        let backdrop = image(ImageType::Backdrop, "/meta/b.jpg");
        assert_eq!(
            channel_images_update(
                &[image(ImageType::Primary, refused), backdrop.clone()],
                Some(refused)
            ),
            Some(vec![backdrop])
        );
    }

    /// A listing value that is no URL is a listing without that image.
    #[test]
    fn a_programme_value_that_is_no_url_counts_as_none() {
        let refused = "file:///var/lib/art.png";
        let listing = ProgramImages {
            primary: Some(refused.to_owned()),
            thumb: Some("  ".to_owned()),
            backdrop: Some("http://img/b.jpg".to_owned()),
        };
        assert_eq!(listing.refused(), 1);
        let backdrop = image(ImageType::Backdrop, "http://img/b.jpg");
        assert_eq!(
            program_images_update(&[], &listing, false),
            Some(vec![backdrop.clone()])
        );
        // The row an earlier version stored from it goes as a removal does:
        // only with a programme that is written anyway.
        let stored = [image(ImageType::Primary, refused), backdrop.clone()];
        assert_eq!(program_images_update(&stored, &listing, false), None);
        assert_eq!(
            program_images_update(&stored, &listing, true),
            Some(vec![backdrop])
        );
    }

    /// `GuideManager.UpdateImages`/`UpdateImage`.
    #[test]
    fn a_programmes_images_follow_its_listing() {
        let listing = ProgramImages {
            primary: Some("http://img/p.png".to_owned()),
            thumb: Some("http://img/t.jpg".to_owned()),
            backdrop: None,
        };
        let primary = image(ImageType::Primary, "http://img/p.png");
        let thumb = image(ImageType::Thumb, "http://img/t.jpg");
        assert_eq!(
            program_images_update(&[], &listing, false),
            Some(vec![primary.clone(), thumb.clone()])
        );
        // Unchanged, ignoring case: nothing to write.
        let upper = image(ImageType::Primary, "HTTP://IMG/P.PNG");
        assert_eq!(
            program_images_update(&[upper, thumb.clone()], &listing, false),
            None
        );
        // A logo or backdrop the listing no longer has is removed, but a
        // removal alone is written only with the programme's other changes.
        let logo = image(ImageType::Logo, "/meta/logo.png");
        let current = [primary.clone(), thumb.clone(), logo];
        assert_eq!(program_images_update(&current, &listing, false), None);
        assert_eq!(
            program_images_update(&current, &listing, true),
            Some(vec![primary.clone(), thumb.clone()])
        );
        // A moved URL replaces the image of its type.
        let moved = ProgramImages {
            backdrop: Some("http://img/b.jpg".to_owned()),
            ..listing
        };
        assert_eq!(
            program_images_update(&[primary.clone(), thumb.clone()], &moved, false),
            Some(vec![
                primary,
                thumb,
                image(ImageType::Backdrop, "http://img/b.jpg")
            ])
        );
    }

    #[test]
    fn sort_name_pads_numeric_channel_numbers_like_upstream() {
        // v12.2 C# string.Format("{0:0000000000.00000}", n) + "-" + name.
        assert_eq!(
            channel_sort_name(Some("1"), "Parity One"),
            "0000000001.00000-Parity One"
        );
        assert_eq!(
            channel_sort_name(Some("8.5"), "Eight"),
            "0000000008.50000-Eight"
        );
        assert_eq!(
            channel_sort_name(Some("123456"), "Big"),
            "0000123456.00000-Big"
        );
        assert_eq!(
            channel_sort_name(Some("4.11"), "Sub"),
            "0000000004.11000-Sub"
        );
        // .NET keeps the sign outside the zero padding, and a number wider
        // than the pattern keeps all its digits.
        assert_eq!(
            channel_sort_name(Some("-1"), "Neg"),
            "-0000000001.00000-Neg"
        );
        assert_eq!(
            channel_sort_name(Some("12345678901"), "Wide"),
            "12345678901.00000-Wide"
        );
        // An unparseable number is used verbatim.
        assert_eq!(channel_sort_name(Some("A1"), "Alpha"), "A1-Alpha");
        assert_eq!(channel_sort_name(Some("inf"), "X"), "inf-X");
        assert_eq!(channel_sort_name(None, "Bare"), "-Bare");
        // The keys order numerically (jellyfin/jellyfin#15399).
        assert!(channel_sort_name(Some("10002"), "") < channel_sort_name(Some("100010"), ""));
        assert!(channel_sort_name(Some("4.11"), "Z") < channel_sort_name(Some("4.12"), "A"));
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
            show_id: None,
            channel_name: String::new(),
            channel_number: None,
            channel_media_kind: "Tv".to_owned(),
        };
        assert_eq!(
            program_tags(&row),
            ["Live", "Premiere", "News", "Repeat", "Series"]
        );
    }

    /// A guide row for the gl-1591 tests: one airing with `genres` and a
    /// listing series id.
    fn guide_row(genres: &str, series: Option<&str>) -> ProgramRow {
        ProgramRow {
            id: "AAAAAAAA-0000-0000-0000-000000000009".to_owned(),
            channel_id: "BBBBBBBB-0000-0000-0000-000000000002".to_owned(),
            start_date: "2026-08-23T18:00:00Z".to_owned(),
            end_date: Some("2026-08-23T19:00:00Z".to_owned()),
            title: "Show".to_owned(),
            episode_title: None,
            overview: None,
            genres: Some(genres.to_owned()),
            production_year: None,
            official_rating: None,
            is_premiere: false,
            is_repeat: false,
            is_movie: false,
            is_series: true,
            is_news: false,
            is_kids: false,
            is_sports: false,
            is_live: false,
            external_id: None,
            external_series_id: series.map(ToOwned::to_owned),
            season_number: None,
            episode_number: None,
            date_created: None,
            show_id: None,
            channel_name: String::new(),
            channel_number: None,
            channel_media_kind: "Tv".to_owned(),
        }
    }

    /// gl-1591: `BaseItemMapper.Map` joins `Genres` after
    /// `Distinct(StringComparer.OrdinalIgnoreCase)`, the first spelling kept.
    #[test]
    fn program_genres_are_stored_once_whatever_their_case() {
        let entity = program_entity(
            &guide_row(r#"["Drama","drama","News","DRAMA"]"#, None),
            parse,
            None,
        );
        assert_eq!(entity.genres.as_deref(), Some("Drama|News"));
    }

    /// gl-1591: `GetProgram` sets `ExternalSeriesId` only from a non-blank
    /// listing id that differs, ignoring case (`GuideManager.cs:540-545`): a
    /// listing without one keeps the stored id, and so does one in another
    /// case.
    #[test]
    fn a_listing_series_id_replaces_the_stored_one_only_when_it_differs() {
        let stored = program_entity(&guide_row("[]", Some("ser-1")), parse, None);
        for (listing, kept) in [
            (None, "ser-1"),
            (Some("  "), "ser-1"),
            (Some("SER-1"), "ser-1"),
            (Some("ser-2"), "ser-2"),
        ] {
            let fresh = program_entity(&guide_row("[]", listing), parse, None);
            let item = program_item_update(&stored, &fresh).unwrap_or_else(|| stored.clone());
            assert_eq!(
                item.external_series_id.as_deref(),
                Some(kept),
                "{listing:?}"
            );
        }
        assert_eq!(
            program_entity(&guide_row("[]", Some(" ")), parse, None).external_series_id,
            None,
            "a new airing stores no blank series id"
        );
    }

    /// gl-1591: only a tag value counts as tags to index, so a channel stored
    /// as Jellyfin stores one without tags (`''`) is not re-indexed on every
    /// guide refresh.
    #[test]
    fn an_empty_tags_column_carries_no_tag() {
        assert!(!carries_tags(None));
        assert!(!carries_tags(Some("")));
        assert!(!carries_tags(Some("|")));
        assert!(!carries_tags(Some(" ")));
        assert!(carries_tags(Some("Kids")));
        assert!(carries_tags(Some("|Kids")));
    }

    /// gl-1591, gl-1503: the channel takes its airings' aggregate; "Kids" is
    /// added once, in any case, beside the editor's own tags, and never
    /// removed.
    #[test]
    fn a_channel_takes_its_airings_aggregate_and_the_kids_tag() {
        let mut kids = guide_row("[]", None);
        kids.is_kids = true;
        kids.episode_number = Some(1);
        let mut movie = guide_row("[]", None);
        movie.is_series = false;
        movie.is_movie = true;
        let aggregate = ChannelAggregate::of(&[kids, movie]);
        assert_eq!(
            aggregate,
            ChannelAggregate {
                is_movie: true,
                is_series: true,
                is_kids: true
            }
        );
        assert_eq!(ChannelAggregate::of(&[]), ChannelAggregate::default());
        // a sub-title alone widens the airing's own `IsSeries`, not the
        // listing's flag the channel ORs (XMLTV); a Schedules Direct series
        // id is the listing's flag
        let subtitled = guide_row("[]", None);
        assert!(subtitled.is_series && !subtitled.listed_as_series());
        assert!(!ChannelAggregate::of(&[subtitled]).is_series);
        assert!(guide_row("[]", Some("SH01234567")).listed_as_series());

        let stored = BaseItemEntity {
            tags: Some("Favourite".to_owned()),
            ..BaseItemEntity::default()
        };
        let updated = channel_aggregate_update(&stored, aggregate).expect("changed");
        assert!(updated.is_movie && updated.is_series);
        assert_eq!(updated.tags.as_deref(), Some("Favourite|Kids"));
        assert_eq!(
            channel_aggregate_update(&updated, aggregate),
            None,
            "unchanged"
        );

        let lower = BaseItemEntity {
            tags: Some("kids".to_owned()),
            ..updated.clone()
        };
        assert_eq!(
            channel_aggregate_update(&lower, aggregate),
            None,
            "not again in another case"
        );
        let quiet =
            channel_aggregate_update(&updated, ChannelAggregate::default()).expect("changed");
        assert!(!quiet.is_movie && !quiet.is_series);
        assert_eq!(
            quiet.tags.as_deref(),
            Some("Favourite|Kids"),
            "never removed"
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
            show_id: Some("a584069bf4e49f1dbb474bfb43129f3c".to_owned()),
            channel_name: "Parity One".to_owned(),
            channel_number: Some("1".to_owned()),
            channel_media_kind: "Tv".to_owned(),
        };
        let view = Uuid::from_u128(0x2b2b_ca16_aacc_8a14_d53a_11bb_829e_afa5);
        let entity = program_entity(&row, parse, Some(view));
        assert_eq!(entity.type_, PROGRAM_TYPE_NAME);
        assert_eq!(entity.run_time_ticks, Some(36_000_000_000));
        assert_eq!(entity.channel_id.as_deref(), Some(row.channel_id.as_str()));
        assert_eq!(entity.parent_id.as_deref(), Some(row.channel_id.as_str()));
        assert_eq!(entity.genres.as_deref(), Some("News"));
        assert_eq!(entity.tags.as_deref(), Some("News"));
        assert_eq!(entity.media_type.as_deref(), Some("Unknown"));
        assert!(!entity.is_folder);
        // The oracle row's shape: the channel is the PARENT, the Live TV view is
        // the TOP parent — the column `scope_to_user_libraries` filters on, so
        // it is what puts an airing in the recursive user universe.
        assert_eq!(
            entity.top_parent_id.as_deref(),
            Some(db_guid(view).as_str())
        );
        assert_eq!(entity.unrated_type.as_deref(), Some("LiveTvProgram"));
        assert!(entity.is_virtual_item);
        assert_eq!(entity.width, Some(0));
        assert_eq!(entity.height, Some(0));
        assert_eq!(
            entity.show_id.as_deref(),
            Some("a584069bf4e49f1dbb474bfb43129f3c")
        );
    }

    #[test]
    fn a_guide_refresh_does_not_revert_an_edited_programme() {
        // `GuideManager.GetProgram` (v10.11.8 GuideManager.cs:473-640) loads the
        // stored `LiveTvProgram` and assigns only the listing's fields onto it,
        // so anything the metadata editor put on the item survives the refresh.
        // Rebuilding from `program_entity` + the full-column `save_items` upsert
        // would revert every one of them on the guide timer.
        let stored = BaseItemEntity {
            id: "AAAAAAAA-0000-0000-0000-000000000001".to_owned(),
            type_: PROGRAM_TYPE_NAME.to_owned(),
            name: Some("News at Six".to_owned()),
            custom_rating: Some("edited".to_owned()),
            is_locked: true,
            forced_sort_name: Some("zzz".to_owned()),
            sort_name: Some("zzz".to_owned()),
            ..BaseItemEntity::default()
        };
        let fresh = BaseItemEntity {
            id: stored.id.clone(),
            type_: PROGRAM_TYPE_NAME.to_owned(),
            name: Some("News at Seven".to_owned()),
            sort_name: Some("news at seven".to_owned()),
            ..BaseItemEntity::default()
        };
        let merged = program_item_update(&stored, &fresh).expect("the name moved");
        assert_eq!(merged.name.as_deref(), Some("News at Seven"));
        assert_eq!(merged.custom_rating.as_deref(), Some("edited"));
        assert!(merged.is_locked);
        // A forced sort key is the user's, not the guide's.
        assert_eq!(merged.sort_name.as_deref(), Some("zzz"));
        // Nothing changed the second time round, so there is nothing to write.
        assert!(program_item_update(&merged, &fresh).is_none());
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
            image_url: None,
        };
        let entity = channel_entity(&row, parse, Some(view));
        assert_eq!(entity.type_, CHANNEL_TYPE_NAME);
        assert_eq!(entity.path, None); // upstream channel items carry no path
        assert_eq!(entity.media_type.as_deref(), Some("Video"));
        assert_eq!(
            entity.sort_name.as_deref(),
            Some("0000000001.00000-Parity One")
        );
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
            image_url: None,
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
            ..row
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
        let mut forced = stored;
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

    const ASC: Option<SortOrder> = Some(SortOrder::Ascending);
    const DESC: Option<SortOrder> = Some(SortOrder::Descending);

    /// The ids of the lineup below, sorted. `e` and `c` share the sort key
    /// "0000000002.00000-alpha". `c` is a favourite, marked played, and
    /// watched three times; `a` was watched last but never marked played (as
    /// playback leaves a channel); `b` has a row with no play; `d` and `e`
    /// have no row.
    fn sorted_lineup(
        sort_by: &[ItemSortBy],
        sort_order: Option<SortOrder>,
        enable_favorite_sorting: bool,
    ) -> Vec<String> {
        let row = |id: &str, number: Option<&str>, name: &str, created: &str| ChannelRow {
            id: id.to_owned(),
            tvg_id: String::new(),
            external_id: String::new(),
            name: name.to_owned(),
            number: number.map(str::to_owned),
            channel_type: "Tv".to_owned(),
            date_created: Some(created.to_owned()),
            image_url: None,
        };
        let mut rows = vec![
            row("e", Some("2.0"), "ALPHA", "2026-01-02"),
            row("a", Some("10"), "bravo", "2026-01-03"),
            row("b", Some("2"), "Charlie", "2026-01-01"),
            row("c", Some("2"), "alpha", "2026-01-02"),
            row("d", None, "Delta", "2026-01-01"),
        ];
        let played = |play_count: i32, last: Option<&str>| ChannelUserDataRow {
            play_count,
            played: last.is_some(),
            last_played_date: last.map(str::to_owned),
            ..ChannelUserDataRow::default()
        };
        let user_data: ChannelUserData = [
            (
                "c".to_owned(),
                ChannelUserDataRow {
                    is_favorite: true,
                    ..played(3, Some("2026-11-01 08:00:00.0000000"))
                },
            ),
            (
                "a".to_owned(),
                ChannelUserDataRow {
                    played: false,
                    ..played(1, Some("2026-11-03 08:00:00.0000000"))
                },
            ),
            ("b".to_owned(), played(0, None)),
        ]
        .into();
        let query = ferrofin_traits::stubs::LiveTvChannelQuery {
            sort_by: sort_by.to_vec(),
            sort_order,
            enable_favorite_sorting,
            ..ferrofin_traits::stubs::LiveTvChannelQuery::default()
        };
        sort_channel_rows(&mut rows, &query, &user_data);
        rows.into_iter().map(|r| r.id).collect()
    }

    /// Every key the lineup sorts on, in both directions, with favourite-first
    /// and the appended `SortName`; ties keep the stored order. A channel
    /// without a row, or without a play, is `NULL` for `DatePlayed` and
    /// `PlayCount` (last descending, first ascending) and unplayed.
    #[rstest::rstest]
    #[case::sort_name(&[], None, false, ["d", "e", "c", "b", "a"])]
    // `ApplyOrder` drops `Default`: the appended ascending `SortName` is left.
    #[case::default_dropped(&[ItemSortBy::Default], DESC, false, ["d", "e", "c", "b", "a"])]
    #[case::sort_name_desc(&[ItemSortBy::SortName], DESC, false, ["a", "b", "e", "c", "d"])]
    #[case::name(&[ItemSortBy::Name], None, false, ["e", "c", "a", "b", "d"])]
    #[case::name_desc(&[ItemSortBy::Name], DESC, false, ["d", "b", "a", "e", "c"])]
    #[case::date_created(&[ItemSortBy::DateCreated], None, false, ["d", "b", "e", "c", "a"])]
    #[case::date_created_desc(&[ItemSortBy::DateCreated], DESC, false, ["a", "e", "c", "d", "b"])]
    #[case::favourites_first(&[], None, true, ["c", "d", "e", "b", "a"])]
    #[case::favourites_then_name(&[ItemSortBy::Name], DESC, true, ["c", "d", "b", "a", "e"])]
    #[case::favourite_key(&[ItemSortBy::IsFavoriteOrLiked], DESC, false, ["c", "d", "e", "b", "a"])]
    #[case::date_played_desc(&[ItemSortBy::DatePlayed], DESC, false, ["a", "c", "d", "e", "b"])]
    #[case::date_played(&[ItemSortBy::DatePlayed], ASC, false, ["d", "e", "b", "c", "a"])]
    // The guide's "Recently watched" order.
    #[case::recently_watched(&[ItemSortBy::DatePlayed], DESC, true, ["c", "a", "d", "e", "b"])]
    #[case::play_count_desc(&[ItemSortBy::PlayCount], DESC, false, ["c", "a", "b", "d", "e"])]
    #[case::play_count(&[ItemSortBy::PlayCount], ASC, false, ["d", "e", "b", "a", "c"])]
    // Only the played flag counts: `a`'s plays leave it unplayed.
    #[case::played_desc(&[ItemSortBy::IsPlayed], DESC, false, ["c", "d", "e", "b", "a"])]
    #[case::played(&[ItemSortBy::IsPlayed], ASC, false, ["d", "e", "b", "a", "c"])]
    #[case::unplayed_desc(&[ItemSortBy::IsUnplayed], DESC, false, ["d", "e", "b", "a", "c"])]
    #[case::unplayed(&[ItemSortBy::IsUnplayed], ASC, false, ["c", "d", "e", "b", "a"])]
    // `AirTime` and the keys `OrderMapper` has no arm for are `SortName`.
    #[case::air_time(&[ItemSortBy::AirTime], DESC, false, ["a", "b", "e", "c", "d"])]
    #[case::unknown_key(&[ItemSortBy::Unrecognized(99)], DESC, false, ["a", "b", "e", "c", "d"])]
    #[case::aired_episode_order(&[ItemSortBy::AiredEpisodeOrder], None, false, ["d", "e", "c", "b", "a"])]
    // A key over a column no channel item fills ties.
    #[case::runtime(&[ItemSortBy::Runtime], DESC, false, ["d", "e", "c", "b", "a"])]
    // `SeriesDatePlayed` leaves `SortName` ascending alone: favourites and
    // the other keys are dropped.
    #[case::series_date_played(
        &[ItemSortBy::PlayCount, ItemSortBy::SeriesDatePlayed],
        DESC,
        true,
        ["d", "e", "c", "b", "a"]
    )]
    fn the_lineup_sorts_on_each_requested_key(
        #[case] sort_by: &[ItemSortBy],
        #[case] sort_order: Option<SortOrder>,
        #[case] enable_favorite_sorting: bool,
        #[case] expected: [&str; 5],
    ) {
        assert_eq!(
            sorted_lineup(sort_by, sort_order, enable_favorite_sorting),
            expected
        );
    }

    /// `Random` below the favourites: `c` leads, and the rest is a fresh
    /// order on each call.
    #[test]
    fn a_random_lineup_is_a_fresh_order_each_time() {
        let draws: std::collections::HashSet<Vec<String>> = (0..64)
            .map(|_| sorted_lineup(&[ItemSortBy::Random], None, true))
            .collect();
        assert!(draws.len() > 1, "{draws:?}");
        for draw in &draws {
            assert_eq!(draw[0], "c", "{draw:?}");
            let mut ids = draw.clone();
            ids.sort();
            assert_eq!(ids, ["a", "b", "c", "d", "e"]);
        }
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
