//! Contract-wide guard: every JSON-body operation binds its member names
//! ignoring case, as ASP.NET's MVC binder does (`PropertyNameCaseInsensitive`).
//!
//! The oracle needs no handler state. For each operation in the vendored spec
//! whose request body is a JSON object (or an array of objects), the test
//! probes its members ONE AT A TIME, each in a body that names only that member
//! (nested objects and arrays of objects are followed two levels down), with
//! the value `[[[]]]`. Bound PascalCase, the binder answers 400 at that member
//! (`$.Member`, `$.Outer[0].Inner`) when the member's type rejects the value —
//! most do; a `serde_json::Value`, a nested-array or a lenient member accepts
//! it and is skipped. The same body with the member names re-cased (camelCase,
//! lowercase, UPPERCASE) must fail at the same key: a case-sensitive binder
//! would ignore the re-cased names as unknown and answer something else. One
//! member per body also keeps the result independent of member order.
//!
//! An operation none of whose members can be probed must be listed in
//! `KNOWN_UNREACHABLE`, so the coverage claim stays honest.

use std::collections::BTreeSet;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::test_support::elevated_fake_state;
use serde_json::{Map, Value, json};
use tower::ServiceExt;

/// How deep nested members are probed.
const MAX_DEPTH: usize = 2;

fn spec() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/jellyfin-openapi-10.11.8.json"
    ))
    .expect("spec")
}

/// Follows a `$ref` (and a single-entry `allOf`/`oneOf` wrapper) to its schema.
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

/// One step of a member path: an object member or "the first array element".
#[derive(Clone, Debug)]
enum Step {
    Member(String),
    Element,
}

/// Every member path under an object schema, nested ones included.
fn member_paths(spec: &Value, schema: &Value, prefix: &[Step], depth: usize) -> Vec<Vec<Step>> {
    let schema = resolve(spec, schema);
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, member) in properties {
        let mut path = prefix.to_vec();
        path.push(Step::Member(name.clone()));
        out.push(path.clone());
        if depth >= MAX_DEPTH {
            continue;
        }
        let member = resolve(spec, member);
        if member.get("type").and_then(Value::as_str) == Some("array") {
            if let Some(items) = member.get("items") {
                let mut element = path.clone();
                element.push(Step::Element);
                out.extend(member_paths(spec, items, &element, depth + 1));
            }
        } else {
            out.extend(member_paths(spec, member, &path, depth + 1));
        }
    }
    out
}

/// A body naming only `path` (member names passed through `case`), ending in
/// the poison value.
fn body_for(path: &[Step], case: fn(&str) -> String) -> Value {
    match path.split_first() {
        None => json!([[[]]]),
        Some((Step::Element, rest)) => json!([body_for(rest, case)]),
        Some((Step::Member(name), rest)) => {
            let mut object = Map::new();
            object.insert(case(name), body_for(rest, case));
            Value::Object(object)
        }
    }
}

/// The request-body schema of a JSON-body operation and whether the body is a
/// top-level array, or `None` when the operation takes no JSON object body.
fn body_schema<'a>(spec: &'a Value, operation: &'a Value) -> Option<(&'a Value, bool)> {
    let content = operation.get("requestBody")?.get("content")?;
    let schema = ["application/json", "text/json", "application/*+json"]
        .iter()
        .find_map(|t| content.get(*t))?
        .get("schema")?;
    let schema = resolve(spec, schema);
    if schema.get("type").and_then(Value::as_str) == Some("array") {
        return Some((schema.get("items")?, true));
    }
    Some((schema, false))
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
        let value = match (
            schema.get("type").and_then(Value::as_str),
            schema.get("format").and_then(Value::as_str),
        ) {
            (_, Some("uuid")) => "11111111111111111111111111111111",
            (Some("integer" | "number"), _) => "1",
            _ => "x",
        };
        // The spec's parameter name and its path placeholder can differ in
        // case (`id` vs `{Id}`).
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

/// The status and the member key (`$.…`) of a binder rejection, if any.
///
/// Runs on its own task: a probe the binder accepts reaches the handler, and a
/// fake manager behind it may panic (`unimplemented!`); that is reported as a
/// non-rejection, `(500, None)`.
async fn member_error(
    router: &Router,
    method: &str,
    uri: &str,
    body: &Value,
) -> (StatusCode, Option<String>) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    let router = router.clone();
    let Ok(response) = tokio::spawn(async move { router.oneshot(request).await }).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, None);
    };
    let response = response.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let key = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|doc| doc.get("errors")?.as_object().cloned())
        .and_then(|errors| {
            errors
                .keys()
                .find(|k| k.starts_with("$.") || k.starts_with("$["))
                .cloned()
        });
    (status, key)
}

fn camel(name: &str) -> String {
    let mut chars = name.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_ascii_lowercase().to_string() + chars.as_str()
    })
}

/// The key a PascalCase probe of `path` should fail at, as a prefix
/// (`$.Outer[0].Inner` or `$[0].Member`).
fn expected_prefix(path: &[Step], top_array: bool) -> String {
    let mut key = String::from("$");
    if top_array {
        key.push_str("[0]");
    }
    for step in path {
        match step {
            Step::Member(name) => {
                key.push('.');
                key.push_str(name);
            }
            Step::Element => key.push_str("[0]"),
        }
    }
    key
}

/// JSON-body operations the oracle cannot reach, with the reason. Empty: every
/// one is checked.
const KNOWN_UNREACHABLE: &[&str] = &[];

/// The member paths the oracle compared when it was written (2026-10-07). A
/// lower count means members quietly moved into the skip bucket; raise this
/// when the contract or the DTOs grow.
const MIN_MEMBERS_CHECKED: usize = 3396;

/// Every JSON-body operation binds each re-cased member name to the same
/// member, and the oracle reaches every operation.
#[tokio::test]
async fn every_json_body_member_binds_ignoring_case() {
    // A probe the binder accepts reaches a fake manager that panics
    // `unimplemented!("fake")`; those are expected, so keep them out of the
    // output and let every other panic through.
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
    let mut unreachable = BTreeSet::new();
    let (mut operations, mut members, mut skipped) = (0, 0, 0);
    for (path, item) in spec["paths"].as_object().expect("paths") {
        for (method, operation) in item.as_object().expect("path item") {
            if !matches!(method.as_str(), "post" | "put" | "patch" | "delete") {
                continue;
            }
            // `/System/Configuration/{key}` is case-sensitive by design
            // (`extract::bind_document`); its spec schema has no members anyway.
            let Some((schema, top_array)) = body_schema(&spec, operation) else {
                continue;
            };
            let paths = member_paths(&spec, schema, &[], 0);
            if paths.is_empty() {
                continue;
            }
            let method = method.to_uppercase();
            let uri = concrete_path(path, operation, &spec);
            let wrap = |body: Value| if top_array { json!([body]) } else { body };
            let mut reached = false;
            for member in paths {
                let pascal = wrap(body_for(&member, str::to_owned));
                let (status, key) = member_error(&router, &method, &uri, &pascal).await;
                let prefix = expected_prefix(&member, top_array);
                let Some(key) = key.filter(|k| {
                    status == StatusCode::BAD_REQUEST
                        && (k == &prefix
                            || k.starts_with(&format!("{prefix}."))
                            || k.starts_with(&format!("{prefix}[")))
                }) else {
                    // The member accepts the poison value (or a nested schema
                    // member the DTO does not model); nothing to compare.
                    skipped += 1;
                    continue;
                };
                reached = true;
                members += 1;
                for case in [
                    camel as fn(&str) -> String,
                    |n: &str| n.to_ascii_lowercase(),
                    |n: &str| n.to_ascii_uppercase(),
                ] {
                    let recased = wrap(body_for(&member, case));
                    let got = member_error(&router, &method, &uri, &recased).await;
                    assert_eq!(
                        (got.0, got.1.as_deref()),
                        (StatusCode::BAD_REQUEST, Some(key.as_str())),
                        "{method} {path}: the re-cased body {recased} did not bind \
                         member {key} (case-sensitive binding?)"
                    );
                }
            }
            if reached {
                operations += 1;
            } else {
                unreachable.insert(format!("{method} {path}"));
            }
        }
    }
    println!(
        "json body casing: {operations} operations, {members} members checked \
         ({skipped} accept the probe value), {} operations unreachable",
        unreachable.len()
    );
    let known: BTreeSet<String> = KNOWN_UNREACHABLE.iter().map(|s| (*s).to_owned()).collect();
    assert_eq!(
        unreachable, known,
        "the set of JSON-body operations the casing oracle cannot reach changed"
    );
    assert!(
        members >= MIN_MEMBERS_CHECKED,
        "only {members} member paths were compared (floor {MIN_MEMBERS_CHECKED})"
    );
}
