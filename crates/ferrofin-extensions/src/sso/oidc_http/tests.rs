//! The identity provider HTTP client against the fake identity provider: it
//! fetches within its limits and refuses a redirect, an over-long answer, an
//! `http` URL without the switch, and a slow answer.

use std::time::Duration;

use openidconnect::http::{Method, Request, StatusCode};

use super::*;
use crate::sso::fake_idp::{FakeIdentityProvider, FakeResponse};

/// A `GET` of `url` with an empty body, as `openidconnect` sends one.
fn get(url: &str) -> HttpRequest {
    Request::builder()
        .method(Method::GET)
        .uri(url)
        .header("accept", "application/json")
        .body(Vec::new())
        .expect("request")
}

#[test]
fn the_limits_are_the_milestones_values() {
    assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(5));
    assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(10));
    assert_eq!(MAX_RESPONSE_BYTES, 1_048_576);
    assert_eq!(OidcHttpClient::new().request_timeout, REQUEST_TIMEOUT);
    assert!(!OidcHttpClient::default().allow_http);
}

#[tokio::test]
async fn an_answer_is_returned_with_its_status_headers_and_body() {
    let fake = FakeIdentityProvider::start().await;
    fake.set(
        "/doc",
        FakeResponse::json(&serde_json::json!({"a": 1})).with_header("x-fake", "yes"),
    );
    let http = OidcHttpClient::new().allowing_http(true);
    let answer = http.send(get(&fake.url("/doc"))).await.expect("answer");
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(answer.headers()["content-type"], "application/json");
    assert_eq!(answer.headers()["x-fake"], "yes");
    assert_eq!(answer.body(), br#"{"a":1}"#);
    // A non-200 answer is returned as it is, for the caller to judge.
    let answer = http.send(get(&fake.url("/missing"))).await.expect("answer");
    assert_eq!(answer.status(), StatusCode::NOT_FOUND);
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "GET");
    assert!(
        requests[0]
            .headers
            .iter()
            .any(|(name, value)| name == "user-agent" && value.starts_with("Ferrofin/")),
        "{requests:?}"
    );
}

#[tokio::test]
async fn a_redirect_is_refused_and_not_followed() {
    let fake = FakeIdentityProvider::start().await;
    fake.set("/target", FakeResponse::json(&serde_json::json!({})));
    for status in [301, 302, 303, 307, 308] {
        fake.set(
            "/moved",
            FakeResponse {
                status,
                ..FakeResponse::redirect(&fake.url("/target"))
            },
        );
        let refused = OidcHttpClient::new()
            .allowing_http(true)
            .send(get(&fake.url("/moved")))
            .await;
        assert!(
            matches!(refused, Err(OidcHttpError::Redirect(code)) if code.as_u16() == status),
            "{status}: {refused:?}"
        );
    }
    assert!(
        fake.requests()
            .iter()
            .all(|request| request.path == "/moved"),
        "the target was never asked for"
    );
    let message = OidcHttpError::Redirect(StatusCode::FOUND).to_string();
    assert!(message.contains("not followed"), "{message}");
}

#[tokio::test]
async fn an_answer_over_the_cap_is_refused_and_one_at_it_is_read() {
    let fake = FakeIdentityProvider::start().await;
    fake.set(
        "/at-cap",
        FakeResponse::status(200).with_body("application/json", vec![b' '; MAX_RESPONSE_BYTES]),
    );
    fake.set(
        "/over-cap",
        FakeResponse::status(200).with_body("application/json", vec![b' '; MAX_RESPONSE_BYTES + 1]),
    );
    let http = OidcHttpClient::new().allowing_http(true);
    let read = http
        .send(get(&fake.url("/at-cap")))
        .await
        .expect("at the cap");
    assert_eq!(read.body().len(), MAX_RESPONSE_BYTES);
    let refused = http.send(get(&fake.url("/over-cap"))).await;
    assert!(
        matches!(refused, Err(OidcHttpError::TooLarge)),
        "{refused:?}"
    );
}

/// A chunked or decoded answer has no `Content-Length` to refuse early: the
/// cap holds as it is read.
#[tokio::test]
async fn an_answer_without_a_length_is_capped_as_it_is_read() {
    let fake = FakeIdentityProvider::start().await;
    fake.set(
        "/at-cap",
        FakeResponse::status(200)
            .with_body("application/json", vec![b' '; MAX_RESPONSE_BYTES])
            .chunked(),
    );
    fake.set(
        "/over-cap",
        FakeResponse::status(200)
            .with_body("application/json", vec![b' '; MAX_RESPONSE_BYTES + 1])
            .chunked(),
    );
    let http = OidcHttpClient::new().allowing_http(true);
    let read = http
        .send(get(&fake.url("/at-cap")))
        .await
        .expect("at the cap");
    assert_eq!(read.body().len(), MAX_RESPONSE_BYTES);
    assert_eq!(read.headers().get("content-length"), None, "streamed");
    let refused = http.send(get(&fake.url("/over-cap"))).await;
    assert!(
        matches!(refused, Err(OidcHttpError::TooLarge)),
        "{refused:?}"
    );
}

#[tokio::test]
async fn an_http_url_is_refused_without_the_switch_and_never_sent() {
    let fake = FakeIdentityProvider::start().await;
    let refused = OidcHttpClient::new().send(get(&fake.issuer())).await;
    assert!(
        matches!(refused, Err(OidcHttpError::HttpRefused)),
        "{refused:?}"
    );
    assert!(fake.requests().is_empty(), "nothing was sent");
    // The switch on a copy leaves the shared client as it was.
    let shared = OidcHttpClient::new();
    assert!(
        shared
            .allowing_http(true)
            .send(get(&fake.url("/x")))
            .await
            .is_ok()
    );
    assert!(matches!(
        shared.send(get(&fake.url("/x"))).await,
        Err(OidcHttpError::HttpRefused)
    ));
    let refused = OidcHttpClient::new()
        .allowing_http(true)
        .send(get("ftp://127.0.0.1/x"))
        .await;
    assert!(matches!(refused, Err(OidcHttpError::Scheme)), "{refused:?}");
}

#[tokio::test]
async fn a_slow_answer_times_out() {
    let fake = FakeIdentityProvider::start().await;
    fake.set(
        "/slow",
        FakeResponse::json(&serde_json::json!({})).delayed(Duration::from_secs(5)),
    );
    let started = std::time::Instant::now();
    let refused = OidcHttpClient::with_request_timeout(Duration::from_secs(1))
        .allowing_http(true)
        .send(get(&fake.url("/slow")))
        .await;
    assert!(
        matches!(refused, Err(OidcHttpError::Timeout(after)) if after == Duration::from_secs(1)),
        "{refused:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    let message = OidcHttpError::Timeout(REQUEST_TIMEOUT).to_string();
    assert!(message.contains("within 10 seconds"), "{message}");
}

#[tokio::test]
async fn a_refused_connection_says_why_without_the_url() {
    // A port nothing listens on any more.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("address").port();
    drop(listener);
    let url = format!("http://127.0.0.1:{port}/realms/secret-path");
    let refused = OidcHttpClient::new()
        .allowing_http(true)
        .send(get(&url))
        .await;
    let Err(OidcHttpError::Connect(why)) = refused else {
        panic!("{refused:?}");
    };
    assert_ne!(why, "");
    let message = OidcHttpError::Connect(why).to_string();
    assert!(!message.contains("secret-path"), "{message}");
    assert!(!message.contains(&port.to_string()), "{message}");
}
