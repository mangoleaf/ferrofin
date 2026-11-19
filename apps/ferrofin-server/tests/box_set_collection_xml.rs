//! A box set's `collection.xml` over the real router, database, collection
//! manager and provider manager: `BoxSetXmlSaver` writes it on every member
//! change and metadata edit, and `BoxSetXmlProvider` reads it back in the
//! box set's refresh — its members only for a box set outside the data
//! directory (v12.4 `BoxSetMetadataService.MergeData`).
use std::path::Path;
use std::time::Duration;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use ferrofin_db::Database;
use ferrofin_server::{config::Config, state::build_app_state};
use ferrofin_traits::providers::MetadataRefreshOptions;
use serde_json::{Value, json};
use tower::ServiceExt as _;

async fn call(
    router: &axum::Router,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let auth = r#"MediaBrowser Client="box-set-xml-test", Device="fixture", DeviceId="box-set-xml-test", Version="1""#;
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

/// The file once `ready` holds for it: the saves run in the request, the
/// queued refreshes after it.
async fn wait_for_file(path: &Path, ready: impl Fn(&str) -> bool) -> String {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(path)
            && ready(&text)
        {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "{} never got there: {:?}",
        path.display(),
        std::fs::read_to_string(path)
    );
}

/// Waits out the refreshes the membership changes queued, the one the worker
/// is running included: `get_refresh_queue` lists only those still waiting,
/// and a refresh still running when the test edits the file by hand would
/// write over the edit.
async fn settle(providers: &dyn ferrofin_traits::providers::ProviderManager) {
    // 30 s: a slow CI runner took about 18 s for the whole test.
    for _ in 0..600 {
        if !providers.is_processing_refresh_queue().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the refresh queue never went idle");
}

/// Waits until no library reports a scan in flight (`RefreshStatus`).
async fn wait_for_libraries_idle(router: &axum::Router, token: &str) {
    for _ in 0..600 {
        let (_, folders) = call(router, Some(token), "GET", "/Library/VirtualFolders", None).await;
        if folders
            .as_array()
            .is_some_and(|folders| folders.iter().all(|f| f["RefreshStatus"] == "Idle"))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the libraries never went idle");
}

/// The paths of the `<CollectionItem>`s, in order.
fn member_paths(xml: &str) -> Vec<&str> {
    xml.split("<Path>")
        .skip(1)
        .filter_map(|rest| rest.split_once("</Path>").map(|(path, _)| path))
        .collect()
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one HTTP fixture walks the file through members, edits and reads"
)]
async fn a_box_sets_collection_xml_follows_its_members_and_edits_and_is_read_back() {
    use ferrofin_db::{entities::base_items::BaseItemEntity, store::guid_to_db};
    use ferrofin_traits::persistence::ItemPersistenceService as _;
    use uuid::Uuid;
    let temp = tempfile::tempdir().unwrap();
    let config = Config {
        admin_user: "admin".to_owned(),
        admin_password: "box-set-xml-pw".to_owned(),
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
    let providers = wired.state.providers.clone();
    let router = ferrofin_api::create_router(wired.state.clone());
    let (status, admin) = call(
        &router,
        None,
        "POST",
        "/Users/AuthenticateByName",
        Some(json!({"Username": "admin", "Pw": "box-set-xml-pw"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{admin}");
    let token = admin["AccessToken"].as_str().unwrap();
    let user = admin["User"]["Id"].as_str().unwrap();

    // A movies library that did not opt into saving into its folders.
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
    let library = Uuid::parse_str(
        folders
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["Name"] == "Movies")
            .unwrap()["ItemId"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    for name in ["Alpha", "Beta"] {
        std::fs::write(media.join(format!("{name}.mkv")), b"").unwrap();
    }
    // The first collection makes the Collections library, whose creation
    // scans every library: the movies are stored then. A collection made
    // with no member is not saved (`CreateCollectionAsync` adds nothing).
    let (status, empty) = call(
        &router,
        Some(token),
        "POST",
        "/Collections?name=Empty",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    let mut movies = Vec::new();
    for _ in 0..600 {
        let (_, found) = call(
            &router,
            Some(token),
            "GET",
            &format!("/Items?recursive=true&includeItemTypes=Movie&sortBy=SortName&fields=Path&userId={user}"),
            None,
        )
        .await;
        movies = found["Items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        (
                            Uuid::parse_str(item["Id"].as_str().unwrap()).unwrap(),
                            item["Path"].as_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        if movies.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(movies.len(), 2, "the scan stored both movies");
    wait_for_libraries_idle(&router, token).await;
    let ((alpha, alpha_path), (beta, beta_path)) = (movies[0].clone(), movies[1].clone());
    let (_, empty) = call(
        &router,
        Some(token),
        "GET",
        &format!("/Users/{user}/Items/{}", empty["Id"].as_str().unwrap()),
        None,
    )
    .await;
    let empty_folder = empty["Path"].as_str().unwrap().to_owned();
    assert!(
        !Path::new(&empty_folder).join("collection.xml").exists(),
        "{empty_folder}"
    );

    // ── Written: made with a member (`AddToCollectionAsync`'s save).
    let (status, created) = call(
        &router,
        Some(token),
        "POST",
        &format!("/Collections?name=Saga&ids={alpha}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let set = created["Id"].as_str().unwrap().to_owned();
    let (_, item) = call(
        &router,
        Some(token),
        "GET",
        &format!("/Users/{user}/Items/{set}"),
        None,
    )
    .await;
    let folder = item["Path"].as_str().unwrap().to_owned();
    assert!(folder.ends_with("/collections/Saga [boxset]"), "{folder}");
    let xml = Path::new(&folder).join("collection.xml");
    let text = wait_for_file(&xml, |text| member_paths(text) == [alpha_path.as_str()]).await;
    assert!(text.starts_with(r#"<?xml version="1.0" encoding="utf-8" standalone="yes"?>"#));
    assert!(text.contains("<LocalTitle>Saga</LocalTitle>"), "{text}");
    assert!(text.contains("<LockData>false</LockData>"), "{text}");

    // Added, then removed: the file follows the links, in their order.
    let (status, body) = call(
        &router,
        Some(token),
        "POST",
        &format!("/Collections/{set}/Items?ids={beta}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    wait_for_file(&xml, |text| {
        member_paths(text) == [alpha_path.as_str(), beta_path.as_str()]
    })
    .await;
    let (status, body) = call(
        &router,
        Some(token),
        "DELETE",
        &format!("/Collections/{set}/Items?ids={alpha}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    wait_for_file(&xml, |text| member_paths(text) == [beta_path.as_str()]).await;

    // A metadata edit saves it too.
    let (status, body) = call(
        &router,
        Some(token),
        "POST",
        &format!("/Items/{set}"),
        Some(json!({"Name": "Saga", "Overview": "Edited overview"})),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    wait_for_file(&xml, |text| {
        text.contains("<Overview>Edited overview</Overview>")
    })
    .await;
    settle(providers.as_ref()).await;

    // ── Read: the file edited by hand is newer than the box set's save.
    let edited = std::fs::read_to_string(&xml)
        .unwrap()
        .replace(
            "<LocalTitle>Saga</LocalTitle>",
            "<LocalTitle>Saga Renamed</LocalTitle>\n  <LockedFields>Overview</LockedFields>",
        )
        .replace(
            "<CollectionItems>",
            &format!(
                "<CollectionItems>\n    <CollectionItem>\n      <Path>{alpha_path}</Path>\n    </CollectionItem>"
            ),
        );
    std::fs::write(&xml, &edited).unwrap();
    let id = Uuid::parse_str(&set).unwrap();
    providers
        .refresh_single_item(id, &MetadataRefreshOptions::default())
        .await
        .unwrap();
    let (status, item) = call(
        &router,
        Some(token),
        "GET",
        &format!("/Users/{user}/Items/{set}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{item}");
    assert_eq!(item["Name"], "Saga Renamed");
    assert_eq!(item["Overview"], "Edited overview");
    assert_eq!(item["LockedFields"], json!(["Overview"]));
    // In the data directory the links are the source of truth: the file's
    // extra member is not taken.
    let (_, children) = call(
        &router,
        Some(token),
        "GET",
        &format!("/Items?parentId={set}&userId={user}"),
        None,
    )
    .await;
    let names: Vec<&str> = children["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Beta"]);
    // What only the file supplied (`MetadataImport`) is not written back.
    assert_eq!(std::fs::read_to_string(&xml).unwrap(), edited);

    // ── A box set folder outside the data directory, in the movies library:
    // its file's members join its links, file first.
    let folder = media.join("Heist [boxset]");
    std::fs::create_dir_all(&folder).unwrap();
    let outside_xml = folder.join("collection.xml");
    std::fs::write(
        &outside_xml,
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<Item>\n  <LocalTitle>The Heist</LocalTitle>\n  \
             <CollectionItems>\n    <CollectionItem><Path>{beta_path}</Path></CollectionItem>\n    \
             <CollectionItem><Path>{alpha_path}</Path></CollectionItem>\n    \
             <CollectionItem><Path>/nowhere/Gone.mkv</Path></CollectionItem>\n  </CollectionItems>\n</Item>\n"
        ),
    )
    .unwrap();
    let persistence = ferrofin_core::FerrofinItemPersistenceService::new(db.clone());
    let heist = Uuid::new_v4();
    persistence
        .save_items(&[BaseItemEntity {
            id: guid_to_db(heist),
            type_: "MediaBrowser.Controller.Entities.Movies.BoxSet".into(),
            name: Some("Heist".into()),
            is_folder: true,
            path: Some(folder.to_string_lossy().into_owned()),
            parent_id: Some(guid_to_db(library)),
            top_parent_id: Some(guid_to_db(library)),
            ..Default::default()
        }])
        .await
        .unwrap();
    persistence.set_ancestors(heist, &[library]).await.unwrap();
    providers
        .refresh_single_item(heist, &MetadataRefreshOptions::default())
        .await
        .unwrap();
    let (_, item) = call(
        &router,
        Some(token),
        "GET",
        &format!("/Users/{user}/Items/{heist}"),
        None,
    )
    .await;
    assert_eq!(item["Name"], "The Heist");
    let (_, children) = call(
        &router,
        Some(token),
        "GET",
        &format!("/Items?parentId={heist}&userId={user}"),
        None,
    )
    .await;
    let mut names: Vec<&str> = children["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["Alpha", "Beta"], "a path naming no item is dropped");

    // The movies library did not open its folders: a metadata edit leaves
    // the file there as it is (the owner rule on media-library writes,
    // gl-1870), and with no file there writes none.
    let before = std::fs::read(&outside_xml).unwrap();
    let (status, body) = call(
        &router,
        Some(token),
        "POST",
        &format!("/Items/{heist}"),
        Some(json!({"Name": "The Heist", "Overview": "Edited"})),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    settle(providers.as_ref()).await;
    assert_eq!(
        std::fs::read(&outside_xml).unwrap(),
        before,
        "the existing file is not rewritten"
    );
    std::fs::remove_file(&outside_xml).unwrap();
    let (status, body) = call(
        &router,
        Some(token),
        "POST",
        &format!("/Items/{heist}"),
        Some(json!({"Name": "The Heist", "Overview": "Edited"})),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !outside_xml.exists(),
        "nothing is written into the media folder"
    );
}
