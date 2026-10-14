//! `GET /Items/{itemId}/Collections` (Jellyfin 12.1's `GetItemCollections`)
//! over the real router, database and collection manager.
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
    let auth = r#"MediaBrowser Client="item-collections-test", Device="fixture", DeviceId="item-collections-test", Version="1""#;
    let auth = token.map_or_else(
        || auth.to_owned(),
        |token| format!(r#"{auth}, Token="{token}""#),
    );
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, auth)
        .header(header::CONTENT_TYPE, "application/json");
    let request = match body {
        Some(body) => request.body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
}

async fn login(router: &axum::Router, user: &str, password: &str) -> Value {
    let (status, auth) = call(
        router,
        None,
        "POST",
        "/Users/AuthenticateByName",
        Some(json!({"Username": user, "Pw": password})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{auth}");
    auth
}

fn names(result: &Value) -> Vec<&str> {
    result["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Name"].as_str().unwrap())
        .collect()
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one HTTP fixture covers sorting, paging, visibility and every guard"
)]
async fn item_collections_lists_sorted_pages_and_guards() {
    use ferrofin_db::{entities::base_items::BaseItemEntity, store::guid_to_db};
    use ferrofin_traits::persistence::ItemPersistenceService as _;
    use uuid::Uuid;
    let temp = tempfile::tempdir().unwrap();
    let config = Config {
        admin_user: "admin".to_owned(),
        admin_password: "collections-pw".to_owned(),
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
    let admin = login(&router, "admin", "collections-pw").await;
    let token = admin["AccessToken"].as_str().unwrap();
    let admin_id = admin["User"]["Id"].as_str().unwrap();

    let media = temp.path().join("movies");
    std::fs::create_dir_all(&media).unwrap();
    let (status, body) = call(
        &router,
        Some(token),
        "POST",
        "/Library/VirtualFolders?name=Movies&refreshLibrary=false",
        Some(
            json!({"LibraryOptions":{"PathInfos":[{"Path":media.to_str().unwrap()}],"EnableRealtimeMonitor":false,"EnableChapterImageExtraction":false,"EnableTrickplayImageExtraction":false,"TypeOptions":[]}}),
        ),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    let (_, folders) = call(&router, Some(token), "GET", "/Library/VirtualFolders", None).await;
    let parent = folders
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["Name"] == "Movies")
        .unwrap()["ItemId"]
        .as_str()
        .unwrap()
        .to_owned();
    let parent_id = Uuid::parse_str(&parent).unwrap();
    let persistence = ferrofin_core::FerrofinItemPersistenceService::new(db.clone());
    let mut movies = Vec::new();
    for name in ["Collected", "Other", "Uncollected"] {
        let id = Uuid::new_v4();
        wired
            .state
            .library
            .create_items(
                &[BaseItemEntity {
                    id: guid_to_db(id),
                    type_: "MediaBrowser.Controller.Entities.Movies.Movie".into(),
                    name: Some(name.into()),
                    sort_name: Some(name.to_lowercase()),
                    media_type: Some("Video".into()),
                    parent_id: Some(guid_to_db(parent_id)),
                    top_parent_id: Some(guid_to_db(parent_id)),
                    ..Default::default()
                }],
                None,
            )
            .await
            .unwrap();
        persistence.set_ancestors(id, &[parent_id]).await.unwrap();
        movies.push(id);
    }
    let (collected, other, uncollected) = (movies[0], movies[1], movies[2]);
    // Created out of order; the route sorts by SortName, case-insensitively.
    let mut sets = Vec::new();
    for (name, ids) in [
        ("beta%20set", format!("{collected}")),
        ("Alpha%20set", format!("{collected},{other}")),
        ("Gamma%20set", format!("{other}")),
    ] {
        let (status, body) = call(
            &router,
            Some(token),
            "POST",
            &format!("/Collections?name={name}&ids={ids}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        sets.push(body["Id"].as_str().unwrap().to_owned());
    }

    let path = format!("/Items/{collected}/Collections");
    let (status, all) = call(&router, Some(token), "GET", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{all}");
    assert_eq!(names(&all), ["Alpha set", "beta set"]);
    assert_eq!(all["TotalRecordCount"], 2);
    assert_eq!(all["StartIndex"], 0);
    assert_eq!(all["Items"][0]["Type"], "BoxSet");

    let (_, page) = call(
        &router,
        Some(token),
        "GET",
        &format!("{path}?startIndex=1&limit=1"),
        None,
    )
    .await;
    assert_eq!(names(&page), ["beta set"]);
    assert_eq!(
        page["TotalRecordCount"], 2,
        "the total is the pre-paging count"
    );
    assert_eq!(page["StartIndex"], 1);

    let (_, none) = call(
        &router,
        Some(token),
        "GET",
        &format!("/Items/{uncollected}/Collections"),
        None,
    )
    .await;
    assert_eq!(names(&none), Vec::<&str>::new());
    assert_eq!(none["TotalRecordCount"], 0);

    let (status, _) = call(
        &router,
        Some(token),
        "GET",
        &format!("/Items/{}/Collections", Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(&router, None, "GET", &path, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // An API key carries no user, and none was named: C# `Unauthorized()`.
    let (status, _) = call(&router, Some(token), "POST", "/Auth/Keys?app=probe", None).await;
    assert!(status.is_success());
    let (_, keys) = call(&router, Some(token), "GET", "/Auth/Keys", None).await;
    let key = keys["Items"][0]["AccessToken"].as_str().unwrap().to_owned();
    let (status, _) = call(&router, Some(&key), "GET", &path, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, keyed) = call(
        &router,
        Some(&key),
        "GET",
        &format!("{path}?userId={admin_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "an API key may name a user");
    assert_eq!(names(&keyed), ["Alpha set", "beta set"]);

    // A non-admin with a parental limit loses the R-rated set, and may not ask
    // on another user's behalf.
    let (status, kid) = call(
        &router,
        Some(token),
        "POST",
        "/Users/New",
        Some(json!({"Name": "kid", "Password": "kid-pw"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{kid}");
    let kid_id = kid["Id"].as_str().unwrap();
    let mut policy = kid["Policy"].clone();
    policy["MaxParentalRating"] = json!(13);
    let (status, body) = call(
        &router,
        Some(token),
        "POST",
        &format!("/Users/{kid_id}/Policy"),
        Some(policy),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    sqlx::query(r#"UPDATE "BaseItems" SET "OfficialRating" = 'R' WHERE "Id" = ?"#)
        .bind(guid_to_db(Uuid::parse_str(&sets[1]).unwrap()))
        .execute(db.writer())
        .await
        .unwrap();
    let kid_token = login(&router, "kid", "kid-pw").await["AccessToken"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, limited) = call(&router, Some(&kid_token), "GET", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{limited}");
    assert_eq!(names(&limited), ["beta set"]);
    assert_eq!(limited["TotalRecordCount"], 1);
    let (status, _) = call(
        &router,
        Some(&kid_token),
        "GET",
        &format!("{path}?userId={admin_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
