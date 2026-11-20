//! Handler tests for the Single Sign-On plugin's provider settings routes
//! (`handlers::sso`): `GET /sso/admin/oidc/providers` and
//! `PUT`/`DELETE /sso/admin/oidc/providers/{provider}`.
//!
//! Each test drives a route through the real router against a [`FakeSso`]
//! manager, asserting the status, the body and whether the manager was
//! reached. The caller is an API key (an administrator), a plain user, or
//! nobody.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    authed_state_with_plugins, fake_state, user_authed_state_with_plugins,
};
use ferrofin_model::sso::{OidcProvider, OidcProviderInfo, OidcProviderList};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::plugins::DisabledPluginManager;
use ferrofin_traits::sso::SsoManager;
use tower::ServiceExt;

const SECRET: &str = "route-client-secret-31";

/// An [`SsoManager`] fake: enabled or not, refusing a save with `refusal`
/// when set, and keeping what it was given.
#[derive(Default)]
struct FakeSso {
    enabled: bool,
    refusal: Option<String>,
    saved: Mutex<Vec<(String, OidcProvider)>>,
    deleted: Mutex<Vec<String>>,
    listed: Mutex<usize>,
}

impl FakeSso {
    fn new(enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            enabled,
            ..Self::default()
        })
    }

    fn saved(&self) -> Vec<(String, OidcProvider)> {
        self.saved.lock().expect("saved").clone()
    }

    fn deleted(&self) -> Vec<String> {
        self.deleted.lock().expect("deleted").clone()
    }

    /// Whether any route reached the manager past its enabled check.
    fn untouched(&self) -> bool {
        self.saved().is_empty()
            && self.deleted().is_empty()
            && *self.listed.lock().expect("listed") == 0
    }
}

/// What [`FakeSso`] lists and answers a save with: `provider` with a stored
/// secret.
fn info(provider: OidcProvider) -> OidcProviderInfo {
    OidcProviderInfo {
        callback_url: Some(format!(
            "https://media.example.org/sso/oidc/{}/callback",
            provider.key
        )),
        provider: OidcProvider {
            client_secret: None,
            ..provider
        },
        has_client_secret: true,
        validation_error: None,
    }
}

#[async_trait::async_trait]
impl SsoManager for FakeSso {
    async fn ensure_enabled(&self) -> Result<(), ServiceError> {
        if self.enabled {
            Ok(())
        } else {
            Err(ServiceError::not_found(
                "the Single Sign-On plugin is disabled",
            ))
        }
    }

    async fn oidc_providers(&self) -> Result<OidcProviderList, ServiceError> {
        self.ensure_enabled().await?;
        *self.listed.lock().expect("listed") += 1;
        Ok(OidcProviderList {
            public_url: "https://media.example.org".to_owned(),
            providers: vec![info(OidcProvider {
                key: "pocket".to_owned(),
                name: "Pocket ID".to_owned(),
                issuer: "https://id.example.org".to_owned(),
                client_id: "ferrofin".to_owned(),
                ..OidcProvider::default()
            })],
            public_url_error: None,
        })
    }

    async fn save_oidc_provider(
        &self,
        key: &str,
        provider: OidcProvider,
    ) -> Result<OidcProviderInfo, ServiceError> {
        self.ensure_enabled().await?;
        if let Some(why) = &self.refusal {
            return Err(ServiceError::invalid_input(why.clone()));
        }
        self.saved
            .lock()
            .expect("saved")
            .push((key.to_owned(), provider.clone()));
        Ok(info(OidcProvider {
            key: key.to_owned(),
            ..provider
        }))
    }

    async fn delete_oidc_provider(&self, key: &str) -> Result<(), ServiceError> {
        self.ensure_enabled().await?;
        if key != "pocket" {
            return Err(ServiceError::not_found(format!("OIDC provider {key}")));
        }
        self.deleted.lock().expect("deleted").push(key.to_owned());
        Ok(())
    }
}

fn as_admin(fake: Arc<FakeSso>) -> AppState {
    authed_state_with_plugins(Arc::new(DisabledPluginManager)).with_sso(fake)
}

fn as_user(fake: Arc<FakeSso>) -> AppState {
    user_authed_state_with_plugins(Arc::new(DisabledPluginManager)).with_sso(fake)
}

/// What the settings page sends for a new provider.
const PAGE_BODY: &str = r#"{"Name":"Pocket ID","Enabled":true,"Issuer":"https://id.example.org",
    "AllowHttpIssuer":false,"ClientId":"ferrofin","Scopes":"openid profile",
    "UsernameClaim":"preferred_username","ClientSecret":"route-client-secret-31"}"#;

/// Each route, with the body the settings page sends.
const PAGE_REQUESTS: [(&str, &str, &str); 3] = [
    ("GET", "/sso/admin/oidc/providers", ""),
    ("PUT", "/sso/admin/oidc/providers/pocket", PAGE_BODY),
    ("DELETE", "/sso/admin/oidc/providers/pocket", ""),
];

fn request(method: &str, path: &str, body: &str, token: bool) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if token {
        builder = builder.header("Authorization", "Token abc");
    }
    if !body.is_empty() {
        builder = builder.header("Content-Type", "application/json");
    }
    builder.body(Body::from(body.to_owned())).expect("request")
}

async fn send(state: AppState, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = create_router(state)
        .oneshot(request)
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, bytes.to_vec())
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("a JSON body")
}

#[tokio::test]
async fn an_admin_lists_the_providers_without_a_secret() {
    let fake = FakeSso::new(true);
    let (status, body) = send(
        as_admin(fake.clone()),
        request("GET", "/sso/admin/oidc/providers", "", true),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&body);
    assert_eq!(body["PublicUrl"], "https://media.example.org");
    let provider = &body["Providers"][0];
    assert_eq!(provider["Key"], "pocket");
    assert_eq!(provider["Name"], "Pocket ID");
    assert_eq!(provider["Scopes"], "openid profile");
    assert_eq!(provider["UsernameClaim"], "preferred_username");
    assert_eq!(provider["AllowHttpIssuer"], false);
    assert_eq!(provider["HasClientSecret"], true);
    assert_eq!(
        provider["CallbackUrl"],
        "https://media.example.org/sso/oidc/pocket/callback"
    );
    assert!(provider.get("ClientSecret").is_none(), "{body}");
    assert!(provider.get("ValidationError").is_none(), "{body}");
}

#[tokio::test]
async fn an_admin_saves_a_provider_under_the_key_in_its_path() {
    let fake = FakeSso::new(true);
    let (status, body) = send(
        as_admin(fake.clone()),
        request("PUT", "/sso/admin/oidc/providers/pocket", PAGE_BODY, true),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body_text = String::from_utf8_lossy(&body).into_owned();
    assert!(!body_text.contains(SECRET), "{body_text}");
    let body = json(&body);
    assert_eq!(body["Key"], "pocket");
    assert_eq!(body["HasClientSecret"], true);
    let saved = fake.saved();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].0, "pocket");
    assert_eq!(saved[0].1.key, "", "the body names no key: the path does");
    assert_eq!(saved[0].1.client_id, "ferrofin");
    assert_eq!(
        saved[0]
            .1
            .client_secret
            .as_ref()
            .map(|s| s.expose().to_owned()),
        Some(SECRET.to_owned())
    );
}

#[tokio::test]
async fn a_body_without_a_secret_or_settings_binds_to_the_defaults() {
    let fake = FakeSso::new(true);
    // camelCase binds too, as every body does.
    let (status, _) = send(
        as_admin(fake.clone()),
        request(
            "PUT",
            "/sso/admin/oidc/providers/pocket",
            r#"{"name":"Pocket","issuer":"https://id.example.org","clientId":"a"}"#,
            true,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, provider) = fake.saved().remove(0);
    assert_eq!(provider.name, "Pocket");
    assert_eq!(provider.client_id, "a");
    assert_eq!(provider.client_secret, None, "keeps the stored secret");
    assert!(provider.enabled);
    assert_eq!(provider.scopes, "openid profile");
    assert_eq!(provider.username_claim, "preferred_username");
}

#[tokio::test]
async fn a_refused_setting_is_a_bad_request_naming_it() {
    let fake = Arc::new(FakeSso {
        enabled: true,
        refusal: Some("the provider name is required".to_owned()),
        ..FakeSso::default()
    });
    let (status, body) = send(
        as_admin(fake),
        request("PUT", "/sso/admin/oidc/providers/pocket", PAGE_BODY, true),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json(&body)["error"],
        "invalid input: the provider name is required"
    );
}

#[tokio::test]
async fn a_body_that_does_not_bind_is_a_validation_problem() {
    for body in ["not json", "[]", r#"{"Enabled":"yes"}"#] {
        let fake = FakeSso::new(true);
        let (status, answer) = send(
            as_admin(fake.clone()),
            request("PUT", "/sso/admin/oidc/providers/pocket", body, true),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            json(&answer)["title"],
            "One or more validation errors occurred.",
            "{body}"
        );
        assert!(fake.untouched(), "{body}");
    }
}

#[tokio::test]
async fn an_admin_deletes_a_provider_and_an_unknown_one_is_not_found() {
    let fake = FakeSso::new(true);
    let (status, body) = send(
        as_admin(fake.clone()),
        request("DELETE", "/sso/admin/oidc/providers/pocket", "", true),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(body, Vec::<u8>::new());
    assert_eq!(fake.deleted(), ["pocket"]);
    let (status, _) = send(
        as_admin(fake.clone()),
        request("DELETE", "/sso/admin/oidc/providers/nope", "", true),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_plain_user_is_refused_before_the_plugin_is_reached() {
    for (method, path, body) in PAGE_REQUESTS {
        let fake = FakeSso::new(true);
        let (status, _) = send(as_user(fake.clone()), request(method, path, body, true)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        assert!(fake.untouched(), "{method} {path}");
    }
}

#[tokio::test]
async fn an_anonymous_caller_is_unauthorized_while_the_plugin_is_enabled() {
    for (method, path, body) in PAGE_REQUESTS {
        let fake = FakeSso::new(true);
        let (status, _) = send(
            fake_state().with_sso(fake.clone()),
            request(method, path, body, false),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
        assert!(fake.untouched(), "{method} {path}");
    }
}

/// B1: a disabled plugin's routes do not exist, for any caller and any body.
#[tokio::test]
async fn a_disabled_plugin_answers_not_found_to_everyone_whatever_the_body() {
    for (method, path, page_body) in PAGE_REQUESTS {
        for body in [page_body, "{}", "not json"] {
            let callers: [(AppState, bool); 3] = [
                (as_admin(FakeSso::new(false)), true),
                (as_user(FakeSso::new(false)), true),
                (fake_state().with_sso(FakeSso::new(false)), false),
            ];
            for (state, token) in callers {
                let (status, _) = send(state, request(method, path, body, token)).await;
                assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path} {body}");
            }
        }
    }
    // A provider segment that does not decode is not looked at either.
    for method in ["PUT", "DELETE"] {
        let (status, _) = send(
            fake_state().with_sso(FakeSso::new(false)),
            request(method, "/sso/admin/oidc/providers/%FF", "", false),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method}");
    }
}

#[tokio::test]
async fn an_undecodable_provider_segment_is_a_bad_request_for_an_admin() {
    for (method, body) in [("PUT", PAGE_BODY), ("DELETE", "")] {
        let fake = FakeSso::new(true);
        let (status, _) = send(
            as_admin(fake.clone()),
            request(method, "/sso/admin/oidc/providers/%FF", body, true),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method}");
        assert!(fake.untouched(), "{method}");
    }
}

#[tokio::test]
async fn a_server_without_the_extension_answers_not_found() {
    for (method, path, body) in PAGE_REQUESTS {
        let state = authed_state_with_plugins(Arc::new(DisabledPluginManager));
        let (status, _) = send(state, request(method, path, body, true)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
    }
}

#[tokio::test]
async fn every_sso_plugin_route_is_registered() {
    use ferrofin_api::handlers::{EXTENSION_ROUTES, PLUGIN_ROUTES, REAL_ROUTES};
    let rows: Vec<(&str, &str)> = PLUGIN_ROUTES
        .iter()
        .filter(|(_, _, plugin)| *plugin == "sso")
        .map(|(method, path, _)| (*method, *path))
        .collect();
    assert_eq!(
        rows,
        [
            ("get", "/sso/admin/oidc/providers"),
            ("put", "/sso/admin/oidc/providers/{provider}"),
            ("delete", "/sso/admin/oidc/providers/{provider}"),
        ]
    );
    for (method, path) in rows {
        assert!(!REAL_ROUTES.contains(&(method, path)), "{path}");
        assert!(
            !EXTENSION_ROUTES
                .iter()
                .any(|(m, p, _)| *m == method && *p == path),
            "{path}"
        );
        let path = path.replace("{provider}", "pocket");
        let body = if method == "put" { PAGE_BODY } else { "" };
        let (status, _) = send(
            as_admin(FakeSso::new(true)),
            request(&method.to_ascii_uppercase(), &path, body, true),
        )
        .await;
        assert!(status.is_success(), "{method} {path}: {status}");
    }
}
