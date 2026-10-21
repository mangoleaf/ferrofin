//! `FilterController` — the query-filter facets under `/Items/Filters*`.
//!
//! Ports:
//!
//! - `GET /Items/Filters`  — the legacy flat-string facets ([`QueryFiltersLegacy`]:
//!   genres, tags, official ratings, years) aggregated over a parent's items.
//! - `GET /Items/Filters2` — the richer facets ([`QueryFilters`]: genre
//!   name/id pairs, the tag names (Jellyfin 12.1+), and since Jellyfin 12 the
//!   audio and subtitle stream languages as `{Name, Value}` pairs for movie and
//!   TV type sets).
//!
//! Both endpoints scope to an optional parent and honor the `includeItemTypes`
//! restriction (a lone `Trailer`/`Program` type skips the parent lookup, exactly
//! as Jellyfin does). The genre facet dispatches to the music-genre aggregate for
//! a music-only type set and to the plain-genre aggregate otherwise.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::{BaseItemKind, MediaType};
use ferrofin_model::dto::{NameGuidPair, NameValuePair};
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::querying::{QueryFilters, QueryFiltersLegacy};
use ferrofin_traits::localization::LocalizationManager;
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::extract::Query;
use crate::handlers::items::resolve_user_opt;
use crate::handlers::query_parse::parse_csv_enums_lenient;
use crate::state::AppState;

/// The query parameters honoured by `GET /Items/Filters`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FiltersLegacyQuery {
    /// The target user; scopes visibility when present.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
    /// Localizes the aggregation to a specific parent item/folder.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    parent_id: Option<Uuid>,
    /// Comma-delimited [`BaseItemKind`] set to include.
    #[serde(default)]
    include_item_types: Option<String>,
    /// Comma-delimited [`MediaType`] set to include.
    #[serde(default)]
    media_types: Option<String>,
}

/// Whether the (single) included type is `Trailer` or `Program`, which Jellyfin
/// treats specially by *not* resolving a parent item.
fn is_trailer_or_program(types: &[BaseItemKind]) -> bool {
    types.len() == 1 && matches!(types[0], BaseItemKind::Trailer | BaseItemKind::Program)
}

/// The parent scope the `/Items/Filters2` facet queries share, as
/// `(ancestor_ids, parent_id)` — v12.2 `FilterController.cs:198-210`:
///
/// ```text
/// if ((recursive ?? true) || parentItem is UserView || parentItem is ICollectionFolder)
///     AncestorIds = parentItem is null ? [] : [parentItem.Id]   // its descendants
/// else
///     Parent = parentItem                                       // its direct children
/// ```
///
/// `ICollectionFolder` is a library (`CollectionFolder`) or a `BasePluginFolder`;
/// the playlists folder derives from it, stored as `PlaylistsFolder` or the
/// older `ManualPlaylistsFolder`. A `parentId` that names no item leaves
/// `parentItem` null, and so no scope at all: the facets then cover everything
/// the user can see.
///
/// Upstream's `Parent` reaches `TranslateQuery` as a strict `ParentId` equality
/// (`TranslateQuery.cs:320-322`): the direct children, never a box set's or a
/// playlist's linked members, nor a root folder's rewritten children. The
/// facet queries say so with `physical_children_only`, which is that shape.
fn parent_scope(
    parent_id: Uuid,
    parent: Option<&BaseItemEntity>,
    recursive: Option<bool>,
) -> (Vec<Uuid>, Uuid) {
    let Some(parent) = parent else {
        return (Vec::new(), Uuid::nil());
    };
    let view_or_collection_folder = matches!(
        BaseItemKind::from_stored_type_name(&parent.type_),
        Some(
            BaseItemKind::UserView
                | BaseItemKind::CollectionFolder
                | BaseItemKind::BasePluginFolder
                | BaseItemKind::PlaylistsFolder
                | BaseItemKind::ManualPlaylistsFolder
        )
    );
    if recursive.unwrap_or(true) || view_or_collection_folder {
        (vec![parent_id], Uuid::nil())
    } else {
        (Vec::new(), parent_id)
    }
}

/// `GET /Items/Filters` — the legacy flat-string filter facets.
///
/// Port of `FilterController.GetQueryFiltersLegacy`. The facets are aggregated
/// from the matching items' distinct genre/tag/rating/year values.
#[utoipa::path(
    get,
    path = "/Items/Filters",
    responses((status = 200, description = "Legacy filters returned", body = QueryFiltersLegacy)),
    tag = "ferrofin"
)]
async fn get_query_filters_legacy(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<FiltersLegacyQuery>,
) -> Result<Json<QueryFiltersLegacy>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let include_item_types: Vec<BaseItemKind> =
        parse_csv_enums_lenient(query.include_item_types.as_deref());
    let media_types: Vec<MediaType> = parse_csv_enums_lenient(query.media_types.as_deref());

    // A lone Trailer/Program type set skips the parent lookup; otherwise a parent
    // that is not a folder yields empty filters (the C# `is not Folder` guard).
    // Without the OOP folder tree we always aggregate over the parent's subtree.
    let mut ancestor_ids = Vec::new();
    if !is_trailer_or_program(&include_item_types)
        && let Some(parent) = query.parent_id
    {
        ancestor_ids.push(parent);
    }

    let internal = InternalItemsQuery {
        user,
        media_types,
        include_item_types,
        recursive: true,
        enable_total_record_count: false,
        ancestor_ids,
        ..InternalItemsQuery::default()
    };
    let filters = state.library.get_query_filters_legacy(&internal).await?;
    Ok(Json(filters))
}

/// The query parameters honoured by `GET /Items/Filters2`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
struct FiltersQuery {
    /// The target user; scopes visibility when present.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
    /// Localizes the aggregation to a specific parent item/folder.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    parent_id: Option<Uuid>,
    /// Comma-delimited [`BaseItemKind`] set to include.
    #[serde(default)]
    include_item_types: Option<String>,
    /// Optional live-tv "is airing" filter.
    #[serde(default)]
    is_airing: Option<bool>,
    /// Optional live-tv "is movie" filter.
    #[serde(default)]
    is_movie: Option<bool>,
    /// Optional live-tv "is sports" filter.
    #[serde(default)]
    is_sports: Option<bool>,
    /// Optional live-tv "is kids" filter.
    #[serde(default)]
    is_kids: Option<bool>,
    /// Optional live-tv "is news" filter.
    #[serde(default)]
    is_news: Option<bool>,
    /// Optional live-tv "is series" filter.
    #[serde(default)]
    is_series: Option<bool>,
    /// Whether to aggregate over the parent's descendants (defaults `true`) or
    /// only its direct children — see [`parent_scope`].
    #[serde(default)]
    recursive: Option<bool>,
}

/// Whether the type set is one of the four music kinds Jellyfin routes to the
/// music-genre aggregate.
fn is_music_type_set(types: &[BaseItemKind]) -> bool {
    types.len() == 1
        && matches!(
            types[0],
            BaseItemKind::MusicAlbum
                | BaseItemKind::MusicVideo
                | BaseItemKind::MusicArtist
                | BaseItemKind::Audio
        )
}

/// Whether the type set gets the audio/subtitle language facets: it names a
/// movie or TV kind (v12.2 `FilterController.cs:242-245`).
fn has_language_facets(types: &[BaseItemKind]) -> bool {
    types.iter().any(|kind| {
        matches!(
            kind,
            BaseItemKind::Movie
                | BaseItemKind::Series
                | BaseItemKind::Season
                | BaseItemKind::Episode
        )
    })
}

/// The item kinds the stream-language query runs over: the requested set, with
/// `Episode` appended when it names `Series` or `Season` but not `Episode` —
/// streams hang off episodes, never shows or seasons (v12.2
/// `FilterController.cs:212-217`, `[.. includeItemTypes, BaseItemKind.Episode]`).
fn stream_language_types(types: &[BaseItemKind]) -> Vec<BaseItemKind> {
    let mut stream_types = types.to_vec();
    if (types.contains(&BaseItemKind::Series) || types.contains(&BaseItemKind::Season))
        && !types.contains(&BaseItemKind::Episode)
    {
        stream_types.push(BaseItemKind::Episode);
    }
    stream_types
}

/// The facet pairs for a list of stream-language codes (v12.2
/// `FilterController.cs:246-271`): `Name` is `"{DisplayName} ({code})"` for a
/// code `FindLanguageInfo` knows — the raw `iso6392.txt` name, untruncated —
/// and the bare code otherwise; `Value` is the code. Ordered by `Name` with
/// `OrderBy`'s `Comparer<string>.Default`, i.e. the request's `CurrentCulture`
/// (a stable, linguistic sort, not an ordinal one).
fn language_pairs(
    localization: &dyn LocalizationManager,
    languages: Vec<String>,
) -> Vec<NameValuePair> {
    let mut pairs: Vec<NameValuePair> = languages
        .into_iter()
        .map(|code| {
            let name = match localization.find_language_info(&code) {
                Some(culture) => format!("{} ({code})", culture.display_name),
                None => code.clone(),
            };
            NameValuePair::new(name, code)
        })
        .collect();
    let comparer =
        ferrofin_util::current_culture::comparer(&ferrofin_util::current_culture::capture()).ok();
    pairs.sort_by(|a, b| {
        let (a, b) = (
            a.name.as_deref().unwrap_or_default(),
            b.name.as_deref().unwrap_or_default(),
        );
        comparer
            .as_ref()
            .map_or_else(|| a.cmp(b), |comparer| comparer.compare(a, b))
    });
    pairs
}

/// `GET /Items/Filters2` — the genre, tag and stream-language filter facets.
///
/// Port of `FilterController.GetQueryFilters` (v12.2). The genre facet routes
/// to the music-genre aggregate for a music-only type set. The tag facet
/// (Jellyfin 12.1+, `FilterController.cs:240`) runs for every type set, over
/// the genre query's user, types, live-TV flags and parent scope
/// (`tagQuery`, `:181-196`).
///
/// The audio and subtitle language facets (Jellyfin 12+) are filled only for
/// a type set naming `Movie`, `Series`, `Season` or `Episode`
/// (`FilterController.cs:242-272`), from a second query that shares the genre
/// query's user, live-TV flags and parent scope (`:161-178`, `:198-210`) but
/// includes owned items — a language may exist only on an alternate version —
/// and widens a show/season set to its episodes (`:212-217`).
///
/// A `parentId` scopes every facet query to the parent's descendants, or to its
/// direct children for `recursive=false` under a parent that is neither a view
/// nor a collection folder ([`parent_scope`]). A lone `Trailer` or `Program`
/// type set ignores the parent (`FilterController.cs:132-137`).
#[utoipa::path(
    get,
    path = "/Items/Filters2",
    responses((status = 200, description = "Filters returned", body = QueryFilters)),
    tag = "ferrofin"
)]
async fn get_query_filters(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<FiltersQuery>,
) -> Result<Json<QueryFilters>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let include_item_types: Vec<BaseItemKind> =
        parse_csv_enums_lenient(query.include_item_types.as_deref());
    let language_facets = has_language_facets(&include_item_types);
    let stream_types = stream_language_types(&include_item_types);

    // `parentItem` (`FilterController.cs:131-141`): none for a lone Trailer or
    // Program type set, else the item `parentId` names, if there is one.
    let (ancestor_ids, parent_id) = match query.parent_id {
        Some(id) if !is_trailer_or_program(&include_item_types) => {
            let parent = state.library.get_item_by_id(id).await?;
            parent_scope(id, parent.as_ref(), query.recursive)
        }
        _ => (Vec::new(), Uuid::nil()),
    };

    let is_music = is_music_type_set(&include_item_types);
    let base = InternalItemsQuery {
        user,
        include_item_types,
        // `DtoOptions { Fields = [], EnableImages = false, EnableUserData =
        // false }` (v12 `FilterController.cs:147-152`): no `ItemCounts`, so
        // the by-name repository computes no per-kind counts for a facet list.
        dto_options: DtoOptions {
            fields: Vec::new(),
            enable_images: false,
            enable_user_data: false,
            ..DtoOptions::default()
        },
        is_airing: query.is_airing,
        is_movie: query.is_movie,
        is_sports: query.is_sports,
        is_kids: query.is_kids,
        is_news: query.is_news,
        is_series: query.is_series,
        ancestor_ids,
        parent_id,
        physical_children_only: !parent_id.is_nil(),
        ..InternalItemsQuery::default()
    };

    let mut filters = QueryFilters::default();

    // Genre facet: music-genre aggregate for a music-only type set, else genres.
    let genre_result = if is_music {
        state.library.get_music_genres(&base).await?
    } else {
        state.library.get_genres(&base).await?
    };
    filters.genres = genre_result
        .items
        .into_iter()
        .filter_map(|iwc| {
            // Upstream's facet is `(item.Name, item.Id)` off a `Guid` column, so
            // it can only ever emit a real id. Ferrofin stores the id as text, so
            // a row that is not a Guid is **dropped** rather than published as
            // the nil GUID: `Id` is what the client sends back as `genreIds=…`,
            // and a nil one selects nothing, so emitting it would only add a
            // facet chip that silently returns an empty library.
            let id = Uuid::parse_str(&iwc.item.id).ok()?;
            Some(NameGuidPair {
                name: iwc.item.name,
                id,
            })
        })
        .collect();

    // `filters.Tags = _libraryManager.GetTagNames(tagQuery)`: `tagQuery` is the
    // genre query's twin, with no owned items and no episode widening.
    filters.tags = state.library.get_tag_names(&base).await?;

    if language_facets {
        let stream_query = InternalItemsQuery {
            // "It's possible that different languages are only available on
            // alternative versions. To fetch them all, owned items are included."
            include_owned_items: true,
            include_item_types: stream_types,
            ..base
        };
        let mut languages = state
            .library
            .get_media_stream_languages_by_type(
                &[MediaStreamType::Audio, MediaStreamType::Subtitle],
                &stream_query,
            )
            .await?;
        let localization = state.localization.as_ref();
        filters.audio_languages = language_pairs(
            localization,
            languages
                .remove(&MediaStreamType::Audio)
                .unwrap_or_default(),
        );
        filters.subtitle_languages = language_pairs(
            localization,
            languages
                .remove(&MediaStreamType::Subtitle)
                .unwrap_or_default(),
        );
    }

    Ok(Json(filters))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Items/Filters", get(get_query_filters_legacy))
        .route("/Items/Filters2", get(get_query_filters))
}
crate::query::query_parameters! {
    FiltersLegacyQuery {
        "includeItemTypes" => ',',
        "mediaTypes" => ',',
    } => [("get", "/Items/Filters")];
    FiltersQuery {
        "includeItemTypes" => ',',
    } => [("get", "/Items/Filters2")];
}
