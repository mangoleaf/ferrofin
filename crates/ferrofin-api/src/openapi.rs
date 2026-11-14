//! The utoipa OpenAPI document for `ferrofin-api`'s **real** handlers.
//!
//! This spec describes the handlers listed in [`ApiDoc`]'s `paths(...)` (plus
//! the shared health endpoints, merged in at the router), a subset of the
//! routes: a handler's `#[utoipa::path]` annotation shows here only once it is
//! added to that list. It is *not* the client contract — that is the vendored
//! Jellyfin spec, of which the registered route table is a superset (enforced
//! by `tests/contract_superset.rs`).

use utoipa::openapi::extensions::ExtensionsBuilder;
use utoipa::openapi::{self, InfoBuilder};
use utoipa::{Modify, OpenApi};

/// The **Jellyfin server version Ferrofin reports** in `SystemInfo`/`PublicSystemInfo`
/// (`Version`) and as this document's `info.version`: the newest Jellyfin release
/// Ferrofin adopts (12.4.0, whose database carries 12.2's migration-id set and adopts
/// as generation `12.2.0`; handlers are ported from `UPSTREAM_TAG`) — NOT Ferrofin's own
/// release version (`ferrofin_health::RELEASE_VERSION`, sent in User-Agents and backups).
/// Clients gate on it: jellyfin-web's SDK refuses any server below
/// `MINIMUM_VERSION = 10.10.0` with an "Update Required" screen, and jellyfin-sdk-kotlin
/// (Android TV and friends) flags anything below `Jellyfin.minimumVersion = 12.0.0`
/// as unsupported. The route gate stays pinned to the 10.11.8 contract, which covers
/// every 12.x operation except `GET /Items/{itemId}/Collections` (added in 12.1).
pub const JELLYFIN_API_VERSION: &str = "12.4.0";

/// OpenAPI document for `ferrofin-api`'s ported handlers.
///
/// It documents the handlers listed in `paths(...)` below. Merge
/// [`ferrofin_health::HealthApi`] into this document to include the probe
/// endpoints in the published spec.
#[derive(OpenApi)]
#[openapi(
    modifiers(&JellyfinInfo),
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
        crate::handlers::media_info::open_live_stream,
        crate::handlers::media_info::close_live_stream,
        crate::handlers::media_info::get_bitrate_test,
        crate::handlers::videos::get_additional_parts,
        crate::handlers::videos::merge_versions,
        crate::handlers::videos::delete_alternate_sources,
        crate::handlers::videos::get_download,
        crate::handlers::merge_versions::merge_movies,
        crate::handlers::merge_versions::split_movies,
        crate::handlers::merge_versions::merge_episodes,
        crate::handlers::merge_versions::split_episodes,
        crate::handlers::ferrofin_settings::get_settings,
        crate::handlers::ferrofin_settings::update_settings,
        crate::handlers::ldap::test_server_bind,
        crate::handlers::ldap::test_ldap_filters,
        crate::handlers::ldap::ldap_user_search,
        crate::handlers::plugins::get_plugins,
        crate::handlers::plugins::get_plugin_configuration,
        crate::handlers::plugins::update_plugin_configuration,
        crate::handlers::plugins::enable_plugin,
        crate::handlers::plugins::disable_plugin,
        crate::handlers::plugins::uninstall_plugin,
        crate::handlers::plugins::uninstall_plugin_by_version,
        crate::handlers::plugins::get_plugin_image,
        crate::handlers::plugins::get_plugin_manifest,
        crate::handlers::plugins::get_repositories,
        crate::handlers::plugins::set_repositories,
        crate::handlers::plugins::get_packages,
        crate::handlers::plugins::get_package_info,
        crate::handlers::plugins::install_package,
        crate::handlers::plugins::cancel_package_installation,
        crate::handlers::genres::get_genres,
        crate::handlers::genres::get_genre,
        crate::handlers::music_genres::get_music_genres,
        crate::handlers::music_genres::get_music_genre,
        crate::handlers::studios::get_studios,
        crate::handlers::studios::get_studio,
        crate::handlers::persons::get_persons,
        crate::handlers::persons::get_person,
        crate::handlers::artists::get_artists,
        crate::handlers::artists::get_album_artists,
        crate::handlers::artists::get_artist_by_name,
        crate::handlers::years::get_years,
        crate::handlers::years::get_year,
        crate::handlers::media_segments::get_item_segments,
        crate::handlers::trickplay::get_trickplay_hls_playlist,
        crate::handlers::trickplay::get_trickplay_tile_image,
        crate::handlers::lyrics::get_lyrics,
        crate::handlers::lyrics::upload_lyrics,
        crate::handlers::lyrics::delete_lyrics,
        crate::handlers::lyrics::search_remote_lyrics,
        crate::handlers::lyrics::download_remote_lyrics,
        crate::handlers::lyrics::get_remote_lyrics,
        crate::handlers::subtitles::delete_subtitle,
        crate::handlers::subtitles::upload_subtitle,
        crate::handlers::subtitles::search_remote_subtitles,
        crate::handlers::subtitles::download_remote_subtitles,
        crate::handlers::subtitles::get_remote_subtitles,
        crate::handlers::library::get_physical_paths,
        crate::handlers::library::get_available_options,
        crate::handlers::library_structure::get_virtual_folders,
        crate::handlers::library_structure::add_virtual_folder,
        crate::handlers::library_structure::remove_virtual_folder,
        crate::handlers::library_structure::rename_virtual_folder,
        crate::handlers::library_structure::update_library_options,
        crate::handlers::library_structure::add_media_path,
        crate::handlers::library_structure::update_media_path,
        crate::handlers::library_structure::remove_media_path,
    ),
    tags((name = "ferrofin", description = "Ported Jellyfin controller endpoints"))
)]
pub struct ApiDoc;

/// Upstream's document identity (`AddJellyfinApiSwagger`): title `Jellyfin API`, and
/// `version` and `x-jellyfin-version` set to the server version, with no other `info`
/// field. It replaces the `CARGO_PKG_*` values utoipa fills in by default.
struct JellyfinInfo;

impl Modify for JellyfinInfo {
    fn modify(&self, doc: &mut openapi::OpenApi) {
        doc.info = InfoBuilder::new()
            .title("Jellyfin API")
            .version(JELLYFIN_API_VERSION)
            .extensions(Some(
                ExtensionsBuilder::new()
                    .add("x-jellyfin-version", JELLYFIN_API_VERSION)
                    .build(),
            ))
            .build();
    }
}

#[cfg(test)]
mod tests {
    use super::ApiDoc;
    use utoipa::OpenApi;

    #[test]
    fn api_doc_renders() {
        let json = ApiDoc::openapi().to_pretty_json().unwrap();
        assert!(json.contains("\"Jellyfin API\""));
    }
}
