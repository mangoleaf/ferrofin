//! Route-table plumbing: the shared `not_implemented` stub and the axum-path
//! normalization that lets every vendored Jellyfin path register on axum 0.8.
//!
//! Two of Jellyfin's path conventions don't fit axum's `matchit` router, so the
//! vendored path is normalized to an equivalent axum pattern before it is
//! registered. The transform is captured in [`to_axum_path`] and reused by both
//! the router builder and the contract-superset test, so they can never drift.

use axum::response::IntoResponse;
use std::collections::{BTreeMap, BTreeSet};

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

#[cfg(test)]
mod tests {
    use super::{axum_routes, to_axum_path};
    use std::collections::BTreeMap;

    fn one(path: &str) -> String {
        to_axum_path(path, &mut BTreeMap::new())
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
        assert!(!routes.is_empty());
        let mut sorted = routes.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), routes.len(), "route table has duplicates");
    }
}
