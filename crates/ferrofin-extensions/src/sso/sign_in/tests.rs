//! OIDC sign-in against the fake identity provider: what start sends the
//! browser to, a callback that passes every check, and one test for each way
//! a callback is refused, each asserting its reason code.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::time::Duration;

use ferrofin_core::FerrofinPluginManager;
use ferrofin_model::secret::Secret;
use ferrofin_traits::plugins::PluginManager;
use ferrofin_traits::sso::SsoManager;
use serde_json::json;

use super::*;
use crate::Extension;
use crate::sso::fake_idp::{
    FakeIdentityProvider, FakeResponse, FakeSignature, JWKS_PATH, KEY_ID, SUBJECT, TOKEN_PATH,
    USERNAME,
};
use crate::sso::{EXTENSION_ID, SsoExtension};

const SECRET: &str = "fake-client-secret-91c2";
const CLIENT_ID: &str = "ferrofin";
const PUBLIC_URL: &str = "https://media.example.org/jf";
const CALLBACK: &str = "https://media.example.org/jf/sso/oidc/fake/callback";
const CLIENT: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7));

/// The service with the plugin enabled and the provider `fake` saved at the
/// fake identity provider, and what they run on.
struct Harness {
    sso: SsoService,
    fake: FakeIdentityProvider,
    _dir: tempfile::TempDir,
}

/// A sign-in started: where the browser was sent, and its flow cookie.
struct Started {
    location: String,
    cookie: String,
}

/// The provider `fake` as saved, `edit`ed.
fn fake_provider(
    fake: &FakeIdentityProvider,
    edit: impl FnOnce(&mut OidcProvider),
) -> OidcProvider {
    let mut provider = OidcProvider {
        name: "Fake <IdP>".to_owned(),
        issuer: fake.issuer(),
        allow_http_issuer: true,
        client_id: CLIENT_ID.to_owned(),
        client_secret: Some(Secret::new(SECRET)),
        ..OidcProvider::default()
    };
    edit(&mut provider);
    provider
}

impl Harness {
    async fn new() -> Self {
        Self::with(Some(PUBLIC_URL), |_| {}).await
    }

    async fn with(published_url: Option<&str>, edit: impl FnOnce(&mut OidcProvider)) -> Self {
        let fake = FakeIdentityProvider::start().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let extensions: Vec<Arc<dyn Extension>> = vec![Arc::new(SsoExtension)];
        let plugins = Arc::new(FerrofinPluginManager::new(
            crate::registered_plugins(&extensions),
            dir.path().to_path_buf(),
        ));
        plugins.enable_plugin(EXTENSION_ID).await.expect("enable");
        let sso = SsoService::new(
            plugins as Arc<dyn PluginManager>,
            published_url.map(str::to_owned),
        );
        sso.save_oidc_provider("fake", fake_provider(&fake, edit))
            .await
            .expect("the provider saves");
        Self {
            sso,
            fake,
            _dir: dir,
        }
    }

    async fn provider(&self, key: &str) -> OidcProvider {
        sign_in_provider(&self.sso.config().await.expect("config"), key).expect("a provider")
    }

    async fn start(&self) -> Started {
        let reply = self
            .sso
            .start_oidc_sign_in("fake", CLIENT)
            .await
            .expect("starts");
        assert_eq!(reply.status, 302, "{reply:?}");
        let cookie = reply.set_cookies[0]
            .strip_prefix("ferrofin_sso_flow=")
            .and_then(|rest| rest.split(';').next())
            .expect("the flow cookie")
            .to_owned();
        Started {
            location: reply.location.expect("a redirect"),
            cookie,
        }
    }

    /// Where the fake's authorization endpoint sends the browser back to,
    /// as the callback route reads it, with `started`'s cookie.
    async fn callback(&self, started: &Started) -> OidcCallback {
        let url = self.fake.authorize(&started.location).await;
        assert!(url.starts_with(&format!("{CALLBACK}?")), "{url}");
        callback_of(&url, Some(&started.cookie))
    }

    /// A whole sign-in at `fake`, to the callback's checks.
    async fn sign_in(&self) -> Result<(FlowPurpose, VerifiedIdentity), Refusal> {
        let started = self.start().await;
        let callback = self.callback(&started).await;
        self.sso
            .verify_callback(&self.provider("fake").await, callback)
            .await
    }

    /// The reason a whole sign-in is refused.
    async fn refused(&self) -> &'static str {
        match self.sign_in().await {
            Ok((_, identity)) => panic!("accepted: {identity:?}"),
            Err(refusal) => refusal.reason,
        }
    }

    /// Serves `edit`ed discovery document from now on.
    fn discovery(&self, edit: impl FnOnce(&mut Value)) {
        let mut document = self.fake.discovery_document();
        edit(&mut document);
        self.fake.set(
            &self.fake.path("/.well-known/openid-configuration"),
            FakeResponse::json(&document),
        );
    }

    /// How many times the key set was fetched.
    fn key_set_fetches(&self) -> usize {
        let path = self.fake.path(JWKS_PATH);
        self.fake
            .requests()
            .iter()
            .filter(|request| request.path == path)
            .count()
    }
}

/// The query parameters of `url`.
fn query(url: &str) -> HashMap<String, String> {
    url::Url::parse(url)
        .expect("a URL")
        .query_pairs()
        .into_owned()
        .collect()
}

/// The callback `url` and `cookie` make, as the route reads them.
fn callback_of(url: &str, cookie: Option<&str>) -> OidcCallback {
    let query = query(url);
    OidcCallback {
        state: query.get("state").cloned(),
        code: query.get("code").map(Secret::new),
        iss: query.get("iss").cloned(),
        error: query.get("error").cloned(),
        flow_cookie: cookie.map(Secret::new),
    }
}

#[tokio::test]
async fn start_sends_the_browser_to_the_provider_with_pkce_a_state_and_a_nonce() {
    let harness = Harness::new().await;
    let started = harness.start().await;
    let authorize = format!("{}/protocol/openid-connect/auth?", harness.fake.issuer());
    assert!(
        started.location.starts_with(&authorize),
        "{}",
        started.location
    );
    let query = query(&started.location);
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["client_id"], CLIENT_ID);
    assert_eq!(query["redirect_uri"], CALLBACK);
    assert_eq!(query["scope"], "openid profile");
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["code_challenge"].len(), 43, "a SHA-256, base64url");
    assert!(query["state"].len() >= 22, "{query:?}");
    assert!(query["nonce"].len() >= 22, "{query:?}");

    // The cookie binds the flow to this browser; only its hash is kept.
    let flow = harness
        .sso
        .flows
        .take(&query["state"])
        .expect("a pending flow");
    assert_eq!(flow.provider_key, "fake");
    assert_eq!(flow.redirect_uri, CALLBACK);
    assert_eq!(flow.client, CLIENT);
    assert_eq!(flow.purpose, FlowPurpose::SignIn);
    assert_eq!(flow.nonce.secret(), &query["nonce"]);
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(flow.verifier.secret().as_bytes())),
        query["code_challenge"]
    );
    assert_eq!(
        flow.binding_hash,
        <[u8; 32]>::from(Sha256::digest(started.cookie.as_bytes()))
    );
    assert!(started.cookie.len() >= 43, "32 random bytes");
}

#[tokio::test]
async fn the_cookie_is_http_only_lax_scoped_to_sso_and_secure_on_https() {
    for (published, path, secure) in [
        (PUBLIC_URL, "/jf/sso", "; Secure"),
        ("http://127.0.0.1:8096", "/sso", ""),
    ] {
        let harness = Harness::with(Some(published), |_| {}).await;
        let reply = harness
            .sso
            .start_oidc_sign_in("fake", CLIENT)
            .await
            .expect("starts");
        let cookie = &reply.set_cookies[0];
        let attributes = cookie.split_once(';').expect("attributes").1;
        assert_eq!(
            attributes,
            format!(" Path={path}; Max-Age=600; HttpOnly; SameSite=Lax{secure}"),
            "{published}"
        );
    }
}

#[tokio::test]
async fn a_valid_callback_yields_the_verified_identity() {
    let harness = Harness::new().await;
    let started = harness.start().await;
    let challenge = query(&started.location)["code_challenge"].clone();
    let callback = harness.callback(&started).await;
    let (purpose, identity) = harness
        .sso
        .verify_callback(&harness.provider("fake").await, callback)
        .await
        .expect("verified");
    assert_eq!(purpose, FlowPurpose::SignIn);
    assert_eq!(identity.provider_key, "fake");
    assert_eq!(identity.issuer, harness.fake.issuer());
    assert_eq!(identity.subject, SUBJECT);
    assert_eq!(identity.username.as_deref(), Some(USERNAME));
    assert_eq!(identity.claims["aud"], CLIENT_ID);
    assert!(harness.sso.flows.is_empty(), "the flow is used up");

    // The code was exchanged with the verifier of the challenge, the
    // redirect URI and the client's credentials, once.
    let tokens: Vec<_> = harness
        .fake
        .requests()
        .into_iter()
        .filter(|request| request.path == harness.fake.path(TOKEN_PATH))
        .collect();
    assert_eq!(tokens.len(), 1);
    let form: HashMap<String, String> = url::form_urlencoded::parse(&tokens[0].body)
        .into_owned()
        .collect();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["redirect_uri"], CALLBACK);
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(form["code_verifier"].as_bytes())),
        challenge
    );
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{CLIENT_ID}:{SECRET}"))
    );
    assert!(
        tokens[0]
            .headers
            .iter()
            .any(|(name, value)| name == "authorization" && *value == basic),
        "{:?}",
        tokens[0].headers
    );
    // The ID token held the username: no userinfo call.
    assert!(
        !harness
            .fake
            .requests()
            .iter()
            .any(|request| request.path.ends_with("/userinfo"))
    );
}

#[tokio::test]
async fn the_callback_page_names_the_identity_and_clears_the_cookie() {
    let harness = Harness::new().await;
    let started = harness.start().await;
    let callback = harness.callback(&started).await;
    let reply = harness
        .sso
        .finish_oidc_sign_in("fake", callback)
        .await
        .expect("an answer");
    assert_eq!(reply.status, 200, "{reply:?}");
    assert_eq!(reply.location, None);
    assert!(reply.html.contains(SUBJECT), "{}", reply.html);
    assert!(reply.html.contains(&format!("<strong>{USERNAME}</strong>")));
    assert!(
        reply.html.contains("Fake &lt;IdP&gt;"),
        "escaped: {}",
        reply.html
    );
    assert!(reply.html.contains("No Ferrofin account is linked"));
    assert!(reply.html.contains(r#"<a href="../../../web/">"#));
    assert_eq!(
        reply.set_cookies,
        ["ferrofin_sso_flow=; Path=/jf/sso; Max-Age=0; HttpOnly; SameSite=Lax; Secure"]
    );
}

#[tokio::test]
async fn every_refusal_is_one_generic_page_that_clears_the_cookie() {
    let harness = Harness::new().await;
    let started = harness.start().await;
    let mut callback = harness.callback(&started).await;
    callback.flow_cookie = Some(Secret::new("another-browser"));
    let reply = harness
        .sso
        .finish_oidc_sign_in("fake", callback)
        .await
        .expect("an answer");
    assert_eq!(reply.status, 400);
    assert!(reply.html.contains("<p>Sign-in failed. Try again.</p>"));
    assert!(reply.html.contains(r#"<a href="../../../web/">"#));
    assert!(!reply.html.contains("cookie"), "says nothing of why");
    assert_eq!(
        reply.set_cookies,
        ["ferrofin_sso_flow=; Path=/jf/sso; Max-Age=0; HttpOnly; SameSite=Lax; Secure"]
    );
}

#[tokio::test]
async fn an_unknown_expired_or_replayed_state_is_refused() {
    let harness = Harness::new().await;
    let provider = harness.provider("fake").await;
    let started = harness.start().await;
    let mut callback = harness.callback(&started).await;
    let state = callback.state.clone().expect("a state");

    callback.state = Some("not-a-state".to_owned());
    let refusal = harness
        .sso
        .verify_callback(&provider, callback.clone())
        .await
        .expect_err("refused");
    assert_eq!(refusal.reason, "unknown_state");
    callback.state = None;
    let refusal = harness
        .sso
        .verify_callback(&provider, callback.clone())
        .await
        .expect_err("refused");
    assert_eq!(refusal.reason, "unknown_state");

    // Accepted once; the same callback again finds nothing.
    callback.state = Some(state);
    harness
        .sso
        .verify_callback(&provider, callback.clone())
        .await
        .expect("verified");
    let refusal = harness
        .sso
        .verify_callback(&provider, callback)
        .await
        .expect_err("replayed");
    assert_eq!(refusal.reason, "unknown_state");

    // Older than the flow's lifetime.
    let started = harness.start().await;
    let state = query(&started.location)["state"].clone();
    let mut flow = harness.sso.flows.take(&state).expect("pending");
    flow.created = Instant::now()
        .checked_sub(FLOW_LIFETIME + Duration::from_secs(1))
        .expect("the clock reaches back");
    harness.sso.flows.insert(state, flow);
    let callback = harness.callback(&started).await;
    let refusal = harness
        .sso
        .verify_callback(&provider, callback)
        .await
        .expect_err("expired");
    assert_eq!(refusal.reason, "expired_state");
}

#[tokio::test]
async fn a_missing_or_wrong_cookie_is_refused_and_uses_the_flow_up() {
    let harness = Harness::new().await;
    let provider = harness.provider("fake").await;
    for (cookie, reason) in [(None, "missing_cookie"), (Some("guessed"), "wrong_cookie")] {
        let started = harness.start().await;
        let mut callback = harness.callback(&started).await;
        callback.flow_cookie = cookie.map(Secret::new);
        let refusal = harness
            .sso
            .verify_callback(&provider, callback.clone())
            .await
            .expect_err("refused");
        assert_eq!(refusal.reason, reason);
        // The right cookie after that is too late: the state was used.
        callback.flow_cookie = Some(Secret::new(started.cookie.clone()));
        let refusal = harness
            .sso
            .verify_callback(&provider, callback)
            .await
            .expect_err("used up");
        assert_eq!(refusal.reason, "unknown_state");
    }
}

#[tokio::test]
async fn a_flow_started_at_another_provider_is_refused() {
    let harness = Harness::new().await;
    harness
        .sso
        .save_oidc_provider("other", fake_provider(&harness.fake, |_| {}))
        .await
        .expect("saved");
    let started = harness.start().await;
    let callback = harness.callback(&started).await;
    let refusal = harness
        .sso
        .verify_callback(&harness.provider("other").await, callback)
        .await
        .expect_err("refused");
    assert_eq!(refusal.reason, "wrong_provider");
}

#[tokio::test]
async fn an_error_response_from_the_provider_is_refused() {
    let harness = Harness::new().await;
    harness
        .fake
        .sign_in(|sign_in| sign_in.error = Some("access_denied".to_owned()));
    assert_eq!(harness.refused().await, "provider_error");
}

#[tokio::test]
async fn a_wrong_iss_parameter_is_refused_and_a_missing_one_only_when_advertised() {
    let harness = Harness::new().await;
    harness
        .fake
        .sign_in(|sign_in| sign_in.iss = Some("https://evil.example.org".to_owned()));
    assert_eq!(harness.refused().await, "wrong_iss");

    harness.fake.sign_in(|sign_in| sign_in.iss = None);
    harness.sign_in().await.expect("not advertised: optional");

    harness.discovery(|document| {
        document["authorization_response_iss_parameter_supported"] = json!(true);
    });
    harness.sso.metadata.invalidate("fake");
    assert_eq!(harness.refused().await, "missing_iss");
}

#[tokio::test]
async fn an_error_from_the_token_endpoint_is_refused() {
    let harness = Harness::new().await;
    harness.fake.set(
        &harness.fake.path(TOKEN_PATH),
        FakeResponse {
            status: 400,
            ..FakeResponse::json(&json!({"error": "invalid_grant"}))
        },
    );
    assert_eq!(harness.refused().await, "token_invalid_grant");
    for (error, reason) in [
        ("invalid_client", "token_invalid_client"),
        ("unauthorized_client", "token_error"),
    ] {
        harness.fake.set(
            &harness.fake.path(TOKEN_PATH),
            FakeResponse {
                status: 400,
                ..FakeResponse::json(&json!({ "error": error }))
            },
        );
        assert_eq!(harness.refused().await, reason, "{error}");
    }
}

#[test]
fn the_browser_goes_to_an_http_authorization_endpoint_only_with_the_http_switch() {
    let https = url::Url::parse("https://id.example.org/auth").expect("a URL");
    let http = url::Url::parse("http://id.example.org/auth").expect("a URL");
    let other = url::Url::parse("javascript:alert(1)").expect("a URL");
    assert!(browser_url_allowed(&https, false));
    assert!(!browser_url_allowed(&http, false));
    assert!(browser_url_allowed(&http, true));
    assert!(!browser_url_allowed(&other, true));
}

#[tokio::test]
async fn a_badly_signed_id_token_is_refused() {
    for (signature, advertised, reason) in [
        (
            FakeSignature::Tampered,
            json!(["RS256"]),
            "id_token_signature",
        ),
        (
            FakeSignature::Unsigned,
            json!(["RS256"]),
            "id_token_signature",
        ),
        (
            FakeSignature::Unsigned,
            json!(["none", "RS256"]),
            "id_token_signature",
        ),
        (
            FakeSignature::Unsigned,
            json!(["none"]),
            "id_token_algorithm",
        ),
        // An HMAC keyed with the client secret, even advertised.
        (
            FakeSignature::Hmac(SECRET.to_owned()),
            json!(["RS256", "HS256"]),
            "id_token_signature",
        ),
        (
            FakeSignature::Hmac(SECRET.to_owned()),
            json!(["HS256"]),
            "id_token_algorithm",
        ),
    ] {
        let harness = Harness::new().await;
        harness.discovery(|document| {
            document["id_token_signing_alg_values_supported"] = advertised.clone();
        });
        harness
            .fake
            .sign_in(|sign_in| sign_in.signature = signature.clone());
        assert_eq!(harness.refused().await, reason, "{signature:?}");
    }
}

#[tokio::test]
async fn an_unknown_key_id_fetches_the_keys_again_once() {
    let harness = Harness::new().await;
    harness
        .fake
        .sign_in(|sign_in| sign_in.signature = FakeSignature::KeyId("rotated".to_owned()));
    // Still unknown after one more fetch.
    assert_eq!(harness.refused().await, "id_token_signature");
    assert_eq!(harness.key_set_fetches(), 2);

    // The provider rotated the key in: found on the fetch the unknown id
    // makes, and kept.
    harness.fake.set(
        &harness.fake.path(JWKS_PATH),
        FakeResponse::json(&FakeIdentityProvider::key_set("rotated")),
    );
    harness.sign_in().await.expect("verified with the new key");
    assert_eq!(harness.key_set_fetches(), 3);
    harness.sign_in().await.expect("verified again");
    assert_eq!(harness.key_set_fetches(), 3, "from the cache");
    assert_ne!(KEY_ID, "rotated");
}

#[tokio::test]
async fn an_id_token_with_wrong_claims_is_refused() {
    let now = chrono::Utc::now().timestamp();
    for (claim, value, reason) in [
        ("iss", json!("https://evil.example.org"), "id_token_issuer"),
        ("aud", json!("another-client"), "id_token_audience"),
        // Another audience beside the client's, with no `azp` naming it.
        (
            "aud",
            json!([CLIENT_ID, "another-client"]),
            "id_token_audience",
        ),
        ("azp", json!("another-client"), "id_token_azp"),
        ("exp", json!(now - 60), "id_token_expired"),
        ("nonce", json!("another-nonce"), "id_token_nonce"),
        ("nonce", Value::Null, "id_token_nonce"),
    ] {
        let harness = Harness::new().await;
        harness.fake.sign_in(|sign_in| {
            sign_in.claims.insert(claim.to_owned(), value.clone());
        });
        assert_eq!(harness.refused().await, reason, "{claim}: {value}");
    }
    // An `azp` naming the client is fine, and lets another audience in.
    let harness = Harness::new().await;
    harness.fake.sign_in(|sign_in| {
        sign_in.claims.insert("azp".to_owned(), json!(CLIENT_ID));
    });
    harness.sign_in().await.expect("verified");
    harness.fake.sign_in(|sign_in| {
        sign_in
            .claims
            .insert("aud".to_owned(), json!([CLIENT_ID, "an-api"]));
    });
    harness.sign_in().await.expect("verified with an azp");
}

#[tokio::test]
async fn userinfo_fills_a_missing_username_claim_for_the_same_subject_only() {
    let harness = Harness::with(Some(PUBLIC_URL), |provider| {
        provider.username_claim = "nickname".to_owned();
    })
    .await;
    harness.fake.sign_in(|sign_in| {
        sign_in
            .userinfo
            .insert("nickname".to_owned(), json!("nick"));
        // An ID token claim is never overridden by userinfo.
        sign_in.userinfo.insert("aud".to_owned(), json!("someone"));
    });
    let (_, identity) = harness.sign_in().await.expect("verified");
    assert_eq!(identity.username.as_deref(), Some("nick"));
    assert_eq!(identity.claims["aud"], CLIENT_ID);

    harness.fake.sign_in(|sign_in| {
        sign_in
            .userinfo
            .insert("sub".to_owned(), json!("someone-else"));
    });
    assert_eq!(harness.refused().await, "userinfo_subject");
}

#[tokio::test]
async fn the_secret_goes_in_the_body_when_the_provider_lists_only_client_secret_post() {
    let harness = Harness::new().await;
    harness.discovery(|document| {
        document["token_endpoint_auth_methods_supported"] = json!(["client_secret_post"]);
    });
    harness.sign_in().await.expect("verified");
    let token = harness
        .fake
        .requests()
        .into_iter()
        .find(|request| request.path == harness.fake.path(TOKEN_PATH))
        .expect("a token request");
    let form: HashMap<String, String> = url::form_urlencoded::parse(&token.body)
        .into_owned()
        .collect();
    assert_eq!(form["client_id"], CLIENT_ID);
    assert_eq!(form["client_secret"], SECRET);
    assert!(
        !token
            .headers
            .iter()
            .any(|(name, _)| name == "authorization"),
        "{:?}",
        token.headers
    );
}

#[tokio::test]
async fn a_provider_edited_since_the_start_is_refused() {
    let harness = Harness::new().await;
    let started = harness.start().await;
    harness
        .sso
        .save_oidc_provider(
            "fake",
            fake_provider(&harness.fake, |provider| {
                provider.client_id = "another-client".to_owned();
            }),
        )
        .await
        .expect("saved");
    let callback = harness.callback(&started).await;
    let refusal = harness
        .sso
        .verify_callback(&harness.provider("fake").await, callback)
        .await
        .expect_err("refused");
    assert_eq!(refusal.reason, "provider_changed");
}

#[tokio::test]
async fn a_callback_without_a_code_or_a_token_answer_to_use_is_refused() {
    // No code beside a valid state and cookie.
    let harness = Harness::new().await;
    let started = harness.start().await;
    let mut callback = harness.callback(&started).await;
    callback.code = None;
    let refusal = harness
        .sso
        .verify_callback(&harness.provider("fake").await, callback)
        .await
        .expect_err("refused");
    assert_eq!(refusal, Refusal::bad("missing_code"));

    for (answer, refusal) in [
        // Tokens, but no ID token.
        (
            FakeResponse::json(&json!({"access_token": "a", "token_type": "Bearer"})),
            Refusal::bad("no_id_token"),
        ),
        // Not a token answer at all.
        (
            FakeResponse::status(200).with_body("text/html", b"<html>".to_vec()),
            Refusal::upstream("token_request"),
        ),
        (
            FakeResponse::status(503),
            Refusal::upstream("token_request"),
        ),
    ] {
        let harness = Harness::new().await;
        harness.fake.set(&harness.fake.path(TOKEN_PATH), answer);
        assert_eq!(harness.sign_in().await.expect_err("refused"), refusal);
    }
}

#[tokio::test]
async fn a_failing_userinfo_or_a_lost_client_secret_is_refused() {
    let harness = Harness::with(Some(PUBLIC_URL), |provider| {
        provider.username_claim = "nickname".to_owned();
    })
    .await;
    for answer in [
        FakeResponse::status(500),
        FakeResponse::status(200).with_body("application/jwt", b"e30.e30.".to_vec()),
    ] {
        harness.fake.set(
            &harness.fake.path("/protocol/openid-connect/userinfo"),
            answer,
        );
        assert_eq!(
            harness.sign_in().await.expect_err("refused"),
            Refusal::upstream("userinfo")
        );
    }

    // The private data lost the secret, as a hand edit could do.
    harness
        .sso
        .plugins
        .set_plugin_private_data(EXTENSION_ID, b"{}".to_vec())
        .await
        .expect("written");
    assert_eq!(harness.refused().await, "client_secret");
    // Or it does not read: the server's fault, not the callback's.
    harness
        .sso
        .plugins
        .set_plugin_private_data(EXTENSION_ID, b"not json".to_vec())
        .await
        .expect("written");
    assert_eq!(
        harness.sign_in().await.expect_err("refused"),
        Refusal {
            reason: "private_data",
            status: 500
        }
    );
}

#[tokio::test]
async fn start_without_a_public_url_names_the_setting_and_keeps_no_flow() {
    let harness = Harness::with(None, |_| {}).await;
    let reply = harness
        .sso
        .start_oidc_sign_in("fake", CLIENT)
        .await
        .expect("an answer");
    assert_eq!(reply.status, 503);
    assert!(reply.html.contains("Public URL"), "{}", reply.html);
    assert_eq!(reply.set_cookies, Vec::<String>::new());
    assert!(harness.sso.flows.is_empty());
    assert!(harness.fake.requests().is_empty(), "nothing fetched");
}

#[tokio::test]
async fn start_sends_the_browser_to_no_authorization_endpoint_but_https_or_allowed_http() {
    let harness = Harness::new().await;
    harness.discovery(|document| {
        document["authorization_endpoint"] = json!("ftp://id.example.org/auth");
    });
    harness.sso.metadata.invalidate("fake");
    let reply = harness
        .sso
        .start_oidc_sign_in("fake", CLIENT)
        .await
        .expect("an answer");
    assert_eq!(reply.status, 502);
    assert_eq!(reply.location, None);
    assert_eq!(reply.set_cookies, Vec::<String>::new());
    assert!(harness.sso.flows.is_empty());
}

#[tokio::test]
async fn an_unreachable_provider_is_the_refusal_page() {
    let harness = Harness::new().await;
    harness.fake.set(
        &harness.fake.path("/.well-known/openid-configuration"),
        FakeResponse::status(500),
    );
    let reply = harness
        .sso
        .start_oidc_sign_in("fake", CLIENT)
        .await
        .expect("an answer");
    assert_eq!(reply.status, 502);
    assert!(reply.html.contains("Sign-in failed. Try again."));
    assert!(harness.sso.flows.is_empty());
}

#[tokio::test]
async fn an_unknown_disabled_or_refused_provider_or_plugin_is_not_found() {
    let harness = Harness::new().await;
    let not_found =
        |result: Result<SsoReply, ServiceError>| matches!(result, Err(ServiceError::NotFound(_)));
    assert!(not_found(
        harness.sso.start_oidc_sign_in("nope", CLIENT).await
    ));
    assert!(not_found(
        harness
            .sso
            .finish_oidc_sign_in("nope", OidcCallback::default())
            .await
    ));

    harness
        .sso
        .save_oidc_provider(
            "off",
            fake_provider(&harness.fake, |provider| provider.enabled = false),
        )
        .await
        .expect("saved");
    assert!(not_found(
        harness.sso.start_oidc_sign_in("off", CLIENT).await
    ));

    // Stored through the configuration route with an issuer the checks
    // refuse: treated as disabled.
    let mut config = harness.sso.config().await.expect("config");
    config.providers.push(json!({
        "Key": "refused", "Name": "Refused", "Issuer": "ftp://id.example.org",
        "ClientId": CLIENT_ID,
    }));
    harness.sso.save_config(&config).await.expect("saved");
    assert!(not_found(
        harness.sso.start_oidc_sign_in("refused", CLIENT).await
    ));

    harness
        .sso
        .plugins
        .disable_plugin(EXTENSION_ID)
        .await
        .expect("disable");
    assert!(not_found(
        harness.sso.start_oidc_sign_in("fake", CLIENT).await
    ));
    assert!(harness.sso.flows.is_empty());
}

/// Captures what the log subscriber writes.
#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("logs")).into_owned()
    }
}

impl std::io::Write for CapturedLogs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("logs").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// B15: the log of a sign-in, at every level and from every crate, holds no
/// code, token, verifier, nonce, state, cookie, client secret or claim.
#[tokio::test]
async fn the_log_of_a_sign_in_holds_no_secret_and_no_claim() {
    let logs = CapturedLogs::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .with_env_filter("trace")
            .finish(),
    );
    let harness = Harness::new().await;
    let mut secrets = vec![SECRET.to_owned(), USERNAME.to_owned(), SUBJECT.to_owned()];
    for refused in [false, true] {
        if refused {
            harness.fake.sign_in(|sign_in| {
                sign_in
                    .claims
                    .insert("nonce".to_owned(), json!("another-nonce"));
            });
        }
        let started = harness.start().await;
        let callback = harness.callback(&started).await;
        let query = query(&started.location);
        secrets.extend([
            started.cookie.clone(),
            query["state"].clone(),
            query["nonce"].clone(),
            query["code_challenge"].clone(),
            callback.code.as_ref().expect("a code").expose().to_owned(),
        ]);
        let reply = harness
            .sso
            .finish_oidc_sign_in("fake", callback)
            .await
            .expect("an answer");
        assert_eq!(reply.status, if refused { 400 } else { 200 });
    }
    for request in harness.fake.requests() {
        if request.path == harness.fake.path(TOKEN_PATH) {
            let form: HashMap<String, String> = url::form_urlencoded::parse(&request.body)
                .into_owned()
                .collect();
            secrets.push(form["code_verifier"].clone());
        }
    }
    let text = logs.text();
    assert!(text.contains("OIDC sign-in verified"), "{text}");
    assert!(
        text.contains("OIDC sign-in refused") && text.contains("reason=\"id_token_nonce\""),
        "{text}"
    );
    for secret in &secrets {
        assert!(
            !text.contains(secret.as_str()),
            "{secret} in the log:\n{text}"
        );
    }
    // No token: an access token, nor a JWT (its header always starts so).
    assert!(!text.contains("fake-access-token-"), "{text}");
    assert!(!text.contains("eyJ"), "{text}");
}
