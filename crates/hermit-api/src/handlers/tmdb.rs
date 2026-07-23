//! `TmdbController` — the TMDb plugin's client-configuration probe.
//!
//! Ports `GET /Tmdb/ClientConfiguration`, which surfaces the image portion of
//! TMDb's `/configuration` response so a client can construct TMDb image URLs.
//!
//! ## Port scope — static configuration
//!
//! In Jellyfin this endpoint proxies a *live* call to TMDb
//! (`TmdbClientManager.GetClientConfiguration` → `TMDbClient.GetConfigAsync`),
//! which needs a configured TMDb API key. The remote TMDb provider is **deferred**
//! in this wave (feature-gated, needs API keys — see the provider-manager port),
//! so no live fetch is made. This handler instead returns TMDb's well-known,
//! publicly documented image configuration — the same static bucket set the
//! client would receive — so the endpoint's shape and payload are faithful. These
//! are canonical configuration values, not fabricated remote *results*; when the
//! TMDb provider lands with a key, this handler swaps the constants for the live
//! fetch and the request/response contract is unchanged.

use axum::routing::get;
use axum::{Json, Router};
use hermit_model::dto::ConfigImageTypes;

use crate::auth::RequireAuth;
use crate::state::AppState;

/// Builds a [`Vec<String>`] of image-size labels from string literals.
fn sizes(labels: &[&str]) -> Vec<String> {
    labels.iter().map(|s| (*s).to_owned()).collect()
}

/// Returns TMDb's canonical image configuration.
///
/// The base URLs and per-kind size buckets TMDb publishes at
/// `/3/configuration`; stable, public values baked in while the live TMDb
/// provider is deferred (see the module docs).
fn tmdb_image_config() -> ConfigImageTypes {
    ConfigImageTypes {
        base_url: Some("http://image.tmdb.org/t/p/".to_owned()),
        secure_base_url: Some("https://image.tmdb.org/t/p/".to_owned()),
        backdrop_sizes: Some(sizes(&["w300", "w780", "w1280", "original"])),
        logo_sizes: Some(sizes(&[
            "w45", "w92", "w154", "w185", "w300", "w500", "original",
        ])),
        poster_sizes: Some(sizes(&[
            "w92", "w154", "w185", "w342", "w500", "w780", "original",
        ])),
        profile_sizes: Some(sizes(&["w45", "w185", "h632", "original"])),
        still_sizes: Some(sizes(&["w92", "w185", "w300", "original"])),
    }
}

/// `GET /Tmdb/ClientConfiguration` — the TMDb image configuration options.
///
/// Port of `TmdbController.TmdbClientConfiguration`, returning the `Images`
/// portion of TMDb's client configuration. Serves the canonical static bucket
/// set while the live TMDb provider is deferred (see the module docs).
#[utoipa::path(
    get,
    path = "/Tmdb/ClientConfiguration",
    responses((status = 200, description = "TMDb image configuration returned", body = ConfigImageTypes)),
    tag = "hermit"
)]
async fn tmdb_client_configuration(RequireAuth(_auth): RequireAuth) -> Json<ConfigImageTypes> {
    Json(tmdb_image_config())
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/Tmdb/ClientConfiguration", get(tmdb_client_configuration))
}

#[cfg(test)]
mod tests {
    use super::tmdb_image_config;

    #[test]
    fn config_carries_tmdb_base_urls_and_sizes() {
        let config = tmdb_image_config();
        assert_eq!(
            config.base_url.as_deref(),
            Some("http://image.tmdb.org/t/p/")
        );
        assert_eq!(
            config.secure_base_url.as_deref(),
            Some("https://image.tmdb.org/t/p/")
        );
        // Every size bucket is populated and includes the "original" size.
        for sizes in [
            config.backdrop_sizes.as_deref(),
            config.logo_sizes.as_deref(),
            config.poster_sizes.as_deref(),
            config.profile_sizes.as_deref(),
            config.still_sizes.as_deref(),
        ] {
            let sizes = sizes.expect("sizes present");
            assert!(sizes.contains(&"original".to_owned()));
        }
    }
}
