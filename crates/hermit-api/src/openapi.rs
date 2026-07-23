//! The utoipa OpenAPI document for `hermit-api`'s **real** handlers.
//!
//! This spec describes only the routes with ported handlers (plus the shared
//! health endpoints, merged in at the router). It is *not* the client contract —
//! that is the vendored Jellyfin spec, of which the registered route table is a
//! superset (enforced by `tests/contract_superset.rs`). As waves port handlers,
//! their `#[utoipa::path]` annotations are added to [`ApiDoc`]'s `paths(...)`.

use utoipa::OpenApi;

/// OpenAPI document for `hermit-api`'s ported handlers.
///
/// Unit 1 (INFRA) registers every contract route to the shared
/// `not_implemented` stub, so no handler paths are documented yet. Merge
/// [`hermit_health::HealthApi`] into this document to include the probe
/// endpoints in the published spec.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "hermit-api",
        description = "Hermit media server HTTP API — a Rust port of Jellyfin.Api."
    ),
    paths(
        crate::handlers::system::get_system_info,
        crate::handlers::system::get_public_system_info,
        crate::handlers::users::authenticate_by_name,
        crate::handlers::users::get_current_user,
        crate::handlers::user_views::get_user_views,
        crate::handlers::items::get_items,
        crate::handlers::items::get_item,
        crate::handlers::media_info::get_playback_info,
        crate::handlers::media_info::post_playback_info,
    ),
    tags((name = "hermit", description = "Ported Jellyfin controller endpoints"))
)]
pub struct ApiDoc;

#[cfg(test)]
mod tests {
    use super::ApiDoc;
    use utoipa::OpenApi;

    #[test]
    fn api_doc_renders() {
        let json = ApiDoc::openapi().to_pretty_json().unwrap();
        assert!(json.contains("hermit-api"));
    }
}
