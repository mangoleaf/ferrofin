//! Route-table plumbing: the shared `not_implemented` stub and the axum-path
//! normalization that lets every vendored Jellyfin path register on axum 0.8.
//!
//! Two of Jellyfin's path conventions don't fit axum's `matchit` router, so the
//! vendored path is normalized to an equivalent axum pattern before it is
//! registered. The transform is captured in [`to_axum_path`] and reused by both
//! the router builder and the contract-superset test, so they can never drift.

use axum::response::IntoResponse;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use crate::contract_routes::VENDORED_ROUTES;
use crate::error::ApiError;

/// The shared handler for every contract route that has no ported handler yet.
///
/// Returns `501 Not Implemented`. As First-Light and later waves port real
/// handlers, their routes are registered over this stub; the rest keep returning
/// `501` so a known route never surprises a client with a `404`.
///
pub async fn not_implemented() -> impl IntoResponse {
    ApiError::NotImplemented
}

/// Rewrites a vendored Jellyfin path into an equivalent axum 0.8 route pattern.
///
/// Two normalizations are applied, both structure-preserving (they change only
/// how a segment is *captured*, never which URLs match):
///
/// 1. **One parameter per segment.** axum's router rejects a segment holding
///    more than one placeholder (or a placeholder next to a literal), e.g.
///    `{segmentId}.{container}`. Such a segment collapses to a single capture of
///    its first placeholder (`{segmentId}`), which still matches the whole
///    segment; the handler parses the trailing literal itself.
/// 2. **Positional parameter-name agreement.** axum keys its route tree on the
///    parameter *name* at each position, so two vendored paths that differ only
///    in a placeholder's name at the same position (e.g.
///    `/MediaSegmentsApi/{itemId}` vs `/MediaSegmentsApi/{segmentId}`) would
///    conflict. Every placeholder at a given position is renamed to the
///    first name seen there, using `canon` as the running position → name map.
///
/// Jellyfin's `{itemId}` maps directly to axum `{itemId}`; case-sensitive
/// literal segments are preserved verbatim.
fn to_axum_path(path: &str, canon: &mut BTreeMap<String, String>) -> String {
    let mut prefix_key = String::new();
    let mut out = String::new();
    for segment in path.split('/') {
        // The empty leading segment (before the first '/') becomes the leading
        // '/' of the output; skip re-emitting a '/' for it.
        if !out.is_empty() || !segment.is_empty() {
            out.push('/');
        }
        if is_param_segment(segment) {
            // First placeholder name in this segment (guaranteed present).
            let first = segment
                .split(['{', '}'])
                .nth(1)
                .expect("param segment has a placeholder");
            prefix_key.push_str("/{}");
            let name = canon
                .entry(prefix_key.clone())
                .or_insert_with(|| first.to_owned());
            out.push('{');
            out.push_str(name);
            out.push('}');
        } else {
            prefix_key.push('/');
            prefix_key.push_str(segment);
            out.push_str(segment);
        }
    }
    out
}

/// Whether a path segment contains at least one `{placeholder}`.
fn is_param_segment(segment: &str) -> bool {
    segment.contains('{')
}

/// Normalizes a single Jellyfin contract path into its axum route pattern,
/// using the same positional param-name canonicalization the whole table uses.
///
/// This is the standalone entry point the contract-superset test calls to check
/// that each vendored path maps into [`axum_routes`]. It seeds the canonical
/// name map from the full vendored table first, so a path's params are renamed
/// consistently with how the router registered them (the first name seen at each
/// position across the *whole* table wins, not just within this one path).
#[must_use]
pub fn normalize_contract_path(path: &str) -> String {
    let mut canon = canonical_name_map();
    to_axum_path(path, &mut canon)
}

/// Builds the position → canonical-name map by replaying the full vendored table
/// in its declared order, so lookups match the router's registration exactly.
fn canonical_name_map() -> BTreeMap<String, String> {
    let mut canon: BTreeMap<String, String> = BTreeMap::new();
    for (_, path) in VENDORED_ROUTES {
        let _ = to_axum_path(path, &mut canon);
    }
    canon
}

/// The normalized axum route table derived from the vendored contract.
///
/// Applies [`to_axum_path`] to every vendored path and de-duplicates the
/// resulting `(method, axum_path)` pairs. Two vendored paths can normalize to the
/// same axum path (different methods on what becomes one pattern); the router
/// registers each `(method, path)` once. Order is deterministic (sorted).
#[must_use]
pub fn axum_routes() -> Vec<(&'static str, String)> {
    let mut canon: BTreeMap<String, String> = BTreeMap::new();
    // De-dup via a set keyed on the normalized `(axum_path, method)` pair.
    let mut seen: BTreeSet<(String, &'static str)> = BTreeSet::new();
    for (method, path) in VENDORED_ROUTES {
        let axum_path = to_axum_path(path, &mut canon);
        seen.insert((axum_path, method));
    }
    seen.into_iter()
        .map(|(path, method)| (method, path))
        .collect()
}

/// One segment of a registered route: a literal path component (in its canonical
/// case) or a captured parameter.
enum Seg {
    /// A literal segment, e.g. `Localization` — matched case-insensitively.
    Literal(String),
    /// A single-segment parameter, e.g. `{itemId}` — matches any one segment.
    Param,
    /// A catch-all parameter, e.g. `{*path}` — matches the remaining segments.
    Wildcard,
}

/// The registered routes, parsed into segments, for case-insensitive matching.
///
/// A path is looked up by its first segment and its segment count, which narrows
/// the table to the few routes sharing both, so the lookup that runs on every
/// request is a binary search rather than a walk of the table. The search
/// compares case-insensitively, so the path needs no lowercased copy.
struct RouteIndex {
    /// Every route, in registration order. Where two routes fit a path the
    /// earlier one wins, which makes a literal outrank a parameter at the same
    /// position: [`axum_routes`] sorts by path, and `{` sorts after every letter.
    routes: Vec<Vec<Seg>>,
    /// `(first literal lowercased, segment count, index into routes)`, sorted.
    keyed: Vec<(String, usize, usize)>,
    /// The routes that key cannot find, as indices into `routes`: a parameter
    /// in first position, or a catch-all, whose segment count varies.
    unkeyed: Vec<usize>,
}

impl RouteIndex {
    /// Indexes `paths`, given in registration order; a repeat is dropped.
    fn new(paths: impl IntoIterator<Item = String>) -> Self {
        let mut seen = BTreeSet::new();
        let mut routes = Vec::new();
        for path in paths {
            if !seen.insert(path.clone()) {
                continue;
            }
            let route: Vec<Seg> = segments(&path)
                .map(|s| {
                    if s.starts_with("{*") {
                        Seg::Wildcard
                    } else if s.starts_with('{') {
                        Seg::Param
                    } else {
                        Seg::Literal(s.to_owned())
                    }
                })
                .collect();
            routes.push(route);
        }
        let mut keyed = Vec::new();
        let mut unkeyed = Vec::new();
        for (index, route) in routes.iter().enumerate() {
            match route.first() {
                Some(Seg::Literal(head)) if !matches!(route.last(), Some(Seg::Wildcard)) => {
                    keyed.push((head.to_ascii_lowercase(), route.len(), index));
                }
                _ => unkeyed.push(index),
            }
        }
        keyed.sort_unstable();
        Self {
            routes,
            keyed,
            unkeyed,
        }
    }

    /// The first route in registration order that `path` fits.
    fn find(&self, path: &str) -> Option<&[Seg]> {
        let head = segments(path).next().unwrap_or_default();
        let count = segments(path).count();
        let order = |(key, len, _): &(String, usize, usize)| {
            case_insensitive_order(key, head).then(len.cmp(&count))
        };
        let start = self.keyed.partition_point(|key| order(key).is_lt());
        let end = self.keyed.partition_point(|key| order(key).is_le());
        self.keyed[start..end]
            .iter()
            .map(|&(_, _, index)| index)
            .chain(self.unkeyed.iter().copied())
            .filter(|&index| fits(&self.routes[index], path))
            .min()
            .map(|index| self.routes[index].as_slice())
    }

    /// [`canonicalize_path`] over this table.
    fn canonicalize(&self, path: &str) -> Option<String> {
        let route = self.find(path)?;
        if spells(path, canonical_segments(route, path)) {
            return None;
        }
        let mut rebuilt = String::with_capacity(path.len());
        for segment in canonical_segments(route, path) {
            rebuilt.push('/');
            rebuilt.push_str(segment);
        }
        Some(rebuilt)
    }
}

/// The non-empty segments of `path`.
fn segments(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|s| !s.is_empty())
}

/// Orders `a` and `b` by their ASCII-lowercased bytes: `Equal` exactly when
/// [`str::eq_ignore_ascii_case`] holds.
fn case_insensitive_order(a: &str, b: &str) -> Ordering {
    a.bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(b.bytes().map(|byte| byte.to_ascii_lowercase()))
}

/// Whether `path` fits `route`: every literal in any case, one segment per
/// parameter, and any remainder for a catch-all.
fn fits(route: &[Seg], path: &str) -> bool {
    let mut incoming = segments(path);
    for seg in route {
        match (seg, incoming.next()) {
            (Seg::Wildcard, _) => return true,
            (Seg::Param, Some(_)) => {}
            (Seg::Literal(literal), Some(segment)) if segment.eq_ignore_ascii_case(literal) => {}
            _ => return false,
        }
    }
    incoming.next().is_none()
}

/// The segments of `path`, which fits `route`, with the route's literals in
/// their registered case and everything else as sent.
fn canonical_segments<'a>(route: &'a [Seg], path: &'a str) -> impl Iterator<Item = &'a str> {
    let mut incoming = segments(path);
    let mut route = route.iter();
    let mut in_catch_all = false;
    std::iter::from_fn(move || {
        if in_catch_all {
            return incoming.next();
        }
        match route.next()? {
            Seg::Literal(literal) => {
                incoming.next();
                Some(literal.as_str())
            }
            Seg::Param => incoming.next(),
            Seg::Wildcard => {
                in_catch_all = true;
                incoming.next()
            }
        }
    })
}

/// Whether `path` is exactly `/`, then `spelled` joined by `/`.
fn spells<'a>(path: &str, spelled: impl Iterator<Item = &'a str>) -> bool {
    let mut rest = path;
    for segment in spelled {
        let Some(after) = rest
            .strip_prefix('/')
            .and_then(|tail| tail.strip_prefix(segment))
        else {
            return false;
        };
        rest = after;
    }
    rest.is_empty()
}

/// Rewrites a request path so its route **literals** match the registered routes
/// case-insensitively — Jellyfin/ASP.NET routing is case-insensitive, axum is not,
/// and clients (including jellyfin-web) call some paths in non-canonical case.
///
/// Only literal segments are re-cased; parameter values (item ids, names, …) are
/// preserved verbatim. Returns `Some(canonical)` when a differently-cased match is
/// found, or `None` when the path is already canonical or matches no API route
/// (so `/web/*` assets, health, and unknown paths pass through untouched — their
/// case is significant).
///
/// This runs on every request, so an answer of `None` allocates nothing.
#[must_use]
pub fn canonicalize_path(path: &str) -> Option<String> {
    static INDEX: OnceLock<RouteIndex> = OnceLock::new();
    INDEX
        .get_or_init(|| RouteIndex::new(axum_routes().into_iter().map(|(_, path)| path)))
        .canonicalize(path)
}

#[cfg(test)]
mod tests {
    use super::{RouteIndex, VENDORED_ROUTES, axum_routes, canonicalize_path, to_axum_path};
    use std::collections::BTreeMap;

    fn one(path: &str) -> String {
        to_axum_path(path, &mut BTreeMap::new())
    }

    #[test]
    fn canonicalize_recases_route_literals() {
        assert_eq!(
            canonicalize_path("/Localization/cultures").as_deref(),
            Some("/Localization/Cultures")
        );
        assert_eq!(
            canonicalize_path("/localization/options").as_deref(),
            Some("/Localization/Options")
        );
        assert_eq!(
            canonicalize_path("/SYSTEM/INFO/PUBLIC").as_deref(),
            Some("/System/Info/Public")
        );
    }

    #[test]
    fn canonicalize_preserves_parameter_values() {
        // The `{itemId}` value keeps its exact case; only the `Images` literal is recased.
        assert_eq!(
            canonicalize_path("/items/AbC-123/images").as_deref(),
            Some("/Items/AbC-123/Images")
        );
    }

    #[test]
    fn canonicalize_returns_none_for_canonical_or_non_api() {
        assert_eq!(canonicalize_path("/Localization/Cultures"), None); // already canonical
        assert_eq!(canonicalize_path("/web/index.html"), None); // static asset, not an API route
        assert_eq!(canonicalize_path("/definitely/not/a/route"), None);
    }

    /// What [`canonicalize_path`] answered when it walked the whole table, the
    /// first matching route winning: the oracle for the indexed lookup.
    fn table_walk(routes: &[Vec<String>], path: &str) -> Option<String> {
        let incoming: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        for pattern in routes {
            if incoming.len() != pattern.len() {
                continue;
            }
            let literals_match = pattern
                .iter()
                .enumerate()
                .all(|(i, seg)| seg.starts_with('{') || incoming[i].eq_ignore_ascii_case(seg));
            if !literals_match {
                continue;
            }
            let mut rebuilt = String::with_capacity(path.len());
            for (i, seg) in pattern.iter().enumerate() {
                rebuilt.push('/');
                rebuilt.push_str(if seg.starts_with('{') {
                    incoming[i]
                } else {
                    seg
                });
            }
            return (rebuilt != path).then_some(rebuilt);
        }
        None
    }

    /// The router's patterns in registration order, split into segments.
    fn route_table() -> Vec<Vec<String>> {
        let mut seen = std::collections::BTreeSet::new();
        axum_routes()
            .into_iter()
            .filter(|(_, path)| seen.insert(path.clone()))
            .map(|(_, path)| {
                path.split('/')
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .collect()
    }

    /// `path` with every `{placeholder}` replaced by a value.
    fn filled(path: &str) -> String {
        let mut out = String::with_capacity(path.len());
        let mut rest = path;
        while let Some((head, tail)) = rest.split_once('{') {
            out.push_str(head);
            out.push_str("AbC-123");
            rest = tail.split_once('}').map_or("", |(_, after)| after);
        }
        out.push_str(rest);
        out
    }

    /// Every spelling a client could send for `path`, from the canonical one to
    /// the near misses that must find no route.
    fn spellings(path: &str) -> Vec<String> {
        let parent = path.rsplit_once('/').map_or("", |(head, _)| head);
        vec![
            path.to_owned(),
            path.to_ascii_lowercase(),
            path.to_ascii_uppercase(),
            format!("{path}/"),
            format!("{path}//"),
            format!("/{path}"),
            path.replacen('/', "//", 2),
            format!("{path}/extra"),
            parent.to_owned(),
            format!("{parent}/"),
        ]
    }

    /// Every contract path, in every spelling, canonicalizes exactly as the
    /// table walk did.
    #[test]
    fn every_contract_path_canonicalizes_as_the_table_walk_did() {
        let routes = route_table();
        for (_, path) in VENDORED_ROUTES {
            for spelling in spellings(&filled(path)) {
                assert_eq!(
                    canonicalize_path(&spelling),
                    table_walk(&routes, &spelling),
                    "{spelling}"
                );
            }
        }
    }

    /// Paths outside the contract: assets, whose case is significant, and what
    /// the table has no shape for.
    #[test]
    fn paths_outside_the_contract_canonicalize_as_the_table_walk_did() {
        let routes = route_table();
        for path in [
            "",
            "/",
            "//",
            "/web",
            "/web/",
            "/WEB/",
            "/web/index.html",
            "/WEB/index.html",
            "/web/Index.HTML",
            "/web/assets/img/banner-light.png",
            "/web/configurationpage",
            "/WEB/CONFIGURATIONPAGES",
            "/web/ConfigurationPage/extra",
            "/health",
            "/Health/",
            "/health/live",
            "/metrics",
            "/socket",
            "/Socket",
            "/api-docs/openapi.json",
            "/definitely/not/a/route",
            "/Itéms",
            "/ITÉMS/AbC-123",
            "/items/é/images",
            "/Items/%2F/Images",
            "no-leading-slash",
        ] {
            assert_eq!(canonicalize_path(path), table_walk(&routes, path), "{path}");
        }
    }

    /// Where a literal route and a parameter route both fit, the literal wins,
    /// whatever the case: the table lists it first.
    #[test]
    fn a_literal_route_outranks_a_parameter_at_the_same_position() {
        for (path, expected) in [
            ("/items/counts", "/Items/Counts"),
            ("/ITEMS/FILTERS2", "/Items/Filters2"),
            ("/users/me", "/Users/Me"),
            ("/users/public", "/Users/Public"),
        ] {
            assert_eq!(canonicalize_path(path).as_deref(), Some(expected), "{path}");
        }
    }

    /// A contract path spelled canonically is left alone, and the trailing
    /// slash a client adds to it is dropped.
    #[test]
    fn a_canonical_path_is_a_fixed_point_and_its_trailing_slash_is_dropped() {
        for (_, path) in VENDORED_ROUTES {
            let path = filled(path);
            assert_eq!(canonicalize_path(&path), None, "{path}");
            assert_eq!(
                canonicalize_path(&format!("{path}/")).as_deref(),
                Some(path.as_str()),
            );
        }
    }

    /// A route the key cannot describe is still found, and table order still
    /// decides between it and a keyed one.
    #[test]
    fn routes_outside_the_key_are_found_in_table_order() {
        let index = RouteIndex::new(
            ["/Alpha/Beta", "/Alpha/{*rest}", "/{id}/Gamma", "/Zeta"].map(str::to_owned),
        );
        for (path, expected) in [
            ("/alpha/beta", Some("/Alpha/Beta")),
            ("/alpha/other/Tail", Some("/Alpha/other/Tail")),
            ("/alpha", Some("/Alpha")),
            ("/Value/gamma", Some("/Value/Gamma")),
            ("/ZETA/", Some("/Zeta")),
            ("/Alpha/Tail", None),
            ("/Value/Gamma", None),
            ("/nothing/here/at-all", None),
        ] {
            assert_eq!(index.canonicalize(path).as_deref(), expected, "{path}");
        }
        // Both the catch-all and `/{id}/Gamma` fit this one; the catch-all is
        // earlier in the table, so it wins.
        assert_eq!(
            index.canonicalize("/alpha/gamma").as_deref(),
            Some("/Alpha/gamma")
        );
    }

    #[test]
    fn plain_path_is_unchanged() {
        assert_eq!(one("/System/Info"), "/System/Info");
        assert_eq!(one("/Users/{userId}/Items"), "/Users/{userId}/Items");
    }

    #[test]
    fn multi_param_segment_collapses_to_first_param() {
        assert_eq!(
            one("/Audio/{itemId}/hls1/{playlistId}/{segmentId}.{container}"),
            "/Audio/{itemId}/hls1/{playlistId}/{segmentId}"
        );
        assert_eq!(
            one("/Videos/{itemId}/Trickplay/{width}/{index}.jpg"),
            "/Videos/{itemId}/Trickplay/{width}/{index}"
        );
    }

    #[test]
    fn positional_param_names_are_unified() {
        let mut canon = BTreeMap::new();
        let a = to_axum_path("/MediaSegmentsApi/{itemId}", &mut canon);
        let b = to_axum_path("/MediaSegmentsApi/{segmentId}", &mut canon);
        assert_eq!(a, "/MediaSegmentsApi/{itemId}");
        assert_eq!(b, "/MediaSegmentsApi/{itemId}");
    }

    #[test]
    fn axum_routes_are_deduped_and_nonempty() {
        let routes = axum_routes();
        assert!(!routes.is_empty(), "{routes:?}");
        let mut sorted = routes.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), routes.len(), "route table has duplicates");
    }
}
