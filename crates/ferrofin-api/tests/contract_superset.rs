//! HARD GATE: `ferrofin-api`'s registered route table is a **superset** of the
//! vendored Jellyfin 10.11.8 OpenAPI contract.
//!
//! A real client (Wolphin) must never receive a `404` on a path the contract
//! declares. This test reads the authoritative spec straight from
//! `contracts/jellyfin-openapi-10.11.8.json` and asserts:
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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use ferrofin_api::handlers::{Verified, VerifiedSubsystem};
use ferrofin_api::routes::{axum_routes, normalize_contract_path};
use ferrofin_api::test_support::fake_state;
use ferrofin_api::{VENDORED_ROUTES, create_router};
use tower::ServiceExt;

/// Parses the vendored spec into a sorted set of `(method, path)` pairs, exactly
/// as the spec declares them (method lowercased, path verbatim).
fn spec_routes() -> BTreeSet<(String, String)> {
    let raw = include_str!("../../../contracts/jellyfin-openapi-10.11.8.json");
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

/// Every operation the newer vendored contract (Jellyfin 12.1) adds over the
/// 10.11.8 pin is served too: the server reports a 12.x version, so a client may
/// call it. Each one is probed on the live router with its path parameters
/// filled in, and must not `404`.
#[tokio::test]
async fn newer_contract_operations_never_404() {
    let raw = include_str!("../../../contracts/jellyfin-openapi-12.1.0.json");
    let spec: serde_json::Value = serde_json::from_str(raw).expect("spec is valid JSON");
    let pinned = spec_routes();
    let router = create_router(fake_state());
    let mut added = 0;
    for (path, item) in spec["paths"].as_object().expect("paths") {
        for method in item.as_object().expect("path item").keys() {
            let verb = matches!(
                method.as_str(),
                "get" | "post" | "put" | "delete" | "patch" | "head" | "options" | "trace"
            );
            if !verb || pinned.contains(&(method.clone(), path.clone())) {
                continue;
            }
            assert!(
                matches!(method.as_str(), "get" | "post" | "delete" | "head"),
                "12.1 uses HTTP method {method:?} the router does not handle"
            );
            added += 1;
            let uri: String = path
                .split('/')
                .map(|seg| {
                    if seg.starts_with('{') {
                        "00000000-0000-0000-0000-0000000000a1"
                    } else {
                        seg
                    }
                })
                .collect::<Vec<_>>()
                .join("/");
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method.to_uppercase().as_str())
                        .uri(&uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(
                !matches!(
                    response.status(),
                    StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                ),
                "12.1 operation {method} {path} is not routed: {}",
                response.status()
            );
        }
    }
    assert!(
        added > 0,
        "the 12.1 spec adds GET /Items/{{itemId}}/Collections"
    );
    // The probe can tell: an unrouted path still 404s through the auth layers.
    let unrouted = router
        .oneshot(
            Request::builder()
                .uri("/Items/00000000-0000-0000-0000-0000000000a1/NoSuchRoute")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unrouted.status(), StatusCode::NOT_FOUND);
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

/// Earlier-pin entries (`earlier`, as `(row key, tag)`, from the list named
/// `list`) must each name a distinct row of `dates` (row key → its `date`),
/// carry a release tag other than `UPSTREAM_TAG`, and belong to a row dated
/// before `UPSTREAM_PIN_DATE` (a later row was compared against the current
/// pin). Conversely, every row dated before `UPSTREAM_PIN_DATE` must be listed:
/// it was compared before the pin moved, so counting it against the current
/// pin would claim a comparison nobody made.
fn earlier_pin_problems<K: Ord + Copy + std::fmt::Debug>(
    dates: &BTreeMap<K, &str>,
    earlier: impl IntoIterator<Item = (K, &'static str)>,
    list: &str,
) -> Vec<String> {
    use ferrofin_api::handlers::{UPSTREAM_PIN_DATE, UPSTREAM_TAG};

    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for (key, tag) in earlier {
        if !seen.insert(key) {
            out.push(format!("{key:?}: listed twice in {list}"));
        }
        match dates.get(&key) {
            None => out.push(format!("{key:?}: in {list} but not a row of its table")),
            Some(date) if *date >= UPSTREAM_PIN_DATE => out.push(format!(
                "{key:?}: dated {date}, on or after the move to {UPSTREAM_TAG} \
                 ({UPSTREAM_PIN_DATE}), but still in {list}"
            )),
            Some(_) => {}
        }
        if !tag.starts_with('v') || tag == UPSTREAM_TAG {
            out.push(format!(
                "{key:?}: {tag:?} is not an earlier jellyfin release tag"
            ));
        }
    }
    for (key, date) in dates {
        if *date < UPSTREAM_PIN_DATE && !seen.contains(key) {
            out.push(format!(
                "{key:?}: dated {date}, before the move to {UPSTREAM_TAG} \
                 ({UPSTREAM_PIN_DATE}), but not in {list}: list it under the tag it \
                 was compared against, or re-check it and move its date"
            ));
        }
    }
    out
}

/// The well-formedness rules every parity row obeys, operation or subsystem
/// (everything except its key). Empty = well-formed.
fn record_problems(
    upstream_file: &str,
    upstream_method: &str,
    ferrofin: &str,
    date: &str,
    divergences: &[&str],
) -> Vec<String> {
    let mut out = Vec::new();
    let is_cs = std::path::Path::new(upstream_file)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cs"));
    if !is_cs || upstream_file.contains(char::is_whitespace) {
        out.push(format!(
            "upstream_file must be a repo-relative `.cs` path, got {upstream_file:?}"
        ));
    }
    if upstream_method.is_empty() || upstream_method.contains(char::is_whitespace) {
        out.push(format!(
            "upstream_method must be one method name, got {upstream_method:?}"
        ));
    }
    if divergences.iter().any(|d| d.trim().is_empty()) {
        out.push("empty divergence string".to_string());
    }
    let d = date.as_bytes();
    let digits =
        |r: std::ops::Range<usize>| d.get(r).is_some_and(|b| b.iter().all(u8::is_ascii_digit));
    if d.len() != 10
        || d[4] != b'-'
        || d[7] != b'-'
        || !digits(0..4)
        || !digits(5..7)
        || !digits(8..10)
    {
        out.push(format!("date must be YYYY-MM-DD, got {date:?}"));
    }
    if ferrofin.is_empty() {
        out.push("ferrofin code path is empty".to_string());
    }
    out
}

/// Subsystem rows must each use a distinct `SUBSYSTEM_KEYS` key, and every
/// operation row's `refs` must name one of those rows.
fn ref_problems(ops: &[Verified], subsystems: &[VerifiedSubsystem]) -> Vec<String> {
    use ferrofin_api::handlers::SUBSYSTEM_KEYS;

    let mut out = Vec::new();
    let mut keys = BTreeSet::new();
    for s in subsystems {
        if !SUBSYSTEM_KEYS.contains(&s.key) {
            out.push(format!(
                "subsystem {:?}: not one of SUBSYSTEM_KEYS {SUBSYSTEM_KEYS:?}",
                s.key
            ));
        }
        if !keys.insert(s.key) {
            out.push(format!("subsystem {:?}: duplicate row", s.key));
        }
    }
    for v in ops {
        for r in v.refs.iter().filter(|r| !keys.contains(*r)) {
            out.push(format!(
                "{:?}: refs {r:?}, which is not a VERIFIED_SUBSYSTEMS row",
                (v.method, v.path)
            ));
        }
    }
    out
}

/// Whether `name` occurs in `src` as a whole identifier (so `GetItem` does not
/// match `GetItems`).
fn names_identifier(src: &str, name: &str) -> bool {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    src.match_indices(name).any(|(i, _)| {
        !src[..i].chars().next_back().is_some_and(is_ident)
            && !src[i + name.len()..].chars().next().is_some_and(is_ident)
    })
}

/// Each `(row label, upstream_file, upstream_method)` must name a file under
/// the upstream checkout `dir` that contains `upstream_method`.
fn upstream_problems<'a>(
    dir: &Path,
    rows: impl IntoIterator<Item = (String, &'a str, &'a str)>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (label, file, method) in rows {
        match std::fs::read_to_string(dir.join(file)) {
            Err(e) => out.push(format!(
                "{label}: upstream_file {file:?} is not readable under {}: {e}",
                dir.display()
            )),
            Ok(src) if !names_identifier(&src, method) => out.push(format!(
                "{label}: upstream_method {method:?} does not appear in {file}"
            )),
            Ok(_) => {}
        }
    }
    out
}

/// Every rule the parity record obeys across `VERIFIED`, `VERIFIED_SUBSYSTEMS`
/// and their earlier-pin lists, except the upstream-checkout ones.
/// Empty = well-formed.
fn parity_record_problems() -> Vec<String> {
    use ferrofin_api::handlers::{
        REAL_ROUTES, VERIFIED, VERIFIED_AGAINST_EARLIER_PIN, VERIFIED_SUBSYSTEMS,
        VERIFIED_SUBSYSTEMS_AGAINST_EARLIER_PIN,
    };

    let real: BTreeSet<(&str, &str)> = REAL_ROUTES.iter().copied().collect();
    let mut dates = BTreeMap::new();
    let mut problems = Vec::new();
    for v in VERIFIED {
        let key = (v.method, v.path);
        if dates.insert(key, v.date).is_some() {
            problems.push(format!("{key:?}: duplicate row"));
        }
        if !real.contains(&key) {
            problems.push(format!("{key:?}: not a REAL_ROUTES operation"));
        }
        let rp = record_problems(
            v.upstream_file,
            v.upstream_method,
            v.ferrofin,
            v.date,
            v.divergences,
        );
        problems.extend(rp.into_iter().map(|p| format!("{key:?}: {p}")));
    }
    let subsystem_dates: BTreeMap<&str, &str> = VERIFIED_SUBSYSTEMS
        .iter()
        .map(|s| (s.key, s.date))
        .collect();
    for s in VERIFIED_SUBSYSTEMS {
        let rp = record_problems(
            s.upstream_file,
            s.upstream_method,
            s.ferrofin,
            s.date,
            s.divergences,
        );
        problems.extend(
            rp.into_iter()
                .map(|p| format!("subsystem {:?}: {p}", s.key)),
        );
    }
    problems.extend(ref_problems(VERIFIED, VERIFIED_SUBSYSTEMS));
    problems.extend(earlier_pin_problems(
        &dates,
        VERIFIED_AGAINST_EARLIER_PIN
            .iter()
            .map(|&(m, p, tag)| ((m, p), tag)),
        "VERIFIED_AGAINST_EARLIER_PIN",
    ));
    problems.extend(earlier_pin_problems(
        &subsystem_dates,
        VERIFIED_SUBSYSTEMS_AGAINST_EARLIER_PIN.iter().copied(),
        "VERIFIED_SUBSYSTEMS_AGAINST_EARLIER_PIN",
    ));
    problems
}

/// **The API-parity record** (`handlers::VERIFIED`) is well-formed, and this
/// test prints the parity section the README publishes:
/// `N / <ops> operations deep-verified against <UPSTREAM_TAG>, plus M compared
/// against an earlier release` plus the per-controller table (`--nocapture`).
/// Every row must be a `REAL_ROUTES` operation (so a verified op is a served
/// op), name the C# file and method it was compared against (the tag and commit
/// are pinned once, as consts, with `VERIFIED_AGAINST_EARLIER_PIN` naming the
/// rows compared against an earlier tag), and carry a `YYYY-MM-DD` date.
/// The counts move only when a row is written or re-checked, or the pin moves;
/// a suite rewrite cannot touch them.
#[test]
fn verified_rows_are_real_operations_and_print_the_parity_line() {
    use ferrofin_api::handlers::{
        EXTENSION_ROUTES, UPSTREAM_COMMIT, UPSTREAM_TAG, VERIFIED, VERIFIED_AGAINST_EARLIER_PIN,
        VERIFIED_SUBSYSTEMS,
    };

    assert!(
        UPSTREAM_TAG.starts_with('v'),
        "UPSTREAM_TAG is a jellyfin release tag"
    );
    assert!(
        UPSTREAM_COMMIT.len() >= 7 && UPSTREAM_COMMIT.chars().all(|c| c.is_ascii_hexdigit()),
        "UPSTREAM_COMMIT must be a hex commit id"
    );

    let problems = parity_record_problems();
    assert!(
        problems.is_empty(),
        "VERIFIED rows are malformed:\n{}",
        problems.join("\n")
    );

    // Per-controller table: controller = the operation's first OpenAPI tag.
    let raw = include_str!("../../../contracts/jellyfin-openapi-10.11.8.json");
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
        "\n{} / {total} operations deep-verified against Jellyfin {UPSTREAM_TAG} ({UPSTREAM_COMMIT}), plus {} compared against an earlier release and owed a re-check — of all {} rows, {with_div} with recorded divergences; {ext_verified} owned by compiled-in extensions; {} subsystem rows (not counted above)\n",
        VERIFIED.len() - VERIFIED_AGAINST_EARLIER_PIN.len(),
        VERIFIED_AGAINST_EARLIER_PIN.len(),
        VERIFIED.len(),
        VERIFIED_SUBSYSTEMS.len()
    );
    println!("| controller | rows, any pin / operations |\n|---|---|");
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

/// With `FERROFIN_UPSTREAM_DIR` naming a Jellyfin checkout at
/// `UPSTREAM_COMMIT`, every `VERIFIED` and `VERIFIED_SUBSYSTEMS` row names an
/// upstream file that exists there and a method that appears in it. Skips with
/// a message when the variable is unset or the checkout is at another commit,
/// so CI runs without the checkout.
#[test]
fn verified_rows_name_existing_upstream_code() {
    use ferrofin_api::handlers::{UPSTREAM_COMMIT, UPSTREAM_TAG, VERIFIED, VERIFIED_SUBSYSTEMS};

    let Ok(dir) = std::env::var("FERROFIN_UPSTREAM_DIR") else {
        eprintln!(
            "SKIP: set FERROFIN_UPSTREAM_DIR to a jellyfin checkout at {UPSTREAM_COMMIT} \
             ({UPSTREAM_TAG}) to check VERIFIED rows against the upstream code"
        );
        return;
    };
    let dir = PathBuf::from(dir);
    let head = Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if !head.starts_with(UPSTREAM_COMMIT) {
        eprintln!(
            "SKIP: FERROFIN_UPSTREAM_DIR {} is at {head:?}, not UPSTREAM_COMMIT \
             {UPSTREAM_COMMIT} ({UPSTREAM_TAG})",
            dir.display()
        );
        return;
    }
    let rows = VERIFIED
        .iter()
        .map(|v| {
            (
                format!("{} {}", v.method, v.path),
                v.upstream_file,
                v.upstream_method,
            )
        })
        .chain(VERIFIED_SUBSYSTEMS.iter().map(|s| {
            (
                format!("subsystem {}", s.key),
                s.upstream_file,
                s.upstream_method,
            )
        }));
    let problems = upstream_problems(&dir, rows);
    assert!(
        problems.is_empty(),
        "VERIFIED rows name upstream code missing at {UPSTREAM_COMMIT}:\n{}",
        problems.join("\n")
    );
}

#[test]
fn upstream_problems_name_the_row_with_a_missing_file_or_method() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = "Jellyfin.Api/Controllers/ItemsController.cs";
    std::fs::create_dir_all(dir.path().join("Jellyfin.Api/Controllers")).expect("mkdir");
    std::fs::write(
        dir.path().join(file),
        "public ActionResult<QueryResult<BaseItemDto>> GetItems(Guid? userId)",
    )
    .expect("write");

    let found = upstream_problems(dir.path(), [("ok".to_string(), file, "GetItems")]);
    assert!(found.is_empty(), "{found:?}");
    let problems = upstream_problems(
        dir.path(),
        [
            ("get /Items".to_string(), file, "GetItem"),
            ("get /Gone".to_string(), "Jellyfin.Api/Gone.cs", "GetItems"),
        ],
    );
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(problems[0].starts_with("get /Items: upstream_method \"GetItem\""));
    assert!(problems[1].starts_with("get /Gone: upstream_file"));
}

#[test]
fn earlier_pin_rejects_a_row_dated_on_or_after_the_pin_move() {
    use ferrofin_api::handlers::UPSTREAM_PIN_DATE;

    let dates = BTreeMap::from([("before", "2026-01-01"), ("after", UPSTREAM_PIN_DATE)]);
    let problems = earlier_pin_problems(
        &dates,
        [("before", "v12.0-rc7"), ("after", "v12.0-rc7")],
        "EARLIER",
    );
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].starts_with("\"after\": dated"), "{problems:?}");
}

#[test]
fn earlier_pin_requires_every_row_dated_before_the_pin_move() {
    use ferrofin_api::handlers::UPSTREAM_PIN_DATE;

    let dates = BTreeMap::from([("before", "2026-01-01"), ("after", UPSTREAM_PIN_DATE)]);
    let problems = earlier_pin_problems(&dates, [], "EARLIER");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with("\"before\": dated 2026-01-01, before the move"),
        "{problems:?}"
    );
}

#[test]
fn refs_must_name_a_subsystem_row_with_a_known_key() {
    let subsystem = |key| VerifiedSubsystem {
        key,
        upstream_file: "Jellyfin.Server.Implementations/Item/BaseItemRepository.cs",
        upstream_method: "TranslateQuery",
        ferrofin: "translate_query.rs",
        date: "2026-10-19",
        divergences: &[],
    };
    let op = Verified {
        method: "get",
        path: "/Items",
        upstream_file: "Jellyfin.Api/Controllers/ItemsController.cs",
        upstream_method: "GetItems",
        ferrofin: "handlers/items.rs",
        date: "2026-10-19",
        divergences: &[],
        refs: &["query-translation", "user-data"],
    };

    let subsystems = [subsystem("query-translation"), subsystem("user-data")];
    let found = ref_problems(std::slice::from_ref(&op), &subsystems);
    assert!(found.is_empty(), "{found:?}");
    let problems = ref_problems(
        std::slice::from_ref(&op),
        &[subsystem("query-translation"), subsystem("bogus")],
    );
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(problems[0].starts_with("subsystem \"bogus\": not one of SUBSYSTEM_KEYS"));
    assert!(problems[1].contains("refs \"user-data\""), "{problems:?}");
}
