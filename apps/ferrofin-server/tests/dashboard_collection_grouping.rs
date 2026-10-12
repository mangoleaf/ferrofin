//! Dashboard collection grouping reaches the real configuration and Items routes.
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use ferrofin_db::Database;
use ferrofin_server::{config::Config, state::build_app_state};
use serde_json::{Value, json};
use tower::ServiceExt as _;

async fn call(
    router: &axum::Router,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let auth = token.map_or_else(|| r#"MediaBrowser Client="folder-view-test", Device="fixture", DeviceId="folder-view-test", Version="1""#.to_owned(), |token| format!(r#"MediaBrowser Client="folder-view-test", Device="fixture", DeviceId="folder-view-test", Version="1", Token="{token}""#));
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
async fn get(router: &axum::Router, token: &str, path: &str) -> Value {
    let (status, value) = call(router, Some(token), "GET", path, None).await;
    assert_eq!(status, StatusCode::OK, "{path}: {value}");
    value
}
async fn post(router: &axum::Router, token: &str, path: &str, value: Value) {
    let (status, body) = call(router, Some(token), "POST", path, Some(value)).await;
    assert!(status.is_success(), "{path}: {status} {body}");
}
fn names(items: &Value) -> Vec<&str> {
    items["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Name"].as_str().unwrap())
        .collect()
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one HTTP fixture exercises the live collection-toggle and request-override matrix"
)]
async fn live_collection_flags_and_request_overrides_reach_real_folder_queries() {
    use ferrofin_db::{
        entities::base_items::BaseItemEntity, enums::ItemValueType, store::guid_to_db,
    };
    use ferrofin_traits::persistence::ItemPersistenceService as _;
    use uuid::Uuid;
    let temp = tempfile::tempdir().unwrap();
    let config = Config {
        admin_user: "admin".to_owned(),
        admin_password: "grouping-pw".to_owned(),
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
        Some(json!({"Username":"admin","Pw":"grouping-pw"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = auth["AccessToken"].as_str().unwrap();
    let media = temp.path().join("mixed");
    std::fs::create_dir_all(&media).unwrap();
    post(&router, token, "/Library/VirtualFolders?name=Mixed&refreshLibrary=false", json!({"LibraryOptions":{"PathInfos":[{"Path":media.to_str().unwrap()}],"EnableRealtimeMonitor":false,"EnableChapterImageExtraction":false,"EnableTrickplayImageExtraction":false,"EnableAutomaticSeriesGrouping":false,"TypeOptions":[]}})).await;
    let folders = get(&router, token, "/Library/VirtualFolders").await;
    let parent = folders
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["Name"] == "Mixed")
        .unwrap()["ItemId"]
        .as_str()
        .unwrap();
    let parent_id = Uuid::parse_str(parent).unwrap();
    let stored_parent = guid_to_db(parent_id);
    let persistence = ferrofin_core::FerrofinItemPersistenceService::new(db.clone());
    let mut view_ids = Vec::new();
    for view_type in [
        json!("movies"),
        json!(111),
        json!("tvshows"),
        json!("tvshowseries"),
    ] {
        let id = Uuid::new_v4();
        wired
            .state
            .library
            .create_items(
                &[BaseItemEntity {
                    id: guid_to_db(id),
                    type_: "MediaBrowser.Controller.Entities.UserView".into(),
                    name: Some("fixture view".into()),
                    is_folder: true,
                    data: Some(json!({"ViewType":view_type,"DisplayParentId":parent}).to_string()),
                    ..Default::default()
                }],
                None,
            )
            .await
            .unwrap();
        view_ids.push(id);
    }
    let mut ids = Vec::new();
    for (kind, name) in [
        (
            "MediaBrowser.Controller.Entities.Movies.Movie",
            "Alpha movie",
        ),
        ("MediaBrowser.Controller.Entities.TV.Series", "Bravo series"),
        ("MediaBrowser.Controller.Entities.Video", "Charlie video"),
    ] {
        let id = Uuid::new_v4();
        ids.push(id);
        wired
            .state
            .library
            .create_items(
                &[BaseItemEntity {
                    id: guid_to_db(id),
                    type_: kind.into(),
                    name: Some(name.into()),
                    sort_name: Some(name.to_lowercase()),
                    clean_name: Some(name.to_lowercase()),
                    media_type: Some("Video".into()),
                    parent_id: Some(stored_parent.clone()),
                    top_parent_id: Some(stored_parent.clone()),
                    is_folder: kind.ends_with("Series"),
                    ..Default::default()
                }],
                None,
            )
            .await
            .unwrap();
        persistence.set_ancestors(id, &[parent_id]).await.unwrap();
    }
    let episode_id = Uuid::new_v4();
    let series_key = wired
        .state
        .library
        .get_item_by_id(ids[1])
        .await
        .unwrap()
        .unwrap()
        .presentation_unique_key
        .expect("the stored Series has its source presentation key");
    wired
        .state
        .library
        .create_items(
            &[BaseItemEntity {
                id: guid_to_db(episode_id),
                type_: "MediaBrowser.Controller.Entities.TV.Episode".into(),
                name: Some("Unplayed episode".into()),
                parent_id: Some(guid_to_db(ids[1])),
                series_id: Some(guid_to_db(ids[1])),
                series_presentation_unique_key: Some(series_key),
                top_parent_id: Some(stored_parent.clone()),
                media_type: Some("Video".into()),
                ..Default::default()
            }],
            None,
        )
        .await
        .unwrap();
    persistence
        .set_ancestors(episode_id, &[ids[1], parent_id])
        .await
        .unwrap();
    let mut collection_ids = Vec::new();
    for (id, name) in ids.iter().zip([
        "Zulu movie collection",
        "Alpha show collection",
        "Video collection",
    ]) {
        let (status, collection) = call(
            &router,
            Some(token),
            "POST",
            &format!("/Collections?name={}&ids={id}", name.replace(' ', "%20")),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{collection}");
        collection_ids.push(collection["Id"].as_str().unwrap().to_owned());
    }
    let (status, _) = call(
        &router,
        Some(token),
        "POST",
        &format!("/Collections?name=Episode%20collection&ids={episode_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let base = format!(
        "/Items?parentId={parent}&recursive=true&includeItemTypes=Movie,Series,Video&sortBy=SortName"
    );
    let mut settings = get(&router, token, "/System/Configuration").await;
    for (movies, shows, expected) in [
        (
            false,
            false,
            vec!["Alpha movie", "Bravo series", "Charlie video"],
        ),
        (
            true,
            false,
            vec!["Bravo series", "Charlie video", "Zulu movie collection"],
        ),
        (
            false,
            true,
            vec!["Alpha movie", "Alpha show collection", "Charlie video"],
        ),
        (
            true,
            true,
            vec![
                "Alpha show collection",
                "Video collection",
                "Zulu movie collection",
            ],
        ),
    ] {
        settings["EnableGroupingMoviesIntoCollections"] = json!(movies);
        settings["EnableGroupingShowsIntoCollections"] = json!(shows);
        post(&router, token, "/System/Configuration", settings.clone()).await;
        let result = get(&router, token, &base).await;
        assert_eq!(names(&result), expected, "live {movies}/{shows}");
        assert_eq!(result["TotalRecordCount"], 3);
        let movie_view = get(
            &router,
            token,
            &format!(
                "/Items?parentId={}&recursive=true&sortBy=SortName",
                view_ids[0]
            ),
        )
        .await;
        assert_eq!(
            names(&movie_view),
            if movies {
                vec!["Zulu movie collection"]
            } else {
                vec!["Alpha movie"]
            },
            "recursive movie view uses its default kind and Folder policy"
        );
        let direct_movie_view = get(
            &router,
            token,
            &format!("/Items?parentId={}&sortBy=SortName", view_ids[1]),
        )
        .await;
        assert_eq!(
            names(&direct_movie_view),
            ["Alpha movie"],
            "direct moviemovies builder forces its kind/recursion without automatic global grouping"
        );
        let direct_override = get(
            &router,
            token,
            &format!(
                "/Items?parentId={}&collapseBoxSetItems=true&sortBy=SortName",
                view_ids[1]
            ),
        )
        .await;
        assert_eq!(
            names(&direct_override),
            ["Zulu movie collection"],
            "direct view still forwards the explicit override to SQL"
        );
        let tv_view = get(
            &router,
            token,
            &format!(
                "/Items?parentId={}&recursive=true&sortBy=SortName",
                view_ids[2]
            ),
        )
        .await;
        let tv_expected = if movies && shows {
            vec!["Alpha show collection", "Episode collection"]
        } else if shows {
            vec!["Alpha show collection", "Unplayed episode"]
        } else {
            vec!["Bravo series", "Unplayed episode"]
        };
        assert_eq!(
            names(&tv_view),
            tv_expected,
            "TV base view defaults to Series/Season/Episode and uses only its applicable global flag"
        );
        let series_children = get(
            &router,
            token,
            &format!("/Items?parentId={}&recursive=true&sortBy=SortName", ids[1]),
        )
        .await;
        assert_eq!(
            names(&series_children),
            ["Unplayed episode"],
            "Series overrides Folder's automatic policy even without includeItemTypes"
        );
        let plain_query = base.replace("recursive=true", "recursive=false");
        let plain = get(&router, token, &plain_query).await;
        let plain_expected = if movies && shows {
            vec![
                "Alpha show collection",
                "Charlie video",
                "Zulu movie collection",
            ]
        } else {
            expected.clone()
        };
        assert_eq!(names(&plain), plain_expected, "plain live {movies}/{shows}");
        assert_eq!(plain["TotalRecordCount"], 3);
        let plain_page = get(
            &router,
            token,
            &format!("{plain_query}&startIndex=1&limit=1"),
        )
        .await;
        assert_eq!(names(&plain_page), [plain_expected[1]]);
        assert_eq!(plain_page["TotalRecordCount"], 3);
        let unplayed = get(&router, token, &format!("{plain_query}&filters=IsUnplayed")).await;
        assert_eq!(
            names(&unplayed),
            ["Alpha movie", "Bravo series", "Charlie video"],
            "plain unplayed disables automatic grouping"
        );
    }
    let page = get(&router, token, &format!("{base}&startIndex=1&limit=1")).await;
    assert_eq!(names(&page), ["Video collection"]);
    assert_eq!(page["TotalRecordCount"], 3);
    let beyond = get(&router, token, &format!("{base}&startIndex=99&limit=1")).await;
    assert!(names(&beyond).is_empty());
    assert_eq!(beyond["TotalRecordCount"], 3);
    assert_eq!(
        names(&get(&router, token, &format!("{base}&nameStartsWith=A")).await),
        ["Alpha show collection"]
    );
    assert_eq!(
        names(&get(&router, token, &format!("{base}&collapseBoxSetItems=false")).await),
        ["Alpha movie", "Bravo series", "Charlie video"]
    );
    assert_eq!(
        names(&get(&router, token, &format!("{base}&filters=IsUnplayed")).await),
        ["Alpha movie", "Bravo series", "Charlie video"],
        "explicit false user-data criteria disable automatic grouping"
    );
    assert_eq!(
        names(
            &get(
                &router,
                token,
                &format!("{base}&ids={}&collapseBoxSetItems=true", ids[0])
            )
            .await
        ),
        ["Alpha movie"],
        "the controller always disables grouping for explicit IDs"
    );
    assert_eq!(
        names(
            &get(
                &router,
                token,
                &format!("{base}&searchTerm=Alpha&collapseBoxSetItems=true")
            )
            .await
        ),
        ["Alpha movie"],
        "search returns the matching title"
    );
    let plain = base.replace("recursive=true", "recursive=false");
    assert_eq!(
        names(&get(&router, token, &plain).await),
        [
            "Alpha show collection",
            "Charlie video",
            "Zulu movie collection"
        ]
    );
    assert_eq!(
        names(
            &get(
                &router,
                token,
                &format!("{plain}&nameContains=does-not-exist&indexNumber=99&parentIndexNumber=42")
            )
            .await
        ),
        [
            "Alpha show collection",
            "Charlie video",
            "Zulu movie collection"
        ],
        "plain Folder filtering uses the source whitelist and accepts missing parent indices"
    );
    let plain_page = get(&router, token, &format!("{plain}&startIndex=1&limit=1")).await;
    assert_eq!(names(&plain_page), ["Charlie video"]);
    assert_eq!(plain_page["TotalRecordCount"], 3);
    assert!(names(&get(&router, token, &format!("{plain}&nameStartsWith=A")).await).is_empty());
    let collection_browse = get(
        &router,
        token,
        &format!(
            "/Items?parentId={}&recursive=false&includeItemTypes=Movie&sortBy=SortName",
            collection_ids[0]
        ),
    )
    .await;
    assert_eq!(
        names(&collection_browse),
        ["Alpha movie"],
        "browsing a collection must expose members without collapsing back into itself"
    );
    let recursive_collection_browse = get(
        &router,
        token,
        &format!(
            "/Items?parentId={}&recursive=true&includeItemTypes=Movie&sortBy=SortName",
            collection_ids[0]
        ),
    )
    .await;
    assert_eq!(names(&recursive_collection_browse), ["Alpha movie"]);
    assert_eq!(recursive_collection_browse["TotalRecordCount"], 1);
    settings["EnableGroupingMoviesIntoCollections"] = json!(false);
    settings["EnableGroupingShowsIntoCollections"] = json!(false);
    post(&router, token, "/System/Configuration", settings).await;
    assert_eq!(
        names(&get(&router, token, &format!("{base}&collapseBoxSetItems=true")).await),
        [
            "Alpha show collection",
            "Video collection",
            "Zulu movie collection"
        ],
        "recursive request override remains supported"
    );
    assert_eq!(
        names(&get(&router, token, &format!("{plain}&collapseBoxSetItems=true")).await),
        ["Alpha movie", "Bravo series", "Charlie video"],
        "plain folder path retains the pinned disabled-flags behavior"
    );
    // Folder.GetItems bypasses every domain/view builder for explicit IDs.
    // The controller checks the actual parent, then clears query.Parent.
    // Requested kinds, visibility, paging and container redirects still apply.
    let user = auth["User"]["Id"].as_str().unwrap();
    let mut policy = get(&router, token, &format!("/Users/{user}")).await["Policy"].clone();
    policy["BlockedTags"] = json!(["hidden-selection"]);
    post(&router, token, &format!("/Users/{user}/Policy"), policy).await;
    let hidden = Uuid::new_v4();
    let hidden_parent = Uuid::new_v4();
    wired
        .state
        .library
        .create_items(
            &[
                BaseItemEntity {
                    id: guid_to_db(hidden),
                    type_: "MediaBrowser.Controller.Entities.Video".into(),
                    name: Some("Hidden requested video".into()),
                    sort_name: Some("hidden requested video".into()),
                    tags: Some("hidden-selection".into()),
                    media_type: Some("Video".into()),
                    parent_id: Some(stored_parent.clone()),
                    top_parent_id: Some(stored_parent.clone()),
                    ..Default::default()
                },
                BaseItemEntity {
                    id: guid_to_db(hidden_parent),
                    type_: "MediaBrowser.Controller.Entities.UserView".into(),
                    name: Some("Hidden view".into()),
                    is_folder: true,
                    tags: Some("hidden-selection".into()),
                    data: Some(json!({"ViewType":"movies","DisplayParentId":parent}).to_string()),
                    ..Default::default()
                },
            ],
            None,
        )
        .await
        .unwrap();
    let outside_media = temp.path().join("outside");
    for item in [hidden, hidden_parent] {
        persistence
            .save_item_values(
                item,
                &[(i32::from(ItemValueType::Tags), "hidden-selection".into())],
            )
            .await
            .unwrap();
    }
    persistence
        .set_ancestors(hidden, &[parent_id])
        .await
        .unwrap();
    std::fs::create_dir_all(&outside_media).unwrap();
    post(&router, token, "/Library/VirtualFolders?name=Outside&refreshLibrary=false", json!({"LibraryOptions":{"PathInfos":[{"Path":outside_media.to_str().unwrap()}],"EnableRealtimeMonitor":false,"EnableChapterImageExtraction":false,"EnableTrickplayImageExtraction":false,"TypeOptions":[]}})).await;
    let outside_folders = get(&router, token, "/Library/VirtualFolders").await;
    let outside_parent = outside_folders
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["Name"] == "Outside")
        .unwrap()["ItemId"]
        .as_str()
        .unwrap();
    let outside = Uuid::new_v4();
    let outside_parent_id = Uuid::parse_str(outside_parent).unwrap();
    let stored_outside_parent = guid_to_db(outside_parent_id);
    wired
        .state
        .library
        .create_items(
            &[BaseItemEntity {
                id: guid_to_db(outside),
                type_: "MediaBrowser.Controller.Entities.Video".into(),
                name: Some("Outside requested video".into()),
                sort_name: Some("outside requested video".into()),
                clean_name: Some("outside requested video".into()),
                media_type: Some("Video".into()),
                parent_id: Some(stored_outside_parent.clone()),
                top_parent_id: Some(stored_outside_parent),
                ..Default::default()
            }],
            None,
        )
        .await
        .unwrap();
    persistence
        .set_ancestors(outside, &[outside_parent_id])
        .await
        .unwrap();
    let requested = format!("{},{},{hidden},{outside}", ids[2], ids[1]);
    let mut settings = get(&router, token, "/System/Configuration").await;
    for enabled in [false, true] {
        settings["EnableGroupingMoviesIntoCollections"] = json!(enabled);
        settings["EnableGroupingShowsIntoCollections"] = json!(enabled);
        post(&router, token, "/System/Configuration", settings.clone()).await;
        for view in &view_ids {
            let result = get(&router, token, &format!("/Items?parentId={view}&recursive=true&ids={requested}&collapseBoxSetItems=true&sortBy=SortName")).await;
            assert_eq!(
                names(&result),
                ["Bravo series", "Charlie video", "Outside requested video"],
                "explicit IDs bypass view defaults and parent scope, while honoring blocked tags (enabled={enabled}, view={view})"
            );
            assert_eq!(result["TotalRecordCount"], 3);
            let page = get(&router, token, &format!("/Items?parentId={view}&recursive=true&ids={requested}&collapseBoxSetItems=true&sortBy=SortName&startIndex=1&limit=1")).await;
            assert_eq!(names(&page), ["Charlie video"]);
            assert_eq!(page["TotalRecordCount"], 3);
            let videos = get(&router, token, &format!("/Items?parentId={view}&recursive=true&ids={requested}&includeItemTypes=Video&collapseBoxSetItems=true&sortBy=SortName")).await;
            assert_eq!(names(&videos), ["Charlie video", "Outside requested video"]);
            assert_eq!(videos["TotalRecordCount"], 2);
            let nonrecursive = get(
                &router,
                token,
                &format!(
                    "/Items?parentId={view}&recursive=false&ids={},{},{}&collapseBoxSetItems=true&sortBy=SortName",
                    ids[0], ids[1], ids[2]
                ),
            )
            .await;
            assert_eq!(
                names(&nonrecursive),
                ["Alpha movie", "Bravo series", "Charlie video"],
                "nonrecursive direct-ID dispatch retains requested kinds without a view child scope"
            );
            assert_eq!(nonrecursive["TotalRecordCount"], 3);
        }
    }
    let (status, outside_collection) = call(
        &router,
        Some(token),
        "POST",
        &format!("/Collections?name=Outside%20collection&ids={outside}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let outside_collection_id = outside_collection["Id"].as_str().unwrap();
    let containers = get(&router, token, &format!("/Items?parentId={parent}&recursive=false&ids={},{}&includeItemTypes=BoxSet&sortBy=SortName", collection_ids[0], outside_collection_id)).await;
    assert_eq!(names(&containers), ["Zulu movie collection"]);
    assert_eq!(
        containers["TotalRecordCount"], 1,
        "clearing Parent retains the preceding linked-child ancestor constraint"
    );
    let (status, _) = call(
        &router,
        Some(token),
        "GET",
        &format!(
            "/Items?parentId={hidden_parent}&recursive=true&ids={}&collapseBoxSetItems=true",
            ids[2]
        ),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "direct-ID dispatch retains the actual explicit-parent access gate"
    );
    for task in wired.background {
        task.abort();
    }
}
