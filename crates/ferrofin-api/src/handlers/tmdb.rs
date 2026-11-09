//! `TmdbController` — the TMDb plugin's client-configuration probe.
//!
//! Ports `GET /Tmdb/ClientConfiguration`, which surfaces the image portion of
//! TMDb's `/configuration` response so a client can construct TMDb image URLs.
//!
//! ## Fixed values, not the live fetch
//!
//! In Jellyfin this endpoint proxies a *live* call to TMDb
//! (`TmdbClientManager.GetClientConfiguration` → `TMDbClient.GetConfigAsync`).
//! This handler makes no call: it returns a fixed copy of TMDb's published
//! image configuration (the base URLs and size buckets below), so the shape is
//! upstream's but the values do not follow later changes TMDb makes.
//! TODO(parity, gl-1508): open work item, not an accepted divergence: port the
//! live `GetConfigAsync` fetch over the TMDb client in `ferrofin-providers`.

use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::dto::ConfigImageTypes;

use crate::auth::RequireAuth;
use crate::state::AppState;

/// Builds a [`Vec<String>`] of image-size labels from string literals.
fn sizes(labels: &[&str]) -> Vec<String> {
    labels.iter().map(|s| (*s).to_owned()).collect()
}

/// Returns TMDb's canonical image configuration.
///
/// The base URLs and per-kind size buckets TMDb publishes at
/// `/3/configuration`, as fixed values (see the module docs).
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
/// portion of TMDb's client configuration, from fixed values rather than
/// upstream's live fetch (see the module docs).
#[utoipa::path(
    get,
    path = "/Tmdb/ClientConfiguration",
    responses((status = 200, description = "TMDb image configuration returned", body = ConfigImageTypes)),
    tag = "ferrofin"
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
