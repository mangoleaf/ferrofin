//! HARD GATE: `hermit-api`'s registered route table is a **superset** of the
//! vendored Jellyfin 10.11.8 OpenAPI contract.
//!
//! A real client (Wolphin) must never receive a `404` on a path the contract
//! declares. This test reads the authoritative spec straight from
//! `tests/data/jellyfin-openapi-10.11.8.json` and asserts:
//!
//! 1. **Generator fidelity** — every `(method, path)` in the JSON spec is present
//!    in the crate's embedded [`hermit_api::VENDORED_ROUTES`] table, so the
//!    generated constant can't silently drift from the spec.
//! 2. **Superset** — every vendored `(method, path)`, after the crate's
//!    axum-path normalization, is registered in [`hermit_api::routes::axum_routes`]
//!    (the exact table `create_router` mounts). Nothing in the contract is
//!    missing.
//! 3. **Live routing** — building the real router and probing a spread of
//!    vendored paths never yields `404` (it yields `501`/`401`, i.e. the route
//!    exists).

use std::collections::BTreeSet;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use hermit_api::routes::{axum_routes, normalize_contract_path};
use hermit_api::test_support::fake_state;
use hermit_api::{VENDORED_ROUTES, create_router};
use tower::ServiceExt;

/// Parses the vendored spec into a sorted set of `(method, path)` pairs, exactly
/// as the spec declares them (method lowercased, path verbatim).
fn spec_routes() -> BTreeSet<(String, String)> {
    let raw = include_str!("data/jellyfin-openapi-10.11.8.json");
    let spec: serde_json::Value = serde_json::from_str(raw).expect("spec is valid JSON");
    let paths = spec
        .get("paths")
        .and_then(serde_json::Value::as_object)
        .expect("spec has a paths object");

    let mut out = BTreeSet::new();
    for (path, item) in paths {
        let methods = item
            .as_object()
            .expect("each path item is an object of methods");
        for method in methods.keys() {
            // Skip OpenAPI non-operation keys (parameters, $ref, summary, …);
            // only real HTTP verbs are routes.
            if matches!(
                method.as_str(),
                "get" | "post" | "put" | "delete" | "patch" | "head" | "options" | "trace"
            ) {
                out.insert((method.clone(), path.clone()));
            }
        }
    }
    out
}

#[test]
fn embedded_table_covers_the_whole_spec() {
    let spec = spec_routes();
    let embedded: BTreeSet<(String, String)> = VENDORED_ROUTES
        .iter()
        .map(|(m, p)| ((*m).to_owned(), (*p).to_owned()))
        .collect();

    let missing: Vec<_> = spec.difference(&embedded).collect();
    assert!(
        missing.is_empty(),
        "VENDORED_ROUTES is missing {} spec entries: {:?}",
        missing.len(),
        missing
    );
    assert_eq!(
        spec.len(),
        VENDORED_ROUTES.len(),
        "VENDORED_ROUTES has entries not in the spec"
    );
}

#[test]
fn registered_routes_are_a_superset_of_the_contract() {
    // The normalized table the router actually mounts.
    let registered: BTreeSet<(String, String)> = axum_routes()
        .into_iter()
        .map(|(m, p)| (m.to_owned(), p))
        .collect();

    assert!(
        !registered.is_empty(),
        "no routes were registered from the contract"
    );

    // Normalize every vendored pair through the crate's public path transform and
    // assert the resulting `(method, axum_path)` is in the registered table.
    // This is the actual superset gate: not one contract route is dropped.
    let mut missing = Vec::new();
    for (method, path) in spec_routes() {
        assert!(
            matches!(method.as_str(), "get" | "post" | "delete" | "head"),
            "contract uses HTTP method {method:?} the router does not handle"
        );
        let axum_path = normalize_contract_path(&path);
        if !registered.contains(&(method.clone(), axum_path.clone())) {
            missing.push(format!("{method} {path} -> {axum_path}"));
        }
    }
    assert!(
        missing.is_empty(),
        "{} contract routes are NOT registered (hermit-api is not a superset):\n{}",
        missing.len(),
        missing.join("\n")
    );
}

#[tokio::test]
async fn probed_contract_routes_never_404() {
    let router = create_router(fake_state());

    // A spread across controllers, methods, and the tricky normalized segments.
    let probes: &[(Method, &str)] = &[
        (Method::GET, "/System/Info"),
        (Method::GET, "/System/Info/Public"),
        (Method::GET, "/Users"),
        (Method::GET, "/Artists"),
        (Method::GET, "/Items"),
        (Method::POST, "/Sessions/Playing"),
        (Method::DELETE, "/Auth/Keys/some-key"),
        (Method::GET, "/Videos/abc/Trickplay/320/0.jpg"),
        (Method::GET, "/Audio/abc/hls1/playlist/segment.mp4"),
    ];

    for (method, uri) in probes {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method.clone())
                    .uri(*uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(
            response.status(),
            StatusCode::NOT_FOUND,
            "contract route {method} {uri} returned 404 — it must be registered"
        );
    }
}
