//! Discovery and its cache against the fake identity provider.

use ferrofin_model::sso::OidcProvider;

use super::*;
use crate::sso::fake_idp::{FakeIdentityProvider, FakeResponse, JWKS_PATH};

const DISCOVERY: &str = "/.well-known/openid-configuration";

/// A provider for the fake's issuer, with `AllowHttpIssuer` on.
fn provider(fake: &FakeIdentityProvider) -> OidcProvider {
    OidcProvider {
        key: "fake".to_owned(),
        name: "Fake".to_owned(),
        issuer: fake.issuer(),
        client_id: "ferrofin".to_owned(),
        allow_http_issuer: true,
        ..OidcProvider::default()
    }
}

/// The fake's discovery document with `member` set to `value`.
fn document_with(fake: &FakeIdentityProvider, member: &str, value: serde_json::Value) {
    let mut document = fake.discovery_document();
    document[member] = value;
    fake.set(&fake.path(DISCOVERY), FakeResponse::json(&document));
}

/// How many requests the fake has had for `suffix` below its issuer.
fn asked(fake: &FakeIdentityProvider, suffix: &str) -> usize {
    let path = fake.path(suffix);
    fake.requests().iter().filter(|r| r.path == path).count()
}

#[test]
fn the_cache_lifetime_is_an_hour() {
    assert_eq!(METADATA_CACHE_LIFETIME, Duration::from_secs(3600));
}

#[tokio::test]
async fn discovery_finds_the_endpoints_and_the_keys() {
    let fake = FakeIdentityProvider::start().await;
    let metadata = discover(&OidcHttpClient::new(), &provider(&fake))
        .await
        .expect("discovered");
    let issuer = fake.issuer();
    assert_eq!(
        summary(&metadata),
        OidcDiscovery {
            error: None,
            authorization_endpoint: Some(format!("{issuer}/protocol/openid-connect/auth")),
            token_endpoint: Some(format!("{issuer}/protocol/openid-connect/token")),
            userinfo_endpoint: Some(format!("{issuer}/protocol/openid-connect/userinfo")),
            jwks_uri: Some(format!("{issuer}{JWKS_PATH}")),
            signing_keys: Some(1),
            warnings: Vec::new(),
        }
    );
    assert_eq!(
        metadata
            .additional_metadata()
            .code_challenge_methods_supported,
        Some(vec!["plain".to_owned(), "S256".to_owned()])
    );
    assert_eq!(asked(&fake, DISCOVERY), 1);
    assert_eq!(asked(&fake, JWKS_PATH), 1);
}

#[tokio::test]
async fn an_issuer_with_a_trailing_slash_is_discovered_below_it() {
    let fake = FakeIdentityProvider::start().await;
    let issuer = format!("{}/", fake.issuer());
    document_with(&fake, "issuer", serde_json::json!(issuer));
    let slashed = OidcProvider {
        issuer,
        ..provider(&fake)
    };
    discover(&OidcHttpClient::new(), &slashed)
        .await
        .expect("discovered");
    assert_eq!(asked(&fake, DISCOVERY), 1);
}

#[tokio::test]
async fn a_document_for_another_issuer_is_refused() {
    let fake = FakeIdentityProvider::start().await;
    document_with(
        &fake,
        "issuer",
        serde_json::json!(fake.url("/realms/someone-else")),
    );
    let failure = discover(&OidcHttpClient::new(), &provider(&fake))
        .await
        .expect_err("another issuer");
    assert_eq!(failure.kind, "issuer");
    assert!(
        failure.message.contains("another issuer")
            && failure.message.contains("/realms/someone-else"),
        "{failure:?}"
    );
    assert_eq!(asked(&fake, JWKS_PATH), 0, "the keys are not fetched");
}

#[tokio::test]
async fn what_sign_in_cannot_use_is_reported() {
    let fake = FakeIdentityProvider::start().await;
    let mut document = fake.discovery_document();
    document["response_types_supported"] = serde_json::json!(["id_token", "code id_token"]);
    document["code_challenge_methods_supported"] = serde_json::json!(["plain"]);
    fake.set(&fake.path(DISCOVERY), FakeResponse::json(&document));
    let metadata = discover(&OidcHttpClient::new(), &provider(&fake))
        .await
        .expect("discovered");
    assert_eq!(
        summary(&metadata).warnings,
        vec![
            "the provider does not advertise the code response type, which sign-in uses".to_owned(),
            "the provider lists PKCE methods without S256, which sign-in uses".to_owned(),
        ]
    );

    // A provider that lists no PKCE methods, or an empty list, is not warned
    // about them.
    for methods in [None, Some(serde_json::json!([]))] {
        let mut document = fake.discovery_document();
        match methods {
            Some(methods) => document["code_challenge_methods_supported"] = methods,
            None => {
                document
                    .as_object_mut()
                    .expect("object")
                    .remove("code_challenge_methods_supported");
            }
        }
        fake.set(&fake.path(DISCOVERY), FakeResponse::json(&document));
        let metadata = discover(&OidcHttpClient::new(), &provider(&fake))
            .await
            .expect("discovered");
        assert_eq!(summary(&metadata).warnings, Vec::<String>::new());
    }

    // No token endpoint, and a key set with no key Ferrofin reads (a key of
    // an unknown type is skipped).
    let mut document = fake.discovery_document();
    document
        .as_object_mut()
        .expect("object")
        .remove("token_endpoint");
    fake.set(&fake.path(DISCOVERY), FakeResponse::json(&document));
    fake.set(
        &fake.path(JWKS_PATH),
        FakeResponse::json(&serde_json::json!({"keys": [{"kty": "unknown", "kid": "x"}]})),
    );
    let metadata = discover(&OidcHttpClient::new(), &provider(&fake))
        .await
        .expect("discovered");
    let found = summary(&metadata);
    assert_eq!(found.token_endpoint, None);
    assert_eq!(found.signing_keys, Some(0));
    assert_eq!(
        found.warnings,
        vec![
            "the provider advertises no token endpoint, which sign-in exchanges its code at"
                .to_owned(),
            "the provider's key set holds no key Ferrofin can read, so no ID token can be checked"
                .to_owned(),
        ]
    );
}

#[tokio::test]
async fn each_bad_answer_fails_discovery_with_its_reason() {
    let http = OidcHttpClient::new();
    let cases = [
        (
            DISCOVERY,
            FakeResponse::status(404),
            "response",
            "HTTP status code 404",
        ),
        (
            DISCOVERY,
            FakeResponse::status(200).with_body("text/html", b"<html></html>".to_vec()),
            "response",
            "Content-Type",
        ),
        (
            DISCOVERY,
            FakeResponse::status(200).with_body("application/json", b"{\"issuer\": 1".to_vec()),
            "parse",
            "does not read",
        ),
        (
            DISCOVERY,
            FakeResponse::redirect("https://elsewhere.example/"),
            "request",
            "redirect",
        ),
        (JWKS_PATH, FakeResponse::status(500), "response", "500"),
        (
            JWKS_PATH,
            FakeResponse::status(200).with_body("application/json", b"[]".to_vec()),
            "parse",
            "does not read",
        ),
    ];
    for (suffix, answer, kind, says) in cases {
        let fake = FakeIdentityProvider::start().await;
        fake.set(&fake.path(suffix), answer);
        let failure = discover(&http, &provider(&fake))
            .await
            .expect_err("a bad answer");
        assert_eq!(failure.kind, kind, "{suffix}: {failure:?}");
        assert!(failure.message.contains(says), "{suffix}: {failure:?}");
        assert!(!failure.message.contains("<html>"), "{failure:?}");
    }
}

#[tokio::test]
async fn an_http_issuer_needs_the_switch() {
    let fake = FakeIdentityProvider::start().await;
    let http = OidcHttpClient::new();
    let off = OidcProvider {
        allow_http_issuer: false,
        ..provider(&fake)
    };
    let failure = discover(&http, &off).await.expect_err("http refused");
    assert_eq!(failure.kind, "request");
    assert!(
        failure.message.contains("allow an http issuer"),
        "{failure:?}"
    );
    assert_eq!(fake.requests().len(), 0, "nothing was fetched");

    let failure = discover(
        &http,
        &OidcProvider {
            issuer: "not a url".to_owned(),
            ..provider(&fake)
        },
    )
    .await
    .expect_err("not a URL");
    assert_eq!(failure.kind, "issuer");
}

#[tokio::test]
async fn the_cache_keeps_metadata_for_its_issuer_until_it_is_dropped_or_old() {
    let fake = FakeIdentityProvider::start().await;
    let http = OidcHttpClient::new();
    let cache = MetadataCache::default();
    let fake_provider = provider(&fake);
    assert!(cache.get(&fake_provider).is_none());

    let first = cache
        .get_or_refresh(&http, &fake_provider)
        .await
        .expect("fetched");
    let again = cache
        .get_or_refresh(&http, &fake_provider)
        .await
        .expect("cached");
    assert!(Arc::ptr_eq(&first, &again));
    assert_eq!(asked(&fake, DISCOVERY), 1, "the second came from the cache");

    // Another issuer, or the switch turned off, does not use the entry.
    let moved = OidcProvider {
        issuer: format!("{}/", fake.issuer()),
        ..fake_provider.clone()
    };
    assert!(cache.get(&moved).is_none());
    let switched = OidcProvider {
        allow_http_issuer: false,
        ..fake_provider.clone()
    };
    assert!(cache.get(&switched).is_none());

    // Dropped, it is fetched again.
    cache.invalidate("fake");
    assert!(cache.get(&fake_provider).is_none());
    cache
        .get_or_refresh(&http, &fake_provider)
        .await
        .expect("fetched again");
    assert_eq!(asked(&fake, DISCOVERY), 2);

    // An hour old, it is not used.
    {
        let mut entries = cache.entries.lock().expect("entries");
        let entry = entries.get_mut("fake").expect("entry");
        entry.fetched = Instant::now()
            .checked_sub(METADATA_CACHE_LIFETIME)
            .expect("an hour ago");
    }
    assert!(cache.get(&fake_provider).is_none());
    cache
        .get_or_refresh(&http, &fake_provider)
        .await
        .expect("fetched a third time");
    assert_eq!(asked(&fake, DISCOVERY), 3);

    // A failed refresh leaves the cached entry in place.
    fake.set(&fake.path(DISCOVERY), FakeResponse::status(503));
    assert!(cache.refresh(&http, &fake_provider).await.is_err());
    assert!(cache.get(&fake_provider).is_some());
}
