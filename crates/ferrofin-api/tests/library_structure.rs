//! Library structure — virtual folders, media paths, library options, physical/available.
//!
//! Drives every `LibraryStructureController` + the two `LibraryController`
//! structure-read routes through the real router, against the in-memory
//! [`FakeVirtualFolders`] double, asserting the ported status codes and payload
//! shapes:
//!
//! - `GET/POST/DELETE /Library/VirtualFolders`
//! - `POST /Library/VirtualFolders/Name`
//! - `POST /Library/VirtualFolders/LibraryOptions`
//! - `POST /Library/VirtualFolders/Paths`, `.../Paths/Update`, `DELETE .../Paths`
//! - `GET /Library/PhysicalPaths`
//! - `GET /Libraries/AvailableOptions`

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_api::AppState;
use ferrofin_api::create_router;
use ferrofin_api::test_support::{
    ApiKeyAuthService, AuthedAuthService, CountingScans, FakeAuthService, FakeVirtualFolders,
    authed_state_with_library_and_monitor, authed_state_with_virtual_folders,
    elevated_state_with_virtual_folders, fake_state, setup_complete_state,
};
use tower::ServiceExt;

/// The permissive-auth token the handlers accept (any non-empty token works with
/// the always-authenticating auth service).
const TOKEN: &str = "valid";

/// Reads a response body as JSON.
async fn json_body(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    if bytes.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::from_slice(&bytes).expect("json body")
}

/// Builds a working authed state and returns it plus the shared fake handle.
fn working_state() -> (AppState, Arc<FakeVirtualFolders>) {
    let vf = Arc::new(FakeVirtualFolders::working());
    (authed_state_with_virtual_folders(vf.clone()), vf)
}

#[tokio::test]
async fn get_virtual_folders_returns_the_list() {
    let (state, vf) = working_state();
    // Pre-seed one folder through the trait.
    ferrofin_traits::library::VirtualFolderManager::add_virtual_folder(
        &*vf,
        "Movies",
        Some(ferrofin_model::entities::CollectionTypeOptions::movies),
        &ferrofin_model::configuration::LibraryOptions::default(),
    )
    .await
    .unwrap();

    let response = create_router(state)
        .oneshot(
            Request::builder()
                .uri("/Library/VirtualFolders")
                .header("X-Emby-Token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert!(json.is_array());
    assert_eq!(json[0]["Name"], "Movies");
    assert_eq!(json[0]["CollectionType"], "movies");
}

#[tokio::test]
async fn get_virtual_folders_accessible_during_first_time_setup() {
    // LibraryStructureController is `[Authorize(FirstTimeSetupOrElevated)]`: while
    // the startup wizard is incomplete (the fake config's default), the endpoint is
    // reachable WITHOUT a token so the setup wizard can list/add libraries.
    let state = fake_state().with_virtual_folders(Arc::new(FakeVirtualFolders::working()));
    let response = create_router(state)
        .oneshot(
            Request::builder()
                .uri("/Library/VirtualFolders")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn add_virtual_folder_succeeds_with_query_and_body() {
    let (state, vf) = working_state();
    let response = create_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Library/VirtualFolders?name=Shows&collectionType=tvshows")
                .header("X-Emby-Token", TOKEN)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"LibraryOptions":{"Enabled":true,"PathInfos":[]}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let folders = ferrofin_traits::library::VirtualFolderManager::get_virtual_folders(&*vf)
        .await
        .unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].name.as_deref(), Some("Shows"));
    assert_eq!(
        folders[0].collection_type,
        Some(ferrofin_model::entities::CollectionTypeOptions::tvshows)
    );
}

#[tokio::test]
async fn add_virtual_folder_without_body_uses_query_paths() {
    let (state, vf) = working_state();
    let response = create_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Library/VirtualFolders?name=Music&paths=/a,/b")
                .header("X-Emby-Token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let folders = ferrofin_traits::library::VirtualFolderManager::get_virtual_folders(&*vf)
        .await
        .unwrap();
    assert_eq!(folders[0].locations, vec!["/a".to_owned(), "/b".to_owned()]);
}

#[tokio::test]
async fn add_virtual_folder_rejects_names_outside_the_upstream_regex() {
    let (state, vf) = working_state();
    let router = create_router(state);
    for name in [
        "",
        "%20%20",
        "%20Leading",
        "Trailing%20",
        "Line%0ABreak",
        "%C2%A0Name",
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/Library/VirtualFolders?name={name}"))
                    .header("X-Emby-Token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
    }
    let virtual_folders = ferrofin_traits::library::VirtualFolderManager::get_virtual_folders(&*vf)
        .await
        .unwrap();
    assert!(virtual_folders.is_empty(), "{virtual_folders:?}");
}

#[tokio::test]
async fn remove_virtual_folder_deletes_and_missing_is_404() {
    let (state, vf) = working_state();
    ferrofin_traits::library::VirtualFolderManager::add_virtual_folder(
        &*vf,
        "Lib",
        None,
        &ferrofin_model::configuration::LibraryOptions::default(),
    )
    .await
    .unwrap();
    let router = create_router(state);

    let ok = router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/Library/VirtualFolders?name=Lib")
                .header("X-Emby-Token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::NO_CONTENT);

    let missing = router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/Library/VirtualFolders?name=Ghost")
                .header("X-Emby-Token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rename_virtual_folder_conflict_is_409_and_missing_is_404() {
    let (state, vf) = working_state();
    for name in ["A", "B"] {
        ferrofin_traits::library::VirtualFolderManager::add_virtual_folder(
            &*vf,
            name,
            None,
            &ferrofin_model::configuration::LibraryOptions::default(),
        )
        .await
        .unwrap();
    }
    let router = create_router(state);

    // Rename A -> B (B exists) → 409.
    let conflict = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Library/VirtualFolders/Name?name=A&newName=B")
                .header("X-Emby-Token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);

    // Rename missing source → 404.
    let missing = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Library/VirtualFolders/Name?name=Ghost&newName=Z")
                .header("X-Emby-Token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    // Empty newName → 400.
    let bad = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Library/VirtualFolders/Name?name=A&newName=%20")
                .header("X-Emby-Token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn add_media_path_variants_and_validation() {
    let (state, vf) = working_state();
    ferrofin_traits::library::VirtualFolderManager::add_virtual_folder(
        &*vf,
        "Lib",
        None,
        &ferrofin_model::configuration::LibraryOptions::default(),
    )
    .await
    .unwrap();
    let router = create_router(state);

    // Bare Path form.
    let ok = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Library/VirtualFolders/Paths")
                .header("X-Emby-Token", TOKEN)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"Name":"Lib","Path":"/media/x"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::NO_CONTENT);

    // Neither Path nor PathInfo → 400.
    let bad = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Library/VirtualFolders/Paths")
                .header("X-Emby-Token", TOKEN)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"Name":"Lib"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

    // Empty name → 400.
    let empty = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Library/VirtualFolders/Paths")
                .header("X-Emby-Token", TOKEN)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"Name":"","Path":"/x"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn update_media_path_requires_name_and_path_info() {
    let (state, vf) = working_state();
    ferrofin_traits::library::VirtualFolderManager::add_virtual_folder(
        &*vf,
        "Lib",
        None,
        &ferrofin_model::configuration::LibraryOptions::default(),
    )
    .await
    .unwrap();
    let router = create_router(state);

    let ok = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Library/VirtualFolders/Paths/Update")
                .header("X-Emby-Token", TOKEN)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"Name":"Lib","PathInfo":{"Path":"/x"}}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::NO_CONTENT);

    // Missing name → 400.
    let bad = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Library/VirtualFolders/Paths/Update")
                .header("X-Emby-Token", TOKEN)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"PathInfo":{"Path":"/x"}}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn remove_media_path_validates_and_removes() {
    let (state, vf) = working_state();
    let mut options = ferrofin_model::configuration::LibraryOptions::default();
    options
        .path_infos
        .push(ferrofin_model::configuration::MediaPathInfo {
            path: "/x".to_owned(),
        });
    ferrofin_traits::library::VirtualFolderManager::add_virtual_folder(&*vf, "Lib", None, &options)
        .await
        .unwrap();
    let router = create_router(state);

    let ok = router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/Library/VirtualFolders/Paths?name=Lib&path=/x")
                .header("X-Emby-Token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::NO_CONTENT);

    // Empty path → 400.
    let bad = router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/Library/VirtualFolders/Paths?name=Lib&path=")
                .header("X-Emby-Token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn update_library_options_by_name_and_missing_is_404() {
    let (state, vf) = working_state();
    ferrofin_traits::library::VirtualFolderManager::add_virtual_folder(
        &*vf,
        "Lib",
        None,
        &ferrofin_model::configuration::LibraryOptions::default(),
    )
    .await
    .unwrap();
    let router = create_router(state);

    let ok = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Library/VirtualFolders/LibraryOptions")
                .header("X-Emby-Token", TOKEN)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"Id":"00000000-0000-0000-0000-000000000000","Name":"Lib","LibraryOptions":{"Enabled":false,"PathInfos":[]}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::NO_CONTENT);

    // Only an id, and it matches no library → 404.
    let missing = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Library/VirtualFolders/LibraryOptions")
                .header("X-Emby-Token", TOKEN)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"Id":"00000000-0000-0000-0000-000000000001","LibraryOptions":{"Enabled":false,"PathInfos":[]}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

/// `GET /Library/VirtualFolders` as JSON, for tests that key off the projected `ItemId`.
async fn folders(router: &axum::Router) -> serde_json::Value {
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/Library/VirtualFolders")
                .header("X-Emby-Token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// `POST /Library/VirtualFolders/LibraryOptions` keyed by `id` alone — no `Name`, which
/// is what jellyfin-web sends.
async fn post_options(router: &axum::Router, id: &str, trickplay: bool) -> StatusCode {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Library/VirtualFolders/LibraryOptions")
                .header("X-Emby-Token", TOKEN)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"Id":"{id}","LibraryOptions":{{"EnableTrickplayImageExtraction":{trickplay},"PathInfos":[]}}}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// The id-only form is the *only* one a real client can use — `UpdateLibraryOptionsDto`
/// carries `Guid Id` and nothing else — and the id it echoes back is the dashless
/// `ItemId` from `GET /Library/VirtualFolders`. C# resolves it with
/// `GetItemById<CollectionFolder>(request.Id)`, a `Guid` lookup that does not care how
/// the id was spelled, so both spellings must resolve and the option must round-trip.
#[tokio::test]
async fn update_library_options_by_projected_item_id_round_trips() {
    let (state, vf) = working_state();
    ferrofin_traits::library::VirtualFolderManager::add_virtual_folder(
        &*vf,
        "Lib",
        None,
        &ferrofin_model::configuration::LibraryOptions::default(),
    )
    .await
    .unwrap();
    let router = create_router(state);

    // The id exactly as the server projects it: dashless, no `Name` in the body.
    let listed = folders(&router).await;
    let projected = listed[0]["ItemId"].as_str().unwrap().to_owned();
    assert!(
        !projected.contains('-'),
        "ItemId must be projected dashless like Guid.ToString(\"N\"), got {projected}"
    );
    assert_eq!(
        post_options(&router, &projected, true).await,
        StatusCode::NO_CONTENT
    );
    let after = folders(&router).await;
    assert_eq!(
        after[0]["LibraryOptions"]["EnableTrickplayImageExtraction"],
        serde_json::json!(true),
        "the write must be reflected in the read-back"
    );

    // The same id hyphenated must resolve too — C#'s `Guid` lookup is format-agnostic.
    let hyphenated = uuid::Uuid::parse_str(&projected).unwrap().to_string();
    assert!(hyphenated.contains('-'));
    assert_eq!(
        post_options(&router, &hyphenated, false).await,
        StatusCode::NO_CONTENT
    );
    let after = folders(&router).await;
    assert_eq!(
        after[0]["LibraryOptions"]["EnableTrickplayImageExtraction"],
        serde_json::json!(false)
    );
}

/// Two libraries must project **distinct** `ItemId`s, and an id-keyed write must
/// land on exactly the one it addressed.
///
/// The real manager derives the id from the library's path, so distinct libraries
/// always differ; the fake's projection has to preserve that or a two-library test
/// would silently key both rows to the same id and the "wrote the right one"
/// assertion above would hold vacuously.
#[tokio::test]
async fn distinct_libraries_project_distinct_ids_and_writes_do_not_cross() {
    let (state, vf) = working_state();
    for name in ["Movies", "Shows"] {
        ferrofin_traits::library::VirtualFolderManager::add_virtual_folder(
            &*vf,
            name,
            None,
            &ferrofin_model::configuration::LibraryOptions::default(),
        )
        .await
        .unwrap();
    }
    let router = create_router(state);

    let listed = folders(&router).await;
    let ids: Vec<String> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["ItemId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(
        ids[0], ids[1],
        "distinct libraries must not share an ItemId"
    );

    // The projection must separate names the *old* tiling fold mapped together:
    // it filled the 16 id bytes with `name[i % len] + i`, so any name and its own
    // repetition produced the identical id.
    assert_ne!(
        ferrofin_api::test_support::FakeVirtualFolders::projected_item_id("ab"),
        ferrofin_api::test_support::FakeVirtualFolders::projected_item_id("abab"),
        "a name and its repetition must not fold to the same ItemId"
    );

    assert_eq!(
        post_options(&router, &ids[0], true).await,
        StatusCode::NO_CONTENT
    );
    let after = folders(&router).await;
    assert_eq!(
        after[0]["LibraryOptions"]["EnableTrickplayImageExtraction"],
        serde_json::json!(true)
    );
    assert_eq!(
        after[1]["LibraryOptions"]["EnableTrickplayImageExtraction"],
        serde_json::json!(false),
        "the write must not leak onto the other library"
    );
}

#[tokio::test]
async fn physical_paths_unions_locations() {
    // `GET /Library/PhysicalPaths` is `RequiresElevation` upstream.
    let vf = Arc::new(FakeVirtualFolders::working());
    let state = elevated_state_with_virtual_folders(vf.clone());
    let mut options = ferrofin_model::configuration::LibraryOptions::default();
    options
        .path_infos
        .push(ferrofin_model::configuration::MediaPathInfo {
            path: "/media/a".to_owned(),
        });
    ferrofin_traits::library::VirtualFolderManager::add_virtual_folder(&*vf, "Lib", None, &options)
        .await
        .unwrap();

    let response = create_router(state)
        .oneshot(
            Request::builder()
                .uri("/Library/PhysicalPaths")
                .header("X-Emby-Token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert_eq!(json, serde_json::json!(["/media/a"]));
}

#[tokio::test]
async fn available_options_delegates_to_provider_manager() {
    // The handler resolves the representative item types and delegates the whole
    // projection to the provider manager (the real registry is tested in
    // `ferrofin-providers::library_options`). With the default fake provider this
    // is an empty-but-valid `LibraryOptionsResultDto`.
    let (state, _vf) = working_state();
    let response = create_router(state)
        .oneshot(
            Request::builder()
                .uri("/Libraries/AvailableOptions?libraryContentType=movies")
                .header("X-Emby-Token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_body(response).await;
    assert!(json["MetadataSavers"].is_array());
    assert!(json["TypeOptions"].is_array());
}

#[tokio::test]
async fn available_options_without_content_type_is_ok() {
    let (state, _vf) = working_state();
    let response = create_router(state)
        .oneshot(
            Request::builder()
                .uri("/Libraries/AvailableOptions")
                .header("X-Emby-Token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn backend_failure_maps_to_500() {
    let state = authed_state_with_virtual_folders(Arc::new(FakeVirtualFolders::failing()));
    let response = create_router(state)
        .oneshot(
            Request::builder()
                .uri("/Library/VirtualFolders")
                .header("X-Emby-Token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

/// The complete Jellyfin Web library editor payload (`library` in
/// `fixtures/dashboard-settings.json`, whose provenance `tests/config.rs`
/// records), through creation and ID-based update.
#[tokio::test]
async fn current_web_library_options_survive_creation_and_update() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/dashboard-settings.json")).unwrap();
    let mut options = fixture["library"].clone();
    let (state, _vf) = working_state();
    let router = create_router(state);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Library/VirtualFolders?name=Fixture&collectionType=movies")
                .header("X-Emby-Token", TOKEN)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"LibraryOptions": options}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let listed = folders(&router).await;
    for (field, expected) in options.as_object().unwrap() {
        // Web still sends this hardcoded false. Jellyfin v12 removed it from
        // LibraryOptions; it is not an editable control or a stored setting.
        if field == "EnableArchiveMediaFiles" {
            continue;
        }
        assert_eq!(
            &listed[0]["LibraryOptions"][field], expected,
            "created library: {field}"
        );
    }
    options["Enabled"] = serde_json::json!(false);
    options["TypeOptions"][0]["SimilarItemProviders"] =
        serde_json::json!(["Local Genre/Tag", "TheMovieDb"]);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Library/VirtualFolders/LibraryOptions")
                .header("X-Emby-Token", TOKEN)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"Id": listed[0]["ItemId"], "LibraryOptions": options})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let updated = folders(&router).await;
    for (field, expected) in options.as_object().unwrap() {
        // Web still sends this hardcoded false. Jellyfin v12 removed it from
        // LibraryOptions; it is not an editable control or a stored setting.
        if field == "EnableArchiveMediaFiles" {
            continue;
        }
        assert_eq!(
            &updated[0]["LibraryOptions"][field], expected,
            "updated library: {field}"
        );
    }
}

/// FakeLibrary panics if a scan is queued: a default/false rename must neither
/// index newly added files nor replace metadata through a background refresh.
#[tokio::test]
async fn rename_without_refresh_does_not_queue_a_scan() {
    for refresh in ["", "&refreshLibrary=false"] {
        let (state, vf) = working_state();
        ferrofin_traits::library::VirtualFolderManager::add_virtual_folder(
            &*vf,
            "Old",
            None,
            &ferrofin_model::configuration::LibraryOptions::default(),
        )
        .await
        .unwrap();
        let response = create_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/Library/VirtualFolders/Name?name=Old&newName=New{refresh}"
                    ))
                    .header("X-Emby-Token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let folders = ferrofin_traits::library::VirtualFolderManager::get_virtual_folders(&*vf)
            .await
            .unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name.as_deref(), Some("New"));
    }
}

#[tokio::test]
async fn structure_routes_require_an_administrator_once_setup_is_complete() {
    // Upstream gates the whole controller `FirstTimeSetupOrElevated`
    // (LibraryStructureController.cs:31): anonymous only while the wizard runs,
    // an administrator afterwards. An ordinary account must never reach a
    // handler that deletes, renames or retargets a library.
    let routes = [
        ("GET", "/Library/VirtualFolders", ""),
        ("POST", "/Library/VirtualFolders?name=Shows", ""),
        ("DELETE", "/Library/VirtualFolders?name=Movies", ""),
        (
            "POST",
            "/Library/VirtualFolders/Name?name=Movies&newName=Films",
            "",
        ),
        (
            "POST",
            "/Library/VirtualFolders/LibraryOptions",
            r#"{"Name":"Movies","LibraryOptions":{}}"#,
        ),
        (
            "POST",
            "/Library/VirtualFolders/Paths",
            r#"{"Name":"Movies","PathInfo":{"Path":"/m"}}"#,
        ),
        (
            "POST",
            "/Library/VirtualFolders/Paths/Update",
            r#"{"Name":"Movies","PathInfo":{"Path":"/m"}}"#,
        ),
        (
            "DELETE",
            "/Library/VirtualFolders/Paths?name=Movies&path=/m",
            "",
        ),
    ];
    for (method, uri, body) in routes {
        let state = setup_complete_state(Arc::new(AuthedAuthService))
            .with_virtual_folders(Arc::new(FakeVirtualFolders::working()));
        let response = create_router(state)
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("X-Emby-Token", TOKEN)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {uri}");
    }

    // Anonymous is only for the wizard: after setup it is `401`.
    let state = setup_complete_state(Arc::new(FakeAuthService))
        .with_virtual_folders(Arc::new(FakeVirtualFolders::working()));
    let response = create_router(state)
        .oneshot(
            Request::builder()
                .uri("/Library/VirtualFolders")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // An administrator still gets through (an API key is elevated upstream).
    let state = setup_complete_state(Arc::new(ApiKeyAuthService))
        .with_virtual_folders(Arc::new(FakeVirtualFolders::working()));
    let response = create_router(state)
        .oneshot(
            Request::builder()
                .uri("/Library/VirtualFolders")
                .header("X-Emby-Token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// A [`LibraryMonitor`](ferrofin_traits::library::LibraryMonitor) that counts
/// its restarts.
#[derive(Default)]
struct CountingMonitor(AtomicUsize);

#[async_trait::async_trait]
impl ferrofin_traits::library::LibraryMonitor for CountingMonitor {
    async fn start(&self) -> Result<(), ferrofin_traits::error::ServiceError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn stop(&self) -> Result<(), ferrofin_traits::error::ServiceError> {
        Ok(())
    }
    async fn report_file_system_change_beginning(
        &self,
        _path: &str,
    ) -> Result<(), ferrofin_traits::error::ServiceError> {
        Ok(())
    }
    async fn report_file_system_change_complete(
        &self,
        _path: &str,
        _refresh_path: bool,
    ) -> Result<(), ferrofin_traits::error::ServiceError> {
        Ok(())
    }
    async fn report_file_system_changed(
        &self,
        _path: &str,
    ) -> Result<(), ferrofin_traits::error::ServiceError> {
        Ok(())
    }
}

/// Sends `method uri` (with an optional JSON `body`) to a router whose
/// virtual folders are `vf`, and returns `(status, monitor restarts, queued
/// scans)`.
async fn structure_request(
    vf: FakeVirtualFolders,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, usize, usize) {
    let scans = Arc::new(CountingScans::default());
    let monitor = Arc::new(CountingMonitor::default());
    let state = authed_state_with_library_and_monitor(scans.clone(), monitor.clone())
        .with_virtual_folders(Arc::new(vf));
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("X-Emby-Token", TOKEN);
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let status = create_router(state)
        .oneshot(
            request
                .body(body.map_or_else(Body::empty, |b| Body::from(b.to_owned())))
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    (
        status,
        monitor.0.load(Ordering::SeqCst),
        scans.0.load(Ordering::SeqCst),
    )
}

/// Upstream makes each structure change in a `try` and restarts the monitor
/// or queues the requested refresh in `finally`, after its argument,
/// existence and conflict checks: a change that fails part-way still gets
/// both, and the request still fails.
#[rstest::rstest]
#[case::add_library(
    "POST",
    "/Library/VirtualFolders?name=Movies&refreshLibrary=true",
    None
)]
#[case::remove_library(
    "DELETE",
    "/Library/VirtualFolders?name=Movies&refreshLibrary=true",
    None
)]
#[case::rename_library(
    "POST",
    "/Library/VirtualFolders/Name?name=Movies&newName=Films&refreshLibrary=true",
    None
)]
#[case::add_media_path(
    "POST",
    "/Library/VirtualFolders/Paths?refreshLibrary=true",
    Some(r#"{"Name":"Movies","Path":"/media/movies"}"#)
)]
#[case::remove_media_path(
    "DELETE",
    "/Library/VirtualFolders/Paths?name=Movies&path=/media/movies&refreshLibrary=true",
    None
)]
#[tokio::test]
async fn a_structure_change_that_fails_part_way_still_restarts_and_refreshes(
    #[case] method: &str,
    #[case] uri: &str,
    #[case] body: Option<&str>,
) {
    // Every manager step fails with a backend error, as one after the change.
    let (status, restarts, scans) =
        structure_request(FakeVirtualFolders::failing(), method, uri, body).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(restarts, 1, "monitor restarted");
    assert_eq!(scans, 1, "requested scan queued");
}

/// A change refused by upstream's checks before its `try` (a missing
/// library, 404; a taken name, 409; a bad argument, 400) changes nothing,
/// so nothing is restarted or scanned.
#[rstest::rstest]
#[case::missing_library(
    "DELETE",
    "/Library/VirtualFolders?name=Gone&refreshLibrary=true",
    StatusCode::NOT_FOUND
)]
#[case::taken_name(
    "POST",
    "/Library/VirtualFolders/Name?name=Movies&newName=Films&refreshLibrary=true",
    StatusCode::CONFLICT
)]
#[case::bad_name(
    "POST",
    "/Library/VirtualFolders?name=%20Movies&refreshLibrary=true",
    StatusCode::BAD_REQUEST
)]
#[tokio::test]
async fn a_structure_change_refused_up_front_neither_restarts_nor_refreshes(
    #[case] method: &str,
    #[case] uri: &str,
    #[case] expected: StatusCode,
) {
    let vf = FakeVirtualFolders::working();
    for name in ["Movies", "Films"] {
        ferrofin_traits::library::VirtualFolderManager::add_virtual_folder(
            &vf,
            name,
            None,
            &ferrofin_model::configuration::LibraryOptions::default(),
        )
        .await
        .unwrap();
    }
    let (status, restarts, scans) = structure_request(vf, method, uri, None).await;
    assert_eq!(status, expected);
    assert_eq!((restarts, scans), (0, 0));
}
