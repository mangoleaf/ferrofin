//! Contract-wide guard: every typed query parameter binds its key ignoring
//! case, as ASP.NET's query value provider does.
//!
//! For every operation in the vendored spec, each query parameter the spec
//! types as an integer, number, boolean or uuid is sent alone with a value it
//! cannot parse (`zz`). Bound under the spec's own key, axum's query binder
//! answers 400 naming the member (`Failed to deserialize query string:
//! <member>: …`). The same request under the PascalCase, lowercase and
//! UPPERCASE key must name the same member: a case-sensitive binder would
//! ignore the unknown key and answer something else.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::test_support::elevated_fake_state;
use serde_json::Value;
use tower::ServiceExt;

/// The query parameters the oracle compared when it was written (2026-10-07).
/// A lower count means parameters quietly stopped reaching the binder; raise
/// this when the contract or the handlers grow.
const MIN_PARAMS_CHECKED: usize = 723;

/// The prefix axum's query rejection puts before the failing member's path.
const REJECTION: &str = "Failed to deserialize query string: ";

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

/// The operation's path with every `{param}` filled from its declared type.
fn concrete_path(path: &str, operation: &Value, spec: &Value) -> String {
    let mut out = path.to_owned();
    for param in operation
        .get("parameters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if param.get("in").and_then(Value::as_str) != Some("path") {
            continue;
        }
        let name = param["name"].as_str().unwrap_or_default();
        let schema = resolve(spec, &param["schema"]);
        let first_enum = schema
            .get("enum")
            .and_then(Value::as_array)
            .and_then(|values| values.first())
            .and_then(Value::as_str);
        let value = match (
            schema.get("type").and_then(Value::as_str),
            schema.get("format").and_then(Value::as_str),
            first_enum,
        ) {
            (_, _, Some(variant)) => variant,
            (_, Some("uuid"), _) => "11111111111111111111111111111111",
            (Some("integer" | "number"), _, _) => "1",
            _ => "x",
        };
        let placeholder = format!("{{{name}}}");
        if let Some(at) = out
            .to_ascii_lowercase()
            .find(&placeholder.to_ascii_lowercase())
        {
            out.replace_range(at..at + placeholder.len(), value);
        }
    }
    out
}

/// The member a query rejection names, if the response is one.
async fn rejected_member(router: &Router, method: &str, uri: &str) -> Option<String> {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    let router = router.clone();
    // A probe the binder accepts may reach a fake manager that panics
    // (`unimplemented!("fake")`): that is "not a rejection". Any other panic
    // fails the test.
    let response = match tokio::spawn(async move { router.oneshot(request).await }).await {
        Ok(response) => response.expect("response"),
        Err(e) => {
            let payload = e.into_panic();
            let fake = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .is_some_and(|m| m.contains("not implemented: fake"));
            if fake {
                return None;
            }
            std::panic::resume_unwind(payload);
        }
    };
    if response.status() != StatusCode::BAD_REQUEST {
        return None;
    }
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes);
    let rest = text.strip_prefix(REJECTION)?;
    Some(rest.split(':').next()?.to_owned())
}

fn pascal(name: &str) -> String {
    let mut chars = name.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_ascii_uppercase().to_string() + chars.as_str()
    })
}

#[tokio::test]
async fn every_typed_query_parameter_binds_ignoring_case() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if info
            .payload_as_str()
            .is_some_and(|m| m.contains("not implemented: fake"))
        {
            return;
        }
        default_hook(info);
    }));
    let spec = spec();
    let router = create_router(elevated_fake_state());
    let (mut checked, mut skipped) = (0, 0);
    for (path, item) in spec["paths"].as_object().expect("paths") {
        for (method, operation) in item.as_object().expect("path item") {
            // HEAD answers without a body to read the member from; its GET
            // twin binds the same query struct.
            if !matches!(method.as_str(), "get" | "post" | "put" | "patch" | "delete") {
                continue;
            }
            let method = method.to_uppercase();
            let base = concrete_path(path, operation, &spec);
            for param in operation
                .get("parameters")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if param.get("in").and_then(Value::as_str) != Some("query") {
                    continue;
                }
                let name = param["name"].as_str().expect("name");
                let schema = resolve(&spec, &param["schema"]);
                let typed = matches!(
                    schema.get("type").and_then(Value::as_str),
                    Some("integer" | "number" | "boolean")
                ) || schema.get("format").and_then(Value::as_str) == Some("uuid");
                if !typed {
                    continue;
                }
                // Every spelling must name THIS parameter's member, or none
                // may: a probe that names another member (a required field
                // the struct is missing, a path-less rejection) proves nothing.
                let mut named = Vec::new();
                for key in [
                    name.to_owned(),
                    pascal(name),
                    name.to_ascii_lowercase(),
                    name.to_ascii_uppercase(),
                ] {
                    let member = rejected_member(&router, &method, &format!("{base}?{key}=zz"))
                        .await
                        .filter(|m| m.eq_ignore_ascii_case(name));
                    named.push((key, member));
                }
                if named.iter().all(|(_, member)| member.is_none()) {
                    // Not modelled by the handler's struct (ignored, as ASP.NET
                    // ignores unknown keys), parsed leniently, or rejected
                    // before the query binds.
                    skipped += 1;
                    continue;
                }
                checked += 1;
                let first = named.iter().find_map(|(_, m)| m.clone());
                for (key, member) in &named {
                    assert_eq!(
                        member, &first,
                        "{method} {path}: `?{key}=zz` did not bind the member the other \
                         spellings bind (case-sensitive query binding?): {named:?}"
                    );
                }
            }
        }
    }
    println!("query casing: {checked} typed parameters checked, {skipped} not reached");
    assert!(
        checked >= MIN_PARAMS_CHECKED,
        "only {checked} query parameters were compared (floor {MIN_PARAMS_CHECKED})"
    );
}
