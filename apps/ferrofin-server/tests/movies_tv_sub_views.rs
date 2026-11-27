//! A movies or TV `UserView` browsed without `Recursive` lists its sub-views,
//! as `UserViewBuilder.GetMovieFolders` and `GetTvView` do, and each sub-view
//! lists what its builder selects; a recursive browse, which jellyfin-web's
//! movies and TV pages send, still lists the library's items.
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use chrono::{TimeZone as _, Utc};
use ferrofin_db::Database;
use ferrofin_db::{entities::base_items::BaseItemEntity, store::guid_to_db};
use ferrofin_model::data::BaseItemKind;
use ferrofin_server::{config::Config, state::build_app_state};
use ferrofin_traits::persistence::ItemPersistenceService as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;

async fn call(
    router: &axum::Router,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let client = r#"MediaBrowser Client="sub-views-test", Device="fixture", DeviceId="sub-views-test", Version="1""#;
    let auth = token.map_or_else(
        || client.to_owned(),
        |token| format!(r#"{client}, Token="{token}""#),
    );
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, auth);
    let request = match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        },
    )
}

fn field<'a>(items: &'a Value, name: &str) -> Vec<&'a str> {
    items["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item[name].as_str().unwrap_or_default())
        .collect()
}

fn names(items: &Value) -> Vec<&str> {
    field(items, "Name")
}

/// A server over a fresh database, signed in as its administrator.
struct Fixture {
    temp: tempfile::TempDir,
    db: Database,
    router: axum::Router,
    token: String,
    user: String,
}

impl Fixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            admin_user: "admin".to_owned(),
            admin_password: "sub-views-pw".to_owned(),
            ..Config::test_stub(temp.path())
        };
        std::fs::create_dir_all(&config.data_dir).unwrap();
        std::fs::create_dir_all(temp.path().join("config")).unwrap();
        let db = Database::connect(&config.database_url()).await.unwrap();
        db.run_migrations().await.unwrap();
        let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
            encoder_app_path_display: None,
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
            chromaprint_muxer: false,
        };
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        let wired = build_app_state(&db, &config, &ffmpeg, None, shutdown_tx)
            .await
            .unwrap();
        ferrofin_server::seed::seed_default_admin(wired.state.users.as_ref(), &config)
            .await
            .unwrap();
        let router = ferrofin_api::create_router(wired.state.clone());
        let (status, auth) = call(
            &router,
            None,
            "POST",
            "/Users/AuthenticateByName",
            Some(json!({"Username":"admin","Pw":"sub-views-pw"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        Self {
            token: auth["AccessToken"].as_str().unwrap().to_owned(),
            user: auth["User"]["Id"].as_str().unwrap().to_owned(),
            temp,
            db,
            router,
        }
    }

    async fn send(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, value) = call(&self.router, Some(&self.token), method, path, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {value}");
        value
    }

    async fn get(&self, path: &str) -> Value {
        self.send("GET", path, None).await
    }

    /// The items of `/Items?userId=…&parentId={parent}&{query}`.
    async fn browse(&self, parent: &str, query: &str) -> Value {
        self.get(&format!(
            "/Items?userId={}&parentId={parent}&{query}",
            self.user
        ))
        .await
    }

    /// A library of `collection_type` with no scan, and its id.
    async fn library(&self, name: &str, collection_type: &str) -> Uuid {
        let media = self.temp.path().join(name);
        std::fs::create_dir_all(&media).unwrap();
        self.send(
            "POST",
            &format!(
                "/Library/VirtualFolders?name={name}&collectionType={collection_type}&refreshLibrary=false"
            ),
            Some(json!({"LibraryOptions":{"PathInfos":[{"Path":media.to_str().unwrap()}],"EnableRealtimeMonitor":false}})),
        )
        .await;
        let folders = self.get("/Library/VirtualFolders").await;
        let id = folders
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["Name"] == name)
            .unwrap()["ItemId"]
            .as_str()
            .unwrap()
            .to_owned();
        Uuid::parse_str(&id).unwrap()
    }

    /// Stores `row` under `ancestors` (nearest first, the library last).
    async fn save(&self, row: BaseItemEntity, ancestors: &[Uuid]) {
        let id = Uuid::parse_str(&row.id).unwrap();
        let persistence = ferrofin_core::FerrofinItemPersistenceService::new(self.db.clone());
        persistence.save_items(&[row]).await.unwrap();
        persistence.set_ancestors(id, ancestors).await.unwrap();
    }
}

fn item(id: Uuid, kind: BaseItemKind, name: &str, parent: Uuid, library: Uuid) -> BaseItemEntity {
    BaseItemEntity {
        id: guid_to_db(id),
        type_: kind.stored_type_name().unwrap().into(),
        name: Some(name.into()),
        sort_name: Some(name.to_lowercase()),
        clean_name: Some(name.to_lowercase()),
        parent_id: Some(guid_to_db(parent)),
        top_parent_id: Some(guid_to_db(library)),
        is_folder: matches!(kind, BaseItemKind::Series | BaseItemKind::Season),
        media_type: (!matches!(kind, BaseItemKind::Series | BaseItemKind::Season))
            .then(|| "Video".to_owned()),
        ..BaseItemEntity::default()
    }
}

/// The `UserView` `/UserViews?presetViews=…` lists for `collection_type`.
async fn preset_view(f: &Fixture, collection_type: &str) -> String {
    let views = f
        .get(&format!(
            "/UserViews?userId={}&presetViews=movies&presetViews=tvshows",
            f.user
        ))
        .await;
    let view = views["Items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|view| view["CollectionType"] == collection_type)
        .unwrap_or_else(|| panic!("a {collection_type} view: {views}"));
    assert_eq!(view["Type"], "UserView", "{view}");
    view["Id"].as_str().unwrap().to_owned()
}

/// The movies view: its six sub-views, then what each lists, measured
/// against a stock Jellyfin 12.4 over the same shape of library.
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one HTTP fixture walks a movies view and every sub-view it lists"
)]
async fn a_movies_view_lists_its_sub_views_and_each_lists_its_builders_items() {
    let f = Fixture::new().await;
    let library = f.library("Movies", "movies").await;
    let (heat, clue, alien) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    for (id, name, genres, day) in [
        (heat, "Heat", "Crime|Drama", 1),
        (clue, "Clue", "Comedy", 2),
        (alien, "Alien", "Horror", 3),
    ] {
        f.save(
            BaseItemEntity {
                genres: Some(genres.into()),
                date_created: Utc.with_ymd_and_hms(2026, 1, day, 0, 0, 0).single(),
                ..item(id, BaseItemKind::Movie, name, library, library)
            },
            &[library],
        )
        .await;
    }
    f.send(
        "POST",
        &format!("/UserFavoriteItems/{}?userId={}", heat.simple(), f.user),
        None,
    )
    .await;
    f.send(
        "POST",
        &format!("/UserItems/{}/UserData?userId={}", clue.simple(), f.user),
        Some(json!({"PlaybackPositionTicks": 20_000_000})),
    )
    .await;
    f.send(
        "POST",
        &format!(
            "/Collections?name=Crime%20Set&ids={},{}",
            heat.simple(),
            alien.simple()
        ),
        None,
    )
    .await;
    let view = preset_view(&f, "movies").await;

    let listed = f.browse(&view, "fields=SortName").await;
    assert_eq!(
        names(&listed),
        [
            "Continue Watching",
            "Latest",
            "Movies",
            "Collections",
            "Favorites",
            "Genres"
        ],
        "{listed}"
    );
    assert_eq!(
        field(&listed, "CollectionType"),
        [
            "movieresume",
            "movielatest",
            "moviemovies",
            "moviecollection",
            "moviefavorites",
            "moviegenres"
        ]
    );
    assert_eq!(
        field(&listed, "SortName"),
        [
            "0000000000",
            "0000000001",
            "0000000002",
            "0000000003",
            "0000000004",
            "0000000005"
        ]
    );
    assert!(
        field(&listed, "Type")
            .iter()
            .all(|kind| *kind == "UserView")
    );
    assert_eq!(listed["TotalRecordCount"], 6);
    let again = f.browse(&view, "").await;
    assert_eq!(field(&again, "Id"), field(&listed, "Id"), "the same views");
    // `UserView.GetChildCount`: what each sub-view's own browse lists.
    let counted = f.browse(&view, "fields=ChildCount").await;
    let counts: Vec<i64> = counted["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["ChildCount"].as_i64().unwrap())
        .collect();
    assert_eq!(counts, [1, 3, 3, 1, 1, 4], "{counted}");
    let sub_view = |name: &str| {
        let index = names(&listed).iter().position(|n| *n == name).unwrap();
        field(&listed, "Id")[index].to_owned()
    };

    // `GetResult` counts every view before it pages, whatever the request says.
    let paged = f
        .browse(&view, "startIndex=1&limit=2&enableTotalRecordCount=false")
        .await;
    assert_eq!(names(&paged), ["Latest", "Movies"]);
    assert_eq!(paged["TotalRecordCount"], 6);
    assert_eq!(paged["StartIndex"], 1);
    let reversed = f
        .browse(&view, "sortBy=SortName&sortOrder=Descending")
        .await;
    assert_eq!(names(&reversed)[0], "Genres");
    let typed = f.browse(&view, "includeItemTypes=Movie").await;
    assert_eq!(
        typed["TotalRecordCount"], 0,
        "Filter keeps only the asked kinds"
    );

    // jellyfin-web's movies page browses recursively: still the movies.
    let recursive = f
        .browse(
            &view,
            "recursive=true&includeItemTypes=Movie&sortBy=SortName",
        )
        .await;
    assert_eq!(names(&recursive), ["Alien", "Clue", "Heat"], "{recursive}");
    assert_eq!(recursive["TotalRecordCount"], 3);
    let recursive_untyped = f.browse(&view, "recursive=true&sortBy=SortName").await;
    assert_eq!(names(&recursive_untyped), ["Alien", "Clue", "Heat"]);

    let resume = f.browse(&sub_view("Continue Watching"), "").await;
    assert_eq!(names(&resume), ["Clue"], "{resume}");
    let latest = f.browse(&sub_view("Latest"), "").await;
    assert_eq!(names(&latest), ["Alien", "Clue", "Heat"], "newest first");
    // The 50-item cap replaces the request's limit, and the total is the
    // list's length (`ConvertToResult(GetItemList)`).
    let latest_paged = f.browse(&sub_view("Latest"), "startIndex=1&limit=1").await;
    assert_eq!(names(&latest_paged), ["Clue", "Heat"]);
    assert_eq!(latest_paged["TotalRecordCount"], 2);
    let movies = f.browse(&sub_view("Movies"), "sortBy=SortName").await;
    assert_eq!(names(&movies), ["Alien", "Clue", "Heat"]);
    let collections = f.browse(&sub_view("Collections"), "").await;
    assert_eq!(names(&collections), ["Crime Set"], "{collections}");
    let favorites = f.browse(&sub_view("Favorites"), "").await;
    assert_eq!(names(&favorites), ["Heat"]);

    // `GetUserViewWithName` keys the genre view by the library alone and
    // names it the library's id: one view, once per genre, as 12.4 lists it.
    let genres = f.browse(&sub_view("Genres"), "").await;
    let library_name = library.simple().to_string();
    assert_eq!(names(&genres), vec![library_name.as_str(); 4], "{genres}");
    assert_eq!(field(&genres, "CollectionType"), ["moviegenre"; 4]);
    let genre_ids = field(&genres, "Id");
    assert!(genre_ids.iter().all(|id| *id == genre_ids[0]));
    assert_eq!(genres["TotalRecordCount"], 4);
    // Its `GenreIds` is its own id, which no movie carries.
    let genre = f.browse(genre_ids[0], "").await;
    assert_eq!(genre["TotalRecordCount"], 0, "{genre}");
}

/// The TV view: its seven sub-views, and what the TV-only ones list. Next Up
/// is the user's next-up queue under the library, as `/Shows/NextUp` answers
/// it with `NextUpQuery`'s defaults.
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one HTTP fixture walks a TV view and the sub-views it lists"
)]
async fn a_tv_view_lists_its_sub_views_and_next_up_lists_the_queue() {
    let f = Fixture::new().await;
    let library = f.library("Shows", "tvshows").await;
    let (series, season) = (Uuid::new_v4(), Uuid::new_v4());
    f.save(
        BaseItemEntity {
            genres: Some("Drama".into()),
            ..item(series, BaseItemKind::Series, "Show One", library, library)
        },
        &[library],
    )
    .await;
    // The save derives the series' presentation key; its episodes carry it.
    let stored_series: (Option<String>,) =
        sqlx::query_as(r#"SELECT "PresentationUniqueKey" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(series))
            .fetch_one(f.db.pool())
            .await
            .unwrap();
    let series_key = stored_series.0.expect("the series' presentation key");
    f.save(
        BaseItemEntity {
            index_number: Some(1),
            series_id: Some(guid_to_db(series)),
            series_name: Some("Show One".into()),
            series_presentation_unique_key: Some(series_key.clone()),
            ..item(season, BaseItemKind::Season, "Season 1", series, library)
        },
        &[series, library],
    )
    .await;
    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
    for (id, name, number, day) in [(first, "Pilot", 1, 1), (second, "Second", 2, 2)] {
        f.save(
            BaseItemEntity {
                index_number: Some(number),
                parent_index_number: Some(1),
                series_id: Some(guid_to_db(series)),
                season_id: Some(guid_to_db(season)),
                series_name: Some("Show One".into()),
                series_presentation_unique_key: Some(series_key.clone()),
                date_created: Utc.with_ymd_and_hms(2026, 1, day, 0, 0, 0).single(),
                ..item(id, BaseItemKind::Episode, name, season, library)
            },
            &[season, series, library],
        )
        .await;
    }
    f.send(
        "POST",
        &format!("/UserPlayedItems/{}?userId={}", first.simple(), f.user),
        None,
    )
    .await;
    for favourite in [series, second] {
        f.send(
            "POST",
            &format!(
                "/UserFavoriteItems/{}?userId={}",
                favourite.simple(),
                f.user
            ),
            None,
        )
        .await;
    }
    let view = preset_view(&f, "tvshows").await;

    let listed = f.browse(&view, "").await;
    assert_eq!(
        names(&listed),
        [
            "Continue Watching",
            "Next Up",
            "Latest",
            "Shows",
            "Favorite Shows",
            "Favorite Episodes",
            "Genres"
        ],
        "{listed}"
    );
    assert_eq!(
        field(&listed, "CollectionType"),
        [
            "tvresume",
            "tvnextup",
            "tvlatest",
            "tvshowseries",
            "tvfavoriteseries",
            "tvfavoriteepisodes",
            "tvgenres"
        ]
    );
    let sub_view = |name: &str| {
        let index = names(&listed).iter().position(|n| *n == name).unwrap();
        field(&listed, "Id")[index].to_owned()
    };

    // jellyfin-web's TV page browses recursively: still the series.
    let recursive = f
        .browse(
            &view,
            "recursive=true&includeItemTypes=Series&sortBy=SortName",
        )
        .await;
    assert_eq!(names(&recursive), ["Show One"], "{recursive}");

    let next_up = f.browse(&sub_view("Next Up"), "").await;
    let queue = f
        .get(&format!(
            "/Shows/NextUp?userId={}&parentId={}&enableResumable=false",
            f.user,
            library.simple()
        ))
        .await;
    assert_eq!(names(&queue), ["Second"], "{queue}");
    assert_eq!(field(&next_up, "Id"), field(&queue, "Id"), "{next_up}");
    let next_up_paged = f.browse(&sub_view("Next Up"), "startIndex=1").await;
    assert_eq!(next_up_paged["TotalRecordCount"], 1);
    assert_eq!(next_up_paged["StartIndex"], 1);
    assert_eq!(names(&next_up_paged), Vec::<&str>::new());

    let latest = f.browse(&sub_view("Latest"), "").await;
    assert_eq!(names(&latest), ["Second", "Pilot"], "newest first");
    let shows = f.browse(&sub_view("Shows"), "").await;
    assert_eq!(names(&shows), ["Show One"]);
    let favorite_shows = f.browse(&sub_view("Favorite Shows"), "").await;
    assert_eq!(names(&favorite_shows), ["Show One"]);
    let favorite_episodes = f.browse(&sub_view("Favorite Episodes"), "").await;
    assert_eq!(names(&favorite_episodes), ["Second"]);
    let genres = f.browse(&sub_view("Genres"), "").await;
    assert_eq!(names(&genres), [library.simple().to_string().as_str()]);
    assert_eq!(field(&genres, "CollectionType"), ["tvgenre"]);
}
