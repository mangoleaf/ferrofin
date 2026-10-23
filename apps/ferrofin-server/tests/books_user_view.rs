//! A books `UserView` browses as `UserViewBuilder.GetBooks` does: every Book
//! and AudioBook under its library, whatever the request asked for.
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
    let client = r#"MediaBrowser Client="books-view-test", Device="fixture", DeviceId="books-view-test", Version="1""#;
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

fn names(items: &Value) -> Vec<&str> {
    items["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Name"].as_str().unwrap())
        .collect()
}

/// `GetBooks` forces `Recursive`, the view's display parent and
/// `IncludeItemTypes = [Book, AudioBook]`: a plain browse of a books view lists
/// the library's books and audiobooks at any depth and nothing else, and a
/// narrower `includeItemTypes` is overridden as upstream does.
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one HTTP fixture wires a real server, a books library and its view"
)]
async fn a_books_view_lists_every_book_and_audiobook_of_its_library() {
    use ferrofin_db::{entities::base_items::BaseItemEntity, store::guid_to_db};
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_traits::persistence::ItemPersistenceService as _;
    use uuid::Uuid;
    let temp = tempfile::tempdir().unwrap();
    let config = Config {
        admin_user: "admin".to_owned(),
        admin_password: "books-pw".to_owned(),
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
        Some(json!({"Username":"admin","Pw":"books-pw"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = auth["AccessToken"].as_str().unwrap();

    let media = temp.path().join("books");
    std::fs::create_dir_all(&media).unwrap();
    let (status, body) = call(
        &router,
        Some(token),
        "POST",
        "/Library/VirtualFolders?name=Books&collectionType=books&refreshLibrary=false",
        Some(json!({"LibraryOptions":{"PathInfos":[{"Path":media.to_str().unwrap()}],"EnableRealtimeMonitor":false}})),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    let (_, folders) = call(&router, Some(token), "GET", "/Library/VirtualFolders", None).await;
    let library = folders
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["Name"] == "Books")
        .unwrap()["ItemId"]
        .as_str()
        .unwrap()
        .to_owned();
    let library_id = Uuid::parse_str(&library).unwrap();
    let stored_library = guid_to_db(library_id);
    let persistence = ferrofin_core::FerrofinItemPersistenceService::new(db.clone());

    let author = Uuid::new_v4();
    let rows = [
        (author, BaseItemKind::Folder, "Author folder", None),
        (Uuid::new_v4(), BaseItemKind::Book, "Alpha book", None),
        (
            Uuid::new_v4(),
            BaseItemKind::AudioBook,
            "Bravo audiobook",
            Some(author),
        ),
        (Uuid::new_v4(), BaseItemKind::Video, "Charlie video", None),
    ];
    for (id, kind, name, parent) in rows {
        let parent_id = parent.unwrap_or(library_id);
        wired
            .state
            .library
            .create_items(
                &[BaseItemEntity {
                    id: guid_to_db(id),
                    type_: kind.stored_type_name().unwrap().into(),
                    name: Some(name.into()),
                    sort_name: Some(name.to_lowercase()),
                    clean_name: Some(name.to_lowercase()),
                    parent_id: Some(guid_to_db(parent_id)),
                    top_parent_id: Some(stored_library.clone()),
                    is_folder: kind == BaseItemKind::Folder,
                    ..Default::default()
                }],
                None,
            )
            .await
            .unwrap();
        let ancestors: Vec<Uuid> = parent.into_iter().chain([library_id]).collect();
        persistence.set_ancestors(id, &ancestors).await.unwrap();
    }
    let view = Uuid::new_v4();
    wired
        .state
        .library
        .create_items(
            &[BaseItemEntity {
                id: guid_to_db(view),
                type_: BaseItemKind::UserView.stored_type_name().unwrap().into(),
                name: Some("Books".into()),
                is_folder: true,
                data: Some(json!({"ViewType":"books","DisplayParentId":library}).to_string()),
                ..Default::default()
            }],
            None,
        )
        .await
        .unwrap();

    let get = |path: String| {
        let router = router.clone();
        let token = token.to_owned();
        async move {
            let (status, value) = call(&router, Some(&token), "GET", &path, None).await;
            assert_eq!(status, StatusCode::OK, "{path}: {value}");
            value
        }
    };
    let plain = get(format!("/Items?parentId={view}&sortBy=SortName")).await;
    assert_eq!(
        names(&plain),
        ["Alpha book", "Bravo audiobook"],
        "every Book and AudioBook of the library, at any depth: {plain}"
    );
    let narrower = get(format!(
        "/Items?parentId={view}&includeItemTypes=Book&sortBy=SortName"
    ))
    .await;
    assert_eq!(
        names(&narrower),
        ["Alpha book", "Bravo audiobook"],
        "GetBooks overrides the request's kinds: {narrower}"
    );
    let wider = get(format!(
        "/Items?parentId={view}&includeItemTypes=Video,Book&sortBy=SortName"
    ))
    .await;
    assert_eq!(
        names(&wider),
        ["Alpha book", "Bravo audiobook"],
        "the override replaces a wider request too, so the library's video stays out: {wider}"
    );
    assert_eq!(wider["TotalRecordCount"], 2, "{wider}");
}
