//! HARD GATE: `ferrofin-api`'s registered route table is a **superset** of the
//! vendored Jellyfin 10.11.8 OpenAPI contract.
//!
//! A real client (Wolphin) must never receive a `404` on a path the contract
//! declares. This test reads the authoritative spec straight from
//! `tests/data/jellyfin-openapi-10.11.8.json` and asserts:
//!
//! 1. **Generator fidelity** — every `(method, path)` in the JSON spec is present
//!    in the crate's embedded [`ferrofin_api::VENDORED_ROUTES`] table, so the
//!    generated constant can't silently drift from the spec.
//! 2. **Superset** — every vendored `(method, path)`, after the crate's
//!    axum-path normalization, is registered in [`ferrofin_api::routes::axum_routes`]
//!    (the exact table `create_router` mounts). Nothing in the contract is
//!    missing.
//! 3. **Live routing** — building the real router and probing a spread of
//!    vendored paths never yields `404` (it yields `501`/`401`, i.e. the route
//!    exists).

use std::collections::BTreeSet;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use ferrofin_api::routes::{axum_routes, normalize_contract_path};
use ferrofin_api::test_support::fake_state;
use ferrofin_api::{VENDORED_ROUTES, create_router};
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
        "{} contract routes are NOT registered (ferrofin-api is not a superset):\n{}",
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

/// `REAL_ROUTES` must have no duplicate `(method, path)` rows.
///
/// Duplicates are harmless to the router (which mounts real handlers by
/// membership, not by iterating this table), but they inflate the "REAL vs 501"
/// route count and mislead the contract accounting. This guard keeps the table
/// a true set so downstream implementation-status counts stay honest.
#[test]
fn real_routes_have_no_duplicates() {
    use ferrofin_api::handlers::REAL_ROUTES;

    let mut seen = BTreeSet::new();
    let mut dups = Vec::new();
    for (method, path) in REAL_ROUTES {
        if !seen.insert((*method, *path)) {
            dups.push((*method, *path));
        }
    }
    assert!(
        dups.is_empty(),
        "REAL_ROUTES contains duplicate (method, path) rows: {dups:?}"
    );
    // Every real route must also be a genuine contract operation (no orphans).
    let vendored: BTreeSet<(String, String)> = VENDORED_ROUTES
        .iter()
        .map(|(m, p)| (m.to_string(), normalize_contract_path(p)))
        .collect();
    let orphans: Vec<_> = REAL_ROUTES
        .iter()
        .filter(|(m, p)| !vendored.contains(&(m.to_string(), (*p).to_string())))
        .collect();
    assert!(
        orphans.is_empty(),
        "REAL_ROUTES has entries absent from the vendored contract: {orphans:?}"
    );
}

/// `EXTENSION_ROUTES` (the core-vs-extension ownership manifest) must be a
/// duplicate-free set. Membership in `REAL_ROUTES` is
/// already a compile-time assertion next to the const; this guards the one
/// property a `const fn` can't cheaply express.
#[test]
fn extension_routes_have_no_duplicates() {
    use ferrofin_api::handlers::EXTENSION_ROUTES;

    let mut seen = BTreeSet::new();
    let mut dups = Vec::new();
    for (method, path, _ext) in EXTENSION_ROUTES {
        if !seen.insert((*method, *path)) {
            dups.push((*method, *path));
        }
    }
    assert!(
        dups.is_empty(),
        "EXTENSION_ROUTES contains duplicate (method, path) rows: {dups:?}"
    );
}

/// The per-row well-formedness rules of a `VERIFIED` entry (everything except
/// its membership in the route table). Empty = well-formed.
fn row_problems(v: &ferrofin_api::handlers::Verified) -> Vec<String> {
    let mut out = Vec::new();
    let is_cs = std::path::Path::new(v.upstream_file)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cs"));
    if !is_cs || v.upstream_file.contains(char::is_whitespace) {
        out.push(format!(
            "upstream_file must be a repo-relative `.cs` path, got {:?}",
            v.upstream_file
        ));
    }
    if v.upstream_method.is_empty() || v.upstream_method.contains(char::is_whitespace) {
        out.push(format!(
            "upstream_method must be one method name, got {:?}",
            v.upstream_method
        ));
    }
    if v.divergences.iter().any(|d| d.trim().is_empty()) {
        out.push("empty divergence string".to_string());
    }
    let d = v.date.as_bytes();
    let digits =
        |r: std::ops::Range<usize>| d.get(r).is_some_and(|b| b.iter().all(u8::is_ascii_digit));
    if d.len() != 10
        || d[4] != b'-'
        || d[7] != b'-'
        || !digits(0..4)
        || !digits(5..7)
        || !digits(8..10)
    {
        out.push(format!("date must be YYYY-MM-DD, got {:?}", v.date));
    }
    if v.ferrofin.is_empty() {
        out.push("ferrofin code path is empty".to_string());
    }
    out
}

/// **The API-parity record** (`handlers::VERIFIED`) is well-formed, and this
/// test prints the parity section the README publishes:
/// `N / <ops> operations deep-verified (K with recorded divergences)` plus the
/// per-controller table (`--nocapture`). Every row must be a `REAL_ROUTES`
/// operation (so a verified op is a served op), name the C# file and method it
/// was compared against (the tag and commit are pinned once, as consts), and
/// carry a `YYYY-MM-DD` date.
/// The count moves only when a row is written; a suite rewrite cannot touch it.
#[test]
fn verified_rows_are_real_operations_and_print_the_parity_line() {
    use std::collections::BTreeMap;

    use ferrofin_api::handlers::{
        EXTENSION_ROUTES, REAL_ROUTES, UPSTREAM_COMMIT, UPSTREAM_TAG, VERIFIED,
    };

    assert!(
        UPSTREAM_TAG.starts_with('v'),
        "UPSTREAM_TAG is a jellyfin release tag"
    );
    assert!(
        UPSTREAM_COMMIT.len() >= 7 && UPSTREAM_COMMIT.chars().all(|c| c.is_ascii_hexdigit()),
        "UPSTREAM_COMMIT must be a hex commit id"
    );

    let real: BTreeSet<(&str, &str)> = REAL_ROUTES.iter().copied().collect();
    let mut seen = BTreeSet::new();
    let mut problems = Vec::new();
    for v in VERIFIED {
        let key = (v.method, v.path);
        if !seen.insert(key) {
            problems.push(format!("{key:?}: duplicate row"));
        }
        if !real.contains(&key) {
            problems.push(format!("{key:?}: not a REAL_ROUTES operation"));
        }
        problems.extend(row_problems(v).into_iter().map(|p| format!("{key:?}: {p}")));
    }
    assert!(
        problems.is_empty(),
        "VERIFIED rows are malformed:\n{}",
        problems.join("\n")
    );

    // Per-controller table: controller = the operation's first OpenAPI tag.
    let raw = include_str!("data/jellyfin-openapi-10.11.8.json");
    let spec: serde_json::Value = serde_json::from_str(raw).expect("spec is valid JSON");
    let mut per: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let verified: BTreeSet<(String, String)> = VERIFIED
        .iter()
        .map(|v| (v.method.to_string(), v.path.to_string()))
        .collect();
    let extension: BTreeSet<(&str, &str)> =
        EXTENSION_ROUTES.iter().map(|(m, p, _)| (*m, *p)).collect();
    let mut total = 0;
    for (method, path) in spec_routes() {
        let tag = spec["paths"][&path][&method]["tags"][0]
            .as_str()
            .unwrap_or("(untagged)")
            .to_string();
        let norm = normalize_contract_path(&path);
        let e = per.entry(tag).or_insert((0, 0));
        e.1 += 1;
        total += 1;
        if verified.contains(&(method.clone(), norm)) {
            e.0 += 1;
        }
    }
    assert_eq!(
        per.values().map(|(v, _)| *v).sum::<usize>(),
        VERIFIED.len(),
        "every VERIFIED row must map to exactly one contract operation"
    );
    let with_div = VERIFIED
        .iter()
        .filter(|v| !v.divergences.is_empty())
        .count();
    let ext_verified = VERIFIED
        .iter()
        .filter(|v| extension.contains(&(v.method, v.path)))
        .count();
    println!(
        "\n{} / {total} operations deep-verified against Jellyfin {UPSTREAM_TAG} ({UPSTREAM_COMMIT}) — {with_div} with recorded divergences; {ext_verified} owned by compiled-in extensions\n",
        VERIFIED.len()
    );
    println!("| controller | verified / operations |\n|---|---|");
    for (tag, (v, n)) in &per {
        println!("| {tag} | {v} / {n} |");
    }
    for v in VERIFIED.iter().filter(|v| !v.divergences.is_empty()) {
        println!(
            "- {} {} — {}",
            v.method.to_uppercase(),
            v.path,
            v.divergences.join("; ")
        );
    }
    assert_eq!(
        total,
        VENDORED_ROUTES.len(),
        "spec_routes and VENDORED_ROUTES disagree"
    );
}
