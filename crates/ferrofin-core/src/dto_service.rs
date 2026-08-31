//! [`FerrofinDtoService`] — the concrete [`DtoService`] (entity → `BaseItemDto`).
//!
//! Port of `Emby.Server.Implementations.Dto.DtoService`. This is the presentation
//! seam: it turns a persisted [`BaseItemEntity`] row into the wire-shaped
//! [`BaseItemDto`] the API returns, honoring the field/image toggles carried by
//! [`DtoOptions`].
//!
//! ## Port shape
//!
//! The C# `DtoService` walks a live `BaseItem` domain object whose subclasses
//! (`Video`/`Episode`/`Season`/`Series`/`Audio`/`Photo`/…) expose typed
//! properties. Ferrofin has no such object graph — a DTO is built from a flat
//! [`BaseItemEntity`] row plus the row's [`BaseItemKind`] (recovered from the
//! stored `Type` name via [`kind_from_type_name`]). The many `item is Foo`
//! type-tests therefore become `match`es on the kind, and the multi-value
//! columns (`Genres`/`Studios`/`Artists`/`AlbumArtists`/`Tags`/
//! `ProductionLocations`) are the row's pipe-delimited strings rather than
//! navigation collections.
//!
//! ## Injected siblings (composition root, Wave 8)
//!
//! Every collaborator the C# constructor takes is an `Arc<dyn Trait>` here:
//! [`LibraryManager`] (people/artist/name-id lookups + name-item counts),
//! [`UserDataManager`] (play-state), [`ItemCountService`] (child counts),
//! [`ImageProcessor`] (cache tags + blurhashes), [`MediaSourceManager`] (media
//! sources/streams), [`ChapterManager`] and [`TrickplayManager`]. The
//! "Links" row (`ExternalUrls`) is built in-crate from the page's already
//! batched provider ids — see [`ferrofin_providers::external_urls`]. The
//! `server_id` string the C# code reads
//! from `IApplicationHost.SystemId` is supplied at construction (the app host is
//! not part of this seam). Item images (`BaseItemImageInfos`) have no repository
//! trait, so they are read directly through the injected [`Database`] handle,
//! exactly as the sibling managers read `ferrofin-db` for data with no repository
//! surface.
//!
//! ## Deferred (noted, faithful stubs)
//!
//! LiveTV program/channel enrichment (`AddInfoToProgramDto`/`AddChannelInfo`)
//! and active-recording rewrites depend on the `ILiveTvManager`/
//! `IRecordingsManager` seams, which are not injected into this unit; those
//! branches are skipped and flagged. `CanDelete`/`CanDownload` collapse to thin
//! defaults (the C# logic needs the domain tree). Everything else — the
//! full field/image/user-data/people/media-source/chapter/trickplay mapping —
//! is ported.

// The DTO assembly copies dozens of scalar/collection fields straight from the
// item row onto a fresh, `Option`-valued DTO field (`dto.name =
// item.name.clone()`). `clone_from` cannot help there — the target is a distinct
// `Option` being set for the first time — and rewriting each as
// `dto.name.clone_from(&item.name)` reads worse across the mapping, so the lint
// is allowed for this module.
#![allow(clippy::assigning_clones)]

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::{BaseItemEntity, BaseItemImageInfoEntity};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::{BaseItemKind, CollectionType, MediaType};
use ferrofin_model::dto::{
    BaseItemDto, BaseItemPerson, ItemCounts, NameGuidPair, TrickplayInfoDto, UserItemDataDto,
};
use ferrofin_model::entities::{ExtraType, ImageType, LocationType, VideoType};
use ferrofin_model::querying::ItemFields;
use uuid::Uuid;

use ferrofin_traits::chapters::ChapterManager;
use ferrofin_traits::drawing::ImageProcessor;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, MediaSourceManager, UserDataManager};
use ferrofin_traits::options::{DtoOptions, ItemImageInfo};
use ferrofin_traits::persistence::{ItemCountService, NameItemRow};
use ferrofin_traits::trickplay::TrickplayManager;

use crate::db_error::db_err;
use crate::item_type_lookup::kind_from_type_name;

/// Relation rows bulk-loaded for a whole page of items, so `build_dto` needs
/// no per-item queries for them (list endpoints); absent entries mean "no rows"
/// for that item, not "not prefetched".
#[derive(Default)]
struct Prefetched {
    /// Image rows per item id (same order as [`FerrofinDtoService::load_images`]).
    images: HashMap<Uuid, Vec<ItemImageInfo>>,
    /// The requesting user's play-state per item id.
    user_data: HashMap<Uuid, UserItemDataDto>,
    /// Media streams per item id (populated when EITHER the `MediaStreams` or
    /// the `MediaSources` field is requested), so a page builds them in one
    /// query instead of N. Read three times — see [`take_or_clone`].
    media_streams: HashMap<Uuid, Vec<ferrofin_model::entities_media::MediaStream>>,
    /// Media attachments per item id (when `MediaSources` is requested).
    media_attachments: HashMap<Uuid, Vec<ferrofin_model::entities_media::MediaAttachment>>,
    /// Provider-id maps per item id (populated when EITHER the `ProviderIds`
    /// or the `ExternalUrls` field is requested — the "Links" row is built from
    /// the same ids, so one batch read serves both).
    provider_ids: HashMap<Uuid, HashMap<String, String>>,
    /// `DisplayOrder` per *series* id, for the seasons/episodes on the page:
    /// TMDB emits a season/episode link only for a series in aired order, and
    /// the value lives in the SERIES' `Data` blob, not the child's.
    series_display_order: HashMap<Uuid, String>,
    /// Album name per *PhotoAlbum* id, for the photos on the page: a photo's
    /// `Album`/`AlbumId` come from its parent album (C# `Photo.AlbumEntity`),
    /// and one batch read serves the whole page.
    photo_album_names: HashMap<Uuid, String>,
    /// Provider-id maps per *series* id, for the seasons/episodes on the page:
    /// their IMDb/TMDB links are built from the owning series' id, not their
    /// own (C# `ImdbExternalUrlProvider`/`TmdbExternalUrlProvider`).
    series_provider_ids: HashMap<Uuid, HashMap<String, String>>,
    /// The page's seasons'/episodes' SERIES rows, keyed by series id, mapped to
    /// the series' pipe-joined `Studios` column — the source of `SeriesStudio`,
    /// which is `series.Studios.FirstOrDefault()` and therefore lives on a row
    /// the projected item is not.
    series_studios: HashMap<Uuid, String>,
    /// Credited people per item id (populated only when the `People` field is
    /// requested), so a page's cast/crew loads in one query.
    people: HashMap<Uuid, Vec<ferrofin_db::entities::base_items::PeopleEntity>>,
    /// Image rows per *person* id, for the whole page's cast/crew at once, so the
    /// primary-image tag lookup does not re-query per person per item.
    person_images: HashMap<Uuid, Vec<ItemImageInfo>>,
    /// `ItemValues` id per clean value, bucketed by value type, for every
    /// studio/genre/artist name across the page, so `attach_studios`/`_genres`/
    /// `_artists` resolve from memory instead of a query per name. Nested (not
    /// keyed by a `(i32, String)` tuple) so a lookup borrows the clean `&str`
    /// instead of allocating a key per name per item.
    value_ids: HashMap<i32, HashMap<String, Uuid>>,
    /// The clean value ([`crate::text_util::get_clean_value`]) of every distinct
    /// studio/genre/artist name on the page, computed once by the prefetch that
    /// already had to compute it to build `value_ids`. `Prefetched::value_id`
    /// reads it instead of re-cleaning the same name once per item.
    clean_values: HashMap<String, String>,
    /// Chapters per item id (populated only when the `Chapters` field is requested).
    chapters: HashMap<Uuid, Vec<ferrofin_model::entities_media::ChapterInfo>>,
    /// Trickplay manifest per item id (populated only when the `Trickplay` field
    /// is requested).
    trickplay: HashMap<
        Uuid,
        HashMap<String, HashMap<i32, ferrofin_db::entities::playback::TrickplayInfoEntity>>,
    >,
    /// Direct-child counts per folder item id (populated only when the
    /// `ChildCount` field is requested and a user is present).
    child_counts: HashMap<Uuid, i32>,
    /// Linked-children counts (`Folder.LinkedChildren.Length`) for the page's
    /// `MusicAlbum`/`Season`/`Playlist` rows, populated only when a user is
    /// present. Backs the second half of upstream's ChildCount shortcut, which
    /// runs with no `ItemFields` gate — so it cannot ride on `child_counts`,
    /// which is field-gated.
    linked_child_counts: HashMap<Uuid, i32>,
    /// Played/total leaf-descendant counts per folder item id (populated when
    /// user data is enabled and a user is present), for folder `UnplayedItemCount`.
    played_counts: HashMap<Uuid, ferrofin_traits::persistence::PlayedAndTotal>,
    /// Merged alternate-version rows per primary item id (populated only when
    /// the `MediaSources` field is requested), so a merged item reports its
    /// extra selectable sources without a per-item query.
    alternates: HashMap<Uuid, Vec<BaseItemEntity>>,
    /// The page's video item ids that carry a subtitle stream. Backs the
    /// unconditional `HasSubtitles` on video DTOs (C# emits it outside the
    /// `ItemFields` system) via one ids-only query per page.
    has_subtitles: std::collections::HashSet<Uuid>,
    /// The page's audio items that carry a lyric stream, for `HasLyrics` — which
    /// C# emits on every `Audio` DTO outside the `ItemFields` system, so it must
    /// be `false` and not absent when there is none.
    has_lyrics: std::collections::HashSet<Uuid>,
    /// The requesting user's content permissions (populated only when the
    /// `CanDelete`/`CanDownload` fields are requested and a user is present),
    /// so the whole page gates on one `Permissions` query.
    content_permissions: Option<UserContentPermissions>,
    /// The per-NAME `Person` item id for each credited name on the page, so
    /// `People[].Id` points at the favoritable by-name item. Keyed by the credit
    /// row's name *as stored*: the prefetch resolves names case-insensitively
    /// and then registers the resolved id under every raw spelling it saw, so
    /// `attach_people` looks up by `&str` instead of lowercasing per credit.
    person_ids_by_name: HashMap<String, Uuid>,
    /// Ids that some item on the page lists as a merged alternate version.
    /// Their `media_streams` entry is read again while projecting that OTHER
    /// item, so it must survive its own item's projection — see
    /// [`FerrofinDtoService::attach_basic_fields`]'s `MediaStreams` read.
    alt_referenced: std::collections::HashSet<Uuid>,
}

/// The delete/download half of a user's policy (C# `HasPermission` over
/// `EnableContentDeletion` / `EnableContentDownloading`).
#[derive(Debug, Clone, Copy)]
struct UserContentPermissions {
    /// `PermissionKind::EnableContentDeletion` (10).
    can_delete: bool,
    /// `PermissionKind::EnableContentDownloading` (11).
    can_download: bool,
}

/// Takes an item's prefetched entry instead of cloning it, when this is the
/// item's only occurrence on the page.
///
/// A prefetched map is built once per page and read once per item, so the
/// clone the read used to make was pure waste — the map is dropped right after
/// the page is projected. The exception is a page that repeats an item (a
/// playlist may legitimately list the same track twice, and `/Items?ids=` can
/// be handed the same id twice): there the entry is read once per occurrence,
/// so a repeated id keeps cloning and only unique ids move.
///
/// **Only safe where no reader of that id remains after this point.** For most
/// maps that is trivially true — they have exactly one read site, keyed by the
/// item's own id. `media_streams` is the exception and needs care: it is read
/// three times (the `MediaSources` block, once more there per merged alternate
/// keyed by the *alternate's* id, and the `MediaStreams` field). It may be
/// drained ONLY at the last of those, the `MediaStreams` field, and only when
/// `repeated` also folds in `Prefetched::alt_referenced` — otherwise a page item
/// that lists this id as its alternate is projected later and finds the entry
/// gone. Do not drain it at the `MediaSources` block: that read comes first, and
/// doing so empties `MediaStreams` on every `/Items/{id}`.
fn take_or_clone<V: Clone>(map: &mut HashMap<Uuid, V>, id: &Uuid, repeated: bool) -> Option<V> {
    if repeated {
        map.get(id).cloned()
    } else {
        map.remove(id)
    }
}

impl Prefetched {
    /// The clean lookup key for a display name, reusing the value the prefetch
    /// already computed for it.
    ///
    /// Same key as `get_clean_value(name)` in every case: the cache is filled
    /// from that exact function, and a name the prefetch never saw (an unpopulated
    /// `Prefetched`, or a name reached outside the collected fields) falls back
    /// to computing it. Never normalize differently here — the clean value is the
    /// join key against the stored `ItemValues.CleanValue` column, so a divergence
    /// silently empties Genres/Studios/Artists.
    fn clean_key<'a>(&'a self, name: &str) -> std::borrow::Cow<'a, str> {
        self.clean_values.get(name).map_or_else(
            || std::borrow::Cow::Owned(crate::text_util::get_clean_value(name)),
            |clean| std::borrow::Cow::Borrowed(clean.as_str()),
        )
    }

    /// The `ItemValues` id stored for an already-cleaned value under `value_type`,
    /// or `None` when the page's prefetch found no such row.
    fn lookup_clean(&self, value_type: i32, clean: &str) -> Option<Uuid> {
        self.value_ids.get(&value_type)?.get(clean).copied()
    }

    /// [`Self::lookup_clean`] with the nil id for a missing row — exactly what the
    /// per-name lookup resolved a missing row to.
    fn value_id_clean(&self, value_type: i32, clean: &str) -> Uuid {
        self.lookup_clean(value_type, clean)
            .unwrap_or_else(Uuid::nil)
    }

    /// A studio/genre/artist id from the prefetched `ItemValues` map — the nil
    /// id when the name has no stored value row, exactly as the per-name lookup
    /// resolved a missing row.
    fn value_id(&self, value_type: i32, name: &str) -> Uuid {
        self.value_id_clean(value_type, &self.clean_key(name))
    }
}
use crate::kinds;

/// The `ImageType` discriminants that the C# `ItemImageInfo` marks as "allows
/// multiple" (backdrops/chapters/screenshots) — the single-image loop skips
/// these so they are handled by their own limited fetch.
///
/// Mirrors `BaseItem.AllowsMultipleImages`.
fn allows_multiple_images(image_type: ImageType) -> bool {
    matches!(
        image_type,
        ImageType::Backdrop | ImageType::Screenshot | ImageType::Chapter
    )
}

/// Reads an [`ImageType`] from its stored `BaseItemImageInfos.ImageType`
/// discriminant (0-based, matching the C# `ImageType` declaration order).
///
/// A stored row should never carry an out-of-range value; an unknown one maps to
/// [`ImageType::Primary`] rather than failing the whole projection.
fn image_type_from_disc(disc: i32) -> ImageType {
    match disc {
        1 => ImageType::Art,
        2 => ImageType::Backdrop,
        3 => ImageType::Banner,
        4 => ImageType::Logo,
        5 => ImageType::Thumb,
        6 => ImageType::Disc,
        7 => ImageType::Box,
        8 => ImageType::Screenshot,
        9 => ImageType::Menu,
        10 => ImageType::Chapter,
        11 => ImageType::BoxRear,
        12 => ImageType::Profile,
        _ => ImageType::Primary,
    }
}

/// Maps a stored `BaseItemImageInfos` row onto the in-flight [`ItemImageInfo`]
/// the image processor and tag helpers consume.
fn to_image_info(row: &BaseItemImageInfoEntity) -> ItemImageInfo {
    ItemImageInfo {
        path: row.path.clone(),
        image_type: image_type_from_disc(row.image_type),
        date_modified: row.date_modified.unwrap_or_default(),
        width: i32::try_from(row.width).unwrap_or(0),
        height: i32::try_from(row.height).unwrap_or(0),
        blur_hash: row
            .blurhash
            .as_ref()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned()),
    }
}

/// Parses a [`UserEntity`]'s stored `Guid` id into a [`Uuid`].
///
/// The id is a lookup key for this user's `UserData`/permissions. Degrading a
/// malformed one to the nil GUID would scope those reads to a user that does not
/// exist and return a `200` carrying *someone else's* answer — no favourites, no
/// resume position, `CanDelete: false` — so a corrupt row is an error instead.
fn parse_user_id(id: &str) -> Result<Uuid, ServiceError> {
    Uuid::parse_str(id)
        .map_err(|_| ServiceError::Backend("stored user id is not a guid".to_owned()))
}

/// Parses the row's stored `Guid` id into a [`Uuid`], or the nil UUID on a
/// malformed value.
///
/// The fallback is unreachable: `BaseItems.Id` is the table's primary key, and
/// every writer — Ferrofin's `guid_to_db` and Jellyfin's EF `Guid` conversion on
/// an adopted database — emits canonical hyphenated `Guid` text. It stays
/// infallible because this is the hottest call in the DTO projection (it also
/// keys the prefetch maps, so a fallible signature would have to decide "skip
/// the row" versus "fail the page" at fifteen call sites) — see the
/// `parse_user_id` sibling for the cases where the fallback *was* observable.
fn row_id(item: &BaseItemEntity) -> Uuid {
    Uuid::parse_str(&item.id).unwrap_or_else(|_| Uuid::nil())
}

/// C# `BaseItem.GetEtag`: MD5 (Guid byte layout, `"N"` format) of the pipe-joined
/// etag values — the base list is just `DateLastSaved.Ticks` (100 ns units since
/// 0001-01-01; a never-saved row matches C#'s `DateTime.MinValue` = 0 ticks).
fn compute_etag(date_last_saved: Option<chrono::DateTime<chrono::Utc>>) -> String {
    const UNIX_EPOCH_TICKS: i64 = 621_355_968_000_000_000;
    let ticks = date_last_saved.map_or(0, |d| {
        UNIX_EPOCH_TICKS + d.timestamp() * 10_000_000 + i64::from(d.timestamp_subsec_nanos() / 100)
    });
    ferrofin_common::extensions::get_md5(&ticks.to_string())
        .simple()
        .to_string()
}

/// Copies a photo's EXIF fields out of the row's `Data` blob onto the DTO —
/// the read side of the scan's `Emby.Photos.PhotoProvider` port.
///
/// C# serializes the whole `Photo` object into `Data` and its `DtoService`
/// copies each property across; Ferrofin stores only the EXIF keys there (under
/// the same names) and reads them back here. Ungated by `ItemFields`, as the
/// C# assignments are.
fn attach_photo_exif(dto: &mut BaseItemDto, item: &BaseItemEntity) {
    use crate::item_data::{read_data_f64, read_data_i32, read_data_string};

    if dto.type_ != BaseItemKind::Photo {
        return;
    }
    let data = crate::item_data::parse_data(item.data.as_deref());
    if data.is_empty() {
        return;
    }
    dto.camera_make = read_data_string(&data, "CameraMake");
    dto.camera_model = read_data_string(&data, "CameraModel");
    dto.software = read_data_string(&data, "Software");
    dto.exposure_time = read_data_f64(&data, "ExposureTime");
    dto.focal_length = read_data_f64(&data, "FocalLength");
    dto.aperture = read_data_f64(&data, "Aperture");
    dto.shutter_speed = read_data_f64(&data, "ShutterSpeed");
    dto.latitude = read_data_f64(&data, "Latitude");
    dto.longitude = read_data_f64(&data, "Longitude");
    dto.altitude = read_data_f64(&data, "Altitude");
    dto.iso_speed_rating = read_data_i32(&data, "IsoSpeedRating");
    dto.image_orientation = read_data_string(&data, "Orientation")
        .as_deref()
        .and_then(image_orientation_from_name);
}

/// Sets a photo's `Album`/`AlbumId` from its parent album — C#
/// `SetPhotoProperties`, which reads `Photo.AlbumEntity` and leaves both unset
/// when the photo has no album.
fn attach_photo_album(dto: &mut BaseItemDto, item: &BaseItemEntity, names: &HashMap<Uuid, String>) {
    if dto.type_ != BaseItemKind::Photo {
        return;
    }
    let Some(album_id) = item
        .parent_id
        .as_deref()
        .and_then(|id| Uuid::parse_str(id).ok())
    else {
        return;
    };
    if let Some(name) = names.get(&album_id) {
        dto.album = Some(name.clone());
        dto.album_id = Some(album_id);
    }
}

/// The `ImageOrientation` whose name matches `value` (the `Data` blob stores
/// the enum as its C# name, e.g. `"RightTop"`).
fn image_orientation_from_name(value: &str) -> Option<ferrofin_model::drawing::ImageOrientation> {
    use ferrofin_model::drawing::ImageOrientation as O;
    // Spelled out rather than derived from `Debug`: this is the wire contract,
    // and a `#[derive(Debug)]` on the model enum must not be able to silently
    // change what a client sees. It also allocates nothing per photo.
    Some(match value {
        v if v.eq_ignore_ascii_case("TopLeft") => O::TopLeft,
        v if v.eq_ignore_ascii_case("TopRight") => O::TopRight,
        v if v.eq_ignore_ascii_case("BottomRight") => O::BottomRight,
        v if v.eq_ignore_ascii_case("BottomLeft") => O::BottomLeft,
        v if v.eq_ignore_ascii_case("LeftTop") => O::LeftTop,
        v if v.eq_ignore_ascii_case("RightTop") => O::RightTop,
        v if v.eq_ignore_ascii_case("RightBottom") => O::RightBottom,
        v if v.eq_ignore_ascii_case("LeftBottom") => O::LeftBottom,
        _ => return None,
    })
}

/// The [`BaseItemKind`] of a row, defaulting to [`BaseItemKind::Folder`] for an
/// unrecognized stored `Type` (the conservative default used across the crate).
fn row_kind(item: &BaseItemEntity) -> BaseItemKind {
    kind_from_type_name(&item.type_).unwrap_or(BaseItemKind::Folder)
}

/// Whether this kind is a Live TV channel (C# `item is LiveTvChannel`). Both
/// spellings appear because `kind_from_type_name` maps the stored type name to
/// `LiveTvChannel` while callers may hand a `TvChannel` row directly.
fn is_live_tv_channel(kind: BaseItemKind) -> bool {
    matches!(kind, BaseItemKind::LiveTvChannel | BaseItemKind::TvChannel)
}

/// Whether this kind is a Live TV programme (C# `item is LiveTvProgram`).
fn is_live_tv_program(kind: BaseItemKind) -> bool {
    matches!(
        kind,
        BaseItemKind::LiveTvProgram | BaseItemKind::TvProgram | BaseItemKind::Program
    )
}

/// The kind a client sees as the DTO's `Type` — C# `GetClientTypeName`, which
/// `LiveTvChannel`/`LiveTvProgram` override to `"TvChannel"`/`"Program"` and
/// `PlaylistsFolder` overrides to `"ManualPlaylistsFolder"`. Every other kind
/// passes through.
///
/// The playlists arm is why 10.11.8 ships no `ManualPlaylistsFolder` *class*
/// while every client sees that `Type`: the row is stored as
/// `Emby.Server.Implementations.Playlists.PlaylistsFolder` and only renamed on
/// the way out (`PlaylistsFolder.GetClientTypeName()`, v10.11.8
/// `Emby.Server.Implementations/Playlists/PlaylistsFolder.cs:50`).
fn client_kind(kind: BaseItemKind) -> BaseItemKind {
    match kind {
        BaseItemKind::LiveTvChannel => BaseItemKind::TvChannel,
        BaseItemKind::LiveTvProgram => BaseItemKind::Program,
        BaseItemKind::PlaylistsFolder => BaseItemKind::ManualPlaylistsFolder,
        other => other,
    }
}

/// Whether this row enters the C# `AttachUserSpecificInfo` folder branch — the
/// *runtime* `BaseItem.IsFolder`, not just the stored column. Pure by-name kinds
/// (`Genre`/`MusicGenre`/`Studio`/`Person`/`Year`) are `BaseItem` subclasses in
/// C#, never folders, whatever the stored flag says (Ferrofin materializes their
/// rows with `IsFolder = 1`). `MusicArtist` is the one by-name kind that *is* a
/// C# `Folder`, but overrides `IsFolder => !IsAccessedByName`, and
/// `IsAccessedByName => ParentId.IsEmpty()` — so only a physically-parented
/// artist folder counts as a folder here.
fn folder_emits_counts(item: &BaseItemEntity) -> bool {
    if !item.is_folder {
        return false;
    }
    match row_kind(item) {
        BaseItemKind::Genre
        | BaseItemKind::MusicGenre
        | BaseItemKind::Studio
        | BaseItemKind::Person
        | BaseItemKind::Year => false,
        BaseItemKind::MusicArtist => item
            .parent_id
            .as_deref()
            .and_then(|p| Uuid::parse_str(p).ok())
            .is_some_and(|p| !p.is_nil()),
        _ => true,
    }
}

/// `100 × position ÷ runtime` as the C# double division (both counts are tick
/// magnitudes well inside `f64`'s 2^53 integer range, so the casts are exact
/// enough for a display percentage).
#[allow(clippy::cast_precision_loss)]
fn percent_of_ticks(position: i64, runtime: i64) -> f64 {
    100.0 * position as f64 / runtime as f64
}

/// An empty (never-played) [`UserItemDataDto`] for `item_id` — the shape
/// `UserDataManager` returns for an item with no stored row, used when a folder
/// needs a UserData object solely to carry `UnplayedItemCount`.
fn empty_user_data_dto(item_id: Uuid) -> UserItemDataDto {
    UserItemDataDto {
        rating: None,
        played_percentage: None,
        unplayed_item_count: None,
        playback_position_ticks: 0,
        play_count: 0,
        is_favorite: false,
        likes: None,
        last_played_date: None,
        played: false,
        key: item_id.to_string(),
        item_id,
    }
}

/// Sets `ChildCount` on a folder DTO from the prefetched per-parent counts.
///
/// Port of the `AttachUserSpecificInfo` ChildCount attach + `GetChildCount`:
/// only folders get a count, and an already-set value is kept (`??=`).
/// `ICollectionFolder` and `UserView` rows skip the count: C# returns
/// `Random.Shared.Next(1, 10)` there (DtoService.cs:649-656, "too slow to
/// calculate for top level folders on a per-user basis — just return something
/// so that apps that are expecting a value won't think the folders are empty");
/// an id-derived 1..=9 honors the same contract (nonzero, meaningless) without a
/// rand dependency. `ICollectionFolder` covers `BasePluginFolder`
/// (BasePluginFolder.cs:12) too, hence the playlists folder.
fn attach_child_count(dto: &mut BaseItemDto, item: &BaseItemEntity, counts: &HashMap<Uuid, i32>) {
    if dto.child_count.is_some() || !item.is_folder {
        return;
    }
    let id = row_id(item);
    dto.child_count = Some(match row_kind(item) {
        BaseItemKind::CollectionFolder
        | BaseItemKind::UserView
        | BaseItemKind::BasePluginFolder
        | BaseItemKind::ManualPlaylistsFolder
        | BaseItemKind::PlaylistsFolder => i32::from(id.as_bytes()[15] % 9) + 1,
        _ => counts.get(&id).copied().unwrap_or(0),
    });
}

/// The `CollectionType` of an `IHasCollectionType` row (`DtoService`
/// AttachBasicFields, DtoService.cs:1061-1064), or `None` for every other kind.
///
/// The three implementors Ferrofin models:
/// - `CollectionFolder` (CollectionFolder.cs:32) — the library's own type,
///   persisted in the row's `Data` blob as `CollectionType`, the same place
///   `ItemRepository::collection_type_of` and a real Jellyfin database keep it;
/// - `UserView` (UserView.cs:58, `CollectionType => ViewType`) — its `ViewType`,
///   persisted in `Data` as `ViewType`;
/// - `PlaylistsFolder`/`ManualPlaylistsFolder` (PlaylistsFolder.cs:29) — the
///   constant `playlists`, a property on the type with nothing stored.
///
/// An unparseable or absent value leaves the field unset, exactly as a C# null.
fn collection_type_of(item: &BaseItemEntity, kind: BaseItemKind) -> Option<CollectionType> {
    let key = match kind {
        BaseItemKind::ManualPlaylistsFolder | BaseItemKind::PlaylistsFolder => {
            return Some(CollectionType::playlists);
        }
        BaseItemKind::CollectionFolder => "CollectionType",
        BaseItemKind::UserView => "ViewType",
        _ => return None,
    };
    let parsed: serde_json::Value = serde_json::from_str(item.data.as_deref()?).ok()?;
    let name = parsed.get(key)?.as_str()?.to_ascii_lowercase();
    serde_json::from_value(serde_json::Value::String(name)).ok()
}

/// Splits a stored pipe-delimited multi-value column into a list, dropping
/// empties. Jellyfin joins `Genres`/`Studios`/`Artists`/… with `|`.
fn split_multi(stored: Option<&str>) -> Vec<String> {
    split_multi_str(stored).map(str::to_owned).collect()
}

/// [`split_multi`] without the allocation: the same segments, borrowed from the
/// stored column. Used where the names are only read (the page's value-id
/// prefetch), which is most of the calls — a 50-item page splits four columns
/// per item and threw away a `String` per segment.
fn split_multi_str(stored: Option<&str>) -> impl Iterator<Item = &str> {
    stored
        .into_iter()
        .flat_map(|s| s.split('|').filter(|p| !p.is_empty()))
}

/// Parses a stored `MediaType` string into the enum, defaulting to
/// [`MediaType::Unknown`].
fn parse_media_type(stored: Option<&str>) -> MediaType {
    match stored {
        Some("Video") => MediaType::Video,
        Some("Audio") => MediaType::Audio,
        Some("Photo") => MediaType::Photo,
        Some("Book") => MediaType::Book,
        _ => MediaType::Unknown,
    }
}

/// Maps a stored `ExtraType` discriminant onto the enum, or `None` for a value
/// with no corresponding extra type (the `0`/`Unknown` sentinel and any
/// out-of-range discriminant).
fn extra_type_from_disc(disc: i32) -> Option<ExtraType> {
    Some(match disc {
        1 => ExtraType::Clip,
        2 => ExtraType::Trailer,
        3 => ExtraType::BehindTheScenes,
        4 => ExtraType::DeletedScene,
        5 => ExtraType::Interview,
        6 => ExtraType::Scene,
        7 => ExtraType::Sample,
        8 => ExtraType::ThemeSong,
        9 => ExtraType::ThemeVideo,
        10 => ExtraType::Featurette,
        11 => ExtraType::Short,
        _ => return None,
    })
}

/// The concrete DTO-projection service.
#[derive(Clone)]
pub struct FerrofinDtoService {
    db: Database,
    server_id: String,
    library: Arc<dyn LibraryManager>,
    user_data: Arc<dyn UserDataManager>,
    item_counts: Arc<dyn ItemCountService>,
    image_processor: Arc<dyn ImageProcessor>,
    media_sources: Arc<dyn MediaSourceManager>,
    chapters: Arc<dyn ChapterManager>,
    trickplay: Arc<dyn TrickplayManager>,
    /// The MusicBrainz root the "Links" row points music items at — the
    /// configured mirror, as C# uses `Plugin.Instance.Configuration.Server`.
    musicbrainz_server: String,
    /// The Live TV manager that finishes a channel's DTO.
    ///
    /// Upstream's `DtoService` holds `Lazy<ILiveTvManager> LivetvManager` for
    /// exactly this and for exactly this reason: the Live TV manager needs the
    /// DTO service, so one of the two references has to be resolved late.
    /// `Arc<OnceLock<_>>` rather than a plain `OnceLock` because this service
    /// is `Clone` and the composition root wires the seam after the clones
    /// exist — every copy must see the value once it lands.
    live_tv: Arc<OnceLock<Arc<dyn ferrofin_traits::stubs::LiveTvManager>>>,
}

impl std::fmt::Debug for FerrofinDtoService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinDtoService")
            .field("server_id", &self.server_id)
            .finish_non_exhaustive()
    }
}

impl FerrofinDtoService {
    /// Creates the DTO service over its database handle and injected siblings.
    ///
    /// `server_id` is the app host's `SystemId` (stamped onto every DTO's
    /// `ServerId`); the composition root supplies it.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Database,
        server_id: String,
        library: Arc<dyn LibraryManager>,
        user_data: Arc<dyn UserDataManager>,
        item_counts: Arc<dyn ItemCountService>,
        image_processor: Arc<dyn ImageProcessor>,
        media_sources: Arc<dyn MediaSourceManager>,
        chapters: Arc<dyn ChapterManager>,
        trickplay: Arc<dyn TrickplayManager>,
    ) -> Self {
        Self {
            db,
            server_id,
            library,
            user_data,
            item_counts,
            image_processor,
            media_sources,
            chapters,
            trickplay,
            musicbrainz_server: ferrofin_providers::musicbrainz::DEFAULT_BASE_URL.to_owned(),
            live_tv: Arc::new(OnceLock::new()),
        }
    }

    /// Attaches the Live TV manager whose `AddChannelInfo` finishes a channel
    /// DTO (C# `DtoService.LivetvManager`). A second call is ignored.
    ///
    /// Without it a channel projects as an ordinary item: no `Number`, no
    /// `ChannelNumber`, no `ChannelType`, no `CurrentProgram`.
    pub fn set_live_tv(&self, live_tv: Arc<dyn ferrofin_traits::stubs::LiveTvManager>) {
        let _ = self.live_tv.set(live_tv);
    }

    /// Points the music "Links" row at a configured MusicBrainz mirror. Empty
    /// (or unset) keeps the canonical `https://musicbrainz.org`.
    #[must_use]
    pub fn with_musicbrainz_server(mut self, server: &str) -> Self {
        let server = server.trim().trim_end_matches('/');
        if !server.is_empty() {
            self.musicbrainz_server = server.to_owned();
        }
        self
    }

    /// Loads an item's image rows from `BaseItemImageInfos`, ordered by type then
    /// by row id for a stable presentation order.
    async fn load_images(&self, item_id: Uuid) -> Result<Vec<ItemImageInfo>, ServiceError> {
        let rows = sqlx::query_as::<_, BaseItemImageInfoEntity>(
            r#"SELECT * FROM "BaseItemImageInfos"
               WHERE "ItemId" = ?1 ORDER BY "ImageType", "Id""#,
        )
        .bind(guid_to_db(item_id))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;

        Ok(rows.iter().map(to_image_info).collect())
    }

    /// Batch form of [`Self::load_images`]: all image rows for `item_ids` in one
    /// query per chunk, keyed by item id (per-item ordering preserved).
    ///
    /// The per-item form is an N+1 that dominates list-endpoint latency under
    /// concurrent load; list callers prefetch through this instead.
    async fn load_images_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<ItemImageInfo>>, ServiceError> {
        let mut map: HashMap<Uuid, Vec<ItemImageInfo>> = HashMap::with_capacity(item_ids.len());
        for chunk in item_ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT * FROM "BaseItemImageInfos"
                   WHERE "ItemId" IN ({placeholders})
                   ORDER BY "ItemId", "ImageType", "Id""#,
            );
            let mut query = sqlx::query_as::<_, BaseItemImageInfoEntity>(&sql);
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            let rows = query.fetch_all(self.db.pool()).await.map_err(db_err)?;
            for row in &rows {
                if let Ok(item_id) = Uuid::parse_str(&row.item_id) {
                    map.entry(item_id).or_default().push(to_image_info(row));
                }
            }
        }
        Ok(map)
    }

    /// Resolves many `(value type, CLEAN value)` pairs to their `ItemValues` ids
    /// in one query — the page's studios/genres/artists — bucketed by type.
    /// Pairs with no row are simply absent.
    ///
    /// The caller passes values already normalized by
    /// [`crate::text_util::get_clean_value`] (the prefetch computes each name's
    /// clean form once and caches it for the projection); this must stay the
    /// same normalization the stored `ItemValues.CleanValue` column holds.
    ///
    /// Port of the `_libraryManager.GetGenreId`/`GetStudioId`/… helpers, which
    /// hash-map a clean value to a stable id; here the stored `ItemValues` row
    /// already carries that id, so a lookup keyed by `(Type, CleanValue)`
    /// suffices.
    async fn resolve_value_ids(
        &self,
        clean_pairs: &[(i32, String)],
    ) -> Result<HashMap<i32, HashMap<String, Uuid>>, ServiceError> {
        let mut map: HashMap<i32, HashMap<String, Uuid>> = HashMap::new();
        // Dedup the (type, clean) keys we need.
        let mut want: std::collections::HashSet<(i32, String)> = std::collections::HashSet::new();
        for (t, clean) in clean_pairs {
            want.insert((*t, clean.clone()));
        }
        if want.is_empty() {
            return Ok(map);
        }
        let keys: Vec<(i32, String)> = want.into_iter().collect();
        // Two host variables per key.
        for chunk in keys.chunks(ferrofin_db::BATCH_BIND_CHUNK / 2) {
            let ph = (0..chunk.len())
                .map(|_| "(?, ?)")
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                r#"SELECT "Type", "CleanValue", "ItemValueId" FROM "ItemValues"
                   WHERE ("Type", "CleanValue") IN ({ph})"#,
            );
            let mut query = sqlx::query_as::<_, (i32, String, String)>(&sql);
            for (t, clean) in chunk {
                query = query.bind(*t).bind(clean);
            }
            for (t, clean, id) in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                if let Ok(uuid) = Uuid::parse_str(&id) {
                    map.entry(t).or_default().insert(clean, uuid);
                }
            }
        }
        Ok(map)
    }

    /// Computes the primary-image aspect ratio for a set of already-loaded image
    /// rows, or `None` when there is no primary image.
    async fn primary_aspect_ratio(&self, item_id: Uuid, images: &[ItemImageInfo]) -> Option<f64> {
        let primary = images.iter().find(|i| i.image_type == ImageType::Primary)?;
        if !primary.is_local_file() {
            // Remote images have no measurable local dimensions; the C# default
            // (a domain-tree computation) is not available here.
            return None;
        }
        match self
            .image_processor
            .get_item_image_dimensions(item_id, primary)
            .await
        {
            Ok(dim) if dim.width > 0 && dim.height > 0 => {
                Some(f64::from(dim.width) / f64::from(dim.height))
            }
            _ => None,
        }
    }

    /// Computes the cache tag for one image, tolerating processor failures
    /// (logged-and-skipped in C#).
    async fn image_tag(&self, item_id: Uuid, image: &ItemImageInfo) -> Option<String> {
        self.image_processor
            .get_image_cache_tag(item_id, image)
            .await
            .ok()
            .flatten()
    }

    /// Records an image's blurhash under its tag on the DTO's blurhash map.
    fn record_blur_hash(dto: &mut BaseItemDto, image_type: ImageType, tag: &str, hash: &str) {
        dto.image_blur_hashes
            .get_or_insert_with(HashMap::new)
            .entry(image_type)
            .or_default()
            .insert(tag.to_owned(), hash.to_owned());
    }

    /// Computes an image's cache tag and, when present, records its blurhash —
    /// the port of C# `GetTagAndFillBlurhash`.
    async fn tag_and_fill_blur_hash(
        &self,
        dto: &mut BaseItemDto,
        item_id: Uuid,
        image: &ItemImageInfo,
    ) -> Option<String> {
        let tag = self.image_tag(item_id, image).await?;
        if let Some(hash) = image.blur_hash.as_deref().filter(|h| !h.is_empty()) {
            Self::record_blur_hash(dto, image.image_type, &tag, hash);
        }
        Some(tag)
    }

    /// Attaches the item's cast/crew people (port of `AttachPeople`), including
    /// each person's primary-image tag when available.
    async fn attach_people(
        &self,
        dto: &mut BaseItemDto,
        item: &BaseItemEntity,
        prefetched: &Prefetched,
    ) -> Result<(), ServiceError> {
        let item_id = row_id(item);
        // The page's credits and their images were bulk-loaded once by the
        // prefetch (the per-item get_people + per-person load_images was the
        // N+1 cost of a large-cast item).
        let people = prefetched
            .people
            .get(&item_id)
            .map_or(&[][..], Vec::as_slice);
        let images_by_person = &prefetched.person_images;

        let mut list = Vec::with_capacity(people.len());
        for person in people {
            // The by-name item id (one per name, what favorites key on);
            // pre-unification rows fall back to the credit id.
            //
            // A credit that resolves to neither has no id a client could follow.
            // C# `AttachPeople` only ever adds a `BaseItemPerson` once the name
            // resolved to a `Person` item (`list.Add` sits inside the
            // `dictionary.TryGetValue` branch), so dropping the credit is the
            // upstream behaviour — emitting one carrying the nil GUID would give
            // the client a `People[].Id` that 404s on every follow-up request.
            let Some(person_id) = prefetched
                .person_ids_by_name
                .get(person.name.as_str())
                .copied()
                .or_else(|| Uuid::parse_str(&person.id).ok())
            else {
                tracing::warn!(
                    person = %person.name,
                    "skipping credit: stored person id is not a guid"
                );
                continue;
            };
            // Resolve the person's primary image tag (from the materialized Person
            // item's image rows) so the client renders cast/crew artwork.
            let primary_image_tag = match images_by_person
                .get(&person_id)
                .and_then(|images| images.iter().find(|i| i.image_type == ImageType::Primary))
            {
                Some(primary) => self.image_tag(person_id, primary).await,
                None => None,
            };
            list.push(BaseItemPerson {
                name: Some(person.name.clone()),
                id: person_id,
                role: person.role.clone(),
                type_: person
                    .person_type
                    .as_deref()
                    .map_or(ferrofin_model::data::PersonKind::Unknown, |t| {
                        person_kind_from_str(t)
                    }),
                primary_image_tag,
                image_blur_hashes: None,
            });
        }
        dto.people = Some(list); // Jellyfin emits [] when People is requested but there are none
        Ok(())
    }

    /// `SeriesStudio` — `series.Studios.FirstOrDefault()`, field-gated.
    ///
    /// Port of the two identical `ItemFields.SeriesStudio` blocks upstream
    /// (v10.11.8 Emby.Server.Implementations/Dto/DtoService.cs:1228-1234 for an
    /// episode and :1256-1262 for a season). The value belongs to the SERIES,
    /// so it comes out of the page's prefetched series rows rather than off the
    /// item being projected — which is why Ferrofin, which only ever read the
    /// item's own columns here, emitted nothing at all.
    fn attach_series_studio(
        dto: &mut BaseItemDto,
        item: &BaseItemEntity,
        options: &DtoOptions,
        prefetched: &Prefetched,
    ) {
        if !options.contains_field(ItemFields::SeriesStudio) {
            return;
        }
        let Some(series_id) = item
            .series_id
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            return;
        };
        // `FirstOrDefault()` over the series' studio list, in stored order.
        dto.series_studio = prefetched
            .series_studios
            .get(&series_id)
            .and_then(|studios| split_multi(Some(studios.as_str())).into_iter().next());
    }

    /// Attaches the item's studios as name/id pairs (port of `AttachStudios`).
    fn attach_studios(dto: &mut BaseItemDto, item: &BaseItemEntity, prefetched: &Prefetched) {
        let studios = split_multi(item.studios.as_deref());
        let pairs = studios
            .into_iter()
            .map(|name| NameGuidPair {
                id: prefetched.value_id(3, &name), // 3 = Studios
                name: Some(name),
            })
            .collect();
        dto.studios = Some(pairs);
    }

    /// Attaches the item's genres as names and as name/id pairs (port of the
    /// `Genres`/`AttachGenreItems` block).
    fn attach_genres(
        dto: &mut BaseItemDto,
        item: &BaseItemEntity,
        kind: BaseItemKind,
        prefetched: &Prefetched,
    ) {
        let genres = split_multi(item.genres.as_deref());
        // Music items resolve against the MusicGenre value space; everything else
        // against the plain Genre space. Both are stored as `ItemValueType::Genre`
        // (2) in this schema, so the id lookup is the same table.
        let _is_music_genres = kinds::is_music(kind);
        let pairs = genres
            .iter()
            .map(|name| NameGuidPair {
                name: Some(name.clone()),
                id: prefetched.value_id(2, name), // 2 = Genre
            })
            .collect();
        dto.genre_items = Some(pairs);
        dto.genres = Some(genres);
    }

    /// Attaches artist / album-artist names and name-id pairs (port of the
    /// `IHasArtist`/`IHasAlbumArtist` blocks). Artist item ids are resolved from
    /// the shared `ItemValues` table (`Artist`/`AlbumArtist` value types).
    fn attach_artists(dto: &mut BaseItemDto, item: &BaseItemEntity, prefetched: &Prefetched) {
        let artists = split_multi(item.artists.as_deref());
        if !artists.is_empty() {
            let items = artists
                .iter()
                .map(|name| NameGuidPair {
                    name: Some(name.clone()),
                    // Prefer the ALBUM-ARTIST value id: that is the one the
                    // by-name materializer backs with a browsable MusicArtist
                    // row, so a performer who is also an album artist links to
                    // a real page instead of a dangling id. Pure performers
                    // keep the Artist (0) value id until the artist-hierarchy
                    // work lands.
                    id: {
                        // One clean-key computation serves both lookups.
                        let clean = prefetched.clean_key(name);
                        prefetched
                            .lookup_clean(1, &clean)
                            .unwrap_or_else(|| prefetched.value_id_clean(0, &clean))
                    },
                })
                .collect();
            dto.artists = Some(artists);
            dto.artist_items = Some(items);
        }

        let album_artists = split_multi(item.album_artists.as_deref());
        if !album_artists.is_empty() {
            dto.album_artist = album_artists.first().cloned();
            let items = album_artists
                .iter()
                .map(|name| NameGuidPair {
                    name: Some(name.clone()),
                    id: prefetched.value_id(1, name), // 1 = AlbumArtist
                })
                .collect();
            dto.album_artists = Some(items);
        }
    }

    /// Applies the images (single-image tags + backdrops) to the DTO (port of
    /// the image loop in `AttachBasicFields`).
    async fn attach_images(
        &self,
        dto: &mut BaseItemDto,
        item_id: Uuid,
        images: &[ItemImageInfo],
        options: &DtoOptions,
    ) {
        dto.image_blur_hashes = Some(HashMap::new());

        // Backdrops (a "multiple" image type) up to the per-type limit.
        let backdrop_limit = options.image_limit(ImageType::Backdrop);
        if backdrop_limit > 0 {
            let backdrops: Vec<&ItemImageInfo> = images
                .iter()
                .filter(|i| i.image_type == ImageType::Backdrop)
                .take(usize::try_from(backdrop_limit).unwrap_or(usize::MAX))
                .collect();
            let mut tags = Vec::with_capacity(backdrops.len());
            for image in backdrops {
                if let Some(tag) = self.tag_and_fill_blur_hash(dto, item_id, image).await {
                    tags.push(tag);
                }
            }
            dto.backdrop_image_tags = Some(tags); // [] when the item has no backdrops (matches Jellyfin)
        }

        if options.enable_images {
            let mut image_tags = HashMap::new();
            for image in images
                .iter()
                .filter(|i| !allows_multiple_images(i.image_type))
                .filter(|i| options.image_limit(i.image_type) > 0)
            {
                if let Some(tag) = self.tag_and_fill_blur_hash(dto, item_id, image).await {
                    image_tags.insert(image.image_type, tag);
                }
            }
            // Always emit the map (empty `{}` when the item has no single-image
            // tags), matching Jellyfin's `dto.ImageTags = []` inside
            // `EnableImages`. A `None` here omits the field → the SDK sees null,
            // and the Android TV client NPEs on `getImageTags().containsKey(...)`
            // while binding a 16:9 card.
            dto.image_tags = Some(image_tags);
        }

        // Keep the blurhash map even when empty: Jellyfin sets
        // `dto.ImageBlurHashes = []` unconditionally in `AttachBasicFields` and
        // never nulls it, so strict clients that deref it (same crash class as
        // `ImageTags`) always see `{}`, not null.
    }

    /// Builds the full DTO for one item row (port of `GetBaseItemDtoInternal` +
    /// `AttachBasicFields`), honoring every [`DtoOptions`] toggle.
    ///
    /// `prefetched` carries the relation rows bulk-loaded for the page (a
    /// single item is a page of one) — `build_dto` itself issues no per-item
    /// relation queries, so the N+1 projection path no longer exists.
    #[allow(clippy::too_many_lines)]
    async fn build_dto(
        &self,
        item: &BaseItemEntity,
        options: &DtoOptions,
        user: Option<&UserEntity>,
        owner_id: Option<Uuid>,
        prefetched: &mut Prefetched,
        repeated: bool,
    ) -> Result<BaseItemDto, ServiceError> {
        let item_id = row_id(item);
        let kind = row_kind(item);

        let images = if options.enable_images
            || options.contains_field(ItemFields::PrimaryImageAspectRatio)
        {
            take_or_clone(&mut prefetched.images, &item_id, repeated).unwrap_or_default()
        } else {
            Vec::new()
        };

        let mut dto = BaseItemDto {
            id: item_id,
            server_id: Some(self.server_id.clone()),
            // Clients see `GetClientTypeName` (`LiveTvChannel` → "TvChannel",
            // `LiveTvProgram` → "Program"); the internal `kind` keeps driving
            // the per-kind gates below.
            type_: client_kind(kind),
            media_type: parse_media_type(item.media_type.as_deref()),
            ..BaseItemDto::default()
        };

        // People.
        if options.contains_field(ItemFields::People) {
            self.attach_people(&mut dto, item, prefetched).await?;
        }

        // Primary-image aspect ratio.
        if options.contains_field(ItemFields::PrimaryImageAspectRatio) {
            dto.primary_image_aspect_ratio = self.primary_aspect_ratio(item_id, &images).await;
        }

        // Display-preferences id. `BaseItem.DisplayPreferencesId`
        // (v10.11.8 MediaBrowser.Controller/Entities/BaseItem.cs:243-251) keys
        // display prefs by the item's TYPE, not by the item —
        // `thisType == typeof(Folder) ? Id : thisType.FullName.GetMD5()` — with
        // `CollectionFolder` overriding it back to `Id`
        // (CollectionFolder.cs:55). Two episodes of the same series therefore
        // share one key, which is what makes "sort this view by X" stick across
        // a library instead of being re-chosen per item.
        if options.contains_field(ItemFields::DisplayPreferencesId) {
            let keyed_by_own_id = matches!(
                item.type_.as_str(),
                "MediaBrowser.Controller.Entities.Folder"
                    | "MediaBrowser.Controller.Entities.CollectionFolder"
            );
            dto.display_preferences_id = Some(if keyed_by_own_id {
                item_id.simple().to_string()
            } else {
                ferrofin_common::extensions::get_md5(&item.type_)
                    .simple()
                    .to_string()
            });
        }

        // User-specific play-state.
        if user.is_some() {
            // C# `item.GetPlayAccess(user)` — Full unless parental control blocks it (not ported).
            if options.contains_field(ItemFields::PlayAccess) {
                dto.play_access = Some(ferrofin_model::library::PlayAccess::Full);
            }
            if options.enable_user_data {
                dto.user_data = take_or_clone(&mut prefetched.user_data, &item_id, repeated);
                // C# `BaseItem.FillUserDataDtoValues`: a positive resume position
                // over a known runtime becomes `PlayedPercentage` — the value
                // client progress bars render on posters and resume rows.
                if !item.is_folder
                    && let Some(ud) = dto.user_data.as_mut()
                    && ud.playback_position_ticks > 0
                    && let Some(runtime) = item.run_time_ticks.filter(|rt| *rt > 0)
                {
                    ud.played_percentage =
                        Some(percent_of_ticks(ud.playback_position_ticks, runtime));
                }
                // Folder UserData carries UnplayedItemCount = unplayed leaf
                // descendants; leaf items leave it unset. The branch keys on the
                // runtime C# `IsFolder` (`folder_emits_counts`): pure by-name
                // kinds never enter it, a MusicArtist only when physically
                // parented.
                //
                // Port of `Folder.FillUserDataDtoValues` (v10.11.8
                // MediaBrowser.Controller/Entities/Folder.cs:1798-1838), whose
                // two gates are DIFFERENT and must stay so: everything below is
                // behind `SupportsUserDataFromChildren` (`folder_emits_counts`),
                // but `RecursiveItemCount` is then gated on its FIELD alone
                // while the played numbers are gated on `SupportsPlayedStatus`.
                // Measured on the oracle: a MusicAlbum carries
                // RecursiveItemCount=3 and NO PlayedPercentage, because
                // MusicAlbum overrides SupportsPlayedStatus to false and
                // SupportsUserDataFromChildren to true. Ferrofin emitted
                // neither, on any folder.
                if folder_emits_counts(item)
                    && let Some(c) = prefetched.played_counts.get(&item_id).copied()
                {
                    // `GetRecursiveChildCount(user)` is the same recursive
                    // non-folder non-virtual query as the unplayed one, minus
                    // `IsPlayed = false` — which is exactly `total`.
                    if options.contains_field(ItemFields::RecursiveItemCount) {
                        dto.recursive_item_count = Some(c.total);
                    }
                    if kinds::supports_played_status(kind) {
                        let unplayed = c.total - c.played;
                        let ud = dto
                            .user_data
                            .get_or_insert_with(|| empty_user_data_dto(item_id));
                        ud.unplayed_item_count = Some(unplayed);
                        // `if (itemDto?.RecursiveItemCount > 0)` — the DTO
                        // field, so a caller that did not ask for
                        // `RecursiveItemCount` takes the else branch, exactly
                        // as upstream does.
                        if let Some(total) = dto.recursive_item_count.filter(|t| *t > 0) {
                            let pct = 100.0 - (f64::from(unplayed) / f64::from(total)) * 100.0;
                            ud.played_percentage = Some(pct);
                            ud.played = pct >= 100.0;
                        } else {
                            ud.played = unplayed == 0;
                        }
                    }
                }
            }
        }

        // `if (!dto.ChildCount.HasValue && item.SourceType == SourceType.Library)
        //  { if (item is MusicAlbum || item is Season || item is Playlist)
        //      { dto.ChildCount = dto.RecursiveItemCount; … } }`
        // (Emby.Server.Implementations/Dto/DtoService.cs:473-480). NOT gated on
        // the `ChildCount` field — the comment upstream is "for these types we
        // can try to optimize and assume these values will be equal" — which is
        // why a live 10.11.8 answers `ChildCount: 3` for an album on a page that
        // never asked for it. `attach_child_count` already has `??=` semantics,
        // so the real count still wins where one was asked for.
        //
        // The `SourceType == Library` guard costs nothing to honour here: the
        // three kinds named are library kinds, and the guide's `LiveTV`-sourced
        // rows are not among them.
        //
        // The shortcut has a SECOND half, which Ferrofin used to drop:
        //     var folderChildCount = folder.LinkedChildren.Length;
        //     // The default is an empty array, so we can't reliably use the
        //     // count when it's empty
        //     if (folderChildCount > 0) { dto.ChildCount ??= folderChildCount; }
        // (DtoService.cs:481-486). It is what gives a Playlist a `ChildCount` on
        // a page that asked for neither `ChildCount` nor `RecursiveItemCount` —
        // a playlist's entries ARE its linked children, so the count is real
        // there, while a MusicAlbum's/Season's linked-children array is empty
        // and the `> 0` test keeps it out. The whole shortcut lives inside
        // `AttachUserSpecificInfo`, which upstream calls only for a user; the
        // first half is already user-gated in effect (`recursive_item_count` is
        // only ever set under `user.is_some()`), so only this half needs to say
        // so out loud.
        if dto.child_count.is_none()
            && matches!(
                kind,
                BaseItemKind::MusicAlbum | BaseItemKind::Season | BaseItemKind::Playlist
            )
        {
            dto.child_count = dto.recursive_item_count;
            if dto.child_count.is_none() && user.is_some() {
                dto.child_count = prefetched
                    .linked_child_counts
                    .get(&item_id)
                    .copied()
                    .filter(|count| *count > 0);
            }
        }

        // `if (options.ContainsField(ItemFields.CumulativeRunTimeTicks))
        //  { dto.CumulativeRunTimeTicks = item.RunTimeTicks; }`
        // (Emby.Server.Implementations/Dto/DtoService.cs:492-495). It sits in
        // the `item is Folder` branch and NOT under `EnableUserData`, so it is
        // emitted for an anonymous caller too. The value is the folder's own
        // stored runtime, which Ferrofin already had and simply never copied —
        // measured: J's MusicAlbum returns CumulativeRunTimeTicks=60000000 next
        // to the RunTimeTicks=60000000 Ferrofin was already reporting.
        if item.is_folder && options.contains_field(ItemFields::CumulativeRunTimeTicks) {
            dto.cumulative_run_time_ticks = item.run_time_ticks;
        }

        // Media sources. Jellyfin only attaches these for `IHasMediaSources`
        // (video/audio) — a Genre/Studio/Person/folder has no playable source, so
        // it must not carry a spurious one (C# `DtoService` gates on the interface).
        // A Live TV channel IS `IHasMediaSources`, but its `GetMediaSources`
        // override returns the one Placeholder source, not a probed file.
        if options.contains_field(ItemFields::MediaSources) && is_live_tv_channel(kind) {
            dto.media_sources = Some(vec![ferrofin_model::dto::MediaSourceInfo {
                id: Some(item_id.simple().to_string()),
                name: item.name.clone(),
                path: item.path.clone(),
                run_time_ticks: item.run_time_ticks,
                type_: ferrofin_model::dto::MediaSourceType::Placeholder,
                is_infinite_stream: item.run_time_ticks.is_none(),
                ..ferrofin_model::dto::MediaSourceInfo::default()
            }]);
        } else if options.contains_field(ItemFields::MediaSources)
            && (kinds::is_video(kind) || kinds::is_audio(kind))
        {
            // The row and its streams are already prefetched, so assemble the
            // static source directly — no per-item retrieve_item + streams_dto.
            let streams = prefetched
                .media_streams
                .get(&item_id)
                .cloned()
                .unwrap_or_default();
            let attachments = prefetched
                .media_attachments
                .get(&item_id)
                .cloned()
                .unwrap_or_default();
            let mut sources = vec![
                crate::media_source_manager::FerrofinMediaSourceManager::static_source(
                    item,
                    streams,
                    attachments,
                ),
            ];
            // Merged alternate versions report as additional selectable sources
            // (C# `GetStaticMediaSources` includes `LinkedAlternateVersions`).
            for alt in prefetched.alternates.get(&item_id).into_iter().flatten() {
                let alt_streams = prefetched
                    .media_streams
                    .get(&row_id(alt))
                    .cloned()
                    .unwrap_or_default();
                let alt_attachments = prefetched
                    .media_attachments
                    .get(&row_id(alt))
                    .cloned()
                    .unwrap_or_default();
                sources.push(
                    crate::media_source_manager::FerrofinMediaSourceManager::static_source(
                        alt,
                        alt_streams,
                        alt_attachments,
                    ),
                );
            }
            dto.media_sources = Some(sources);
        }

        // Studios.
        if options.contains_field(ItemFields::Studios) {
            Self::attach_studios(&mut dto, item, prefetched);
        }

        self.attach_basic_fields(
            &mut dto, item, kind, &images, options, owner_id, prefetched, repeated,
        )
        .await?;

        let perms = prefetched.content_permissions.as_ref();
        // Can-delete / can-download: the file-level fact gated by the user's
        // policy (C# `BaseItem.CanDelete(user)` / `CanDownload(user)`). The
        // per-library `EnableContentDeletionFromFolders` refinement needs the
        // un-ported collection-folder walk and is deferred; admin or the global
        // permission covers the real cases.
        // Live TV channels/programmes hard-override both to false upstream
        // (`LiveTvChannel.CanDelete() => false`, and `LiveTvProgram` keeps the
        // `BaseItem` defaults — no file to delete or download).
        let live_tv = is_live_tv_channel(kind) || is_live_tv_program(kind);
        if options.contains_field(ItemFields::CanDelete) {
            // By-name items (Genre/Studio/Person/…) have no file — C# `CanDelete()`
            // returns false (default `IsFileProtocol`, plus explicit overrides) —
            // and so do the library containers: `CollectionFolder.CanDelete()`
            // (CollectionFolder.cs:107) and `BasePluginFolder.CanDelete()`
            // hard-return false, and `Folder.CanDelete()` (Folder.cs:187) returns
            // false for an `IsRoot` folder (`UserRootFolder`/`AggregateFolder`).
            // `kinds::can_delete` is that override table; `has_parent` carries
            // `MusicArtist.CanDelete => !IsAccessedByName`.
            let has_parent = item
                .parent_id
                .as_deref()
                .and_then(|p| Uuid::parse_str(p).ok())
                .is_some_and(|p| !p.is_nil());
            let file_deletable =
                !item.is_virtual_item && kinds::can_delete(kind, has_parent) && !live_tv;
            dto.can_delete = Some(file_deletable && perms.is_none_or(|p| p.can_delete));
        }
        if options.contains_field(ItemFields::CanDownload) {
            // C# `CanDownload()` is false by default and only true for playable media;
            // a by-name item is not a folder but still isn't downloadable.
            let file_downloadable = !item.is_folder && !kinds::is_item_by_name(kind) && !live_tv;
            dto.can_download = Some(file_downloadable && perms.is_none_or(|p| p.can_download));
        }

        Ok(dto)
    }

    /// Sets the simple scalar/collection fields and the kind-specific extras on
    /// the DTO (port of `AttachBasicFields`).
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    async fn attach_basic_fields(
        &self,
        dto: &mut BaseItemDto,
        item: &BaseItemEntity,
        kind: BaseItemKind,
        images: &[ItemImageInfo],
        options: &DtoOptions,
        _owner_id: Option<Uuid>,
        prefetched: &mut Prefetched,
        repeated: bool,
    ) -> Result<(), ServiceError> {
        let item_id = row_id(item);

        // `if (item is IHasCollectionType hasCollectionType) dto.CollectionType =
        // hasCollectionType.CollectionType;` (DtoService.cs:1061-1064) — set for
        // EVERY endpoint and gated by no `ItemFields` flag, which is why it must
        // live here and not be backfilled per handler. jellyfin-web keys a
        // library's whole presentation off it.
        dto.collection_type = collection_type_of(item, kind);

        if options.contains_field(ItemFields::DateCreated) {
            dto.date_created = item.date_created;
        }

        // C# `DtoService` sets DateLastMediaAdded only for `Folder` items.
        if options.contains_field(ItemFields::DateLastMediaAdded) && item.is_folder {
            dto.date_last_media_added = item.date_last_media_added;
        }

        if options.contains_field(ItemFields::Etag) {
            dto.etag = Some(compute_etag(item.date_last_saved));
        }

        if options.contains_field(ItemFields::Settings) {
            dto.lock_data = Some(item.is_locked);
            dto.forced_sort_name = item.forced_sort_name.clone();
            dto.preferred_metadata_country_code = item.preferred_metadata_country_code.clone();
            dto.preferred_metadata_language = item.preferred_metadata_language.clone();
            dto.locked_fields = Some(Vec::new()); // Jellyfin emits item.LockedFields ([] here)
        }

        dto.end_date = item.end_date;

        // Container is always set from the file extension (C# `dto.Container = item.Container`,
        // which resolution fills from the extension) — folders have none, so it stays absent.
        dto.container = item
            .path
            .as_deref()
            .and_then(|p| std::path::Path::new(p).extension())
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);

        // Gated scalar defaults Jellyfin emits when the field is requested (item_detail, not lists).
        if options.contains_field(ItemFields::EnableMediaSourceDisplay) {
            dto.enable_media_source_display = Some(true);
        }
        if options.contains_field(ItemFields::SpecialFeatureCount) {
            dto.special_feature_count = Some(0); // no extras subsystem yet
        }
        if options.contains_field(ItemFields::LocalTrailerCount) {
            dto.local_trailer_count = Some(0);
        }

        // Jellyfin emits an empty [] / {} for these when the field is requested but the item has
        // none (its DtoService always assigns the collection), so populate the empty default.
        if options.contains_field(ItemFields::ExternalUrls) {
            // Built here rather than behind a manager seam, so a page
            // of items costs the one batched id read the prefetch already did
            // instead of a query per item (C# builds them inside `DtoService`
            // from the item's already-loaded `ProviderIds` for the same reason).
            let empty = HashMap::new();
            let series_ids = item
                .series_id
                .as_deref()
                .and_then(|id| Uuid::parse_str(id).ok())
                .and_then(|id| prefetched.series_provider_ids.get(&id));
            // C# reads `season.Series.DisplayOrder` / `episode.Series.DisplayOrder`
            // — the OWNING SERIES' value, out of its `Data` blob. Reading the
            // season's or episode's own blob would always find nothing, and
            // gating on `kind == Series` would never fire, because only a
            // Season/Episode link consults it at all.
            let display_order = item
                .series_id
                .as_deref()
                .and_then(|id| Uuid::parse_str(id).ok())
                .and_then(|id| prefetched.series_display_order.get(&id));
            dto.external_urls = Some(ferrofin_providers::external_urls(
                &ferrofin_providers::ExternalIdItem {
                    kind,
                    provider_ids: prefetched.provider_ids.get(&item_id).unwrap_or(&empty),
                    index_number: item.index_number.and_then(|n| i32::try_from(n).ok()),
                    parent_index_number: item
                        .parent_index_number
                        .and_then(|n| i32::try_from(n).ok()),
                    series_provider_ids: series_ids,
                    series_display_order: display_order.map(String::as_str),
                    musicbrainz_server: &self.musicbrainz_server,
                },
            ));
        }

        if options.contains_field(ItemFields::Tags) {
            dto.tags = Some(split_multi(item.tags.as_deref()));
        }

        // Images (single-type tags + backdrops).
        self.attach_images(dto, item_id, images, options).await;

        // Width/Height are the item's OWN stored dimensions — the video/photo
        // frame size on `BaseItem.Width`/`Height` (BaseItem.cs:405/407) — read
        // straight off the row by `DtoService` (DtoService.cs:1478-1494) and
        // emitted only when positive. NOT the primary image's dimensions: a
        // folder stores 0 and upstream therefore omits both, where sourcing them
        // from the generated poster made every library folder claim the poster's
        // size.
        if options.contains_field(ItemFields::Width)
            && let Some(width) = item.width.filter(|w| *w > 0)
        {
            dto.width = i32::try_from(width).ok();
        }
        if options.contains_field(ItemFields::Height)
            && let Some(height) = item.height.filter(|h| *h > 0)
        {
            dto.height = i32::try_from(height).ok();
        }

        if options.contains_field(ItemFields::Genres) {
            Self::attach_genres(dto, item, kind, prefetched);
        }

        dto.index_number = item.index_number.and_then(|n| i32::try_from(n).ok());
        dto.parent_index_number = item.parent_index_number.and_then(|n| i32::try_from(n).ok());

        // Jellyfin's `IsFolder` is a per-type property, not a stored flag: a
        // Genre/Studio/Person is `BaseItem` (not a folder) even though Ferrofin stores
        // `is_folder=true` for some of them. For by-name items use the kind-faithful
        // value (`kinds::is_folder` — false for Genre/Studio/Person/Year/MusicGenre,
        // true only for MusicArtist), matching the C# class hierarchy.
        let item_is_folder = if kinds::is_item_by_name(kind) {
            kinds::is_folder(kind)
        } else {
            item.is_folder
        };
        if item_is_folder {
            dto.is_folder = Some(true);
        } else if kinds::is_video(kind) || kinds::is_audio(kind) || is_live_tv_channel(kind) {
            // A Live TV channel is `IHasMediaSources`, so C# sets IsFolder=false;
            // a programme is neither a folder nor a source, so its stays unset.
            dto.is_folder = Some(false);
        }

        // C# skips LocationType for `LiveTvProgram`, and `LiveTvChannel`
        // overrides it to `Remote`; everything else derives from the row.
        if is_live_tv_program(kind) {
            // absent on programme DTOs
        } else if is_live_tv_channel(kind) {
            dto.location_type = Some(LocationType::Remote);
        } else {
            dto.location_type = Some(if item.is_virtual_item {
                LocationType::Virtual
            } else {
                LocationType::FileSystem
            });
        }

        dto.audio = item.audio.and_then(program_audio_from_disc);
        dto.critic_rating = item.critic_rating.map(f64_to_f32);

        if options.contains_field(ItemFields::RemoteTrailers) {
            // Trailers live in the serialized `Data` blob (Jellyfin's only home
            // for them); the scan writes them there from TMDB/NFO.
            dto.remote_trailers = Some(
                crate::item_data::read_remote_trailers(item.data.as_deref())
                    .into_iter()
                    .map(|(name, url)| ferrofin_model::entities_media::MediaUrl {
                        url: Some(url),
                        name,
                    })
                    .collect(),
            );
        }

        dto.name = item.name.clone();
        dto.original_title = item.original_title.clone();
        dto.official_rating = item.official_rating.clone();
        // `Container` has no dedicated column on the row at this layer (it lives in
        // the serialized `Data` blob, not yet parsed here), so it stays `None`.

        if options.contains_field(ItemFields::Overview) {
            dto.overview = item.overview.clone();
        }
        if options.contains_field(ItemFields::OriginalTitle) {
            dto.original_title = item.original_title.clone();
        }
        if options.contains_field(ItemFields::ParentId) {
            dto.parent_id = item
                .parent_id
                .as_deref()
                .and_then(|s| Uuid::parse_str(s).ok());
        }
        if options.contains_field(ItemFields::Path) {
            dto.path = item.path.clone();
        }

        dto.premiere_date = item.premiere_date;
        dto.production_year = item.production_year.and_then(|y| i32::try_from(y).ok());

        if options.contains_field(ItemFields::ProviderIds) {
            // {} when none (matches Jellyfin).
            dto.provider_ids = Some(
                take_or_clone(&mut prefetched.provider_ids, &item_id, repeated).unwrap_or_default(),
            );
        }

        dto.run_time_ticks = item.run_time_ticks;

        if options.contains_field(ItemFields::SortName) {
            // C# `BaseItem.SortName` always derives from the name when no sort name
            // is stored/forced. Ferrofin stores it for scanned items but not for
            // by-name items (Genre/Studio/Person), so derive it here when empty.
            dto.sort_name = item
                .sort_name
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    item.name
                        .as_deref()
                        .map(ferrofin_util::sort_name::create_sort_name)
                });
        }
        if options.contains_field(ItemFields::CustomRating) {
            dto.custom_rating = item.custom_rating.clone();
        }
        if options.contains_field(ItemFields::Taglines) {
            dto.taglines = Some(match item.tagline.as_deref() {
                Some(t) if !t.is_empty() => vec![t.to_owned()],
                _ => Vec::new(),
            });
        }

        if let Some(rating) = item.community_rating.map(f64_to_f32).filter(|r| *r > 0.0) {
            dto.community_rating = Some(rating);
        }

        // Audio-normalization gain: LUFS wins over a stored gain (−18 LUFS ref).
        if let Some(lufs) = item.lufs.map(f64_to_f32) {
            dto.normalization_gain = Some(-18.0 - lufs);
        } else if let Some(gain) = item.normalization_gain.map(f64_to_f32) {
            dto.normalization_gain = Some(gain);
        }

        // Audio extras.
        if kinds::is_audio(kind) {
            dto.album = item.album.clone();
            dto.extra_type = item.extra_type.and_then(extra_type_from_disc);
            // A track's parent is its album row — jellyfin-web's now-playing
            // bar and track lists link back through AlbumId. Upstream reads
            // `Audio.AlbumEntity`, i.e. `FindParent<MusicAlbum>()`, so the id is
            // only emitted when the parent really IS an album: an `AudioBook`
            // hangs off its books library, and pointing AlbumId at a collection
            // folder sends the client somewhere that is not an album.
            if kind == BaseItemKind::Audio {
                dto.album_id = item
                    .parent_id
                    .as_deref()
                    .and_then(|p| Uuid::parse_str(p).ok());
            }
        }

        // Artists / album-artists — only the kinds that implement C#
        // `IHasArtist`/`IHasAlbumArtist` (Audio, AudioBook, MusicAlbum,
        // MusicVideo) carry them; Jellyfin never emits artist fields elsewhere.
        if kinds::has_artist_fields(kind) {
            Self::attach_artists(dto, item, prefetched);
        }

        // `dto.HasLyrics = audio.GetMediaStreams().Any(s => s.Type ==
        // MediaStreamType.Lyric)` (v10.11.8 Emby.Server.Implementations/Dto/
        // DtoService.cs:308-311). NOT field-gated, and NOT true-only: Jellyfin
        // sends `false` on every `Audio` DTO with no lyric stream, and omitting
        // the key is the null-where-Jellyfin-sends-non-null shape strict clients
        // crash on. `AudioBook : Audio` upstream, so the predicate is
        // `kinds::is_audio`.
        if kinds::is_audio(kind) {
            dto.has_lyrics = Some(prefetched.has_lyrics.contains(&item_id));
        }

        // Video extras.
        if kinds::is_video(kind) {
            dto.video_type = Some(VideoType::VideoFile);
            dto.extra_type = item.extra_type.and_then(extra_type_from_disc);
            // C# only assigns when true, so the key is absent otherwise (the
            // `skip_serializing_if` on the DTO matches that omission).
            if prefetched.has_subtitles.contains(&item_id) {
                dto.has_subtitles = Some(true);
            }

            if options.contains_field(ItemFields::Trickplay) {
                // Jellyfin emits {} when requested but there is no manifest.
                let manifest = take_or_clone(&mut prefetched.trickplay, &item_id, repeated)
                    .unwrap_or_default();
                dto.trickplay = Some(to_trickplay_manifest(&manifest));
            }
        }

        // Chapters — [] when requested but there are none (matches Jellyfin).
        // C# assigns them only inside its `item is Video` branch, so every
        // non-video kind (a folder, an album, a Live TV channel or programme)
        // omits the key entirely.
        if options.contains_field(ItemFields::Chapters) && kinds::is_video(kind) {
            let mut chapters =
                take_or_clone(&mut prefetched.chapters, &item_id, repeated).unwrap_or_default();
            // Each extracted chapter thumbnail needs its cache tag: clients gate
            // the chapter image request on `ImageTag` (port of
            // `ImageProcessor.GetImageCacheTag(item, chapter)`), so without it
            // the thumbnails never load however well the extraction ran.
            for chapter in &mut chapters {
                let Some(path) = chapter.image_path.clone().filter(|p| !p.is_empty()) else {
                    continue;
                };
                chapter.image_tag = self
                    .image_tag(
                        item_id,
                        &ItemImageInfo {
                            path,
                            image_type: ImageType::Chapter,
                            date_modified: chapter.image_date_modified,
                            width: 0,
                            height: 0,
                            blur_hash: None,
                        },
                    )
                    .await;
            }
            dto.chapters = Some(chapters);
        }

        // Media streams. This is the LAST read of this id's stream rows, so
        // no reader remains and the entry can be moved out rather than cloned —
        // unless one does: a repeated id is projected again, and an id some
        // other page item lists as a merged alternate is read while projecting
        // THAT item, both after this point. (The earlier `MediaSources` read,
        // when its field is requested and the kind is video/audio, already took
        // its own copy; when it is not, there was no earlier read at all.)
        if options.contains_field(ItemFields::MediaStreams) {
            if is_live_tv_channel(kind) {
                // C# assigns for every `IHasMediaSources`, and a channel's
                // `GetMediaStreams()` override is always `[]` — the field is
                // present-and-empty on the channel detail, never probed rows.
                dto.media_streams = Some(Vec::new());
            } else if is_live_tv_program(kind) {
                // A programme is not `IHasMediaSources`; C# never assigns.
            } else {
                let pinned = repeated || prefetched.alt_referenced.contains(&item_id);
                let streams = take_or_clone(&mut prefetched.media_streams, &item_id, pinned)
                    .unwrap_or_default();
                if !streams.is_empty() {
                    dto.media_streams = Some(streams);
                }
            }
        }

        // Episode extras.
        if kind == BaseItemKind::Episode {
            dto.series_name = item.series_name.clone();
            dto.season_name = item.season_name.clone();
            dto.season_id = item
                .season_id
                .as_deref()
                .and_then(|s| Uuid::parse_str(s).ok());
            dto.series_id = item
                .series_id
                .as_deref()
                .and_then(|s| Uuid::parse_str(s).ok());
            Self::attach_series_studio(dto, item, options, prefetched);
        }

        // Season extras.
        if kind == BaseItemKind::Season {
            dto.series_name = item.series_name.clone();
            dto.series_id = item
                .series_id
                .as_deref()
                .and_then(|s| Uuid::parse_str(s).ok());
            Self::attach_series_studio(dto, item, options, prefetched);
        }

        // Book extras — port of `DtoService.SetBookProperties`, which projects
        // the one `IHasSeries` field a book carries (the book series its
        // filename or containing folder names). Upstream has no equivalent for
        // `AudioBook`, so neither do we.
        if kind == BaseItemKind::Book {
            dto.series_name = item.series_name.clone();
        }

        // Series air days/time — C# v10.11.8 `DtoService.cs:1243-1244`
        // (`dto.AirDays = series.AirDays; dto.AirTime = series.AirTime;`;
        // master carries the same two lines at `DtoService.cs:1422-1423`).
        // `Series.AirDays` is a runtime-only property (v10.11.8 `Series.cs:31`
        // sets it to `Array.Empty<DayOfWeek>()` in the constructor and 10.11.8
        // persists no `AirDays` column), so a DB-loaded series always serializes `[]` —
        // never null. Ferrofin omitted the field entirely, which is the
        // null-where-Jellyfin-sends-non-null shape strict clients crash on.
        if kind == BaseItemKind::Series {
            dto.air_days = Some(Vec::new());
            dto.air_time = None; // no flat column at this layer, as upstream
        }

        // Production locations.
        if options.contains_field(ItemFields::ProductionLocations) {
            let locations = split_multi(item.production_locations.as_deref());
            if !locations.is_empty() || kind == BaseItemKind::Movie {
                dto.production_locations = Some(locations);
            }
        }

        if options.contains_field(ItemFields::Width)
            && let Some(width) = item
                .width
                .and_then(|w| i32::try_from(w).ok())
                .filter(|w| *w > 0)
        {
            dto.width = Some(width);
        }
        if options.contains_field(ItemFields::Height)
            && let Some(height) = item
                .height
                .and_then(|h| i32::try_from(h).ok())
                .filter(|h| *h > 0)
        {
            dto.height = Some(height);
        }

        dto.channel_id = item
            .channel_id
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok());

        attach_photo_exif(dto, item);
        attach_photo_album(dto, item, &prefetched.photo_album_names);

        Ok(())
    }

    /// All provider ids for `item_ids` in one query per chunk, keyed by item
    /// id. Prefetched for the page so the per-item lookup does not fan out
    /// across the 2-connection pool.
    async fn load_provider_ids_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, HashMap<String, String>>, ServiceError> {
        let mut map: HashMap<Uuid, HashMap<String, String>> =
            HashMap::with_capacity(item_ids.len());
        if item_ids.is_empty() {
            return Ok(map);
        }
        for chunk in item_ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let ph = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT "ItemId", "ProviderId", "ProviderValue" FROM "BaseItemProviders"
                   WHERE "ItemId" IN ({ph})"#,
            );
            let mut query = sqlx::query_as::<_, (String, String, String)>(&sql);
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            for (item_id, key, value) in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                if let Ok(id) = Uuid::parse_str(&item_id) {
                    map.entry(id).or_default().insert(key, value);
                }
            }
        }
        Ok(map)
    }

    /// The `DisplayOrder` of each of `series_ids` that declares one.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] on a storage failure.
    async fn load_series_display_order(
        &self,
        series_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, String>, ServiceError> {
        let mut map: HashMap<Uuid, String> = HashMap::new();
        if series_ids.is_empty() {
            return Ok(map);
        }
        let stored: Vec<String> = series_ids.iter().copied().map(guid_to_db).collect();
        for (item_id, data) in self
            .db
            .item_data_blobs(&stored)
            .await
            .map_err(|e| ServiceError::Backend(e.to_string()))?
        {
            if let Ok(id) = Uuid::parse_str(&item_id)
                && let Some(order) = crate::item_data::read_data_string(
                    &crate::item_data::parse_data(Some(&data)),
                    "DisplayOrder",
                )
            {
                map.insert(id, order);
            }
        }
        Ok(map)
    }

    /// The `Studios` column of the given series rows, for the `SeriesStudio` a
    /// season's/episode's DTO carries (C# `series.Studios.FirstOrDefault()`).
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] on a storage failure.
    async fn load_series_studios(
        &self,
        series_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, String>, ServiceError> {
        let mut map: HashMap<Uuid, String> = HashMap::new();
        if series_ids.is_empty() {
            return Ok(map);
        }
        let stored: Vec<String> = series_ids.iter().copied().map(guid_to_db).collect();
        for (item_id, studios) in self
            .db
            .item_studios(&stored)
            .await
            .map_err(|e| ServiceError::Backend(e.to_string()))?
        {
            if let Ok(id) = Uuid::parse_str(&item_id) {
                map.insert(id, studios);
            }
        }
        Ok(map)
    }

    /// Names the given PhotoAlbum rows, for the `Album`/`AlbumId` a photo's DTO
    /// carries (C# `DtoService.SetPhotoProperties` reads `Photo.AlbumEntity`).
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] on a storage failure.
    async fn load_photo_album_names(
        &self,
        album_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, String>, ServiceError> {
        let mut map: HashMap<Uuid, String> = HashMap::with_capacity(album_ids.len());
        if album_ids.is_empty() {
            return Ok(map);
        }
        // Only a real PhotoAlbum names a photo: a loose photo hangs off the
        // library's collection folder, and calling that an album would send the
        // client somewhere that is not one. The filter lives in the query.
        let stored: Vec<String> = album_ids.iter().copied().map(guid_to_db).collect();
        for (item_id, name) in self
            .db
            .photo_album_names(&stored)
            .await
            .map_err(|e| ServiceError::Backend(e.to_string()))?
        {
            if let Ok(id) = Uuid::parse_str(&item_id) {
                map.insert(id, name);
            }
        }
        Ok(map)
    }

    /// Populates the item-by-name counts on a DTO (port of `SetItemByNameInfo`)
    /// using the injected [`ItemCountService`].
    async fn set_item_by_name_info(
        &self,
        dto: &mut BaseItemDto,
        user: Option<&UserEntity>,
    ) -> Result<(), ServiceError> {
        let Some(related) = related_item_kinds(dto.type_) else {
            return Ok(());
        };
        let access_filter = access_filter_for(user);
        let counts = self
            .item_counts
            .get_item_counts_for_name_item(dto.type_, dto.id, related, &access_filter)
            .await?;
        apply_name_counts(dto, &counts);
        Ok(())
    }

    /// Resolves by-name item counts for the whole page: groups the by-name rows
    /// by kind and issues one batched count query per kind (instead of one per
    /// row — the `ItemCounts` N+1 on an Artists/Genres/Persons page).
    async fn name_counts_batch(
        &self,
        items: &[BaseItemEntity],
        user: Option<&UserEntity>,
    ) -> Result<HashMap<Uuid, ItemCounts>, ServiceError> {
        // The rows carry their own `Name`/`CleanName`, so the count service
        // keys off those instead of re-selecting them for the page.
        let mut by_kind: HashMap<BaseItemKind, Vec<NameItemRow<'_>>> = HashMap::new();
        for item in items {
            let kind = row_kind(item);
            if related_item_kinds(kind).is_some() {
                by_kind.entry(kind).or_default().push(NameItemRow {
                    id: row_id(item),
                    name: item.name.as_deref(),
                    clean_name: item.clean_name.as_deref(),
                });
            }
        }
        let access_filter = access_filter_for(user);
        let mut out = HashMap::new();
        for (kind, rows) in by_kind {
            let related = related_item_kinds(kind).unwrap_or(&[]);
            out.extend(
                self.item_counts
                    .get_item_counts_for_name_items(kind, &rows, related, &access_filter)
                    .await?,
            );
        }
        Ok(out)
    }
}

/// Copies a by-name item's related counts onto its DTO (the count-assignment
/// tail of C# `SetItemByNameInfo`); `ChildCount` is the per-kind total.
fn apply_name_counts(dto: &mut BaseItemDto, counts: &ItemCounts) {
    dto.album_count = Some(counts.album_count);
    dto.artist_count = Some(counts.artist_count);
    dto.episode_count = Some(counts.episode_count);
    dto.movie_count = Some(counts.movie_count);
    dto.music_video_count = Some(counts.music_video_count);
    dto.program_count = Some(counts.program_count);
    dto.series_count = Some(counts.series_count);
    dto.song_count = Some(counts.song_count);
    dto.trailer_count = Some(counts.trailer_count);
    dto.child_count = Some(total_item_count(counts));
}

/// The related item kinds counted for a by-name item (port of the C#
/// `_relatedItemKinds` frozen dictionary).
fn related_item_kinds(kind: BaseItemKind) -> Option<&'static [BaseItemKind]> {
    match kind {
        BaseItemKind::MusicArtist => Some(&[
            BaseItemKind::Audio,
            BaseItemKind::MusicAlbum,
            BaseItemKind::MusicVideo,
        ]),
        BaseItemKind::MusicGenre => Some(&[
            BaseItemKind::Audio,
            BaseItemKind::MusicAlbum,
            BaseItemKind::MusicArtist,
            BaseItemKind::MusicVideo,
        ]),
        BaseItemKind::Person => Some(&[
            BaseItemKind::Audio,
            BaseItemKind::AudioBook,
            BaseItemKind::Book,
            BaseItemKind::Episode,
            BaseItemKind::Movie,
            BaseItemKind::LiveTvProgram,
            BaseItemKind::MusicAlbum,
            BaseItemKind::MusicArtist,
            BaseItemKind::MusicVideo,
            BaseItemKind::Series,
            BaseItemKind::Trailer,
        ]),
        BaseItemKind::Genre | BaseItemKind::Studio | BaseItemKind::Year => Some(&[
            BaseItemKind::Audio,
            BaseItemKind::Episode,
            BaseItemKind::Movie,
            BaseItemKind::LiveTvProgram,
            BaseItemKind::MusicAlbum,
            BaseItemKind::MusicArtist,
            BaseItemKind::MusicVideo,
            BaseItemKind::Series,
            BaseItemKind::Trailer,
        ]),
        _ => None,
    }
}

/// The access filter for the item-by-name count queries: scoped to the user when
/// one is present.
fn access_filter_for(user: Option<&UserEntity>) -> ferrofin_traits::options::InternalItemsQuery {
    ferrofin_traits::options::InternalItemsQuery {
        user: user.cloned(),
        ..Default::default()
    }
}

/// Sums the per-kind counts into the total child count (port of
/// `ItemCounts.TotalItemCount`).
fn total_item_count(counts: &ferrofin_model::dto::ItemCounts) -> i32 {
    counts.album_count
        + counts.artist_count
        + counts.episode_count
        + counts.movie_count
        + counts.music_video_count
        + counts.program_count
        + counts.series_count
        + counts.song_count
        + counts.trailer_count
}

/// Maps a stored `ProgramAudio` discriminant onto the enum.
fn program_audio_from_disc(disc: i32) -> Option<ferrofin_model::dto::ProgramAudio> {
    use ferrofin_model::dto::ProgramAudio;
    Some(match disc {
        0 => ProgramAudio::Mono,
        1 => ProgramAudio::Stereo,
        2 => ProgramAudio::Dolby,
        3 => ProgramAudio::DolbyDigital,
        4 => ProgramAudio::Thx,
        5 => ProgramAudio::Atmos,
        _ => return None,
    })
}

/// Narrows an [`f64`] rating/gain to the [`f32`] the DTO carries.
#[allow(clippy::cast_possible_truncation)]
fn f64_to_f32(value: f64) -> f32 {
    value as f32
}

/// Maps a stored `PersonType` string onto a [`PersonKind`].
fn person_kind_from_str(value: &str) -> ferrofin_model::data::PersonKind {
    use ferrofin_model::data::PersonKind;
    match value {
        "Actor" => PersonKind::Actor,
        "Director" => PersonKind::Director,
        "Composer" => PersonKind::Composer,
        "Writer" => PersonKind::Writer,
        "GuestStar" => PersonKind::GuestStar,
        "Producer" => PersonKind::Producer,
        "Conductor" => PersonKind::Conductor,
        "Lyricist" => PersonKind::Lyricist,
        "Artist" => PersonKind::Artist,
        "AlbumArtist" => PersonKind::AlbumArtist,
        "Author" => PersonKind::Author,
        "Narrator" => PersonKind::Narrator,
        _ => PersonKind::Unknown,
    }
}

/// Maps the trickplay-manager manifest of stored rows onto the DTO's
/// `mediaSourceId → (width → TrickplayInfoDto)` map.
fn to_trickplay_manifest(
    manifest: &HashMap<String, HashMap<i32, ferrofin_db::entities::playback::TrickplayInfoEntity>>,
) -> HashMap<String, HashMap<i32, TrickplayInfoDto>> {
    manifest
        .iter()
        .map(|(source_id, by_width)| {
            let widths = by_width
                .iter()
                .map(|(width, info)| (*width, to_trickplay_dto(info)))
                .collect();
            (source_id.clone(), widths)
        })
        .collect()
}

/// Maps one stored trickplay row onto its wire DTO.
fn to_trickplay_dto(
    info: &ferrofin_db::entities::playback::TrickplayInfoEntity,
) -> TrickplayInfoDto {
    TrickplayInfoDto {
        width: info.width,
        height: info.height,
        tile_width: info.tile_width,
        tile_height: info.tile_height,
        thumbnail_count: info.thumbnail_count,
        interval: info.interval,
        bandwidth: info.bandwidth,
    }
}

#[async_trait]
impl ferrofin_traits::dto::DtoService for FerrofinDtoService {
    async fn get_primary_image_aspect_ratio(
        &self,
        item_id: Uuid,
    ) -> Result<Option<f64>, ServiceError> {
        let images = self.load_images(item_id).await?;
        Ok(self.primary_aspect_ratio(item_id, &images).await)
    }

    async fn get_base_item_dto(
        &self,
        item: &BaseItemEntity,
        options: &DtoOptions,
        user: Option<&UserEntity>,
        owner_id: Option<Uuid>,
    ) -> Result<BaseItemDto, ServiceError> {
        // A single item is a batch of one: the same prefetched projection path
        // as a page, so a per-item N+1 fallback no longer exists for new
        // handlers to reach.
        self.get_base_item_dtos(std::slice::from_ref(item), options, user, owner_id, true)
            .await?
            .pop()
            .ok_or_else(|| ServiceError::Backend("projection returned no DTO".to_owned()))
    }

    async fn get_base_item_dtos(
        &self,
        items: &[BaseItemEntity],
        options: &DtoOptions,
        user: Option<&UserEntity>,
        owner_id: Option<Uuid>,
        _skip_visibility_check: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        // Visibility filtering needs the domain tree (`IsVisible`), which is not
        // ported at this layer; the caller is expected to have filtered the set,
        // so every input row is projected.
        let mut prefetched = self.prefetch(items, options, user).await?;
        // Ids the page lists more than once (a playlist may repeat a track).
        // Their prefetched entries are read once per occurrence, so they keep
        // cloning while every unique id moves its entry out — see `take_or_clone`.
        // `row_id` parses the stored id string, so the page's ids are resolved
        // once here and reused for both the repeat check and the per-item flag.
        // A single-item page cannot repeat, and `/Items/{id}`-class requests all
        // land here through a one-element slice — so skip building the set.
        let page_ids: Vec<Uuid> = items.iter().map(row_id).collect();
        let repeated_ids: std::collections::HashSet<Uuid> = if page_ids.len() < 2 {
            std::collections::HashSet::new()
        } else {
            let mut seen = std::collections::HashSet::with_capacity(page_ids.len());
            page_ids
                .iter()
                .filter(|id| !seen.insert(**id))
                .copied()
                .collect()
        };
        // By-name related counts for the page in one grouped query per kind
        // (C# calls `SetItemByNameInfo` per item).
        let name_counts = if options.contains_field(ItemFields::ItemCounts) {
            self.name_counts_batch(items, user).await?
        } else {
            HashMap::new()
        };

        let mut out = Vec::with_capacity(items.len());
        for (item, item_id) in items.iter().zip(&page_ids) {
            let mut dto = self
                .build_dto(
                    item,
                    options,
                    user,
                    owner_id,
                    &mut prefetched,
                    repeated_ids.contains(item_id),
                )
                .await?;
            if let Some(counts) = name_counts.get(&dto.id) {
                apply_name_counts(&mut dto, counts);
            }
            // ChildCount only where the C# runtime `IsFolder` is true (see
            // `folder_emits_counts`) — by-name rows are folders in storage only.
            if user.is_some()
                && options.contains_field(ItemFields::ChildCount)
                && folder_emits_counts(item)
            {
                attach_child_count(&mut dto, item, &prefetched.child_counts);
            }
            out.push(dto);
        }
        // Upstream's `DtoService` finishes a Live TV channel's DTO ITSELF
        // (v10.11.8 Emby.Server.Implementations/Dto/DtoService.cs:168-192): it
        // buckets `item is LiveTvChannel` while projecting the page and hands
        // the bucket to `LivetvManager.AddChannelInfo` at the end. That is why
        // an ordinary `GET /Items/{id}` of a channel comes back carrying its
        // channel number and currently-airing programme — the post-pass belongs
        // to projecting a channel, not to the `/LiveTv/*` routes. The `any`
        // guard keeps an ordinary page from costing a lookup.
        if let Some(live_tv) = self.live_tv.get() {
            // Two buckets, exactly as upstream keeps two: `item is LiveTvChannel`
            // and `item is LiveTvProgram` (DtoService.cs:168-192, handed over at
            // :186-192). Each `any` guard keeps an ordinary page — every page
            // that is not Live TV — from costing a lookup at all.
            if items.iter().any(|item| is_live_tv_channel(row_kind(item))) {
                live_tv.add_channel_info(&mut out, options, user).await?;
            }
            if items.iter().any(|item| is_live_tv_program(row_kind(item))) {
                live_tv
                    .add_info_to_program_dto(&mut out, options, user)
                    .await?;
            }
        }
        Ok(out)
    }

    async fn get_item_by_name_dto(
        &self,
        item: &BaseItemEntity,
        options: &DtoOptions,
        tagged_item_ids: Option<&[Uuid]>,
        user: Option<&UserEntity>,
    ) -> Result<BaseItemDto, ServiceError> {
        let mut prefetched = self
            .prefetch(std::slice::from_ref(item), options, user)
            .await?;
        // Single-item page: the id cannot repeat, so every entry moves.
        let mut dto = self
            .build_dto(item, options, user, None, &mut prefetched, false)
            .await?;

        // When the caller pre-supplies the tagged items, count them by kind
        // (port of the static `SetItemByNameInfo` overload); otherwise fall back
        // to the count-service path.
        if options.contains_field(ItemFields::ItemCounts) {
            if let Some(ids) = tagged_item_ids.filter(|ids| !ids.is_empty()) {
                self.set_tagged_counts(&mut dto, ids).await?;
            } else {
                self.set_item_by_name_info(&mut dto, user).await?;
            }
        }
        Ok(dto)
    }
}

impl FerrofinDtoService {
    /// Bulk-loads every relation `build_dto` reads for `items` — one query per
    /// relation family for the whole page instead of one (or more) per item.
    /// The per-item N+1 convoyed the 2-connection pool under concurrent load.
    #[allow(clippy::too_many_lines)] // a flat sequence of independent page prefetches
    async fn prefetch(
        &self,
        items: &[BaseItemEntity],
        options: &DtoOptions,
        user: Option<&UserEntity>,
    ) -> Result<Prefetched, ServiceError> {
        let ids: Vec<Uuid> = items.iter().map(row_id).collect();
        let want_images =
            options.enable_images || options.contains_field(ItemFields::PrimaryImageAspectRatio);
        let want_user_data = user.is_some() && options.enable_user_data;
        // User-data and the page's credits are independent; run them concurrently.
        // Images wait for the credits, because the cast's by-name Person rows
        // want images too and both id sets go in ONE `BaseItemImageInfos` read
        // (this used to be two, and a random page is mostly people).
        let user_data_fut = async {
            if want_user_data && let Some(u) = user {
                let user_id = parse_user_id(&u.id)?;
                self.user_data.get_user_data_dtos(&ids, user_id).await
            } else {
                Ok(HashMap::new())
            }
        };
        let people_fut = async {
            if options.contains_field(ItemFields::People) {
                self.library.get_people_batch(&ids).await
            } else {
                Ok(HashMap::new())
            }
        };
        let (user_data, people) = tokio::try_join!(user_data_fut, people_fut)?;
        // The page ids that can actually own media sources. A folder or a
        // by-name item (person, genre, studio, …) owns no stream, chapter,
        // trickplay or alternate-version row, so asking for them is four
        // guaranteed-empty round trips — the whole cost of an all-fields
        // `/Library/MediaFolders`, `/Items/{id}/Ancestors` or `/Persons` page,
        // where upstream pays nothing because a C# `Folder` has no streams to
        // begin with. A mixed page still asks, just with the folders left out.
        let media_ids: Vec<Uuid> = items
            .iter()
            .filter(|i| kinds::has_media_sources(row_kind(i)))
            .map(row_id)
            .collect();
        // Merged alternate versions (rows pointing at a page item via
        // `PrimaryVersionId`), so each item's extra selectable sources build
        // without a per-item query; their streams join the stream batch below.
        let alternates =
            if options.contains_field(ItemFields::MediaSources) && !media_ids.is_empty() {
                self.media_sources
                    .get_alternate_versions_batch(&media_ids)
                    .await?
            } else {
                HashMap::new()
            };
        // The heavy per-item relations, bulk-loaded once for the page when their
        // field is requested (an all-fields list DTO otherwise fans out a query
        // per item for each — costly on the 2-connection pool).
        let want_streams = options.contains_field(ItemFields::MediaStreams)
            || options.contains_field(ItemFields::MediaSources);
        // Only the items that can HAVE a media source are asked for one, and an
        // alternate version is read alongside the primary it belongs to.
        let stream_ids: Vec<Uuid> = if media_ids.is_empty() {
            Vec::new()
        } else {
            media_ids
                .iter()
                .copied()
                .chain(alternates.values().flatten().map(row_id))
                .collect()
        };
        // A source lists its attachments too (C# `MediaAttachments =
        // MediaSourceManager.GetMediaAttachments(item.Id)` on every static source);
        // both relations load concurrently so `MediaSources` costs one round-trip.
        let want_attachments = options.contains_field(ItemFields::MediaSources);
        let (media_streams, media_attachments) = tokio::try_join!(
            async {
                if want_streams && !stream_ids.is_empty() {
                    self.media_sources
                        .get_media_streams_batch(&stream_ids)
                        .await
                } else {
                    Ok(HashMap::new())
                }
            },
            async {
                if want_attachments && !stream_ids.is_empty() {
                    self.media_sources
                        .get_media_attachments_batch(&stream_ids)
                        .await
                } else {
                    Ok(HashMap::new())
                }
            }
        )?;
        // An id listed here is another page item's alternate, so its streams are
        // read while projecting that item — it cannot be drained by its own.
        let alt_referenced: std::collections::HashSet<Uuid> =
            alternates.values().flatten().map(row_id).collect();
        let want_external_urls = options.contains_field(ItemFields::ExternalUrls);
        let provider_ids = if options.contains_field(ItemFields::ProviderIds) || want_external_urls
        {
            self.load_provider_ids_batch(&ids).await?
        } else {
            HashMap::new()
        };
        // A season/episode's links come from its series' ids, so collect the
        // distinct series on the page and read their ids in the same batched
        // way (one extra query for a page of episodes, none otherwise).
        let mut series_display_order: HashMap<Uuid, String> = HashMap::new();
        // `SeriesStudio` reads the same set of series rows, so the id list is
        // built once and each half runs only when its own field was asked for
        // — a page with neither field costs no query at all.
        let mut series_ids: Vec<Uuid> = Vec::new();
        let want_series_studio = options.contains_field(ItemFields::SeriesStudio);
        if want_external_urls || want_series_studio {
            series_ids = items
                .iter()
                .filter(|i| matches!(row_kind(i), BaseItemKind::Season | BaseItemKind::Episode))
                .filter_map(|i| i.series_id.as_deref())
                .filter_map(|id| Uuid::parse_str(id).ok())
                .collect();
            series_ids.sort_unstable();
            series_ids.dedup();
        }
        let series_provider_ids = if want_external_urls {
            series_display_order = self.load_series_display_order(&series_ids).await?;
            self.load_provider_ids_batch(&series_ids).await?
        } else {
            HashMap::new()
        };
        let series_studios = if want_series_studio {
            self.load_series_studios(&series_ids).await?
        } else {
            HashMap::new()
        };
        // A photo's album is its parent row. Collect the distinct parents of
        // the page's photos and name them in one query — none at all for a page
        // with no photos on it.
        let mut album_ids: Vec<Uuid> = items
            .iter()
            .filter(|i| row_kind(i) == BaseItemKind::Photo)
            .filter_map(|i| i.parent_id.as_deref())
            .filter_map(|id| Uuid::parse_str(id).ok())
            .collect();
        album_ids.sort_unstable();
        album_ids.dedup();
        let photo_album_names = self.load_photo_album_names(&album_ids).await?;
        // Every credited person resolved to its by-name Person item, and the
        // ids whose images the projection will want.
        let (person_image_ids, person_ids_by_name) = if options.contains_field(ItemFields::People) {
            // Resolve each distinct credit NAME to its by-name Person item
            // (C# AttachPeople: `People[].Id` is the per-name item id, the
            // one favorites are written against — never the per-credit
            // `Peoples` row id, which fragments a person across types).
            // One lowercase per distinct spelling, not one per credit per
            // item: `slot_by_name` maps every RAW spelling seen to the slot
            // of the case-insensitively-deduped name it resolves through, so
            // the projection can look the id up by the stored string.
            let mut names: Vec<String> = Vec::new();
            let mut slot_by_lower: HashMap<String, usize> = HashMap::new();
            let mut slot_by_name: HashMap<String, usize> = HashMap::new();
            for person in people.values().flatten() {
                if slot_by_name.contains_key(person.name.as_str()) {
                    continue;
                }
                let slot = *slot_by_lower
                    .entry(person.name.to_lowercase())
                    .or_insert_with(|| {
                        names.push(person.name.clone());
                        names.len() - 1
                    });
                slot_by_name.insert(person.name.clone(), slot);
            }
            // The id is the ONLY thing this resolution needs, so it asks for the
            // id — not the row. Materializing a full `BaseItemEntity` per
            // credited name was the single most expensive statement on an
            // all-fields page (hundreds of 72-column rows decoded and dropped).
            let resolved = self
                .library
                .get_named_item_ids(ferrofin_model::data::BaseItemKind::Person, &names)
                .await
                .unwrap_or_default();
            let mut id_by_slot: Vec<Option<Uuid>> = vec![None; names.len()];
            let mut person_ids: Vec<Uuid> = Vec::new();
            for (slot, resolved_id) in resolved.into_iter().enumerate() {
                if let Some(id) = resolved_id
                    && let Some(entry) = id_by_slot.get_mut(slot)
                {
                    *entry = Some(id);
                    person_ids.push(id);
                }
            }
            let by_name: HashMap<String, Uuid> = slot_by_name
                .into_iter()
                .filter_map(|(name, slot)| {
                    id_by_slot.get(slot).copied().flatten().map(|id| (name, id))
                })
                .collect();
            // Pre-unification rows keyed images on the credit id; keep
            // loading those too so old databases still render cast art.
            person_ids.extend(
                people
                    .values()
                    .flatten()
                    .filter_map(|p| Uuid::parse_str(&p.id).ok()),
            );
            (person_ids, by_name)
        } else {
            (Vec::new(), HashMap::new())
        };

        // The one image read: the page's own rows (when images are wanted) plus
        // every credited person's row. `person_images` keeps its own copy of the
        // cast entries because a page item drains its own entry as it projects.
        let mut image_ids: Vec<Uuid> = if want_images { ids.clone() } else { Vec::new() };
        image_ids.extend(person_image_ids.iter().copied());
        let fetched_images = if image_ids.is_empty() {
            HashMap::new()
        } else {
            self.load_images_batch(&image_ids).await?
        };
        let person_images: HashMap<Uuid, Vec<ItemImageInfo>> = person_image_ids
            .iter()
            .filter_map(|id| fetched_images.get(id).map(|rows| (*id, rows.clone())))
            .collect();
        // With images switched off the page keeps none — only the cast art the
        // People field asked for, exactly as when these were two reads.
        let images = if want_images {
            fetched_images
        } else {
            HashMap::new()
        };
        // Studio/genre/artist ids for every name on the page in one query. Collect
        // exactly what the attach steps resolve: studios/genres only when their
        // field is requested, artists/album-artists only for the kinds that carry
        // artist fields — so a prefetched miss never wrongly nils a real id.
        let (value_ids, clean_values) = {
            // Dedup by the RAW name first, borrowing from the rows, so each
            // distinct name is cleaned exactly once for the whole page (the
            // projection then reads those cleans back out of `clean_values`
            // instead of recomputing them per name per item).
            let mut wanted: std::collections::HashSet<(i32, &str)> =
                std::collections::HashSet::new();
            let want_studios = options.contains_field(ItemFields::Studios);
            let want_genres = options.contains_field(ItemFields::Genres);
            for item in items {
                if want_studios {
                    wanted.extend(split_multi_str(item.studios.as_deref()).map(|n| (3, n)));
                }
                if want_genres {
                    wanted.extend(split_multi_str(item.genres.as_deref()).map(|n| (2, n)));
                }
                if kinds::has_artist_fields(row_kind(item)) {
                    wanted.extend(split_multi_str(item.artists.as_deref()).map(|n| (0, n)));
                    wanted.extend(split_multi_str(item.album_artists.as_deref()).map(|n| (1, n)));
                }
            }
            let mut clean_values: HashMap<String, String> = HashMap::new();
            let mut pairs: Vec<(i32, String)> = Vec::with_capacity(wanted.len());
            for (value_type, name) in wanted {
                let clean = if let Some(clean) = clean_values.get(name) {
                    clean.clone()
                } else {
                    let clean = crate::text_util::get_clean_value(name);
                    clean_values.insert(name.to_owned(), clean.clone());
                    clean
                };
                pairs.push((value_type, clean));
            }
            (self.resolve_value_ids(&pairs).await?, clean_values)
        };
        let chapters = if options.contains_field(ItemFields::Chapters) && !media_ids.is_empty() {
            self.chapters.get_chapters_batch(&media_ids).await?
        } else {
            HashMap::new()
        };
        let trickplay = if options.contains_field(ItemFields::Trickplay) && !media_ids.is_empty() {
            self.trickplay
                .get_trickplay_manifest_batch(&media_ids)
                .await?
        } else {
            HashMap::new()
        };
        // Child counts for the page's folders in one batch (C# prefetches the
        // same way before `AttachUserSpecificInfo`, which is user-gated).
        let child_counts = match user {
            Some(user) if options.contains_field(ItemFields::ChildCount) => {
                let folder_ids: Vec<Uuid> = items
                    .iter()
                    .filter(|i| {
                        folder_emits_counts(i)
                            && !matches!(
                                row_kind(i),
                                BaseItemKind::CollectionFolder | BaseItemKind::UserView
                            )
                    })
                    .map(row_id)
                    .collect();
                if folder_ids.is_empty() {
                    HashMap::new()
                } else {
                    let user_id = Uuid::parse_str(&user.id).ok();
                    self.item_counts
                        .get_child_count_batch(&folder_ids, user_id)
                        .await?
                }
            }
            _ => HashMap::new(),
        };
        // `Folder.LinkedChildren.Length` for the three kinds whose ChildCount
        // shortcut reads it. Behind `any(...)` so an ordinary page pays nothing,
        // and behind the user gate because `AttachUserSpecificInfo` — where the
        // shortcut lives — only runs for a user.
        let linked_child_counts = {
            let shortcut_ids: Vec<Uuid> = if user.is_some() {
                items
                    .iter()
                    .filter(|i| {
                        matches!(
                            row_kind(i),
                            BaseItemKind::MusicAlbum
                                | BaseItemKind::Season
                                | BaseItemKind::Playlist
                        )
                    })
                    .map(row_id)
                    .collect()
            } else {
                Vec::new()
            };
            if shortcut_ids.is_empty() {
                HashMap::new()
            } else {
                self.item_counts
                    .get_linked_children_count_batch(&shortcut_ids)
                    .await?
            }
        };
        // Played/total leaf counts for the page's folders in one pass, so folder
        // UserData can carry UnplayedItemCount (C# AttachUserSpecificInfo folder branch).
        let played_counts = match user {
            Some(user) if options.enable_user_data => {
                let folder_ids: Vec<Uuid> = items
                    .iter()
                    .filter(|i| {
                        folder_emits_counts(i)
                            && !matches!(
                                row_kind(i),
                                BaseItemKind::CollectionFolder | BaseItemKind::UserView
                            )
                    })
                    .map(row_id)
                    .collect();
                if folder_ids.is_empty() {
                    HashMap::new()
                } else {
                    self.item_counts
                        .get_played_and_total_count_batch(&folder_ids, user)
                        .await?
                }
            }
            _ => HashMap::new(),
        };
        // Subtitle presence for the page's videos — C# emits `HasSubtitles` on
        // every video DTO regardless of `ItemFields`.
        let video_ids: Vec<Uuid> = items
            .iter()
            .filter(|i| kinds::is_video(row_kind(i)))
            .map(row_id)
            .collect();
        let has_subtitles: std::collections::HashSet<Uuid> = if video_ids.is_empty() {
            std::collections::HashSet::new()
        } else if want_streams {
            // The page's streams are already in hand (they cover every page id,
            // videos included), so the answer is a scan of what was read rather
            // than a second round trip — that ids-only query was the costliest
            // statement on the /Items/Suggestions page after the page query
            // itself (0.47 ms of a 3.8 ms request).
            video_ids
                .iter()
                .copied()
                .filter(|id| {
                    media_streams.get(id).is_some_and(|streams| {
                        streams.iter().any(|s| {
                            s.stream_type == ferrofin_model::entities::MediaStreamType::Subtitle
                        })
                    })
                })
                .collect()
        } else {
            // No stream fields requested, so nothing was bulk-loaded to read.
            self.media_sources
                .get_item_ids_with_subtitles(&video_ids)
                .await?
                .into_iter()
                .collect()
        };
        // Lyric presence for the page's audio — `dto.HasLyrics =
        // audio.GetMediaStreams().Any(s => s.Type == MediaStreamType.Lyric)`
        // (v10.11.8 Emby.Server.Implementations/Dto/DtoService.cs:308-311),
        // which is unconditional: `AudioBook : Audio` upstream, so the predicate
        // is `kinds::is_audio`. Same two-branch shape as `has_subtitles` — read
        // the streams already in hand when a stream field was asked for, and
        // fall back to the ids-only probe when none was.
        let audio_ids: Vec<Uuid> = items
            .iter()
            .filter(|i| kinds::is_audio(row_kind(i)))
            .map(row_id)
            .collect();
        let has_lyrics: std::collections::HashSet<Uuid> = if audio_ids.is_empty() {
            std::collections::HashSet::new()
        } else if want_streams {
            audio_ids
                .iter()
                .copied()
                .filter(|id| {
                    media_streams.get(id).is_some_and(|streams| {
                        streams.iter().any(|s| {
                            s.stream_type == ferrofin_model::entities::MediaStreamType::Lyric
                        })
                    })
                })
                .collect()
        } else {
            self.media_sources
                .get_item_ids_with_lyrics(&audio_ids)
                .await?
                .into_iter()
                .collect()
        };
        // One Permissions read gates the whole page's CanDelete/CanDownload
        // (C# `BaseItem.CanDelete(user)`/`CanDownload(user)` per item).
        let content_permissions = match user {
            Some(user)
                if options.contains_field(ItemFields::CanDelete)
                    || options.contains_field(ItemFields::CanDownload) =>
            {
                let user_id = parse_user_id(&user.id)?;
                self.user_data.get_content_permissions(user_id).await?.map(
                    |(can_delete, can_download)| UserContentPermissions {
                        can_delete,
                        can_download,
                    },
                )
            }
            _ => None,
        };
        Ok(Prefetched {
            images,
            user_data,
            media_streams,
            media_attachments,
            provider_ids,
            series_display_order,
            photo_album_names,
            series_provider_ids,
            series_studios,
            people,
            person_images,
            value_ids,
            clean_values,
            chapters,
            trickplay,
            child_counts,
            linked_child_counts,
            played_counts,
            alternates,
            has_subtitles,
            has_lyrics,
            content_permissions,
            person_ids_by_name,
            alt_referenced,
        })
    }

    /// Counts pre-supplied tagged items by kind onto a by-name DTO (port of the
    /// static `SetItemByNameInfo(item, dto, taggedItems)` overload). The kinds of
    /// the tagged items are read from their rows.
    async fn set_tagged_counts(
        &self,
        dto: &mut BaseItemDto,
        tagged_item_ids: &[Uuid],
    ) -> Result<(), ServiceError> {
        let mut kinds_vec = Vec::with_capacity(tagged_item_ids.len());
        for id in tagged_item_ids {
            if let Some(row) = self.library.get_item_by_id(*id).await? {
                kinds_vec.push(row_kind(&row));
            }
        }

        let count = |target: BaseItemKind| {
            i32::try_from(kinds_vec.iter().filter(|k| **k == target).count()).unwrap_or(i32::MAX)
        };

        dto.artist_count = Some(count(BaseItemKind::MusicArtist));
        dto.album_count = Some(count(BaseItemKind::MusicAlbum));
        dto.episode_count = Some(count(BaseItemKind::Episode));
        dto.movie_count = Some(count(BaseItemKind::Movie));
        dto.trailer_count = Some(count(BaseItemKind::Trailer));
        dto.music_video_count = Some(count(BaseItemKind::MusicVideo));
        dto.series_count = Some(count(BaseItemKind::Series));
        dto.program_count = Some(count(BaseItemKind::LiveTvProgram));
        dto.song_count = Some(count(BaseItemKind::Audio));
        dto.child_count = Some(i32::try_from(tagged_item_ids.len()).unwrap_or(i32::MAX));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::{DateTime, TimeZone as _, Utc};
    use ferrofin_db::entities::base_items::PeopleEntity;
    use ferrofin_db::entities::playback::TrickplayInfoEntity;
    use ferrofin_model::drawing::{ImageDimensions, ImageFormat};
    use ferrofin_model::dto::{MediaSourceInfo, UserItemDataDto};
    use ferrofin_model::entities_media::{ChapterInfo, MediaAttachment, MediaStream};
    use ferrofin_traits::drawing::ProcessedImage;
    use ferrofin_traits::dto::DtoService as _;

    use crate::test_support::{
        fetch_item, fetch_item_opt, image_info, seed_child_item, seed_folder_item, seed_images,
        seed_item_of_series, seed_item_with_data, seed_named_item, seed_provider_id,
        seed_series_with_studios, seed_user, test_db,
    };

    // ---- Fakes for the injected siblings -------------------------------------
    //
    // Each fake returns the empty/neutral value for every method the DTO paths
    // don't exercise, and a deterministic value for the few that matter.

    /// A [`LibraryManager`] fake: `get_people` returns a fixed list,
    /// `get_item_list` serves `named_items` by name (what the by-name lookup
    /// `get_named_item(s)` is built on), everything else is empty/neutral.
    #[derive(Default)]
    struct FakeLibrary {
        people: Vec<PeopleEntity>,
        named_items: Vec<BaseItemEntity>,
    }

    #[async_trait]
    impl LibraryManager for FakeLibrary {
        async fn get_item_by_id(&self, _id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
            Ok(None)
        }
        async fn get_item_images(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<ferrofin_traits::options::ItemImageInfo>, ServiceError> {
            Ok(vec![])
        }
        async fn query_items(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<ferrofin_model::querying::QueryResult<BaseItemEntity>, ServiceError> {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_item_ids(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<Vec<Uuid>, ServiceError> {
            Ok(vec![])
        }
        async fn get_item_list(
            &self,
            query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            // Only the by-name lookup is served (`get_named_item` filters
            // `get_item_list` by name). Match the way the real by-name resolver
            // does — on the CLEAN value (`LibraryManager::get_named_items`
            // compares `get_clean_value(name)` against the stored `CleanName`),
            // so accents, punctuation, spacing and case all fold exactly as they
            // do against a real database.
            let Some(name) = query.name.as_deref() else {
                return Ok(vec![]);
            };
            let want = crate::text_util::get_clean_value(name);
            Ok(self
                .named_items
                .iter()
                .filter(|row| {
                    row.name
                        .as_deref()
                        .is_some_and(|n| crate::text_util::get_clean_value(n) == want)
                })
                .cloned()
                .collect())
        }
        async fn get_latest_item_list(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
            _collection_type: ferrofin_model::data::CollectionType,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            Ok(vec![])
        }
        async fn create_items(
            &self,
            _items: &[BaseItemEntity],
            _parent_id: Option<Uuid>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn update_items(
            &self,
            _items: &[BaseItemEntity],
            _parent_id: Option<Uuid>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn delete_item(
            &self,
            _id: Uuid,
            _options: &ferrofin_traits::options::DeleteOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_people(
            &self,
            _query: &ferrofin_traits::options::InternalPeopleQuery,
        ) -> Result<Vec<PeopleEntity>, ServiceError> {
            Ok(self.people.clone())
        }
        async fn get_people_names(
            &self,
            _query: &ferrofin_traits::options::InternalPeopleQuery,
        ) -> Result<Vec<String>, ServiceError> {
            Ok(vec![])
        }
        async fn get_count(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<i32, ServiceError> {
            Ok(0)
        }
        async fn get_item_counts(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<ferrofin_model::dto::ItemCounts, ServiceError> {
            Ok(ferrofin_model::dto::ItemCounts::default())
        }
        async fn get_genres(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_studios(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_artists(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_music_genres(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_album_artists(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_query_filters_legacy(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<ferrofin_model::querying::QueryFiltersLegacy, ServiceError> {
            Ok(ferrofin_model::querying::QueryFiltersLegacy::default())
        }
        async fn get_media_stream_languages(
            &self,
            _stream_type: ferrofin_model::entities::MediaStreamType,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<Vec<String>, ServiceError> {
            Ok(Vec::new())
        }
        async fn queue_library_scan(&self) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// A [`UserDataManager`] fake returning a canned favourite DTO for any item.
    #[derive(Default)]
    struct FakeUserData;

    #[async_trait]
    impl UserDataManager for FakeUserData {
        async fn get_content_permissions(
            &self,
            _user_id: Uuid,
        ) -> Result<Option<(bool, bool)>, ServiceError> {
            // Deletion granted, downloading denied — asymmetric on purpose so a
            // test can prove each side gates independently.
            Ok(Some((true, false)))
        }
        async fn save_user_data(
            &self,
            _user_id: Uuid,
            _item_id: Uuid,
            _user_data: &ferrofin_model::dto::UpdateUserItemDataDto,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_user_data_dto(
            &self,
            item_id: Uuid,
            _user_id: Uuid,
        ) -> Result<Option<UserItemDataDto>, ServiceError> {
            Ok(Some(UserItemDataDto {
                rating: None,
                played_percentage: None,
                unplayed_item_count: None,
                playback_position_ticks: 0,
                play_count: 0,
                is_favorite: true,
                likes: None,
                last_played_date: None,
                played: false,
                key: item_id.simple().to_string(),
                item_id,
            }))
        }
        async fn get_user_data_batch(
            &self,
            _item_ids: &[Uuid],
            _user_id: Uuid,
        ) -> Result<std::collections::HashMap<Uuid, UserItemDataDto>, ServiceError> {
            Ok(std::collections::HashMap::new())
        }
        async fn update_play_state(
            &self,
            _user_id: Uuid,
            _item_id: Uuid,
            _reported_position_ticks: Option<i64>,
        ) -> Result<bool, ServiceError> {
            Ok(false)
        }
        async fn mark_played(
            &self,
            _user_id: Uuid,
            item_id: Uuid,
            _date_played: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<UserItemDataDto, ServiceError> {
            self.get_user_data_dto(item_id, _user_id)
                .await
                .map(|dto| dto.expect("fake always returns some"))
        }
        async fn mark_unplayed(
            &self,
            _user_id: Uuid,
            item_id: Uuid,
        ) -> Result<UserItemDataDto, ServiceError> {
            self.get_user_data_dto(item_id, _user_id)
                .await
                .map(|dto| dto.expect("fake always returns some"))
        }
        async fn reset_playback_stream_selections(
            &self,
            _user_id: Uuid,
            _item_id: Uuid,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    #[test]
    fn etag_matches_csharp_get_etag() {
        // `DateTime(2000,1,1, UTC).Ticks` is the .NET constant 630_822_816_000_000_000;
        // expected strings derived independently (python: MD5 of the UTF-16LE tick
        // string, bytes laid out as a .NET `Guid`, `"N"` format) — not via `get_md5`.
        let d = Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(compute_etag(Some(d)), "52fe91fd23a6cb2a7569d114180b4a38");
        // A never-saved row matches C#'s `DateTime.MinValue` (0 ticks).
        assert_eq!(compute_etag(None), "543b6ca4c9f21c87d81daf7a932499c0");
    }

    #[test]
    fn percent_of_ticks_is_the_played_percentage_division() {
        // 50% of a 2 h runtime — the C# `100 * position / runtime` double math.
        assert!((percent_of_ticks(36_000_000_000, 72_000_000_000) - 50.0).abs() < 1e-9);
        // Tiny fractions stay positive and precise enough for display.
        assert!(percent_of_ticks(1, 72_000_000_000) > 0.0);
    }

    /// A folder's `RecursiveItemCount` and its played numbers come from the SAME
    /// child count but through DIFFERENT gates, and Ferrofin emitted neither.
    ///
    /// `Folder.FillUserDataDtoValues` (v10.11.8
    /// MediaBrowser.Controller/Entities/Folder.cs:1798-1838) is behind
    /// `SupportsUserDataFromChildren` as a whole, then gates `RecursiveItemCount`
    /// on its FIELD alone and the played numbers on `SupportsPlayedStatus`.
    /// Measured on a real 10.11.8: a `MusicAlbum` carries `RecursiveItemCount`
    /// and no `PlayedPercentage`, because it overrides `SupportsPlayedStatus` to
    /// false while leaving `SupportsUserDataFromChildren` true. Collapsing the
    /// two gates into one is what would make that album wrong.
    #[tokio::test]
    async fn a_folder_carries_recursive_item_count_and_the_played_numbers_it_supports() {
        let db = test_db().await;
        let season_id = Uuid::new_v4();
        seed_folder_item(&db, season_id, BaseItemKind::Season, "Season 1", None).await;
        let album_id = Uuid::new_v4();
        seed_folder_item(&db, album_id, BaseItemKind::MusicAlbum, "Album 01", None).await;
        let user = seed_user(&db, Uuid::new_v4()).await;
        let season = fetch_item(&db, season_id).await;
        let album = fetch_item(&db, album_id).await;
        let svc = service(db);
        // `DtoOptions::default()` is all-fields, which is what
        // `GET /Items/{id}` and `SuggestionsController` both project with.
        let options = DtoOptions::default();

        // FakeCounts reports 4 leaf descendants, 1 played.
        let dto = svc
            .get_base_item_dto(&season, &options, Some(&user), None)
            .await
            .unwrap();
        assert_eq!(dto.recursive_item_count, Some(4));
        let ud = dto.user_data.as_ref().unwrap();
        assert_eq!(ud.unplayed_item_count, Some(3));
        // `100 - (unplayed / recursive) * 100`.
        assert!((ud.played_percentage.unwrap() - 25.0).abs() < 1e-9);
        assert!(!ud.played);

        // A MusicAlbum: counted, but never played-tracked.
        let album_dto = svc
            .get_base_item_dto(&album, &options, Some(&user), None)
            .await
            .unwrap();
        assert_eq!(album_dto.recursive_item_count, Some(4));
        // `dto.ChildCount = dto.RecursiveItemCount` for MusicAlbum/Season/
        // Playlist (DtoService.cs:473-480), which is NOT field-gated — a live
        // 10.11.8 answers `ChildCount: 3` for an album on a page that never
        // asked for one.
        assert_eq!(album_dto.child_count, Some(4));
        assert_eq!(
            album_dto
                .user_data
                .as_ref()
                .and_then(|ud| ud.played_percentage),
            None,
            "MusicAlbum overrides SupportsPlayedStatus to false"
        );

        // …and the field gate is real: no `RecursiveItemCount` field, no count.
        let narrow = DtoOptions {
            fields: vec![ItemFields::Path],
            ..DtoOptions::default()
        };
        let narrow_dto = svc
            .get_base_item_dto(&season, &narrow, Some(&user), None)
            .await
            .unwrap();
        assert_eq!(narrow_dto.recursive_item_count, None);
        // With no `RecursiveItemCount` on the DTO the C# takes its ELSE branch:
        // `Played = (UnplayedItemCount ?? 0) == 0`, and no percentage.
        let narrow_ud = narrow_dto.user_data.as_ref().unwrap();
        assert_eq!(narrow_ud.played_percentage, None);
        assert!(!narrow_ud.played);
    }

    /// `CumulativeRunTimeTicks` is the folder's own stored runtime, field-gated,
    /// and NOT under `EnableUserData` — `DtoService.cs:492-495` sits in the
    /// `item is Folder` branch. Ferrofin stored the value and never copied it.
    #[tokio::test]
    async fn a_folder_carries_its_cumulative_run_time_ticks() {
        let db = test_db().await;
        let album_id = Uuid::new_v4();
        seed_folder_item(&db, album_id, BaseItemKind::MusicAlbum, "Album 01", None).await;
        let mut album = fetch_item(&db, album_id).await;
        album.run_time_ticks = Some(60_000_000);
        let leaf_id = Uuid::new_v4();
        seed_named_item(&db, leaf_id, BaseItemKind::Movie, "A Movie").await;
        let mut leaf = fetch_item(&db, leaf_id).await;
        leaf.run_time_ticks = Some(60_000_000);
        let svc = service(db);

        let dto = svc
            .get_base_item_dto(&album, &DtoOptions::default(), None, None)
            .await
            .unwrap();
        assert_eq!(dto.cumulative_run_time_ticks, Some(60_000_000));
        // A leaf is not a folder, so it carries none however long it is.
        let leaf_dto = svc
            .get_base_item_dto(&leaf, &DtoOptions::default(), None, None)
            .await
            .unwrap();
        assert_eq!(leaf_dto.cumulative_run_time_ticks, None);
        // Field-gated.
        let narrow = DtoOptions {
            fields: vec![ItemFields::Path],
            ..DtoOptions::default()
        };
        let narrow_dto = svc
            .get_base_item_dto(&album, &narrow, None, None)
            .await
            .unwrap();
        assert_eq!(narrow_dto.cumulative_run_time_ticks, None);
    }

    /// `HasLyrics` is emitted on every `Audio` DTO, `false` included.
    ///
    /// `dto.HasLyrics = audio.GetMediaStreams().Any(s => s.Type ==
    /// MediaStreamType.Lyric)` (v10.11.8 DtoService.cs:308-311) — unconditional,
    /// outside the `ItemFields` system, and a plain `bool`. Ferrofin omitted the
    /// key, which is the null-where-Jellyfin-sends-non-null shape strict clients
    /// crash on.
    #[tokio::test]
    async fn an_audio_dto_always_carries_has_lyrics() {
        let db = test_db().await;
        let audio_id = Uuid::new_v4();
        seed_named_item(&db, audio_id, BaseItemKind::Audio, "Track 01").await;
        let movie_id = Uuid::new_v4();
        seed_named_item(&db, movie_id, BaseItemKind::Movie, "A Movie").await;
        let audio = fetch_item(&db, audio_id).await;
        let movie = fetch_item(&db, movie_id).await;
        let svc = service(db);

        // The fake's canned streams carry a video + a subtitle stream and no
        // lyric one, so the answer is a present `false`, not an absent key.
        let dto = svc
            .get_base_item_dto(&audio, &DtoOptions::default(), None, None)
            .await
            .unwrap();
        assert_eq!(dto.has_lyrics, Some(false));
        // …and only an Audio carries it at all.
        let movie_dto = svc
            .get_base_item_dto(&movie, &DtoOptions::default(), None, None)
            .await
            .unwrap();
        assert_eq!(movie_dto.has_lyrics, None);
    }

    /// `SeriesStudio` is `series.Studios.FirstOrDefault()` — a value on the
    /// SERIES row, which is why an implementation that only reads the projected
    /// item's own columns emits nothing (v10.11.8
    /// Emby.Server.Implementations/Dto/DtoService.cs:1228-1234 for an episode
    /// and :1256-1262 for a season, two identical field-gated blocks).
    #[tokio::test]
    async fn an_episode_and_a_season_carry_their_series_studio() {
        let db = test_db().await;
        let series_id = Uuid::new_v4();
        seed_series_with_studios(
            &db,
            series_id,
            "Series 01",
            "Ferrofin Studios|Second Studio",
        )
        .await;
        let season_id = Uuid::new_v4();
        seed_item_of_series(&db, season_id, BaseItemKind::Season, "Season 1", series_id).await;
        let episode_id = Uuid::new_v4();
        seed_item_of_series(&db, episode_id, BaseItemKind::Episode, "S01E01", series_id).await;
        let season = fetch_item(&db, season_id).await;
        let episode = fetch_item(&db, episode_id).await;
        let svc = service(db);

        for item in [&season, &episode] {
            let dto = svc
                .get_base_item_dto(item, &DtoOptions::default(), None, None)
                .await
                .unwrap();
            assert_eq!(
                dto.series_studio.as_deref(),
                Some("Ferrofin Studios"),
                "FirstOrDefault() over the series' studio list"
            );
        }

        // Field-gated: no `SeriesStudio` field, no value — and no query for one.
        let narrow = DtoOptions {
            fields: vec![ItemFields::Path],
            ..DtoOptions::default()
        };
        let narrow_dto = svc
            .get_base_item_dto(&episode, &narrow, None, None)
            .await
            .unwrap();
        assert_eq!(narrow_dto.series_studio, None);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // a flat sequence of per-kind assertions
    async fn folder_user_data_carries_unplayed_item_count() {
        let db = test_db().await;
        let folder_id = Uuid::new_v4();
        seed_folder_item(&db, folder_id, BaseItemKind::Season, "Season 1", None).await;
        let leaf_id = Uuid::new_v4();
        seed_named_item(&db, leaf_id, BaseItemKind::Movie, "A Movie").await;
        // A by-name row (Genre) stored IsFolder=1 but with no ancestor closure.
        let genre_id = Uuid::new_v4();
        seed_folder_item(&db, genre_id, BaseItemKind::Genre, "Drama", None).await;
        // Two MusicArtist rows, both IsFolder=1: accessed-by-name (no parent —
        // C# `IsFolder` false) and a physical artist folder (parented — C#
        // `IsFolder` true).
        let byname_artist_id = Uuid::new_v4();
        seed_folder_item(
            &db,
            byname_artist_id,
            BaseItemKind::MusicArtist,
            "ByName",
            None,
        )
        .await;
        let physical_artist_id = Uuid::new_v4();
        seed_folder_item(
            &db,
            physical_artist_id,
            BaseItemKind::MusicArtist,
            "OnDisk",
            Some(folder_id),
        )
        .await;
        let user = seed_user(&db, Uuid::new_v4()).await;
        let folder = fetch_item(&db, folder_id).await;
        let leaf = fetch_item(&db, leaf_id).await;
        let genre = fetch_item(&db, genre_id).await;
        let byname_artist = fetch_item(&db, byname_artist_id).await;
        let physical_artist = fetch_item(&db, physical_artist_id).await;
        let db2 = db.clone();
        let svc = service(db);
        let options = DtoOptions::default(); // enables user data

        // FakeCounts reports 1/4 leaf descendants played → UnplayedItemCount = 3,
        // on both the single-item and the batch (prefetched) path.
        let single = svc
            .get_base_item_dto(&folder, &options, Some(&user), None)
            .await
            .unwrap();
        assert_eq!(
            single.user_data.as_ref().unwrap().unplayed_item_count,
            Some(3)
        );
        let batch = svc
            .get_base_item_dtos(
                std::slice::from_ref(&folder),
                &options,
                Some(&user),
                None,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            batch[0].user_data.as_ref().unwrap().unplayed_item_count,
            Some(3)
        );

        // A leaf (non-folder) item never carries UnplayedItemCount.
        let leaf_dto = svc
            .get_base_item_dto(&leaf, &options, Some(&user), None)
            .await
            .unwrap();
        assert_eq!(
            leaf_dto.user_data.as_ref().unwrap().unplayed_item_count,
            None
        );

        // A by-name row (Genre) is stored IsFolder=1 but has no ancestor closure,
        // so it must NOT carry UnplayedItemCount on either path — Jellyfin, where
        // by-name items are `BaseItem`+`IItemByName`, never emits it.
        let genre_single = svc
            .get_base_item_dto(&genre, &options, Some(&user), None)
            .await
            .unwrap();
        assert_eq!(
            genre_single.user_data.as_ref().unwrap().unplayed_item_count,
            None
        );
        let genre_batch = svc
            .get_base_item_dtos(
                std::slice::from_ref(&genre),
                &options,
                Some(&user),
                None,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            genre_batch[0]
                .user_data
                .as_ref()
                .unwrap()
                .unplayed_item_count,
            None
        );

        // A by-name MusicArtist (no parent) is not a folder at runtime in C#
        // (`MusicArtist.IsFolder => !IsAccessedByName`) — no count on either path.
        let byname_single = svc
            .get_base_item_dto(&byname_artist, &options, Some(&user), None)
            .await
            .unwrap();
        assert_eq!(
            byname_single
                .user_data
                .as_ref()
                .unwrap()
                .unplayed_item_count,
            None
        );
        let byname_batch = svc
            .get_base_item_dtos(
                std::slice::from_ref(&byname_artist),
                &options,
                Some(&user),
                None,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            byname_batch[0]
                .user_data
                .as_ref()
                .unwrap()
                .unplayed_item_count,
            None
        );

        // A physically-parented MusicArtist IS a folder at runtime, but
        // `MusicArtist.SupportsPlayedStatus => false` (MusicArtist.cs:48) is
        // unconditional, and `Folder.FillUserDataDtoValues` (Folder.cs:1973)
        // gates the count on it — so it carries none either. Verified against a
        // live 10.11.8 whose three physically-parented artists all come back
        // with no `UnplayedItemCount`.
        let physical_single = svc
            .get_base_item_dto(&physical_artist, &options, Some(&user), None)
            .await
            .unwrap();
        assert_eq!(
            physical_single
                .user_data
                .as_ref()
                .unwrap()
                .unplayed_item_count,
            None
        );
        let physical_batch = svc
            .get_base_item_dtos(
                std::slice::from_ref(&physical_artist),
                &options,
                Some(&user),
                None,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            physical_batch[0]
                .user_data
                .as_ref()
                .unwrap()
                .unplayed_item_count,
            None
        );

        // The library containers that override `SupportsPlayedStatus` to false
        // (CollectionFolder.cs:74, UserRootFolder.cs:39, UserView.cs:66,
        // AggregateFolder.cs:50, MusicAlbum.cs:51, PhotoAlbum.cs:14) carry no
        // count either, however many played descendants they have.
        for kind in [
            BaseItemKind::CollectionFolder,
            BaseItemKind::UserRootFolder,
            BaseItemKind::UserView,
            BaseItemKind::AggregateFolder,
            BaseItemKind::MusicAlbum,
            BaseItemKind::PhotoAlbum,
        ] {
            let id = Uuid::new_v4();
            seed_folder_item(&db2, id, kind, "Container", Some(folder_id)).await;
            let row = fetch_item(&db2, id).await;
            let dto = svc
                .get_base_item_dto(&row, &options, Some(&user), None)
                .await
                .unwrap();
            assert_eq!(
                dto.user_data.as_ref().unwrap().unplayed_item_count,
                None,
                "{kind:?}"
            );
        }
    }

    /// An [`ItemCountService`] fake returning fixed name-item counts.
    ///
    /// `linked` is how many LINKED children every parent reports
    /// (`Folder.LinkedChildren.Length`). It defaults to ZERO because that is
    /// what a real `MusicAlbum`/`Season` has — upstream's own comment on the
    /// `> 0` test is "the default is an empty array, so we can't reliably use
    /// the count when it's empty" — and a fake that reported entries for every
    /// folder would hide the guard.
    #[derive(Default)]
    struct FakeCounts {
        linked: i32,
    }

    #[async_trait]
    impl ItemCountService for FakeCounts {
        async fn get_count(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<i32, ServiceError> {
            Ok(0)
        }
        async fn get_item_counts(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<ferrofin_model::dto::ItemCounts, ServiceError> {
            Ok(ferrofin_model::dto::ItemCounts::default())
        }
        async fn get_item_counts_for_name_item(
            &self,
            _kind: BaseItemKind,
            _id: Uuid,
            _related_item_kinds: &[BaseItemKind],
            _access_filter: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<ferrofin_model::dto::ItemCounts, ServiceError> {
            Ok(ferrofin_model::dto::ItemCounts {
                movie_count: 3,
                series_count: 2,
                ..ferrofin_model::dto::ItemCounts::default()
            })
        }
        async fn get_played_count(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
            _ancestor_id: Uuid,
        ) -> Result<i32, ServiceError> {
            Ok(0)
        }
        async fn get_total_count(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
            _ancestor_id: Uuid,
        ) -> Result<i32, ServiceError> {
            Ok(0)
        }
        async fn get_played_and_total_count(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
            _ancestor_id: Uuid,
        ) -> Result<ferrofin_traits::persistence::PlayedAndTotal, ServiceError> {
            Ok(ferrofin_traits::persistence::PlayedAndTotal::default())
        }
        async fn get_played_and_total_count_from_linked_children(
            &self,
            _filter: &ferrofin_traits::options::InternalItemsQuery,
            _parent_id: Uuid,
        ) -> Result<ferrofin_traits::persistence::PlayedAndTotal, ServiceError> {
            Ok(ferrofin_traits::persistence::PlayedAndTotal::default())
        }
        async fn get_played_and_total_count_batch(
            &self,
            folder_ids: &[Uuid],
            _user: &UserEntity,
        ) -> Result<HashMap<Uuid, ferrofin_traits::persistence::PlayedAndTotal>, ServiceError>
        {
            // Every folder reports 1 of 4 leaf descendants played → 3 unplayed.
            Ok(folder_ids
                .iter()
                .map(|&f| {
                    (
                        f,
                        ferrofin_traits::persistence::PlayedAndTotal {
                            played: 1,
                            total: 4,
                        },
                    )
                })
                .collect())
        }
        async fn get_linked_children_count_batch(
            &self,
            parent_ids: &[Uuid],
        ) -> Result<HashMap<Uuid, i32>, ServiceError> {
            // A parent with no linked children is ABSENT from the map, exactly
            // as the real service leaves it.
            if self.linked == 0 {
                return Ok(HashMap::new());
            }
            Ok(parent_ids.iter().map(|&p| (p, self.linked)).collect())
        }
        async fn get_child_count_batch(
            &self,
            parent_ids: &[Uuid],
            _user_id: Option<Uuid>,
        ) -> Result<HashMap<Uuid, i32>, ServiceError> {
            // Every requested parent reports a fixed 4 children.
            Ok(parent_ids.iter().map(|&p| (p, 4)).collect())
        }
    }

    /// An [`ImageProcessor`] fake: a deterministic cache tag per path, a fixed
    /// 2:1 dimension.
    #[derive(Default)]
    struct FakeImages;

    #[async_trait]
    impl ImageProcessor for FakeImages {
        fn supported_input_formats(&self) -> Vec<String> {
            vec![]
        }
        fn supports_image_collage_creation(&self) -> bool {
            false
        }
        fn supported_image_output_formats(&self) -> Vec<ImageFormat> {
            vec![]
        }
        async fn get_image_dimensions(&self, _path: &str) -> Result<ImageDimensions, ServiceError> {
            Ok(ImageDimensions {
                width: 400,
                height: 200,
            })
        }
        async fn get_item_image_dimensions(
            &self,
            _item_id: Uuid,
            _info: &ItemImageInfo,
        ) -> Result<ImageDimensions, ServiceError> {
            Ok(ImageDimensions {
                width: 400,
                height: 200,
            })
        }
        async fn get_image_blur_hash(&self, _path: &str) -> Result<String, ServiceError> {
            Ok("blur".into())
        }
        async fn get_image_blur_hash_sized(
            &self,
            _path: &str,
            _image_dimensions: ImageDimensions,
        ) -> Result<String, ServiceError> {
            Ok("blur".into())
        }
        async fn get_image_cache_tag(
            &self,
            _item_id: Uuid,
            image: &ItemImageInfo,
        ) -> Result<Option<String>, ServiceError> {
            Ok(Some(format!("tag:{}", image.path)))
        }
        async fn get_image_cache_tag_for_path(
            &self,
            _base_item_path: &str,
            _image_date_modified: DateTime<Utc>,
        ) -> Result<Option<String>, ServiceError> {
            Ok(None)
        }
        async fn process_image(
            &self,
            _options: &ferrofin_traits::options::ImageProcessingOptions,
        ) -> Result<ProcessedImage, ServiceError> {
            Err(ServiceError::NotFound("process_image".into()))
        }
        async fn create_image_collage(
            &self,
            _options: &ferrofin_traits::options::ImageCollageOptions,
            _library_name: Option<&str>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// A [`MediaSourceManager`] fake — canned streams and one alternate version.
    ///
    /// `get_item_ids_with_subtitles` deliberately reports EVERY id as subtitled
    /// while the stream batch is per-id: a projection that answered
    /// `HasSubtitles` from that query instead of from the streams it already
    /// read would mark `without_subtitles` items subtitled, and the tests below
    /// would catch it.
    #[derive(Default)]
    struct FakeSources {
        /// Ids whose canned stream list carries no subtitle stream.
        without_subtitles: std::collections::HashSet<Uuid>,
    }

    #[async_trait]
    impl MediaSourceManager for FakeSources {
        async fn get_item_ids_with_subtitles(
            &self,
            item_ids: &[Uuid],
        ) -> Result<Vec<Uuid>, ServiceError> {
            // Every video in these fixtures "has subtitles", so the DTO's
            // HasSubtitles emit path is exercised.
            Ok(item_ids.to_vec())
        }
        async fn get_media_streams(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<MediaStream>, ServiceError> {
            Ok(vec![])
        }
        async fn get_media_streams_batch(
            &self,
            item_ids: &[Uuid],
        ) -> Result<HashMap<Uuid, Vec<MediaStream>>, ServiceError> {
            // Non-empty so the prefetched `media_streams` map is actually
            // populated in these tests: it is read TWICE per video DTO
            // (MediaSources, then the MediaStreams field) plus once per merged
            // alternate keyed by the alternate's id, which is why it may only
            // be drained at its last read. An empty map would hide a regression there.
            Ok(item_ids
                .iter()
                .map(|id| {
                    let mut streams = vec![MediaStream {
                        index: 0,
                        stream_type: ferrofin_model::entities::MediaStreamType::Video,
                        codec: Some("h264".to_owned()),
                        ..MediaStream::default()
                    }];
                    if !self.without_subtitles.contains(id) {
                        streams.push(MediaStream {
                            index: 1,
                            stream_type: ferrofin_model::entities::MediaStreamType::Subtitle,
                            codec: Some("subrip".to_owned()),
                            ..MediaStream::default()
                        });
                    }
                    (*id, streams)
                })
                .collect())
        }
        async fn get_media_attachments(
            &self,
            item_id: Uuid,
        ) -> Result<Vec<MediaAttachment>, ServiceError> {
            // One canned font per item, tagged with the item it belongs to, so a
            // projection that handed a primary's attachments to its alternate (or
            // vice versa) would be caught.
            Ok(vec![MediaAttachment {
                index: 3,
                codec: Some("ttf".to_owned()),
                file_name: Some(format!("{}.ttf", item_id.simple())),
                ..MediaAttachment::default()
            }])
        }
        async fn get_playback_media_sources(
            &self,
            _item_id: Uuid,
            _user_id: Uuid,
            _allow_media_probe: bool,
            _enable_path_substitution: bool,
        ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
            Ok(vec![])
        }
        async fn get_static_media_sources(
            &self,
            _item_id: Uuid,
            _enable_path_substitution: bool,
            _user_id: Option<Uuid>,
        ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
            Ok(vec![])
        }
        async fn get_alternate_versions_batch(
            &self,
            primary_ids: &[Uuid],
        ) -> Result<HashMap<Uuid, Vec<BaseItemEntity>>, ServiceError> {
            // Every requested primary reports one canned alternate version.
            Ok(primary_ids
                .iter()
                .map(|&id| {
                    (
                        id,
                        vec![BaseItemEntity {
                            id: Uuid::from_u128(0xA17).to_string(),
                            name: Some("Alt Cut".to_owned()),
                            path: Some("/media/alt.mkv".to_owned()),
                            media_type: Some("Video".to_owned()),
                            primary_version_id: Some(id.to_string()),
                            ..Default::default()
                        }],
                    )
                })
                .collect())
        }
        async fn open_live_stream(
            &self,
            _request: &ferrofin_model::media_info::LiveStreamRequest,
        ) -> Result<MediaSourceInfo, ServiceError> {
            Err(ServiceError::NotFound("open_live_stream".into()))
        }
        async fn get_live_stream(&self, _id: &str) -> Result<MediaSourceInfo, ServiceError> {
            Err(ServiceError::NotFound("get_live_stream".into()))
        }
        async fn close_live_stream(&self, _id: &str) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn refresh_media_streams(&self, _item_id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// A [`ChapterManager`] fake — no chapters.
    #[derive(Default)]
    struct FakeChapters;

    #[async_trait]
    impl ChapterManager for FakeChapters {
        async fn supports(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
            Ok(false)
        }
        async fn save_chapters(
            &self,
            _item_id: Uuid,
            _chapters: &[ChapterInfo],
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_chapter(
            &self,
            _item_id: Uuid,
            _index: i32,
        ) -> Result<Option<ChapterInfo>, ServiceError> {
            Ok(None)
        }
        async fn get_chapters(&self, _item_id: Uuid) -> Result<Vec<ChapterInfo>, ServiceError> {
            Ok(vec![])
        }
        async fn delete_chapter_data(&self, _item_id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// A [`ChapterManager`] fake that records the id sets it is asked for, so a
    /// test can assert which of a page's items the prefetch decided could own
    /// chapters at all. A fake that only returned rows could not see the
    /// difference between "asked and got nothing" and "never asked".
    #[derive(Default)]
    struct RecordingChapters {
        batches: std::sync::Mutex<Vec<Vec<Uuid>>>,
    }

    #[async_trait]
    impl ChapterManager for RecordingChapters {
        async fn supports(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn save_chapters(
            &self,
            _item_id: Uuid,
            _chapters: &[ChapterInfo],
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_chapter(
            &self,
            _item_id: Uuid,
            _index: i32,
        ) -> Result<Option<ChapterInfo>, ServiceError> {
            Ok(None)
        }
        async fn get_chapters(&self, _item_id: Uuid) -> Result<Vec<ChapterInfo>, ServiceError> {
            Ok(vec![])
        }
        async fn get_chapters_batch(
            &self,
            item_ids: &[Uuid],
        ) -> Result<HashMap<Uuid, Vec<ChapterInfo>>, ServiceError> {
            if let Ok(mut batches) = self.batches.lock() {
                batches.push(item_ids.to_vec());
            }
            Ok(HashMap::new())
        }
        async fn delete_chapter_data(&self, _item_id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// A [`ChapterManager`] fake with one thumbnailed and one bare chapter.
    struct ChaptersWithImages;

    #[async_trait]
    impl ChapterManager for ChaptersWithImages {
        async fn supports(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn save_chapters(
            &self,
            _item_id: Uuid,
            _chapters: &[ChapterInfo],
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_chapter(
            &self,
            _item_id: Uuid,
            _index: i32,
        ) -> Result<Option<ChapterInfo>, ServiceError> {
            Ok(None)
        }
        async fn get_chapters(&self, _item_id: Uuid) -> Result<Vec<ChapterInfo>, ServiceError> {
            Ok(vec![
                ChapterInfo {
                    start_position_ticks: 0,
                    name: Some("Opening".to_owned()),
                    image_path: Some("/meta/chapters/0.jpg".to_owned()),
                    ..ChapterInfo::default()
                },
                ChapterInfo {
                    start_position_ticks: 100_000_000,
                    name: Some("No thumbnail".to_owned()),
                    ..ChapterInfo::default()
                },
            ])
        }
        async fn delete_chapter_data(&self, _item_id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// A [`TrickplayManager`] fake — one canned 1080/320 manifest per item.
    #[derive(Default)]
    struct FakeTrickplay;

    #[async_trait]
    impl TrickplayManager for FakeTrickplay {
        async fn refresh_trickplay_data(
            &self,
            _item_id: Uuid,
            _replace: bool,
            _library_options: &ferrofin_model::configuration::LibraryOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_trickplay_resolutions(
            &self,
            _item_id: Uuid,
        ) -> Result<HashMap<i32, TrickplayInfoEntity>, ServiceError> {
            Ok(HashMap::new())
        }
        async fn get_trickplay_items(
            &self,
            _limit: i32,
            _offset: i32,
        ) -> Result<Vec<TrickplayInfoEntity>, ServiceError> {
            Ok(vec![])
        }
        async fn save_trickplay_info(
            &self,
            _info: &TrickplayInfoEntity,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn delete_trickplay_data(&self, _item_id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_trickplay_manifest(
            &self,
            item_id: Uuid,
        ) -> Result<HashMap<String, HashMap<i32, TrickplayInfoEntity>>, ServiceError> {
            // Non-empty: against an EMPTY map `.remove()` and `.get().cloned()`
            // are indistinguishable, so an empty manifest would let a wrong
            // `repeated` flag on the trickplay read pass unnoticed.
            Ok(HashMap::from([(
                "1080".to_owned(),
                HashMap::from([(
                    320,
                    TrickplayInfoEntity {
                        item_id: item_id.to_string(),
                        width: 320,
                        height: 180,
                        tile_width: 10,
                        tile_height: 10,
                        thumbnail_count: 100,
                        interval: 10000,
                        bandwidth: 1000,
                    },
                )]),
            )]))
        }
        async fn get_hls_playlist(
            &self,
            _item_id: Uuid,
            _width: i32,
            _api_key: Option<&str>,
        ) -> Result<Option<String>, ServiceError> {
            Ok(None)
        }
        async fn get_trickplay_tile_path(
            &self,
            _item_id: Uuid,
            _width: i32,
            _index: i32,
        ) -> Result<Option<String>, ServiceError> {
            Ok(None)
        }
    }

    /// Builds a DTO service over `db` wired to the fakes, with an optional custom
    /// library fake (for the people test).
    fn service_with(db: Database, library: Arc<dyn LibraryManager>) -> FerrofinDtoService {
        FerrofinDtoService::new(
            db,
            "server-1".into(),
            library,
            Arc::new(FakeUserData),
            Arc::new(FakeCounts::default()),
            Arc::new(FakeImages),
            Arc::new(FakeSources::default()),
            Arc::new(FakeChapters),
            Arc::new(FakeTrickplay),
        )
    }

    /// [`service`] with a chapter manager that has thumbnailed chapters.
    fn service_with_chapters(db: Database) -> FerrofinDtoService {
        FerrofinDtoService::new(
            db,
            "server-1".into(),
            Arc::new(FakeLibrary::default()),
            Arc::new(FakeUserData),
            Arc::new(FakeCounts::default()),
            Arc::new(FakeImages),
            Arc::new(FakeSources::default()),
            Arc::new(ChaptersWithImages),
            Arc::new(FakeTrickplay),
        )
    }

    fn service(db: Database) -> FerrofinDtoService {
        service_with(db, Arc::new(FakeLibrary::default()))
    }

    /// [`service`] whose count service reports `linked` linked children for
    /// every parent — the `Folder.LinkedChildren.Length` half of upstream's
    /// ChildCount shortcut.
    fn service_with_linked_children(db: Database, linked: i32) -> FerrofinDtoService {
        FerrofinDtoService::new(
            db,
            "server-1".into(),
            Arc::new(FakeLibrary::default()),
            Arc::new(FakeUserData),
            Arc::new(FakeCounts { linked }),
            Arc::new(FakeImages),
            Arc::new(FakeSources::default()),
            Arc::new(FakeChapters),
            Arc::new(FakeTrickplay),
        )
    }

    /// [`service`] with a media-source fake whose canned streams the caller
    /// controls.
    fn service_with_sources(db: Database, sources: FakeSources) -> FerrofinDtoService {
        FerrofinDtoService::new(
            db,
            "server-1".into(),
            Arc::new(FakeLibrary::default()),
            Arc::new(FakeUserData),
            Arc::new(FakeCounts::default()),
            Arc::new(FakeImages),
            Arc::new(sources),
            Arc::new(FakeChapters),
            Arc::new(FakeTrickplay),
        )
    }

    // A folder or a by-name item owns no chapter, stream, trickplay or
    // alternate-version row, so an all-fields page of them must not spend four
    // round trips proving it. Upstream never asks — a C# `Folder` has no
    // streams in memory — and `/Library/MediaFolders`, `/Persons` and
    // `/Items/{id}/Ancestors` are exactly such pages.
    #[tokio::test]
    async fn a_series_dto_carries_an_empty_air_days_array_never_null() {
        // C# v10.11.8 `DtoService.cs:1243` `dto.AirDays = series.AirDays`, and
        // v10.11.8 `Series.cs:31` initialises `AirDays = Array.Empty<DayOfWeek>()` — a
        // non-nullable array 10.11.8 never persists, so every Series DTO
        // serializes `"AirDays": []`. Ferrofin omitted the key entirely, the
        // null-where-Jellyfin-sends-non-null shape strict clients crash on.
        let db = test_db().await;
        let series = Uuid::new_v4();
        let movie = Uuid::new_v4();
        seed_named_item(&db, series, BaseItemKind::Series, "Firefly").await;
        seed_named_item(&db, movie, BaseItemKind::Movie, "Serenity").await;
        let svc = FerrofinDtoService::new(
            db.clone(),
            "server-1".into(),
            Arc::new(FakeLibrary::default()),
            Arc::new(FakeUserData),
            Arc::new(FakeCounts::default()),
            Arc::new(FakeImages),
            Arc::new(FakeSources::default()),
            Arc::new(FakeChapters),
            Arc::new(FakeTrickplay),
        );
        let rows = vec![fetch_item(&db, series).await, fetch_item(&db, movie).await];
        let dtos = svc
            .get_base_item_dtos(&rows, &DtoOptions::default(), None, None, true)
            .await
            .unwrap();
        assert_eq!(dtos[0].air_days.as_deref(), Some(&[][..]));
        // Only a Series carries it — C# guards on `item is Series tmp`.
        assert_eq!(dtos[1].air_days, None);
    }
    /// `DisplayPreferencesId` keys display prefs by TYPE, not by item.
    ///
    /// Port of `BaseItem.DisplayPreferencesId` (v10.11.8
    /// MediaBrowser.Controller/Entities/BaseItem.cs:243-251):
    /// `thisType == typeof(Folder) ? Id : thisType.FullName.GetMD5()`, with
    /// `CollectionFolder` overriding back to `Id` (CollectionFolder.cs:55).
    /// The expected hashes were reproduced against a live Jellyfin 10.11.8.
    #[tokio::test]
    async fn display_preferences_are_keyed_by_type_not_by_item() {
        let db = test_db().await;
        let movie = Uuid::new_v4();
        let other_movie = Uuid::new_v4();
        let library = Uuid::new_v4();
        seed_named_item(&db, movie, BaseItemKind::Movie, "Solaris").await;
        seed_named_item(&db, other_movie, BaseItemKind::Movie, "Stalker").await;
        seed_named_item(&db, library, BaseItemKind::CollectionFolder, "Movies").await;
        let rows = vec![
            fetch_item(&db, movie).await,
            fetch_item(&db, other_movie).await,
            fetch_item(&db, library).await,
        ];

        let svc = FerrofinDtoService::new(
            db,
            "server-1".into(),
            Arc::new(FakeLibrary::default()),
            Arc::new(FakeUserData),
            Arc::new(FakeCounts::default()),
            Arc::new(FakeImages),
            Arc::new(FakeSources::default()),
            Arc::new(RecordingChapters::default()) as Arc<dyn ChapterManager>,
            Arc::new(FakeTrickplay),
        );
        let options = DtoOptions {
            fields: vec![ItemFields::DisplayPreferencesId],
            ..DtoOptions::default()
        };
        let dtos = svc
            .get_base_item_dtos(&rows, &options, None, None, true)
            .await
            .expect("dtos");

        // Two different films share one key — that is the whole point of the
        // type-keyed rule, and what makes a view's chosen sort stick.
        assert_eq!(
            dtos[0].display_preferences_id.as_deref(),
            Some("dbf7709c41faaa746463d67978eb863d"),
            "MD5(UTF-16LE(\"MediaBrowser.Controller.Entities.Movies.Movie\")) as a .NET Guid"
        );
        assert_eq!(
            dtos[1].display_preferences_id,
            dtos[0].display_preferences_id
        );
        // A CollectionFolder overrides back to its own id.
        assert_eq!(
            dtos[2].display_preferences_id.as_deref(),
            Some(library.simple().to_string().as_str())
        );
    }

    #[tokio::test]
    async fn a_page_that_cannot_own_media_sources_is_never_asked_for_them() {
        let db = test_db().await;
        let folder = Uuid::new_v4();
        let person = Uuid::new_v4();
        let movie = Uuid::new_v4();
        seed_named_item(&db, folder, BaseItemKind::CollectionFolder, "Movies").await;
        seed_named_item(&db, person, BaseItemKind::Person, "Ada").await;
        seed_named_item(&db, movie, BaseItemKind::Movie, "Solaris").await;
        let folder_row = fetch_item(&db, folder).await;
        let person_row = fetch_item(&db, person).await;
        let movie_row = fetch_item(&db, movie).await;

        let chapters = Arc::new(RecordingChapters::default());
        let svc = FerrofinDtoService::new(
            db,
            "server-1".into(),
            Arc::new(FakeLibrary::default()),
            Arc::new(FakeUserData),
            Arc::new(FakeCounts::default()),
            Arc::new(FakeImages),
            Arc::new(FakeSources::default()),
            Arc::clone(&chapters) as Arc<dyn ChapterManager>,
            Arc::new(FakeTrickplay),
        );

        svc.get_base_item_dtos(
            &[folder_row.clone(), person_row.clone()],
            &DtoOptions::default(),
            None,
            None,
            true,
        )
        .await
        .unwrap();
        assert!(
            chapters.batches.lock().unwrap().is_empty(),
            "a page of only folders/people must not query chapters at all"
        );

        // A mixed page still asks — but only for the rows that can answer.
        svc.get_base_item_dtos(
            &[folder_row, movie_row, person_row],
            &DtoOptions::default(),
            None,
            None,
            true,
        )
        .await
        .unwrap();
        let batches = chapters.batches.lock().unwrap().clone();
        assert_eq!(
            batches,
            vec![vec![movie]],
            "only the movie can own chapters"
        );
    }

    // Clients gate the chapter-thumbnail request on `ImageTag`; without it the
    // extracted images are never fetched, however well the extraction ran.
    #[tokio::test]
    async fn chapter_dtos_carry_an_image_tag_when_a_thumbnail_exists() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Movie, "Chaptered").await;
        let item = fetch_item(&db, id).await;
        let svc = service_with_chapters(db);

        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), None, None)
            .await
            .unwrap();
        let chapters = dto.chapters.expect("chapters requested by default");
        assert_eq!(chapters.len(), 2);
        assert_eq!(
            chapters[0].image_tag.as_deref(),
            Some("tag:/meta/chapters/0.jpg")
        );
        // A chapter with no extracted image carries no tag.
        assert_eq!(chapters[1].image_tag, None);
    }

    #[tokio::test]
    async fn remote_trailers_come_from_the_data_blob() {
        // jellyfin-web's Trailer button is gated on RemoteTrailers.length; the
        // scan writes them into `Data` (Jellyfin's only home for them).
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Movie, "Solaris").await;
        let mut item = fetch_item(&db, id).await;
        item.data = Some(
            r#"{"RemoteTrailers":[{"Url":"https://www.youtube.com/watch?v=abc","Name":"Trailer"}]}"#
                .to_owned(),
        );
        let svc = service(db);

        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), None, None)
            .await
            .unwrap();
        let trailers = dto.remote_trailers.expect("field requested by default");
        assert_eq!(trailers.len(), 1);
        assert_eq!(
            trailers[0].url.as_deref(),
            Some("https://www.youtube.com/watch?v=abc")
        );
        assert_eq!(trailers[0].name.as_deref(), Some("Trailer"));

        // Not requested → the field stays absent.
        let no_fields = DtoOptions {
            fields: vec![],
            ..DtoOptions::default()
        };
        let bare = svc
            .get_base_item_dto(&item, &no_fields, None, None)
            .await
            .unwrap();
        assert!(bare.remote_trailers.is_none());
    }

    #[tokio::test]
    async fn maps_core_scalar_fields() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Movie, "Inception").await;
        sqlx::query(
            r#"UPDATE "BaseItems" SET "ProductionYear" = 2010, "RunTimeTicks" = 88_000_000,
               "Overview" = 'A thief', "OfficialRating" = 'PG-13' WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id))
        .execute(db.writer())
        .await
        .unwrap();

        let item = fetch_item(&db, id).await;
        let svc = service(db);
        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), None, None)
            .await
            .unwrap();

        assert_eq!(dto.id, id);
        assert_eq!(dto.name.as_deref(), Some("Inception"));
        assert_eq!(dto.type_, BaseItemKind::Movie);
        assert_eq!(dto.production_year, Some(2010));
        assert_eq!(dto.run_time_ticks, Some(88_000_000));
        assert_eq!(dto.overview.as_deref(), Some("A thief"));
        assert_eq!(dto.official_rating.as_deref(), Some("PG-13"));
        assert_eq!(dto.server_id.as_deref(), Some("server-1"));
    }

    #[tokio::test]
    async fn honors_field_toggles() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Movie, "Inception").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "Overview" = 'A thief' WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .execute(db.writer())
            .await
            .unwrap();
        let item = fetch_item(&db, id).await;
        let svc = service(db);

        // Overview omitted when its field is not requested.
        let options = DtoOptions::with_all_fields(false);
        let dto = svc
            .get_base_item_dto(&item, &options, None, None)
            .await
            .unwrap();
        assert!(dto.overview.is_none());
        // Name is always mapped (it has no gating field).
        assert_eq!(dto.name.as_deref(), Some("Inception"));
    }

    #[tokio::test]
    async fn maps_genres_and_tags_from_pipe_columns() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Movie, "M").await;
        sqlx::query(
            r#"UPDATE "BaseItems" SET "Genres" = 'Action|Sci-Fi', "Tags" = 'imax|4k'
               WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id))
        .execute(db.writer())
        .await
        .unwrap();
        let item = fetch_item(&db, id).await;
        let svc = service(db);
        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), None, None)
            .await
            .unwrap();

        assert_eq!(
            dto.genres,
            Some(vec!["Action".to_owned(), "Sci-Fi".to_owned()])
        );
        assert_eq!(dto.genre_items.as_ref().unwrap().len(), 2);
        assert_eq!(dto.tags, Some(vec!["imax".to_owned(), "4k".to_owned()]));
    }

    #[tokio::test]
    async fn video_dto_emits_has_subtitles_and_policy_gated_can_flags() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Movie, "Subbed").await;
        let item = fetch_item(&db, id).await;
        let user = crate::test_support::seed_user(&db, Uuid::from_u128(0x99)).await;
        let svc = service(db);

        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), Some(&user), None)
            .await
            .unwrap();
        // The subtitle-presence prefetch marks this video (C# emits the flag
        // outside the ItemFields system, only when true).
        assert_eq!(dto.has_subtitles, Some(true));
        // CanDelete/CanDownload gate on the user's content permissions
        // (EnableContentDeletion granted, EnableContentDownloading denied in
        // the fake), not just the file-level fact.
        assert_eq!(dto.can_delete, Some(true));
        assert_eq!(dto.can_download, Some(false));
    }

    /// `HasSubtitles` is read out of the page's already-prefetched streams, not
    /// bought with a second ids-only query — that query was the second-costliest
    /// statement on `/Items/Suggestions`.
    ///
    /// The fake's `get_item_ids_with_subtitles` reports EVERY id as subtitled,
    /// so a projection that still asked it would mark `bare` subtitled too. The
    /// per-item answer must therefore come from the streams themselves.
    #[tokio::test]
    async fn has_subtitles_reads_the_prefetched_streams_not_a_second_query() {
        let db = test_db().await;
        let subbed = Uuid::from_u128(0x5B01);
        let bare = Uuid::from_u128(0x5B02);
        seed_named_item(&db, subbed, BaseItemKind::Movie, "Subbed").await;
        seed_named_item(&db, bare, BaseItemKind::Movie, "Bare").await;
        let items = vec![fetch_item(&db, subbed).await, fetch_item(&db, bare).await];
        let svc = service_with_sources(
            db,
            FakeSources {
                without_subtitles: std::iter::once(bare).collect(),
            },
        );

        let dtos = svc
            .get_base_item_dtos(&items, &DtoOptions::default(), None, None, true)
            .await
            .unwrap();

        assert_eq!(dtos[0].has_subtitles, Some(true), "subtitle stream present");
        assert_eq!(
            dtos[1].has_subtitles, None,
            "no subtitle stream on the page"
        );
    }

    /// With no stream-bearing field requested nothing is prefetched to read, so
    /// the ids-only query is still the answer — dropping it outright would nil
    /// `HasSubtitles` for every caller asking for a lean DTO.
    #[tokio::test]
    async fn has_subtitles_falls_back_to_the_query_when_streams_are_not_prefetched() {
        let db = test_db().await;
        let bare = Uuid::from_u128(0x5B03);
        seed_named_item(&db, bare, BaseItemKind::Movie, "Bare").await;
        let items = vec![fetch_item(&db, bare).await];
        let svc = service_with_sources(
            db,
            FakeSources {
                without_subtitles: std::iter::once(bare).collect(),
            },
        );

        // No MediaStreams/MediaSources field → no stream batch was loaded.
        let lean = DtoOptions::with_all_fields(false);
        let dtos = svc
            .get_base_item_dtos(&items, &lean, None, None, true)
            .await
            .unwrap();

        assert_eq!(
            dtos[0].has_subtitles,
            Some(true),
            "the ids-only query still answers when nothing was prefetched"
        );
    }

    #[tokio::test]
    async fn can_delete_true_for_non_virtual_item() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Movie, "M").await;
        let item = fetch_item(&db, id).await;
        assert!(!item.is_virtual_item, "seeded item is a real file item");
        let svc = service(db);
        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), None, None)
            .await
            .unwrap();
        assert_eq!(dto.can_delete, Some(true));
    }

    #[tokio::test]
    async fn by_name_item_shape_matches_jellyfin() {
        // Genre/Studio/Person are `BaseItem` (not folders, not IHasMediaSources) in
        // Jellyfin: IsFolder omitted, CanDelete/CanDownload false, no MediaSources.
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Genre, "Drama").await;
        let item = fetch_item(&db, id).await;
        let svc = service(db);
        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), None, None)
            .await
            .unwrap();
        assert_eq!(dto.is_folder, None, "by-name item is not a folder");
        assert_eq!(dto.can_delete, Some(false));
        assert_eq!(dto.can_download, Some(false));
        assert_eq!(
            dto.sort_name.as_deref(),
            Some("drama"),
            "SortName derives from the name when unstored (like C#)"
        );
        assert!(
            dto.media_sources.is_none(),
            "by-name item has no media source; got {:?}",
            dto.media_sources
        );
    }

    #[tokio::test]
    async fn maps_provider_ids_and_external_urls() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Movie, "M").await;
        seed_provider_id(&db, id, "Imdb", "tt1375666").await;
        let item = fetch_item(&db, id).await;
        let svc = service(db);
        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), None, None)
            .await
            .unwrap();

        assert_eq!(dto.provider_ids.as_ref().unwrap()["Imdb"], "tt1375666");
        assert_eq!(dto.external_urls.as_ref().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_photos_exif_fields_come_back_out_of_the_data_blob() {
        // The scan writes the EXIF under Jellyfin's own property names; the DTO
        // reads them back so a client's photo detail page has them.
        let db = test_db().await;
        let id = Uuid::new_v4();
        let data = r#"{"CameraMake":"ACME","CameraModel":"X1","Software":"Darktable",
            "ExposureTime":0.008,"FocalLength":35.0,"Orientation":"RightTop",
            "Aperture":2.8,"ShutterSpeed":7.0,"Latitude":51.5,"Longitude":-0.12,
            "Altitude":11.0,"IsoSpeedRating":400}"#;
        seed_item_with_data(&db, id, BaseItemKind::Photo, "DSC_0001", data).await;

        let item = fetch_item(&db, id).await;
        let svc = service(db);
        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), None, None)
            .await
            .unwrap();

        assert_eq!(dto.camera_make.as_deref(), Some("ACME"));
        assert_eq!(dto.camera_model.as_deref(), Some("X1"));
        assert_eq!(dto.software.as_deref(), Some("Darktable"));
        assert_eq!(dto.exposure_time, Some(0.008));
        assert_eq!(dto.focal_length, Some(35.0));
        assert_eq!(dto.aperture, Some(2.8));
        assert_eq!(dto.shutter_speed, Some(7.0));
        assert_eq!(dto.latitude, Some(51.5));
        assert_eq!(dto.longitude, Some(-0.12));
        assert_eq!(dto.altitude, Some(11.0));
        assert_eq!(dto.iso_speed_rating, Some(400));
        assert_eq!(
            dto.image_orientation,
            Some(ferrofin_model::drawing::ImageOrientation::RightTop)
        );
    }

    #[tokio::test]
    async fn a_photo_carries_its_albums_name_and_id() {
        // C# `SetPhotoProperties` reads `Photo.AlbumEntity` and sets both
        // fields; jellyfin-web's photo viewer pages through an album by AlbumId.
        let db = test_db().await;
        let album = Uuid::new_v4();
        let photo = Uuid::new_v4();
        let loose = Uuid::new_v4();
        let folder = Uuid::new_v4();
        seed_folder_item(&db, album, BaseItemKind::PhotoAlbum, "Iceland 2024", None).await;
        seed_child_item(&db, photo, BaseItemKind::Photo, "DSC_0002", album).await;
        // A loose photo hangs off the library's collection folder, which is not
        // an album — neither field may be set for it.
        seed_folder_item(&db, folder, BaseItemKind::CollectionFolder, "Photos", None).await;
        seed_child_item(&db, loose, BaseItemKind::Photo, "DSC_0003", folder).await;

        let svc = service(db.clone());
        let dto = svc
            .get_base_item_dto(
                &fetch_item(&db, photo).await,
                &DtoOptions::default(),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(dto.album.as_deref(), Some("Iceland 2024"));
        assert_eq!(dto.album_id, Some(album));

        let dto = svc
            .get_base_item_dto(
                &fetch_item(&db, loose).await,
                &DtoOptions::default(),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(dto.album, None);
        assert_eq!(dto.album_id, None);
    }

    #[tokio::test]
    async fn a_movie_never_grows_photo_fields() {
        // The EXIF keys only mean anything on a Photo; a movie whose Data blob
        // happens to carry one must not sprout a camera on its detail page.
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_item_with_data(
            &db,
            id,
            BaseItemKind::Movie,
            "M",
            r#"{"CameraMake":"ACME"}"#,
        )
        .await;
        let item = fetch_item(&db, id).await;
        let svc = service(db);
        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), None, None)
            .await
            .unwrap();
        assert_eq!(dto.camera_make, None);
    }

    #[tokio::test]
    async fn a_repeated_item_on_one_page_keeps_its_prefetched_rows() {
        // A page may legitimately list the same item twice — a playlist
        // repeating a track, or `/Items?ids=` handed the same id twice. The
        // prefetched relation maps are read once per OCCURRENCE, so the
        // page-build must not hand the first occurrence the only copy and
        // leave the second one bare (`take_or_clone`'s `repeated` guard).
        // All FIVE maps `take_or_clone` drains are covered — images, user_data,
        // provider_ids, chapters, trickplay — because the whole risk of the
        // change is "was the right flag threaded to each site", and each site
        // must be caught individually. Against an EMPTY map `.remove()` and
        // `.get().cloned()` are indistinguishable, so every map here is
        // deliberately populated (the fakes return non-empty trickplay/streams).
        let db = test_db().await;
        let id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let user = seed_user(&db, user_id).await;
        seed_named_item(&db, id, BaseItemKind::Movie, "Twice").await;
        seed_images(
            &db,
            id,
            &[image_info(ImageType::Primary, "/primary.jpg", Some("LKO2"))],
        )
        .await;
        seed_provider_id(&db, id, "Imdb", "tt1375666").await;
        let item = fetch_item(&db, id).await;
        // Backs the chapter repository, so `chapters` is a populated map.
        let svc = service_with_chapters(db);
        let options = DtoOptions {
            fields: vec![
                ItemFields::ProviderIds,
                ItemFields::Chapters,
                ItemFields::Trickplay,
                // `media_streams` is drained at its LAST read (the
                // MediaStreams field), and a repeated id is read again by its
                // next occurrence — so that half of the guard is covered here.
                ItemFields::MediaStreams,
            ],
            ..DtoOptions::default()
        };

        let dtos = svc
            .get_base_item_dtos(
                &[item.clone(), item.clone()],
                &options,
                Some(&user),
                None,
                false,
            )
            .await
            .unwrap();

        assert_eq!(dtos.len(), 2);
        for (i, dto) in dtos.iter().enumerate() {
            let tags = dto
                .image_tags
                .as_ref()
                .unwrap_or_else(|| panic!("occurrence {i} lost its image tags"));
            assert_eq!(
                tags[&ImageType::Primary],
                "tag:/primary.jpg",
                "occurrence {i} lost its primary image"
            );
            let hashes = dto
                .image_blur_hashes
                .as_ref()
                .unwrap_or_else(|| panic!("occurrence {i} lost its blur hashes"));
            assert_eq!(hashes[&ImageType::Primary]["tag:/primary.jpg"], "LKO2");
            assert_eq!(
                dto.provider_ids
                    .as_ref()
                    .unwrap_or_else(|| panic!("occurrence {i} lost its provider ids"))["Imdb"],
                "tt1375666",
                "occurrence {i} lost its provider ids"
            );
            assert!(dto.user_data.is_some(), "occurrence {i} lost its user data");
            assert!(
                !dto.chapters
                    .as_ref()
                    .unwrap_or_else(|| panic!("occurrence {i} lost its chapters"))
                    .is_empty(),
                "occurrence {i} got an empty chapter list"
            );
            assert!(
                !dto.trickplay
                    .as_ref()
                    .unwrap_or_else(|| panic!("occurrence {i} lost its trickplay manifest"))
                    .is_empty(),
                "occurrence {i} got an empty trickplay manifest"
            );
            assert!(
                dto.media_streams.as_ref().is_some_and(|s| !s.is_empty()),
                "occurrence {i} lost its media streams"
            );
        }
        // Both projections are identical — the second is not a degraded copy.
        assert_eq!(dtos[0].image_tags, dtos[1].image_tags);
        assert_eq!(dtos[0].provider_ids, dtos[1].provider_ids);
        assert_eq!(dtos[0].chapters, dtos[1].chapters);
        assert_eq!(dtos[0].user_data, dtos[1].user_data);
        assert_eq!(dtos[0].trickplay, dtos[1].trickplay);
    }

    #[tokio::test]
    async fn a_video_requesting_both_media_sources_and_streams_gets_both() {
        // `media_streams` is drained only at its LAST read (the MediaStreams
        // field). Item detail asks for MediaSources AND MediaStreams, so the map
        // is read twice for the same id; draining at the FIRST (MediaSources)
        // read would silently empty `MediaStreams` on every `/Items/{id}` —
        // killing audio/subtitle track selection in every client — and no other
        // test notices. This pins that ordering.
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Movie, "Streamed").await;
        let item = fetch_item(&db, id).await;
        let svc = service(db);
        let options = DtoOptions {
            fields: vec![ItemFields::MediaSources, ItemFields::MediaStreams],
            ..DtoOptions::default()
        };

        let dtos = svc
            .get_base_item_dtos(&[item], &options, None, None, false)
            .await
            .unwrap();

        let dto = &dtos[0];
        let sources = dto.media_sources.as_ref().expect("MediaSources requested");
        assert!(
            !sources[0].media_streams.is_empty(),
            "MediaSources lost its streams"
        );
        assert!(
            dto.media_streams.as_ref().is_some_and(|s| !s.is_empty()),
            "MediaStreams emptied — the second read of the prefetched map lost its rows"
        );
    }

    #[tokio::test]
    async fn an_items_alternate_version_keeps_its_streams_when_also_on_the_page() {
        // The other half of the `media_streams` exclusion: the map is read once
        // more keyed by a merged ALTERNATE's id, so draining it at EITHER
        // per-item read site strands the alternate. A single-item page can't
        // show this (a drain still returns the value to its own reader), so the
        // page here deliberately overlaps — `FakeSources` hands every primary
        // the same canned alternate id, which is also the first item's own id.
        let db = test_db().await;
        let shared = Uuid::from_u128(0xA17);
        let other = Uuid::from_u128(0xA16);
        seed_named_item(&db, shared, BaseItemKind::Movie, "Alt Cut").await;
        seed_named_item(&db, other, BaseItemKind::Movie, "Feature").await;
        let a = fetch_item(&db, shared).await;
        let b = fetch_item(&db, other).await;
        let svc = service(db);
        let options = DtoOptions {
            fields: vec![ItemFields::MediaSources, ItemFields::MediaStreams],
            ..DtoOptions::default()
        };

        let dtos = svc
            .get_base_item_dtos(&[a, b], &options, None, None, false)
            .await
            .unwrap();

        for (i, dto) in dtos.iter().enumerate() {
            let sources = dto.media_sources.as_ref().expect("MediaSources requested");
            for (j, source) in sources.iter().enumerate() {
                assert!(
                    !source.media_streams.is_empty(),
                    "item {i} source {j} lost its streams — a prefetched \
                     media_streams entry was drained out from under it"
                );
            }
        }
    }

    #[tokio::test]
    async fn resolves_images_into_tags_and_blurhashes() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Movie, "M").await;
        // Primary (single) + backdrop (multiple) images, one with a blurhash.
        seed_images(
            &db,
            id,
            &[
                image_info(ImageType::Primary, "/primary.jpg", Some("LKO2")),
                image_info(ImageType::Backdrop, "/backdrop.jpg", None),
            ],
        )
        .await;
        let item = fetch_item(&db, id).await;
        let svc = service(db);
        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), None, None)
            .await
            .unwrap();

        let image_tags = dto.image_tags.as_ref().expect("image tags");
        assert_eq!(image_tags[&ImageType::Primary], "tag:/primary.jpg");
        assert_eq!(
            dto.backdrop_image_tags.as_deref(),
            Some(&["tag:/backdrop.jpg".to_owned()][..])
        );
        // Blurhash recorded under the primary image's tag.
        let hashes = dto.image_blur_hashes.as_ref().expect("blur hashes");
        assert_eq!(hashes[&ImageType::Primary]["tag:/primary.jpg"], "LKO2");
        // Aspect ratio comes from the fake processor's 400x200 → 2.0.
        assert_eq!(dto.primary_image_aspect_ratio, Some(2.0));
    }

    #[tokio::test]
    async fn image_tags_is_empty_map_not_null_when_item_has_no_images() {
        // An item with no images must still serialize `ImageTags` as `{}` (not
        // omit it → null). The Jellyfin Android TV client NPEs on
        // `getImageTags().containsKey(...)` when it is null. Matches Jellyfin's
        // `dto.ImageTags = []` inside `EnableImages`.
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Movie, "M").await;
        let item = fetch_item(&db, id).await;
        let svc = service(db);
        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), None, None)
            .await
            .unwrap();

        let image_tags = dto.image_tags.as_ref().expect("ImageTags must be present");
        assert!(
            image_tags.is_empty(),
            "empty map for an item with no images"
        );
        // Same rule for ImageBlurHashes: Jellyfin always emits `{}`, never null.
        let hashes = dto
            .image_blur_hashes
            .as_ref()
            .expect("ImageBlurHashes must be present");
        assert!(hashes.is_empty(), "empty blurhash map, not null");
    }

    #[tokio::test]
    async fn primary_aspect_ratio_endpoint_matches_processor() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Movie, "M").await;
        seed_images(
            &db,
            id,
            &[image_info(ImageType::Primary, "/primary.jpg", None)],
        )
        .await;
        let svc = service(db);
        let ratio = svc.get_primary_image_aspect_ratio(id).await.unwrap();
        assert_eq!(ratio, Some(2.0));
    }

    #[tokio::test]
    async fn attaches_user_data_when_a_user_is_present() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Movie, "M").await;
        let user = seed_user(&db, Uuid::new_v4()).await;
        let item = fetch_item(&db, id).await;
        let svc = service(db);
        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), Some(&user), None)
            .await
            .unwrap();

        assert!(dto.user_data.as_ref().expect("user data").is_favorite);
    }

    #[tokio::test]
    async fn item_by_name_counts_use_the_count_service() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Genre, "Action").await;
        let item = fetch_item(&db, id).await;
        let svc = service(db);
        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), None, None)
            .await
            .unwrap();

        assert_eq!(dto.movie_count, Some(3));
        assert_eq!(dto.series_count, Some(2));
        // Child count sums the per-kind counts.
        assert_eq!(dto.child_count, Some(5));
    }

    #[tokio::test]
    async fn child_count_attaches_to_folders_when_requested() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_folder_item(&db, id, BaseItemKind::Season, "Season 1", None).await;
        let user = seed_user(&db, Uuid::new_v4()).await;
        let item = fetch_item(&db, id).await;
        let svc = service(db);
        let options = DtoOptions {
            fields: vec![ItemFields::ChildCount],
            ..DtoOptions::default()
        };

        // Both the single and the batch path attach the count-service value.
        let dto = svc
            .get_base_item_dto(&item, &options, Some(&user), None)
            .await
            .unwrap();
        assert_eq!(dto.child_count, Some(4));
        let dtos = svc
            .get_base_item_dtos(
                std::slice::from_ref(&item),
                &options,
                Some(&user),
                None,
                true,
            )
            .await
            .unwrap();
        assert_eq!(dtos[0].child_count, Some(4));

        // C# attaches ChildCount in `AttachUserSpecificInfo`: no user → no count.
        let anon = svc
            .get_base_item_dto(&item, &options, None, None)
            .await
            .unwrap();
        assert_eq!(anon.child_count, None);
        // And only when the field is requested (default options enable every
        // field, so pass an explicitly empty list).
        let no_fields = DtoOptions {
            fields: vec![],
            ..DtoOptions::default()
        };
        let no_field = svc
            .get_base_item_dto(&item, &no_fields, Some(&user), None)
            .await
            .unwrap();
        assert_eq!(no_field.child_count, None);
    }

    /// The `folder.LinkedChildren.Length` half of upstream's ChildCount
    /// shortcut (v10.11.8 Emby.Server.Implementations/Dto/DtoService.cs:481-486).
    ///
    /// It runs with NO `ItemFields` gate, so a playlist page that asked for
    /// neither `ChildCount` nor `RecursiveItemCount` still carries a count on
    /// 10.11.8 — a playlist's entries ARE its linked children. Ferrofin ported
    /// only `dto.ChildCount = dto.RecursiveItemCount;` and answered with no
    /// `ChildCount` at all on exactly that page.
    #[tokio::test]
    async fn child_count_falls_back_to_the_linked_children_length() {
        let db = test_db().await;
        let playlist = Uuid::new_v4();
        let season = Uuid::new_v4();
        seed_folder_item(&db, playlist, BaseItemKind::Playlist, "Road Trip", None).await;
        seed_folder_item(&db, season, BaseItemKind::Season, "Season 1", None).await;
        let user = seed_user(&db, Uuid::new_v4()).await;
        let playlist_row = fetch_item(&db, playlist).await;
        let season_row = fetch_item(&db, season).await;
        // Neither ChildCount nor RecursiveItemCount requested: the ONLY thing
        // that can answer is the linked-children length.
        let no_fields = DtoOptions {
            fields: vec![],
            ..DtoOptions::default()
        };

        let svc = service_with_linked_children(db.clone(), 7);
        let dto = svc
            .get_base_item_dto(&playlist_row, &no_fields, Some(&user), None)
            .await
            .unwrap();
        assert_eq!(
            Some(7),
            dto.child_count,
            "a playlist reports its linked-children length"
        );
        // The batch path too — the prefetch is where the count comes from.
        let dtos = svc
            .get_base_item_dtos(
                std::slice::from_ref(&playlist_row),
                &no_fields,
                Some(&user),
                None,
                true,
            )
            .await
            .unwrap();
        assert_eq!(Some(7), dtos[0].child_count);

        // `AttachUserSpecificInfo` runs only for a user.
        let anon = svc
            .get_base_item_dto(&playlist_row, &no_fields, None, None)
            .await
            .unwrap();
        assert_eq!(None, anon.child_count);

        // `if (folderChildCount > 0)`: an empty LinkedChildren array — what a
        // real MusicAlbum and Season have — leaves ChildCount unset rather than
        // reporting a spurious 0.
        let empty = service_with_linked_children(db, 0);
        for row in [&season_row, &playlist_row] {
            assert_eq!(
                None,
                empty
                    .get_base_item_dto(row, &no_fields, Some(&user), None)
                    .await
                    .unwrap()
                    .child_count
            );
        }
    }

    #[tokio::test]
    async fn child_count_placeholder_for_collection_folders() {
        // C# `GetChildCount` returns a random 1..10 for collection folders and
        // user views instead of a real count; the port derives a stable 1..=9.
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_folder_item(&db, id, BaseItemKind::CollectionFolder, "Shows", None).await;
        let user = seed_user(&db, Uuid::new_v4()).await;
        let item = fetch_item(&db, id).await;
        let svc = service(db);
        let options = DtoOptions {
            fields: vec![ItemFields::ChildCount],
            ..DtoOptions::default()
        };

        let dto = svc
            .get_base_item_dto(&item, &options, Some(&user), None)
            .await
            .unwrap();
        let count = dto.child_count.expect("placeholder set");
        assert!((1..=9).contains(&count));
    }

    /// `UserRootFolder` is neither `ICollectionFolder` nor `UserView`, so C#
    /// falls through to `folder.GetChildCount(user)` — a REAL count
    /// (DtoService.cs:648-665). Its children are the libraries plus the
    /// playlists plugin folder `CreateRootFolder()` parents to it, and getting
    /// this wrong is what made `GET /Items/Root` report 3 where 10.11.8
    /// reports 4.
    #[tokio::test]
    async fn child_count_is_real_for_the_user_root_folder() {
        let db = test_db().await;
        // A fixed id whose id-derived placeholder is NOT the count service's
        // canned value, so the assertion below can only pass on the real branch.
        let root = Uuid::from_u128(0x1234_5678_9abc_def0_1122_3344_5566_7700);
        let placeholder = i32::from(root.as_bytes()[15] % 9) + 1;
        assert_ne!(placeholder, 4, "the fixture must separate the two branches");
        seed_folder_item(
            &db,
            root,
            BaseItemKind::UserRootFolder,
            "Media Folders",
            None,
        )
        .await;
        // The children upstream's root has: the libraries plus the playlists
        // plugin folder. (`FakeCounts` reports a fixed 4 per parent, so what is
        // under test is which branch `attach_child_count` takes, not the sum.)
        for (kind, name) in [
            (BaseItemKind::CollectionFolder, "Movies"),
            (BaseItemKind::CollectionFolder, "Shows"),
            (BaseItemKind::PlaylistsFolder, "Playlists"),
        ] {
            seed_folder_item(&db, Uuid::new_v4(), kind, name, Some(root)).await;
        }
        let user = seed_user(&db, Uuid::new_v4()).await;
        let item = fetch_item(&db, root).await;
        let svc = service(db);
        let options = DtoOptions {
            fields: vec![ItemFields::ChildCount],
            ..DtoOptions::default()
        };

        let dto = svc
            .get_base_item_dto(&item, &options, Some(&user), None)
            .await
            .unwrap();
        assert_eq!(
            dto.child_count,
            Some(4),
            "the root takes the real-count branch, not the id-derived placeholder"
        );
    }

    /// `BasePluginFolder.CanDelete() => false` (v10.11.8
    /// `MediaBrowser.Controller/Entities/BasePluginFolder.cs:24`), and
    /// `PlaylistsFolder : BasePluginFolder` — so the Playlists folder must
    /// report `CanDelete: false`, which live 10.11.8 does on both
    /// `GET /Library/MediaFolders` and `GET /Items?parentId=<root>`.
    ///
    /// This is a DTO-level test on purpose. `CanDelete` is resolved from the
    /// **stored** kind (`row_kind`, not `client_kind`), and the stored kind is
    /// `PlaylistsFolder` on a fresh or adopted 10.11.8 database — while an
    /// older Ferrofin wrote `ManualPlaylistsFolder` at the same path. A table
    /// that knew only the second spelling let the row fall through to the
    /// `_ => true` arm and served `CanDelete: true`, and no lab whose row had
    /// been adopted under the legacy type could see it. Both spellings are
    /// seeded here, both with a parent, so neither route can regress.
    ///
    /// The fake permissions reader grants deletion (`Ok(Some((true, false)))`),
    /// so a `false` here can only come from the kind table — not from the
    /// user's `EnableContentDeletion` policy, which is the other half of
    /// C# `BaseItem.CanDelete(user)`.
    #[tokio::test]
    async fn the_playlists_plugin_folder_can_never_be_deleted() {
        let db = test_db().await;
        let root = Uuid::new_v4();
        seed_folder_item(
            &db,
            root,
            BaseItemKind::UserRootFolder,
            "Media Folders",
            None,
        )
        .await;
        let user = seed_user(&db, Uuid::new_v4()).await;
        let svc = service(db.clone());
        let options = DtoOptions {
            fields: vec![ItemFields::CanDelete],
            ..DtoOptions::default()
        };

        for kind in [
            BaseItemKind::PlaylistsFolder,
            BaseItemKind::ManualPlaylistsFolder,
        ] {
            let id = Uuid::new_v4();
            seed_folder_item(&db, id, kind, "Playlists", Some(root)).await;
            let item = fetch_item(&db, id).await;
            let dto = svc
                .get_base_item_dto(&item, &options, Some(&user), None)
                .await
                .unwrap();
            assert_eq!(
                dto.can_delete,
                Some(false),
                "{kind:?} is a BasePluginFolder — CanDelete() is a hard false"
            );
            // A library alongside it stays false too, and an ordinary media
            // folder stays true — so the assertion above is not a table that
            // simply says "no" to everything.
            assert_eq!(
                dto.type_,
                BaseItemKind::ManualPlaylistsFolder,
                "clients see GetClientTypeName() under either stored spelling"
            );
        }

        let plain = Uuid::new_v4();
        seed_folder_item(&db, plain, BaseItemKind::Folder, "Some Folder", Some(root)).await;
        let item = fetch_item(&db, plain).await;
        let dto = svc
            .get_base_item_dto(&item, &options, Some(&user), None)
            .await
            .unwrap();
        assert_eq!(
            dto.can_delete,
            Some(true),
            "the permission is granted, so a deletable kind must still say true"
        );
    }

    /// `PlaylistsFolder.GetClientTypeName()` returns `"ManualPlaylistsFolder"`
    /// (v10.11.8 `Emby.Server.Implementations/Playlists/PlaylistsFolder.cs:50`),
    /// which is why 10.11.8 ships no class of that name yet every client sees
    /// that `Type`.
    #[test]
    fn playlists_folder_renders_as_manual_playlists_folder() {
        assert_eq!(
            client_kind(BaseItemKind::PlaylistsFolder),
            BaseItemKind::ManualPlaylistsFolder
        );
        assert_eq!(
            client_kind(BaseItemKind::LiveTvChannel),
            BaseItemKind::TvChannel
        );
        assert_eq!(client_kind(BaseItemKind::Movie), BaseItemKind::Movie);
    }

    #[tokio::test]
    async fn item_by_name_dto_counts_supplied_tagged_items() {
        let db = test_db().await;
        // The genre item plus two tagged movies and a series it groups.
        let genre = Uuid::new_v4();
        seed_named_item(&db, genre, BaseItemKind::Genre, "Action").await;
        let m1 = Uuid::new_v4();
        let m2 = Uuid::new_v4();
        let s1 = Uuid::new_v4();
        seed_named_item(&db, m1, BaseItemKind::Movie, "A").await;
        seed_named_item(&db, m2, BaseItemKind::Movie, "B").await;
        seed_named_item(&db, s1, BaseItemKind::Series, "C").await;

        let item = fetch_item(&db, genre).await;
        // Library fake must resolve the tagged ids to rows.
        let library = Arc::new(DbBackedLibrary { db: db.clone() });
        let svc = service_with(db, library);
        let dto = svc
            .get_item_by_name_dto(&item, &DtoOptions::default(), Some(&[m1, m2, s1]), None)
            .await
            .unwrap();

        assert_eq!(dto.movie_count, Some(2));
        assert_eq!(dto.series_count, Some(1));
        assert_eq!(dto.child_count, Some(3));
    }

    /// A [`LibraryManager`] fake whose `get_item_by_id` hits the real DB — used by
    /// the tagged-items count test, which needs each id resolved to its kind.
    struct DbBackedLibrary {
        db: Database,
    }

    #[async_trait]
    impl LibraryManager for DbBackedLibrary {
        async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
            Ok(fetch_item_opt(&self.db, id).await)
        }
        async fn get_item_images(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<ferrofin_traits::options::ItemImageInfo>, ServiceError> {
            Ok(vec![])
        }
        async fn query_items(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<ferrofin_model::querying::QueryResult<BaseItemEntity>, ServiceError> {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_item_ids(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<Vec<Uuid>, ServiceError> {
            Ok(vec![])
        }
        async fn get_item_list(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            Ok(vec![])
        }
        async fn get_latest_item_list(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
            _collection_type: ferrofin_model::data::CollectionType,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            Ok(vec![])
        }
        async fn create_items(
            &self,
            _items: &[BaseItemEntity],
            _parent_id: Option<Uuid>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn update_items(
            &self,
            _items: &[BaseItemEntity],
            _parent_id: Option<Uuid>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn delete_item(
            &self,
            _id: Uuid,
            _options: &ferrofin_traits::options::DeleteOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_people(
            &self,
            _query: &ferrofin_traits::options::InternalPeopleQuery,
        ) -> Result<Vec<PeopleEntity>, ServiceError> {
            Ok(vec![])
        }
        async fn get_people_names(
            &self,
            _query: &ferrofin_traits::options::InternalPeopleQuery,
        ) -> Result<Vec<String>, ServiceError> {
            Ok(vec![])
        }
        async fn get_count(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<i32, ServiceError> {
            Ok(0)
        }
        async fn get_item_counts(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<ferrofin_model::dto::ItemCounts, ServiceError> {
            Ok(ferrofin_model::dto::ItemCounts::default())
        }
        async fn get_genres(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_studios(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_artists(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_music_genres(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_album_artists(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(ferrofin_model::querying::QueryResult::default())
        }
        async fn get_query_filters_legacy(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<ferrofin_model::querying::QueryFiltersLegacy, ServiceError> {
            Ok(ferrofin_model::querying::QueryFiltersLegacy::default())
        }
        async fn get_media_stream_languages(
            &self,
            _stream_type: ferrofin_model::entities::MediaStreamType,
            _query: &ferrofin_traits::options::InternalItemsQuery,
        ) -> Result<Vec<String>, ServiceError> {
            Ok(Vec::new())
        }
        async fn queue_library_scan(&self) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn attaches_people_from_the_library() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Movie, "M").await;
        let item = fetch_item(&db, id).await;

        let library = Arc::new(FakeLibrary {
            people: vec![PeopleEntity {
                id: Uuid::new_v4().to_string(),
                name: "Leonardo DiCaprio".into(),
                person_type: Some("Actor".into()),
                ..Default::default()
            }],
            ..Default::default()
        });
        let svc = service_with(db, library);
        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), None, None)
            .await
            .unwrap();

        let people = dto.people.as_ref().expect("people");
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].name.as_deref(), Some("Leonardo DiCaprio"));
        assert_eq!(people[0].type_, ferrofin_model::data::PersonKind::Actor);
    }

    #[tokio::test]
    async fn batched_value_ids_match_single_lookup() {
        let db = test_db().await;
        let vid = Uuid::new_v4();
        let clean = crate::text_util::get_clean_value("Warner Bros.");
        sqlx::query(
            r#"INSERT INTO "ItemValues" ("ItemValueId","CleanValue","Type","Value")
               VALUES (?1, ?2, 3, 'Warner Bros.')"#,
        )
        .bind(guid_to_db(vid))
        .bind(&clean)
        .execute(db.writer())
        .await
        .unwrap();
        let svc = service(db);

        // The batch resolver finds the stored id under its (type, clean) key.
        let map = svc.resolve_value_ids(&[(3, clean.clone())]).await.unwrap();
        assert_eq!(map.get(&3).and_then(|m| m.get(&clean)).copied(), Some(vid));

        // Prefetched::value_id reads the map without a query, and nil-s a name
        // with no row. With an empty clean cache it falls back to cleaning the
        // name itself, so an un-prefetched name still resolves.
        let pf = Prefetched {
            value_ids: map,
            ..Prefetched::default()
        };
        assert_eq!(pf.value_id(3, "Warner Bros."), vid);
        assert!(pf.value_id(3, "Nobody").is_nil());

        // ...and the cached clean value resolves the same id, for every spelling
        // that cleans to it.
        let pf = Prefetched {
            value_ids: pf.value_ids,
            clean_values: [("WARNER BROS.".to_owned(), clean.clone())]
                .into_iter()
                .collect(),
            ..Prefetched::default()
        };
        assert_eq!(pf.value_id(3, "WARNER BROS."), vid);
    }

    /// Studios/genres/artists must resolve to the SAME `ItemValues` ids the
    /// per-name lookup found, for names whose clean form differs from the stored
    /// spelling — the page's cached clean keys are the join key against
    /// `ItemValues.CleanValue`, so any normalization drift empties these fields.
    ///
    /// The spellings below differ only in case and diacritics, which is exactly
    /// what the clean value folds: C# `GetCleanValue` is
    /// `RemoveDiacritics().ToLowerInvariant()` and keeps punctuation and
    /// whitespace, so `'WARNER   BROS!'` is NOT another spelling of
    /// `'Warner Bros.'` — to Jellyfin those are two different studios.
    #[tokio::test]
    async fn value_ids_resolve_end_to_end_for_awkward_names() {
        let db = test_db().await;

        // Seed one ItemValues row per (type, clean value).
        let seed_value = |value_type: i32, name: &'static str| {
            let db = db.clone();
            async move {
                let vid = Uuid::new_v4();
                sqlx::query(
                    r#"INSERT INTO "ItemValues" ("ItemValueId","CleanValue","Type","Value")
                       VALUES (?1, ?2, ?3, ?4)"#,
                )
                .bind(guid_to_db(vid))
                .bind(crate::text_util::get_clean_value(name))
                .bind(value_type)
                .bind(name)
                .execute(db.writer())
                .await
                .unwrap();
                vid
            }
        };
        let studio_id = seed_value(3, "Warner Bros.").await;
        let genre_id = seed_value(2, "Sci-Fi").await;
        // The artist is stored under BOTH value types, with different spellings
        // that share a clean value — attach_artists must prefer the AlbumArtist
        // (1) row, which is the browsable one.
        let artist_id = seed_value(0, "Sigur Rós").await;
        let album_artist_id = seed_value(1, "sigur ros").await;

        let movie = Uuid::from_u128(0xA_1234);
        seed_named_item(&db, movie, BaseItemKind::Movie, "M").await;
        sqlx::query(
            r#"UPDATE "BaseItems" SET "Studios" = 'WARNER BROS.', "Genres" = 'sci-fi'
               WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(movie))
        .execute(db.writer())
        .await
        .unwrap();
        let song = Uuid::from_u128(0xA_5678);
        seed_named_item(&db, song, BaseItemKind::Audio, "Song").await;
        sqlx::query(
            r#"UPDATE "BaseItems" SET "Artists" = 'sigur rós', "AlbumArtists" = 'Sigur ROS'
               WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(song))
        .execute(db.writer())
        .await
        .unwrap();

        let movie_row = fetch_item(&db, movie).await;
        let song_row = fetch_item(&db, song).await;
        let svc = service(db);
        // One page holding both rows: the batch prefetch builds the clean keys.
        let dtos = svc
            .get_base_item_dtos(
                &[movie_row, song_row],
                &DtoOptions::default(),
                None,
                None,
                true,
            )
            .await
            .unwrap();

        let studios = dtos[0].studios.as_ref().expect("studios");
        assert_eq!(studios[0].name.as_deref(), Some("WARNER BROS."));
        assert_eq!(studios[0].id, studio_id, "studio id");
        let genres = dtos[0].genre_items.as_ref().expect("genre items");
        assert_eq!(genres[0].id, genre_id, "genre id");

        let artists = dtos[1].artist_items.as_ref().expect("artist items");
        assert_eq!(
            artists[0].id, album_artist_id,
            "performer prefers the AlbumArtist value id"
        );
        assert_ne!(artists[0].id, artist_id);
        let album_artists = dtos[1].album_artists.as_ref().expect("album artists");
        assert_eq!(album_artists[0].id, album_artist_id, "album-artist id");
    }

    /// Every credit spelling on the page must resolve to the ONE by-name `Person`
    /// item (what favorites are written against), not to its per-credit row id.
    #[tokio::test]
    async fn people_ids_resolve_for_every_credit_spelling() {
        let db = test_db().await;
        let movie = Uuid::from_u128(0xB_1234);
        seed_named_item(&db, movie, BaseItemKind::Movie, "M").await;
        let person = Uuid::from_u128(0xB_5678);
        seed_named_item(&db, person, BaseItemKind::Person, "Leonardo DiCaprio").await;
        let person_row = fetch_item(&db, person).await;
        let item = fetch_item(&db, movie).await;

        let library = Arc::new(FakeLibrary {
            people: vec![
                PeopleEntity {
                    id: Uuid::new_v4().to_string(),
                    name: "Leonardo DiCaprio".into(),
                    person_type: Some("Actor".into()),
                    ..Default::default()
                },
                // The same person credited again with different casing — one
                // by-name item backs both.
                PeopleEntity {
                    id: Uuid::new_v4().to_string(),
                    name: "leonardo dicaprio".into(),
                    person_type: Some("Director".into()),
                    ..Default::default()
                },
            ],
            named_items: vec![person_row],
        });
        let svc = service_with(db, library);
        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), None, None)
            .await
            .unwrap();

        let people = dto.people.as_ref().expect("people");
        assert_eq!(people.len(), 2);
        assert_eq!(people[0].id, person, "first spelling");
        assert_eq!(people[1].id, person, "second spelling");
    }

    /// The page's own images and the cast's images come out of ONE
    /// `BaseItemImageInfos` read, and each side must still land on its own DTO.
    ///
    /// The two used to be separate queries; merging them is only safe if the
    /// page rows keep their images (they are drained per item as it projects)
    /// AND the by-name `Person` rows keep theirs (they are read by name, and a
    /// person may also be sitting on the page).
    #[tokio::test]
    async fn page_and_cast_images_both_survive_the_single_image_read() {
        let db = test_db().await;
        let movie = Uuid::from_u128(0xC_1234);
        seed_named_item(&db, movie, BaseItemKind::Movie, "M").await;
        seed_images(
            &db,
            movie,
            &[image_info(ImageType::Primary, "/m.jpg", None)],
        )
        .await;
        let person = Uuid::from_u128(0xC_5678);
        seed_named_item(&db, person, BaseItemKind::Person, "Leonardo DiCaprio").await;
        seed_images(
            &db,
            person,
            &[image_info(ImageType::Primary, "/p.jpg", None)],
        )
        .await;
        let person_row = fetch_item(&db, person).await;
        let item = fetch_item(&db, movie).await;

        let library = Arc::new(FakeLibrary {
            people: vec![PeopleEntity {
                id: Uuid::new_v4().to_string(),
                name: "Leonardo DiCaprio".into(),
                person_type: Some("Actor".into()),
                ..Default::default()
            }],
            named_items: vec![person_row],
        });
        let svc = service_with(db, library);
        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), None, None)
            .await
            .unwrap();

        assert!(
            dto.image_tags
                .as_ref()
                .is_some_and(|tags| tags.contains_key(&ImageType::Primary)),
            "the page item keeps its own image"
        );
        let people = dto.people.as_ref().expect("people");
        assert!(
            people[0].primary_image_tag.is_some(),
            "the credited person keeps its cast art"
        );
    }

    /// Credit spellings whose *stored* form differs from every "tidy" convention
    /// a test would otherwise use: mixed case, leading/trailing and doubled
    /// internal whitespace, punctuation, accents, and non-Latin script. Paired
    /// with the name of the ONE by-name `Person` item that backs each.
    ///
    /// The clean value folds case and diacritics and NOTHING else (C#
    /// `GetCleanValue` is `RemoveDiacritics().ToLowerInvariant()`), so each
    /// pair differs only in those two dimensions — punctuation is carried
    /// through verbatim, and a pair that differed in it would be two different
    /// people to Jellyfin as much as to Ferrofin.
    ///
    /// The one exception is the surrounding whitespace on the Meryl Streep
    /// row: `LibraryManager::get_named_item` trims the name it looks up, where
    /// C# `TranslateQuery` passes `filter.Name` to `GetCleanValue` untrimmed.
    /// That trim is a Ferrofin divergence, and this row is what pins it.
    const AWKWARD_CREDITS: &[(&str, &str)] = &[
        // Mixed case: `to_lowercase()` is not the identity here.
        ("Robert De Niro", "Robert De Niro"),
        ("Andie MacDowell", "ANDIE MACDOWELL"),
        // Leading/trailing whitespace on the credit row: `get_named_item`
        // trims the name it looks up, so this still resolves to the tidy item.
        ("  Meryl Streep  ", "Meryl Streep"),
        // Doubled internal whitespace + an apostrophe, both carried through.
        ("Conan  O'Brien", "CONAN  O'BRIEN"),
        // Hyphenated given name.
        ("Jean-Luc Godard", "jean-luc godard"),
        // Accent on the credit, folded on the by-name item.
        ("Renée Zellweger", "Renee Zellweger"),
        // Accent on both sides — lowercasing this is NOT a no-op.
        ("Björk", "Björk"),
        // Non-Latin script.
        ("宮崎 駿", "宮崎 駿"),
    ];

    /// A slice index as a `u128`, for deriving distinct fixture ids.
    fn idx(i: usize) -> u128 {
        u128::try_from(i).expect("index fits")
    }

    /// The `person_ids_by_name` key convention, pinned END TO END: the prefetch
    /// builds the map and `attach_people` reads it, so the two sides must agree
    /// about the key for every one of [`AWKWARD_CREDITS`].
    ///
    /// If they ever disagree (one side lowercasing, trimming or cleaning while
    /// the other does not) every cast member silently falls back to its
    /// per-credit `Peoples` row id — which is NOT what favorites are written
    /// against — and nothing else in the suite would notice, because tidy ASCII
    /// names key the same under either convention.
    #[tokio::test]
    async fn person_ids_resolve_for_awkwardly_spelled_credits() {
        let db = test_db().await;
        let movie = Uuid::from_u128(0xB_9001);
        seed_named_item(&db, movie, BaseItemKind::Movie, "M").await;

        // One by-name Person item per credit, plus one per-credit Peoples row
        // with a DIFFERENT id, so a fallback to the credit id is visible.
        let mut person_rows = Vec::new();
        let mut expected = Vec::new();
        let mut credits = Vec::new();
        let mut credit_ids = Vec::new();
        for (i, (credited_as, item_name)) in AWKWARD_CREDITS.iter().enumerate() {
            let person_id = Uuid::from_u128(0xB_9100 + idx(i));
            seed_named_item(&db, person_id, BaseItemKind::Person, item_name).await;
            person_rows.push(fetch_item(&db, person_id).await);
            expected.push(person_id);
            let credit_id = Uuid::from_u128(0xB_9200 + idx(i));
            credit_ids.push(credit_id);
            credits.push(PeopleEntity {
                id: credit_id.to_string(),
                name: (*credited_as).into(),
                person_type: Some("Actor".into()),
                ..Default::default()
            });
        }
        let item = fetch_item(&db, movie).await;

        let library = Arc::new(FakeLibrary {
            people: credits,
            named_items: person_rows,
        });
        let svc = service_with(db, library);
        let dto = svc
            .get_base_item_dto(&item, &DtoOptions::default(), None, None)
            .await
            .unwrap();

        let people = dto.people.as_ref().expect("people");
        assert_eq!(people.len(), AWKWARD_CREDITS.len());
        for (i, (credited_as, _)) in AWKWARD_CREDITS.iter().enumerate() {
            // The credit keeps its stored spelling…
            assert_eq!(
                people[i].name.as_deref(),
                Some(*credited_as),
                "credit name for {credited_as:?}"
            );
            // …and resolves to the by-name Person item, not the credit row.
            assert_ne!(
                people[i].id, credit_ids[i],
                "{credited_as:?} fell back to its per-credit row id — the build \
                 and lookup sides of person_ids_by_name disagree about the key"
            );
            assert_eq!(
                people[i].id, expected[i],
                "by-name Person id for {credited_as:?}"
            );
        }
    }

    /// The lookup half of the same convention, pinned in isolation: the map is
    /// keyed by the credit row's name EXACTLY as stored (the contract its doc
    /// comment states), and a name the prefetch never resolved falls back to the
    /// per-credit row id — or is dropped when that is not a GUID.
    #[tokio::test]
    async fn attach_people_looks_up_the_raw_stored_credit_name() {
        let db = test_db().await;
        let movie = Uuid::from_u128(0xB_9301);
        seed_named_item(&db, movie, BaseItemKind::Movie, "M").await;
        let item = fetch_item(&db, movie).await;
        let svc = service(db);

        let mut credits = Vec::new();
        let mut by_name: HashMap<String, Uuid> = HashMap::new();
        let mut expected = Vec::new();
        for (i, (credited_as, _)) in AWKWARD_CREDITS.iter().enumerate() {
            let person_id = Uuid::from_u128(0xB_9400 + idx(i));
            // Registered under the RAW spelling — what the prefetch stores.
            by_name.insert((*credited_as).to_owned(), person_id);
            expected.push(person_id);
            credits.push(PeopleEntity {
                id: Uuid::from_u128(0xB_9500 + idx(i)).to_string(),
                name: (*credited_as).into(),
                person_type: Some("Actor".into()),
                ..Default::default()
            });
        }
        // An unresolved credit keeps its own row id…
        let unresolved = Uuid::from_u128(0xB_95FF);
        credits.push(PeopleEntity {
            id: unresolved.to_string(),
            name: "Nobody At All".into(),
            person_type: Some("Actor".into()),
            ..Default::default()
        });
        // …and a credit whose stored id is not a GUID is dropped entirely
        // rather than emitted with the nil GUID (C# `AttachPeople` only adds a
        // `BaseItemPerson` for a credit it could resolve).
        credits.push(PeopleEntity {
            id: "not-a-guid".into(),
            name: "Also Nobody".into(),
            person_type: Some("Actor".into()),
            ..Default::default()
        });

        let prefetched = Prefetched {
            people: [(movie, credits)].into_iter().collect(),
            person_ids_by_name: by_name,
            ..Prefetched::default()
        };
        let mut dto = BaseItemDto::default();
        svc.attach_people(&mut dto, &item, &prefetched)
            .await
            .unwrap();

        let people = dto.people.as_ref().expect("people");
        // The non-GUID credit is dropped, so only the resolvable ones survive.
        assert_eq!(people.len(), AWKWARD_CREDITS.len() + 1);
        for (i, (credited_as, _)) in AWKWARD_CREDITS.iter().enumerate() {
            assert_eq!(
                people[i].id, expected[i],
                "{credited_as:?} must be looked up under its raw stored spelling"
            );
        }
        assert_eq!(
            people[AWKWARD_CREDITS.len()].id,
            unresolved,
            "unresolved credit keeps its per-credit row id"
        );
        assert!(
            people.iter().all(|p| !p.id.is_nil()),
            "no credit is emitted with the nil GUID"
        );
        assert!(
            people
                .iter()
                .all(|p| p.name.as_deref() != Some("Also Nobody")),
            "a credit whose stored id is not a GUID is dropped, not emitted as nil"
        );
    }

    /// The clean lookup key the projection uses must be byte-identical to
    /// `get_clean_value` for every name — a divergence silently empties
    /// Genres/Studios/Artists/People instead of failing loudly.
    #[tokio::test]
    async fn clean_lookup_keys_match_get_clean_value_for_awkward_names() {
        const NAMES: &[&str] = &[
            "Warner Bros.",
            "warner bros",
            "WARNER BROS.",
            "  Leading And Trailing  ",
            "Ångström Þéâtre",
            "Motörhead",
            "Amélie",
            "AC/DC",
            "Sigur Rós & Björk",
            "Beyoncé feat. Jay-Z",
            "  ",
            "",
            "20th Century Fox",
            "!!!",
        ];

        let db = test_db().await;
        let svc = service(db.clone());

        // A stored row for every distinct clean value, so a wrong key is a
        // missing id rather than a coincidental match.
        let mut expected: HashMap<String, Uuid> = HashMap::new();
        for name in NAMES {
            let clean = crate::text_util::get_clean_value(name);
            if expected.contains_key(&clean) {
                continue;
            }
            let vid = Uuid::new_v4();
            sqlx::query(
                r#"INSERT INTO "ItemValues" ("ItemValueId","CleanValue","Type","Value")
                   VALUES (?1, ?2, 3, ?3)"#,
            )
            .bind(guid_to_db(vid))
            .bind(&clean)
            .bind(*name)
            .execute(db.writer())
            .await
            .unwrap();
            expected.insert(clean, vid);
        }

        // The prefetched (cached-clean) path and the uncached fallback path must
        // agree with each other AND with a fresh get_clean_value lookup.
        let pairs: Vec<(i32, String)> = NAMES
            .iter()
            .map(|n| (3, crate::text_util::get_clean_value(n)))
            .collect();
        let value_ids = svc.resolve_value_ids(&pairs).await.unwrap();
        let cached = Prefetched {
            value_ids: value_ids.clone(),
            clean_values: NAMES
                .iter()
                .map(|n| ((*n).to_owned(), crate::text_util::get_clean_value(n)))
                .collect(),
            ..Prefetched::default()
        };
        let uncached = Prefetched {
            value_ids,
            ..Prefetched::default()
        };
        for name in NAMES {
            let want = expected
                .get(&crate::text_util::get_clean_value(name))
                .copied()
                .expect("seeded id");
            assert_eq!(
                cached.clean_key(name).as_ref(),
                crate::text_util::get_clean_value(name),
                "clean key diverged for {name:?}"
            );
            assert_eq!(cached.value_id(3, name), want, "cached id for {name:?}");
            assert_eq!(uncached.value_id(3, name), want, "uncached id for {name:?}");
        }
    }

    #[tokio::test]
    async fn media_sources_include_merged_alternate_versions() {
        let db = test_db().await;
        let id = Uuid::from_u128(0xA16);
        seed_named_item(&db, id, BaseItemKind::Movie, "Heat").await;
        let item = fetch_item(&db, id).await;
        let svc = service(db);

        let options = DtoOptions {
            fields: vec![ItemFields::MediaSources],
            ..DtoOptions::default()
        };
        // FakeSources reports one alternate version per primary: the DTO's
        // sources are the primary's static source plus the alternate's, on the
        // single-item path and the batch path alike.
        let dto = svc
            .get_base_item_dto(&item, &options, None, None)
            .await
            .unwrap();
        let sources = dto.media_sources.expect("sources");
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[1].path.as_deref(), Some("/media/alt.mkv"));

        let batch = svc
            .get_base_item_dtos(std::slice::from_ref(&item), &options, None, None, true)
            .await
            .unwrap();
        let batch_sources = batch[0].media_sources.as_ref().expect("batch sources");
        assert_eq!(batch_sources.len(), 2);
        assert_eq!(batch_sources[1].path.as_deref(), Some("/media/alt.mkv"));

        // Every source carries ITS OWN attachments (C# `MediaAttachments =
        // MediaSourceManager.GetMediaAttachments(item.Id)`), on both paths.
        let own = |s: &MediaSourceInfo| {
            s.media_attachments
                .iter()
                .map(|a| a.file_name.clone().unwrap_or_default())
                .collect::<Vec<_>>()
        };
        let alt_id = Uuid::parse_str(sources[1].id.as_deref().unwrap()).unwrap();
        assert_eq!(own(&sources[0]), [format!("{}.ttf", id.simple())]);
        assert_eq!(own(&sources[1]), [format!("{}.ttf", alt_id.simple())]);
        assert_eq!(own(&batch_sources[0]), [format!("{}.ttf", id.simple())]);
        assert_eq!(own(&batch_sources[1]), [format!("{}.ttf", alt_id.simple())]);
    }

    // The two book kinds project the fields Jellyfin's DtoService gives them —
    // and, just as importantly, not the ones it withholds. An audiobook is an
    // `Audio` but hangs off its books library, so the `AlbumEntity` lookup
    // (`FindParent<MusicAlbum>`) finds nothing and no AlbumId is emitted;
    // pointing it at the collection folder would send jellyfin-web's
    // now-playing bar to a page that is not an album.
    #[tokio::test]
    async fn book_kinds_project_the_fields_jellyfin_gives_them() {
        let db = test_db().await;
        let library = Uuid::new_v4();
        let album = Uuid::new_v4();
        let (book_id, audiobook_id, track_id) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        seed_named_item(&db, book_id, BaseItemKind::Book, "A Study in Scarlet").await;
        seed_named_item(&db, audiobook_id, BaseItemKind::AudioBook, "The Hobbit").await;
        seed_named_item(&db, track_id, BaseItemKind::Audio, "In the Flesh").await;

        let mut book = fetch_item(&db, book_id).await;
        book.series_name = Some("Sherlock Holmes".to_owned());
        let mut audiobook = fetch_item(&db, audiobook_id).await;
        audiobook.parent_id = Some(guid_to_db(library));
        audiobook.series_name = Some("Sprawl".to_owned());
        let mut track = fetch_item(&db, track_id).await;
        track.parent_id = Some(guid_to_db(album));

        let svc = service(db);
        let dto = async |item| {
            svc.get_base_item_dto(item, &DtoOptions::default(), None, None)
                .await
                .unwrap()
        };

        // `SetBookProperties` projects a book's series…
        let book_dto = dto(&book).await;
        assert_eq!(book_dto.series_name.as_deref(), Some("Sherlock Holmes"));
        // …and a Book is not `IHasMediaSources`, so IsFolder stays absent.
        assert_eq!(book_dto.is_folder, None);

        let audiobook_dto = dto(&audiobook).await;
        assert_eq!(
            audiobook_dto.album_id, None,
            "an audiobook's parent is its library, not a MusicAlbum"
        );
        assert_eq!(audiobook_dto.is_folder, Some(false));
        assert_eq!(
            audiobook_dto.series_name, None,
            "upstream has no SetAudioBookProperties"
        );

        // A real track still links back to its album row.
        assert_eq!(dto(&track).await.album_id, Some(album));
    }

    /// A synthetic Live TV channel entity, as `ferrofin-livetv` builds them
    /// (`type_` is the stored `LiveTvChannel` name; no path — the tuner URL is
    /// resolved at stream time, exactly like upstream's channel items).
    fn live_tv_channel_entity(id: Uuid) -> BaseItemEntity {
        BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(id),
            type_: "MediaBrowser.Controller.LiveTv.LiveTvChannel".to_owned(),
            name: Some("Parity One".to_owned()),
            media_type: Some("Video".to_owned()),
            sort_name: Some("00001.0-Parity One".to_owned()),
            date_created: Some(Utc.with_ymd_and_hms(2026, 8, 23, 18, 0, 0).unwrap()),
            is_folder: false,
            ..BaseItemEntity::default()
        }
    }

    // The Live TV channel kind hooks: C# `LiveTvChannel` overrides LocationType
    // to Remote, IsFolder to false (IHasMediaSources), CanDelete to false, the
    // client type name to "TvChannel", and its media sources to the one
    // Placeholder source with empty MediaStreams.
    #[tokio::test]
    async fn live_tv_channel_dto_carries_the_upstream_overrides() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        let entity = live_tv_channel_entity(id);
        let svc = service(db);

        let dto = svc
            .get_base_item_dto(&entity, &DtoOptions::default(), None, None)
            .await
            .unwrap();

        assert_eq!(dto.type_, BaseItemKind::TvChannel);
        assert_eq!(
            dto.location_type,
            Some(ferrofin_model::entities::LocationType::Remote)
        );
        assert_eq!(dto.is_folder, Some(false));
        assert_eq!(dto.can_delete, Some(false));
        assert_eq!(dto.can_download, Some(false));
        assert_eq!(dto.media_type, MediaType::Video);
        assert_eq!(dto.sort_name.as_deref(), Some("00001.0-Parity One"));
        assert!(dto.date_created.is_some());
        // The all-fields channel detail carries a present-and-empty stream list
        // (C# assigns `GetMediaStreams()` = [] for every IHasMediaSources).
        assert_eq!(dto.media_streams, Some(Vec::new()));
        // C# only assigns Chapters for a `Video`; a channel omits the key.
        assert_eq!(dto.chapters, None);

        let sources = dto.media_sources.expect("placeholder media source");
        assert_eq!(sources.len(), 1);
        let source = &sources[0];
        assert_eq!(source.id.as_deref(), Some(id.simple().to_string().as_str()));
        assert_eq!(
            source.type_,
            ferrofin_model::dto::MediaSourceType::Placeholder
        );
        assert_eq!(
            source.protocol,
            ferrofin_model::media_info::MediaProtocol::File
        );
        assert!(source.is_infinite_stream, "no runtime → infinite");
        assert!(source.media_streams.is_empty());
        assert_eq!(source.path, None);
    }

    // The Live TV programme kind hooks: "Program" client type name, no
    // LocationType, no IsFolder, CanDelete/CanDownload false, MediaType
    // passes through as Unknown, and no media sources (a programme is not
    // IHasMediaSources).
    #[tokio::test]
    async fn live_tv_program_dto_omits_location_and_folder_and_sources() {
        let db = test_db().await;
        let channel = Uuid::new_v4();
        let entity = BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(Uuid::new_v4()),
            type_: "MediaBrowser.Controller.LiveTv.LiveTvProgram".to_owned(),
            name: Some("Parity Show".to_owned()),
            media_type: Some("Unknown".to_owned()),
            channel_id: Some(ferrofin_db::store::guid_to_db(channel)),
            parent_id: Some(ferrofin_db::store::guid_to_db(channel)),
            end_date: Some(Utc.with_ymd_and_hms(2026, 8, 23, 19, 0, 0).unwrap()),
            run_time_ticks: Some(36_000_000_000),
            genres: Some("News".to_owned()),
            tags: Some("News".to_owned()),
            is_folder: false,
            ..BaseItemEntity::default()
        };
        let svc = service(db);

        let dto = svc
            .get_base_item_dto(&entity, &DtoOptions::default(), None, None)
            .await
            .unwrap();

        assert_eq!(dto.type_, BaseItemKind::Program);
        assert_eq!(
            dto.location_type, None,
            "C# skips LocationType for programmes"
        );
        assert_eq!(dto.is_folder, None, "not a folder, not IHasMediaSources");
        assert_eq!(dto.can_delete, Some(false));
        assert_eq!(dto.can_download, Some(false));
        assert_eq!(dto.media_type, MediaType::Unknown);
        assert_eq!(dto.media_sources, None);
        assert_eq!(dto.media_streams, None);
        assert_eq!(dto.chapters, None, "a programme is not a Video");
        assert_eq!(dto.channel_id, Some(channel));
        assert_eq!(dto.parent_id, Some(channel));
        assert_eq!(dto.run_time_ticks, Some(36_000_000_000));
        assert_eq!(dto.genres, Some(vec!["News".to_owned()]));
        assert_eq!(dto.tags, Some(vec!["News".to_owned()]));
    }
}
