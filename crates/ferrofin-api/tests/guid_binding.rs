//! Route and query ids bind the way ASP.NET binds a `Guid`: through
//! `GuidConverter`, which is `Guid.Parse`. It trims the value, takes the D, N,
//! B, P and X formats, and has no `urn:uuid:` form.
//!
//! Every case's expectation is Jellyfin 12.4's answer, measured on the stock
//! docker image on 2026-11-08 with `GET /Users/{userId}` (a route id) and
//! `GET /Items?limit=1&userId=` (a query id): both answered 200 or 400 alike.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::test_support::{FakeAdminUsers, elevated_fake_state, elevated_state_with_users};
use rstest::rstest;
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

const ID: Uuid = Uuid::from_u128(0x9c4e_63f1_031b_4f25_988b_4f7d_78a8_b53e);

/// Percent-encodes everything but letters, digits and `-`, so braces, commas,
/// parentheses and spaces reach the router as a client would send them.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

async fn get(router: &Router, uri: &str) -> (StatusCode, String) {
    send(router, "GET", uri).await
}

#[rstest]
#[case::d("9c4e63f1-031b-4f25-988b-4f7d78a8b53e", true)]
#[case::d_upper("9C4E63F1-031B-4F25-988B-4F7D78A8B53E", true)]
#[case::n("9c4e63f1031b4f25988b4f7d78a8b53e", true)]
#[case::b("{9c4e63f1-031b-4f25-988b-4f7d78a8b53e}", true)]
#[case::p("(9c4e63f1-031b-4f25-988b-4f7d78a8b53e)", true)]
#[case::x(
    "{0x9c4e63f1,0x031b,0x4f25,{0x98,0x8b,0x4f,0x7d,0x78,0xa8,0xb5,0x3e}}",
    true
)]
#[case::padded(" 9c4e63f1-031b-4f25-988b-4f7d78a8b53e ", true)]
#[case::urn("urn:uuid:9c4e63f1-031b-4f25-988b-4f7d78a8b53e", false)]
#[case::urn_upper("URN:UUID:9C4E63F1-031B-4F25-988B-4F7D78A8B53E", false)]
#[case::garbage("not-a-guid", false)]
#[tokio::test]
async fn route_and_query_ids_bind_like_dotnet_guid_parse(
    #[case] spelling: &str,
    #[case] binds: bool,
) {
    let router = create_router(elevated_state_with_users(Arc::new(FakeAdminUsers)));
    let canonical = format!("/Users/{ID}");
    let (route_status, route_body) = get(&router, &format!("/Users/{}", encode(spelling))).await;
    let (query_status, query_body) = get(
        &router,
        &format!("/Items?limit=1&userId={}", encode(spelling)),
    )
    .await;
    if binds {
        // The user the canonical spelling names, so the same answer.
        let expected = get(&router, &canonical).await;
        assert_eq!(expected.0, StatusCode::OK, "{}", expected.1);
        assert!(expected.1.contains(&format!(r#""Id":"{}""#, ID.simple())));
        assert_eq!((route_status, route_body), expected, "route {spelling:?}");
        // Past the binder, `/Items` answers what it answers the canonical id.
        assert_ne!(query_status, StatusCode::BAD_REQUEST, "query {spelling:?}");
        assert_eq!(
            (query_status, query_body),
            get(&router, &format!("/Items?limit=1&userId={ID}")).await,
            "query {spelling:?}"
        );
    } else {
        // Today's binder rejections, unchanged.
        assert_eq!(route_status, StatusCode::BAD_REQUEST, "route {spelling:?}");
        assert!(
            route_body.starts_with("Invalid URL: Cannot parse `userId` with value"),
            "route {spelling:?}: {route_body}"
        );
        assert_eq!(query_status, StatusCode::BAD_REQUEST, "query {spelling:?}");
        assert!(
            query_body.starts_with("Failed to deserialize query string: userId: "),
            "query {spelling:?}: {query_body}"
        );
        // A spelling `Uuid::parse_str` refuses too keeps, word for word, the
        // body it had when ids were parsed that way.
        if let Err(error) = Uuid::parse_str(spelling) {
            assert_eq!(
                route_body,
                format!(
                    "Invalid URL: Cannot parse `userId` with value `{spelling}`: \
                     UUID parsing failed: {error}"
                )
            );
            assert_eq!(
                query_body,
                format!("Failed to deserialize query string: userId: {error}")
            );
        }
    }
}

/// The uuid parameters the sweep below reached when it was written
/// (2026-11-08). A lower count means parameters stopped reaching a binder;
/// raise it when the contract or the handlers grow.
const MIN_ROUTE_IDS_CHECKED: usize = 131;
/// See [`MIN_ROUTE_IDS_CHECKED`].
const MIN_QUERY_IDS_CHECKED: usize = 142;

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

fn is_uuid(schema: &Value) -> bool {
    schema.get("format").and_then(Value::as_str) == Some("uuid")
}

/// The operation's path with every `{param}` filled from its declared type,
/// and `{probe}` (when given) filled with `value` instead.
fn concrete_path(
    path: &str,
    operation: &Value,
    spec: &Value,
    probe: Option<(&str, &str)>,
) -> String {
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
        let value = match (probe, first_enum) {
            (Some((probed, value)), _) if probed == name => value.to_owned(),
            (_, Some(variant)) => variant.to_owned(),
            _ if is_uuid(schema) => ID.simple().to_string(),
            _ if matches!(schema["type"].as_str(), Some("integer" | "number")) => "1".to_owned(),
            _ => "x".to_owned(),
        };
        let placeholder = format!("{{{name}}}");
        if let Some(at) = out
            .to_ascii_lowercase()
            .find(&placeholder.to_ascii_lowercase())
        {
            out.replace_range(at..at + placeholder.len(), &value);
        }
    }
    out
}

async fn send(router: &Router, method: &str, uri: &str) -> (StatusCode, String) {
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
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// Whether the answer is the binder refusing parameter `name` sent as `value`:
/// axum's path rejection (keyed by the router's own name for the segment,
/// which can differ from the contract's), the query binder's, or
/// `parse_csv_uuids`' for an id list.
fn refuses(name: &str, value: &str, (status, body): &(StatusCode, String)) -> bool {
    let body = body.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    *status == StatusCode::BAD_REQUEST
        && ((body.starts_with("invalid url: cannot parse `")
            && body.contains(&format!("` with value `{}`", value.to_ascii_lowercase())))
            || body.starts_with(&format!("failed to deserialize query string: {name}: "))
            || body.contains("invalid id"))
}

/// Every uuid-typed route and query parameter in the vendored contract binds
/// through `Guid.Parse`: the P spelling of an id gets past the binder, and the
/// `urn:uuid:` spelling is refused there. A route id must be refused (every
/// route binds one); a query id the handler does not model, or binds as a
/// string, is skipped, as the query-casing sweep skips it.
#[tokio::test]
async fn every_contract_guid_parameter_binds_like_dotnet_guid_parse() {
    // Probes the binder accepts reach fakes that panic (`unimplemented!`); the
    // router answers those 500, and their noise is not a failure.
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
    let router = create_router(elevated_fake_state());
    let (accepted, refused) = (format!("({ID})"), format!("urn:uuid:{ID}"));
    let (mut route_ids, mut query_ids, mut failures) = (0, 0, Vec::new());
    for (path, item) in spec["paths"].as_object().expect("paths") {
        for (method, operation) in item.as_object().expect("path item") {
            if !matches!(method.as_str(), "get" | "post" | "put" | "patch" | "delete") {
                continue;
            }
            let method = method.to_uppercase();
            for param in operation["parameters"].as_array().into_iter().flatten() {
                let name = param["name"].as_str().expect("name");
                let schema = resolve(&spec, &param["schema"]);
                let required =
                    ferrofin_api::test_support::required_query(&method, path, operation, name);
                let probe = |value: &str| match param["in"].as_str() {
                    Some("path") => {
                        format!(
                            "{}?{required}",
                            concrete_path(path, operation, &spec, Some((name, &encode(value))))
                        )
                    }
                    _ => format!(
                        "{}?{name}={}&{required}",
                        concrete_path(path, operation, &spec, None),
                        encode(value)
                    ),
                };
                let is_query = param["in"].as_str() == Some("query");
                let list = schema["type"].as_str() == Some("array")
                    && is_uuid(resolve(&spec, &schema["items"]));
                if !(is_uuid(schema) && param["in"].as_str() == Some("path"))
                    && !(is_query && (is_uuid(schema) || list))
                {
                    continue;
                }
                let p_answer = send(&router, &method, &probe(&accepted)).await;
                let urn_answer = send(&router, &method, &probe(&refused)).await;
                let took = !refuses(name, &accepted, &p_answer);
                let refused_urn = refuses(name, &refused, &urn_answer);
                if is_query && took && !refused_urn {
                    // Not modelled, or bound as a string: nothing to compare.
                    continue;
                }
                if !took || !refused_urn {
                    failures.push(format!(
                        "{method} {path} {name}: P {p_answer:?}, urn {urn_answer:?}"
                    ));
                }
                if is_query {
                    query_ids += 1;
                } else {
                    route_ids += 1;
                }
            }
        }
    }
    println!("guid binding: {route_ids} route ids, {query_ids} query ids checked");
    assert!(failures.is_empty(), "{failures:#?}");
    assert!(
        route_ids >= MIN_ROUTE_IDS_CHECKED,
        "only {route_ids} route ids checked"
    );
    assert!(
        query_ids >= MIN_QUERY_IDS_CHECKED,
        "only {query_ids} query ids checked"
    );
}
