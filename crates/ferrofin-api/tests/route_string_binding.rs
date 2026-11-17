//! Route strings bind the way ASP.NET binds a `[FromRoute, Required] string`:
//! `SimpleTypeModelBinder` reads a value of nothing but white space as null,
//! `[Required]` fails, and `[ApiController]`'s model-state filter answers `400`
//! "The {name} field is required." before the action runs, and after
//! authorization. A route `container` (`[RegularExpression]` too) and a
//! plugin's `Version version` take the same rule, through their binders.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::test_support::{elevated_fake_state, fake_state};
use serde_json::{Value, json};
use tower::ServiceExt;

/// Contract string route parameters this sweep does not hold to the rule, as
/// `(operationId, parameter, why)`.
const SKIPPED: &[(&str, &str, &str)] = &[
    (
        "GetCriticReviews",
        "itemId",
        "10.11.8's action binds no id at all",
    ),
    // Bound by gl-1212's change, which this branch does not hold yet.
    ("GetItemImage2", "tag", "gl-1212"),
    ("HeadItemImage2", "tag", "gl-1212"),
];

/// Operations upstream marks `[AllowAnonymous]`, which overrides the
/// controller's `[Authorize]` and which the contract's `security` does not show.
const ALLOW_ANONYMOUS: &[&str] = &["GetPluginImage"];

/// The parameters the sweep checked when it last grew (2026-11-15, with the
/// route containers and the plugin versions). A lower count means parameters
/// stopped reaching the check; raise it when the contract or the handlers grow.
const MIN_CHECKED: usize = 92;

fn spec() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/jellyfin-openapi-10.11.8.json"
    ))
    .expect("spec")
}

fn resolve<'a>(spec: &'a Value, schema: &'a Value) -> &'a Value {
    if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
        let name = r.trim_start_matches("#/components/schemas/");
        return resolve(spec, &spec["components"]["schemas"][name]);
    }
    for wrapper in ["allOf", "oneOf", "anyOf"] {
        if let Some([only]) = schema
            .get(wrapper)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
        {
            return resolve(spec, only);
        }
    }
    schema
}

/// A `string` with no format and no enum: what a C# `string` documents as.
fn is_plain_string(schema: &Value) -> bool {
    schema["type"].as_str() == Some("string")
        && schema.get("format").is_none()
        && schema.get("enum").is_none()
}

/// The operation's path with every `{param}` filled from its declared type,
/// and `{probe}` filled with `%20`.
fn blank_path(path: &str, operation: &Value, spec: &Value, probe: &str) -> String {
    let mut out = path.to_owned();
    for param in operation["parameters"].as_array().into_iter().flatten() {
        if param["in"].as_str() != Some("path") {
            continue;
        }
        let name = param["name"].as_str().unwrap_or_default();
        let schema = resolve(spec, &param["schema"]);
        let first_enum = schema["enum"]
            .as_array()
            .and_then(|values| values.first())
            .and_then(Value::as_str);
        let value = match first_enum {
            _ if name == probe => "%20",
            Some(variant) => variant,
            None if schema["format"].as_str() == Some("uuid") => "9c4e63f1031b4f25988b4f7d78a8b53e",
            None if matches!(schema["type"].as_str(), Some("integer" | "number")) => "1",
            None => "x",
        };
        out = out.replacen(&format!("{{{name}}}"), value, 1);
    }
    out
}

async fn send(router: &Router, method: &str, uri: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body = serde_json::from_slice(&body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()));
    (status, body)
}

/// The `ValidationProblemDetails` Jellyfin answers for a blank `name`.
fn required(name: &str) -> Value {
    json!({
        "type": "https://tools.ietf.org/html/rfc9110#section-15.5.1",
        "title": "One or more validation errors occurred.",
        "status": 400,
        "errors": { name: [format!("The {name} field is required.")] },
    })
}

/// Every plain-string route parameter in the vendored contract answers a
/// blank value with Jellyfin's `400`. An administrator gets it on every
/// route; an anonymous caller gets it only where the route takes anonymous
/// callers, and `401` everywhere else, because authorization runs first.
#[tokio::test]
async fn every_contract_route_string_refuses_a_blank_value_after_authorization() {
    // A probe that gets past the binder reaches a fake that panics
    // (`unimplemented!`); the router answers that 500, and its noise is not
    // the failure being reported.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !info
            .payload_as_str()
            .is_some_and(|m| m.contains("not implemented: fake"))
        {
            default_hook(info);
        }
    }));
    let spec = spec();
    let admin = create_router(elevated_fake_state());
    let anonymous = create_router(fake_state());
    let (mut checked, mut failures) = (0, Vec::new());
    for (path, item) in spec["paths"].as_object().expect("paths") {
        for (method, operation) in item.as_object().expect("path item") {
            if !matches!(method.as_str(), "get" | "head" | "post" | "delete") {
                continue;
            }
            let method = method.to_uppercase();
            let operation_id = operation["operationId"].as_str().unwrap_or_default();
            let secured = !ALLOW_ANONYMOUS.contains(&operation_id)
                && operation
                    .get("security")
                    .and_then(Value::as_array)
                    .is_some_and(|s| !s.is_empty());
            for param in operation["parameters"].as_array().into_iter().flatten() {
                let name = param["name"].as_str().expect("name");
                if param["in"].as_str() != Some("path")
                    || !is_plain_string(resolve(&spec, &param["schema"]))
                    || SKIPPED
                        .iter()
                        .any(|(op, skipped, _)| *op == operation_id && *skipped == name)
                {
                    continue;
                }
                let query =
                    ferrofin_api::test_support::required_query(&method, path, operation, "");
                let uri = format!("{}?{query}", blank_path(path, operation, &spec, name));
                let expected = required(name);
                let as_admin = send(&admin, &method, &uri).await;
                // A HEAD answer carries no body to compare.
                let admin_ok = as_admin.0 == StatusCode::BAD_REQUEST
                    && (method == "HEAD" || as_admin.1 == expected);
                let as_anonymous = send(&anonymous, &method, &uri).await;
                let anonymous_ok = if secured {
                    as_anonymous.0 == StatusCode::UNAUTHORIZED
                } else {
                    as_anonymous.0 == StatusCode::BAD_REQUEST
                        && (method == "HEAD" || as_anonymous.1 == expected)
                };
                if !admin_ok || !anonymous_ok {
                    failures.push(format!(
                        "{method} {uri} ({operation_id} {name}): admin {as_admin:?}, \
                         anonymous {as_anonymous:?}"
                    ));
                }
                checked += 1;
            }
        }
    }
    println!("route strings: {checked} parameters checked");
    assert!(failures.is_empty(), "{failures:#?}");
    assert!(checked >= MIN_CHECKED, "only {checked} parameters checked");
}

/// `MoveItem` binds two required strings; with both blank, the one `400`
/// names both, as the model-state filter reports every failing parameter.
#[tokio::test]
async fn a_move_with_both_ids_blank_names_both() {
    let router = create_router(elevated_fake_state());
    let (status, body) = send(&router, "POST", "/Playlists/%20/Items/%09/Move/1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["errors"],
        json!({
            "playlistId": ["The playlistId field is required."],
            "itemId": ["The itemId field is required."],
        })
    );
}

/// `{segmentId}.{segmentContainer}` is one captured segment here; each half
/// is a required string of its own.
#[tokio::test]
async fn a_legacy_segment_names_whichever_half_is_blank() {
    let router = create_router(elevated_fake_state());
    for (segment, names) in [
        ("%20.ts", vec!["segmentId"]),
        ("seg.%20", vec!["segmentContainer"]),
        ("%20.%20", vec!["segmentId", "segmentContainer"]),
        // Split at the last `.`, as ASP.NET's complex segment is.
        ("a.b.%20", vec!["segmentContainer"]),
    ] {
        let (status, body) = send(&router, "GET", &format!("/Videos/x/hls/pl/{segment}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{segment}: {body}");
        let errors: Vec<&str> = body["errors"]
            .as_object()
            .expect("errors")
            .keys()
            .map(String::as_str)
            .collect();
        let mut names = names;
        names.sort_unstable();
        assert_eq!(errors, names, "{segment}");
    }
}

/// An empty part of a captured segment, or a subtitle segment without the
/// `Stream.` literal, is no match for ASP.NET's complex segment (Jellyfin 12.4
/// answers 404), so it is not the blank-value 400.
#[tokio::test]
async fn an_unmatched_segment_part_is_not_a_blank_value() {
    let router = create_router(elevated_fake_state());
    for uri in [
        "/Videos/x/hls/pl/.ts",
        "/Videos/x/hls/pl/seg.",
        "/Videos/x/hls/pl/.%20",
        "/Videos/9c4e63f1031b4f25988b4f7d78a8b53e/x/Subtitles/0/Stream.",
        "/Videos/9c4e63f1031b4f25988b4f7d78a8b53e/x/Subtitles/0/%20",
        "/Videos/9c4e63f1031b4f25988b4f7d78a8b53e/x/Subtitles/0/Foo.%20",
        "/Videos/9c4e63f1031b4f25988b4f7d78a8b53e/x/Subtitles/0/1/Stream.",
    ] {
        let (status, body) = send(&router, "GET", uri).await;
        assert!(body.get("errors").is_none(), "{uri}: {status} {body}");
        assert!(!status.is_server_error(), "{uri}: {status} {body}");
    }
}

/// The complaint `[RegularExpression(ContainerValidationRegexStr)]` files.
const CONTAINER_PATTERN: &str =
    r"The field container must match the regular expression '^[a-zA-Z0-9\-\._,|]{0,40}$'.";

/// A route `container` is `[Required]` and patterned: a blank one is the
/// required complaint alone (the pattern passes a null), one outside the
/// pattern is the pattern's, and each is filed beside the other parameters'
/// in one body. The `stream.` literal matches in any case. Settled against a
/// stock 12.4 container (2026-11-15).
#[tokio::test]
async fn a_route_container_is_required_and_patterned() {
    let router = create_router(elevated_fake_state());
    let id = "9c4e63f1031b4f25988b4f7d78a8b53e";
    let required = |name: &str| format!("The {name} field is required.");
    for (uri, errors) in [
        (
            format!("/Videos/{id}/stream.a%24b"),
            json!({"container": [CONTAINER_PATTERN]}),
        ),
        (
            format!("/Audio/{id}/Stream.%20ts"),
            json!({"container": [CONTAINER_PATTERN]}),
        ),
        (
            format!("/Audio/{id}/Stream.%09"),
            json!({"container": [required("container")]}),
        ),
        (
            format!("/Videos/{id}/stream.%20?audioCodec=a%24b"),
            json!({
                "container": [required("container")],
                "audioCodec": [CONTAINER_PATTERN.replace("container", "audioCodec")],
            }),
        ),
        (
            "/LiveTv/LiveStreamFiles/x/stream.a%24b".to_owned(),
            json!({"container": [CONTAINER_PATTERN]}),
        ),
        (
            "/LiveTv/LiveStreamFiles/%20/stream.%20".to_owned(),
            json!({
                "streamId": [required("streamId")],
                "container": [required("container")],
            }),
        ),
        (
            "/LiveTv/LiveStreamFiles/%20/stream.a%24b".to_owned(),
            json!({
                "streamId": [required("streamId")],
                "container": [CONTAINER_PATTERN],
            }),
        ),
    ] {
        let (status, body) = send(&router, "GET", &uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
        assert_eq!(body["errors"], errors, "{uri}");
    }
}

/// A live stream file's segment without the `stream.` literal, or with
/// nothing after it, is no match for upstream's route (Jellyfin 12.4 answers
/// an empty 404), so it is not a container complaint.
#[tokio::test]
async fn a_live_stream_file_needs_the_stream_literal() {
    let router = create_router(elevated_fake_state());
    for uri in [
        "/LiveTv/LiveStreamFiles/x/stream.",
        "/LiveTv/LiveStreamFiles/x/%20",
        "/LiveTv/LiveStreamFiles/x/a%24b",
        "/Videos/9c4e63f1031b4f25988b4f7d78a8b53e/stream.",
    ] {
        let (status, body) = send(&router, "GET", uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
        assert!(body.get("errors").is_none(), "{uri}: {body}");
    }
}

/// A `{version}` that is no `System.Version` is the binder's complaint, keyed
/// `version`, before the action looks the plugin up, so an unknown plugin gets
/// it too.
#[tokio::test]
async fn a_plugin_version_that_is_no_version_is_the_binders_complaint() {
    let router = create_router(elevated_fake_state());
    let id = "9c4e63f1031b4f25988b4f7d78a8b53e";
    for (method, uri) in [
        ("POST", format!("/Plugins/{id}/abc/Enable")),
        ("POST", format!("/Plugins/{id}/1/Disable")),
        ("DELETE", format!("/Plugins/{id}/1.0-beta")),
        ("GET", format!("/Plugins/{id}/2147483648.0/Image")),
    ] {
        let (status, body) = send(&router, method, &uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {uri}: {body}");
        let version = uri.split('/').nth(3).expect("version segment");
        assert_eq!(
            body,
            json!({
                "type": "https://tools.ietf.org/html/rfc9110#section-15.5.1",
                "title": "One or more validation errors occurred.",
                "status": 400,
                "errors": { "version": [format!("The value '{version}' is not valid.")] },
            }),
            "{method} {uri}"
        );
    }
}
