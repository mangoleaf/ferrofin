//! Live EnableFolderView configuration reaches localized home views and browsing.
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
fn folders(views: &Value) -> Vec<&Value> {
    views["Items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["CollectionType"] == "folders")
        .collect()
}
fn names(items: &Value) -> Vec<&str> {
    items["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Name"].as_str().unwrap())
        .collect()
}

async fn setup() -> (
    tempfile::TempDir,
    ferrofin_server::state::WiredApp,
    axum::Router,
    Value,
) {
    let temp = tempfile::tempdir().unwrap();
    let config = Config {
        admin_user: "admin".to_owned(),
        admin_password: "folder-view-pw".to_owned(),
        ..Config::test_stub(temp.path())
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    let db = Database::connect(&config.database_url()).await.unwrap();
    db.run_migrations().await.unwrap();
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
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
        Some(json!({"Username":"admin","Pw":"folder-view-pw"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    (temp, wired, router, auth)
}

#[tokio::test]
async fn live_folder_view_flag_name_and_secure_root_browse() {
    let (temp, wired, router, auth) = setup().await;
    let token = auth["AccessToken"].as_str().unwrap();
    let admin = auth["User"]["Id"].as_str().unwrap();
    for name in ["Alpha", "Zulu"] {
        let media = temp.path().join(name);
        std::fs::create_dir_all(&media).unwrap();
        post(&router, token, &format!("/Library/VirtualFolders?name={name}&collectionType=movies&refreshLibrary=false"), json!({"LibraryOptions":{"PathInfos":[{"Path":media.to_str().unwrap()}],"EnableRealtimeMonitor":false,"EnableChapterImageExtraction":false,"EnableTrickplayImageExtraction":false,"EnableAutomaticSeriesGrouping":false,"TypeOptions":[]}})).await;
    }
    let view_path = format!("/UserViews?userId={admin}");
    let mut settings = get(&router, token, "/System/Configuration").await;
    settings["EnableFolderView"] = json!(false);
    post(&router, token, "/System/Configuration", settings.clone()).await;
    assert!(folders(&get(&router, token, &view_path).await).is_empty());
    settings["EnableFolderView"] = json!(true);
    post(&router, token, "/System/Configuration", settings.clone()).await;
    let views = get(&router, token, &view_path).await;
    assert_eq!(folders(&views).len(), 1);
    let folder = folders(&views)[0];
    assert_eq!(folder["Name"], "Folders");
    let id = folder["Id"].as_str().unwrap().to_owned();
    let parent = format!(
        "/Items?userId={admin}&parentId={id}&recursive=true&sortBy=SortName&sortOrder=Descending"
    );
    let page = get(&router, token, &format!("{parent}&startIndex=1&limit=1")).await;
    assert_eq!(page["TotalRecordCount"], 3);
    assert_eq!(names(&page), ["Playlists"]);
    assert_eq!(
        get(&router, token, &format!("{parent}&includeItemTypes=Movie")).await["TotalRecordCount"],
        0
    );
    assert_eq!(
        get(
            &router,
            token,
            &format!("{parent}&includeItemTypes=CollectionFolder&nameStartsWith=A")
        )
        .await["TotalRecordCount"],
        1
    );
    // The HTTP binder makes all three blank name boundaries absent. The
    // selected Folders consumer consequently keeps its full visible count.
    for parameter in ["nameStartsWith", "nameStartsWithOrGreater", "nameLessThan"] {
        for blank in ["", "%20", "%09%0A", "%C2%A0%E2%80%83"] {
            let rows = get(&router, token, &format!("{parent}&{parameter}={blank}")).await;
            assert_eq!(rows["TotalRecordCount"], 3, "{parameter}={blank}");
            assert_eq!(names(&rows), ["Zulu", "Playlists", "Alpha"]);
        }
    }
    let literal = get(&router, token, &format!("{parent}&nameStartsWith=%20A%20")).await;
    assert_eq!(literal["TotalRecordCount"], 0);
    verify_folders_api_filter_whitelist(&router, token, admin, &id).await;
    let beyond = get(&router, token, &format!("{parent}&startIndex=99&limit=1")).await;
    assert_eq!(beyond["TotalRecordCount"], 3);
    assert!(names(&beyond).is_empty());
    settings["UICulture"] = json!("fr");
    post(&router, token, "/System/Configuration", settings.clone()).await;
    let localized = get(&router, token, &view_path).await;
    assert_eq!(folders(&localized)[0]["Id"], id);
    assert_eq!(folders(&localized)[0]["Name"], "Dossiers");
    settings["EnableFolderView"] = json!(false);
    post(&router, token, "/System/Configuration", settings.clone()).await;
    assert!(folders(&get(&router, token, &view_path).await).is_empty());
    settings["EnableFolderView"] = json!(true);
    post(&router, token, "/System/Configuration", settings).await;
    assert_eq!(folders(&get(&router, token, &view_path).await)[0]["Id"], id);
    verify_restricted_root(&router, token, &id).await;
    for task in wired.background {
        task.abort();
    }
}

/// Real managers apply HTTP GetResult filters before count, adjacency and page.
async fn verify_folders_api_filter_whitelist(
    router: &axum::Router,
    token: &str,
    user: &str,
    parent: &str,
) {
    for route in [
        format!("/Items?userId={user}&parentId={parent}"),
        format!("/Users/{user}/Items?parentId={parent}"),
    ] {
        let base = format!("{route}&recursive=true&sortBy=SortName&sortOrder=Descending");
        for parameter in [
            "hasOverview",
            "hasImdbId",
            "hasTmdbId",
            "hasTvdbId",
            "hasParentalRating",
        ] {
            let positive = get(router, token, &format!("{base}&{parameter}=true&limit=1")).await;
            assert_eq!(positive["TotalRecordCount"], 0, "{parameter}");
            assert!(names(&positive).is_empty());
            let negative = get(router, token, &format!("{base}&{parameter}=false&limit=1")).await;
            assert_eq!(negative["TotalRecordCount"], 3, "{parameter}");
            assert_eq!(names(&negative), ["Zulu"]);
            let absent = get(router, token, &format!("{base}&{parameter}=%20&limit=1")).await;
            assert_eq!(absent["TotalRecordCount"], 3, "{parameter}");
            assert_eq!(names(&absent), ["Zulu"]);
        }
        for parameter in ["minPremiereDate", "maxPremiereDate"] {
            let rows = get(
                router,
                token,
                &format!("{base}&{parameter}=2026-01-01T00:00:00Z&limit=1"),
            )
            .await;
            assert_eq!(
                rows["TotalRecordCount"], 0,
                "undated root children: {parameter}"
            );
            assert!(names(&rows).is_empty());
            let absent = get(router, token, &format!("{base}&{parameter}=%20&limit=1")).await;
            assert_eq!(absent["TotalRecordCount"], 3);
            assert_eq!(names(&absent), ["Zulu"]);
        }
        verify_folders_adjacency(router, token, &base).await;
    }
}

/// Adjacency keeps sorted neighbours and the selected row, then pages that set.
async fn verify_folders_adjacency(router: &axum::Router, token: &str, base: &str) {
    let all = get(router, token, base).await;
    let alpha = all["Items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["Name"] == "Alpha")
        .unwrap()["Id"]
        .as_str()
        .unwrap();
    let adjacent = get(router, token, &format!("{base}&adjacentTo={alpha}")).await;
    assert_eq!(adjacent["TotalRecordCount"], 2);
    assert_eq!(names(&adjacent), ["Playlists", "Alpha"]);
    let page = get(
        router,
        token,
        &format!("{base}&adjacentTo={alpha}&startIndex=1&limit=1"),
    )
    .await;
    assert_eq!(page["TotalRecordCount"], 2);
    assert_eq!(names(&page), ["Alpha"]);
    let unknown = get(
        router,
        token,
        &format!("{base}&adjacentTo=11111111-1111-1111-1111-111111111111"),
    )
    .await;
    assert_eq!(unknown["TotalRecordCount"], 0);
    assert!(names(&unknown).is_empty());
    let nil = get(
        router,
        token,
        &format!("{base}&adjacentTo=00000000-0000-0000-0000-000000000000"),
    )
    .await;
    assert_eq!(nil["TotalRecordCount"], 3);
    // These are not source-exposed GetResult inputs; do not invent binding.
    let internal = get(
        router,
        token,
        &format!("{base}&minIndexNumber=42&isFolder=false"),
    )
    .await;
    assert_eq!(internal["TotalRecordCount"], 3);
}

async fn verify_restricted_root(router: &axum::Router, token: &str, id: &str) {
    let (status, created) = call(
        router,
        Some(token),
        "POST",
        "/Users/New",
        Some(json!({"Name":"restricted","Password":"restricted-pw"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_id = created["Id"].as_str().unwrap();
    let libraries = get(router, token, "/Library/VirtualFolders").await;
    let allowed = libraries
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["Name"] == "Alpha")
        .unwrap()["ItemId"]
        .as_str()
        .unwrap();
    let mut policy = created["Policy"].clone();
    policy["EnableAllFolders"] = json!(false);
    policy["EnabledFolders"] = json!([allowed]);
    post(router, token, &format!("/Users/{user_id}/Policy"), policy).await;
    let (status, restricted) = call(
        router,
        None,
        "POST",
        "/Users/AuthenticateByName",
        Some(json!({"Username":"restricted","Pw":"restricted-pw"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let restricted_token = restricted["AccessToken"].as_str().unwrap();
    let restricted_views = get(
        router,
        restricted_token,
        &format!("/UserViews?userId={user_id}"),
    )
    .await;
    assert_eq!(
        folders(&restricted_views)[0]["Id"],
        id,
        "the synthetic root view remains available to a restricted user"
    );
    let allowed_rows = get(
        router,
        restricted_token,
        &format!("/Items?userId={user_id}&parentId={id}&recursive=true&sortBy=SortName"),
    )
    .await;
    assert_eq!(
        names(&allowed_rows),
        ["Alpha", "Playlists"],
        "root child visibility still excludes the other library"
    );
}
