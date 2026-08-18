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
//! sources/streams), [`ChapterManager`], [`TrickplayManager`], and
//! [`ProviderManager`] (external URLs). The `server_id` string the C# code reads
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
//! branches are skipped and flagged. `CanDelete`/`CanDownload`/`Etag` collapse
//! to thin defaults (the C# logic needs the domain tree). Everything else — the
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
use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::{BaseItemEntity, BaseItemImageInfoEntity};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::{BaseItemKind, MediaType};
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
use ferrofin_traits::persistence::ItemCountService;
use ferrofin_traits::providers::ProviderManager;
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
    /// Provider-id maps per item id (populated only when the `ProviderIds`
    /// field is requested).
    provider_ids: HashMap<Uuid, HashMap<String, String>>,
    /// Credited people per item id (populated only when the `People` field is
    /// requested), so a page's cast/crew loads in one query.
    people: HashMap<Uuid, Vec<ferrofin_db::entities::base_items::PeopleEntity>>,
    /// Image rows per *person* id, for the whole page's cast/crew at once, so the
    /// primary-image tag lookup does not re-query per person per item.
    person_images: HashMap<Uuid, Vec<ItemImageInfo>>,
    /// `ItemValues` id per `(value type, clean value)` for every studio/genre/
    /// artist name across the page, so `attach_studios`/`_genres`/`_artists`
    /// resolve from memory instead of a query per name.
    value_ids: HashMap<(i32, String), Uuid>,
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
    /// The requesting user's content permissions (populated only when the
    /// `CanDelete`/`CanDownload` fields are requested and a user is present),
    /// so the whole page gates on one `Permissions` query.
    content_permissions: Option<UserContentPermissions>,
    /// The per-NAME `Person` item id for each credited name on the page
    /// (lowercased), so `People[].Id` points at the favoritable by-name item.
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
    /// A studio/genre/artist id from the prefetched `ItemValues` map — the nil
    /// id when the name has no stored value row, exactly as the per-name lookup
    /// resolved a missing row.
    fn value_id(&self, value_type: i32, name: &str) -> Uuid {
        let clean = crate::text_util::get_clean_value(name);
        self.value_ids
            .get(&(value_type, clean))
            .copied()
            .unwrap_or_else(Uuid::nil)
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

/// Parses the row's stored `Guid` id into a [`Uuid`], or the nil UUID on a
/// malformed value (a stored id should always parse).
fn row_id(item: &BaseItemEntity) -> Uuid {
    Uuid::parse_str(&item.id).unwrap_or_else(|_| Uuid::nil())
}

/// The [`BaseItemKind`] of a row, defaulting to [`BaseItemKind::Folder`] for an
/// unrecognized stored `Type` (the conservative default used across the crate).
fn row_kind(item: &BaseItemEntity) -> BaseItemKind {
    kind_from_type_name(&item.type_).unwrap_or(BaseItemKind::Folder)
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
/// Collection folders and user views skip the count: C# returns
/// `Random.Shared.Next(1, 10)` there ("too slow to calculate for top level
/// folders on a per-user basis — just return something so that apps that are
/// expecting a value won't think the folders are empty"); an id-derived 1..=9
/// honors the same contract (nonzero, meaningless) without a rand dependency.
fn attach_child_count(dto: &mut BaseItemDto, item: &BaseItemEntity, counts: &HashMap<Uuid, i32>) {
    if dto.child_count.is_some() || !item.is_folder {
        return;
    }
    let id = row_id(item);
    dto.child_count = Some(match row_kind(item) {
        BaseItemKind::CollectionFolder | BaseItemKind::UserView => {
            i32::from(id.as_bytes()[15] % 9) + 1
        }
        _ => counts.get(&id).copied().unwrap_or(0),
    });
}

/// Splits a stored pipe-delimited multi-value column into a list, dropping
/// empties. Jellyfin joins `Genres`/`Studios`/`Artists`/… with `|`.
fn split_multi(stored: Option<&str>) -> Vec<String> {
    stored
        .map(|s| {
            s.split('|')
                .filter(|p| !p.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
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
    providers: Arc<dyn ProviderManager>,
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
        providers: Arc<dyn ProviderManager>,
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
            providers,
        }
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
        // 500 stays far below SQLite's conservative 999-host-variable floor.
        for chunk in item_ids.chunks(500) {
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

    /// Resolves many `(value type, name)` pairs to their `ItemValues` ids in one
    /// query, keyed by `(type, clean value)` — the page's studios/genres/artists.
    /// Pairs with no row are simply absent.
    ///
    /// Port of the `_libraryManager.GetGenreId`/`GetStudioId`/… helpers, which
    /// hash-map a clean value to a stable id; here the stored `ItemValues` row
    /// already carries that id, so a lookup keyed by `(Type, CleanValue)`
    /// suffices.
    async fn resolve_value_ids(
        &self,
        pairs: &[(i32, String)],
    ) -> Result<HashMap<(i32, String), Uuid>, ServiceError> {
        let mut map = HashMap::new();
        // Dedup the (type, clean) keys we need.
        let mut want: std::collections::HashSet<(i32, String)> = std::collections::HashSet::new();
        for (t, name) in pairs {
            want.insert((*t, crate::text_util::get_clean_value(name)));
        }
        if want.is_empty() {
            return Ok(map);
        }
        let keys: Vec<(i32, String)> = want.into_iter().collect();
        for chunk in keys.chunks(500) {
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
                    map.insert((t, clean), uuid);
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
            let person_id = prefetched
                .person_ids_by_name
                .get(&person.name.to_lowercase())
                .copied()
                .unwrap_or_else(|| Uuid::parse_str(&person.id).unwrap_or_else(|_| Uuid::nil()));
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
                    id: prefetched
                        .value_ids
                        .get(&(1, crate::text_util::get_clean_value(name)))
                        .copied()
                        .unwrap_or_else(|| prefetched.value_id(0, name)),
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
            type_: kind,
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

        // Display-preferences id (the item id in `N` form, hyphen-stripped).
        if options.contains_field(ItemFields::DisplayPreferencesId) {
            dto.display_preferences_id = Some(item_id.simple().to_string());
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
                // Folder UserData carries UnplayedItemCount = unplayed leaf descendants
                // (C# AttachUserSpecificInfo folder branch); leaf items leave it unset.
                // The branch keys on the runtime C# `IsFolder` (`folder_emits_counts`):
                // pure by-name kinds never enter it, a MusicArtist only when
                // physically parented.
                if folder_emits_counts(item)
                    && !matches!(
                        kind,
                        BaseItemKind::CollectionFolder | BaseItemKind::UserView
                    )
                    && let Some(c) = prefetched.played_counts.get(&item_id).copied()
                {
                    let ud = dto
                        .user_data
                        .get_or_insert_with(|| empty_user_data_dto(item_id));
                    ud.unplayed_item_count = Some(c.total - c.played);
                }
            }
        }

        // Media sources. Jellyfin only attaches these for `IHasMediaSources`
        // (video/audio) — a Genre/Studio/Person/folder has no playable source, so
        // it must not carry a spurious one (C# `DtoService` gates on the interface).
        if options.contains_field(ItemFields::MediaSources)
            && (kinds::is_video(kind) || kinds::is_audio(kind))
        {
            // The row and its streams are already prefetched, so assemble the
            // static source directly — no per-item retrieve_item + streams_dto.
            let streams = prefetched
                .media_streams
                .get(&item_id)
                .cloned()
                .unwrap_or_default();
            let mut sources = vec![
                crate::media_source_manager::FerrofinMediaSourceManager::static_source(
                    item, streams,
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
                sources.push(
                    crate::media_source_manager::FerrofinMediaSourceManager::static_source(
                        alt,
                        alt_streams,
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
        if options.contains_field(ItemFields::CanDelete) {
            // By-name items (Genre/Studio/Person/…) have no file — C# `CanDelete()`
            // returns false (default `IsFileProtocol`, plus explicit overrides).
            let file_deletable = !item.is_virtual_item && !kinds::is_item_by_name(kind);
            dto.can_delete = Some(file_deletable && perms.is_none_or(|p| p.can_delete));
        }
        if options.contains_field(ItemFields::CanDownload) {
            // C# `CanDownload()` is false by default and only true for playable media;
            // a by-name item is not a folder but still isn't downloadable.
            let file_downloadable = !item.is_folder && !kinds::is_item_by_name(kind);
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

        if options.contains_field(ItemFields::DateCreated) {
            dto.date_created = item.date_created;
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
            dto.external_urls = Some(self.providers.get_external_urls(item_id).await?);
        }

        if options.contains_field(ItemFields::Tags) {
            dto.tags = Some(split_multi(item.tags.as_deref()));
        }

        // Images (single-type tags + backdrops).
        self.attach_images(dto, item_id, images, options).await;

        // Width/Height come from the primary image (C# reads item.GetImageInfo(Primary, 0)),
        // gated by their own ItemFields. Zero dims (unscanned image) are treated as unknown.
        if let Some(primary) = images.iter().find(|i| i.image_type == ImageType::Primary) {
            if options.contains_field(ItemFields::Width) && primary.width > 0 {
                dto.width = Some(primary.width);
            }
            if options.contains_field(ItemFields::Height) && primary.height > 0 {
                dto.height = Some(primary.height);
            }
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
        } else if kinds::is_video(kind) || kinds::is_audio(kind) {
            dto.is_folder = Some(false);
        }

        dto.location_type = Some(if item.is_virtual_item {
            LocationType::Virtual
        } else {
            LocationType::FileSystem
        });

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
                .or_else(|| item.name.as_deref().map(crate::resolvers::sort_name));
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
            // bar and track lists link back through AlbumId.
            dto.album_id = item
                .parent_id
                .as_deref()
                .and_then(|p| Uuid::parse_str(p).ok());
        }

        // Artists / album-artists — only the kinds that implement C#
        // `IHasArtist`/`IHasAlbumArtist` (Audio, AudioBook, MusicAlbum,
        // MusicVideo) carry them; Jellyfin never emits artist fields elsewhere.
        if kinds::has_artist_fields(kind) {
            Self::attach_artists(dto, item, prefetched);
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
        if options.contains_field(ItemFields::Chapters) {
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
            let pinned = repeated || prefetched.alt_referenced.contains(&item_id);
            let streams =
                take_or_clone(&mut prefetched.media_streams, &item_id, pinned).unwrap_or_default();
            if !streams.is_empty() {
                dto.media_streams = Some(streams);
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
        }

        // Season extras.
        if kind == BaseItemKind::Season {
            dto.series_name = item.series_name.clone();
            dto.series_id = item
                .series_id
                .as_deref()
                .and_then(|s| Uuid::parse_str(s).ok());
        }

        // Series air-time.
        if kind == BaseItemKind::Series {
            dto.air_time = None; // no flat column at this layer
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
        for chunk in item_ids.chunks(500) {
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
        let mut by_kind: HashMap<BaseItemKind, Vec<Uuid>> = HashMap::new();
        for item in items {
            let kind = row_kind(item);
            if related_item_kinds(kind).is_some() {
                by_kind.entry(kind).or_default().push(row_id(item));
            }
        }
        let access_filter = access_filter_for(user);
        let mut out = HashMap::new();
        for (kind, ids) in by_kind {
            let related = related_item_kinds(kind).unwrap_or(&[]);
            out.extend(
                self.item_counts
                    .get_item_counts_for_name_items(kind, &ids, related, &access_filter)
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
        // Images and user-data are independent; run them concurrently.
        let want_images =
            options.enable_images || options.contains_field(ItemFields::PrimaryImageAspectRatio);
        let want_user_data = user.is_some() && options.enable_user_data;
        let images_fut = async {
            if want_images {
                self.load_images_batch(&ids).await
            } else {
                Ok(HashMap::new())
            }
        };
        let user_data_fut = async {
            if want_user_data && let Some(u) = user {
                let user_id = Uuid::parse_str(&u.id).unwrap_or_else(|_| Uuid::nil());
                self.user_data.get_user_data_dtos(&ids, user_id).await
            } else {
                Ok(HashMap::new())
            }
        };
        let (images, user_data) = tokio::try_join!(images_fut, user_data_fut)?;
        // Merged alternate versions (rows pointing at a page item via
        // `PrimaryVersionId`), so each item's extra selectable sources build
        // without a per-item query; their streams join the stream batch below.
        let alternates = if options.contains_field(ItemFields::MediaSources) {
            self.media_sources
                .get_alternate_versions_batch(&ids)
                .await?
        } else {
            HashMap::new()
        };
        // The heavy per-item relations, bulk-loaded once for the page when their
        // field is requested (an all-fields list DTO otherwise fans out a query
        // per item for each — costly on the 2-connection pool).
        let media_streams = if options.contains_field(ItemFields::MediaStreams)
            || options.contains_field(ItemFields::MediaSources)
        {
            let stream_ids: Vec<Uuid> = ids
                .iter()
                .copied()
                .chain(alternates.values().flatten().map(row_id))
                .collect();
            self.media_sources
                .get_media_streams_batch(&stream_ids)
                .await?
        } else {
            HashMap::new()
        };
        // An id listed here is another page item's alternate, so its streams are
        // read while projecting that item — it cannot be drained by its own.
        let alt_referenced: std::collections::HashSet<Uuid> =
            alternates.values().flatten().map(row_id).collect();
        let provider_ids = if options.contains_field(ItemFields::ProviderIds) {
            self.load_provider_ids_batch(&ids).await?
        } else {
            HashMap::new()
        };
        // People for the page, then every credited person's images in one further
        // query — attach_people otherwise runs get_people + load_images per item.
        let (people, person_images, person_ids_by_name) =
            if options.contains_field(ItemFields::People) {
                let people = self.library.get_people_batch(&ids).await?;
                // Resolve each distinct credit NAME to its by-name Person item
                // (C# AttachPeople: `People[].Id` is the per-name item id, the
                // one favorites are written against — never the per-credit
                // `Peoples` row id, which fragments a person across types).
                let mut names: Vec<String> = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for person in people.values().flatten() {
                    if seen.insert(person.name.to_lowercase()) {
                        names.push(person.name.clone());
                    }
                }
                let resolved = self
                    .library
                    .get_named_items(ferrofin_model::data::BaseItemKind::Person, &names)
                    .await
                    .unwrap_or_default();
                let mut by_name: HashMap<String, Uuid> = HashMap::new();
                let mut person_ids: Vec<Uuid> = Vec::new();
                for (name, row) in names.iter().zip(resolved) {
                    if let Some(row) = row
                        && let Ok(id) = Uuid::parse_str(&row.id)
                    {
                        by_name.insert(name.to_lowercase(), id);
                        person_ids.push(id);
                    }
                }
                // Pre-unification rows keyed images on the credit id; keep
                // loading those too so old databases still render cast art.
                person_ids.extend(
                    people
                        .values()
                        .flatten()
                        .filter_map(|p| Uuid::parse_str(&p.id).ok()),
                );
                let images = self
                    .load_images_batch(&person_ids)
                    .await
                    .unwrap_or_default();
                (people, images, by_name)
            } else {
                (HashMap::new(), HashMap::new(), HashMap::new())
            };
        // Studio/genre/artist ids for every name on the page in one query. Collect
        // exactly what the attach steps resolve: studios/genres only when their
        // field is requested, artists/album-artists only for the kinds that carry
        // artist fields — so a prefetched miss never wrongly nils a real id.
        let value_ids = {
            let mut pairs: Vec<(i32, String)> = Vec::new();
            let want_studios = options.contains_field(ItemFields::Studios);
            let want_genres = options.contains_field(ItemFields::Genres);
            for item in items {
                if want_studios {
                    pairs.extend(
                        split_multi(item.studios.as_deref())
                            .into_iter()
                            .map(|n| (3, n)),
                    );
                }
                if want_genres {
                    pairs.extend(
                        split_multi(item.genres.as_deref())
                            .into_iter()
                            .map(|n| (2, n)),
                    );
                }
                if kinds::has_artist_fields(row_kind(item)) {
                    pairs.extend(
                        split_multi(item.artists.as_deref())
                            .into_iter()
                            .map(|n| (0, n)),
                    );
                    pairs.extend(
                        split_multi(item.album_artists.as_deref())
                            .into_iter()
                            .map(|n| (1, n)),
                    );
                }
            }
            self.resolve_value_ids(&pairs).await?
        };
        let chapters = if options.contains_field(ItemFields::Chapters) {
            self.chapters.get_chapters_batch(&ids).await?
        } else {
            HashMap::new()
        };
        let trickplay = if options.contains_field(ItemFields::Trickplay) {
            self.trickplay.get_trickplay_manifest_batch(&ids).await?
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
        // Subtitle presence for the page's videos in one ids-only query — C#
        // emits `HasSubtitles` on every video DTO regardless of `ItemFields`.
        let video_ids: Vec<Uuid> = items
            .iter()
            .filter(|i| kinds::is_video(row_kind(i)))
            .map(row_id)
            .collect();
        let has_subtitles = if video_ids.is_empty() {
            std::collections::HashSet::new()
        } else {
            self.media_sources
                .get_item_ids_with_subtitles(&video_ids)
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
                let user_id = Uuid::parse_str(&user.id).unwrap_or_else(|_| Uuid::nil());
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
            provider_ids,
            people,
            person_images,
            value_ids,
            chapters,
            trickplay,
            child_counts,
            played_counts,
            alternates,
            has_subtitles,
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

    use chrono::{DateTime, Utc};
    use ferrofin_db::entities::base_items::PeopleEntity;
    use ferrofin_db::entities::playback::TrickplayInfoEntity;
    use ferrofin_model::drawing::{ImageDimensions, ImageFormat};
    use ferrofin_model::dto::{MediaSourceInfo, UserItemDataDto};
    use ferrofin_model::entities_media::{ChapterInfo, MediaAttachment, MediaStream};
    use ferrofin_model::providers::ExternalUrl;
    use ferrofin_traits::drawing::ProcessedImage;
    use ferrofin_traits::dto::DtoService as _;

    use crate::test_support::{seed_named_item, seed_user, test_db};

    // ---- Fakes for the injected siblings -------------------------------------
    //
    // Each fake returns the empty/neutral value for every method the DTO paths
    // don't exercise, and a deterministic value for the few that matter.

    /// A [`LibraryManager`] fake: `get_people` returns a fixed list, everything
    /// else is empty/neutral.
    #[derive(Default)]
    struct FakeLibrary {
        people: Vec<PeopleEntity>,
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
    fn percent_of_ticks_is_the_played_percentage_division() {
        // 50% of a 2 h runtime — the C# `100 * position / runtime` double math.
        assert!((percent_of_ticks(36_000_000_000, 72_000_000_000) - 50.0).abs() < 1e-9);
        // Tiny fractions stay positive and precise enough for display.
        assert!(percent_of_ticks(1, 72_000_000_000) > 0.0);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // a flat sequence of per-kind assertions
    async fn folder_user_data_carries_unplayed_item_count() {
        let db = test_db().await;
        let folder_id = Uuid::new_v4();
        seed_named_item(&db, folder_id, BaseItemKind::Season, "Season 1").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "IsFolder" = 1 WHERE "Id" = ?1"#)
            .bind(guid_to_db(folder_id))
            .execute(db.writer())
            .await
            .expect("mark folder");
        let leaf_id = Uuid::new_v4();
        seed_named_item(&db, leaf_id, BaseItemKind::Movie, "A Movie").await;
        // A by-name row (Genre) stored IsFolder=1 but with no ancestor closure.
        let genre_id = Uuid::new_v4();
        seed_named_item(&db, genre_id, BaseItemKind::Genre, "Drama").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "IsFolder" = 1 WHERE "Id" = ?1"#)
            .bind(guid_to_db(genre_id))
            .execute(db.writer())
            .await
            .expect("mark by-name folder");
        // Two MusicArtist rows: accessed-by-name (no parent — C# `IsFolder` false)
        // and a physical artist folder (parented — C# `IsFolder` true).
        let byname_artist_id = Uuid::new_v4();
        seed_named_item(&db, byname_artist_id, BaseItemKind::MusicArtist, "ByName").await;
        let physical_artist_id = Uuid::new_v4();
        seed_named_item(&db, physical_artist_id, BaseItemKind::MusicArtist, "OnDisk").await;
        // One statement marks both artists IsFolder=1 and parents only the
        // physical one (the by-name artist keeps a NULL ParentId).
        sqlx::query(
            r#"UPDATE "BaseItems"
               SET "IsFolder" = 1,
                   "ParentId" = CASE "Id" WHEN ?2 THEN ?3 ELSE "ParentId" END
               WHERE "Id" IN (?1, ?2)"#,
        )
        .bind(guid_to_db(byname_artist_id))
        .bind(guid_to_db(physical_artist_id))
        .bind(guid_to_db(folder_id))
        .execute(db.writer())
        .await
        .expect("mark artist folders");
        let user = seed_user(&db, Uuid::new_v4()).await;
        let folder = fetch_item(&db, folder_id).await;
        let leaf = fetch_item(&db, leaf_id).await;
        let genre = fetch_item(&db, genre_id).await;
        let byname_artist = fetch_item(&db, byname_artist_id).await;
        let physical_artist = fetch_item(&db, physical_artist_id).await;
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

        // A physically-parented MusicArtist IS a folder at runtime — it carries
        // the count like any other folder (FakeCounts: 1/4 played → 3 unplayed).
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
            Some(3)
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
            Some(3)
        );
    }

    /// An [`ItemCountService`] fake returning fixed name-item counts.
    #[derive(Default)]
    struct FakeCounts;

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
    #[derive(Default)]
    struct FakeSources;

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
                    (
                        *id,
                        vec![MediaStream {
                            index: 0,
                            stream_type: ferrofin_model::entities::MediaStreamType::Video,
                            codec: Some("h264".to_owned()),
                            ..MediaStream::default()
                        }],
                    )
                })
                .collect())
        }
        async fn get_media_attachments(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<MediaAttachment>, ServiceError> {
            Ok(vec![])
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

    /// A [`ProviderManager`] fake: `get_external_urls` returns one link.
    #[derive(Default)]
    struct FakeProviders;

    #[async_trait]
    impl ProviderManager for FakeProviders {
        async fn queue_refresh(
            &self,
            _item_id: Uuid,
            _options: &ferrofin_traits::providers::MetadataRefreshOptions,
            _priority: ferrofin_traits::providers::RefreshPriority,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn refresh_full_item(
            &self,
            _item_id: Uuid,
            _options: &ferrofin_traits::providers::MetadataRefreshOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn refresh_single_item(
            &self,
            _item_id: Uuid,
            _options: &ferrofin_traits::providers::MetadataRefreshOptions,
        ) -> Result<ferrofin_traits::providers::ItemUpdateType, ServiceError> {
            Ok(ferrofin_traits::providers::ItemUpdateType::default())
        }
        async fn save_image_from_url(
            &self,
            _item_id: Uuid,
            _url: &str,
            _image_type: ImageType,
            _image_index: Option<i32>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn save_image(
            &self,
            _item_id: Uuid,
            _content: &[u8],
            _mime_type: &str,
            _image_type: ImageType,
            _image_index: Option<i32>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_available_remote_images(
            &self,
            _item_id: Uuid,
            _query: &ferrofin_model::providers::RemoteImageQuery,
        ) -> Result<Vec<ferrofin_model::providers::RemoteImageInfo>, ServiceError> {
            Ok(vec![])
        }
        async fn get_remote_image_provider_info(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<ferrofin_model::providers::ImageProviderInfo>, ServiceError> {
            Ok(vec![])
        }
        async fn save_metadata(
            &self,
            _item_id: Uuid,
            _update_type: ferrofin_traits::providers::ItemUpdateType,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_external_urls(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<ExternalUrl>, ServiceError> {
            Ok(vec![ExternalUrl {
                name: Some("IMDb".into()),
                url: Some("https://imdb.com/title/tt1".into()),
            }])
        }
        async fn get_external_id_infos(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<ferrofin_model::providers::ExternalIdInfo>, ServiceError> {
            Ok(vec![])
        }
        async fn get_all_metadata_plugins(
            &self,
        ) -> Result<Vec<ferrofin_model::configuration::MetadataPluginSummary>, ServiceError>
        {
            Ok(vec![])
        }
        async fn get_metadata_options(
            &self,
            _item_id: Uuid,
        ) -> Result<ferrofin_model::configuration::MetadataOptions, ServiceError> {
            Ok(ferrofin_model::configuration::MetadataOptions::default())
        }
        async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
            Ok(vec![])
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
            Arc::new(FakeCounts),
            Arc::new(FakeImages),
            Arc::new(FakeSources),
            Arc::new(FakeChapters),
            Arc::new(FakeTrickplay),
            Arc::new(FakeProviders),
        )
    }

    /// [`service`] with a chapter manager that has thumbnailed chapters.
    fn service_with_chapters(db: Database) -> FerrofinDtoService {
        FerrofinDtoService::new(
            db,
            "server-1".into(),
            Arc::new(FakeLibrary::default()),
            Arc::new(FakeUserData),
            Arc::new(FakeCounts),
            Arc::new(FakeImages),
            Arc::new(FakeSources),
            Arc::new(ChaptersWithImages),
            Arc::new(FakeTrickplay),
            Arc::new(FakeProviders),
        )
    }

    fn service(db: Database) -> FerrofinDtoService {
        service_with(db, Arc::new(FakeLibrary::default()))
    }

    /// Seeds one image row on an item.
    async fn seed_image(
        db: &Database,
        item_id: Uuid,
        image_type: i32,
        path: &str,
        blur: Option<&str>,
    ) {
        sqlx::query(
            r#"INSERT INTO "BaseItemImageInfos"
               ("Id", "ItemId", "ImageType", "Path", "Width", "Height", "Blurhash")
               VALUES (?1, ?2, ?3, ?4, 0, 0, ?5)"#,
        )
        .bind(guid_to_db(Uuid::new_v4()))
        .bind(guid_to_db(item_id))
        .bind(image_type)
        .bind(path)
        .bind(blur.map(|b| b.as_bytes().to_vec()))
        .execute(db.writer())
        .await
        .expect("insert image");
    }

    /// Reads back a full item row.
    async fn fetch_item(db: &Database, id: Uuid) -> BaseItemEntity {
        sqlx::query_as::<_, BaseItemEntity>(r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .fetch_one(db.pool())
            .await
            .expect("fetch item")
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
        sqlx::query(
            r#"INSERT INTO "BaseItemProviders" ("ItemId", "ProviderId", "ProviderValue")
               VALUES (?1, 'Imdb', 'tt1375666')"#,
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

        assert_eq!(dto.provider_ids.as_ref().unwrap()["Imdb"], "tt1375666");
        assert_eq!(dto.external_urls.as_ref().unwrap().len(), 1);
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
        seed_image(&db, id, 0, "/primary.jpg", Some("LKO2")).await;
        sqlx::query(
            r#"INSERT INTO "BaseItemProviders" ("ItemId", "ProviderId", "ProviderValue")
               VALUES (?1, 'Imdb', 'tt1375666')"#,
        )
        .bind(guid_to_db(id))
        .execute(db.writer())
        .await
        .unwrap();
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
        seed_image(&db, id, 0, "/primary.jpg", Some("LKO2")).await;
        seed_image(&db, id, 2, "/backdrop.jpg", None).await;
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
        seed_image(&db, id, 0, "/primary.jpg", None).await;
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
        seed_named_item(&db, id, BaseItemKind::Season, "Season 1").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "IsFolder" = 1 WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .execute(db.writer())
            .await
            .expect("mark folder");
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

    #[tokio::test]
    async fn child_count_placeholder_for_collection_folders() {
        // C# `GetChildCount` returns a random 1..10 for collection folders and
        // user views instead of a real count; the port derives a stable 1..=9.
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::CollectionFolder, "Shows").await;
        sqlx::query(r#"UPDATE "BaseItems" SET "IsFolder" = 1 WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .execute(db.writer())
            .await
            .expect("mark folder");
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
            sqlx::query_as::<_, BaseItemEntity>(r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)
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
        let map = svc
            .resolve_value_ids(&[(3, "Warner Bros.".to_string())])
            .await
            .unwrap();
        assert_eq!(map.get(&(3, clean)).copied(), Some(vid));

        // Prefetched::value_id reads the map without a query, and nil-s a name
        // with no row.
        let pf = Prefetched {
            value_ids: map,
            ..Prefetched::default()
        };
        assert_eq!(pf.value_id(3, "Warner Bros."), vid);
        assert!(pf.value_id(3, "Nobody").is_nil());
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
    }
}
