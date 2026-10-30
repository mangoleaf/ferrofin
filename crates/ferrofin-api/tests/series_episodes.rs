//! `GET /Shows/{seriesId}/Episodes` over a whole series: the exact episode
//! list each request shape returns, and the heap allocations the handler
//! makes per episode.
//!
//! The handler reads every season and episode of the series, resolves each
//! episode's season, places specials by their aired position, and lists one
//! season or all of them. The first test pins the list it returns for one
//! season, all seasons, specials placed by `AirsBeforeSeasonNumber` and
//! `AirsAfterSeasonNumber`, missing and virtual episodes, episodes whose season
//! is read on its own or is gone, and two seasons with the same number whose
//! keys differ only in case, with and without `DisplaySpecialsWithinSeasons`.
//!
//! The last test counts the allocations a request makes on its own thread,
//! with this binary's global allocator, and checks that each episode of the
//! series adds only a few. Copying each episode's season, copying each listed
//! episode once per season that lists it, a presentation key per season and
//! episode, and parsing each episode's `Data` into a `serde_json::Value` made
//! about fifty per episode (gl-1003).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    ApiKeyAuthService, FakeActivity, FakeApiKeys, FakeAppHost, FakeAuthContext,
    FakeClientEventLogger, FakeCollections, FakeDevices, FakeDisplayPreferences, FakeFileSystem,
    FakeLocalization, FakeLyrics, FakeMediaSegments, FakeMediaSources, FakeMusic, FakePaths,
    FakePlaylists, FakeProviders, FakeQuickConnect, FakeSearch, FakeSessions, FakeSimilarItems,
    FakeSubtitles, FakeSystem, FakeTasks, FakeTrickplay, FakeTvSeries, FakeUserData, FakeUserViews,
    FakeUsers,
};
use ferrofin_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::branding::BrandingOptions;
use ferrofin_model::configuration::ServerConfiguration;
use ferrofin_model::dto::{BaseItemDto, ItemCounts};
use ferrofin_model::querying::{QueryFiltersLegacy, QueryResult};
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::LibraryManager;
use ferrofin_traits::options::{
    DeleteOptions, DtoOptions, InternalItemsQuery, InternalPeopleQuery,
};
use ferrofin_traits::persistence::ItemWithCounts;
use ferrofin_traits::system::ServerApplicationPaths;
use tower::ServiceExt;
use uuid::Uuid;

thread_local! {
    /// Whether this thread's allocations are being counted.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

/// Allocations (and reallocations) made by a counting thread.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

/// The system allocator, counting what the current thread allocates while
/// [`COUNTING`] is set.
struct Counting;

impl Counting {
    fn count() {
        if COUNTING.try_with(Cell::get).unwrap_or(false) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// SAFETY: every call is forwarded unchanged to `System`; the counter is an
// atomic and the flag a const-initialised thread-local, neither allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::count();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        Self::count();
        // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::count();
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const SERIES_TYPE: &str = "MediaBrowser.Controller.Entities.TV.Series";
const SEASON_TYPE: &str = "MediaBrowser.Controller.Entities.TV.Season";
const EPISODE_TYPE: &str = "MediaBrowser.Controller.Entities.TV.Episode";

const SERIES: Uuid = Uuid::from_u128(0x5E);
const SERIES_KEY: &str = "series-key";
const SPECIALS: Uuid = Uuid::from_u128(0x5A00);
const SEASON_1: Uuid = Uuid::from_u128(0x5A01);
const SEASON_2: Uuid = Uuid::from_u128(0x5A02);
/// A second season 2, keyed like [`SEASON_2`] but for case.
const SEASON_2_AGAIN: Uuid = Uuid::from_u128(0x5A22);
/// A season with no number and no path: a virtual season.
const UNKNOWN_SEASON: Uuid = Uuid::from_u128(0x5A0F);
/// A season the series query does not return, read on its own.
const HIDDEN_SEASON: Uuid = Uuid::from_u128(0x5A03);
/// A season id no row has any more.
const DELETED_SEASON: Uuid = Uuid::from_u128(0x5ADD);

/// The series row.
fn series() -> BaseItemEntity {
    BaseItemEntity {
        id: guid_to_db(SERIES),
        type_: SERIES_TYPE.to_owned(),
        name: Some("The Series".to_owned()),
        presentation_unique_key: Some(SERIES_KEY.to_owned()),
        path: Some("/media/The Series".to_owned()),
        is_folder: true,
        ..Default::default()
    }
}

/// A season of [`series`]: `number` is its index, `key` its presentation key,
/// and `physical` whether it has a folder on disk.
fn season(id: Uuid, number: Option<i64>, key: &str, physical: bool) -> BaseItemEntity {
    let name = number.map_or_else(|| "Unknown Season".to_owned(), |n| format!("Season {n}"));
    BaseItemEntity {
        id: guid_to_db(id),
        type_: SEASON_TYPE.to_owned(),
        name: Some(name.clone()),
        sort_name: Some(name.to_lowercase()),
        index_number: number,
        presentation_unique_key: Some(key.to_owned()),
        series_presentation_unique_key: Some(SERIES_KEY.to_owned()),
        series_id: Some(guid_to_db(SERIES)),
        series_name: Some("The Series".to_owned()),
        parent_id: Some(guid_to_db(SERIES)),
        path: physical.then(|| format!("/media/The Series/{name}")),
        is_folder: true,
        ..Default::default()
    }
}

/// One episode row as the series query returns it.
struct Episode<'a> {
    id: u128,
    name: &'a str,
    /// Its `SeasonId` (and `ParentId`), when it has one.
    season: Option<Uuid>,
    /// Its `ParentIndexNumber`.
    season_number: Option<i64>,
    /// Its `IndexNumber`.
    number: Option<i64>,
    /// Its `Data` blob.
    data: Option<&'a str>,
    /// Whether it has a file; a missing episode has none.
    physical: bool,
    /// Whether, without a season, it is stored under the series itself.
    under_series: bool,
}

impl Episode<'_> {
    fn row(&self) -> BaseItemEntity {
        let parent = self.season.map(guid_to_db);
        BaseItemEntity {
            id: guid_to_db(Uuid::from_u128(self.id)),
            type_: EPISODE_TYPE.to_owned(),
            name: Some(self.name.to_owned()),
            sort_name: Some(self.name.to_lowercase()),
            parent_index_number: self.season_number,
            index_number: self.number,
            season_id: parent.clone(),
            parent_id: parent.or_else(|| self.under_series.then(|| guid_to_db(SERIES))),
            series_id: Some(guid_to_db(SERIES)),
            series_name: Some("The Series".to_owned()),
            series_presentation_unique_key: Some(SERIES_KEY.to_owned()),
            presentation_unique_key: Some(format!("{SERIES_KEY}-{}", self.id)),
            data: self.data.map(str::to_owned),
            path: self
                .physical
                .then(|| format!("/media/The Series/{}.mkv", self.name)),
            is_virtual_item: !self.physical,
            ..Default::default()
        }
    }
}

const S01E02: u128 = 0xE102;
const S02E01: u128 = 0xE201;
/// An alternate version of `S01E02`.
const S01E02_ALTERNATE: u128 = 0xEA02;

/// Every season and episode of the series, in the order the series query
/// returns them (`SortName`).
#[allow(clippy::too_many_lines)] // one row per episode
fn series_rows() -> Vec<BaseItemEntity> {
    let ep = |id: u128,
              name: &'static str,
              season: Option<Uuid>,
              season_number: Option<i64>,
              number: Option<i64>|
     -> Episode<'static> {
        Episode {
            id,
            name,
            season,
            season_number,
            number,
            data: Some("{}"),
            physical: true,
            under_series: false,
        }
    };
    let special = |id: u128, name: &'static str, data: &'static str| Episode {
        data: Some(data),
        ..ep(id, name, Some(SPECIALS), Some(0), None)
    };
    let episodes = [
        ep(0xE101, "S01E01", Some(SEASON_1), Some(1), Some(1)),
        ep(S01E02, "S01E02", Some(SEASON_1), Some(1), Some(2)),
        ep(0xE103, "S01E03", Some(SEASON_1), Some(1), Some(3)),
        Episode {
            physical: false,
            ..ep(0xE104, "S01E04 missing", Some(SEASON_1), Some(1), Some(4))
        },
        // Stored under the series itself: its season is found by number.
        Episode {
            under_series: true,
            ..ep(0xE105, "S01E05 under the series", None, Some(1), Some(5))
        },
        ep(
            0xE106,
            "S01E06 deleted season",
            Some(DELETED_SEASON),
            Some(1),
            Some(6),
        ),
        ep(S02E01, "S02E01", Some(SEASON_2), Some(2), Some(1)),
        ep(0xE202, "S02E02", Some(SEASON_2), Some(2), Some(2)),
        ep(
            0xE203,
            "S02E03 again",
            Some(SEASON_2_AGAIN),
            Some(2),
            Some(3),
        ),
        ep(
            0xE204,
            "S02E00 again",
            Some(SEASON_2_AGAIN),
            Some(2),
            Some(0),
        ),
        ep(
            0xE301,
            "S03E01 hidden season",
            Some(HIDDEN_SEASON),
            Some(3),
            Some(1),
        ),
        Episode {
            physical: false,
            ..ep(0xEF01, "Unknown missing", Some(UNKNOWN_SEASON), None, None)
        },
        Episode {
            physical: false,
            ..ep(0xEF02, "No season missing", None, None, None)
        },
        ep(0xEF03, "No season on disk", None, None, Some(7)),
        special(
            0xE001,
            "Special before S02E02",
            r#"{"AirsBeforeSeasonNumber":2,"AirsBeforeEpisodeNumber":2}"#,
        ),
        special(
            0xE002,
            "Special after S01",
            r#"{"AirsAfterSeasonNumber":1}"#,
        ),
        special(0xE003, "Special plain", "{}"),
        special(0xE004, "Special broken data", "not json"),
        special(
            0xE005,
            "Special string position",
            r#"{"AirsBeforeSeasonNumber":"1"}"#,
        ),
        special(
            0xE006,
            "Special float position",
            r#"{"AirsBeforeSeasonNumber":1.0}"#,
        ),
        special(
            0xE007,
            "Special nested data",
            r#"{"Nested":{"A":[1,{"B":"café \"x\""}]},"AirsBeforeSeasonNumber":1,"AirsBeforeEpisodeNumber":1}"#,
        ),
        special(
            0xE008,
            "Special duplicate member",
            r#"{"AirsBeforeSeasonNumber":5,"AirsBeforeSeasonNumber":2}"#,
        ),
        special(
            0xE009,
            "Special huge position",
            r#"{"AirsBeforeSeasonNumber":9223372036854775808}"#,
        ),
        special(
            0xE00A,
            "Special negative position",
            r#"{"AirsAfterSeasonNumber":-1,"AirsBeforeSeasonNumber":1}"#,
        ),
        special(0xE00B, "Special not an object", "[1,2]"),
        Episode {
            data: None,
            ..special(0xE00C, "Special no data", "")
        },
    ];
    let seasons = [
        season(SEASON_1, Some(1), "season-1", true),
        season(SEASON_2, Some(2), "season-2", true),
        season(SEASON_2_AGAIN, Some(2), "SEASON-2", true),
        season(SPECIALS, Some(0), "specials", true),
        season(UNKNOWN_SEASON, None, "unknown", false),
    ];
    let mut rows: Vec<_> = episodes.iter().map(Episode::row).collect();
    // Interleave the seasons as a SortName order would.
    for (at, season) in [0, 7, 10, 15, 30].into_iter().zip(seasons) {
        rows.insert(at.min(rows.len()), season);
    }
    rows
}

/// Rows `get_item_by_id` finds besides the series query's: the series, the
/// hidden season and the alternate version.
fn rows_by_id(rows: &[BaseItemEntity]) -> HashMap<Uuid, BaseItemEntity> {
    let mut alternate = Episode {
        id: S01E02_ALTERNATE,
        name: "S01E02 alternate",
        season: Some(SEASON_1),
        season_number: Some(1),
        number: Some(2),
        data: None,
        physical: true,
        under_series: false,
    }
    .row();
    alternate.primary_version_id = Some(guid_to_db(Uuid::from_u128(S01E02)));
    rows.iter()
        .cloned()
        .chain([
            series(),
            season(HIDDEN_SEASON, Some(3), "hidden-3", true),
            alternate,
        ])
        .filter_map(|row| Uuid::parse_str(&row.id).ok().map(|id| (id, row)))
        .collect()
}

/// A [`LibraryManager`] holding one series: the series query returns its
/// rows, and `get_item_by_id` finds any row by id.
struct SeriesLibrary {
    rows: Vec<BaseItemEntity>,
    by_id: HashMap<Uuid, BaseItemEntity>,
    /// Rows the next series query hands over as they are, so a counted
    /// request does not count the library copying them.
    staged: Mutex<Option<Vec<BaseItemEntity>>>,
}

impl SeriesLibrary {
    fn new(rows: Vec<BaseItemEntity>) -> Self {
        Self {
            by_id: rows_by_id(&rows),
            rows,
            staged: Mutex::new(None),
        }
    }

    fn stage(&self) {
        *self.staged.lock().expect("staged") = Some(self.rows.clone());
    }
}

#[async_trait]
impl LibraryManager for SeriesLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        Ok(self.by_id.get(&id).cloned())
    }
    async fn get_item_list(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        assert_eq!(
            query.series_presentation_unique_key.as_deref(),
            Some(SERIES_KEY),
            "only the series query is expected"
        );
        let staged = self.staged.lock().expect("staged").take();
        Ok(staged.unwrap_or_else(|| self.rows.clone()))
    }
    async fn query_items(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_item_ids(&self, _q: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!()
    }
    async fn get_latest_item_list(
        &self,
        _q: &InternalItemsQuery,
        _c: ferrofin_model::data::CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!()
    }
    async fn create_items(
        &self,
        _i: &[BaseItemEntity],
        _p: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn update_items(
        &self,
        _i: &[BaseItemEntity],
        _p: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn delete_item(&self, _id: Uuid, _o: &DeleteOptions) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_people(
        &self,
        _q: &InternalPeopleQuery,
    ) -> Result<Vec<PeopleEntity>, ServiceError> {
        unimplemented!()
    }
    async fn get_people_names(
        &self,
        _q: &InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
    async fn get_count(&self, _q: &InternalItemsQuery) -> Result<i32, ServiceError> {
        unimplemented!()
    }
    async fn get_item_counts(&self, _q: &InternalItemsQuery) -> Result<ItemCounts, ServiceError> {
        unimplemented!()
    }
    async fn get_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_studios(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_music_genres(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_album_artists(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unimplemented!()
    }
    async fn get_query_filters_legacy(
        &self,
        _q: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        unimplemented!()
    }
    async fn get_media_stream_languages(
        &self,
        _stream_type: ferrofin_model::entities::MediaStreamType,
        _q: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!()
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A server configuration with `DisplaySpecialsWithinSeasons` set as given.
struct Config(Arc<ServerConfiguration>);

impl Config {
    fn new(display_specials_within_seasons: bool) -> Self {
        Self(Arc::new(ServerConfiguration {
            display_specials_within_seasons,
            ..ServerConfiguration::default()
        }))
    }
}

#[async_trait]
impl ServerConfigurationManager for Config {
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        Arc::new(FakePaths)
    }
    async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
        Ok(Arc::clone(&self.0))
    }
    async fn update_configuration(&self, _c: &ServerConfiguration) -> Result<(), ServiceError> {
        unimplemented!()
    }
    async fn get_branding(&self) -> Result<BrandingOptions, ServiceError> {
        unimplemented!()
    }
    async fn update_branding(&self, _b: &BrandingOptions) -> Result<(), ServiceError> {
        unimplemented!()
    }
}

/// A [`DtoService`] projecting each row to its id and name.
struct NameDto;

#[async_trait]
impl DtoService for NameDto {
    async fn get_primary_image_aspect_ratio(&self, _id: Uuid) -> Result<Option<f64>, ServiceError> {
        unimplemented!()
    }
    async fn get_base_item_dto(
        &self,
        _item: &BaseItemEntity,
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
    ) -> Result<BaseItemDto, ServiceError> {
        unimplemented!()
    }
    async fn get_base_item_dtos(
        &self,
        items: &[BaseItemEntity],
        _options: &DtoOptions,
        _user: Option<&UserEntity>,
        _owner_id: Option<Uuid>,
        _skip_visibility_check: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError> {
        Ok(items
            .iter()
            .map(|item| BaseItemDto {
                id: Uuid::parse_str(&item.id).unwrap_or_default(),
                name: item.name.clone(),
                ..BaseItemDto::default()
            })
            .collect())
    }
    async fn get_item_by_name_dto(
        &self,
        _item: &BaseItemEntity,
        _options: &DtoOptions,
        _tagged_item_ids: Option<&[Uuid]>,
        _user: Option<&UserEntity>,
    ) -> Result<BaseItemDto, ServiceError> {
        unimplemented!()
    }
}

/// The router over `library`, authenticated as an API key (which lists
/// missing episodes), with specials within seasons as given.
fn router(library: Arc<SeriesLibrary>, specials_within_seasons: bool) -> Router {
    create_router(AppState::new(
        library,
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(Config::new(specials_within_seasons)),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(NameDto),
        Arc::new(FakeAuthContext),
        Arc::new(ApiKeyAuthService),
        Arc::new(FakeQuickConnect),
        Arc::new(FakePlaylists),
        Arc::new(FakeCollections),
        Arc::new(FakeTvSeries),
        Arc::new(FakeSubtitles),
        Arc::new(FakeLyrics),
        Arc::new(FakeMediaSegments),
        Arc::new(FakeTrickplay),
        Arc::new(FakeDevices),
        Arc::new(FakeClientEventLogger),
        Arc::new(FakeApiKeys),
        Arc::new(FakeLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(FakeActivity),
        Arc::new(FakeFileSystem),
        Arc::new(FakeTasks),
    ))
}

/// One GET through `router`: the status and the body.
async fn get(router: Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let response = router
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("Authorization", "Bearer token")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, body.to_vec())
}

/// The episode names a request lists, and its total record count.
async fn listed(router: Router, query: &str) -> (Vec<String>, Option<i32>) {
    let (status, body) = get(router, &format!("/Shows/{SERIES}/Episodes{query}")).await;
    assert_eq!(status, StatusCode::OK, "{query}");
    let result: QueryResult<BaseItemDto> = serde_json::from_slice(&body).expect("episodes");
    let names = result
        .items
        .into_iter()
        .map(|item| item.name.unwrap_or_default())
        .collect();
    (names, Some(result.total_record_count))
}

/// The list every request shape returns, with specials within seasons and
/// without. The expected lists are what the handler returned before gl-1003
/// changed how it holds the rows; none may change.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one row per request shape
async fn every_request_shape_lists_the_same_episodes_in_the_same_order() {
    // The whole series, with specials within seasons or without: each
    // season's list in turn, a special in its own season's.
    let all = [
        "S01E01",
        "S01E02",
        "S01E03",
        "S01E04 missing",
        "S01E05 under the series",
        "S01E06 deleted season",
        "S02E00 again",
        "S02E01",
        "S02E02",
        "S02E03 again",
        "Special before S02E02",
        "Special after S01",
        "Special plain",
        "Special broken data",
        "Special string position",
        "Special float position",
        "Special nested data",
        "Special duplicate member",
        "Special huge position",
        "Special negative position",
        "Special not an object",
        "Special no data",
        "Unknown missing",
        "No season missing",
        "No season on disk",
    ];
    let season_1 = &all[..6];
    let season_1_own = &all[..5];
    let season_2 = &all[6..10];
    let specials = &all[10..22];
    let unknown = &all[22..];
    let missing = ["S01E04 missing", "Unknown missing", "No season missing"];
    let on_disk: Vec<_> = all
        .iter()
        .copied()
        .filter(|name| !missing.contains(name))
        .collect();
    let season_1_placed = [
        &["Special nested data"][..],
        season_1,
        &["Special after S01"],
    ]
    .concat();
    let season_2_placed = [
        "Special duplicate member",
        "S02E00 again",
        "S02E01",
        "Special before S02E02",
        "S02E02",
        "S02E03 again",
    ];
    let hidden = ["S03E01 hidden season"];
    let adjacent = ["S02E00 again", "S02E01", "S02E02"];
    let id = |id: u128| Uuid::from_u128(id);
    let both = |query: String, names: &[&'static str]| {
        [
            (true, query.clone(), names.to_vec()),
            (false, query, names.to_vec()),
        ]
    };
    let cases: Vec<(bool, String, Vec<&str>)> = [
        both(String::new(), &all).to_vec(),
        both("?season=0".to_owned(), specials).to_vec(),
        vec![
            (true, "?season=1".to_owned(), season_1_placed.clone()),
            (false, "?season=1".to_owned(), season_1_own.to_vec()),
            (true, "?season=2".to_owned(), season_2_placed.to_vec()),
            (false, "?season=2".to_owned(), season_2.to_vec()),
        ],
        both("?season=7".to_owned(), &[]).to_vec(),
        vec![
            (
                true,
                format!("?seasonId={SEASON_1}"),
                season_1_placed.clone(),
            ),
            (
                false,
                format!("?seasonId={SEASON_1}"),
                season_1_own.to_vec(),
            ),
            // Its key is season 2's but for case: it lists season 2's episodes too.
            (
                true,
                format!("?seasonId={SEASON_2_AGAIN}"),
                season_2_placed.to_vec(),
            ),
            (
                false,
                format!("?seasonId={SEASON_2_AGAIN}"),
                season_2.to_vec(),
            ),
        ],
        both(format!("?seasonId={SPECIALS}"), specials).to_vec(),
        vec![
            (
                true,
                format!("?seasonId={UNKNOWN_SEASON}"),
                unknown.to_vec(),
            ),
            (
                false,
                format!("?seasonId={UNKNOWN_SEASON}"),
                vec!["Unknown missing"],
            ),
        ],
        both(format!("?seasonId={HIDDEN_SEASON}"), &hidden).to_vec(),
        both(format!("?startItemId={}", id(S01E02)), &all[1..]).to_vec(),
        // An alternate version starts at its primary episode.
        both(format!("?startItemId={}", id(S01E02_ALTERNATE)), &all[1..]).to_vec(),
        vec![
            (
                true,
                format!("?season=1&startItemId={}", id(S01E02)),
                season_1_placed[2..].to_vec(),
            ),
            (
                false,
                format!("?season=1&startItemId={}", id(S01E02)),
                season_1_own[1..].to_vec(),
            ),
        ],
        both("?isMissing=true".to_owned(), &missing).to_vec(),
        both("?isMissing=false".to_owned(), &on_disk).to_vec(),
        both(format!("?adjacentTo={}", id(S02E01)), &adjacent).to_vec(),
    ]
    .concat();
    let library = Arc::new(SeriesLibrary::new(series_rows()));
    for (specials_within_seasons, query, expected) in cases {
        let (names, total) = listed(
            router(Arc::clone(&library), specials_within_seasons),
            &query,
        )
        .await;
        assert_eq!(
            (names, total),
            (
                expected.iter().map(ToString::to_string).collect::<Vec<_>>(),
                i32::try_from(expected.len()).ok()
            ),
            "{query} (specials within seasons {specials_within_seasons})"
        );
    }
    // Paging counts the whole list.
    for specials_within_seasons in [true, false] {
        let (names, total) = listed(
            router(Arc::clone(&library), specials_within_seasons),
            "?startIndex=2&limit=3",
        )
        .await;
        assert_eq!(
            (names, total),
            (
                all[2..5].iter().map(ToString::to_string).collect(),
                Some(25)
            )
        );
    }
}

/// The numbered seasons of [`long_series_rows`].
const LONG_SEASONS: u128 = 6;

/// A long-series episode's `Data`: a few members, nested ones and an escaped
/// string among them, as a scanned episode's blob has.
const LONG_DATA: &str = r#"{"Overview":"The crew\u2019s plan goes \"wrong\".","ProviderIds":{"Tvdb":"12345","Imdb":"tt0000001"},"ImageInfos":[{"Path":"/media/thumb.jpg","Type":0,"Width":1920,"Height":1080}],"Tags":["one","two"],"IndexNumberEnd":null}"#;

/// A series of [`LONG_SEASONS`] seasons of `per_season` episodes, each with
/// [`LONG_DATA`], and one special per season airing before its second
/// episode.
fn long_series_rows(per_season: u128) -> Vec<BaseItemEntity> {
    let number = |n: u128| Some(i64::try_from(n).expect("number"));
    let mut rows = vec![season(SPECIALS, Some(0), "specials", true)];
    for season_number in 1..=LONG_SEASONS {
        let season_id = Uuid::from_u128(0x5B00 + season_number);
        rows.push(season(
            season_id,
            number(season_number),
            &format!("season-{season_number}"),
            true,
        ));
        for index in 1..=per_season {
            let name = format!("S{season_number:02}E{index:03}");
            rows.push(
                Episode {
                    id: 0x10_0000 * season_number + index,
                    name: &name,
                    season: Some(season_id),
                    season_number: number(season_number),
                    number: number(index),
                    data: Some(LONG_DATA),
                    physical: true,
                    under_series: false,
                }
                .row(),
            );
        }
    }
    for season_number in 1..=LONG_SEASONS {
        let data =
            format!(r#"{{"AirsBeforeSeasonNumber":{season_number},"AirsBeforeEpisodeNumber":2}}"#);
        let name = format!("Special before S{season_number:02}E002");
        rows.push(
            Episode {
                id: 0xF0_0000 + season_number,
                name: &name,
                season: Some(SPECIALS),
                season_number: Some(0),
                number: number(season_number),
                data: Some(&data),
                physical: true,
                under_series: false,
            }
            .row(),
        );
    }
    rows
}

/// The allocations one request for `query` makes over [`long_series_rows`]
/// of `per_season` episodes a season, and the total it lists.
async fn request_allocations(per_season: u128, specials: bool, query: &str) -> (usize, i32) {
    let library = Arc::new(SeriesLibrary::new(long_series_rows(per_season)));
    let router = router(Arc::clone(&library), specials);
    let uri = format!("/Shows/{SERIES}/Episodes{query}");
    library.stage();
    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.with(|counting| counting.set(true));
    let (status, body) = get(router, &uri).await;
    COUNTING.with(|counting| counting.set(false));
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    assert_eq!(status, StatusCode::OK, "{query}");
    let result: QueryResult<BaseItemDto> = serde_json::from_slice(&body).expect("episodes");
    (allocations, result.total_record_count)
}

/// Episodes a season in the smaller of the two series measured.
const SMALL: u128 = 40;
/// Episodes a season in the larger.
const LARGE: u128 = 80;

/// The most allocations a request may add per episode of the series. What
/// is left is about two for each listed episode in the aired-order sort
/// (`stable_sort_by` merges through new vectors), and up to four while the
/// parser reads a `Data` blob with an escaped string, growing its scratch
/// buffer; everything else the handler keeps per episode lives in vectors
/// that grow by doubling. Copying an episode's season, copying the episode
/// for each season that lists it, a presentation key per season and episode,
/// or a `serde_json::Value` of its `Data` each cost ten or more per episode.
const PER_EPISODE_BUDGET: usize = 8;

/// One season and all of them, with specials within seasons and without:
/// doubling the episodes of every season adds only a few allocations per
/// episode, none of them a copy of a row.
#[tokio::test(flavor = "current_thread")]
async fn a_request_allocates_per_request_not_per_episode() {
    // The first request of the process sets up what every later one reuses.
    request_allocations(1, true, "?limit=1").await;
    let season_2 = Uuid::from_u128(0x5B02);
    let added = usize::try_from((LARGE - SMALL) * LONG_SEASONS).expect("added");
    let mut measured = Vec::new();
    for (specials, query) in [
        (true, "?limit=1".to_owned()),
        (false, "?limit=1".to_owned()),
        (true, "?season=2&limit=1".to_owned()),
        (false, format!("?seasonId={season_2}&limit=1")),
        (true, format!("?seasonId={season_2}&limit=1")),
    ] {
        let (small, small_total) = request_allocations(SMALL, specials, &query).await;
        let (large, large_total) = request_allocations(LARGE, specials, &query).await;
        assert!(
            large_total > small_total,
            "{query}: the larger series lists more"
        );
        measured.push((specials, query, small, large));
    }
    for (specials, query, small, large) in &measured {
        println!("{query} (specials within seasons {specials}): {small} -> {large} allocations");
    }
    for (specials, query, small, large) in measured {
        assert!(
            large.saturating_sub(small) <= PER_EPISODE_BUDGET * added,
            "{query} (specials within seasons {specials}): {added} more episodes made \
             {small} -> {large} allocations, more than {PER_EPISODE_BUDGET} per episode"
        );
    }
}
