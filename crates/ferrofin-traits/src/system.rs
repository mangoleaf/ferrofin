//! System-layer traits — application lifecycle, host info, and path resolution.
//!
//! Port of `MediaBrowser.Controller.ISystemManager`,
//! `IServerApplicationHost`, `IServerApplicationPaths`, and the
//! `MediaBrowser.Controller.IO.IPathManager` / `IExternalDataManager`
//! interfaces.
//!
//! Port rules applied throughout:
//! - `HttpRequest` arguments become the transport-agnostic
//!   [`RequestContext`](crate::net::RequestContext).
//! - `IServerApplicationHost`/`IServerApplicationPaths` extend the generic
//!   `IApplicationHost`/`IApplicationPaths` bases. Those bases carry plugin/DI
//!   machinery that is out of scope here; only the server-relevant members plus
//!   the common filesystem paths are surfaced, and every path accessor is a
//!   plain `fn -> String` (no async, no generics) so the traits stay object-safe.
//! - `IPathManager`/`IExternalDataManager` take the domain `BaseItem`; that
//!   collapses to an `item_id: `[`Uuid`] identity (plus the item's on-disk media
//!   path where the C# reads it from the item). `CancellationToken` is dropped.
//! - `Task`/synchronous void → `async fn -> Result<_, ServiceError>` for the
//!   fallible I/O methods; pure path getters stay synchronous.
//!
//! Every trait is object-safe and carries a `_assert_object_safe_*` assertion.

use async_trait::async_trait;
use ferrofin_model::system::{PublicSystemInfo, SystemInfo, SystemStorageInfo};
use uuid::Uuid;

use crate::error::ServiceError;
use crate::net::RequestContext;

/// Manages the running application instance: info, restart, shutdown, storage.
///
/// Port of `ISystemManager`. The `HttpRequest` arguments become
/// [`RequestContext`]; restart/shutdown/storage become fallible `async fn`.
#[async_trait]
pub trait SystemManager: Send + Sync {
    /// Gets the full system info for an (authenticated) request.
    async fn get_system_info(&self, request: &RequestContext) -> Result<SystemInfo, ServiceError>;

    /// Gets the public (unauthenticated) system info for a request.
    async fn get_public_system_info(
        &self,
        request: &RequestContext,
    ) -> Result<PublicSystemInfo, ServiceError>;

    /// Begins the application restart process.
    async fn restart(&self) -> Result<(), ServiceError>;

    /// Begins the application shutdown process.
    async fn shutdown(&self) -> Result<(), ServiceError>;

    /// Gets the server's storage resource usage.
    async fn get_system_storage_info(&self) -> Result<SystemStorageInfo, ServiceError>;

    /// Writes a consistent snapshot of the database to `dest` for a backup (the
    /// step Jellyfin's backup takes through its database provider). `dest` must
    /// not exist beforehand.
    ///
    /// The default writes nothing — implementations without a database (test
    /// fakes); the server's implementation snapshots the live SQLite file.
    async fn snapshot_database(&self, dest: &std::path::Path) -> Result<(), ServiceError> {
        let _ = dest;
        Ok(())
    }
}

fn _assert_object_safe_system_manager(_: &dyn SystemManager) {}

/// Exposes host-level networking facts and URL construction.
///
/// Port of `IServerApplicationHost` (the server-relevant subset of its
/// `IApplicationHost` base). URL builders are fallible `async fn` (they consult
/// live network state); the simple facts are synchronous getters.
#[async_trait]
pub trait ServerApplicationHost: Send + Sync {
    /// Whether core startup has finished.
    fn core_startup_has_completed(&self) -> bool;

    /// The HTTP listen port.
    fn http_port(&self) -> u16;

    /// The HTTPS listen port.
    fn https_port(&self) -> u16;

    /// Whether the server listens over HTTPS.
    fn listen_with_https(&self) -> bool;

    /// The server's friendly (display) name.
    fn friendly_name(&self) -> String;

    /// Builds the best externally reachable API URL for a request.
    async fn get_smart_api_url(&self, request: &RequestContext) -> Result<String, ServiceError>;

    /// Builds a LAN-reachable API URL for the given host/scheme/port.
    async fn get_local_api_url(
        &self,
        hostname: &str,
        scheme: Option<&str>,
        port: Option<u16>,
    ) -> Result<String, ServiceError>;

    /// Expands a stored virtual path to an absolute filesystem path.
    fn expand_virtual_path(&self, path: &str) -> String;

    /// Collapses an absolute filesystem path back to its virtual form.
    fn reverse_virtual_path(&self, path: &str) -> String;
}

fn _assert_object_safe_server_application_host(_: &dyn ServerApplicationHost) {}

/// Resolves the server's well-known on-disk directories.
///
/// Port of `IServerApplicationPaths` plus the common members of its
/// `IApplicationPaths` base. Every accessor is a synchronous `fn -> String`;
/// paths are process-static, so there is nothing to fail.
pub trait ServerApplicationPaths: Send + Sync {
    /// The base root media directory.
    fn root_folder_path(&self) -> String;

    /// The default user-views directory.
    fn default_user_views_path(&self) -> String;

    /// The People metadata directory.
    fn people_path(&self) -> String;

    /// The Genre metadata directory.
    fn genre_path(&self) -> String;

    /// The music-genre metadata directory.
    fn music_genre_path(&self) -> String;

    /// The Studio metadata directory.
    fn studio_path(&self) -> String;

    /// The Year metadata directory.
    fn year_path(&self) -> String;

    /// The artists metadata directory.
    fn artists_path(&self) -> String;

    /// The user-configuration directory.
    fn user_configuration_directory_path(&self) -> String;

    /// The application configuration root directory (`system.json` and the
    /// per-area config files live here). Defaults to `{program-data}/config`,
    /// the server layout; the real paths override it with the configured dir.
    fn configuration_directory_path(&self) -> String {
        std::path::Path::new(&self.program_data_path())
            .join("config")
            .to_string_lossy()
            .into_owned()
    }

    /// The SQLite database file. Defaults to `{program-data}/ferrofin.db`; the
    /// server overrides it with the file it actually opened (a drop-in-adopted
    /// `jellyfin.db` lives elsewhere).
    fn database_path(&self) -> String {
        std::path::Path::new(&self.program_data_path())
            .join("ferrofin.db")
            .to_string_lossy()
            .into_owned()
    }

    /// The internal metadata directory (custom or default).
    fn internal_metadata_path(&self) -> String;

    /// The program data directory.
    fn program_data_path(&self) -> String;

    /// The web UI resources directory.
    fn web_path(&self) -> String;

    /// The general data directory.
    fn data_path(&self) -> String;

    /// The image cache directory.
    fn image_cache_path(&self) -> String;

    /// The cache directory.
    fn cache_path(&self) -> String;

    /// The log directory.
    fn log_directory_path(&self) -> String;

    /// The transcoding cache directory (`GetTranscodePath()`), where the HLS
    /// segment/playlist files a live transcode produces are written and served
    /// from. Default is a `transcodes` subdirectory of [`cache_path`](Self::cache_path).
    fn transcode_path(&self) -> String {
        std::path::Path::new(&self.cache_path())
            .join("transcodes")
            .to_string_lossy()
            .into_owned()
    }

    /// The scratch directory single-frame extractions write to before the
    /// result is moved into place (the C# `TempDirectory`). A `temp`
    /// subdirectory of [`cache_path`](Self::cache_path).
    ///
    /// One owner for the layout: the media encoder writes here and the
    /// chapter-image task pre-flights it, and the two must agree or the
    /// pre-flight checks a directory nothing uses.
    fn temp_path(&self) -> String {
        std::path::Path::new(&self.cache_path())
            .join("temp")
            .to_string_lossy()
            .into_owned()
    }
}

fn _assert_object_safe_server_application_paths(_: &dyn ServerApplicationPaths) {}

/// Computes on-disk paths for an item's derived/extracted data.
///
/// Port of `IPathManager`. The C# `BaseItem` arguments collapse to an
/// `item_id: `[`Uuid`] plus the item's media path (`media_path`), which the C#
/// implementation reads off the item. Methods return `Option<String>` where the
/// C# returns `null` for an invalid media-source id.
pub trait PathManager: Send + Sync {
    /// The base folder for an item's trickplay tiles.
    fn trickplay_directory(&self, item_id: Uuid, media_path: &str, save_with_media: bool)
    -> String;

    /// The path to a subtitle file for a media source + stream index.
    fn subtitle_path(
        &self,
        media_source_id: &str,
        stream_index: i32,
        extension: &str,
    ) -> Option<String>;

    /// The folder holding a media source's subtitle files.
    fn subtitle_folder_path(&self, media_source_id: &str) -> Option<String>;

    /// The path to a named attachment for a media source.
    fn attachment_path(&self, media_source_id: &str, file_name: &str) -> Option<String>;

    /// The folder holding a media source's attachments.
    fn attachment_folder_path(&self, media_source_id: &str) -> Option<String>;

    /// The folder holding an item's chapter images.
    fn chapter_image_folder_path(&self, item_id: Uuid, media_path: &str) -> String;

    /// The path to a chapter image at a given position (in ticks).
    fn chapter_image_path(
        &self,
        item_id: Uuid,
        media_path: &str,
        chapter_position_ticks: i64,
    ) -> String;

    /// All folders holding an item's extracted data.
    fn extracted_data_paths(&self, item_id: Uuid, media_path: &str) -> Vec<String>;
}

fn _assert_object_safe_path_manager(_: &dyn PathManager) {}

/// Deletes an item's external (filesystem-side) data.
///
/// Port of `IExternalDataManager`. The `BaseItem` argument collapses to an
/// `item_id: `[`Uuid`] plus its media path; `CancellationToken` is dropped.
#[async_trait]
pub trait ExternalDataManager: Send + Sync {
    /// Deletes all external data for an item (DB- and filesystem-side).
    async fn delete_external_item_data(
        &self,
        item_id: Uuid,
        media_path: &str,
    ) -> Result<(), ServiceError>;

    /// Deletes only the filesystem-side external data (attachments, subtitles,
    /// trickplay, chapter images), leaving DB cleanup to the caller.
    async fn delete_external_item_files(
        &self,
        item_id: Uuid,
        media_path: &str,
    ) -> Result<(), ServiceError>;
}

fn _assert_object_safe_external_data_manager(_: &dyn ExternalDataManager) {}
