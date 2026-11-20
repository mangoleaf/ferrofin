//! Handler tests for the Single Sign-On plugin's routes (`handlers::sso`):
//! the provider settings, `GET /sso/admin/oidc/providers` and
//! `PUT`/`DELETE /sso/admin/oidc/providers/{provider}`, and the sign-in,
//! `GET /sso/oidc/{provider}/start`, `GET /sso/oidc/{provider}/callback` and
//! `POST /sso/oidc/exchange`.
//!
//! Each test drives a route through the real router against a [`FakeSso`]
//! manager, asserting the status, the body and whether the manager was
//! reached. The caller is an API key (an administrator), a plain user, or
//! nobody.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    authed_state_with_plugins, fake_state, user_authed_state_with_plugins,
};
use ferrofin_model::dto::{SessionInfoDto, UserDto};
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::{
    OidcAdminLink, OidcDiscovery, OidcLinkProvider, OidcProvider, OidcProviderInfo,
    OidcProviderList, OidcUserLinks,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::plugins::DisabledPluginManager;
use ferrofin_traits::session::{AuthenticationRequest, AuthenticationResultData};
use ferrofin_traits::sso::{
    OIDC_FLOW_COOKIE, OidcCallback, OidcExchangeRefusal, OidcExchangeReply, OidcLinkAt, SsoManager,
    SsoReply,
};
use tower::ServiceExt;
use uuid::Uuid;

const SECRET: &str = "route-client-secret-31";

/// Where [`FakeSso`] sends a started sign-in.
const AUTHORIZE: &str = "https://id.example.org/authorize?state=s1";

/// An [`SsoManager`] fake: enabled or not, refusing a save with `refusal`
/// when set, answering a discovery test with `discovery_error` when set, and
/// keeping what it was given.
#[derive(Default)]
struct FakeSso {
    enabled: bool,
    refusal: Option<String>,
    discovery_error: Option<String>,
    saved: Mutex<Vec<(String, OidcProvider)>>,
    tested: Mutex<Vec<OidcProvider>>,
    deleted: Mutex<Vec<String>>,
    listed: Mutex<usize>,
    started: Mutex<Vec<(String, IpAddr)>>,
    finished: Mutex<Vec<(String, OidcCallback)>>,
    exchanged: Mutex<Vec<(Secret, Option<Secret>, AuthenticationRequest)>>,
    /// Each link call: its name and the account it named.
    links: Mutex<Vec<(String, Uuid)>>,
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

    fn tested(&self) -> Vec<OidcProvider> {
        self.tested.lock().expect("tested").clone()
    }

    fn started(&self) -> Vec<(String, IpAddr)> {
        self.started.lock().expect("started").clone()
    }

    fn finished(&self) -> Vec<(String, OidcCallback)> {
        self.finished.lock().expect("finished").clone()
    }

    fn exchanged(&self) -> Vec<(Secret, Option<Secret>, AuthenticationRequest)> {
        self.exchanged.lock().expect("exchanged").clone()
    }

    /// Whether any route reached the manager past its enabled check.
    fn untouched(&self) -> bool {
        self.saved().is_empty()
            && self.tested().is_empty()
            && self.deleted().is_empty()
            && *self.listed.lock().expect("listed") == 0
            && self.started().is_empty()
            && self.finished().is_empty()
            && self.exchanged().is_empty()
            && self.link_calls().is_empty()
    }

    fn link_calls(&self) -> Vec<(String, Uuid)> {
        self.links.lock().expect("links").clone()
    }

    /// Records the link call `call` for `user`.
    fn record(&self, call: String, user: Uuid) {
        self.links.lock().expect("links").push((call, user));
    }
}

/// The account [`FakeSso`] says another account has the identity of.
const TAKEN: &str = "taken";

/// What [`FakeSso`] answers an admin's link with.
fn admin_link(user_id: Uuid, provider: &str, subject: &str) -> OidcAdminLink {
    OidcAdminLink {
        user_id,
        user_name: "alice".to_owned(),
        single_sign_on_account: false,
        provider: Some(provider.to_owned()),
        provider_name: Some("Pocket ID".to_owned()),
        issuer: "https://id.example.org".to_owned(),
        subject: subject.to_owned(),
        date_created: chrono::DateTime::UNIX_EPOCH,
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
        discovery: None,
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

    async fn test_oidc_provider(
        &self,
        provider: &OidcProvider,
    ) -> Result<OidcDiscovery, ServiceError> {
        self.ensure_enabled().await?;
        self.tested.lock().expect("tested").push(provider.clone());
        Ok(match &self.discovery_error {
            Some(why) => OidcDiscovery {
                error: Some(why.clone()),
                ..OidcDiscovery::default()
            },
            None => OidcDiscovery {
                authorization_endpoint: Some(format!("{}/authorize", provider.issuer)),
                jwks_uri: Some(format!("{}/jwks", provider.issuer)),
                signing_keys: Some(1),
                ..OidcDiscovery::default()
            },
        })
    }

    async fn start_oidc_sign_in(
        &self,
        key: &str,
        client: IpAddr,
    ) -> Result<SsoReply, ServiceError> {
        self.ensure_enabled().await?;
        if key != "pocket" {
            return Err(ServiceError::not_found(format!("OIDC provider {key}")));
        }
        self.started
            .lock()
            .expect("started")
            .push((key.to_owned(), client));
        Ok(SsoReply {
            status: 302,
            location: Some(AUTHORIZE.to_owned()),
            html: String::new(),
            set_cookies: vec![format!("{OIDC_FLOW_COOKIE}=b1; Path=/sso; HttpOnly")],
            content_security_policy: None,
        })
    }

    async fn finish_oidc_sign_in(
        &self,
        key: &str,
        callback: OidcCallback,
    ) -> Result<SsoReply, ServiceError> {
        self.ensure_enabled().await?;
        if key != "pocket" {
            return Err(ServiceError::not_found(format!("OIDC provider {key}")));
        }
        self.finished
            .lock()
            .expect("finished")
            .push((key.to_owned(), callback));
        Ok(SsoReply {
            status: 400,
            location: None,
            html: "<p>Sign-in failed. Try again.</p>".to_owned(),
            set_cookies: vec![format!("{OIDC_FLOW_COOKIE}=; Path=/sso; Max-Age=0")],
            content_security_policy: Some("default-src 'none'".to_owned()),
        })
    }

    async fn exchange_oidc_sign_in(
        &self,
        code: Secret,
        flow_cookie: Option<Secret>,
        request: AuthenticationRequest,
    ) -> Result<OidcExchangeReply, ServiceError> {
        self.ensure_enabled().await?;
        self.exchanged
            .lock()
            .expect("exchanged")
            .push((code.clone(), flow_cookie, request));
        let outcome = match code.expose() {
            "good" => Ok(signed_in()),
            "disabled" => Err(OidcExchangeRefusal {
                status: 403,
                message: "The pocket account is currently disabled.".to_owned(),
            }),
            _ => Err(OidcExchangeRefusal {
                status: 400,
                message: "Sign-in failed. Try again.".to_owned(),
            }),
        };
        Ok(OidcExchangeReply {
            outcome,
            set_cookies: vec![format!("{OIDC_FLOW_COOKIE}=; Path=/sso; Max-Age=0")],
        })
    }

    async fn oidc_link_page(&self) -> Result<SsoReply, ServiceError> {
        self.ensure_enabled().await?;
        Ok(SsoReply {
            status: 200,
            location: None,
            html: "<p id=\"status\">Loading…</p>".to_owned(),
            set_cookies: Vec::new(),
            content_security_policy: Some("default-src 'none'; script-src 'sha256-x'".to_owned()),
        })
    }

    async fn oidc_links(&self, user_id: Uuid) -> Result<OidcUserLinks, ServiceError> {
        self.ensure_enabled().await?;
        self.record("links".to_owned(), user_id);
        Ok(OidcUserLinks {
            single_sign_on_account: true,
            links: Vec::new(),
            providers: vec![OidcLinkProvider {
                key: "pocket".to_owned(),
                name: "Pocket ID".to_owned(),
            }],
        })
    }

    async fn start_oidc_link(
        &self,
        key: &str,
        user_id: Uuid,
        client: IpAddr,
    ) -> Result<SsoReply, ServiceError> {
        self.ensure_enabled().await?;
        if key != "pocket" {
            return Err(ServiceError::not_found(format!("OIDC provider {key}")));
        }
        self.record(format!("start {key} {client}"), user_id);
        Ok(SsoReply {
            status: 302,
            location: Some(AUTHORIZE.to_owned()),
            html: String::new(),
            set_cookies: vec![format!("{OIDC_FLOW_COOKIE}=l1; Path=/sso; HttpOnly")],
            content_security_policy: None,
        })
    }

    async fn all_oidc_links(&self) -> Result<Vec<OidcAdminLink>, ServiceError> {
        self.ensure_enabled().await?;
        self.record("all".to_owned(), Uuid::nil());
        Ok(vec![admin_link(ALICE, "pocket", "alice-sub")])
    }

    async fn add_oidc_link(
        &self,
        user_id: Uuid,
        key: &str,
        subject: &str,
    ) -> Result<OidcAdminLink, ServiceError> {
        self.ensure_enabled().await?;
        self.record(format!("add {key} {subject}"), user_id);
        if subject == TAKEN {
            return Err(ServiceError::Conflict(
                "another account is linked to this identity: remove that link first".to_owned(),
            ));
        }
        Ok(admin_link(user_id, key, subject))
    }

    async fn remove_oidc_link(
        &self,
        user_id: Uuid,
        at: OidcLinkAt<'_>,
    ) -> Result<(), ServiceError> {
        self.ensure_enabled().await?;
        self.record(format!("remove {at:?}"), user_id);
        match at {
            OidcLinkAt::Provider("pocket") | OidcLinkAt::Issuer("https://id.example.org") => Ok(()),
            _ => Err(ServiceError::not_found("no such OIDC link")),
        }
    }
}

/// An account an admin names.
const ALICE: Uuid = Uuid::from_u128(0xA11CE);

/// The plain user [`as_user`] signs in as (`test_support::sample_user`).
const SIGNED_IN: Uuid = Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111);

/// The session [`FakeSso`] answers the exchange code `good` with.
fn signed_in() -> AuthenticationResultData {
    AuthenticationResultData {
        user: UserDto {
            name: Some("pocket".to_owned()),
            server_id: Some("server-1".to_owned()),
            ..UserDto::default()
        },
        session: SessionInfoDto {
            user_name: Some("pocket".to_owned()),
            server_id: Some("server-1".to_owned()),
            ..SessionInfoDto::default()
        },
        access_token: Secret::new("minted-token-0001"),
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
    "UsernameClaim":"preferred_username","CreateUsers":true,
    "ClientSecret":"route-client-secret-31"}"#;

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
    assert!(provider.get("Discovery").is_none(), "{body}");
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
async fn a_save_tests_the_saved_provider_and_answers_what_it_found() {
    let fake = FakeSso::new(true);
    let (status, body) = send(
        as_admin(fake.clone()),
        request("PUT", "/sso/admin/oidc/providers/pocket", PAGE_BODY, true),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&body);
    assert_eq!(
        body["Discovery"],
        serde_json::json!({
            "AuthorizationEndpoint": "https://id.example.org/authorize",
            "JwksUri": "https://id.example.org/jwks",
            "SigningKeys": 1,
        })
    );
    // The provider as saved is tested, under the key of the path.
    let tested = fake.tested();
    assert_eq!(tested.len(), 1);
    assert_eq!(tested[0].key, "pocket");
    assert_eq!(tested[0].issuer, "https://id.example.org");
    assert_eq!(tested[0].client_secret, None);
}

#[tokio::test]
async fn a_failed_test_still_saves_and_answers_ok_with_the_reason() {
    let fake = Arc::new(FakeSso {
        enabled: true,
        discovery_error: Some("could not connect to the identity provider".to_owned()),
        ..FakeSso::default()
    });
    let (status, body) = send(
        as_admin(fake.clone()),
        request("PUT", "/sso/admin/oidc/providers/pocket", PAGE_BODY, true),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&body);
    assert_eq!(body["Key"], "pocket");
    assert_eq!(
        body["Discovery"],
        serde_json::json!({"Error": "could not connect to the identity provider"})
    );
    assert_eq!(fake.saved().len(), 1);
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
        as_admin(fake.clone()),
        request("PUT", "/sso/admin/oidc/providers/pocket", PAGE_BODY, true),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json(&body)["error"],
        "invalid input: the provider name is required"
    );
    assert!(fake.tested().is_empty(), "a refused provider is not tested");
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
            ("get", "/sso/oidc/{provider}/start"),
            ("get", "/sso/oidc/{provider}/callback"),
            ("post", "/sso/oidc/exchange"),
            ("get", "/sso/link"),
            ("get", "/sso/links"),
            ("delete", "/sso/links/{provider}"),
            ("post", "/sso/oidc/{provider}/link"),
            ("get", "/sso/admin/links"),
            ("post", "/sso/admin/links"),
            ("delete", "/sso/admin/links"),
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
        let mut path = path.replace("{provider}", "pocket");
        if (method, path.as_str()) == ("delete", "/sso/admin/links") {
            path.push_str("?userId=000000000000000000000000000a11ce&provider=pocket");
        }
        let body = match (method, path.as_str()) {
            ("put", _) => PAGE_BODY,
            ("post", "/sso/oidc/exchange") => r#"{"Code":"good"}"#,
            ("post", "/sso/admin/links") => ADD_BODY,
            _ => "",
        };
        // The user's link routes need a user, and the admin's an admin.
        let state = if path.starts_with("/sso/links") || path.ends_with("/link") {
            as_user(FakeSso::new(true))
        } else {
            as_admin(FakeSso::new(true))
        };
        let (status, _) = send(
            state,
            request(&method.to_ascii_uppercase(), &path, body, true),
        )
        .await;
        // The fake's sign-in answers a redirect and a refusal page.
        let expected = match path.as_str() {
            "/sso/oidc/pocket/start" => StatusCode::FOUND,
            "/sso/oidc/pocket/callback" => StatusCode::BAD_REQUEST,
            _ if method == "delete" => StatusCode::NO_CONTENT,
            _ => StatusCode::OK,
        };
        assert_eq!(status, expected, "{method} {path}");
    }
}

/// The sign-in routes: what a browser gets.
async fn respond(state: AppState, request: Request<Body>) -> axum::response::Response {
    create_router(state)
        .oneshot(request)
        .await
        .expect("response")
}

/// A header of `response`, as text.
fn header<'r>(response: &'r axum::response::Response, name: &str) -> Vec<&'r str> {
    response
        .headers()
        .get_all(name)
        .iter()
        .map(|value| value.to_str().expect("text"))
        .collect()
}

#[tokio::test]
async fn start_redirects_an_anonymous_browser_with_the_flow_cookie() {
    let fake = FakeSso::new(true);
    let response = respond(
        fake_state().with_sso(fake.clone()),
        request("GET", "/sso/oidc/pocket/start", "", false),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(header(&response, "location"), [AUTHORIZE]);
    assert_eq!(
        header(&response, "set-cookie"),
        ["ferrofin_sso_flow=b1; Path=/sso; HttpOnly"]
    );
    assert_eq!(header(&response, "cache-control"), ["no-store"]);
    // No connection address in a test router: the loopback, as for every
    // other route.
    assert_eq!(
        fake.started(),
        [("pocket".to_owned(), IpAddr::from([127, 0, 0, 1]))]
    );
}

#[tokio::test]
async fn the_callback_hands_its_parameters_and_the_flow_cookie_to_the_plugin() {
    let fake = FakeSso::new(true);
    let callback = Request::builder()
        .uri(
            "/sso/oidc/pocket/callback?state=s1&session_state=x&code=c1\
             &iss=https%3A%2F%2Fid.example.org",
        )
        .header("cookie", "theme=dark; ferrofin_sso_flow=b1")
        .header("cookie", "other=2")
        .body(Body::empty())
        .expect("request");
    let response = respond(fake_state().with_sso(fake.clone()), callback).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        header(&response, "content-type"),
        ["text/html; charset=utf-8"]
    );
    assert_eq!(
        header(&response, "set-cookie"),
        ["ferrofin_sso_flow=; Path=/sso; Max-Age=0"]
    );
    assert_eq!(header(&response, "cache-control"), ["no-store"]);
    assert_eq!(header(&response, "referrer-policy"), ["no-referrer"]);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    assert_eq!(&body[..], b"<p>Sign-in failed. Try again.</p>");

    let (key, callback) = fake.finished().remove(0);
    assert_eq!(key, "pocket");
    assert_eq!(callback.state.as_deref(), Some("s1"));
    assert_eq!(callback.code.as_ref().map(Secret::expose), Some("c1"));
    assert_eq!(callback.iss.as_deref(), Some("https://id.example.org"));
    assert_eq!(callback.error, None);
    assert_eq!(
        callback.flow_cookie.as_ref().map(Secret::expose),
        Some("b1")
    );
}

#[tokio::test]
async fn an_error_answer_reaches_the_plugin_and_a_repeated_parameter_voids_them_all() {
    let fake = FakeSso::new(true);
    let (status, _) = send(
        fake_state().with_sso(fake.clone()),
        request(
            "GET",
            "/sso/oidc/pocket/callback?error=access_denied&state=s1",
            "",
            false,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, callback) = fake.finished().remove(0);
    assert_eq!(callback.error.as_deref(), Some("access_denied"));
    assert_eq!(callback.state.as_deref(), Some("s1"));
    assert!(callback.code.is_none());
    assert!(callback.flow_cookie.is_none());

    for repeated in [
        "state=s2",
        "code=c2",
        "iss=https%3A%2F%2Fevil.example.org",
        "error=x",
    ] {
        let fake = FakeSso::new(true);
        let callback = Request::builder()
            .uri(format!(
                "/sso/oidc/pocket/callback?state=s1&code=c1&iss=https%3A%2F%2Fid.example.org\
                 &error=e1&{repeated}"
            ))
            .header("cookie", "ferrofin_sso_flow=b1")
            .body(Body::empty())
            .expect("request");
        let response = respond(fake_state().with_sso(fake.clone()), callback).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{repeated}");
        let (_, callback) = fake.finished().remove(0);
        assert!(
            callback.state.is_none()
                && callback.code.is_none()
                && callback.iss.is_none()
                && callback.error.is_none(),
            "{repeated}: {callback:?}"
        );
        assert!(callback.flow_cookie.is_some(), "{repeated}");
    }
}

/// B1 for the sign-in routes, and an unknown or undecodable provider: no
/// such route.
#[tokio::test]
async fn sign_in_answers_not_found_while_disabled_or_for_an_unknown_provider() {
    for path in [
        "/sso/oidc/pocket/start",
        "/sso/oidc/pocket/callback?state=s",
    ] {
        let (status, _) = send(
            fake_state().with_sso(FakeSso::new(false)),
            request("GET", path, "", false),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        let (status, _) = send(
            authed_state_with_plugins(Arc::new(DisabledPluginManager)),
            request("GET", path, "", true),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "no extension: {path}");
    }
    for path in [
        "/sso/oidc/nope/start",
        "/sso/oidc/nope/callback",
        "/sso/oidc/%FF/start",
        "/sso/oidc/%FF/callback",
    ] {
        let fake = FakeSso::new(true);
        let (status, _) = send(
            fake_state().with_sso(fake.clone()),
            request("GET", path, "", false),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(fake.untouched(), "{path}");
    }
}

/// A `POST /sso/oidc/exchange` with the completion page's body and `cookie`.
fn exchange(code: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/sso/oidc/exchange")
        .header("content-type", "application/json");
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    builder
        .body(Body::from(format!(r#"{{"Code":"{code}"}}"#)))
        .expect("request")
}

#[tokio::test]
async fn the_exchange_hands_the_code_cookie_and_caller_to_the_plugin_and_answers_the_session() {
    let fake = FakeSso::new(true);
    let response = respond(
        fake_state().with_sso(fake.clone()),
        exchange("good", Some("theme=dark; ferrofin_sso_flow=b1")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "set-cookie"),
        ["ferrofin_sso_flow=; Path=/sso; Max-Age=0"]
    );
    assert_eq!(header(&response, "cache-control"), ["no-store"]);
    assert_eq!(header(&response, "x-content-type-options"), ["nosniff"]);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    // `POST /Users/AuthenticateByName`'s `AuthenticationResult`.
    let body = json(&body);
    assert_eq!(body["AccessToken"], "minted-token-0001");
    assert_eq!(body["ServerId"], "server-1");
    assert_eq!(body["User"]["Name"], "pocket");
    assert_eq!(body["SessionInfo"]["UserName"], "pocket");

    let (code, cookie, request) = fake.exchanged().remove(0);
    assert_eq!(code.expose(), "good");
    assert_eq!(cookie.as_ref().map(Secret::expose), Some("b1"));
    // The caller's address, as the password sign-in resolves it; the device
    // is the auth context's, which a fake state leaves empty.
    assert_eq!(request.remote_endpoint.as_deref(), Some("127.0.0.1"));
    assert_eq!(request.user_id, None);
    assert!(request.password.is_none());
}

#[tokio::test]
async fn a_refused_exchange_is_the_plain_sentence_the_page_shows_and_clears_the_cookie() {
    for (code, status, message) in [
        (
            "disabled",
            StatusCode::FORBIDDEN,
            "The pocket account is currently disabled.",
        ),
        (
            "used",
            StatusCode::BAD_REQUEST,
            "Sign-in failed. Try again.",
        ),
    ] {
        let response = respond(
            fake_state().with_sso(FakeSso::new(true)),
            exchange(code, None),
        )
        .await;
        assert_eq!(response.status(), status, "{code}");
        assert_eq!(
            header(&response, "content-type"),
            ["text/plain; charset=utf-8"]
        );
        assert_eq!(
            header(&response, "set-cookie"),
            ["ferrofin_sso_flow=; Path=/sso; Max-Age=0"]
        );
        assert_eq!(header(&response, "cache-control"), ["no-store"]);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(&body[..], message.as_bytes(), "{code}");
    }
}

#[tokio::test]
async fn the_exchange_answers_not_found_while_disabled_and_refuses_a_body_that_does_not_bind() {
    let fake = FakeSso::new(false);
    let (status, _) = send(fake_state().with_sso(fake.clone()), exchange("good", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        authed_state_with_plugins(Arc::new(DisabledPluginManager)),
        exchange("good", None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "no extension");
    assert!(fake.untouched());

    let fake = FakeSso::new(true);
    let (status, _) = send(
        fake_state().with_sso(fake.clone()),
        Request::builder()
            .method("POST")
            .uri("/sso/oidc/exchange")
            .header("content-type", "application/json")
            .body(Body::from("not json"))
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(fake.untouched());
}

#[tokio::test]
async fn a_page_runs_only_what_its_policy_names_and_is_never_sniffed() {
    let callback = Request::builder()
        .uri("/sso/oidc/pocket/callback?state=s1")
        .body(Body::empty())
        .expect("request");
    let response = respond(fake_state().with_sso(FakeSso::new(true)), callback).await;
    assert_eq!(
        header(&response, "content-security-policy"),
        ["default-src 'none'"]
    );
    assert_eq!(header(&response, "x-content-type-options"), ["nosniff"]);
    // A redirect has no policy of its own.
    let start = respond(
        fake_state().with_sso(FakeSso::new(true)),
        request("GET", "/sso/oidc/pocket/start", "", false),
    )
    .await;
    assert_eq!(
        header(&start, "content-security-policy"),
        Vec::<&str>::new()
    );
}

/// What the settings page posts to link `ALICE` by hand.
const ADD_BODY: &str =
    r#"{"UserId":"000000000000000000000000000a11ce","Provider":"pocket","Subject":"alice-sub"}"#;

/// The user's own link routes, as the link page calls them.
const OWN_REQUESTS: [(&str, &str); 3] = [
    ("GET", "/sso/links"),
    ("POST", "/sso/oidc/pocket/link"),
    ("DELETE", "/sso/links/pocket"),
];

/// The admin's link routes, as the settings page calls them.
const ADMIN_REQUESTS: [(&str, &str, &str); 3] = [
    ("GET", "/sso/admin/links", ""),
    ("POST", "/sso/admin/links", ADD_BODY),
    (
        "DELETE",
        "/sso/admin/links?userId=000000000000000000000000000a11ce&provider=pocket",
        "",
    ),
];

/// A user's route acts on the account the credentials name: a `userId` in
/// the query names nobody.
#[tokio::test]
async fn a_user_reads_and_removes_only_their_own_links() {
    let fake = FakeSso::new(true);
    let (status, body) = send(
        as_user(fake.clone()),
        request(
            "GET",
            "/sso/links?userId=000000000000000000000000000a11ce",
            "",
            true,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&body);
    assert_eq!(body["SingleSignOnAccount"], true);
    assert_eq!(body["Providers"][0]["Key"], "pocket");
    let (status, _) = send(
        as_user(fake.clone()),
        request(
            "DELETE",
            "/sso/links/pocket?userId=000000000000000000000000000a11ce",
            "",
            true,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(
        as_user(fake.clone()),
        request("DELETE", "/sso/links/nope", "", true),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        fake.link_calls(),
        [
            ("links".to_owned(), SIGNED_IN),
            (r#"remove Provider("pocket")"#.to_owned(), SIGNED_IN),
            (r#"remove Provider("nope")"#.to_owned(), SIGNED_IN),
        ]
    );
}

#[tokio::test]
async fn a_link_starts_for_the_signed_in_user_and_answers_where_to_send_the_browser() {
    let fake = FakeSso::new(true);
    let response = respond(
        as_user(fake.clone()),
        request("POST", "/sso/oidc/pocket/link", "", true),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "set-cookie"),
        ["ferrofin_sso_flow=l1; Path=/sso; HttpOnly"]
    );
    assert_eq!(header(&response, "cache-control"), ["no-store"]);
    assert_eq!(header(&response, "location"), Vec::<&str>::new());
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    assert_eq!(json(&body), serde_json::json!({ "Url": AUTHORIZE }));
    assert_eq!(
        fake.link_calls(),
        [("start pocket 127.0.0.1".to_owned(), SIGNED_IN)]
    );

    for path in ["/sso/oidc/nope/link", "/sso/oidc/%FF/link"] {
        let fake = FakeSso::new(true);
        let (status, _) = send(as_user(fake.clone()), request("POST", path, "", true)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}

/// An API key names no account to link.
#[tokio::test]
async fn an_api_key_has_no_links_of_its_own() {
    for (method, path) in OWN_REQUESTS {
        let fake = FakeSso::new(true);
        let (status, _) = send(as_admin(fake.clone()), request(method, path, "", true)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        assert!(fake.untouched(), "{method} {path}");
    }
}

/// The link page is a document anyone opens; the links behind it are not.
#[tokio::test]
async fn an_anonymous_caller_gets_the_link_page_but_no_links() {
    let response = respond(
        fake_state().with_sso(FakeSso::new(true)),
        request("GET", "/sso/link", "", false),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "content-security-policy"),
        ["default-src 'none'; script-src 'sha256-x'"]
    );
    assert_eq!(header(&response, "x-content-type-options"), ["nosniff"]);
    assert_eq!(header(&response, "cache-control"), ["no-store"]);
    for (method, path) in OWN_REQUESTS {
        let fake = FakeSso::new(true);
        let (status, _) = send(
            fake_state().with_sso(fake.clone()),
            request(method, path, "", false),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
        assert!(fake.untouched(), "{method} {path}");
    }
}

#[tokio::test]
async fn a_plain_user_cannot_reach_the_admin_links() {
    for (method, path, body) in ADMIN_REQUESTS {
        let fake = FakeSso::new(true);
        let (status, _) = send(as_user(fake.clone()), request(method, path, body, true)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        assert!(fake.untouched(), "{method} {path}");
    }
}

#[tokio::test]
async fn an_admin_lists_adds_and_removes_any_link() {
    let fake = FakeSso::new(true);
    let (status, body) = send(
        as_admin(fake.clone()),
        request("GET", "/sso/admin/links", "", true),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed = json(&body);
    assert_eq!(listed[0]["UserId"], "000000000000000000000000000a11ce");
    assert_eq!(listed[0]["UserName"], "alice");
    assert_eq!(listed[0]["Provider"], "pocket");
    assert_eq!(listed[0]["Subject"], "alice-sub");
    assert_eq!(listed[0]["SingleSignOnAccount"], false);
    assert!(listed[0]["DateCreated"].is_string(), "{listed}");

    let (status, body) = send(
        as_admin(fake.clone()),
        request("POST", "/sso/admin/links", ADD_BODY, true),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body)["Subject"], "alice-sub");
    let taken = ADD_BODY.replace("alice-sub", TAKEN);
    let (status, body) = send(
        as_admin(fake.clone()),
        request("POST", "/sso/admin/links", &taken, true),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        json(&body)["error"]
            .as_str()
            .is_some_and(|e| e.contains("remove that link first"))
    );

    for at in ["provider=pocket", "issuer=https%3A%2F%2Fid.example.org"] {
        let (status, _) = send(
            as_admin(fake.clone()),
            request(
                "DELETE",
                &format!("/sso/admin/links?userId=000000000000000000000000000a11ce&{at}"),
                "",
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{at}");
    }
    assert_eq!(
        fake.link_calls(),
        [
            ("all".to_owned(), Uuid::nil()),
            ("add pocket alice-sub".to_owned(), ALICE),
            (format!("add pocket {TAKEN}"), ALICE),
            (r#"remove Provider("pocket")"#.to_owned(), ALICE),
            (
                r#"remove Issuer("https://id.example.org")"#.to_owned(),
                ALICE
            ),
        ]
    );
}

/// A removal names one account and exactly one of the provider and the
/// issuer; a body that does not bind is a validation problem.
#[tokio::test]
async fn an_admin_request_that_names_no_link_is_a_bad_request() {
    for query in [
        "?provider=pocket",
        "?userId=000000000000000000000000000a11ce",
        "?userId=000000000000000000000000000a11ce&provider=pocket&issuer=https%3A%2F%2Fx",
        "?userId=not-a-guid&provider=pocket",
    ] {
        let fake = FakeSso::new(true);
        let (status, _) = send(
            as_admin(fake.clone()),
            request("DELETE", &format!("/sso/admin/links{query}"), "", true),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
        assert!(fake.untouched(), "{query}");
    }
    let fake = FakeSso::new(true);
    let (status, body) = send(
        as_admin(fake.clone()),
        request("POST", "/sso/admin/links", r#"{"UserId":5}"#, true),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json(&body)["title"],
        "One or more validation errors occurred."
    );
    assert!(fake.untouched());
}

/// B1 for the link routes: no such route, for any caller and any body.
#[tokio::test]
async fn the_link_routes_answer_not_found_while_disabled() {
    let mut requests: Vec<(&str, &str, &str)> = ADMIN_REQUESTS.to_vec();
    requests.push(("GET", "/sso/link", ""));
    requests.extend(
        OWN_REQUESTS
            .iter()
            .map(|&(method, path)| (method, path, "")),
    );
    for (method, path, body) in requests {
        for (state, token) in [
            (as_admin(FakeSso::new(false)), true),
            (as_user(FakeSso::new(false)), true),
            (fake_state().with_sso(FakeSso::new(false)), false),
            (
                authed_state_with_plugins(Arc::new(DisabledPluginManager)),
                true,
            ),
        ] {
            let (status, _) = send(state, request(method, path, body, token)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
        }
    }
}
