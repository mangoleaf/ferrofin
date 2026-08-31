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
    /// Extra plugins `list_plugins` reports, in REGISTRATION order — the
    /// ordering test needs a registry whose order is not already alphabetical.
    extra: Mutex<Vec<PluginDescriptor>>,
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
            configuration_file_name: None,
        }
    }

    fn not_found(id: Uuid) -> ServiceError {
        ServiceError::not_found(format!("plugin {id}"))
    }
}

#[async_trait::async_trait]
impl PluginManager for RecordingPlugins {
    async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError> {
        let mut all = vec![Self::descriptor()];
        all.extend(self.extra.lock().expect("extra lock").iter().cloned());
        Ok(all)
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
        Ok(vec![PackageInfo {
            name: "Demo".to_owned(),
            description: "a demo plugin".to_owned(),
            overview: "a demo plugin".to_owned(),
            owner: "someone".to_owned(),
            category: "General".to_owned(),
            id: known_id(),
            versions: vec![ferrofin_model::updates::VersionInfo {
                version: "1.2.3".to_owned(),
                version_number: "1.2.3".to_owned(),
                ..ferrofin_model::updates::VersionInfo::default()
            }],
            image_url: None,
        }])
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

/// A versioned plugin route resolves through `GetPlugin(id, version)`, and
/// `Version.Equals` compares all four components.
///
/// Measured on the lane-3 lab pair, `DELETE /Plugins/{omdb}/{version}` against
/// Jellyfin 10.11.8: `10.11.8.0` -> 204, `9.9.9.9` -> 404, `10.11.8` -> 404
/// (three components != four), `notaversion` -> 400 (the `[FromRoute] Version`
/// model binder refuses before the action runs). Ferrofin ignored the path
/// version entirely and answered every one of them identically.
#[tokio::test]
async fn versioned_routes_match_the_installed_version() {
    // The stub's descriptor is version "1.2.3".
    for (version, expect) in [
        ("1.2.3", StatusCode::NO_CONTENT),
        ("9.9.9.9", StatusCode::NOT_FOUND),
        ("1.2.3.0", StatusCode::NOT_FOUND),
        ("1.2", StatusCode::NOT_FOUND),
        ("notaversion", StatusCode::BAD_REQUEST),
        ("1", StatusCode::BAD_REQUEST),
        ("-1.2.3", StatusCode::BAD_REQUEST),
    ] {
        let resp = router(Arc::new(RecordingPlugins::default()))
            .oneshot(authed(
                "POST",
                &format!("/Plugins/{}/{version}/Enable", known_id()),
                Body::empty(),
            ))
            .await
            .expect("resp");
        assert_eq!(resp.status(), expect, "Enable at {version}");
    }
    // The image route takes the same gate (`AllowAnonymous` upstream, so no
    // auth header here) — a wrong version is a miss, not the plugin's image.
    let resp = router(Arc::new(RecordingPlugins::default()))
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/Plugins/{}/9.9.9.9/Image", known_id()))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
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
    // `Response.Headers.ContentDisposition = "attachment"` before the
    // `PhysicalFile(...)` (v10.11.8 PluginsController.cs:236, unchanged on
    // master).
    assert_eq!(
        ok.headers()
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap(),
        "attachment"
    );

    let missing = router(fake.clone())
        .oneshot(authed(
            "GET",
            &format!("/Plugins/{}/1.0/Image", Uuid::from_u128(9)),
            Body::empty(),
        ))
        .await
        .expect("resp");
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

/// The `{version}` segment is a `Version`, not decoration.
///
/// The C# binds `[FromRoute, Required] Version version` and looks the plugin up
/// with `GetPlugin(id, version)`, so a malformed segment is a 400 from the model
/// binder and a mismatched one is a 404 — and `Version.Equals` reads an absent
/// component as `-1`, so `1.2.3` is NOT `1.2.3.0`. Ferrofin discarded the
/// segment on all four `{version}` routes, which meant
/// `POST /Plugins/{id}/notaversion/Enable` answered 204 and really did enable
/// the plugin.
#[tokio::test]
async fn a_version_segment_selects_the_plugin() {
    let fake = Arc::new(RecordingPlugins::default());
    // Every `{version}` route, so none of them can drift back.
    let routes = |version: &str| {
        [
            ("GET", format!("/Plugins/{}/{version}/Image", known_id())),
            ("POST", format!("/Plugins/{}/{version}/Enable", known_id())),
            ("POST", format!("/Plugins/{}/{version}/Disable", known_id())),
            ("DELETE", format!("/Plugins/{}/{version}", known_id())),
        ]
    };
    for (expected, version) in [
        // A version that is not the installed one.
        (StatusCode::NOT_FOUND, "9.9.9.9"),
        // The fake installs "1.2.3", and `Version.Equals` reads the absent
        // revision as -1, so the four-component spelling of the same number is
        // a different version. A live 10.11.8 404s "10.11.8" against its
        // installed "10.11.8.0" for exactly this reason.
        (StatusCode::NOT_FOUND, "1.2.3.0"),
        // Not a version at all — the model binder's 400.
        (StatusCode::BAD_REQUEST, "notaversion"),
        (StatusCode::BAD_REQUEST, "1"),
        (StatusCode::BAD_REQUEST, "1.2.3.4.5"),
        (StatusCode::BAD_REQUEST, "-1.0"),
    ] {
        for (method, uri) in routes(version) {
            let resp = router(fake.clone())
                .oneshot(authed(method, &uri, Body::empty()))
                .await
                .expect("resp");
            assert_eq!(resp.status(), expected, "{method} {uri}");
        }
    }
}

/// The manifest is `MediaBrowser.Common/Plugins/PluginManifest.cs` on the wire:
/// **camelCase**, the id spelled `guid` and dashless, and all thirteen fields
/// present. The previous assertion here (`"Name":"Demo"`) cemented a five-key
/// PascalCase blob that shares not one key with what Jellyfin sends, which is
/// how the divergence survived a green test.
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
    // `PluginManifest` is the one camelCase DTO on this surface — every C#
    // property carries an explicit `[JsonPropertyName]`, and `Id` is spelled
    // `guid`. A stock Jellyfin 10.11.8 answers this route for its five in-tree
    // provider plugins with exactly these thirteen keys (measured on the
    // lane-3 lab pair), so the shape, not just the name, is the assertion.
    let manifest: serde_json::Value =
        serde_json::from_str(&body_string(ok).await).expect("manifest json");
    let mut keys: Vec<&str> = manifest
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "assemblies",
            "autoUpdate",
            "category",
            "changelog",
            "description",
            "guid",
            "name",
            "overview",
            "owner",
            "status",
            "targetAbi",
            "timestamp",
            "version",
        ],
        "manifest must be the camelCase PluginManifest, not a PascalCase projection"
    );
    // `JsonGuidConverter` writes `value.ToString("N")` — no dashes — under the
    // key `guid`, not `Id`.
    assert_eq!(manifest["guid"], known_id().simple().to_string());
    assert_eq!(manifest["name"], "Demo");
    assert_eq!(manifest["version"], "1.2.3");
    assert_eq!(manifest["status"], "Active");
    assert_eq!(manifest["autoUpdate"], true);
    assert_eq!(manifest["assemblies"], serde_json::json!([]));
    assert_eq!(manifest["timestamp"], "0001-01-01T00:00:00.0000000Z");
    // The descriptive fields stay EMPTY: a plugin with no `meta.json` gets the
    // dummy record `PluginManager.CreatePluginInstance` builds, which sets only
    // id/name/version/status (`PluginManager.cs:560-575`). `PluginInfo`'s
    // description is NOT empty — the two have different sources upstream.
    assert_eq!(manifest["description"], "");

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
async fn packages_are_listed_and_install_rejected() {
    let fake = Arc::new(RecordingPlugins::default());
    let list = router(fake.clone())
        .oneshot(authed("GET", "/Packages", Body::empty()))
        .await
        .expect("resp");
    assert_eq!(list.status(), StatusCode::OK);
    let body = body_string(list).await;
    assert!(body.contains("\"name\":\"Demo\""), "{body}");
    // `VersionNumber` is a C# getter, so it is on the wire for every entry (the
    // vendored contract declares it non-nullable readOnly).
    assert!(body.contains("\"VersionNumber\":\"1.2.3\""), "{body}");

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
async fn plugin_routes_require_an_administrator() {
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
    // So are the PLUGIN reads. `PluginsController` carries the same class-level
    // `[Authorize(Policy = Policies.RequiresElevation)]` (v10.11.8
    // PluginsController.cs:25, identical on master) and overrides it for exactly
    // one action. `GET /Plugins/{id}/Configuration` is where a plugin's API key,
    // username and password live — Ferrofin previously served them to any
    // authenticated account.
    for (method, uri) in [
        ("GET", "/Plugins".to_owned()),
        ("GET", format!("/Plugins/{}/Configuration", known_id())),
        ("POST", format!("/Plugins/{}/Manifest", known_id())),
    ] {
        let resp = router()
            .oneshot(authed(method, &uri, Body::empty()))
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
    }
    // …and the ONE action upstream marks `[AllowAnonymous]` (:221) still serves
    // a plain user. Elevating it would break every plugin logo in the dashboard.
    let image = router()
        .oneshot(authed(
            "GET",
            &format!("/Plugins/{}/1.2.3/Image", known_id()),
            Body::empty(),
        ))
        .await
        .expect("resp");
    assert_eq!(image.status(), StatusCode::OK);
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

// ── GET /Packages/{name} — InstallationManager.FilterPackages semantics ─────
//
// The C# is `FilterPackages(packages, name, assemblyGuid ?? default)`, whose two
// predicates are ALTERNATIVES with the guid winning, over an `assemblyGuid` bound
// by ASP.NET as a `Guid?` — so the accepted spellings are exactly
// `Guid.TryParse`'s (N/D/B/P/X, trimmed at both ends) and anything else is a 400.
// Ferrofin used to AND them and string-compare the guid in its hyphenated
// spelling — which never matched, because every guid Ferrofin serialises is
// dashless. That is the exact shape jellyfin-web's plugin-detail page sends.
// The format-set cases below are the handler's end of
// `ferrofin_util::guid_extensions::parse_dotnet_guid`, whose own tests carry the
// live 10.11.8 oracle statuses for each spelling.

/// The dashless (`"N"`) spelling — the one Ferrofin itself emits from
/// `/Plugins[].Id` and `/Packages[].guid`, and the one the dashboard echoes back.
#[tokio::test]
async fn package_by_name_accepts_the_dashless_guid_it_emits() {
    let app = router(Arc::new(RecordingPlugins::default()));
    let uri = format!("/Packages/Demo?assemblyGuid={}", known_id().simple());
    let resp = app
        .oneshot(authed("GET", &uri, Body::empty()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("\"Demo\""));
}

/// The hyphenated (`"D"`) spelling resolves too — ASP.NET's `Guid` binder takes
/// either.
#[tokio::test]
async fn package_by_name_accepts_the_hyphenated_guid() {
    let app = router(Arc::new(RecordingPlugins::default()));
    let uri = format!("/Packages/Demo?assemblyGuid={}", known_id().hyphenated());
    let resp = app
        .oneshot(authed("GET", &uri, Body::empty()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// `if (!id.IsEmpty()) … else if (name is not null) …` — the guid selects on its
/// own and the name is ignored, so a wrong name still resolves the guid's package.
#[tokio::test]
async fn package_guid_wins_over_the_path_name() {
    let app = router(Arc::new(RecordingPlugins::default()));
    let uri = format!(
        "/Packages/zzz-no-such-package?assemblyGuid={}",
        known_id().simple()
    );
    let resp = app
        .oneshot(authed("GET", &uri, Body::empty()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("\"Demo\""));
}

/// An all-zeros guid is `Guid.IsEmpty()`, so it falls through to the name branch.
#[tokio::test]
async fn package_nil_guid_falls_through_to_the_name() {
    let app = router(Arc::new(RecordingPlugins::default()));
    let resp = app
        .oneshot(authed(
            "GET",
            "/Packages/Demo?assemblyGuid=00000000000000000000000000000000",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// An unparseable guid is rejected by C# model binding before the action runs.
#[tokio::test]
async fn package_unparseable_guid_is_a_bad_request() {
    let app = router(Arc::new(RecordingPlugins::default()));
    let resp = app
        .oneshot(authed(
            "GET",
            "/Packages/Demo?assemblyGuid=notaguid",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// An unknown name with no guid is still a 404.
#[tokio::test]
async fn package_unknown_name_is_not_found() {
    let app = router(Arc::new(RecordingPlugins::default()));
    let resp = app
        .oneshot(authed("GET", "/Packages/zzz-nope", Body::empty()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// The braced (`"B"`), parenthesised (`"P"`) and hex-object (`"X"`) spellings,
/// and a value padded with whitespace — all accepted by `Guid.TryParse`, so all
/// accepted here. `P` and `X` were live divergences (400 here, 200 on the
/// 10.11.8 oracle) until the binder stopped being `Uuid::parse_str`.
#[tokio::test]
async fn package_accepts_every_dotnet_guid_spelling() {
    let id = known_id();
    let hyphenated = id.hyphenated().to_string();
    let bytes = id.as_bytes();
    let hex_object = format!(
        "{{0x{:08x},0x{:04x},0x{:04x},{{0x{:02x},0x{:02x},0x{:02x},0x{:02x},\
0x{:02x},0x{:02x},0x{:02x},0x{:02x}}}}}",
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u16::from_be_bytes([bytes[4], bytes[5]]),
        u16::from_be_bytes([bytes[6], bytes[7]]),
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    );
    for spelling in [
        format!("{{{hyphenated}}}"),
        format!("({hyphenated})"),
        hex_object,
        format!("  {hyphenated}  "),
    ] {
        let uri = format!(
            "/Packages/Demo?assemblyGuid={}",
            urlencoding_all(spelling.as_str())
        );
        let resp = router(Arc::new(RecordingPlugins::default()))
            .oneshot(authed("GET", &uri, Body::empty()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{spelling}");
    }
}

/// `urn:uuid:` is the one spelling `Uuid::parse_str` takes and .NET does not —
/// measured 200 here / 400 on the oracle before the binder was ported.
#[tokio::test]
async fn package_urn_guid_is_a_bad_request() {
    let uri = format!("/Packages/Demo?assemblyGuid=urn%3Auuid%3A{}", known_id());
    let resp = router(Arc::new(RecordingPlugins::default()))
        .oneshot(authed("GET", &uri, Body::empty()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// `POST /Packages/Installed/{name}` binds its `assemblyGuid` the same way, so
/// it must take (and refuse) the same spellings. Both outcomes are a 400 with
/// this fake — the trait default refuses runtime installs — so the discriminator
/// is WHOSE 400 it is: model binding never reaches the manager.
#[tokio::test]
async fn install_binds_the_assembly_guid_the_same_way() {
    let uri = format!(
        "/Packages/Installed/Demo?assemblyGuid={}",
        urlencoding_all(&format!("({})", known_id().hyphenated()))
    );
    let resp = router(Arc::new(RecordingPlugins::default()))
        .oneshot(authed("POST", &uri, Body::empty()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_string(resp).await;
    assert!(
        body.contains("runtime plugin installation is not available"),
        "the P spelling must bind and reach the manager: {body}"
    );

    let resp = router(Arc::new(RecordingPlugins::default()))
        .oneshot(authed(
            "POST",
            &format!(
                "/Packages/Installed/Demo?assemblyGuid=urn%3Auuid%3A{}",
                known_id()
            ),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_string(resp).await;
    assert!(
        body.contains("is not a valid GUID"),
        "the URN spelling must be refused by binding, before the manager: {body}"
    );
}

/// Percent-encodes every byte that is not an unreserved URI character, so a
/// guid spelling full of braces, parentheses and commas survives the query
/// string exactly as written.
fn urlencoding_all(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// `GET /Plugins` is `_pluginManager.Plugins.OrderBy(p => p.Name)`
/// (v10.11.8 `PluginsController.cs:55-57`) — the wire order is alphabetical, not
/// registration order.
///
/// Measured on the lane-3 lab pair before the fix: Jellyfin returned AudioDB,
/// MusicBrainz, OMDb, Studio Images, TMDb for its five in-tree provider plugins
/// while Ferrofin returned them TMDb-first, in the order the composition root
/// registers them.
#[tokio::test]
async fn plugins_are_listed_by_name() {
    let fake = Arc::new(RecordingPlugins::default());
    for (n, name) in ["Zulu", "alpha", "Studio Images", "AudioDB"]
        .iter()
        .enumerate()
    {
        fake.extra
            .lock()
            .expect("extra lock")
            .push(PluginDescriptor {
                id: Uuid::from_u128(100 + n as u128),
                name: (*name).to_owned(),
                ..RecordingPlugins::descriptor()
            });
    }
    let resp = router(fake)
        .oneshot(authed("GET", "/Plugins", Body::empty()))
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_str(&body_string(resp).await).expect("plugins json");
    let names: Vec<&str> = body
        .as_array()
        .expect("array")
        .iter()
        .map(|p| p["Name"].as_str().expect("name"))
        .collect();
    // `Ord` on `String` is bytewise, which puts every capitalised name before a
    // lowercase one — the same order .NET's default string comparer produces
    // for this set, and the same order the live Jellyfin returned.
    assert_eq!(
        names,
        vec!["AudioDB", "Demo", "Studio Images", "Zulu", "alpha"]
    );
}
