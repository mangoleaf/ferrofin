//! `RouteLabels` — the per-route `controller` / `action` / `endpoint` label
//! values that give the HTTP metrics their prometheus-net parity.
//!
//! prometheus-net populates these from ASP.NET's routing — which, for Jellyfin,
//! is the OpenAPI operation's first `tag` (controller), its `operationId`
//! (action), and the route template with **no leading slash** (endpoint, e.g.
//! `Users/{userId}` — verified against the live fixture). We rebuild the same
//! mapping by parsing the vendored spec, keyed by the axum route template
//! (`MatchedPath`) so a runtime lookup by the matched path succeeds, and storing
//! the spec's own path (slash-stripped) as the endpoint value so it matches
//! Jellyfin's exact param names.

use std::collections::HashMap;

use axum::http::Method;

/// The per-route label values (all prometheus-net-parity).
#[derive(Debug, Clone)]
struct RouteMeta {
    controller: String,
    action: String,
    /// The Jellyfin-form endpoint label: the spec path with no leading slash.
    endpoint: String,
}

/// A `(method, MatchedPath) → (controller, action, endpoint)` lookup built from
/// the vendored Jellyfin OpenAPI spec. A request whose `(method, MatchedPath)`
/// is absent (non-contract routes: `/metrics`, `/health/*`, `/web`, …) misses
/// the map; the caller then emits empty controller/action and falls back to the
/// raw matched path for `endpoint`, matching prometheus-net's convention.
#[derive(Debug, Clone, Default)]
pub struct RouteLabels {
    map: HashMap<(Method, String), RouteMeta>,
}

impl RouteLabels {
    /// Builds the lookup from the vendored OpenAPI spec JSON.
    ///
    /// `normalize` maps a spec path template (`/Users/{userId}/Items`) to the
    /// axum route template that `MatchedPath` reports at runtime — pass
    /// `hermit_api::routes::normalize_contract_path`. `controller` is the
    /// operation's first `tag`; `action` is its `operationId`.
    ///
    /// A spec that is not valid JSON (or lacks a `paths` object) yields an empty
    /// lookup — every request then gets empty labels rather than failing.
    #[must_use]
    pub fn from_openapi_spec(spec_json: &str, normalize: impl Fn(&str) -> String) -> Self {
        let mut map = HashMap::new();
        let Ok(spec) = serde_json::from_str::<serde_json::Value>(spec_json) else {
            return Self { map };
        };
        let Some(paths) = spec.get("paths").and_then(serde_json::Value::as_object) else {
            return Self { map };
        };
        for (path, item) in paths {
            // Key by the axum template (== `MatchedPath`); store the spec path
            // sans leading slash as the Jellyfin-form endpoint label value.
            let key = normalize(path);
            let endpoint = path.strip_prefix('/').unwrap_or(path).to_owned();
            let Some(methods) = item.as_object() else {
                continue;
            };
            for (verb, op) in methods {
                let Some(method) = parse_method(verb) else {
                    continue;
                };
                let controller = op
                    .get("tags")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|t| t.first())
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let action = op
                    .get("operationId")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                map.insert(
                    (method, key.clone()),
                    RouteMeta {
                        controller,
                        action,
                        endpoint: endpoint.clone(),
                    },
                );
            }
        }
        Self { map }
    }

    /// Looks up the `(controller, action, endpoint)` labels for a matched
    /// `(method, matched_path)`. Returns `None` when the route is not a contract
    /// operation — the caller then emits empty controller/action and uses the
    /// raw matched path as `endpoint`.
    #[must_use]
    pub fn lookup(&self, method: &Method, matched_path: &str) -> Option<(&str, &str, &str)> {
        self.map
            .get(&(method.clone(), matched_path.to_owned()))
            .map(|m| {
                (
                    m.controller.as_str(),
                    m.action.as_str(),
                    m.endpoint.as_str(),
                )
            })
    }
}

/// Parses an OpenAPI verb key (`"get"`, `"post"`, …) into an [`Method`],
/// ignoring non-verb keys (`parameters`, `$ref`, `summary`, …).
fn parse_method(verb: &str) -> Option<Method> {
    match verb {
        "get" => Some(Method::GET),
        "post" => Some(Method::POST),
        "put" => Some(Method::PUT),
        "delete" => Some(Method::DELETE),
        "patch" => Some(Method::PATCH),
        "head" => Some(Method::HEAD),
        "options" => Some(Method::OPTIONS),
        "trace" => Some(Method::TRACE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"{
        "paths": {
            "/Items/{itemId}": {
                "get": { "tags": ["Items"], "operationId": "GetItem" },
                "parameters": []
            },
            "/Users/{userId}/Items": {
                "post": { "tags": ["UserLibrary"], "operationId": "MarkPlayed" }
            }
        }
    }"#;

    #[test]
    fn builds_and_looks_up_controller_action_endpoint() {
        // Identity normalizer for the test — real wiring passes the axum transform.
        let labels = RouteLabels::from_openapi_spec(SPEC, str::to_owned);
        // endpoint is the spec path with the leading slash stripped (Jellyfin form).
        assert_eq!(
            labels.lookup(&Method::GET, "/Items/{itemId}"),
            Some(("Items", "GetItem", "Items/{itemId}"))
        );
        assert_eq!(
            labels.lookup(&Method::POST, "/Users/{userId}/Items"),
            Some(("UserLibrary", "MarkPlayed", "Users/{userId}/Items"))
        );
    }

    #[test]
    fn unmatched_route_is_none() {
        let labels = RouteLabels::from_openapi_spec(SPEC, str::to_owned);
        assert_eq!(labels.lookup(&Method::GET, "/metrics"), None);
        // Right path, wrong method → miss.
        assert_eq!(labels.lookup(&Method::DELETE, "/Items/{itemId}"), None);
    }

    #[test]
    fn invalid_spec_yields_empty_lookup() {
        let labels = RouteLabels::from_openapi_spec("not json", str::to_owned);
        assert_eq!(labels.lookup(&Method::GET, "/anything"), None);
    }
}
