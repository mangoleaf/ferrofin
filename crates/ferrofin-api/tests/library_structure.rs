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

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_api::AppState;
use ferrofin_api::create_router;
use ferrofin_api::test_support::{
    FakeVirtualFolders, authed_state_with_virtual_folders, elevated_state_with_virtual_folders,
    fake_state,
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
async fn add_virtual_folder_empty_name_is_bad_request() {
    let (state, _vf) = working_state();
    let response = create_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Library/VirtualFolders?name=%20%20")
                .header("X-Emby-Token", TOKEN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
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
