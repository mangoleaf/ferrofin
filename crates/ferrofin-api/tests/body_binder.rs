//! The request-body binder, over real routes.
//!
//! This is a CLASS test, not a per-handler one, because the defect it pins was
//! a class defect: every body-taking route in the server bound with
//! `axum::Json`, whose rejection is `422` and `text/plain`, where every Jellyfin
//! controller answers `400` with a `ValidationProblemDetails` document
//! (`[ApiController]` on `BaseJellyfinApiController`, v10.11.8
//! Jellyfin.Api/BaseJellyfinApiController.cs:12-18, with no
//! `InvalidModelStateResponseFactory` anywhere in the tree to change it).
//!
//! The matrix below is the one measured against a live Jellyfin 10.11.8 on the
//! parity pair; the routes are chosen from three different controllers so a
//! regression that re-introduces `axum::Json` in one file still fails here.
//!
//! Two of the cases are named regression pins for `POST /LiveTv/Programs`:
//! `{"SortBy":["NotASort"]}` (the 422→400 half) and `["StartDate"]` (the
//! silently-accepted half — serde's derived `Deserialize` binds a JSON sequence
//! to a struct positionally, so Ferrofin used to answer that malformed body
//! **200 with the whole guide**).

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::authed_fake_state_with_policy;
use ferrofin_model::users::UserPolicy;
use tower::ServiceExt;

/// A stock account: Live TV access granted, management withheld — the seeded
/// Jellyfin defaults (`UserEntityExtensions.cs:187-188`), so the
/// `RequireLiveTvAccess` gate in front of `POST /LiveTv/Programs` lets the
/// request reach the BINDER, which is what these tests are about.
///
/// No Live TV manager is wired: `query_programs_inner` answers an empty
/// `QueryResult` when `state.live_tv` is `None`, which makes a well-formed body
/// a clean `200` with no stub to write.
fn state() -> AppState {
    authed_fake_state_with_policy(UserPolicy {
        enable_live_tv_access: true,
        enable_remote_access: true,
        ..UserPolicy::default()
    })
}

/// One POST, returning `(status, content-type, body)`.
async fn post(
    uri: &str,
    content_type: Option<&str>,
    body: &'static str,
) -> (StatusCode, String, String) {
    let mut request = Request::builder().method("POST").uri(uri);
    if let Some(ct) = content_type {
        request = request.header(header::CONTENT_TYPE, ct);
    }
    let response = create_router(state())
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let ct = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    (status, ct, String::from_utf8_lossy(&bytes).into_owned())
}

/// The three routes the matrix runs over, one per controller. `/LiveTv/Programs`
/// is the op this was measured on; the other two are anonymous, so a failure
/// there cannot be blamed on the auth stub.
const ROUTES: &[&str] = &[
    "/LiveTv/Programs",
    "/Users/AuthenticateByName",
    "/Users/ForgotPassword",
];

/// Every body whose SHAPE the binder refuses is `400` with a
/// `ValidationProblemDetails` document — never axum's `422 text/plain`.
///
/// These bodies are DTO-independent, so the same matrix runs over three
/// controllers: a regression that re-introduces `axum::Json` in one handler
/// file still fails here.
#[tokio::test]
async fn a_malformed_body_is_a_validation_problem_not_a_422() {
    let bodies: &[(&str, &'static str)] = &[
        ("empty", ""),
        ("blank", "   "),
        ("truncated", "{"),
        ("null", "null"),
        ("a number", "5"),
        // The silently-accepted half: serde's derived `Deserialize` binds a
        // JSON sequence to a struct positionally, so before the shared binder
        // `POST /LiveTv/Programs` answered `[]` with 200 and the whole guide.
        ("a sequence", "[]"),
        ("a non-empty sequence", r#"["StartDate"]"#),
    ];
    for uri in ROUTES {
        for (label, body) in bodies {
            assert_validation_problem(uri, label, body).await;
        }
    }
}

/// …and so is a member the DTO's own types cannot take. Pinned on
/// `POST /LiveTv/Programs`, the op this class defect was measured on: measured
/// against Jellyfin 10.11.8, `{"SortBy":["NotASort"]}` is 400 with
/// `"$[0]": ["The JSON value could not be converted to
/// Jellyfin.Data.Enums.ItemSortBy…"]`, where Ferrofin answered 422 `text/plain`.
#[tokio::test]
async fn a_member_the_dto_cannot_take_is_a_validation_problem() {
    for (label, body) in [
        ("an unknown enum variant", r#"{"SortBy":["NotASort"]}"#),
        ("a member of the wrong type", r#"{"Limit":"abc"}"#),
        ("a malformed guid", r#"{"ChannelIds":["nope"]}"#),
    ] {
        assert_validation_problem("/LiveTv/Programs", label, body).await;
    }
}

/// Asserts the full 400 contract for one (route, body).
async fn assert_validation_problem(uri: &str, label: &str, body: &'static str) {
    let (status, content_type, payload) = post(uri, Some("application/json"), body).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "{uri} with {label}: ASP.NET's model binder answers 400, not {status}"
    );
    assert_eq!(
        content_type, "application/json; charset=utf-8",
        "{uri} with {label}: the problem document is JSON, not text/plain"
    );
    let document: serde_json::Value =
        serde_json::from_str(&payload).expect("a JSON problem document");
    assert_eq!(document["status"], 400, "{uri} with {label}");
    assert_eq!(
        document["title"], "One or more validation errors occurred.",
        "{uri} with {label}"
    );
    assert!(
        document["errors"].is_object(),
        "{uri} with {label}: a validation problem names what failed"
    );
}

/// A body that is not JSON at all — or carries no content type — is `415`,
/// also as a problem document.
#[tokio::test]
async fn a_wrong_content_type_is_an_unsupported_media_type_problem() {
    for uri in ROUTES {
        for content_type in [None, Some("text/plain"), Some("application/xml")] {
            let (status, ct, payload) = post(uri, content_type, "{}").await;
            assert_eq!(
                status,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{uri} with content-type {content_type:?}"
            );
            assert_eq!(ct, "application/json; charset=utf-8");
            let document: serde_json::Value =
                serde_json::from_str(&payload).expect("a JSON problem document");
            assert_eq!(document["status"], 415);
            assert_eq!(document["title"], "Unsupported Media Type");
        }
    }
}

/// An OPTIONAL body (`[FromBody] T? dto`) is a different rule and is measured
/// separately: ASP.NET only selects an input formatter when there is something
/// to read, so an EMPTY body binds to null whatever the content type says.
///
/// Measured on Jellyfin 10.11.8, `POST /Items/{id}/PlaybackInfo`
/// (`[FromBody] PlaybackInfoDto?`): `text/plain` with an empty body is 200,
/// `text/plain` with `{}` is 415, `null` and `{}` are both 200, `[]` is 400.
/// This test drives the extractor directly, because the optional-body routes in
/// the router all need a real item behind them.
#[tokio::test]
async fn an_optional_body_binds_nothing_to_none_whatever_the_content_type() {
    use axum::extract::{FromRequest, Request};
    use ferrofin_api::extract::JsonBody;

    #[derive(Debug, serde::Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Dto {
        max_streaming_bitrate: Option<i64>,
    }

    async fn optional(content_type: &str, body: &'static str) -> Result<Option<Dto>, StatusCode> {
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(body))
            .unwrap();
        match <Option<JsonBody<Dto>> as FromRequest<()>>::from_request(request, &()).await {
            Ok(bound) => Ok(bound.map(|JsonBody(dto)| dto)),
            Err(rejection) => Err(rejection.into_response().status()),
        }
    }
    use axum::response::IntoResponse;

    // Nothing to read → null, whatever the content type claims.
    for content_type in ["application/json", "text/plain"] {
        for body in ["", "   "] {
            assert!(
                optional(content_type, body).await.expect("bound").is_none(),
                "{content_type} with {body:?} binds to null"
            );
        }
    }
    // …and `null` is "nothing" too, on a JSON content type.
    assert!(
        optional("application/json", "null")
            .await
            .expect("bound")
            .is_none()
    );
    // Something to read on a content type no formatter claims → 415.
    assert_eq!(
        optional("text/plain", "{}").await.unwrap_err(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    // Something to read that the DTO cannot take → 400, not "no body supplied".
    assert_eq!(
        optional("application/json", "[]").await.unwrap_err(),
        StatusCode::BAD_REQUEST
    );
    // …and a real body binds.
    let dto = optional("application/json", r#"{"MaxStreamingBitrate":42}"#)
        .await
        .expect("bound")
        .expect("present");
    assert_eq!(dto.max_streaming_bitrate, Some(42));
}

/// …and a well-formed body still reaches the handler. Without this the two
/// tests above would pass just as well on a server that rejected everything.
#[tokio::test]
async fn a_well_formed_body_still_binds() {
    let (status, _, payload) = post(
        "/LiveTv/Programs",
        Some("application/json"),
        r#"{"SortBy":["StartDate"],"Limit":5}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{payload}");
    // The charset suffix is accepted too — clients send it.
    let (status, ..) = post(
        "/LiveTv/Programs",
        Some("application/json; charset=utf-8"),
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}
