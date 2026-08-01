//! [`HermitDtoService`] — the concrete [`DtoService`] (entity → `BaseItemDto`).
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
//! properties. Hermit has no such object graph — a DTO is built from a flat
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
//! exactly as the sibling managers read `hermit-db` for data with no repository
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
use hermit_db::Database;
use hermit_db::entities::base_items::{BaseItemEntity, BaseItemImageInfoEntity};
use hermit_db::entities::users::UserEntity;
use hermit_model::data::{BaseItemKind, MediaType};
use hermit_model::dto::{
    BaseItemDto, BaseItemPerson, NameGuidPair, TrickplayInfoDto, UserItemDataDto,
};
use hermit_model::entities::{ExtraType, ImageType, LocationType, VideoType};
use hermit_model::querying::ItemFields;
use uuid::Uuid;

use hermit_traits::chapters::ChapterManager;
use hermit_traits::drawing::ImageProcessor;
use hermit_traits::error::ServiceError;
use hermit_traits::library::{LibraryManager, MediaSourceManager, UserDataManager};
use hermit_traits::options::{DtoOptions, ItemImageInfo};
use hermit_traits::persistence::ItemCountService;
use hermit_traits::providers::ProviderManager;
use hermit_traits::trickplay::TrickplayManager;

use crate::db_error::db_err;
use crate::item_type_lookup::kind_from_type_name;

/// Relation rows bulk-loaded for a whole page of items, so `build_dto` needs
/// no per-item queries for them (list endpoints); absent entries mean "no rows"
/// for that item, not "not prefetched".
#[derive(Default)]
struct Prefetched {
    /// Image rows per item id (same order as [`HermitDtoService::load_images`]).
    images: HashMap<Uuid, Vec<ItemImageInfo>>,
    /// The requesting user's play-state per item id.
    user_data: HashMap<Uuid, UserItemDataDto>,
    /// Media streams per item id (populated only when the `MediaStreams` field
    /// is requested), so a page builds them in one query instead of N.
    media_streams: HashMap<Uuid, Vec<hermit_model::entities_media::MediaStream>>,
    /// Provider-id maps per item id (populated only when the `ProviderIds`
    /// field is requested).
    provider_ids: HashMap<Uuid, HashMap<String, String>>,
    /// Credited people per item id (populated only when the `People` field is
    /// requested), so a page's cast/crew loads in one query.
    people: HashMap<Uuid, Vec<hermit_db::entities::base_items::PeopleEntity>>,
    /// Image rows per *person* id, for the whole page's cast/crew at once, so the
    /// primary-image tag lookup does not re-query per person per item.
    person_images: HashMap<Uuid, Vec<ItemImageInfo>>,
    /// `ItemValues` id per `(value type, clean value)` for every studio/genre/
    /// artist name across the page, so `attach_studios`/`_genres`/`_artists`
    /// resolve from memory instead of a query per name.
    value_ids: HashMap<(i32, String), Uuid>,
    /// Chapters per item id (populated only when the `Chapters` field is requested).
    chapters: HashMap<Uuid, Vec<hermit_model::entities_media::ChapterInfo>>,
    /// Trickplay manifest per item id (populated only when the `Trickplay` field
    /// is requested).
    trickplay: HashMap<
        Uuid,
        HashMap<String, HashMap<i32, hermit_db::entities::playback::TrickplayInfoEntity>>,
    >,
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
pub struct HermitDtoService {
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

impl std::fmt::Debug for HermitDtoService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitDtoService")
            .field("server_id", &self.server_id)
            .finish_non_exhaustive()
    }
}

impl HermitDtoService {
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
        .bind(item_id.to_string())
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
                query = query.bind(id.to_string());
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

    /// Resolves a by-name value (genre/studio/…) to its `ItemValues` id, or the
    /// nil UUID when it has no stored value row.
    ///
    /// Port of the `_libraryManager.GetGenreId`/`GetStudioId`/… helpers, which
    /// hash-map a clean value to a stable id; here the stored `ItemValues` row
    /// already carries that id, so a single lookup keyed by `(Type, CleanValue)`
    /// suffices.
    async fn value_id(&self, value_type: i32, name: &str) -> Result<Uuid, ServiceError> {
        let clean = crate::text_util::get_clean_value(name);
        let stored: Option<String> = sqlx::query_scalar(
            r#"SELECT "ItemValueId" FROM "ItemValues"
               WHERE "Type" = ?1 AND "CleanValue" = ?2 LIMIT 1"#,
        )
        .bind(value_type)
        .bind(&clean)
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;

        Ok(stored
            .and_then(|s| Uuid::parse_str(&s).ok())
            .unwrap_or_else(Uuid::nil))
    }

    /// Resolves many `(value type, name)` pairs to their `ItemValues` ids in one
    /// query, keyed by `(type, clean value)`. The batch form of [`Self::value_id`]
    /// for a page's studios/genres/artists. Pairs with no row are simply absent.
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

    /// A studio/genre/artist id: from the prefetched map on a page projection
    /// (absent ⇒ the nil id, as [`Self::value_id`] returns for a missing row),
    /// else the per-item query for single-item callers.
    async fn value_id_for(
        &self,
        prefetched: Option<&Prefetched>,
        value_type: i32,
        name: &str,
    ) -> Result<Uuid, ServiceError> {
        if let Some(p) = prefetched {
            let clean = crate::text_util::get_clean_value(name);
            return Ok(p
                .value_ids
                .get(&(value_type, clean))
                .copied()
                .unwrap_or_else(Uuid::nil));
        }
        self.value_id(value_type, name).await
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
        prefetched: Option<&Prefetched>,
    ) -> Result<(), ServiceError> {
        let item_id = row_id(item);
        // On a page projection the credits and their images were bulk-loaded once;
        // otherwise fetch this item's people and, in one query, their image rows
        // (the N+1 `load_images` per cast member is the cost of a large-cast item).
        // Failure of the image load stays lenient (no tags), as before.
        let owned_people;
        let owned_images;
        let (people, images_by_person): (
            &[hermit_db::entities::base_items::PeopleEntity],
            &HashMap<Uuid, Vec<ItemImageInfo>>,
        ) = if let Some(p) = prefetched {
            (
                p.people.get(&item_id).map_or(&[][..], Vec::as_slice),
                &p.person_images,
            )
        } else {
            owned_people = self
                .library
                .get_people(&hermit_traits::options::InternalPeopleQuery {
                    item_id,
                    ..Default::default()
                })
                .await?;
            let person_ids: Vec<Uuid> = owned_people
                .iter()
                .map(|p| Uuid::parse_str(&p.id).unwrap_or_else(|_| Uuid::nil()))
                .collect();
            owned_images = self
                .load_images_batch(&person_ids)
                .await
                .unwrap_or_default();
            (&owned_people, &owned_images)
        };

        let mut list = Vec::with_capacity(people.len());
        for person in people {
            let person_id = Uuid::parse_str(&person.id).unwrap_or_else(|_| Uuid::nil());
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
                    .map_or(hermit_model::data::PersonKind::Unknown, |t| {
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
    async fn attach_studios(
        &self,
        dto: &mut BaseItemDto,
        item: &BaseItemEntity,
        prefetched: Option<&Prefetched>,
    ) -> Result<(), ServiceError> {
        let studios = split_multi(item.studios.as_deref());
        let mut pairs = Vec::with_capacity(studios.len());
        for name in studios {
            let id = self.value_id_for(prefetched, 3, &name).await?; // 3 = Studios
            pairs.push(NameGuidPair {
                name: Some(name),
                id,
            });
        }
        dto.studios = Some(pairs);
        Ok(())
    }

    /// Attaches the item's genres as names and as name/id pairs (port of the
    /// `Genres`/`AttachGenreItems` block).
    async fn attach_genres(
        &self,
        dto: &mut BaseItemDto,
        item: &BaseItemEntity,
        kind: BaseItemKind,
        prefetched: Option<&Prefetched>,
    ) -> Result<(), ServiceError> {
        let genres = split_multi(item.genres.as_deref());
        // Music items resolve against the MusicGenre value space; everything else
        // against the plain Genre space. Both are stored as `ItemValueType::Genre`
        // (2) in this schema, so the id lookup is the same table.
        let _is_music_genres = kinds::is_music(kind);
        let mut pairs = Vec::with_capacity(genres.len());
        for name in &genres {
            let id = self.value_id_for(prefetched, 2, name).await?; // 2 = Genre
            pairs.push(NameGuidPair {
                name: Some(name.clone()),
                id,
            });
        }
        dto.genre_items = Some(pairs);
        dto.genres = Some(genres);
        Ok(())
    }

    /// Attaches artist / album-artist names and name-id pairs (port of the
    /// `IHasArtist`/`IHasAlbumArtist` blocks). Artist item ids are resolved from
    /// the shared `ItemValues` table (`Artist`/`AlbumArtist` value types).
    async fn attach_artists(
        &self,
        dto: &mut BaseItemDto,
        item: &BaseItemEntity,
        prefetched: Option<&Prefetched>,
    ) -> Result<(), ServiceError> {
        let artists = split_multi(item.artists.as_deref());
        if !artists.is_empty() {
            let mut items = Vec::with_capacity(artists.len());
            for name in &artists {
                let id = self.value_id_for(prefetched, 0, name).await?; // 0 = Artist
                items.push(NameGuidPair {
                    name: Some(name.clone()),
                    id,
                });
            }
            dto.artists = Some(artists);
            dto.artist_items = Some(items);
        }

        let album_artists = split_multi(item.album_artists.as_deref());
        if !album_artists.is_empty() {
            dto.album_artist = album_artists.first().cloned();
            let mut items = Vec::with_capacity(album_artists.len());
            for name in &album_artists {
                let id = self.value_id_for(prefetched, 1, name).await?; // 1 = AlbumArtist
                items.push(NameGuidPair {
                    name: Some(name.clone()),
                    id,
                });
            }
            dto.album_artists = Some(items);
        }
        Ok(())
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
            if !image_tags.is_empty() {
                dto.image_tags = Some(image_tags);
            }
        }

        // Drop the blurhash map if nothing was recorded, so the wire form omits it.
        if dto
            .image_blur_hashes
            .as_ref()
            .is_some_and(HashMap::is_empty)
        {
            dto.image_blur_hashes = None;
        }
    }

    /// Builds the full DTO for one item row (port of `GetBaseItemDtoInternal` +
    /// `AttachBasicFields`), honoring every [`DtoOptions`] toggle.
    ///
    /// `prefetched` carries relation rows bulk-loaded for a whole page (list
    /// endpoints); `None` falls back to per-item queries (single-item callers).
    #[allow(clippy::too_many_lines)]
    async fn build_dto(
        &self,
        item: &BaseItemEntity,
        options: &DtoOptions,
        user: Option<&UserEntity>,
        owner_id: Option<Uuid>,
        prefetched: Option<&Prefetched>,
    ) -> Result<BaseItemDto, ServiceError> {
        let item_id = row_id(item);
        let kind = row_kind(item);

        let images = if options.enable_images
            || options.contains_field(ItemFields::PrimaryImageAspectRatio)
        {
            match prefetched {
                Some(p) => p.images.get(&item_id).cloned().unwrap_or_default(),
                None => self.load_images(item_id).await?,
            }
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
        if let Some(user) = user {
            let user_id = Uuid::parse_str(&user.id).unwrap_or_else(|_| Uuid::nil());
            // C# `item.GetPlayAccess(user)` — Full unless parental control blocks it (not ported).
            if options.contains_field(ItemFields::PlayAccess) {
                dto.play_access = Some(hermit_model::library::PlayAccess::Full);
            }
            if options.enable_user_data {
                dto.user_data = match prefetched {
                    Some(p) => p.user_data.get(&item_id).cloned(),
                    None => self.user_data.get_user_data_dto(item_id, user_id).await?,
                };
            }
        }

        // Media sources.
        if options.contains_field(ItemFields::MediaSources) {
            // On a page projection we hold the row and its streams already, so
            // assemble the static source directly — no per-item retrieve_item +
            // streams_dto. Falls back to the manager for single-item callers.
            let sources = if let Some(p) = prefetched {
                let streams = p.media_streams.get(&item_id).cloned().unwrap_or_default();
                vec![
                    crate::media_source_manager::HermitMediaSourceManager::static_source(
                        item, streams,
                    ),
                ]
            } else {
                let user_id = user.and_then(|u| Uuid::parse_str(&u.id).ok());
                self.media_sources
                    .get_static_media_sources(item_id, true, user_id)
                    .await?
            };
            if !sources.is_empty() {
                dto.media_sources = Some(sources);
            }
        }

        // Studios.
        if options.contains_field(ItemFields::Studios) {
            self.attach_studios(&mut dto, item, prefetched).await?;
        }

        self.attach_basic_fields(&mut dto, item, kind, &images, options, owner_id, prefetched)
            .await?;

        // Can-delete / can-download collapse to thin defaults (the C# logic needs
        // the domain tree; see the module docs).
        if options.contains_field(ItemFields::CanDelete) {
            dto.can_delete = Some(false);
        }
        if options.contains_field(ItemFields::CanDownload) {
            dto.can_download = Some(!item.is_folder);
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
        prefetched: Option<&Prefetched>,
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
            self.attach_genres(dto, item, kind, prefetched).await?;
        }

        dto.index_number = item.index_number.and_then(|n| i32::try_from(n).ok());
        dto.parent_index_number = item.parent_index_number.and_then(|n| i32::try_from(n).ok());

        if item.is_folder {
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
            // Remote trailers are a per-item collection with no flat column at
            // this layer; left empty until the trailer-types join is wired.
            dto.remote_trailers = Some(Vec::new());
        }

        dto.name = item.name.clone();
        dto.original_title = item.original_title.clone();
        dto.official_rating = item.official_rating.clone();
        dto.original_language = item.original_language.clone();
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
            dto.provider_ids = Some(match prefetched {
                Some(p) => p.provider_ids.get(&item_id).cloned().unwrap_or_default(),
                None => self.load_provider_ids(item_id).await?, // {} when none
            });
        }

        dto.run_time_ticks = item.run_time_ticks;

        if options.contains_field(ItemFields::SortName) {
            dto.sort_name = item.sort_name.clone();
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
        }

        // Artists / album-artists.
        self.attach_artists(dto, item, prefetched).await?;

        // Video extras.
        if kinds::is_video(kind) {
            dto.video_type = Some(VideoType::VideoFile);
            dto.extra_type = item.extra_type.and_then(extra_type_from_disc);

            if options.contains_field(ItemFields::Trickplay) {
                // Jellyfin emits {} when requested but there is no manifest.
                let manifest = match prefetched {
                    Some(p) => p.trickplay.get(&item_id).cloned().unwrap_or_default(),
                    None => self.trickplay.get_trickplay_manifest(item_id).await?,
                };
                dto.trickplay = Some(to_trickplay_manifest(&manifest));
            }
        }

        // Chapters — [] when requested but there are none (matches Jellyfin).
        if options.contains_field(ItemFields::Chapters) {
            dto.chapters = Some(match prefetched {
                Some(p) => p.chapters.get(&item_id).cloned().unwrap_or_default(),
                None => self.chapters.get_chapters(item_id).await?,
            });
        }

        // Media streams.
        if options.contains_field(ItemFields::MediaStreams) {
            let streams = match prefetched {
                Some(p) => p.media_streams.get(&item_id).cloned().unwrap_or_default(),
                None => self.media_sources.get_media_streams(item_id).await?,
            };
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

    /// Loads an item's `(key, value)` provider ids from `BaseItemProviders`.
    async fn load_provider_ids(
        &self,
        item_id: Uuid,
    ) -> Result<HashMap<String, String>, ServiceError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            r#"SELECT "ProviderId", "ProviderValue" FROM "BaseItemProviders"
               WHERE "ItemId" = ?1"#,
        )
        .bind(item_id.to_string())
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;

        Ok(rows.into_iter().collect())
    }

    /// Batch form of [`Self::load_provider_ids`]: all provider ids for `item_ids`
    /// in one query per chunk, keyed by item id. Prefetched for list DTOs so the
    /// per-item lookup does not fan out across the 2-connection pool.
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
                query = query.bind(id.to_string());
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

        dto.album_count = Some(counts.album_count);
        dto.artist_count = Some(counts.artist_count);
        dto.episode_count = Some(counts.episode_count);
        dto.movie_count = Some(counts.movie_count);
        dto.music_video_count = Some(counts.music_video_count);
        dto.program_count = Some(counts.program_count);
        dto.series_count = Some(counts.series_count);
        dto.song_count = Some(counts.song_count);
        dto.trailer_count = Some(counts.trailer_count);
        dto.child_count = Some(total_item_count(&counts));
        Ok(())
    }
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
fn access_filter_for(user: Option<&UserEntity>) -> hermit_traits::options::InternalItemsQuery {
    hermit_traits::options::InternalItemsQuery {
        user: user.cloned(),
        ..Default::default()
    }
}

/// Sums the per-kind counts into the total child count (port of
/// `ItemCounts.TotalItemCount`).
fn total_item_count(counts: &hermit_model::dto::ItemCounts) -> i32 {
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
fn program_audio_from_disc(disc: i32) -> Option<hermit_model::dto::ProgramAudio> {
    use hermit_model::dto::ProgramAudio;
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
fn person_kind_from_str(value: &str) -> hermit_model::data::PersonKind {
    use hermit_model::data::PersonKind;
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
    manifest: &HashMap<String, HashMap<i32, hermit_db::entities::playback::TrickplayInfoEntity>>,
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
fn to_trickplay_dto(info: &hermit_db::entities::playback::TrickplayInfoEntity) -> TrickplayInfoDto {
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
impl hermit_traits::dto::DtoService for HermitDtoService {
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
        let mut dto = self.build_dto(item, options, user, owner_id, None).await?;
        if options.contains_field(ItemFields::ItemCounts) {
            self.set_item_by_name_info(&mut dto, user).await?;
        }
        Ok(dto)
    }

    #[allow(clippy::too_many_lines)] // a flat sequence of independent page prefetches
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

        // Bulk-load the per-item relations for the whole page up front (2
        // queries total) instead of 2 queries × N items inside `build_dto` —
        // the N+1 convoyed the connection pool under concurrent list load.
        let ids: Vec<Uuid> = items.iter().map(row_id).collect();
        let images = if options.enable_images
            || options.contains_field(ItemFields::PrimaryImageAspectRatio)
        {
            self.load_images_batch(&ids).await?
        } else {
            HashMap::new()
        };
        let user_data = match user {
            Some(user) if options.enable_user_data => {
                let user_id = Uuid::parse_str(&user.id).unwrap_or_else(|_| Uuid::nil());
                self.user_data.get_user_data_dtos(&ids, user_id).await?
            }
            _ => HashMap::new(),
        };
        // The heavy per-item relations, bulk-loaded once for the page when their
        // field is requested (an all-fields list DTO otherwise fans out a query
        // per item for each — costly on the 2-connection pool).
        let media_streams = if options.contains_field(ItemFields::MediaStreams)
            || options.contains_field(ItemFields::MediaSources)
        {
            self.media_sources.get_media_streams_batch(&ids).await?
        } else {
            HashMap::new()
        };
        let provider_ids = if options.contains_field(ItemFields::ProviderIds) {
            self.load_provider_ids_batch(&ids).await?
        } else {
            HashMap::new()
        };
        // People for the page, then every credited person's images in one further
        // query — attach_people otherwise runs get_people + load_images per item.
        let (people, person_images) = if options.contains_field(ItemFields::People) {
            let people = self.library.get_people_batch(&ids).await?;
            let person_ids: Vec<Uuid> = people
                .values()
                .flatten()
                .filter_map(|p| Uuid::parse_str(&p.id).ok())
                .collect();
            let images = self
                .load_images_batch(&person_ids)
                .await
                .unwrap_or_default();
            (people, images)
        } else {
            (HashMap::new(), HashMap::new())
        };
        // Studio/genre/artist ids for every name on the page in one query. Collect
        // exactly what the attach steps resolve: studios/genres only when their
        // field is requested, artists/album-artists always (attach_artists is
        // unconditional) — so a prefetched miss never wrongly nils a real id.
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
        let prefetched = Prefetched {
            images,
            user_data,
            media_streams,
            provider_ids,
            people,
            person_images,
            value_ids,
            chapters,
            trickplay,
        };

        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let mut dto = self
                .build_dto(item, options, user, owner_id, Some(&prefetched))
                .await?;
            if options.contains_field(ItemFields::ItemCounts) {
                self.set_item_by_name_info(&mut dto, user).await?;
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
        let mut dto = self.build_dto(item, options, user, None, None).await?;

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

impl HermitDtoService {
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
    use hermit_db::entities::base_items::PeopleEntity;
    use hermit_db::entities::playback::TrickplayInfoEntity;
    use hermit_model::drawing::{ImageDimensions, ImageFormat};
    use hermit_model::dto::{MediaSourceInfo, UserItemDataDto};
    use hermit_model::entities_media::{ChapterInfo, MediaAttachment, MediaStream};
    use hermit_model::providers::ExternalUrl;
    use hermit_traits::drawing::ProcessedImage;
    use hermit_traits::dto::DtoService as _;

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
        ) -> Result<Vec<hermit_traits::options::ItemImageInfo>, ServiceError> {
            Ok(vec![])
        }
        async fn query_items(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<hermit_model::querying::QueryResult<BaseItemEntity>, ServiceError> {
            Ok(hermit_model::querying::QueryResult::default())
        }
        async fn get_item_ids(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<Vec<Uuid>, ServiceError> {
            Ok(vec![])
        }
        async fn get_item_list(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            Ok(vec![])
        }
        async fn get_latest_item_list(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
            _collection_type: hermit_model::data::CollectionType,
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
            _options: &hermit_traits::options::DeleteOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_people(
            &self,
            _query: &hermit_traits::options::InternalPeopleQuery,
        ) -> Result<Vec<PeopleEntity>, ServiceError> {
            Ok(self.people.clone())
        }
        async fn get_people_names(
            &self,
            _query: &hermit_traits::options::InternalPeopleQuery,
        ) -> Result<Vec<String>, ServiceError> {
            Ok(vec![])
        }
        async fn get_count(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<i32, ServiceError> {
            Ok(0)
        }
        async fn get_item_counts(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<hermit_model::dto::ItemCounts, ServiceError> {
            Ok(hermit_model::dto::ItemCounts::default())
        }
        async fn get_genres(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(hermit_model::querying::QueryResult::default())
        }
        async fn get_studios(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(hermit_model::querying::QueryResult::default())
        }
        async fn get_artists(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(hermit_model::querying::QueryResult::default())
        }
        async fn get_music_genres(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(hermit_model::querying::QueryResult::default())
        }
        async fn get_album_artists(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(hermit_model::querying::QueryResult::default())
        }
        async fn get_query_filters_legacy(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<hermit_model::querying::QueryFiltersLegacy, ServiceError> {
            Ok(hermit_model::querying::QueryFiltersLegacy::default())
        }
        async fn get_media_stream_languages(
            &self,
            _stream_type: hermit_model::entities::MediaStreamType,
            _query: &hermit_traits::options::InternalItemsQuery,
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
        async fn save_user_data(
            &self,
            _user_id: Uuid,
            _item_id: Uuid,
            _user_data: &hermit_model::dto::UpdateUserItemDataDto,
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

    /// An [`ItemCountService`] fake returning fixed name-item counts.
    #[derive(Default)]
    struct FakeCounts;

    #[async_trait]
    impl ItemCountService for FakeCounts {
        async fn get_count(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<i32, ServiceError> {
            Ok(0)
        }
        async fn get_item_counts(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<hermit_model::dto::ItemCounts, ServiceError> {
            Ok(hermit_model::dto::ItemCounts::default())
        }
        async fn get_item_counts_for_name_item(
            &self,
            _kind: BaseItemKind,
            _id: Uuid,
            _related_item_kinds: &[BaseItemKind],
            _access_filter: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<hermit_model::dto::ItemCounts, ServiceError> {
            Ok(hermit_model::dto::ItemCounts {
                movie_count: 3,
                series_count: 2,
                ..hermit_model::dto::ItemCounts::default()
            })
        }
        async fn get_played_count(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
            _ancestor_id: Uuid,
        ) -> Result<i32, ServiceError> {
            Ok(0)
        }
        async fn get_total_count(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
            _ancestor_id: Uuid,
        ) -> Result<i32, ServiceError> {
            Ok(0)
        }
        async fn get_played_and_total_count(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
            _ancestor_id: Uuid,
        ) -> Result<hermit_traits::persistence::PlayedAndTotal, ServiceError> {
            Ok(hermit_traits::persistence::PlayedAndTotal::default())
        }
        async fn get_played_and_total_count_from_linked_children(
            &self,
            _filter: &hermit_traits::options::InternalItemsQuery,
            _parent_id: Uuid,
        ) -> Result<hermit_traits::persistence::PlayedAndTotal, ServiceError> {
            Ok(hermit_traits::persistence::PlayedAndTotal::default())
        }
        async fn get_played_and_total_count_batch(
            &self,
            _folder_ids: &[Uuid],
            _user: &UserEntity,
        ) -> Result<HashMap<Uuid, hermit_traits::persistence::PlayedAndTotal>, ServiceError>
        {
            Ok(HashMap::new())
        }
        async fn get_child_count_batch(
            &self,
            _parent_ids: &[Uuid],
            _user_id: Option<Uuid>,
        ) -> Result<HashMap<Uuid, i32>, ServiceError> {
            Ok(HashMap::new())
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
            _options: &hermit_traits::options::ImageProcessingOptions,
        ) -> Result<ProcessedImage, ServiceError> {
            Err(ServiceError::NotFound("process_image".into()))
        }
        async fn create_image_collage(
            &self,
            _options: &hermit_traits::options::ImageCollageOptions,
            _library_name: Option<&str>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// A [`MediaSourceManager`] fake — all empty.
    #[derive(Default)]
    struct FakeSources;

    #[async_trait]
    impl MediaSourceManager for FakeSources {
        async fn get_media_streams(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<MediaStream>, ServiceError> {
            Ok(vec![])
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
        async fn open_live_stream(
            &self,
            _request: &hermit_model::media_info::LiveStreamRequest,
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

    /// A [`TrickplayManager`] fake — no manifest.
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
            _item_id: Uuid,
        ) -> Result<HashMap<String, HashMap<i32, TrickplayInfoEntity>>, ServiceError> {
            Ok(HashMap::new())
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
            _options: &hermit_traits::providers::MetadataRefreshOptions,
            _priority: hermit_traits::providers::RefreshPriority,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn refresh_full_item(
            &self,
            _item_id: Uuid,
            _options: &hermit_traits::providers::MetadataRefreshOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn refresh_single_item(
            &self,
            _item_id: Uuid,
            _options: &hermit_traits::providers::MetadataRefreshOptions,
        ) -> Result<hermit_traits::providers::ItemUpdateType, ServiceError> {
            Ok(hermit_traits::providers::ItemUpdateType::default())
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
            _query: &hermit_model::providers::RemoteImageQuery,
        ) -> Result<Vec<hermit_model::providers::RemoteImageInfo>, ServiceError> {
            Ok(vec![])
        }
        async fn get_remote_image_provider_info(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<hermit_model::providers::ImageProviderInfo>, ServiceError> {
            Ok(vec![])
        }
        async fn save_metadata(
            &self,
            _item_id: Uuid,
            _update_type: hermit_traits::providers::ItemUpdateType,
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
        ) -> Result<Vec<hermit_model::providers::ExternalIdInfo>, ServiceError> {
            Ok(vec![])
        }
        async fn get_all_metadata_plugins(
            &self,
        ) -> Result<Vec<hermit_model::configuration::MetadataPluginSummary>, ServiceError> {
            Ok(vec![])
        }
        async fn get_metadata_options(
            &self,
            _item_id: Uuid,
        ) -> Result<hermit_model::configuration::MetadataOptions, ServiceError> {
            Ok(hermit_model::configuration::MetadataOptions::default())
        }
        async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
            Ok(vec![])
        }
    }

    /// Builds a DTO service over `db` wired to the fakes, with an optional custom
    /// library fake (for the people test).
    fn service_with(db: Database, library: Arc<dyn LibraryManager>) -> HermitDtoService {
        HermitDtoService::new(
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

    fn service(db: Database) -> HermitDtoService {
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
        .bind(Uuid::new_v4().to_string())
        .bind(item_id.to_string())
        .bind(image_type)
        .bind(path)
        .bind(blur.map(|b| b.as_bytes().to_vec()))
        .execute(db.pool())
        .await
        .expect("insert image");
    }

    /// Reads back a full item row.
    async fn fetch_item(db: &Database, id: Uuid) -> BaseItemEntity {
        sqlx::query_as::<_, BaseItemEntity>(r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(id.to_string())
            .fetch_one(db.pool())
            .await
            .expect("fetch item")
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
        .bind(id.to_string())
        .execute(db.pool())
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
            .bind(id.to_string())
            .execute(db.pool())
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
        .bind(id.to_string())
        .execute(db.pool())
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
    async fn maps_provider_ids_and_external_urls() {
        let db = test_db().await;
        let id = Uuid::new_v4();
        seed_named_item(&db, id, BaseItemKind::Movie, "M").await;
        sqlx::query(
            r#"INSERT INTO "BaseItemProviders" ("ItemId", "ProviderId", "ProviderValue")
               VALUES (?1, 'Imdb', 'tt1375666')"#,
        )
        .bind(id.to_string())
        .execute(db.pool())
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
                .bind(id.to_string())
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)
        }
        async fn get_item_images(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<hermit_traits::options::ItemImageInfo>, ServiceError> {
            Ok(vec![])
        }
        async fn query_items(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<hermit_model::querying::QueryResult<BaseItemEntity>, ServiceError> {
            Ok(hermit_model::querying::QueryResult::default())
        }
        async fn get_item_ids(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<Vec<Uuid>, ServiceError> {
            Ok(vec![])
        }
        async fn get_item_list(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<Vec<BaseItemEntity>, ServiceError> {
            Ok(vec![])
        }
        async fn get_latest_item_list(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
            _collection_type: hermit_model::data::CollectionType,
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
            _options: &hermit_traits::options::DeleteOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_people(
            &self,
            _query: &hermit_traits::options::InternalPeopleQuery,
        ) -> Result<Vec<PeopleEntity>, ServiceError> {
            Ok(vec![])
        }
        async fn get_people_names(
            &self,
            _query: &hermit_traits::options::InternalPeopleQuery,
        ) -> Result<Vec<String>, ServiceError> {
            Ok(vec![])
        }
        async fn get_count(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<i32, ServiceError> {
            Ok(0)
        }
        async fn get_item_counts(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<hermit_model::dto::ItemCounts, ServiceError> {
            Ok(hermit_model::dto::ItemCounts::default())
        }
        async fn get_genres(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(hermit_model::querying::QueryResult::default())
        }
        async fn get_studios(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(hermit_model::querying::QueryResult::default())
        }
        async fn get_artists(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(hermit_model::querying::QueryResult::default())
        }
        async fn get_music_genres(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(hermit_model::querying::QueryResult::default())
        }
        async fn get_album_artists(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<
            hermit_model::querying::QueryResult<hermit_traits::persistence::ItemWithCounts>,
            ServiceError,
        > {
            Ok(hermit_model::querying::QueryResult::default())
        }
        async fn get_query_filters_legacy(
            &self,
            _query: &hermit_traits::options::InternalItemsQuery,
        ) -> Result<hermit_model::querying::QueryFiltersLegacy, ServiceError> {
            Ok(hermit_model::querying::QueryFiltersLegacy::default())
        }
        async fn get_media_stream_languages(
            &self,
            _stream_type: hermit_model::entities::MediaStreamType,
            _query: &hermit_traits::options::InternalItemsQuery,
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
        assert_eq!(people[0].type_, hermit_model::data::PersonKind::Actor);
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
        .bind(vid.to_string())
        .bind(&clean)
        .execute(db.pool())
        .await
        .unwrap();
        let svc = service(db);

        // The single-item lookup and the batch resolver agree on the id.
        let single = svc.value_id(3, "Warner Bros.").await.unwrap();
        let map = svc
            .resolve_value_ids(&[(3, "Warner Bros.".to_string())])
            .await
            .unwrap();
        assert_eq!(single, vid);
        assert_eq!(map.get(&(3, clean)).copied(), Some(vid));

        // value_id_for reads the prefetched map without a query, and nil-s a name
        // with no row (matching value_id's missing-row behaviour).
        let pf = Prefetched {
            value_ids: map,
            ..Prefetched::default()
        };
        assert_eq!(
            svc.value_id_for(Some(&pf), 3, "Warner Bros.")
                .await
                .unwrap(),
            vid
        );
        assert!(
            svc.value_id_for(Some(&pf), 3, "Nobody")
                .await
                .unwrap()
                .is_nil()
        );
    }
}
