//! Port of `MediaBrowser.Controller.Entities.InternalItemsQuery` — the central,
//! ~140-field query struct the library/persistence layer filters items with.

use chrono::{DateTime, Utc};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::data::{BaseItemKind, CollectionType, MediaType, UnratedItem};
use ferrofin_model::dto::SortOrder;
use ferrofin_model::entities::{ExtraType, ImageType, SeriesStatus, TrailerType, VideoType};
use ferrofin_model::entities_media::ParentalRatingScore;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_model::querying::ItemFilter;
use std::collections::HashMap;
use uuid::Uuid;

use super::DtoOptions;

/// Where an item's data originates. Local minimal port of
/// `Jellyfin.Data.Enums.SourceType`, which is **not yet present in
/// `ferrofin-model`** (see the port report's belongs-in-model flags). Declared
/// here so [`InternalItemsQuery::source_types`] can be typed faithfully; move to
/// `ferrofin-model` if it becomes a wire type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SourceType {
    /// A normal library item.
    #[default]
    Library,
    /// A channel-provided item.
    Channel,
    /// A Live TV item.
    LiveTv,
}

/// The central item query. Every field is an optional filter, a pagination/sort
/// control, or DTO/user context; an empty query matches everything.
///
/// Mirrors C# `InternalItemsQuery`. Faithfulness notes:
/// - identity fields use [`Uuid`] (C# `Guid`); `Guid.Empty` ⇒ [`Uuid::nil`];
/// - the C# `User` domain property becomes an [`Option`]`<`[`UserEntity`]`>`
///   plus a cached [`user_id`](Self::user_id);
/// - the C# `Parent`/`BaseItem` setter is replaced by
///   [`set_parent`](Self::set_parent), which takes the id and kind directly;
/// - [`Default`] reproduces the C# constructor exactly (not `#[derive]`d),
///   including the `true` defaults for `enable_total_record_count`,
///   `group_by_presentation_unique_key`, and the all-fields [`DtoOptions`].
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct InternalItemsQuery {
    // --- pagination / recursion / user context ---
    /// Whether the query recurses into child folders.
    pub recursive: bool,
    /// The index of the first result to return.
    pub start_index: Option<i32>,
    /// The maximum number of results to return.
    pub limit: Option<i32>,
    /// The user the query is scoped to.
    pub user: Option<UserEntity>,

    // --- boolean tri-state filters ---
    /// Restrict to folders / non-folders.
    pub is_folder: Option<bool>,
    /// Restrict to favourited items.
    pub is_favorite: Option<bool>,
    /// Restrict to favourited-or-liked items.
    pub is_favorite_or_liked: Option<bool>,
    /// Restrict to liked / disliked items.
    pub is_liked: Option<bool>,
    /// Restrict to played / unplayed items.
    pub is_played: Option<bool>,
    /// Restrict to resumable items.
    pub is_resumable: Option<bool>,
    /// Whether by-name items are included.
    pub include_items_by_name: Option<bool>,

    // --- type / tag / genre sets ---
    /// Restrict to these media types.
    pub media_types: Vec<MediaType>,
    /// Restrict to these item kinds.
    pub include_item_types: Vec<BaseItemKind>,
    /// Exclude these item kinds.
    pub exclude_item_types: Vec<BaseItemKind>,
    /// Exclude items bearing any of these tags.
    pub exclude_tags: Vec<String>,
    /// Exclude items inheriting any of these tags.
    pub exclude_inherited_tags: Vec<String>,
    /// Include only items inheriting one of these tags.
    pub include_inherited_tags: Vec<String>,
    /// Restrict to these genres (by name).
    pub genres: Vec<String>,

    /// Restrict to special seasons.
    pub is_special_season: Option<bool>,
    /// Restrict to missing items.
    pub is_missing: Option<bool>,
    /// Restrict to unaired items.
    pub is_unaired: Option<bool>,
    /// Whether box-set members are collapsed into their box set.
    pub collapse_box_set_items: Option<bool>,
    /// Item kinds collapsed into box sets; empty = all kinds.
    pub collapse_box_set_item_types: Vec<BaseItemKind>,

    // --- name range filters ---
    /// Lower bound (starts-with-or-greater) for the name.
    pub name_starts_with_or_greater: Option<String>,
    /// Prefix the name must start with.
    pub name_starts_with: Option<String>,
    /// Upper bound (exclusive) for the name.
    pub name_less_than: Option<String>,
    /// Substring the name must contain.
    pub name_contains: Option<String>,
    /// Lower bound for the sort name.
    pub min_sort_name: Option<String>,
    /// Exact presentation unique key.
    pub presentation_unique_key: Option<String>,
    /// Exact filesystem path.
    pub path: Option<String>,
    /// Exact name.
    pub name: Option<String>,
    /// Exact names (cleaned), matched as a set — the batch form of [`Self::name`]
    /// used to resolve many by-name items (people, years) in one query.
    pub names: Vec<String>,
    /// Whether the raw (un-normalized) name is matched.
    pub use_raw_name: Option<bool>,

    // --- person / id sets ---
    /// Restrict to items featuring this person (by name).
    pub person: Option<String>,
    /// Restrict to items featuring these people (by id).
    pub person_ids: Vec<Uuid>,
    /// Restrict to these item ids.
    pub item_ids: Vec<Uuid>,
    /// Restrict to items owned by these owners.
    pub owner_ids: Vec<Uuid>,
    /// Restrict to these extra types.
    pub extra_types: Vec<ExtraType>,
    /// Exclude these item ids.
    pub exclude_item_ids: Vec<Uuid>,
    /// Return items adjacent to this one (for prev/next navigation).
    pub adjacent_to: Option<Uuid>,
    /// Restrict to these person types.
    pub person_types: Vec<String>,

    // --- media-attribute tri-state filters ---
    /// Restrict to 3D / non-3D items.
    pub is_3d: Option<bool>,
    /// Restrict to HD items.
    pub is_hd: Option<bool>,
    /// Restrict to (un)locked items.
    pub is_locked: Option<bool>,
    /// Restrict to placeholder items.
    pub is_place_holder: Option<bool>,

    // --- provider-id presence filters ---
    /// Restrict to items with an IMDb id.
    pub has_imdb_id: Option<bool>,
    /// Restrict to items with an overview.
    pub has_overview: Option<bool>,
    /// Restrict to items with a TMDb id.
    pub has_tmdb_id: Option<bool>,
    /// Restrict to items with an official rating.
    pub has_official_rating: Option<bool>,
    /// Restrict to items with a TVDB id.
    pub has_tvdb_id: Option<bool>,
    /// Restrict to items whose external provider ids match *any* of these
    /// `(provider-key, value)` pairs (case-insensitive on both key and value).
    ///
    /// Port of Jellyfin's `InternalItemsQuery.AnyProviderIdEquals`: the
    /// filesystem-monitor webhooks (`/Library/Movies/*`, `/Library/Series/*`)
    /// select the items to report by exact provider-id value, which C# does with
    /// an in-memory `GetProviderId(...)` comparison; here it is pushed into the
    /// query as an `EXISTS` over `BaseItemProviders` so the DB does the matching.
    pub any_provider_id_equals: Vec<(String, String)>,
    /// Restrict to items with a theme song.
    pub has_theme_song: Option<bool>,
    /// Restrict to items with a theme video.
    pub has_theme_video: Option<bool>,
    /// Restrict to items with subtitles.
    pub has_subtitles: Option<bool>,
    /// Restrict to items with a special feature.
    pub has_special_feature: Option<bool>,
    /// Restrict to items with a trailer.
    pub has_trailer: Option<bool>,
    /// Restrict to items with a parental rating.
    pub has_parental_rating: Option<bool>,

    // --- studio / genre / image id sets ---
    /// Restrict to these studios (by id).
    pub studio_ids: Vec<Uuid>,
    /// Restrict to these genres (by id).
    pub genre_ids: Vec<Uuid>,
    /// Restrict to items bearing these image types.
    pub image_types: Vec<ImageType>,
    /// Restrict to these video types.
    pub video_types: Vec<VideoType>,
    /// The unrated-item kinds blocked for the scoped user.
    pub block_unrated_items: Vec<UnratedItem>,
    /// Restrict to these production years.
    pub years: Vec<i32>,
    /// Restrict to items bearing these tags.
    pub tags: Vec<String>,
    /// Restrict to these official ratings.
    pub official_ratings: Vec<String>,

    // --- date range filters ---
    /// Earliest premiere date.
    pub min_premiere_date: Option<DateTime<Utc>>,
    /// Latest premiere date.
    pub max_premiere_date: Option<DateTime<Utc>>,
    /// Earliest start date.
    pub min_start_date: Option<DateTime<Utc>>,
    /// Latest start date.
    pub max_start_date: Option<DateTime<Utc>>,
    /// Earliest end date.
    pub min_end_date: Option<DateTime<Utc>>,
    /// Latest end date.
    pub max_end_date: Option<DateTime<Utc>>,

    // --- program-kind tri-state filters ---
    /// Restrict to currently-airing items.
    pub is_airing: Option<bool>,
    /// Restrict to movies.
    pub is_movie: Option<bool>,
    /// Restrict to sports.
    pub is_sports: Option<bool>,
    /// Restrict to kids content.
    pub is_kids: Option<bool>,
    /// Restrict to news.
    pub is_news: Option<bool>,
    /// Restrict to series.
    pub is_series: Option<bool>,

    // --- index / rating numeric filters ---
    /// Minimum index number.
    pub min_index_number: Option<i32>,
    /// Minimum `(ParentIndexNumber, IndexNumber)` pair.
    pub min_parent_and_index_number: Option<(i32, i32)>,
    /// Restrict to items that aired during this season number.
    pub aired_during_season: Option<i32>,
    /// Minimum critic rating.
    pub min_critic_rating: Option<f64>,
    /// Minimum community rating.
    pub min_community_rating: Option<f64>,
    /// Restrict to these channels.
    pub channel_ids: Vec<Uuid>,
    /// Exact parent index number.
    pub parent_index_number: Option<i32>,
    /// Parent index number to exclude.
    pub parent_index_number_not_equals: Option<i32>,
    /// Exact index number.
    pub index_number: Option<i32>,
    /// Minimum permitted parental rating.
    pub min_parental_rating: Option<ParentalRatingScore>,
    /// Maximum permitted parental rating (derived from the user).
    pub max_parental_rating: Option<ParentalRatingScore>,

    // --- structural filters ---
    /// Restrict to items whose parent id no longer exists.
    pub has_dead_parent_id: Option<bool>,
    /// Restrict to virtual items.
    pub is_virtual_item: Option<bool>,
    /// The parent item id (`Guid.Empty` ⇒ nil means unset).
    pub parent_id: Uuid,
    /// When set, a non-recursive `parent_id` browse matches only PHYSICAL children
    /// (`bi.ParentId = parent_id`) and does NOT merge the parent's `FerrofinLinkedChildren`
    /// members. Used by delete-cascade so removing a box-set/playlist never deletes
    /// the referenced items (linked children are references, not owned children).
    pub physical_children_only: bool,
    /// The physical folders [`Self::parent_id`] stands for, when it names a
    /// Jellyfin `CollectionFolder`.
    ///
    /// **Derived, not set by callers** — the item repository fills it in from
    /// `parent_id` before building the statement, and leaving it empty is
    /// always correct. A Jellyfin `CollectionFolder` is virtual: nothing points
    /// at it with `ParentId`, so a plain equality finds nothing and the
    /// browse's children have to be matched against the library's physical
    /// folders instead. Empty on a Ferrofin-written database, where items hang
    /// off the collection folder directly.
    pub parent_physical_folder_ids: Vec<Uuid>,
    /// The `AggregateFolder` whose plug-in folders count as children of
    /// [`Self::parent_id`].
    ///
    /// **Derived, not set by callers** — the item repository fills it in when
    /// `parent_id` names the `UserRootFolder`. Port of
    /// `UserRootFolder.GetEligibleChildrenForRecursiveChildren`
    /// (UserRootFolder.cs:96-102), which concatenates
    /// `LibraryManager.RootFolder.VirtualChildren` onto its own children:
    /// `LibraryManager.CreateRootFolder` parents the playlists folder to the
    /// **aggregate** and registers it as a virtual child, so the user root
    /// lists a row that does not carry its id as `ParentId`.
    pub virtual_child_parent_id: Option<Uuid>,
    /// The caller is `GET /Items` in the shape that C# answers with
    /// `Folder.GetChildren`, not with a query.
    ///
    /// Port of `ItemsController.GetItems`' branch condition
    /// (ItemsController.cs:307-528): when the request is **not** recursive, names
    /// **no** `ids`, and its resolved parent **is** the `UserRootFolder` — which
    /// `LibraryManager.GetParentItem(parentId, userId)` also returns for an
    /// ABSENT `parentId` — upstream skips the whole `InternalItemsQuery` it
    /// otherwise builds and returns
    /// `new QueryResult<BaseItem>(folder.GetChildren(user, true))`.
    ///
    /// That branch applies no sort, no paging and none of the request's filters,
    /// and its children are the user root's own rows with the `AggregateFolder`'s
    /// virtual children APPENDED (`list.AddRange`, UserRootFolder.cs:96-102) —
    /// measured on 10.11.8: `sortBy=SortName&sortOrder=Descending`,
    /// `sortBy=DateCreated`, `limit=2&startIndex=1` and `includeItemTypes=Movie`
    /// all return the identical seven rows in the identical order, Playlists last.
    ///
    /// Set only by the `GET /Items` handler (the controller owns this branch);
    /// the repository ignores it unless the parent really does resolve to the
    /// user root.
    pub user_root_children: bool,
    /// The parent item kind, if known.
    pub parent_type: Option<BaseItemKind>,
    /// Restrict to descendants of these ancestors.
    pub ancestor_ids: Vec<Uuid>,
    /// Restrict to items whose linked children descend from these ancestors.
    pub linked_child_ancestor_ids: Vec<Uuid>,
    /// Restrict to items under these top-level parents.
    pub top_parent_ids: Vec<Uuid>,
    /// The preset library views to include.
    pub preset_views: Vec<Option<CollectionType>>,
    /// Restrict to these trailer types.
    pub trailer_types: Vec<TrailerType>,
    /// Restrict to these source types.
    pub source_types: Vec<SourceType>,
    /// Restrict to these series statuses.
    pub series_statuses: Vec<SeriesStatus>,
    /// Exact external series id.
    pub external_series_id: Option<String>,
    /// Exact external id.
    pub external_id: Option<String>,

    // --- artist / album id sets ---
    /// Restrict to these albums.
    pub album_ids: Vec<Uuid>,
    /// Restrict to these artists.
    pub artist_ids: Vec<Uuid>,
    /// Exclude these artists.
    pub exclude_artist_ids: Vec<Uuid>,
    /// Presentation-unique-key of a required ancestor.
    pub ancestor_with_presentation_unique_key: Option<String>,
    /// Presentation-unique-key of a required series.
    pub series_presentation_unique_key: Option<String>,

    // --- grouping / bookkeeping toggles ---
    /// Whether results are grouped by presentation unique key.
    pub group_by_presentation_unique_key: bool,
    /// Whether results are grouped by series presentation unique key.
    pub group_by_series_presentation_unique_key: bool,
    /// Whether a total record count is computed (defaults `true`).
    pub enable_total_record_count: bool,
    /// Whether the query is forced to run directly against the store.
    pub force_direct: bool,
    /// Provider ids to exclude (name → value).
    pub exclude_provider_ids: Option<HashMap<String, String>>,
    /// Whether results are grouped by metadata key.
    pub enable_group_by_metadata_key: bool,
    /// Restrict to items with chapter images.
    pub has_chapter_images: Option<bool>,
    /// The sort order.
    pub order_by: Vec<(ItemSortBy, SortOrder)>,

    // --- more date filters ---
    /// Earliest date created.
    pub min_date_created: Option<DateTime<Utc>>,
    /// Earliest date last saved.
    pub min_date_last_saved: Option<DateTime<Utc>>,
    /// Earliest date last saved for the scoped user.
    pub min_date_last_saved_for_user: Option<DateTime<Utc>>,

    /// The DTO field/image toggles used when materializing results.
    pub dto_options: DtoOptions,

    // --- audio/subtitle language absence filters ---
    /// Exclude items that have an audio track with this language.
    pub has_no_audio_track_with_language: Option<String>,
    /// Exclude items that have an internal subtitle track with this language.
    pub has_no_internal_subtitle_track_with_language: Option<String>,
    /// Exclude items that have an external subtitle track with this language.
    pub has_no_external_subtitle_track_with_language: Option<String>,
    /// Exclude items that have any subtitle track with this language.
    pub has_no_subtitle_track_with_language: Option<String>,

    // --- "is dead" by-name filters ---
    /// Restrict to artists with no remaining items.
    pub is_dead_artist: Option<bool>,
    /// Restrict to studios with no remaining items.
    pub is_dead_studio: Option<bool>,
    /// Restrict to genres with no remaining items.
    pub is_dead_genre: Option<bool>,
    /// Restrict to people with no remaining items.
    pub is_dead_person: Option<bool>,
    /// Whether album sub-folders are returned when present.
    pub display_album_folders: Option<bool>,

    // --- provider-id map filters ---
    /// Match items having any of these single provider ids.
    pub has_any_provider_id: Option<HashMap<String, String>>,
    /// Match items having any of these multi-valued provider ids.
    pub has_any_provider_ids: Option<HashMap<String, Vec<String>>>,
    /// Restrict to these album artists.
    pub album_artist_ids: Vec<Uuid>,
    /// Restrict to box sets in these library folders.
    pub box_set_library_folders: Vec<Uuid>,
    /// Restrict to these contributing artists.
    pub contributing_artist_ids: Vec<Uuid>,
    /// Restrict to items that have (not) aired.
    pub has_aired: Option<bool>,
    /// Restrict to items that (do not) have an owner.
    pub has_owner_id: Option<bool>,
    /// Whether owner-scoped items (extra parts, alternates) are included.
    pub include_owned_items: bool,

    // --- resolution filters ---
    /// Restrict to 4K items.
    pub is_4k: Option<bool>,
    /// Maximum height.
    pub max_height: Option<i32>,
    /// Maximum width.
    pub max_width: Option<i32>,
    /// Minimum height.
    pub min_height: Option<i32>,
    /// Minimum width.
    pub min_width: Option<i32>,

    // --- misc ---
    /// A free-text search term.
    pub search_term: Option<String>,
    /// A series-timer id filter.
    pub series_timer_id: Option<String>,
    /// Whether deserialization of full items is skipped (id-only results).
    pub skip_deserialization: bool,
    /// Whether extras are included.
    pub include_extras: bool,
    /// Restrict to items with an audio track in one of these languages.
    pub audio_languages: Vec<String>,
    /// Restrict to items with a subtitle track in one of these languages.
    pub subtitle_languages: Vec<String>,
}

impl Default for InternalItemsQuery {
    /// Reproduces the C# `new InternalItemsQuery()` constructor: most fields are
    /// their zero value, but `enable_total_record_count`,
    /// `group_by_presentation_unique_key` are `true`, and `dto_options` is the
    /// all-fields [`DtoOptions::default`].
    #[allow(clippy::too_many_lines)]
    fn default() -> Self {
        Self {
            recursive: false,
            start_index: None,
            limit: None,
            user: None,
            is_folder: None,
            is_favorite: None,
            is_favorite_or_liked: None,
            is_liked: None,
            is_played: None,
            is_resumable: None,
            include_items_by_name: None,
            media_types: Vec::new(),
            include_item_types: Vec::new(),
            exclude_item_types: Vec::new(),
            exclude_tags: Vec::new(),
            exclude_inherited_tags: Vec::new(),
            include_inherited_tags: Vec::new(),
            genres: Vec::new(),
            is_special_season: None,
            is_missing: None,
            is_unaired: None,
            collapse_box_set_items: None,
            collapse_box_set_item_types: Vec::new(),
            name_starts_with_or_greater: None,
            name_starts_with: None,
            name_less_than: None,
            name_contains: None,
            min_sort_name: None,
            presentation_unique_key: None,
            path: None,
            name: None,
            names: Vec::new(),
            use_raw_name: None,
            person: None,
            person_ids: Vec::new(),
            item_ids: Vec::new(),
            owner_ids: Vec::new(),
            extra_types: Vec::new(),
            exclude_item_ids: Vec::new(),
            adjacent_to: None,
            person_types: Vec::new(),
            is_3d: None,
            is_hd: None,
            is_locked: None,
            is_place_holder: None,
            has_imdb_id: None,
            has_overview: None,
            has_tmdb_id: None,
            has_official_rating: None,
            has_tvdb_id: None,
            any_provider_id_equals: Vec::new(),
            has_theme_song: None,
            has_theme_video: None,
            has_subtitles: None,
            has_special_feature: None,
            has_trailer: None,
            has_parental_rating: None,
            studio_ids: Vec::new(),
            genre_ids: Vec::new(),
            image_types: Vec::new(),
            video_types: Vec::new(),
            block_unrated_items: Vec::new(),
            years: Vec::new(),
            tags: Vec::new(),
            official_ratings: Vec::new(),
            min_premiere_date: None,
            max_premiere_date: None,
            min_start_date: None,
            max_start_date: None,
            min_end_date: None,
            max_end_date: None,
            is_airing: None,
            is_movie: None,
            is_sports: None,
            is_kids: None,
            is_news: None,
            is_series: None,
            min_index_number: None,
            min_parent_and_index_number: None,
            aired_during_season: None,
            min_critic_rating: None,
            min_community_rating: None,
            channel_ids: Vec::new(),
            parent_index_number: None,
            parent_index_number_not_equals: None,
            index_number: None,
            min_parental_rating: None,
            max_parental_rating: None,
            has_dead_parent_id: None,
            is_virtual_item: None,
            parent_id: Uuid::nil(),
            physical_children_only: false,
            parent_physical_folder_ids: Vec::new(),
            virtual_child_parent_id: None,
            user_root_children: false,
            parent_type: None,
            ancestor_ids: Vec::new(),
            linked_child_ancestor_ids: Vec::new(),
            top_parent_ids: Vec::new(),
            preset_views: Vec::new(),
            trailer_types: Vec::new(),
            source_types: Vec::new(),
            series_statuses: Vec::new(),
            external_series_id: None,
            external_id: None,
            album_ids: Vec::new(),
            artist_ids: Vec::new(),
            exclude_artist_ids: Vec::new(),
            ancestor_with_presentation_unique_key: None,
            series_presentation_unique_key: None,
            group_by_presentation_unique_key: true,
            group_by_series_presentation_unique_key: false,
            enable_total_record_count: true,
            force_direct: false,
            exclude_provider_ids: None,
            enable_group_by_metadata_key: false,
            has_chapter_images: None,
            order_by: Vec::new(),
            min_date_created: None,
            min_date_last_saved: None,
            min_date_last_saved_for_user: None,
            dto_options: DtoOptions::default(),
            has_no_audio_track_with_language: None,
            has_no_internal_subtitle_track_with_language: None,
            has_no_external_subtitle_track_with_language: None,
            has_no_subtitle_track_with_language: None,
            is_dead_artist: None,
            is_dead_studio: None,
            is_dead_genre: None,
            is_dead_person: None,
            display_album_folders: None,
            has_any_provider_id: None,
            has_any_provider_ids: None,
            album_artist_ids: Vec::new(),
            box_set_library_folders: Vec::new(),
            contributing_artist_ids: Vec::new(),
            has_aired: None,
            has_owner_id: None,
            include_owned_items: false,
            is_4k: None,
            max_height: None,
            max_width: None,
            min_height: None,
            min_width: None,
            search_term: None,
            series_timer_id: None,
            skip_deserialization: false,
            include_extras: false,
            audio_languages: Vec::new(),
            subtitle_languages: Vec::new(),
        }
    }
}

impl InternalItemsQuery {
    /// Whether the query carries any criterion that narrows the result set (as
    /// opposed to pagination, sorting, DTO options or user context). Mirrors the
    /// C# `HasFilters` computed property.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn has_filters(&self) -> bool {
        !self.include_item_types.is_empty()
            || !self.exclude_item_types.is_empty()
            || !self.genres.is_empty()
            || !self.genre_ids.is_empty()
            || !self.years.is_empty()
            || !self.tags.is_empty()
            || !self.exclude_tags.is_empty()
            || !self.official_ratings.is_empty()
            || !self.studio_ids.is_empty()
            || !self.artist_ids.is_empty()
            || !self.album_artist_ids.is_empty()
            || !self.contributing_artist_ids.is_empty()
            || !self.exclude_artist_ids.is_empty()
            || !self.album_ids.is_empty()
            || !self.person_ids.is_empty()
            || !self.person_types.is_empty()
            || !self.media_types.is_empty()
            || !self.video_types.is_empty()
            || !self.image_types.is_empty()
            || !self.series_statuses.is_empty()
            || !self.item_ids.is_empty()
            || !self.exclude_item_ids.is_empty()
            || !self.audio_languages.is_empty()
            || !self.subtitle_languages.is_empty()
            || !self.linked_child_ancestor_ids.is_empty()
            || !self.ancestor_ids.is_empty()
            || self.is_favorite.is_some()
            || self.is_favorite_or_liked.is_some()
            || self.is_liked.is_some()
            || self.is_played.is_some()
            || self.is_resumable.is_some()
            || self.is_folder.is_some()
            || self.is_missing.is_some()
            || self.is_unaired.is_some()
            || self.is_special_season.is_some()
            || self.is_3d.is_some()
            || self.is_hd.is_some()
            || self.is_4k.is_some()
            || self.is_locked.is_some()
            || self.is_place_holder.is_some()
            || self.is_movie.is_some()
            || self.is_sports.is_some()
            || self.is_kids.is_some()
            || self.is_news.is_some()
            || self.is_series.is_some()
            || self.is_airing.is_some()
            || self.is_virtual_item.is_some()
            || self.has_imdb_id.is_some()
            || self.has_tmdb_id.is_some()
            || self.has_tvdb_id.is_some()
            || !self.any_provider_id_equals.is_empty()
            || self.has_overview.is_some()
            || self.has_official_rating.is_some()
            || self.has_parental_rating.is_some()
            || self.has_theme_song.is_some()
            || self.has_theme_video.is_some()
            || self.has_subtitles.is_some()
            || self.has_special_feature.is_some()
            || self.has_trailer.is_some()
            || self.has_chapter_images.is_some()
            || self.min_critic_rating.is_some()
            || self.min_community_rating.is_some()
            || self.min_parental_rating.is_some()
            || self.min_index_number.is_some()
            || self.min_parent_and_index_number.is_some()
            || self.index_number.is_some()
            || self.parent_index_number.is_some()
            || self.aired_during_season.is_some()
            || self.min_width.is_some()
            || self.min_height.is_some()
            || self.max_width.is_some()
            || self.max_height.is_some()
            || self.min_premiere_date.is_some()
            || self.max_premiere_date.is_some()
            || self.min_start_date.is_some()
            || self.max_start_date.is_some()
            || self.min_end_date.is_some()
            || self.max_end_date.is_some()
            || self.min_date_created.is_some()
            || self.min_date_last_saved.is_some()
            || self.min_date_last_saved_for_user.is_some()
            || self.adjacent_to.is_some()
            || is_non_empty(self.name_starts_with.as_ref())
            || is_non_empty(self.name_starts_with_or_greater.as_ref())
            || is_non_empty(self.name_less_than.as_ref())
            || is_non_empty(self.name_contains.as_ref())
            || is_non_empty(self.min_sort_name.as_ref())
            || is_non_empty(self.name.as_ref())
            || is_non_empty(self.person.as_ref())
            || is_non_empty(self.search_term.as_ref())
            || is_non_empty(self.path.as_ref())
    }

    /// Applies the user context to the query. Mirrors the parts of C# `SetUser`
    /// that are derivable from the persisted [`UserEntity`]: the maximum
    /// parental rating and the user reference itself.
    ///
    /// The C# method also derives `block_unrated_items`,
    /// `exclude_inherited_tags` and `include_inherited_tags` from the user's
    /// **preferences** (`PreferenceKind.*`). Those rows live in a separate
    /// preferences table, not on [`UserEntity`], so the caller must populate
    /// those three fields from a preference lookup; they are left untouched here.
    pub fn set_user(&mut self, user: UserEntity) {
        if let Some(max) = user.max_parental_rating_score {
            self.max_parental_rating = Some(ParentalRatingScore::new(
                i32::try_from(max).unwrap_or(i32::MAX),
                user.max_parental_rating_sub_score
                    .map(|s| i32::try_from(s).unwrap_or(i32::MAX)),
            ));
        }
        self.user = Some(user);
    }

    /// The scoped user's id, or [`None`] when unset. Parses the entity's
    /// hyphenated `Guid` string id.
    #[must_use]
    pub fn user_id(&self) -> Option<Uuid> {
        self.user.as_ref().and_then(|u| Uuid::parse_str(&u.id).ok())
    }

    /// Sets the parent from an item id and kind. Replaces the C# `Parent`
    /// setter, which took a whole `BaseItem`; passing [`None`] clears both fields
    /// (C# `Guid.Empty` / `null`).
    pub fn set_parent(&mut self, parent: Option<(Uuid, BaseItemKind)>) {
        if let Some((id, kind)) = parent {
            self.parent_id = id;
            self.parent_type = Some(kind);
        } else {
            self.parent_id = Uuid::nil();
            self.parent_type = None;
        }
    }

    /// Translates a set of [`ItemFilter`] flags onto the tri-state boolean
    /// fields. Mirrors C# `ApplyFilters`.
    ///
    /// # Errors
    ///
    /// Returns [`ConflictingFilters`] when the set contains a contradictory
    /// pair (folder/not-folder, played/unplayed, likes/dislikes), matching the
    /// C# `ArgumentException`.
    pub fn apply_filters(&mut self, filters: &[ItemFilter]) -> Result<(), ConflictingFilters> {
        let has = |f: ItemFilter| filters.contains(&f);
        for &filter in filters {
            match filter {
                ItemFilter::IsFolder => {
                    if has(ItemFilter::IsNotFolder) {
                        return Err(ConflictingFilters);
                    }
                    self.is_folder = Some(true);
                }
                ItemFilter::IsNotFolder => {
                    if has(ItemFilter::IsFolder) {
                        return Err(ConflictingFilters);
                    }
                    self.is_folder = Some(false);
                }
                ItemFilter::IsUnplayed => {
                    if has(ItemFilter::IsPlayed) {
                        return Err(ConflictingFilters);
                    }
                    self.is_played = Some(false);
                }
                ItemFilter::IsPlayed => {
                    if has(ItemFilter::IsUnplayed) {
                        return Err(ConflictingFilters);
                    }
                    self.is_played = Some(true);
                }
                ItemFilter::IsFavorite => self.is_favorite = Some(true),
                ItemFilter::IsResumable => self.is_resumable = Some(true),
                ItemFilter::Likes => {
                    if has(ItemFilter::Dislikes) {
                        return Err(ConflictingFilters);
                    }
                    self.is_liked = Some(true);
                }
                ItemFilter::Dislikes => {
                    if has(ItemFilter::Likes) {
                        return Err(ConflictingFilters);
                    }
                    self.is_liked = Some(false);
                }
                ItemFilter::IsFavoriteOrLikes => self.is_favorite_or_liked = Some(true),
            }
        }
        Ok(())
    }
}

/// Returns whether an optional string holds a non-empty value. Mirrors the C#
/// `!string.IsNullOrEmpty` guards used throughout `HasFilters`.
fn is_non_empty(value: Option<&String>) -> bool {
    value.is_some_and(|s| !s.is_empty())
}

/// The error returned by [`InternalItemsQuery::apply_filters`] when the supplied
/// filter set contains a contradictory pair. Mirrors the C#
/// `ArgumentException("Conflicting filters")`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("conflicting item filters")]
pub struct ConflictingFilters;

#[cfg(test)]
mod tests {
    use super::{ConflictingFilters, InternalItemsQuery, SourceType};
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::querying::ItemFilter;
    use uuid::Uuid;

    #[test]
    fn default_matches_csharp_constructor() {
        let q = InternalItemsQuery::default();
        assert!(q.enable_total_record_count);
        assert!(q.group_by_presentation_unique_key);
        assert!(!q.group_by_series_presentation_unique_key);
        assert!(!q.skip_deserialization);
        assert_eq!(q.parent_id, Uuid::nil());
        // dto_options defaults to all-fields.
        assert!(!q.dto_options.fields.is_empty());
        // A fresh query has no narrowing filters.
        assert!(!q.has_filters());
    }

    #[test]
    fn source_type_default_is_library() {
        assert_eq!(SourceType::default(), SourceType::Library);
    }

    #[test]
    fn has_filters_detects_a_set_criterion() {
        let mut q = InternalItemsQuery::default();
        q.include_item_types.push(BaseItemKind::Movie);
        assert!(q.has_filters());

        let q2 = InternalItemsQuery {
            name: Some("Blade".into()),
            ..Default::default()
        };
        assert!(q2.has_filters());

        // An empty string is not a filter (matches C# IsNullOrEmpty).
        let q3 = InternalItemsQuery {
            name: Some(String::new()),
            ..Default::default()
        };
        assert!(!q3.has_filters());
    }

    #[test]
    fn set_parent_sets_and_clears() {
        let mut q = InternalItemsQuery::default();
        let id = Uuid::from_u128(9);
        q.set_parent(Some((id, BaseItemKind::Folder)));
        assert_eq!(q.parent_id, id);
        assert_eq!(q.parent_type, Some(BaseItemKind::Folder));

        q.set_parent(None);
        assert_eq!(q.parent_id, Uuid::nil());
        assert!(q.parent_type.is_none());
    }

    #[test]
    fn apply_filters_translates_flags() {
        let mut q = InternalItemsQuery::default();
        q.apply_filters(&[ItemFilter::IsFavorite, ItemFilter::IsPlayed])
            .expect("no conflict");
        assert_eq!(q.is_favorite, Some(true));
        assert_eq!(q.is_played, Some(true));
    }

    #[test]
    fn apply_filters_rejects_conflicts() {
        let mut q = InternalItemsQuery::default();
        assert_eq!(
            q.apply_filters(&[ItemFilter::IsFolder, ItemFilter::IsNotFolder]),
            Err(ConflictingFilters)
        );
        assert_eq!(
            q.apply_filters(&[ItemFilter::Likes, ItemFilter::Dislikes]),
            Err(ConflictingFilters)
        );
    }
}
