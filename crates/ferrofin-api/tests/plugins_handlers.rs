//! Handler tests for the Tier-1 plugin-manager surface (`handlers::plugins`).
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` against
//! a [`RecordingPlugins`] fake, asserting the HTTP status and (where relevant) the
//! recorded manager call or response body. Authentication always succeeds via the
//! `authed_state_with_plugins` helper, so a route reaches its handler.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::test_support::authed_state_with_plugins;
use ferrofin_model::updates::{PackageInfo, RepositoryInfo};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::plugins::{PluginDescriptor, PluginImage, PluginManager};
use tower::ServiceExt;
use uuid::Uuid;

/// The one plugin the fake reports as installed.
fn known_id() -> Uuid {
    Uuid::from_u128(0x0ABC)
}

/// A recording [`PluginManager`] fake with a single known plugin.
#[derive(Default)]
struct RecordingPlugins {
    /// Repositories persisted by `set_repositories`.
    repositories: Mutex<Vec<RepositoryInfo>>,
    /// Configs written by `set_plugin_configuration`, in order.
    configs_written: Mutex<Vec<(Uuid, Vec<u8>)>>,
}

impl RecordingPlugins {
    fn descriptor() -> PluginDescriptor {
        PluginDescriptor {
            id: known_id(),
            name: "Demo".to_owned(),
            version: "1.2.3".to_owned(),
            description: "a demo plugin".to_owned(),
            enabled: true,
            has_image: true,
            can_uninstall: true,
        }
    }

    fn not_found(id: Uuid) -> ServiceError {
        ServiceError::not_found(format!("plugin {id}"))
    }
}

#[async_trait::async_trait]
impl PluginManager for RecordingPlugins {
    async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError> {
        Ok(vec![Self::descriptor()])
    }
    async fn get_plugin(&self, id: Uuid) -> Result<Option<PluginDescriptor>, ServiceError> {
        Ok((id == known_id()).then(Self::descriptor))
    }
    async fn enable_plugin(&self, id: Uuid) -> Result<(), ServiceError> {
        if id == known_id() {
            Ok(())
        } else {
            Err(Self::not_found(id))
        }
    }
    async fn disable_plugin(&self, id: Uuid) -> Result<(), ServiceError> {
        if id == known_id() {
            Ok(())
        } else {
            Err(Self::not_found(id))
        }
    }
    async fn remove_plugin(&self, id: Uuid) -> Result<(), ServiceError> {
        if id == known_id() {
            Err(ServiceError::invalid_input(
                "cannot uninstall a compiled-in plugin",
            ))
        } else {
            Err(Self::not_found(id))
        }
    }
    async fn get_plugin_configuration(&self, id: Uuid) -> Result<Vec<u8>, ServiceError> {
        if id == known_id() {
            Ok(br#"{"k":1}"#.to_vec())
        } else {
            Err(Self::not_found(id))
        }
    }
    async fn set_plugin_configuration(
        &self,
        id: Uuid,
        config: Vec<u8>,
    ) -> Result<(), ServiceError> {
        if id != known_id() {
            return Err(Self::not_found(id));
        }
        serde_json::from_slice::<serde_json::Value>(&config)
            .map_err(|_| ServiceError::invalid_input("config must be JSON"))?;
        self.configs_written.lock().unwrap().push((id, config));
        Ok(())
    }
    async fn plugin_image(&self, id: Uuid) -> Result<Option<PluginImage>, ServiceError> {
        Ok((id == known_id()).then(|| PluginImage {
            content_type: "image/png".to_owned(),
            data: vec![1, 2, 3, 4],
        }))
    }
    async fn get_repositories(&self) -> Result<Vec<RepositoryInfo>, ServiceError> {
        Ok(self.repositories.lock().unwrap().clone())
    }
    async fn set_repositories(
        &self,
        repositories: Vec<RepositoryInfo>,
    ) -> Result<(), ServiceError> {
        *self.repositories.lock().unwrap() = repositories;
        Ok(())
    }
    async fn list_packages(&self) -> Result<Vec<PackageInfo>, ServiceError> {
        Ok(Vec::new())
    }
}

fn authed(method: &str, uri: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", "Token abc")
        .header("content-type", "application/json")
        .body(body)
        .expect("request")
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

fn router(fake: Arc<RecordingPlugins>) -> axum::Router {
    create_router(authed_state_with_plugins(fake))
}

#[tokio::test]
async fn get_plugins_lists_installed() {
    let resp = router(Arc::new(RecordingPlugins::default()))
        .oneshot(authed("GET", "/Plugins", Body::empty()))
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("\"Name\":\"Demo\""), "{body}");
    assert!(body.contains("\"Status\":\"Active\""), "{body}");
    assert!(body.contains("\"CanUninstall\":true"), "{body}");
}

#[tokio::test]
async fn get_configuration_known_and_unknown() {
    let fake = Arc::new(RecordingPlugins::default());
    let ok = router(fake.clone())
        .oneshot(authed(
            "GET",
            &format!("/Plugins/{}/Configuration", known_id()),
            Body::empty(),
        ))
        .await
        .expect("resp");
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(body_string(ok).await, r#"{"k":1}"#);

    let missing = router(fake)
        .oneshot(authed(
            "GET",
            &format!("/Plugins/{}/Configuration", Uuid::from_u128(9)),
            Body::empty(),
        ))
        .await
        .expect("resp");
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_configuration_validates_json() {
    let fake = Arc::new(RecordingPlugins::default());
    let ok = router(fake.clone())
        .oneshot(authed(
            "POST",
            &format!("/Plugins/{}/Configuration", known_id()),
            Body::from(r#"{"k":2}"#),
        ))
        .await
        .expect("resp");
    assert_eq!(ok.status(), StatusCode::NO_CONTENT);
    assert_eq!(fake.configs_written.lock().unwrap().len(), 1);

    let bad = router(fake)
        .oneshot(authed(
            "POST",
            &format!("/Plugins/{}/Configuration", known_id()),
            Body::from("not json"),
        ))
        .await
        .expect("resp");
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn enable_and_disable() {
    let fake = Arc::new(RecordingPlugins::default());
    for (verb, expect) in [
        ("Enable", StatusCode::NO_CONTENT),
        ("Disable", StatusCode::NO_CONTENT),
    ] {
        let resp = router(fake.clone())
            .oneshot(authed(
                "POST",
                &format!("/Plugins/{}/1.2.3/{verb}", known_id()),
                Body::empty(),
            ))
            .await
            .expect("resp");
        assert_eq!(resp.status(), expect, "{verb}");
    }
    // Unknown plugin → 404.
    let missing = router(fake)
        .oneshot(authed(
            "POST",
            &format!("/Plugins/{}/1.0/Enable", Uuid::from_u128(9)),
            Body::empty(),
        ))
        .await
        .expect("resp");
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn uninstall_is_rejected_for_compiled_in() {
    let fake = Arc::new(RecordingPlugins::default());
    let known = router(fake.clone())
        .oneshot(authed(
            "DELETE",
            &format!("/Plugins/{}", known_id()),
            Body::empty(),
        ))
        .await
        .expect("resp");
    assert_eq!(known.status(), StatusCode::BAD_REQUEST);

    let by_version = router(fake.clone())
        .oneshot(authed(
            "DELETE",
            &format!("/Plugins/{}/1.2.3", known_id()),
            Body::empty(),
        ))
        .await
        .expect("resp");
    assert_eq!(by_version.status(), StatusCode::BAD_REQUEST);

    let missing = router(fake)
        .oneshot(authed(
            "DELETE",
            &format!("/Plugins/{}", Uuid::from_u128(9)),
            Body::empty(),
        ))
        .await
        .expect("resp");
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn plugin_image_served_and_missing() {
    let fake = Arc::new(RecordingPlugins::default());
    let ok = router(fake.clone())
        .oneshot(authed(
            "GET",
            &format!("/Plugins/{}/1.2.3/Image", known_id()),
            Body::empty(),
        ))
        .await
        .expect("resp");
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(
        ok.headers().get("content-type").unwrap().to_str().unwrap(),
        "image/png"
    );

    let missing = router(fake)
        .oneshot(authed(
            "GET",
            &format!("/Plugins/{}/1.0/Image", Uuid::from_u128(9)),
            Body::empty(),
        ))
        .await
        .expect("resp");
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn manifest_read() {
    let fake = Arc::new(RecordingPlugins::default());
    let ok = router(fake.clone())
        .oneshot(authed(
            "POST",
            &format!("/Plugins/{}/Manifest", known_id()),
            Body::empty(),
        ))
        .await
        .expect("resp");
    assert_eq!(ok.status(), StatusCode::OK);
    assert!(body_string(ok).await.contains("\"Name\":\"Demo\""));

    let missing = router(fake)
        .oneshot(authed(
            "POST",
            &format!("/Plugins/{}/Manifest", Uuid::from_u128(9)),
            Body::empty(),
        ))
        .await
        .expect("resp");
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn repositories_round_trip() {
    let fake = Arc::new(RecordingPlugins::default());
    let set = router(fake.clone())
        .oneshot(authed(
            "POST",
            "/Repositories",
            Body::from(r#"[{"Name":"Main","Url":"https://x.test/m.json","Enabled":true}]"#),
        ))
        .await
        .expect("resp");
    assert_eq!(set.status(), StatusCode::NO_CONTENT);

    let get = router(fake)
        .oneshot(authed("GET", "/Repositories", Body::empty()))
        .await
        .expect("resp");
    assert_eq!(get.status(), StatusCode::OK);
    assert!(body_string(get).await.contains("https://x.test/m.json"));
}

#[tokio::test]
async fn packages_are_empty_and_install_rejected() {
    let fake = Arc::new(RecordingPlugins::default());
    let list = router(fake.clone())
        .oneshot(authed("GET", "/Packages", Body::empty()))
        .await
        .expect("resp");
    assert_eq!(list.status(), StatusCode::OK);
    assert_eq!(body_string(list).await, "[]");

    let by_name = router(fake.clone())
        .oneshot(authed("GET", "/Packages/Anything", Body::empty()))
        .await
        .expect("resp");
    assert_eq!(by_name.status(), StatusCode::NOT_FOUND);

    let install = router(fake.clone())
        .oneshot(authed(
            "POST",
            "/Packages/Installed/Anything",
            Body::empty(),
        ))
        .await
        .expect("resp");
    assert_eq!(install.status(), StatusCode::BAD_REQUEST);

    let cancel = router(fake)
        .oneshot(authed(
            "DELETE",
            &format!("/Packages/Installing/{}", Uuid::from_u128(7)),
            Body::empty(),
        ))
        .await
        .expect("resp");
    assert_eq!(cancel.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn plugin_mutations_require_an_administrator() {
    // A plain authenticated user (no admin policy, not an API key) must get
    // 403 from every plugin-mutating route — install would otherwise let any
    // account stage arbitrary code for the next boot.
    use ferrofin_api::test_support::user_authed_state_with_plugins;
    let fake = Arc::new(RecordingPlugins::default());
    let router = || create_router(user_authed_state_with_plugins(fake.clone()));

    for (method, uri, body) in [
        ("POST", "/Repositories".to_owned(), Body::from("[]")),
        (
            "POST",
            "/Packages/Installed/Anything".to_owned(),
            Body::empty(),
        ),
        ("DELETE", format!("/Plugins/{}", known_id()), Body::empty()),
        (
            "DELETE",
            format!("/Plugins/{}/1.0.0", known_id()),
            Body::empty(),
        ),
        (
            "DELETE",
            format!("/Packages/Installing/{}", Uuid::from_u128(7)),
            Body::empty(),
        ),
        (
            "POST",
            format!("/Plugins/{}/Configuration", known_id()),
            Body::from("{}"),
        ),
        (
            "POST",
            format!("/Plugins/{}/1.0.0/Enable", known_id()),
            Body::empty(),
        ),
        (
            "POST",
            format!("/Plugins/{}/1.0.0/Disable", known_id()),
            Body::empty(),
        ),
    ] {
        let resp = router()
            .oneshot(authed(method, &uri, body))
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
    }
    // The catalog reads are elevated too. `PackageController` carries a
    // CLASS-level `[Authorize(Policy = Policies.RequiresElevation)]` at
    // v10.11.8, so `GET /Repositories` and `GET /Packages` are admin-only
    // upstream — jellyfin-web only surfaces the catalog inside the admin
    // dashboard. Ferrofin previously let any account read them.
    for uri in ["/Repositories", "/Packages"] {
        let resp = router()
            .oneshot(authed("GET", uri, Body::empty()))
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "GET {uri}");
    }
}

/// Captures what the transport forwards to a plugin and answers with
/// deliberately hostile headers, to pin the two filters.
#[derive(Default)]
struct RecordingRequestHandler {
    seen: std::sync::Mutex<Option<ferrofin_traits::plugins::PluginWebRequest>>,
}

#[async_trait::async_trait]
impl ferrofin_traits::plugins::PluginRequestHandler for RecordingRequestHandler {
    async fn handle(
        &self,
        plugin_id: Uuid,
        request: ferrofin_traits::plugins::PluginWebRequest,
    ) -> Result<Option<ferrofin_traits::plugins::PluginWebResponse>, ServiceError> {
        if plugin_id != known_id() {
            return Ok(None);
        }
        *self.seen.lock().unwrap() = Some(request);
        Ok(Some(ferrofin_traits::plugins::PluginWebResponse {
            status: 201,
            headers: vec![
                ("x-plugin".to_owned(), "ok".to_owned()),
                // Framing/hop-by-hop must be dropped by the transport.
                ("content-length".to_owned(), "9999".to_owned()),
                ("transfer-encoding".to_owned(), "chunked".to_owned()),
                ("connection".to_owned(), "close".to_owned()),
            ],
            body: b"made-it".to_vec(),
        }))
    }
}

#[tokio::test]
async fn plugin_route_strips_credentials_and_reserved_headers() {
    let handler = Arc::new(RecordingRequestHandler::default());
    let state = authed_state_with_plugins(Arc::new(RecordingPlugins::default()))
        .with_plugin_request_handler(handler.clone());
    let resp = create_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/Plugins/{}/web/hook?x=1&api_key=SECRET&y=2",
                    known_id()
                ))
                .header("Authorization", "Token super-secret")
                .header("Cookie", "session=abc")
                .header("X-Emby-Token", "tok")
                .header("x-custom", "kept")
                .body(Body::from("payload"))
                .expect("request"),
        )
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(
        resp.headers().get("x-plugin").is_some(),
        "benign guest header forwarded"
    );
    assert!(
        resp.headers().get("transfer-encoding").is_none()
            && resp.headers().get("connection").is_none(),
        "framing/hop-by-hop headers dropped"
    );
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    assert_eq!(&body[..], b"made-it");

    let seen = handler.seen.lock().unwrap().clone().expect("captured");
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.path, "/hook");
    assert_eq!(seen.query, "x=1&y=2", "api_key stripped from the query");
    let names: Vec<&str> = seen.headers.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        !names.contains(&"authorization")
            && !names.contains(&"cookie")
            && !names.contains(&"x-emby-token"),
        "credential headers never reach the guest: {names:?}"
    );
    assert!(names.contains(&"x-custom"), "other headers pass through");
    assert_eq!(seen.body.as_deref(), Some(&b"payload"[..]));

    // No dispatcher wired (default state) -> the URL space 404s.
    let resp = router(Arc::new(RecordingPlugins::default()))
        .oneshot(authed(
            "GET",
            &format!("/Plugins/{}/web/hook", known_id()),
            Body::empty(),
        ))
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
