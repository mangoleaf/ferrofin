//! [`FerrofinServerConfigurationManager`] — the concrete
//! [`ServerConfigurationManager`] over a JSON-persisted [`ServerConfiguration`].
//!
//! Port of `Emby.Server.Implementations.Configuration.ServerConfigurationManager`
//! and the slice of its `BaseConfigurationManager` base that the server actually
//! exercises.
//!
//! Departures from the C#:
//! - The C# base persists the strongly-typed configuration as `system.xml` via
//!   `IXmlSerializer`. Ferrofin uses `serde_json` everywhere (the port rules drop
//!   the XML serializer), so the configuration is stored as `system.json` under
//!   the configuration directory. The wire shape is unchanged — it is the same
//!   [`ServerConfiguration`] the API returns.
//! - `ReplaceConfiguration` fires a `ConfigurationUpdating` event and validates
//!   the metadata path against the filesystem. The event bus is out of scope for
//!   this unit; the metadata-path validation is preserved
//!   ([`validate_metadata_path`](Self::validate_metadata_path)): a new, non-empty
//!   metadata path that differs from the current one must already exist on disk.
//! - `OnConfigurationUpdated` recomputes the internal metadata path; that is
//!   reproduced by pushing the new `MetadataPath` into the shared
//!   [`FerrofinServerApplicationPaths`] on every load and save
//!   (`UpdateMetadataPath`).
//!
//! The in-memory configuration is held as an `RwLock<Arc<ServerConfiguration>>`
//! so the object-safe, `&self` trait methods can read and replace it. On
//! construction the manager loads `system.json` if present, otherwise falls
//! back to [`default_server_configuration`] and writes it out.
//!
//! ## What the `Arc` indirection buys today, and what it does not
//!
//! `ServerConfiguration` is a large struct with dozens of heap-owned fields
//! (`String`s, `Vec`s, the per-type `MetadataOptions` table), so handing out a
//! *clone* of it is dozens of allocations. Storing it behind an `Arc` delivers
//! two things right now:
//!
//! - Writers build a fresh `ServerConfiguration`, wrap it in a new `Arc`, and
//!   swap the pointer, so the write section is a single pointer store and an
//!   in-flight reader keeps a consistent (never torn) view of the document it
//!   started with while the next read observes the new one.
//! - The deep clone that [`snapshot`](FerrofinServerConfigurationManager::snapshot)
//!   performs now happens *after* the read lock is released (it clones the
//!   handle under the lock and the document outside it), so a slow reader no
//!   longer widens the window in which a writer is blocked.
//!
//! It does **not** yet make per-request configuration reads cheap, and nothing
//! in this module should be read as claiming otherwise. The readers that would
//! benefit most — the API auth extractor (`ferrofin-api`'s `auth.rs`) and
//! `AuthorizationContext` — hold the manager as
//! `Arc<dyn ServerConfigurationManager>` and can therefore only call
//! [`ServerConfigurationManager::configuration`], which is declared to return an
//! *owned* `ServerConfiguration`. Every one of those reads still deep-clones the
//! whole document, exactly as it did before the `Arc` landed, and
//! [`snapshot_shared`](FerrofinServerConfigurationManager::snapshot_shared)
//! currently has no caller outside this module.
//!
//! Unlocking the hot path is a **pending, one-line change to a file this module
//! does not own** — `crates/ferrofin-traits/src/configuration.rs`: declare
//!
//! ```text
//! async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError>;
//! ```
//!
//! (still object-safe), return `self.snapshot_shared()` from the impl below,
//! and adjust the handful of call sites that mutate the returned value to clone
//! it explicitly. Until that lands, treat the `Arc` storage as groundwork, not
//! as a delivered per-request win.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use ferrofin_model::branding::BrandingOptions;
use ferrofin_model::configuration::{EncodingOptions, ServerConfiguration};
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::system::ServerApplicationPaths;

use crate::app_paths::FerrofinServerApplicationPaths;

/// The per-item-type [`MetadataOptions`] `ServerConfiguration` ships with — the
/// set the library metadata-options editor lists. Verbatim port of the C#
/// `ServerConfiguration.MetadataOptions` default array (an empty array here left
/// the editor with no per-type rows).
fn default_metadata_options() -> Vec<ferrofin_model::configuration::MetadataOptions> {
    use ferrofin_model::configuration::MetadataOptions;
    let plain = |item_type: &str| MetadataOptions {
        item_type: Some(item_type.to_owned()),
        ..MetadataOptions::default()
    };
    let with_disabled = |item_type: &str, fetchers: &[&str], images: &[&str]| MetadataOptions {
        item_type: Some(item_type.to_owned()),
        disabled_metadata_fetchers: fetchers.iter().map(|s| (*s).to_owned()).collect(),
        disabled_image_fetchers: images.iter().map(|s| (*s).to_owned()).collect(),
        ..MetadataOptions::default()
    };
    vec![
        plain("Book"),
        plain("Movie"),
        with_disabled(
            "MusicVideo",
            &["The Open Movie Database"],
            &["The Open Movie Database"],
        ),
        plain("Series"),
        with_disabled("MusicAlbum", &["TheAudioDB"], &[]),
        with_disabled("MusicArtist", &["TheAudioDB"], &[]),
        plain("BoxSet"),
        plain("Season"),
        plain("Episode"),
    ]
}

/// Builds a fresh [`ServerConfiguration`] with Jellyfin's factory defaults.
///
/// The `ferrofin-model` [`ServerConfiguration`] intentionally derives no
/// `Default` (it is a pure wire DTO), so this reproduces the C#
/// `ServerConfiguration()` constructor defaults (and its `BaseApplicationConfiguration`
/// base) here — the one place a "blank" configuration is minted before the first
/// `system.json` is written, including the per-item-type `MetadataOptions` table
/// the C# constructor seeds (see [`default_metadata_options`]).
#[must_use]
pub fn default_server_configuration() -> ServerConfiguration {
    ServerConfiguration {
        // BaseApplicationConfiguration
        log_file_retention_days: 3,
        is_startup_wizard_completed: false,
        cache_path: None,
        previous_version: None,
        previous_version_str: None,
        // ServerConfiguration
        enable_metrics: false,
        enable_normalized_item_by_name_ids: true,
        is_port_authorized: false,
        quick_connect_available: true,
        enable_case_sensitive_item_ids: true,
        disable_live_tv_channel_user_data_name: true,
        metadata_path: String::new(),
        preferred_metadata_language: "en".to_owned(),
        metadata_country_code: "US".to_owned(),
        sort_replace_characters: vec![".".to_owned(), "+".to_owned(), "%".to_owned()],
        sort_remove_characters: vec![
            ",".to_owned(),
            "&".to_owned(),
            "-".to_owned(),
            "{".to_owned(),
            "}".to_owned(),
            "'".to_owned(),
        ],
        sort_remove_words: vec!["the".to_owned(), "a".to_owned(), "an".to_owned()],
        min_resume_pct: 5,
        max_resume_pct: 90,
        min_resume_duration_seconds: 300,
        min_audiobook_resume: 5,
        max_audiobook_resume: 5,
        inactive_session_threshold: 0,
        library_monitor_delay: 60,
        library_update_duration: 30,
        // C# leaves CacheSize null on a fresh instance (computed lazily), which
        // serializes to 0 on the wire; match that fresh-instance value.
        cache_size: 0,
        image_saving_convention: ferrofin_model::configuration::ImageSavingConvention::default(),
        metadata_options: default_metadata_options(),
        skip_deserialization_for_basic_types: true,
        server_name: String::new(),
        ui_culture: "en-US".to_owned(),
        save_metadata_hidden: false,
        content_types: Vec::new(),
        remote_client_bitrate_limit: 0,
        enable_folder_view: false,
        enable_grouping_movies_into_collections: false,
        enable_grouping_shows_into_collections: false,
        display_specials_within_seasons: true,
        codecs_used: Vec::new(),
        // Jellyfin ships one default plugin repository (the Jellyfin Stable feed).
        plugin_repositories: vec![ferrofin_model::updates::RepositoryInfo {
            name: Some("Jellyfin Stable".to_owned()),
            url: Some("https://repo.jellyfin.org/files/plugin/manifest.json".to_owned()),
            enabled: true,
        }],
        enable_external_content_in_suggestions: true,
        image_extraction_timeout_ms: 0,
        path_substitutions: Vec::new(),
        enable_slow_response_warning: true,
        slow_response_threshold_ms: 500,
        cors_hosts: vec!["*".to_owned()],
        activity_log_retention_days: Some(30),
        library_scan_fanout_concurrency: 0,
        library_metadata_refresh_concurrency: 0,
        allow_client_log_upload: true,
        dummy_chapter_duration: 0,
        chapter_image_resolution: ferrofin_model::drawing::ImageResolution::MatchSource,
        parallel_image_encoding_limit: 0,
        // Jellyfin's ServerConfiguration ships two built-in cast receivers.
        cast_receiver_applications: vec![
            ferrofin_model::system::CastReceiverApplication {
                id: "F007D354".to_owned(),
                name: "Stable".to_owned(),
            },
            ferrofin_model::system::CastReceiverApplication {
                id: "6F511C87".to_owned(),
                name: "Unstable".to_owned(),
            },
        ],
        trickplay_options: ferrofin_model::configuration::TrickplayOptions::default(),
        // Jellyfin's ServerConfiguration seeds EnableLegacyAuthorization = true.
        enable_legacy_authorization: true,
    }
}

/// The concrete server configuration manager.
///
/// Owns the shared application paths and the live configuration. Reads take a
/// refcounted handle to the current document (`Arc` clone, no deep copy);
/// writes replace the whole document and persist it to `system.json`.
pub struct FerrofinServerConfigurationManager {
    paths: Arc<FerrofinServerApplicationPaths>,
    config_file: PathBuf,
    branding_file: PathBuf,
    encoding_file: PathBuf,
    configuration: RwLock<Arc<ServerConfiguration>>,
}

impl std::fmt::Debug for FerrofinServerConfigurationManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinServerConfigurationManager")
            .field("config_file", &self.config_file)
            .finish_non_exhaustive()
    }
}

impl FerrofinServerConfigurationManager {
    /// The on-disk configuration file name (JSON counterpart of C# `system.xml`).
    const CONFIG_FILE_NAME: &'static str = "system.json";

    /// The subdirectory of the user config dir holding named configs
    /// (`branding`/`encoding`), matching the `/System/Configuration/{key}` API.
    const NAMED_SUBDIR: &'static str = "named";

    /// The on-disk branding configuration file name (Jellyfin's named
    /// `branding` configuration, stored as a sibling JSON document).
    const BRANDING_FILE_NAME: &'static str = "branding.json";

    /// The on-disk encoding configuration file name (Jellyfin's named
    /// `encoding` configuration, stored as a sibling JSON document).
    const ENCODING_FILE_NAME: &'static str = "encoding.json";

    /// Loads (or initializes) the configuration for the given paths.
    ///
    /// If `{configuration-directory}/system.json` exists it is parsed; otherwise
    /// a default configuration is written out. Either way the shared paths'
    /// internal metadata path is synced to the loaded `MetadataPath`, matching
    /// the C# constructor's `UpdateMetadataPath` call.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] if the configuration file cannot be read, parsed,
    /// or (when absent) written.
    pub async fn load(paths: Arc<FerrofinServerApplicationPaths>) -> Result<Self, ServiceError> {
        let user_config_dir = PathBuf::from(paths.user_configuration_directory_path());
        let config_dir = user_config_dir.parent().map_or_else(
            || PathBuf::from(paths.program_data_path()),
            std::path::Path::to_path_buf,
        );
        let config_file = config_dir.join(Self::CONFIG_FILE_NAME);
        // Named configs (`branding`/`encoding`) are persisted by the
        // `/System/Configuration/{key}` API under `{user-config-dir}/named/`, so
        // the typed readers must look there too — reading `{config-dir}/*.json`
        // silently missed a client-saved config (e.g. NVENC never took effect).
        let named_dir = user_config_dir.join(Self::NAMED_SUBDIR);
        let branding_file = named_dir.join(Self::BRANDING_FILE_NAME);
        let encoding_file = named_dir.join(Self::ENCODING_FILE_NAME);

        let configuration = if config_file.exists() {
            let bytes = tokio::fs::read(&config_file)
                .await
                .map_err(|e| io_err("read configuration", &config_file, &e))?;
            let mut config = serde_json::from_slice::<ServerConfiguration>(&bytes)
                .map_err(|e| ServiceError::Backend(format!("invalid configuration JSON: {e}")))?;
            // Backfill the per-type MetadataOptions on an older config that
            // predates them (was written empty), so the library editor lists them.
            if config.metadata_options.is_empty() {
                config.metadata_options = default_metadata_options();
            }
            config
        } else {
            let default = default_server_configuration();
            write_config(&config_file, &default).await?;
            default
        };

        paths.set_internal_metadata_path(Some(configuration.metadata_path.as_str()));

        Ok(Self {
            paths,
            config_file,
            branding_file,
            encoding_file,
            configuration: RwLock::new(Arc::new(configuration)),
        })
    }

    /// A shared handle to the current configuration.
    ///
    /// Clones the `Arc`, not the document, so a reader that only needs to
    /// *observe* the configuration pays one refcount bump instead of a deep copy
    /// of every `String`/`Vec` in [`ServerConfiguration`]. The returned handle is
    /// a point-in-time snapshot: a concurrent
    /// [`update_configuration`](ServerConfigurationManager::update_configuration)
    /// swaps in a *new* `Arc`, so an existing handle keeps observing the
    /// document it was taken from and the next call observes the new one.
    ///
    /// **Callers today: this module only** —
    /// [`snapshot`](Self::snapshot) and `validate_metadata_path`. The
    /// per-request readers (the API auth extractor, `AuthorizationContext`) see
    /// this manager as `Arc<dyn ServerConfigurationManager>`, so they can only
    /// reach [`ServerConfigurationManager::configuration`], which returns an
    /// owned document and therefore still deep-clones on every request. This
    /// method is groundwork for the pending trait-signature change described in
    /// the module docs; it is not, on its own, a per-request saving.
    ///
    /// # Panics
    ///
    /// Panics only if the configuration lock is poisoned — i.e. a thread
    /// panicked while holding it, which this crate never does.
    #[must_use]
    pub fn snapshot_shared(&self) -> Arc<ServerConfiguration> {
        Arc::clone(
            &self
                .configuration
                .read()
                .expect("configuration lock poisoned"),
        )
    }

    /// A snapshot clone of the current configuration.
    ///
    /// This deep-copies the whole document. It is nonetheless the path *every*
    /// trait-seam read takes — including every per-request one — because
    /// [`ServerConfigurationManager::configuration`] is declared to hand out an
    /// owned value; see the module docs. The one thing the `Arc` changes here is
    /// that the copy is made after the read lock has been dropped rather than
    /// while holding it. In-crate callers that only need to read a field should
    /// prefer [`snapshot_shared`](Self::snapshot_shared).
    ///
    /// # Panics
    ///
    /// Panics only if the configuration lock is poisoned — i.e. a thread
    /// panicked while holding it, which this crate never does.
    #[must_use]
    pub fn snapshot(&self) -> ServerConfiguration {
        (*self.snapshot_shared()).clone()
    }

    /// The shared application paths as the concrete type (used by sibling
    /// managers in this crate that need the extra accessors — trickplay/plugins
    /// paths — beyond the trait surface).
    #[must_use]
    pub fn concrete_paths(&self) -> Arc<FerrofinServerApplicationPaths> {
        Arc::clone(&self.paths)
    }

    /// Validates a proposed metadata path, mirroring C# `ValidateMetadataPath`.
    ///
    /// A new, non-empty path that differs from the current one must already
    /// exist as a directory; otherwise the update is rejected. An unchanged or
    /// blank path is always accepted.
    fn validate_metadata_path(&self, new_path: &str) -> Result<(), ServiceError> {
        if new_path.trim().is_empty() {
            return Ok(());
        }
        // Read through the shared handle: this only needs one field, so a deep
        // clone of the whole document would be pure waste.
        let current = self.snapshot_shared();
        if new_path == current.metadata_path {
            return Ok(());
        }
        if !PathBuf::from(new_path).is_dir() {
            return Err(ServiceError::InvalidInput(format!(
                "metadata path does not exist: {new_path}"
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl ServerConfigurationManager for FerrofinServerConfigurationManager {
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        Arc::clone(&self.paths) as Arc<dyn ServerApplicationPaths>
    }

    async fn configuration(&self) -> Result<ServerConfiguration, ServiceError> {
        Ok(self.snapshot())
    }

    async fn update_configuration(
        &self,
        configuration: &ServerConfiguration,
    ) -> Result<(), ServiceError> {
        // ReplaceConfiguration: validate, persist, then apply side effects.
        self.validate_metadata_path(&configuration.metadata_path)?;
        write_config(&self.config_file, configuration).await?;

        // Build the replacement outside the lock, then swap the pointer: the
        // write section is a single pointer store, and readers holding an older
        // handle keep a consistent view of the document they took.
        let replacement = Arc::new(configuration.clone());
        {
            let mut guard = self
                .configuration
                .write()
                .expect("configuration lock poisoned");
            *guard = replacement;
        }

        // OnConfigurationUpdated → UpdateMetadataPath.
        self.paths
            .set_internal_metadata_path(Some(configuration.metadata_path.as_str()));
        Ok(())
    }

    async fn get_branding(&self) -> Result<BrandingOptions, ServiceError> {
        if !self.branding_file.exists() {
            return Ok(BrandingOptions::default());
        }
        let bytes = tokio::fs::read(&self.branding_file)
            .await
            .map_err(|e| io_err("read branding", &self.branding_file, &e))?;
        serde_json::from_slice::<BrandingOptions>(&bytes)
            .map_err(|e| ServiceError::Backend(format!("invalid branding JSON: {e}")))
    }

    async fn update_branding(&self, branding: &BrandingOptions) -> Result<(), ServiceError> {
        if let Some(parent) = self.branding_file.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| io_err("create configuration directory", parent, &e))?;
        }
        let json = serde_json::to_vec_pretty(branding)
            .map_err(|e| ServiceError::Backend(format!("serialize branding: {e}")))?;
        tokio::fs::write(&self.branding_file, json)
            .await
            .map_err(|e| io_err("write branding", &self.branding_file, &e))
    }

    async fn get_encoding_options(&self) -> Result<EncodingOptions, ServiceError> {
        if !self.encoding_file.exists() {
            return Ok(EncodingOptions::default());
        }
        let bytes = tokio::fs::read(&self.encoding_file)
            .await
            .map_err(|e| io_err("read encoding options", &self.encoding_file, &e))?;
        serde_json::from_slice::<EncodingOptions>(&bytes)
            .map_err(|e| ServiceError::Backend(format!("invalid encoding options JSON: {e}")))
    }
}

/// Serializes and writes a configuration to `path` (creating the parent dir).
async fn write_config(
    path: &std::path::Path,
    configuration: &ServerConfiguration,
) -> Result<(), ServiceError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| io_err("create configuration directory", parent, &e))?;
    }
    let json = serde_json::to_vec_pretty(configuration)
        .map_err(|e| ServiceError::Backend(format!("serialize configuration: {e}")))?;
    tokio::fs::write(path, json)
        .await
        .map_err(|e| io_err("write configuration", path, &e))
}

/// Wraps a filesystem error touching `path` as a [`ServiceError`].
fn io_err(action: &str, path: &std::path::Path, err: &std::io::Error) -> ServiceError {
    ServiceError::Backend(format!("{action} ({}): {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_paths::test_paths;

    #[test]
    fn default_configuration_matches_jellyfin_fresh_instance() {
        let cfg = default_server_configuration();

        // A single default plugin repository: the Jellyfin Stable feed.
        assert_eq!(cfg.plugin_repositories.len(), 1);
        let repo = &cfg.plugin_repositories[0];
        assert_eq!(repo.name.as_deref(), Some("Jellyfin Stable"));
        assert_eq!(
            repo.url.as_deref(),
            Some("https://repo.jellyfin.org/files/plugin/manifest.json")
        );
        assert!(repo.enabled);

        // Legacy authorization is enabled by default.
        assert!(cfg.enable_legacy_authorization);
        // Cache size is the fresh-instance wire value (0).
        assert_eq!(cfg.cache_size, 0);
        // Fresh-instance runtime state is left untouched.
        assert!(!cfg.is_startup_wizard_completed);
        assert!(!cfg.is_port_authorized);
    }

    #[tokio::test]
    async fn load_writes_default_then_reloads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());

        let mgr = FerrofinServerConfigurationManager::load(Arc::clone(&paths))
            .await
            .expect("load");
        // Default written to disk.
        assert!(mgr.config_file.exists());

        // A second load parses the persisted document rather than rewriting it.
        let mgr2 = FerrofinServerConfigurationManager::load(Arc::clone(&paths))
            .await
            .expect("reload");
        assert_eq!(
            mgr.snapshot().server_name,
            mgr2.snapshot().server_name,
            "reload should observe the same configuration"
        );
    }

    #[tokio::test]
    async fn update_persists_and_syncs_metadata_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());
        let mgr = FerrofinServerConfigurationManager::load(Arc::clone(&paths))
            .await
            .expect("load");

        // Point metadata at a real, existing directory.
        let meta = tmp.path().join("custom-meta");
        std::fs::create_dir_all(&meta).expect("mkdir meta");
        let mut cfg = mgr.snapshot();
        cfg.metadata_path = meta.to_string_lossy().into_owned();
        cfg.server_name = "Ferrofin Test".to_owned();

        mgr.update_configuration(&cfg).await.expect("update");

        assert_eq!(mgr.snapshot().server_name, "Ferrofin Test");
        assert_eq!(paths.internal_metadata_path(), meta.to_string_lossy());

        // A fresh manager over the same paths reloads the persisted change.
        let reloaded = FerrofinServerConfigurationManager::load(paths)
            .await
            .expect("reload");
        assert_eq!(reloaded.snapshot().server_name, "Ferrofin Test");
    }

    /// The read path must hand out the *same* allocation on repeat reads (no
    /// deep clone), and a write must be visible to every subsequent read —
    /// through the shared handle, the owned snapshot, and the trait seam alike.
    #[tokio::test]
    async fn write_swaps_the_shared_document_and_is_observed_by_later_reads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());
        let mgr = FerrofinServerConfigurationManager::load(Arc::clone(&paths))
            .await
            .expect("load");

        // Two reads with no intervening write observe one shared document.
        let before = mgr.snapshot_shared();
        assert!(
            Arc::ptr_eq(&before, &mgr.snapshot_shared()),
            "repeat reads must share one allocation, not deep-clone the document"
        );
        // The owned snapshot is value-identical to the shared one (parity: the
        // trait seam must keep emitting exactly what it emitted before).
        assert_eq!(mgr.snapshot(), *before);
        assert_eq!(
            mgr.configuration().await.expect("configuration"),
            *before,
            "the trait accessor must observe the same document"
        );

        // Mutate a spread of heap-owned and scalar fields.
        let mut cfg = mgr.snapshot();
        cfg.server_name = "Swapped".to_owned();
        cfg.sort_remove_words = vec!["das".to_owned(), "der".to_owned()];
        cfg.cors_hosts = vec!["https://example.invalid".to_owned()];
        cfg.min_resume_pct = 11;
        cfg.enable_metrics = true;
        mgr.update_configuration(&cfg).await.expect("update");

        // The write installed a NEW document; the handle taken before the write
        // still observes the pre-write values (no torn/in-place mutation).
        let after = mgr.snapshot_shared();
        assert!(
            !Arc::ptr_eq(&before, &after),
            "a write must swap in a new document, not mutate the shared one"
        );
        assert_ne!(before.server_name, "Swapped");
        assert_eq!(before.min_resume_pct, 5);

        // Every subsequent read observes the write, on all three read paths.
        for observed in [
            (*after).clone(),
            mgr.snapshot(),
            mgr.configuration().await.expect("configuration"),
        ] {
            assert_eq!(observed.server_name, "Swapped");
            assert_eq!(observed.sort_remove_words, vec!["das", "der"]);
            assert_eq!(observed.cors_hosts, vec!["https://example.invalid"]);
            assert_eq!(observed.min_resume_pct, 11);
            assert!(observed.enable_metrics);
            assert_eq!(observed, cfg, "the read must equal the written document");
        }

        // And the swapped-in document is itself shared, not re-cloned per read.
        assert!(Arc::ptr_eq(&after, &mgr.snapshot_shared()));
    }

    /// `validate_metadata_path` reads the live document, so an unchanged path is
    /// accepted even after the configuration has been replaced.
    #[tokio::test]
    async fn metadata_path_validation_reads_the_current_document() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());
        let mgr = FerrofinServerConfigurationManager::load(Arc::clone(&paths))
            .await
            .expect("load");

        let meta = tmp.path().join("meta");
        std::fs::create_dir_all(&meta).expect("mkdir meta");
        let meta_str = meta.to_string_lossy().into_owned();

        let mut cfg = mgr.snapshot();
        cfg.metadata_path.clone_from(&meta_str);
        mgr.update_configuration(&cfg).await.expect("first update");
        assert_eq!(mgr.snapshot_shared().metadata_path, meta_str);

        // The directory goes away; the same (unchanged) path must still validate
        // because it matches the *current* configuration.
        std::fs::remove_dir_all(&meta).expect("rmdir meta");
        cfg.server_name = "Renamed".to_owned();
        mgr.update_configuration(&cfg)
            .await
            .expect("unchanged metadata path stays valid");
        assert_eq!(mgr.snapshot_shared().server_name, "Renamed");
    }

    #[tokio::test]
    async fn branding_defaults_then_persists_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());
        let mgr = FerrofinServerConfigurationManager::load(Arc::clone(&paths))
            .await
            .expect("load");

        // No branding file yet → defaults.
        let initial = mgr.get_branding().await.expect("get branding");
        assert_eq!(initial, BrandingOptions::default());

        // Persist a customized branding and observe it reload.
        let branding = BrandingOptions {
            login_disclaimer: Some("Be excellent.".to_owned()),
            custom_css: Some("body{}".to_owned()),
            splashscreen_enabled: true,
            splashscreen_location: Some("/tmp/splash.png".to_owned()),
        };
        mgr.update_branding(&branding)
            .await
            .expect("update branding");
        assert_eq!(mgr.get_branding().await.expect("reget"), branding);

        // A fresh manager over the same paths reads the persisted branding.
        let reloaded = FerrofinServerConfigurationManager::load(paths)
            .await
            .expect("reload");
        assert_eq!(
            reloaded.get_branding().await.expect("reload branding"),
            branding
        );
    }

    #[tokio::test]
    async fn encoding_options_default_then_reads_persisted_document() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());
        let mgr = FerrofinServerConfigurationManager::load(Arc::clone(&paths))
            .await
            .expect("load");

        // No encoding file yet → defaults (and no fallback-font path configured).
        let initial = mgr.get_encoding_options().await.expect("get encoding");
        assert_eq!(initial, EncodingOptions::default());
        assert!(initial.fallback_font_path.is_none());

        // Persist a customized encoding document and observe it reload.
        let encoding = EncodingOptions {
            fallback_font_path: Some("/srv/fonts".to_owned()),
            enable_fallback_font: true,
            ..EncodingOptions::default()
        };
        let json = serde_json::to_vec_pretty(&encoding).expect("serialize encoding");
        tokio::fs::create_dir_all(mgr.encoding_file.parent().expect("named dir"))
            .await
            .expect("create named dir");
        tokio::fs::write(&mgr.encoding_file, json)
            .await
            .expect("write encoding");
        assert_eq!(
            mgr.get_encoding_options().await.expect("reget encoding"),
            encoding
        );

        // A fresh manager over the same paths reads the persisted document.
        let reloaded = FerrofinServerConfigurationManager::load(paths)
            .await
            .expect("reload");
        assert_eq!(
            reloaded
                .get_encoding_options()
                .await
                .expect("reload encoding"),
            encoding
        );
    }

    #[tokio::test]
    async fn encoding_options_rejects_invalid_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());
        let mgr = FerrofinServerConfigurationManager::load(paths)
            .await
            .expect("load");

        tokio::fs::create_dir_all(mgr.encoding_file.parent().expect("named dir"))
            .await
            .expect("create named dir");
        tokio::fs::write(&mgr.encoding_file, b"not json")
            .await
            .expect("write bad encoding");
        assert!(mgr.get_encoding_options().await.is_err());
    }

    #[tokio::test]
    async fn update_rejects_missing_metadata_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());
        let mgr = FerrofinServerConfigurationManager::load(paths)
            .await
            .expect("load");

        let mut cfg = mgr.snapshot();
        cfg.metadata_path = "/definitely/not/here".to_owned();
        let err = mgr
            .update_configuration(&cfg)
            .await
            .expect_err("should reject");
        assert!(matches!(err, ServiceError::InvalidInput(_)));
    }
}
