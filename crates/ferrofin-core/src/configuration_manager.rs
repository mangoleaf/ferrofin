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
//!   the metadata path against the filesystem. Successful saves notify registered
//!   observers; the metadata-path validation is preserved
//!   ([`prepare_metadata_path`]): a new, non-empty
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
//! ## What the `Arc` indirection buys
//!
//! `ServerConfiguration` is a large struct with dozens of heap-owned fields
//! (`String`s, `Vec`s, the per-type `MetadataOptions` table), so handing out a
//! *clone* of it is dozens of allocations. Storing it behind an `Arc` — and
//! handing that `Arc` out through the trait seam — delivers three things:
//!
//! - Writers build a fresh `ServerConfiguration`, wrap it in a new `Arc`, and
//!   swap the pointer, so the write section is a single pointer store and an
//!   in-flight reader keeps a consistent (never torn) view of the document it
//!   started with while the next read observes the new one.
//! - [`ServerConfigurationManager::configuration`] — the seam every
//!   per-request reader goes through, including the API auth extractor
//!   (`ferrofin-api`'s `auth.rs`) and `AuthorizationContext`, both of which hold
//!   this manager only as `Arc<dyn ServerConfigurationManager>` — returns
//!   [`snapshot_shared`](FerrofinServerConfigurationManager::snapshot_shared),
//!   so an authenticated request now pays a refcount bump instead of a deep
//!   copy of the whole document.
//! - The callers that genuinely *edit* the configuration (the startup wizard,
//!   `/System/Configuration`, the metadata-options editor) clone the document
//!   explicitly — the cost stays, but only on the write paths that need it.
//!
//! [`snapshot`](FerrofinServerConfigurationManager::snapshot) still exists for
//! in-crate callers that want an owned, mutable copy; in-crate readers that only
//! need a field should prefer `snapshot_shared`.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use ferrofin_model::branding::BrandingOptions;
use ferrofin_model::configuration::{EncodingOptions, MetadataConfiguration, ServerConfiguration};
use ferrofin_traits::configuration::{
    ConfigurationListener, ConfigurationUpdate, ServerConfigurationManager,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::system::ServerApplicationPaths;

use crate::app_paths::FerrofinServerApplicationPaths;
use crate::config_import;

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
        sort_replace_characters: ferrofin_model::configuration::default_sort_replace_characters(),
        sort_remove_characters: ferrofin_model::configuration::default_sort_remove_characters(),
        sort_remove_words: ferrofin_model::configuration::default_sort_remove_words(),
        min_resume_pct: 5,
        max_resume_pct: 90,
        min_resume_duration_seconds: 300,
        min_audiobook_resume: 5,
        max_audiobook_resume: 5,
        inactive_session_threshold: 0,
        library_monitor_delay: 60,
        library_update_duration: 30,
        // `ServerConfiguration.CacheSize` (ServerConfiguration.cs:183) is an
        // eager, non-nullable `Environment.ProcessorCount * 100` — 400 in a
        // four-core container, never 0. `usable_cores` is the cgroup-aware
        // ProcessorCount (a bare `available_parallelism` would report the HOST's
        // cores inside a cpu-limited container).
        cache_size: i32::try_from(ferrofin_db::database::usable_cores().saturating_mul(100))
            .unwrap_or(i32::MAX),
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
        // Jellyfin 12.0: `EnableLegacyAuthorization` defaults to false (its
        // `DisableLegacyAuthorization` routine also turns it off on upgrade).
        // 10.11.8 seeded true; an adopted `system.xml` value is imported as-is.
        enable_legacy_authorization: false,
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
    metadata_file: PathBuf,
    network_file: PathBuf,
    xbmc_metadata_file: PathBuf,
    live_tv_file: PathBuf,
    /// The `metadata.json` the last "unusable" warning was logged for, so a
    /// broken document warns once per change, not on every read.
    metadata_warned: std::sync::Mutex<Option<WarnedFile>>,
    /// The same for `branding.json`.
    branding_warned: std::sync::Mutex<Option<WarnedFile>>,
    /// The same for `encoding.json`.
    encoding_warned: std::sync::Mutex<Option<WarnedFile>>,
    /// The same for `network.json`.
    network_warned: std::sync::Mutex<Option<WarnedFile>>,
    /// The same for `xbmcmetadata.json`.
    xbmc_metadata_warned: std::sync::Mutex<Option<WarnedFile>>,
    /// The same for `livetv.json`.
    live_tv_warned: std::sync::Mutex<Option<WarnedFile>>,
    configuration: Arc<RwLock<Arc<ServerConfiguration>>>,
    write_lock: Arc<tokio::sync::Mutex<()>>,
    listeners: Arc<RwLock<Vec<Arc<ConfigurationListener>>>>,
}

/// A file a warning was logged for, by its modification time (`None` when
/// it has none to read).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WarnedFile(Option<std::time::SystemTime>);

/// Whether the unusable `path` as it stands now has not been warned about
/// yet, by `warned`; records that it now has.
fn first_warning(warned: &std::sync::Mutex<Option<WarnedFile>>, path: &std::path::Path) -> bool {
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let mut warned = warned
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    warned.replace(WarnedFile(mtime)) != Some(WarnedFile(mtime))
}

/// Forgets the last warning in `warned`, so a file that breaks again warns anew.
fn forget_warning(warned: &std::sync::Mutex<Option<WarnedFile>>) {
    warned
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
}

/// Hands the settings that shape derived keys to their derivations: the two
/// that shape item ids, and the one that shapes a Live TV channel's user-data
/// keys.
fn publish_key_policies(configuration: &ServerConfiguration) {
    crate::item_type_lookup::set_item_id_policy(
        configuration.enable_case_sensitive_item_ids,
        configuration.enable_normalized_item_by_name_ids,
    );
    crate::user_data_keys::set_live_tv_channel_user_data_name_disabled(
        configuration.disable_live_tv_channel_user_data_name,
    );
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

    /// The on-disk network configuration file name (Jellyfin's named
    /// `network` configuration). This one is not merely served back: the
    /// request path enforces it (`ferrofin_api::ip_access`).
    const NETWORK_FILE_NAME: &'static str = "network.json";

    /// The on-disk metadata configuration file name (Jellyfin's named
    /// `metadata` configuration, `MetadataConfigurationStore`): the "Date
    /// added behavior for new content" the library scan dates new items by.
    const METADATA_FILE_NAME: &'static str = "metadata.json";

    /// The on-disk NFO configuration file name (Jellyfin's named
    /// `xbmcmetadata` configuration, `XbmcMetadataOptions`).
    const XBMC_METADATA_FILE_NAME: &'static str = "xbmcmetadata.json";

    /// The on-disk Live TV configuration file name (Jellyfin's named `livetv`
    /// configuration): its scalar options, the tuners and listing providers
    /// being kept in the database.
    const LIVE_TV_FILE_NAME: &'static str = "livetv.json";

    /// Loads (or initializes) the configuration for the given paths.
    ///
    /// If `{configuration-directory}/system.json` exists it is parsed. If it
    /// does not, this is either a fresh install or the adoption of a Jellyfin
    /// data directory, so Jellyfin's `config/*.xml` is imported over the
    /// defaults first (see [`crate::config_import`]) and the result written
    /// out. Either way the shared paths' internal metadata path is synced to
    /// the loaded `MetadataPath`, matching the C# constructor's
    /// `UpdateMetadataPath` call.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] if the configuration file cannot be read,
    /// parsed, or (when absent) written, or if a Jellyfin `config/*.xml` is
    /// present but cannot be imported — booting on defaults would silently
    /// discard the operator's settings, `IsStartupWizardCompleted` among them.
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
        let metadata_file = named_dir.join(Self::METADATA_FILE_NAME);

        // Before `system.json` is written below: see `seed_named_configurations`.
        seed_named_configurations(&config_dir, &named_dir).await;

        // No `system.json` means this data directory has never been served by
        // Ferrofin, so it may be a Jellyfin one we are adopting. Jellyfin kept
        // the same settings as XML next to where the JSON goes; import them
        // once, here, and never look at the XML again.
        let adopting = !config_file.exists();
        let mut configuration = if adopting {
            let default = adopt_xml(
                &config_dir.join("system.xml"),
                default_server_configuration(),
                "ServerConfiguration",
                config_import::SYSTEM_XML_DENY,
            )
            .await?;
            write_config(&config_file, &default).await?;
            default
        } else {
            let bytes = tokio::fs::read(&config_file)
                .await
                .map_err(|e| io_err("read configuration", &config_file, &e))?;
            let mut config = serde_json::from_slice::<ServerConfiguration>(&bytes)
                .map_err(|e| json_err("invalid configuration JSON", &config_file, &e))?;
            // Backfill the per-type MetadataOptions on an older config that
            // predates them (was written empty), so the library editor lists them.
            if config.metadata_options.is_empty() {
                config.metadata_options = default_metadata_options();
            }
            // An install adopted before the XML import existed kept Ferrofin's
            // defaults, and a lost `IsStartupWizardCompleted` leaves the
            // `FirstTimeSetupOrAuth` endpoints open to anonymous callers. That
            // combination — Jellyfin's XML saying the wizard was completed, our
            // JSON saying it was not — cannot be a deliberate choice, because
            // nothing in Ferrofin ever sets the flag back to false.
            repair_lost_startup_flag(&config_dir, &config_file, &mut config).await;
            config
        };

        // `ApplicationHost.FindParts` (ApplicationHost.cs:720-726) sets
        // `IsPortAuthorized = true` and saves it on EVERY startup; only a change
        // of the HTTP/HTTPS port clears it again (`OnConfigurationUpdated`,
        // ApplicationHost.cs:810-818). It is startup state, not first-run setup
        // state, so a fresh install reports it true from its first request.
        if !configuration.is_port_authorized {
            configuration.is_port_authorized = true;
            // Non-fatal, like the startup-flag repair above: a server that boots
            // fine must not be refused a start over a config write, and the next
            // boot simply writes it again.
            if let Err(e) = write_config(&config_file, &configuration).await {
                tracing::warn!(error = %e, "could not persist the port-authorized flag");
            }
        }

        initialize_cache_path(&paths, configuration.cache_path.as_deref())?;
        initialize_transcode_path(&paths, &encoding_file).await?;

        prepare_metadata_path(&paths, &configuration.metadata_path, None)?;
        paths.set_internal_metadata_path(Some(configuration.metadata_path.as_str()));
        publish_key_policies(&configuration);

        Ok(Self {
            paths,
            config_file,
            branding_file,
            encoding_file,
            metadata_file,
            network_file: named_dir.join(Self::NETWORK_FILE_NAME),
            xbmc_metadata_file: named_dir.join(Self::XBMC_METADATA_FILE_NAME),
            live_tv_file: named_dir.join(Self::LIVE_TV_FILE_NAME),
            metadata_warned: std::sync::Mutex::new(None),
            branding_warned: std::sync::Mutex::new(None),
            encoding_warned: std::sync::Mutex::new(None),
            network_warned: std::sync::Mutex::new(None),
            xbmc_metadata_warned: std::sync::Mutex::new(None),
            live_tv_warned: std::sync::Mutex::new(None),
            configuration: Arc::new(RwLock::new(Arc::new(configuration))),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            listeners: Arc::new(RwLock::new(Vec::new())),
        })
    }

    /// Persists the validated startup executable for the dashboard's read-only display.
    ///
    /// Executable selection remains a startup operation. Failed ffmpeg validation
    /// must not call this method; a later ffprobe failure still preserves the display.
    /// A failed legacy XML adoption leaves JSON absent for its next-boot retry, so
    /// the display remains unavailable until that configuration can be imported.
    ///
    /// # Errors
    /// Returns an error if the encoding document cannot be serialized or saved.
    pub async fn set_encoder_app_path_display(&self, path: &str) -> Result<(), ServiceError> {
        let _save = self.write_lock.lock().await;
        let legacy_encoding = self.config_file.with_file_name("encoding.xml");
        if !self.encoding_file.exists() && legacy_encoding.exists() {
            tracing::warn!(
                legacy_path = %legacy_encoding.display(),
                "resolved encoder display was not saved while legacy encoding adoption is pending; repair the XML and restart to retry"
            );
            return Ok(());
        }
        let mut options = match self.get_encoding_options().await {
            Ok(options) => options,
            Err(error) => {
                tracing::warn!(%error, "loading encoding configuration failed; using defaults");
                EncodingOptions::default()
            }
        };
        options.encoder_app_path_display = Some(path.to_owned());
        let json = serde_json::to_string_pretty(&options).map_err(|error| {
            ServiceError::backend(format!("serialize encoding configuration: {error}"))
        })?;
        let file = self.encoding_file.clone();
        let committed: Arc<str> = json.clone().into();
        tokio::task::spawn_blocking(move || {
            ferrofin_util::file_helper::atomic_write(&file, json.as_bytes())
                .map_err(|error| io_err("write resolved encoder path", &file, &error))
        })
        .await
        .map_err(|error| ServiceError::backend(format!("encoding writer: {error}")))??;
        self.named_configuration_updated("encoding", committed);
        Ok(())
    }

    /// Subscribes to successful main, branding, and named-store saves.
    ///
    /// The host registers listeners while constructing its services. A
    /// listener receives the exact committed snapshot; failed saves emit none.
    /// Keep captured service references weak if they own this manager.
    ///
    /// # Panics
    ///
    /// Panics if the listener registry's lock is poisoned.
    pub fn add_configuration_listener(&self, listener: Arc<ConfigurationListener>) {
        self.listeners
            .write()
            .expect("configuration listener lock poisoned")
            .push(listener);
    }

    /// The metadata configuration as saved now — C#
    /// `GetMetadataConfiguration()` (`MetadataConfigurationExtensions.cs:12`,
    /// `GetConfiguration<MetadataConfiguration>("metadata")`): the document
    /// `POST /System/Configuration/metadata` writes, read on every call, so
    /// a dashboard change applies to the next reader without a restart.
    ///
    /// A document never saved is the default. One that cannot be read or
    /// parsed is the default too (upstream's `ConfigurationHelper` falls back
    /// the same way), and says so at `warn` once per change of the file (its
    /// modification time): the library scan reads this on every plan pass —
    /// each scan, watcher batch and item refresh — and would otherwise repeat
    /// the same warning for each.
    ///
    /// A synchronous read of one small local file: its readers are the scan's
    /// planning pass, which stats every path it resolves the same way.
    #[must_use]
    pub fn metadata_configuration(&self) -> MetadataConfiguration {
        let bytes = match std::fs::read(&self.metadata_file) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.metadata_usable();
                return MetadataConfiguration::default();
            }
            Err(e) => {
                if self.first_metadata_warning() {
                    tracing::warn!(
                        path = %self.metadata_file.display(),
                        error = %e,
                        "could not read the metadata configuration; using its defaults"
                    );
                }
                return MetadataConfiguration::default();
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(config) => {
                self.metadata_usable();
                config
            }
            Err(e) => {
                if self.first_metadata_warning() {
                    tracing::warn!(
                        path = %self.metadata_file.display(),
                        error = %e,
                        "the saved metadata configuration is not valid; using its defaults"
                    );
                }
                MetadataConfiguration::default()
            }
        }
    }

    /// Whether the unusable `metadata.json` as it stands now has not been
    /// warned about yet; records that it now has.
    fn first_metadata_warning(&self) -> bool {
        first_warning(&self.metadata_warned, &self.metadata_file)
    }

    /// Forgets the last warning once the document reads again, so breaking
    /// it later warns anew whatever its modification time.
    fn metadata_usable(&self) {
        forget_warning(&self.metadata_warned);
    }

    /// Reads a named configuration document (`branding`, `encoding`, and the
    /// others [`ServerConfigurationManager::get_named_configuration`] serves) as C#
    /// `BaseConfigurationManager.LoadConfiguration` does: a document never
    /// saved is the default, and one that does not parse is the default too,
    /// logged with its path. Upstream logs that once, because it caches the
    /// default; Ferrofin rereads the file on every call, so it warns once per
    /// change of the file. A read failure other than not-found is still an
    /// error, as upstream lets an `IOException` through.
    async fn read_named<T: serde::de::DeserializeOwned + Default>(
        what: &'static str,
        path: &std::path::Path,
        warned: &std::sync::Mutex<Option<WarnedFile>>,
    ) -> Result<T, ServiceError> {
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                forget_warning(warned);
                return Ok(T::default());
            }
            Err(e) => return Err(io_err(what, path, &e)),
        };
        match serde_json::from_slice(&bytes) {
            Ok(document) => {
                forget_warning(warned);
                Ok(document)
            }
            Err(e) => {
                if first_warning(warned, path) {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "a saved configuration file is not valid; using its defaults until it is fixed"
                    );
                }
                Ok(T::default())
            }
        }
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
    /// This is what [`ServerConfigurationManager::configuration`] returns, so
    /// it is the path every per-request reader takes — including the ones that
    /// see this manager only as `Arc<dyn ServerConfigurationManager>` (the API
    /// auth extractor and `AuthorizationContext`).
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
    /// This deep-copies the whole document, so it is for callers that need an
    /// owned, *mutable* configuration to edit and hand back to
    /// [`ServerConfigurationManager::update_configuration`]. The copy is made
    /// after the read lock has been dropped rather than while holding it.
    /// Callers that only need to read a field should use
    /// [`snapshot_shared`](Self::snapshot_shared), which is also what the trait
    /// seam hands out.
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
}

#[async_trait]
impl ServerConfigurationManager for FerrofinServerConfigurationManager {
    fn named_configuration_updated(&self, key: &str, json: Arc<str>) {
        if key.eq_ignore_ascii_case("encoding") {
            match serde_json::from_str::<EncodingOptions>(&json) {
                Ok(options) => self
                    .paths
                    .set_transcode_path(options.transcoding_temp_path.as_deref()),
                Err(error) => {
                    tracing::warn!(%error, "committed encoding configuration could not update transcode path");
                }
            }
        }

        notify_listeners(
            &self.listeners,
            &ConfigurationUpdate::Named {
                key: key.to_ascii_lowercase(),
                json,
            },
        );
    }

    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        Arc::clone(&self.paths) as Arc<dyn ServerApplicationPaths>
    }

    async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
        Ok(self.snapshot_shared())
    }

    async fn update_configuration(
        &self,
        configuration: &ServerConfiguration,
    ) -> Result<(), ServiceError> {
        // Hold the writer through validation, persistence, publication and
        // notification. A cancelled caller must not leave disk ahead of memory.
        let save = Arc::clone(&self.write_lock).lock_owned().await;
        let old_metadata = self.snapshot_shared().metadata_path.clone();
        let old_cache = self.snapshot_shared().cache_path.clone();
        let replacement = Arc::new(configuration.clone());
        let json = serde_json::to_vec_pretty(configuration)
            .map_err(|e| ServiceError::backend(format!("serialize configuration: {e}")))?;
        let path = self.config_file.clone();
        let current = Arc::clone(&self.configuration);
        let paths = Arc::clone(&self.paths);
        let listeners = Arc::clone(&self.listeners);
        tokio::task::spawn_blocking(move || {
            let _save = save;
            prepare_metadata_path(&paths, &replacement.metadata_path, Some(&old_metadata))?;
            if let Some(cache) = replacement
                .cache_path
                .as_deref()
                .filter(|p| !p.trim().is_empty())
            {
                let cache_path = PathBuf::from(cache);
                if old_cache.as_deref() != Some(cache) {
                    if !cache_path.is_dir() {
                        return Err(ServiceError::not_found(format!(
                            "cache path does not exist: {cache}"
                        )));
                    }
                    let probe = cache_path.join(uuid::Uuid::new_v4().to_string());
                    std::fs::File::create_new(&probe)
                        .and_then(|_| std::fs::remove_file(&probe))
                        .map_err(|e| io_err("validate cache directory", &cache_path, &e))?;
                }
                FerrofinServerApplicationPaths::prepare_cache_path(&cache_path)
                    .map_err(|e| io_err("prepare cache directory", &cache_path, &e))?;
            }
            ferrofin_util::file_helper::atomic_write(&path, &json)
                .map_err(|e| io_err("write configuration", &path, &e))?;
            *current.write().expect("configuration lock poisoned") = Arc::clone(&replacement);
            publish_key_policies(&replacement);
            paths.set_cache_path(replacement.cache_path.as_deref());
            paths.set_internal_metadata_path(Some(&replacement.metadata_path));
            notify_listeners(&listeners, &ConfigurationUpdate::Server(replacement));
            Ok(())
        })
        .await
        .map_err(|e| ServiceError::backend(format!("configuration writer: {e}")))?
    }

    async fn get_branding(&self) -> Result<BrandingOptions, ServiceError> {
        Self::read_named("read branding", &self.branding_file, &self.branding_warned).await
    }

    async fn update_branding(&self, branding: &BrandingOptions) -> Result<(), ServiceError> {
        let save = Arc::clone(&self.write_lock).lock_owned().await;
        let json: Arc<str> = serde_json::to_string_pretty(branding)
            .map_err(|e| ServiceError::Backend(format!("serialize branding: {e}")))?
            .into();
        let path = self.branding_file.clone();
        let listeners = Arc::clone(&self.listeners);
        tokio::task::spawn_blocking(move || {
            let _save = save;
            ferrofin_util::file_helper::atomic_write(&path, json.as_bytes())
                .map_err(|e| io_err("write branding", &path, &e))?;
            notify_listeners(
                &listeners,
                &ConfigurationUpdate::Named {
                    key: "branding".into(),
                    json,
                },
            );
            Ok(())
        })
        .await
        .map_err(|e| ServiceError::backend(format!("branding writer: {e}")))?
    }

    async fn get_encoding_options(&self) -> Result<EncodingOptions, ServiceError> {
        Self::read_named(
            "read encoding options",
            &self.encoding_file,
            &self.encoding_warned,
        )
        .await
    }

    /// The stores Jellyfin 12.2 registers: `branding`, `encoding`, `network`,
    /// `metadata`, `xbmcmetadata`, `livetv` and `database`. Each document is
    /// bound to its type, so the members it omits read as their defaults and
    /// members the type does not have are dropped. `livetv` is its scalar
    /// options only: the Live TV manager keeps the tuners and listing
    /// providers. `database` is the SQLite document Jellyfin writes on its
    /// first start, the only provider Ferrofin has.
    async fn get_named_configuration(
        &self,
        key: &str,
    ) -> Result<Option<serde_json::Value>, ServiceError> {
        let document = match key.to_ascii_lowercase().as_str() {
            "branding" => serde_json::to_value(self.get_branding().await?),
            "encoding" => serde_json::to_value(self.get_encoding_options().await?),
            "network" => serde_json::to_value(
                Self::read_named::<ferrofin_networking::NetworkConfiguration>(
                    "read network configuration",
                    &self.network_file,
                    &self.network_warned,
                )
                .await?,
            ),
            "metadata" => serde_json::to_value(
                Self::read_named::<MetadataConfiguration>(
                    "read metadata configuration",
                    &self.metadata_file,
                    &self.metadata_warned,
                )
                .await?,
            ),
            "xbmcmetadata" => serde_json::to_value(
                Self::read_named::<ferrofin_model::configuration::XbmcMetadataOptions>(
                    "read NFO configuration",
                    &self.xbmc_metadata_file,
                    &self.xbmc_metadata_warned,
                )
                .await?,
            ),
            "livetv" => serde_json::to_value(
                Self::read_named::<ferrofin_model::live_tv::LiveTvOptions>(
                    "read Live TV configuration",
                    &self.live_tv_file,
                    &self.live_tv_warned,
                )
                .await?,
            ),
            "database" => serde_json::to_value(
                ferrofin_model::configuration::DatabaseConfigurationOptions::default(),
            ),
            _ => return Ok(None),
        };
        document
            .map(Some)
            .map_err(|e| ServiceError::backend(format!("serialize configuration `{key}`: {e}")))
    }
}

/// A changed dashboard directory must already exist and accept a write.
/// Startup and unchanged paths are created if absent, as in UpdateMetadataPath.
fn prepare_metadata_path(
    paths: &FerrofinServerApplicationPaths,
    proposed: &str,
    previous: Option<&str>,
) -> Result<(), ServiceError> {
    let metadata = if proposed.trim().is_empty() {
        PathBuf::from(paths.default_internal_metadata_path())
    } else {
        PathBuf::from(proposed)
    };
    if !proposed.trim().is_empty() && previous.is_some_and(|old| old != proposed) {
        if !metadata.is_dir() {
            return Err(ServiceError::not_found(format!(
                "metadata path does not exist: {proposed}"
            )));
        }
        let probe = metadata.join(uuid::Uuid::new_v4().to_string());
        std::fs::File::create_new(&probe)
            .and_then(|_| std::fs::remove_file(&probe))
            .map_err(|e| io_err("validate metadata directory", &metadata, &e))?;
    }
    std::fs::create_dir_all(&metadata)
        .map_err(|e| io_err("prepare metadata directory", &metadata, &e))
}

fn initialize_cache_path(
    paths: &FerrofinServerApplicationPaths,
    configured: Option<&str>,
) -> Result<(), ServiceError> {
    let cache = configured
        .filter(|p| !p.trim().is_empty())
        .map_or_else(|| PathBuf::from(paths.cache_path()), PathBuf::from);
    FerrofinServerApplicationPaths::prepare_cache_path(&cache)
        .map_err(|e| io_err("prepare cache directory", &cache, &e))?;
    paths.set_cache_path(configured);
    Ok(())
}

/// Loads the named encoding path after cache initialization and prepares it.
/// Read/parse failures retain defaults; filesystem preparation still fails startup.
async fn initialize_transcode_path(
    paths: &FerrofinServerApplicationPaths,
    encoding_file: &std::path::Path,
) -> Result<(), ServiceError> {
    let encoding = match tokio::fs::read(encoding_file).await {
        Ok(bytes) => match serde_json::from_slice::<EncodingOptions>(&bytes) {
            Ok(options) => options,
            Err(error) => {
                tracing::warn!(
                    path = %encoding_file.display(),
                    %error,
                    "loading encoding configuration for transcode path failed; using defaults"
                );
                EncodingOptions::default()
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => EncodingOptions::default(),
        Err(error) => {
            tracing::warn!(
                path = %encoding_file.display(),
                %error,
                "reading encoding configuration for transcode path failed; using defaults"
            );
            EncodingOptions::default()
        }
    };
    paths.set_transcode_path(encoding.transcoding_temp_path.as_deref());
    paths.prepare_transcode_path()?;
    Ok(())
}

fn notify_listeners(
    listeners: &RwLock<Vec<Arc<ConfigurationListener>>>,
    update: &ConfigurationUpdate,
) {
    // Release the registry lock before invoking listeners so a callback can
    // inspect configuration or register another observer without deadlocking.
    let callbacks = listeners
        .read()
        .expect("configuration listener lock poisoned")
        .clone();
    for callback in callbacks {
        callback(update);
    }
}

/// Applies the Jellyfin XML at `xml_path` over `default`, if it is there.
///
/// An absent document is the ordinary fresh-install case and returns `default`.
/// A document that *is* there but cannot be read or imported is an error.
///
/// **This diverges from Jellyfin deliberately.**
/// `Emby.Server.Implementations/AppBase/ConfigurationHelper.cs:33` catches every
/// exception, substitutes `Activator.CreateInstance(type)`, and then — because
/// the re-serialized defaults differ from what it read — *overwrites the file
/// with them*. So an unreadable `system.xml` costs a Jellyfin operator every
/// setting they had, silently, and takes the evidence with it.
///
/// Ferrofin refuses instead, because the caller that matters runs this exactly
/// once (`system.json` does not exist yet) and persists the result immediately:
/// there is no second chance to get it right, and one of the settings at stake
/// is `IsStartupWizardCompleted`, whose loss leaves the `FirstTimeSetupOrAuth`
/// endpoints open to anonymous callers. Where a failure *is* retried on the
/// next boot — seeding a named configuration — [`load`] carries on with a
/// warning instead, which is the same trade made the other way.
///
/// # Errors
///
/// Returns [`ServiceError`] if `xml_path` exists but cannot be read or imported.
async fn adopt_xml<T>(
    xml_path: &std::path::Path,
    default: T,
    root_name: &str,
    deny: &[&str],
) -> Result<T, ServiceError>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    if !xml_path.exists() {
        return Ok(default);
    }
    let xml = tokio::fs::read_to_string(xml_path)
        .await
        .map_err(|e| io_err("read the jellyfin configuration", xml_path, &e))?;
    let imported = config_import::import_over(&default, &xml, root_name, deny).map_err(|e| {
        ServiceError::Backend(format!(
            "could not adopt the jellyfin configuration at {}: {e}. \
             Move or repair the file to start on ferrofin's defaults.",
            xml_path.display()
        ))
    })?;
    tracing::info!(path = %xml_path.display(), "adopted jellyfin configuration");
    Ok(imported)
}

/// Seeds every named configuration (`named/<key>.json`) from Jellyfin's XML.
///
/// The named configurations live in their own files and are read on
/// demand, so adoption has to materialize them or the imported values
/// are never seen. Seeding runs on EVERY boot, not just the adoption
/// one: it is a no-op once the JSON exists, and running it
/// unconditionally is what repairs an install adopted before this import
/// existed — whose `encoding.json` is still missing and whose hardware
/// transcoding is therefore still off. [`FerrofinServerConfigurationManager::load`]
/// runs it BEFORE writing `system.json`, since that file is the sentinel for
/// the adoption branch and crashing between the two would lose these for good.
///
/// Each document is seeded on its own, so one that cannot be imported costs
/// only itself: a corrupt `encoding.xml` must not keep `network.xml`, whose
/// policy the request path enforces, from being imported. A failure is logged
/// with the file it came from, and is never fatal, on any boot. The line
/// between refusing to start and carrying on is not "is this the adoption
/// boot" — it is whether the next boot gets another chance. Seeding is skipped
/// only once its JSON exists, so a failure leaves the JSON absent and is
/// retried on every boot; the cost of carrying on is that document's settings
/// until the operator fixes the file. Refusing to start over that would be
/// worse than the problem.
async fn seed_named_configurations(config_dir: &std::path::Path, named_dir: &std::path::Path) {
    let seeds = [
        (
            "encoding",
            seed_named(
                &config_dir.join("encoding.xml"),
                &named_dir.join(FerrofinServerConfigurationManager::ENCODING_FILE_NAME),
                EncodingOptions::default(),
                "EncodingOptions",
                config_import::ENCODING_XML_DENY,
            )
            .await,
        ),
        (
            "branding",
            seed_named(
                &config_dir.join("branding.xml"),
                &named_dir.join(FerrofinServerConfigurationManager::BRANDING_FILE_NAME),
                BrandingOptions::default(),
                "BrandingOptions",
                config_import::BRANDING_XML_DENY,
            )
            .await,
        ),
        // The network policy is enforced from this file (see
        // `ferrofin_api::ip_access`), so dropping it on adoption would
        // quietly take down an operator's remote-IP filter and widen their
        // `LocalNetworkSubnets` back to every private range — the server
        // would come up more permissive than the one it replaced, and
        // nothing would say so.
        (
            "network",
            seed_named(
                &config_dir.join("network.xml"),
                &named_dir.join(FerrofinServerConfigurationManager::NETWORK_FILE_NAME),
                ferrofin_networking::NetworkConfiguration::default(),
                "NetworkConfiguration",
                config_import::NETWORK_XML_DENY,
            )
            .await,
        ),
        // "Date added behavior for new content": the library scan dates
        // every new item by it, so an operator who chose "Use date
        // scanned into the library" keeps that choice.
        (
            "metadata",
            seed_named(
                &config_dir.join("metadata.xml"),
                &named_dir.join(FerrofinServerConfigurationManager::METADATA_FILE_NAME),
                MetadataConfiguration::default(),
                "MetadataConfiguration",
                &[],
            )
            .await,
        ),
        // NFO settings: the user whose watched state NFO sidecars carry,
        // the release date format and path substitution.
        (
            "xbmcmetadata",
            seed_named(
                &config_dir.join("xbmcmetadata.xml"),
                &named_dir.join(FerrofinServerConfigurationManager::XBMC_METADATA_FILE_NAME),
                ferrofin_model::configuration::XbmcMetadataOptions::default(),
                "XbmcMetadataOptions",
                &[],
            )
            .await,
        ),
        // Live TV's scalar options: guide days, recording padding,
        // subfolders, the post-processor and NFO/image saving. Seeded here
        // rather than by the once-only `live_tv_import`, so installs adopted
        // before this seed existed get them on their next boot.
        (
            "livetv",
            seed_named(
                &config_dir.join("livetv.xml"),
                &named_dir.join(FerrofinServerConfigurationManager::LIVE_TV_FILE_NAME),
                ferrofin_model::live_tv::LiveTvOptions::default(),
                "LiveTvOptions",
                config_import::LIVETV_OPTIONS_XML_DENY,
            )
            .await,
        ),
    ];
    for (key, seeded) in seeds {
        if let Err(e) = seeded {
            tracing::warn!(
                configuration_key = key,
                error = %e,
                "could not seed a named configuration from jellyfin's xml; the server is \
                 starting on ferrofin's defaults for it, and will try again on the next boot"
            );
        }
    }
}

/// Writes `json_path` from the Jellyfin XML at `xml_path`, if that XML exists.
///
/// Named configurations (`encoding`, `branding`, `livetv`, …) are read on demand and fall
/// back to their defaults when their file is absent, so adoption has to
/// materialize them eagerly or the imported values would never be seen.
///
/// # Errors
///
/// Returns [`ServiceError`] if the XML cannot be imported or the JSON cannot be
/// written — the same reasoning as [`adopt_xml`]: a silent miss here is how
/// hardware transcoding turns itself off.
async fn seed_named<T>(
    xml_path: &std::path::Path,
    json_path: &std::path::Path,
    default: T,
    root_name: &str,
    deny: &[&str],
) -> Result<(), ServiceError>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    if json_path.exists() || !xml_path.exists() {
        return Ok(());
    }
    let imported = adopt_xml(xml_path, default, root_name, deny).await?;
    let json = serde_json::to_vec_pretty(&imported)
        .map_err(|e| ServiceError::Backend(format!("serialize the adopted configuration: {e}")))?;
    write_atomically(json_path, json, "write the adopted configuration").await
}

/// Restores `IsStartupWizardCompleted` on an install adopted before the XML
/// import existed, where losing it left the `FirstTimeSetupOrAuth` endpoints
/// open to anonymous callers.
///
/// Only this one flag is touched, and only in the one direction: Jellyfin's XML
/// says the wizard was completed while our JSON says it was not. Ferrofin itself
/// only ever sets the flag; an administrator can clear it (`POST
/// /System/Configuration`, or by editing `system.json` to start over after the
/// database was lost), and on an install that still has a `system.xml` saying
/// `true` this repair sets it again — which is why starting over means clearing
/// it in both files, as [`startup_wizard_completed_files`] and the start-over
/// refusal name them. Every other setting is left as the operator has it — a
/// late, wholesale re-import would overwrite choices they made after adopting.
async fn repair_lost_startup_flag(
    config_dir: &std::path::Path,
    config_file: &std::path::Path,
    config: &mut ServerConfiguration,
) {
    if config.is_startup_wizard_completed {
        return;
    }
    let system_xml = config_dir.join("system.xml");
    if !system_xml.exists() {
        return;
    }
    // Everything below stays non-fatal — unlike adoption, this runs on a server
    // that boots fine today, and refusing to start would be the worse outcome.
    // But it must not be *silent*: we are standing in the exact state the
    // repair exists for, so an unreadable file gets said out loud.
    let unverified = |reason: &str| {
        tracing::warn!(
            path = %system_xml.display(),
            reason,
            "the setup wizard is marked incomplete and jellyfin's configuration could not be \
             read to check whether that is right; if this server was adopted from jellyfin, the \
             first-time-setup endpoints are reachable anonymously until it is set"
        );
    };
    let xml = match tokio::fs::read_to_string(&system_xml).await {
        Ok(xml) => xml,
        Err(e) => return unverified(&e.to_string()),
    };
    match import_system_xml(&xml) {
        Ok(jellyfin) if jellyfin.is_startup_wizard_completed => {}
        // Jellyfin agrees the wizard was never completed: nothing to repair.
        Ok(_) => return,
        Err(e) => return unverified(&e.to_string()),
    }
    config.is_startup_wizard_completed = true;
    tracing::warn!(
        path = %system_xml.display(),
        "this install was adopted before ferrofin imported jellyfin's configuration, so the \
         completed-setup flag was lost and the first-time-setup endpoints were reachable \
         anonymously; restoring it. Other settings are left alone — delete system.json to \
         re-import them all from jellyfin's xml."
    );
    if let Err(e) = write_config(config_file, config).await {
        tracing::error!(error = %e, "could not persist the restored completed-setup flag");
    }
}

/// The configuration files under `config_dir` that record the startup wizard as
/// completed — read before the database is opened, without writing anything.
///
/// An empty list means setup has not been completed. Otherwise it names every
/// file [`FerrofinServerConfigurationManager::load`] would take the completed
/// flag from: `system.json` when it says so, and Jellyfin's `system.xml` when
/// it says so — the import source while `system.json` does not exist yet, and
/// the source [`repair_lost_startup_flag`] restores the flag from when
/// `system.json` says the wizard is incomplete. Resetting the flag means
/// setting it to false in all of them; one left true sets it again.
///
/// The failure modes follow `load`: an unreadable `system.json`, or an
/// unreadable `system.xml` that is about to be imported, is an error (the
/// server would not start on it anyway), while an unreadable `system.xml` next
/// to an existing `system.json` is not consulted, as the repair skips it too.
///
/// # Errors
///
/// Returns [`ServiceError`] if `system.json` exists but cannot be read or
/// parsed, or if it is absent and `system.xml` exists but cannot be imported.
pub fn startup_wizard_completed_files(
    config_dir: &std::path::Path,
) -> Result<Vec<PathBuf>, ServiceError> {
    let json_path = config_dir.join(FerrofinServerConfigurationManager::CONFIG_FILE_NAME);
    let xml_path = config_dir.join("system.xml");
    let mut completed = Vec::new();
    if json_path.exists() {
        let bytes =
            std::fs::read(&json_path).map_err(|e| io_err("read configuration", &json_path, &e))?;
        let config = serde_json::from_slice::<ServerConfiguration>(&bytes)
            .map_err(|e| json_err("invalid configuration JSON", &json_path, &e))?;
        if config.is_startup_wizard_completed {
            completed.push(json_path);
        }
        let jellyfin_completed = std::fs::read_to_string(&xml_path)
            .ok()
            .and_then(|xml| import_system_xml(&xml).ok())
            .is_some_and(|jellyfin| jellyfin.is_startup_wizard_completed);
        if jellyfin_completed {
            completed.push(xml_path);
        }
    } else if xml_path.exists() {
        let xml = std::fs::read_to_string(&xml_path)
            .map_err(|e| io_err("read the jellyfin configuration", &xml_path, &e))?;
        if import_system_xml(&xml)?.is_startup_wizard_completed {
            completed.push(xml_path);
        }
    }
    Ok(completed)
}

/// Imports Jellyfin's `system.xml` document over Ferrofin's defaults.
fn import_system_xml(xml: &str) -> Result<ServerConfiguration, ServiceError> {
    config_import::import_over(
        &default_server_configuration(),
        xml,
        "ServerConfiguration",
        config_import::SYSTEM_XML_DENY,
    )
}

/// Serializes and writes a configuration to `path` (creating the parent dir).
async fn write_config(
    path: &std::path::Path,
    configuration: &ServerConfiguration,
) -> Result<(), ServiceError> {
    let json = serde_json::to_vec_pretty(configuration)
        .map_err(|e| ServiceError::Backend(format!("serialize configuration: {e}")))?;
    write_atomically(path, json, "write configuration").await
}

/// Writes a boot-time configuration file the way every runtime configuration
/// save does.
///
/// [`ferrofin_util::file_helper::atomic_write`] creates the parent directory,
/// replaces the file only once the new contents are complete, and leaves it
/// private (`0600` on Unix). A boot killed mid-write therefore keeps the
/// previous file, or none, instead of a truncated `system.json` that stops the
/// next boot or a truncated named document that is never seeded again.
async fn write_atomically(
    path: &std::path::Path,
    contents: Vec<u8>,
    action: &'static str,
) -> Result<(), ServiceError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        ferrofin_util::file_helper::atomic_write(&path, &contents)
            .map_err(|e| io_err(action, &path, &e))
    })
    .await
    .map_err(|e| ServiceError::backend(format!("boot configuration writer: {e}")))?
}

/// Wraps a filesystem error touching `path` as a [`ServiceError`].
fn io_err(action: &str, path: &std::path::Path, err: &std::io::Error) -> ServiceError {
    ServiceError::Backend(format!("{action} ({}): {err}", path.display()))
}

/// Wraps a parse error in the JSON document at `path` as a [`ServiceError`]
/// naming the file, as [`io_err`] does for a failed read. Upstream's
/// `MyXmlSerializer.DeserializeFromFile` attaches the same `Filename` to any
/// deserialization failure, so a broken configuration file can be found from
/// the log.
fn json_err(what: &str, path: &std::path::Path, err: &serde_json::Error) -> ServiceError {
    ServiceError::Backend(format!("{what} ({}): {err}", path.display()))
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

        // Jellyfin 12.0: legacy (Emby-era) authorization is off by default.
        assert!(!cfg.enable_legacy_authorization);
        // `CacheSize = Environment.ProcessorCount * 100` (ServerConfiguration.cs:183).
        assert_eq!(
            cfg.cache_size,
            i32::try_from(ferrofin_db::database::usable_cores().saturating_mul(100))
                .unwrap_or(i32::MAX)
        );
        assert!(cfg.cache_size >= 100);
        // The bare document's runtime state — `load` is what turns
        // `IsPortAuthorized` on (see `load_authorizes_the_port_and_persists_it`).
        assert!(!cfg.is_startup_wizard_completed);
        assert!(!cfg.is_port_authorized);
    }

    /// `ApplicationHost.FindParts` (ApplicationHost.cs:720-726) sets
    /// `IsPortAuthorized` on every startup and saves; a fresh Jellyfin therefore
    /// reports it `true`, and so must a fresh Ferrofin.
    #[tokio::test]
    async fn load_authorizes_the_port_and_persists_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());

        let mgr = FerrofinServerConfigurationManager::load(Arc::clone(&paths))
            .await
            .expect("load");
        assert!(mgr.snapshot().is_port_authorized);

        // Persisted, not just in memory: reading the file back sees it.
        let bytes = std::fs::read(&mgr.config_file).expect("read config");
        let persisted: ServerConfiguration = serde_json::from_slice(&bytes).expect("parse config");
        assert!(persisted.is_port_authorized);
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
    async fn cache_override_is_live_persisted_and_clear_requires_fresh_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());
        let original = paths.cache_path();
        let mgr = FerrofinServerConfigurationManager::load(Arc::clone(&paths))
            .await
            .expect("load");
        let mut config = mgr.snapshot();
        for name in ["cache-one", "cache-two"] {
            let cache = tmp.path().join(name);
            std::fs::create_dir(&cache).expect("cache");
            config.cache_path = Some(cache.to_string_lossy().into_owned());
            mgr.update_configuration(&config).await.expect("save");
            assert_eq!(paths.cache_path(), cache.to_string_lossy());
            assert_eq!(
                paths.image_cache_path(),
                cache.join("images").to_string_lossy()
            );
            assert!(cache.join(".jellyfin-cache").is_file());
            assert!(
                std::fs::read_to_string(cache.join("CACHEDIR.TAG"))
                    .expect("tag")
                    .starts_with("Signature: 8a477f597d28d172789f06886806bc55\n")
            );
        }
        let reloaded_paths = test_paths(tmp.path());
        FerrofinServerConfigurationManager::load(Arc::clone(&reloaded_paths))
            .await
            .expect("reload");
        assert_eq!(reloaded_paths.cache_path(), paths.cache_path());
        config.cache_path = Some("  ".into());
        mgr.update_configuration(&config).await.expect("clear");
        assert_ne!(paths.cache_path(), original);
        let restarted_paths = test_paths(tmp.path());
        FerrofinServerConfigurationManager::load(Arc::clone(&restarted_paths))
            .await
            .expect("restart");
        assert_eq!(restarted_paths.cache_path(), original);
    }

    #[tokio::test]
    async fn rejected_cache_paths_preserve_saved_configuration_and_live_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());
        let mgr = FerrofinServerConfigurationManager::load(Arc::clone(&paths))
            .await
            .expect("load");
        let previous = std::fs::read(&mgr.config_file).expect("config");
        let original = paths.cache_path();
        let mut config = mgr.snapshot();
        config.cache_path = Some(tmp.path().join("missing").to_string_lossy().into_owned());
        assert!(matches!(
            mgr.update_configuration(&config).await,
            Err(ServiceError::NotFound(_))
        ));
        let incompatible = tmp.path().join("config-root");
        std::fs::create_dir(&incompatible).expect("mkdir");
        std::fs::write(incompatible.join(".jellyfin-config"), "").expect("marker");
        config.cache_path = Some(incompatible.to_string_lossy().into_owned());
        assert!(matches!(
            mgr.update_configuration(&config).await,
            Err(ServiceError::Backend(_))
        ));
        assert_eq!(std::fs::read(&mgr.config_file).expect("config"), previous);
        assert_eq!(paths.cache_path(), original);
        assert!(!incompatible.join(".jellyfin-cache").exists());
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
        // The trait seam must hand out the *shared* handle, not a deep copy:
        // this is the per-request read path, so pointer identity is the
        // invariant, not just value equality.
        let via_trait = mgr.configuration().await.expect("configuration");
        assert!(
            Arc::ptr_eq(&before, &via_trait),
            "the trait accessor must share the document, not deep-clone it"
        );
        assert_eq!(*via_trait, *before);

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
            (*mgr.configuration().await.expect("configuration")).clone(),
        ] {
            assert_eq!(observed.server_name, "Swapped");
            assert_eq!(observed.sort_remove_words, vec!["das", "der"]);
            assert_eq!(observed.cors_hosts, vec!["https://example.invalid"]);
            assert_eq!(observed.min_resume_pct, 11);
            assert!(observed.enable_metrics);
            assert_eq!(observed, cfg, "the read must equal the written document");
        }

        // And the swapped-in document is itself shared, not re-cloned per read —
        // through the trait seam as well as the in-crate handle.
        assert!(Arc::ptr_eq(&after, &mgr.snapshot_shared()));
        assert!(Arc::ptr_eq(
            &after,
            &mgr.configuration().await.expect("configuration")
        ));
    }

    /// Metadata-path validation reads the live document, so an unchanged path is
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
    async fn resolved_encoder_display_preserves_options_and_persists_across_restart() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());
        let mgr = FerrofinServerConfigurationManager::load(Arc::clone(&paths))
            .await
            .expect("load");
        let mut options = EncodingOptions {
            encoding_thread_count: 7,
            encoder_app_path: Some("configured-executable".into()),
            encoder_app_path_display: Some("old-display".into()),
            ..EncodingOptions::default()
        };
        ferrofin_util::file_helper::atomic_write(
            &mgr.encoding_file,
            &serde_json::to_vec(&options).expect("serialize"),
        )
        .expect("save original");
        mgr.set_encoder_app_path_display("/resolved/ffmpeg")
            .await
            .expect("display");
        options.encoder_app_path_display = Some("/resolved/ffmpeg".into());
        assert_eq!(mgr.get_encoding_options().await.expect("encoding"), options);
        let restarted = FerrofinServerConfigurationManager::load(paths)
            .await
            .expect("restart");
        assert_eq!(
            restarted.get_encoding_options().await.expect("encoding"),
            options
        );
        mgr.set_encoder_app_path_display("/new/ffmpeg")
            .await
            .expect("changed executable");
        options.encoder_app_path_display = Some("/new/ffmpeg".into());
        assert_eq!(mgr.get_encoding_options().await.expect("encoding"), options);
    }

    #[tokio::test]
    async fn resolved_encoder_display_initializes_absent_or_malformed_encoding_config() {
        for malformed in [false, true] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
                .await
                .expect("load");
            if malformed {
                ferrofin_util::file_helper::atomic_write(&mgr.encoding_file, b"bad json")
                    .expect("corrupt config");
            }
            mgr.set_encoder_app_path_display("/resolved/ffmpeg")
                .await
                .expect("display");
            assert_eq!(
                mgr.get_encoding_options().await.expect("encoding"),
                EncodingOptions {
                    encoder_app_path_display: Some("/resolved/ffmpeg".into()),
                    ..EncodingOptions::default()
                }
            );
        }
    }

    #[tokio::test]
    async fn resolved_encoder_display_reports_persistence_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("load");
        std::fs::create_dir_all(&mgr.encoding_file).expect("block destination");
        assert!(
            mgr.set_encoder_app_path_display("/resolved/ffmpeg")
                .await
                .is_err()
        );
        assert!(mgr.encoding_file.is_dir());
    }

    #[tokio::test]
    async fn transcode_path_loads_saved_root_and_publishes_committed_updates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());
        let encoding_file =
            PathBuf::from(paths.user_configuration_directory_path()).join("named/encoding.json");
        let first = tmp.path().join("first-transcodes");
        let first_json = serde_json::to_string(&EncodingOptions {
            transcoding_temp_path: Some(first.to_string_lossy().into_owned()),
            ..EncodingOptions::default()
        })
        .expect("serialize");
        ferrofin_util::file_helper::atomic_write(&encoding_file, first_json.as_bytes())
            .expect("save startup configuration");
        let mgr = FerrofinServerConfigurationManager::load(Arc::clone(&paths))
            .await
            .expect("load");
        assert_eq!(PathBuf::from(paths.transcode_path()), first);
        assert!(first.join(".jellyfin-transcode").is_file());
        let operation = paths.transcode_path();
        let second = tmp.path().join("second-transcodes");
        let second_json = serde_json::to_string(&EncodingOptions {
            transcoding_temp_path: Some(second.to_string_lossy().into_owned()),
            ..EncodingOptions::default()
        })
        .expect("serialize");
        ferrofin_util::file_helper::atomic_write(&encoding_file, second_json.as_bytes())
            .expect("persist named update");
        mgr.named_configuration_updated("ENCODING", second_json.into());
        assert_eq!(PathBuf::from(paths.transcode_path()), second);
        assert_eq!(PathBuf::from(operation), first);
        assert!(
            !second.exists(),
            "save publication does not create the directory"
        );
        paths.prepare_transcode_path().expect("operation starts");
        assert!(second.join(".jellyfin-transcode").is_file());
        let restarted = FerrofinServerConfigurationManager::load(Arc::clone(&paths))
            .await
            .expect("restart");
        assert_eq!(
            PathBuf::from(restarted.concrete_paths().transcode_path()),
            second
        );
        mgr.named_configuration_updated("encoding", Arc::from(r#"{"TranscodingTempPath":""}"#));
        assert_eq!(
            PathBuf::from(paths.transcode_path()),
            PathBuf::from(paths.cache_path()).join("transcodes")
        );
    }

    #[tokio::test]
    async fn transcode_path_rejects_conflicting_startup_markers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());
        let transcode = PathBuf::from(paths.cache_path()).join("transcodes");
        let nested = transcode.join("nested");
        std::fs::create_dir_all(&nested).expect("directory");
        std::fs::write(nested.join(".jellyfin-data"), b"").expect("wrong owner");
        assert!(
            FerrofinServerConfigurationManager::load(paths)
                .await
                .is_err()
        );
        assert!(!transcode.join(".jellyfin-transcode").exists());
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

    /// C# `BaseConfigurationManager.LoadConfiguration`: a named document that
    /// does not parse is logged with its path and read as the defaults, so
    /// every reader (the branding routes, the transcode throttle, the segment
    /// cleaner, trickplay) carries on. The warning comes once per change of the
    /// file, not on every read.
    #[tokio::test]
    async fn an_invalid_named_document_reads_as_its_defaults_and_warns_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("load");
        std::fs::create_dir_all(mgr.branding_file.parent().expect("named dir"))
            .expect("create named dir");
        std::fs::write(&mgr.branding_file, b"{ \"LoginDisclaimer\": \"hi\", }")
            .expect("write bad branding");
        std::fs::write(&mgr.encoding_file, b"not json").expect("write bad encoding");

        let logs = crate::test_support::CapturedLogs::default();
        {
            let _guard = tracing::subscriber::set_default(
                tracing_subscriber::fmt()
                    .with_writer(logs.clone())
                    .with_ansi(false)
                    .finish(),
            );
            for _ in 0..3 {
                assert_eq!(
                    mgr.get_branding().await.expect("branding"),
                    BrandingOptions::default()
                );
                assert_eq!(
                    mgr.get_encoding_options().await.expect("encoding"),
                    EncodingOptions::default()
                );
            }
        }
        let text = logs.text();
        for file in [&mgr.branding_file, &mgr.encoding_file] {
            let warnings: Vec<&str> = text
                .lines()
                .filter(|line| {
                    line.contains(" WARN ") && line.contains(&file.display().to_string())
                })
                .collect();
            assert_eq!(warnings.len(), 1, "one warning per broken file:\n{text}");
        }

        // A save overwrites the broken file, as the dashboard's would.
        let branding = BrandingOptions {
            login_disclaimer: Some("fixed".to_owned()),
            ..BrandingOptions::default()
        };
        mgr.update_branding(&branding).await.expect("save branding");
        assert_eq!(mgr.get_branding().await.expect("branding"), branding);
    }

    /// A hand-edited `system.json` that no longer parses stops the boot with
    /// an error naming the file, so an operator with several data directories
    /// does not have to guess which one broke.
    #[tokio::test]
    async fn a_malformed_system_json_names_the_file_at_boot() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("config").join("system.json");
        std::fs::create_dir_all(file.parent().expect("config dir")).expect("config dir");
        std::fs::write(&file, b"{ \"EnableMetrics\": true, }").expect("system.json");

        let error = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect_err("must not boot on a broken system.json");
        assert!(
            error.to_string().contains(&file.display().to_string()),
            "{error}"
        );
    }

    /// The boot-time transcode-path read falls back to the defaults for a
    /// broken `encoding.json`, and its warning names the file.
    #[tokio::test]
    async fn a_malformed_encoding_json_at_boot_is_logged_with_its_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());
        let file = tmp
            .path()
            .join("config")
            .join("users")
            .join(FerrofinServerConfigurationManager::NAMED_SUBDIR)
            .join(FerrofinServerConfigurationManager::ENCODING_FILE_NAME);
        std::fs::create_dir_all(file.parent().expect("named dir")).expect("named dir");
        std::fs::write(&file, b"{ not json").expect("encoding.json");

        let logs = crate::test_support::CapturedLogs::default();
        {
            let _guard = tracing::subscriber::set_default(
                tracing_subscriber::fmt()
                    .with_writer(logs.clone())
                    .with_ansi(false)
                    .finish(),
            );
            FerrofinServerConfigurationManager::load(Arc::clone(&paths))
                .await
                .expect("a broken encoding.json does not stop the boot");
        }
        let text = logs.text();
        let warning = text
            .lines()
            .find(|line| line.contains("transcode path"))
            .unwrap_or_else(|| panic!("no transcode-path warning in:\n{text}"));
        assert!(warning.contains(" WARN "), "{warning}");
        assert!(warning.contains(&file.display().to_string()), "{warning}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn metadata_directory_write_failure_preserves_config_and_clearing_resets_live_path() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());
        let mgr = FerrofinServerConfigurationManager::load(Arc::clone(&paths))
            .await
            .expect("load");
        let original = paths.internal_metadata_path();
        let valid = tmp.path().join("custom-metadata");
        std::fs::create_dir(&valid).expect("mkdir");
        let mut config = mgr.snapshot();
        config.metadata_path = valid.to_string_lossy().into_owned();
        mgr.update_configuration(&config).await.expect("save valid");
        assert_eq!(paths.internal_metadata_path(), config.metadata_path);
        let previous = std::fs::read(&mgr.config_file).expect("read config");
        let blocked = tmp.path().join("readonly");
        std::fs::create_dir(&blocked).expect("mkdir");
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o555))
            .expect("readonly");
        config.metadata_path = blocked.to_string_lossy().into_owned();
        let cannot_write = std::fs::File::create(blocked.join("probe")).is_err();
        let result = mgr.update_configuration(&config).await;
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755))
            .expect("restore permissions");
        if cannot_write {
            assert!(matches!(result, Err(ServiceError::Backend(_))));
            assert_eq!(
                std::fs::read(&mgr.config_file).expect("read config"),
                previous
            );
            assert_eq!(paths.internal_metadata_path(), valid.to_string_lossy());
        }
        config.metadata_path = String::new();
        mgr.update_configuration(&config).await.expect("clear");
        assert_eq!(paths.internal_metadata_path(), original);
        assert!(std::path::Path::new(&original).is_dir());
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
        assert!(matches!(err, ServiceError::NotFound(_)));
    }

    /// A fresh install is 12.0's `false`; an adopted `system.xml` carries its
    /// own value through the import untouched — a 10.11.8 server's `true`
    /// (its clients may still send Emby-era credentials) as much as a 12.0
    /// server's `false`. Ferrofin never runs 12.0's "set it to false" routine
    /// over an adopted configuration.
    #[test]
    fn an_adopted_legacy_authorization_flag_is_honoured_as_imported() {
        assert!(!default_server_configuration().enable_legacy_authorization);
        for (value, expected) in [("true", true), ("false", false)] {
            let xml = format!(
                "<ServerConfiguration><IsStartupWizardCompleted>true</IsStartupWizardCompleted>\
                 <EnableLegacyAuthorization>{value}</EnableLegacyAuthorization>\
                 </ServerConfiguration>"
            );
            let imported = config_import::import_over(
                &default_server_configuration(),
                &xml,
                "ServerConfiguration",
                config_import::SYSTEM_XML_DENY,
            )
            .expect("import");
            assert_eq!(imported.enable_legacy_authorization, expected, "{value}");
        }
        // A `system.xml` that never mentions the flag keeps the 12.0 default.
        let silent = config_import::import_over(
            &default_server_configuration(),
            "<ServerConfiguration><IsStartupWizardCompleted>true</IsStartupWizardCompleted>\
             </ServerConfiguration>",
            "ServerConfiguration",
            config_import::SYSTEM_XML_DENY,
        )
        .expect("import");
        assert!(!silent.enable_legacy_authorization);
    }

    /// Lays down a Jellyfin `config/` directory under `root` — the shape
    /// [`load`](FerrofinServerConfigurationManager::load) meets when adopting.
    fn write_jellyfin_config(root: &std::path::Path, system_xml: &str) {
        let dir = root.join("config");
        std::fs::create_dir_all(&dir).expect("config dir");
        std::fs::write(dir.join("system.xml"), system_xml).expect("system.xml");
        std::fs::write(
            dir.join("encoding.xml"),
            "<EncodingOptions>\
             <HardwareAccelerationType>nvenc</HardwareAccelerationType>\
             <TranscodingTempPath>/config/transcodes</TranscodingTempPath>\
             </EncodingOptions>",
        )
        .expect("encoding.xml");
        std::fs::write(
            dir.join("branding.xml"),
            "<BrandingOptions><SplashscreenEnabled>true</SplashscreenEnabled>\
             <SplashscreenLocation>/config/splash.jpg</SplashscreenLocation>\
             </BrandingOptions>",
        )
        .expect("branding.xml");
        std::fs::write(dir.join("network.xml"), REAL_NETWORK_XML).expect("network.xml");
    }

    /// A `network.xml` as a real Jellyfin 10.11.8 writes it — element names,
    /// casing, the `<string>` list wrapper and the self-closing empty elements
    /// all copied verbatim from a live deployment, because those are exactly
    /// what an import gets wrong.
    const REAL_NETWORK_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<NetworkConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <BaseUrl />
  <EnableHttps>false</EnableHttps>
  <CertificatePath>/config/cert.pfx</CertificatePath>
  <CertificatePassword>hunter2</CertificatePassword>
  <InternalHttpPort>8096</InternalHttpPort>
  <AutoDiscovery>true</AutoDiscovery>
  <EnableIPv4>true</EnableIPv4>
  <EnableIPv6>false</EnableIPv6>
  <EnableRemoteAccess>true</EnableRemoteAccess>
  <LocalNetworkSubnets>
    <string>192.168.1.0/24</string>
  </LocalNetworkSubnets>
  <LocalNetworkAddresses />
  <KnownProxies>
    <string>10.0.0.0/8</string>
  </KnownProxies>
  <IgnoreVirtualInterfaces>true</IgnoreVirtualInterfaces>
  <VirtualInterfaceNames>
    <string>veth</string>
  </VirtualInterfaceNames>
  <PublishedServerUriBySubnet />
  <RemoteIPFilter>
    <string>203.0.113.0/24</string>
  </RemoteIPFilter>
  <IsRemoteIPFilterBlacklist>true</IsRemoteIPFilterBlacklist>
</NetworkConfiguration>"#;

    /// Adoption must carry the operator's network policy across, because the
    /// request path now ENFORCES it: dropping the file would silently retire
    /// their remote-IP filter and widen `LocalNetworkSubnets` back to every
    /// private range — a server that comes up more permissive than the one it
    /// replaced, saying nothing.
    #[tokio::test]
    async fn adoption_imports_the_network_policy_but_not_the_certificate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_jellyfin_config(tmp.path(), COMPLETED_SYSTEM_XML);
        let _mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("load");

        let written = std::fs::read_to_string(
            tmp.path()
                .join("config")
                .join("users")
                .join("named")
                .join("network.json"),
        )
        .expect("network.json");
        let config: ferrofin_networking::NetworkConfiguration =
            serde_json::from_str(&written).expect("parses back");

        assert_eq!(config.remote_ip_filter, ["203.0.113.0/24"]);
        assert!(config.is_remote_ip_filter_blacklist);
        assert_eq!(config.local_network_subnets, ["192.168.1.0/24"]);
        assert_eq!(config.known_proxies, ["10.0.0.0/8"]);
        assert_eq!(config.virtual_interface_names, ["veth"]);
        assert!(config.enable_remote_access);
        assert!(!config.enable_ipv6, "a false element is not a missing one");
        // …and NOT the certificate: its path is inside the Jellyfin container
        // and its password unlocks nothing we kept.
        assert_eq!(config.certificate_path, "");
        assert_eq!(config.certificate_password, "");
    }

    const COMPLETED_SYSTEM_XML: &str = "<ServerConfiguration>\
         <IsStartupWizardCompleted>true</IsStartupWizardCompleted>\
         <ServerName>basement</ServerName>\
         <MetadataPath>/config/metadata</MetadataPath>\
         </ServerConfiguration>";

    #[tokio::test]
    async fn adoption_imports_every_jellyfin_config_document() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_jellyfin_config(tmp.path(), COMPLETED_SYSTEM_XML);
        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("load");

        let cfg = mgr.snapshot();
        // The security-critical one: without it `FirstTimeSetupOrAuth` serves
        // anonymously.
        assert!(cfg.is_startup_wizard_completed);
        assert_eq!(cfg.server_name, "basement");
        assert_eq!(
            cfg.metadata_path, "",
            "a container path must not be adopted"
        );

        // Named configs are read on demand from their own files, so this also
        // pins that adoption wrote them where the readers look.
        let encoding = mgr.get_encoding_options().await.expect("encoding");
        assert_eq!(
            encoding.hardware_acceleration_type,
            ferrofin_model::entities::HardwareAccelerationType::nvenc
        );
        assert_eq!(
            encoding.transcoding_temp_path,
            EncodingOptions::default().transcoding_temp_path,
            "a container path must not be adopted"
        );
        let branding = mgr.get_branding().await.expect("branding");
        assert!(branding.splashscreen_enabled);
        assert_eq!(
            branding.splashscreen_location,
            BrandingOptions::default().splashscreen_location,
            "a container path must not be adopted"
        );
    }

    #[tokio::test]
    async fn a_fresh_install_with_no_jellyfin_xml_still_gets_the_defaults() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("load");
        // `load` authorizes the port on every startup (ApplicationHost.cs:720-726);
        // everything else is the untouched default document.
        let mut expected = default_server_configuration();
        expected.is_port_authorized = true;
        assert_eq!(mgr.snapshot(), expected);
    }

    #[tokio::test]
    async fn a_malformed_jellyfin_config_refuses_to_boot_rather_than_lose_it() {
        // Falling back to defaults here would persist
        // `IsStartupWizardCompleted: false` and re-open the anonymous
        // first-time-setup endpoints — the exact failure the import prevents.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("config");
        std::fs::create_dir_all(&dir).expect("config dir");
        std::fs::write(dir.join("system.xml"), "<ServerConfiguration><A></B>").expect("system.xml");

        let err = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect_err("must not boot on defaults");
        assert!(format!("{err}").contains("system.xml"), "{err}");
        assert!(
            !dir.join("system.json").exists(),
            "a failed adoption must not leave defaults behind as the sentinel"
        );
    }

    #[tokio::test]
    async fn a_second_boot_does_not_re_import_over_the_operator_s_changes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_jellyfin_config(tmp.path(), COMPLETED_SYSTEM_XML);
        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("first load");
        let mut cfg = mgr.snapshot();
        cfg.server_name = "renamed in ferrofin".to_owned();
        mgr.update_configuration(&cfg).await.expect("update");

        let reloaded = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("second load");
        assert_eq!(reloaded.snapshot().server_name, "renamed in ferrofin");
    }

    #[tokio::test]
    async fn an_install_adopted_before_the_import_gets_its_completed_flag_back() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_jellyfin_config(tmp.path(), COMPLETED_SYSTEM_XML);
        // What the buggy build left behind: Ferrofin defaults written over a
        // Jellyfin directory, wizard flag lost.
        let config_file = tmp.path().join("config").join("system.json");
        let mut stale = default_server_configuration();
        stale.server_name = "kept".to_owned();
        write_config(&config_file, &stale)
            .await
            .expect("stale config");
        assert!(!stale.is_startup_wizard_completed);

        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("load");
        let cfg = mgr.snapshot();
        assert!(cfg.is_startup_wizard_completed, "the flag must be restored");
        assert_eq!(cfg.server_name, "kept", "nothing else may be re-imported");

        // Persisted, not just patched in memory.
        let on_disk: ServerConfiguration =
            serde_json::from_slice(&std::fs::read(&config_file).expect("read")).expect("parse");
        assert!(on_disk.is_startup_wizard_completed);
    }

    #[tokio::test]
    async fn the_completed_flag_is_not_invented_when_jellyfin_never_set_it() {
        // Must go through the REPAIR path, not the adoption one, so a stale
        // `system.json` has to already be there — otherwise this only proves
        // that `import_over` carries `false` through.
        let tmp = tempfile::tempdir().expect("tempdir");
        write_jellyfin_config(
            tmp.path(),
            "<ServerConfiguration>\
             <IsStartupWizardCompleted>false</IsStartupWizardCompleted>\
             </ServerConfiguration>",
        );
        let config_file = tmp.path().join("config").join("system.json");
        write_config(&config_file, &default_server_configuration())
            .await
            .expect("stale config");

        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("load");
        assert!(!mgr.snapshot().is_startup_wizard_completed);
        let on_disk: ServerConfiguration =
            serde_json::from_slice(&std::fs::read(&config_file).expect("read")).expect("parse");
        assert!(
            !on_disk.is_startup_wizard_completed,
            "nothing may be written back"
        );
    }

    #[tokio::test]
    async fn an_unreadable_system_xml_does_not_stop_an_already_running_install() {
        // The repair path, unlike adoption, must never turn a booting server
        // into a non-booting one — it only warns.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("config");
        std::fs::create_dir_all(&dir).expect("config dir");
        std::fs::write(dir.join("system.xml"), "<ServerConfiguration><A></B>").expect("system.xml");
        write_config(&dir.join("system.json"), &default_server_configuration())
            .await
            .expect("stale config");

        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("must still boot");
        assert!(!mgr.snapshot().is_startup_wizard_completed);
    }

    /// A corrupt `encoding.xml` costs hardware transcoding, never uptime —
    /// whether or not this is the adoption boot. Seeding is retried on every
    /// boot until its JSON exists, so there is always another chance; that is
    /// what separates it from `system.xml`, whose result is persisted at once.
    #[rstest::rstest]
    #[case::adopting(false)]
    #[case::already_running(true)]
    #[tokio::test]
    async fn a_corrupt_encoding_xml_costs_transcoding_not_uptime(#[case] has_system_json: bool) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("config");
        std::fs::create_dir_all(&dir).expect("config dir");
        std::fs::write(dir.join("encoding.xml"), "<EncodingOptions><A></B>").expect("encoding.xml");
        if has_system_json {
            write_config(&dir.join("system.json"), &default_server_configuration())
                .await
                .expect("existing config");
        }

        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("must still boot");
        assert_eq!(
            mgr.get_encoding_options().await.expect("encoding"),
            EncodingOptions::default()
        );
        assert!(
            !mgr.encoding_file.exists(),
            "nothing was written, so the next boot tries again"
        );
        mgr.set_encoder_app_path_display("/resolved/ffmpeg")
            .await
            .expect("pending adoption is preserved");
        assert!(
            !mgr.encoding_file.exists(),
            "display update must not suppress a later XML import"
        );
        assert_eq!(
            mgr.get_encoding_options().await.expect("encoding"),
            EncodingOptions::default()
        );
        std::fs::write(dir.join("encoding.xml"),
            "<EncodingOptions><EncodingThreadCount>7</EncodingThreadCount><DownMixAudioBoost>1.5</DownMixAudioBoost></EncodingOptions>")
            .expect("repair XML");
        let restarted = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("retry import");
        assert_eq!(
            restarted
                .get_encoding_options()
                .await
                .expect("adopted")
                .encoding_thread_count,
            7
        );
        restarted
            .set_encoder_app_path_display("/resolved/ffmpeg")
            .await
            .expect("display after import");
        let actual = restarted.get_encoding_options().await.expect("updated");
        assert_eq!(actual.encoding_thread_count, 7);
        assert_eq!(actual.down_mix_audio_boost.to_bits(), 1.5_f64.to_bits());
        assert_eq!(
            actual.encoder_app_path_display.as_deref(),
            Some("/resolved/ffmpeg")
        );
    }

    #[tokio::test]
    async fn a_missing_named_config_is_seeded_on_a_later_boot_too() {
        // An install adopted before this import existed has `system.json` but
        // no `encoding.json`, so its hardware transcoding is still off. Seeding
        // is idempotent and runs every boot precisely to repair that.
        let tmp = tempfile::tempdir().expect("tempdir");
        write_jellyfin_config(tmp.path(), COMPLETED_SYSTEM_XML);
        write_config(
            &tmp.path().join("config").join("system.json"),
            &default_server_configuration(),
        )
        .await
        .expect("stale config");

        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("load");
        assert_eq!(
            mgr.get_encoding_options()
                .await
                .expect("encoding")
                .hardware_acceleration_type,
            ferrofin_model::entities::HardwareAccelerationType::nvenc
        );
    }

    #[tokio::test]
    async fn seeding_does_not_overwrite_a_named_config_the_operator_has_saved() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_jellyfin_config(tmp.path(), COMPLETED_SYSTEM_XML);
        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("first load");
        // What `POST /System/Configuration/encoding` leaves behind: the
        // operator has since turned hardware transcoding back off.
        let mut encoding = mgr.get_encoding_options().await.expect("encoding");
        encoding.hardware_acceleration_type =
            ferrofin_model::entities::HardwareAccelerationType::none;
        std::fs::write(
            &mgr.encoding_file,
            serde_json::to_vec_pretty(&encoding).expect("serialize"),
        )
        .expect("save encoding");

        let reloaded = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("second load");
        assert_eq!(
            reloaded
                .get_encoding_options()
                .await
                .expect("encoding")
                .hardware_acceleration_type,
            ferrofin_model::entities::HardwareAccelerationType::none,
            "the operator turned it off; adoption must not turn it back on"
        );
    }

    /// One broken document costs only itself: a corrupt `encoding.xml` must not
    /// keep `network.xml`, whose policy the request path enforces, from being
    /// imported, and each failure is logged with the file it came from.
    #[tokio::test]
    async fn a_corrupt_named_xml_does_not_stop_the_documents_after_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_jellyfin_config(tmp.path(), COMPLETED_SYSTEM_XML);
        let dir = tmp.path().join("config");
        let encoding_xml = dir.join("encoding.xml");
        let livetv_xml = dir.join("livetv.xml");
        std::fs::write(&encoding_xml, "<EncodingOptions><A></B>").expect("encoding.xml");
        std::fs::write(&livetv_xml, "<WrongRoot />").expect("livetv.xml");
        std::fs::write(
            dir.join("metadata.xml"),
            "<MetadataConfiguration><UseFileCreationTimeForDateAdded>false\
             </UseFileCreationTimeForDateAdded></MetadataConfiguration>",
        )
        .expect("metadata.xml");

        let logs = crate::test_support::CapturedLogs::default();
        let mgr = {
            let _guard = tracing::subscriber::set_default(
                tracing_subscriber::fmt()
                    .with_writer(logs.clone())
                    .with_ansi(false)
                    .finish(),
            );
            FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
                .await
                .expect("must still boot")
        };

        let network: ferrofin_networking::NetworkConfiguration =
            seeded_named(tmp.path(), "network");
        assert_eq!(network.remote_ip_filter, ["203.0.113.0/24"]);
        let branding: BrandingOptions = seeded_named(tmp.path(), "branding");
        assert!(branding.splashscreen_enabled);
        assert!(
            !mgr.metadata_configuration()
                .use_file_creation_time_for_date_added
        );
        assert!(!mgr.encoding_file.exists(), "retried on the next boot");
        let named = mgr.encoding_file.parent().expect("named dir");
        assert!(
            !named.join("livetv.json").exists(),
            "retried on the next boot"
        );

        let text = logs.text();
        for xml in [&encoding_xml, &livetv_xml] {
            let warnings = text
                .lines()
                .filter(|line| line.contains(" WARN ") && line.contains(&xml.display().to_string()))
                .count();
            assert_eq!(warnings, 1, "one warning naming {}:\n{text}", xml.display());
        }
    }

    /// The boot writes go through the same atomic, private write as every
    /// runtime save, so a crash mid-write cannot leave a truncated
    /// `system.json` or named document behind.
    #[cfg(unix)]
    #[tokio::test]
    async fn boot_writes_are_private_like_every_other_configuration_write() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().expect("tempdir");
        write_jellyfin_config(tmp.path(), COMPLETED_SYSTEM_XML);
        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("load");
        let named = mgr.encoding_file.parent().expect("named dir");
        for file in [
            mgr.config_file.clone(),
            mgr.encoding_file.clone(),
            named.join("network.json"),
        ] {
            let mode = std::fs::metadata(&file)
                .unwrap_or_else(|e| panic!("{}: {e}", file.display()))
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{}", file.display());
        }
    }

    /// `GetMetadataConfiguration()`: the default until the dashboard saves
    /// one, then what it saved, read live; a document that will not parse
    /// is the default.
    #[tokio::test]
    async fn the_metadata_configuration_is_read_live_from_the_named_store() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("load");
        assert_eq!(
            mgr.metadata_configuration(),
            MetadataConfiguration::default()
        );
        assert!(
            mgr.metadata_configuration()
                .use_file_creation_time_for_date_added
        );

        // What `POST /System/Configuration/metadata` writes for "Use date
        // scanned into the library".
        std::fs::create_dir_all(mgr.metadata_file.parent().expect("named dir")).expect("mkdir");
        std::fs::write(
            &mgr.metadata_file,
            br#"{"UseFileCreationTimeForDateAdded":false}"#,
        )
        .expect("save");
        assert!(
            !mgr.metadata_configuration()
                .use_file_creation_time_for_date_added
        );

        std::fs::write(
            &mgr.metadata_file,
            b"{\"UseFileCreationTimeForDateAdded\": fal",
        )
        .expect("corrupt");
        assert_eq!(
            mgr.metadata_configuration(),
            MetadataConfiguration::default()
        );
    }

    /// Counts the `WARN` events logged while it is the default subscriber.
    struct CountWarnings(Arc<std::sync::atomic::AtomicUsize>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CountWarnings {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    /// A broken `metadata.json` warns once per change of the file, not on
    /// every read (each scan, watcher batch and item refresh reads it); a
    /// document that reads again resets that, so breaking it anew warns.
    #[tokio::test]
    async fn a_broken_metadata_configuration_warns_once_per_change() {
        use tracing_subscriber::layer::SubscriberExt as _;
        let tmp = tempfile::tempdir().expect("tempdir");
        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("load");
        let warnings = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(CountWarnings(Arc::clone(&warnings))),
        );
        let count = || warnings.load(std::sync::atomic::Ordering::SeqCst);
        let write = |body: &[u8], at: u64| {
            std::fs::write(&mgr.metadata_file, body).expect("write");
            std::fs::File::options()
                .write(true)
                .open(&mgr.metadata_file)
                .expect("open")
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(at))
                .expect("mtime");
        };
        std::fs::create_dir_all(mgr.metadata_file.parent().expect("named dir")).expect("mkdir");

        write(b"{\"UseFileCreationTimeForDateAdded\": fal", 1_000_000_000);
        for _ in 0..3 {
            assert_eq!(
                mgr.metadata_configuration(),
                MetadataConfiguration::default()
            );
        }
        assert_eq!(count(), 1, "one warning for three reads of the same file");

        write(b"not json either", 1_000_000_100);
        let _ = mgr.metadata_configuration();
        let _ = mgr.metadata_configuration();
        assert_eq!(count(), 2, "a changed file warns again, once");

        write(
            br#"{"UseFileCreationTimeForDateAdded":false}"#,
            1_000_000_200,
        );
        assert!(
            !mgr.metadata_configuration()
                .use_file_creation_time_for_date_added
        );
        assert_eq!(count(), 2, "a valid document does not warn");
        write(b"{", 1_000_000_100);
        let _ = mgr.metadata_configuration();
        assert_eq!(count(), 3, "broken again after it read: warns anew");
    }

    /// Writes `body` as the named document `key`, as a save would.
    fn write_named(mgr: &FerrofinServerConfigurationManager, key: &str, body: &[u8]) {
        let named = mgr.encoding_file.parent().expect("named dir");
        std::fs::create_dir_all(named).expect("named dir");
        std::fs::write(named.join(format!("{key}.json")), body).expect("named document");
    }

    /// `GetConfiguration(key)` for each registered store, case-insensitive:
    /// the saved document bound to its type, so members it omits read as
    /// their defaults and members the type does not have are dropped. The
    /// network document's older member names still bind, and come back under
    /// the names jellyfin-web reads.
    #[tokio::test]
    async fn a_named_configuration_is_its_saved_document_bound_to_its_type() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("load");
        write_named(
            &mgr,
            "network",
            br#"{"BaseUrl":"/jf","EnableIpv4":false,"RemoteIpFilter":["192.168.1.5"],
                "IsRemoteIpFilterBlacklist":true,"Unrecognized":1}"#,
        );
        write_named(&mgr, "metadata", br#"{"Unrecognized":1}"#);
        write_named(&mgr, "xbmcmetadata", br#"{"UserId":"user-1"}"#);
        write_named(&mgr, "livetv", br#"{"PrePaddingSeconds":120}"#);
        write_named(&mgr, "encoding", br#"{"EncodingThreadCount":3}"#);

        let read = |key: &'static str| {
            let mgr = &mgr;
            async move {
                mgr.get_named_configuration(key)
                    .await
                    .expect("read")
                    .unwrap_or_else(|| panic!("{key} is registered"))
            }
        };
        let network = read("NETWORK").await;
        assert_eq!(network["BaseUrl"], "/jf");
        assert_eq!(network["EnableIPv4"], false);
        assert_eq!(
            network["RemoteIPFilter"],
            serde_json::json!(["192.168.1.5"])
        );
        assert_eq!(network["IsRemoteIPFilterBlacklist"], true);
        assert_eq!(network["InternalHttpPort"], 8096);
        for gone in ["EnableIpv4", "RemoteIpFilter", "Unrecognized"] {
            assert!(network.get(gone).is_none(), "{gone}");
        }
        assert_eq!(
            read("metadata").await,
            serde_json::json!({"UseFileCreationTimeForDateAdded": true})
        );
        let nfo = read("xbmcmetadata").await;
        assert_eq!(nfo["UserId"], "user-1");
        assert_eq!(nfo["SaveImagePathsInNfo"], true);
        let live_tv = read("livetv").await;
        assert_eq!(live_tv["PrePaddingSeconds"], 120);
        assert_eq!(live_tv["SaveRecordingNFO"], true);
        let encoding = read("encoding").await;
        assert_eq!(encoding["EncodingThreadCount"], 3);
        assert_eq!(encoding["DownMixAudioBoost"], 2.0);
        assert_eq!(
            read("branding").await,
            serde_json::to_value(BrandingOptions::default()).expect("branding")
        );
        assert_eq!(
            read("database").await,
            serde_json::json!({"DatabaseType": "Jellyfin-SQLite", "LockingBehavior": "NoLock"})
        );
    }

    /// No store is registered under any other key (`ResourceNotFoundException`
    /// upstream), whatever file a save once left behind.
    #[tokio::test]
    async fn an_unregistered_key_has_no_named_configuration() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("load");
        write_named(&mgr, "plugin-owned", br#"{"CustomSetting":1}"#);
        for key in ["plugin-owned", "never-saved", "", "../system", "named"] {
            assert_eq!(
                mgr.get_named_configuration(key).await.expect("read"),
                None,
                "{key}"
            );
        }
    }

    /// `LoadConfiguration`: a document never saved is the default, silently;
    /// one that does not parse or bind is the default too, logged once per
    /// change of the file and never rewritten. A read failure other than
    /// not-found is an error; upstream serves the defaults for some of those
    /// (see the `GET /System/Configuration/{key}` parity row).
    #[tokio::test]
    async fn a_broken_named_configuration_reads_as_its_defaults_and_warns_once() {
        use tracing_subscriber::layer::SubscriberExt as _;
        let tmp = tempfile::tempdir().expect("tempdir");
        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("load");
        let warnings = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(CountWarnings(Arc::clone(&warnings))),
        );
        let count = || warnings.load(std::sync::atomic::Ordering::SeqCst);

        for key in ["network", "metadata", "xbmcmetadata", "livetv"] {
            let missing = mgr.get_named_configuration(key).await.expect("missing");
            assert!(missing.is_some_and(|value| value.is_object()), "{key}");
        }
        assert_eq!(count(), 0, "a document never saved is not a problem");

        let broken: [(&str, &[u8]); 4] = [
            ("network", br#"{"InternalHttpPort":"not a port"}"#),
            (
                "metadata",
                br#"{"UseFileCreationTimeForDateAdded":"invalid"}"#,
            ),
            ("xbmcmetadata", b"{ not json"),
            ("livetv", br#"{"SaveRecordingNFO":"invalid"}"#),
        ];
        for (key, body) in broken {
            write_named(&mgr, key, body);
            for _ in 0..3 {
                let value = mgr
                    .get_named_configuration(key)
                    .await
                    .expect("defaults")
                    .expect("registered");
                assert!(value.is_object(), "{key}");
            }
            let file = mgr
                .encoding_file
                .parent()
                .expect("named dir")
                .join(format!("{key}.json"));
            assert_eq!(std::fs::read(file).expect("kept"), body, "{key}");
        }
        assert_eq!(count(), 4, "one warning per broken document");
        let network = mgr
            .get_named_configuration("network")
            .await
            .expect("defaults")
            .expect("registered");
        assert_eq!(network["InternalHttpPort"], 8096);

        // A directory where the file belongs cannot be read at all.
        let named = mgr.encoding_file.parent().expect("named dir");
        std::fs::remove_file(named.join("livetv.json")).expect("remove");
        std::fs::create_dir(named.join("livetv.json")).expect("unreadable");
        let error = mgr
            .get_named_configuration("livetv")
            .await
            .expect_err("an unreadable document is an error");
        assert!(error.to_string().contains("livetv.json"), "{error}");
    }

    /// Adoption carries Jellyfin's `metadata.xml` over, so "Use date scanned
    /// into the library" survives the move.
    #[tokio::test]
    async fn adoption_imports_the_date_added_behavior() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_jellyfin_config(tmp.path(), COMPLETED_SYSTEM_XML);
        std::fs::write(
            tmp.path().join("config").join("metadata.xml"),
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <MetadataConfiguration>\
             <UseFileCreationTimeForDateAdded>false</UseFileCreationTimeForDateAdded>\
             </MetadataConfiguration>",
        )
        .expect("metadata.xml");
        let mgr = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("load");
        assert!(
            !mgr.metadata_configuration()
                .use_file_creation_time_for_date_added
        );
        assert!(mgr.metadata_file.exists(), "seeded as the named JSON");
    }

    /// The named JSON document `load` wrote for `key`, parsed.
    fn seeded_named<T: serde::de::DeserializeOwned>(root: &std::path::Path, key: &str) -> T {
        let path = root
            .join("config")
            .join("users")
            .join("named")
            .join(format!("{key}.json"));
        let json = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} was not seeded: {e}", path.display()));
        serde_json::from_str(&json).expect("parses back")
    }

    /// Adoption carries Jellyfin's NFO settings (`xbmcmetadata.xml`) over, so
    /// sidecars keep the watched state of the user the operator chose and
    /// their date format.
    #[tokio::test]
    async fn adoption_imports_the_nfo_settings() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_jellyfin_config(tmp.path(), COMPLETED_SYSTEM_XML);
        std::fs::write(
            tmp.path().join("config").join("xbmcmetadata.xml"),
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <XbmcMetadataOptions>\
             <UserId>6a3c9f0e8b1d4e2fa7c5d3b9e1f0a2c4</UserId>\
             <ReleaseDateFormat>dd.MM.yyyy</ReleaseDateFormat>\
             <SaveImagePathsInNfo>false</SaveImagePathsInNfo>\
             <EnablePathSubstitution>false</EnablePathSubstitution>\
             <EnableExtraThumbsDuplication>true</EnableExtraThumbsDuplication>\
             </XbmcMetadataOptions>",
        )
        .expect("xbmcmetadata.xml");
        FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("load");

        let nfo: ferrofin_model::configuration::XbmcMetadataOptions =
            seeded_named(tmp.path(), "xbmcmetadata");
        assert_eq!(
            nfo,
            ferrofin_model::configuration::XbmcMetadataOptions {
                user_id: Some("6a3c9f0e8b1d4e2fa7c5d3b9e1f0a2c4".to_owned()),
                release_date_format: "dd.MM.yyyy".to_owned(),
                save_image_paths_in_nfo: false,
                enable_path_substitution: false,
                enable_extra_thumbs_duplication: true,
            }
        );
    }

    /// Adoption carries Jellyfin's scalar Live TV options over: guide days,
    /// recording padding, subfolders, the post-processor and NFO/image saving.
    /// The tuners and listing providers are left out: they live in the
    /// database (`live_tv_import`). So are the recording paths, which point
    /// into Jellyfin's filesystem.
    #[tokio::test]
    async fn adoption_imports_the_live_tv_options_but_not_tuners_or_recording_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_jellyfin_config(tmp.path(), COMPLETED_SYSTEM_XML);
        std::fs::write(
            tmp.path().join("config").join("livetv.xml"),
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <LiveTvOptions xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\
             <GuideDays>10</GuideDays>\
             <RecordingPath>/config/recordings</RecordingPath>\
             <MovieRecordingPath>/config/recordings/movies</MovieRecordingPath>\
             <SeriesRecordingPath>/config/recordings/series</SeriesRecordingPath>\
             <EnableRecordingSubfolders>true</EnableRecordingSubfolders>\
             <EnableOriginalAudioWithEncodedRecordings>true</EnableOriginalAudioWithEncodedRecordings>\
             <TunerHosts><TunerHostInfo><Id>4a1f2b</Id><Url>http://tuner.lan/playlist.m3u</Url>\
             <Type>m3u</Type></TunerHostInfo></TunerHosts>\
             <ListingProviders><ListingsProviderInfo><Id>9c8d</Id><Type>xmltv</Type>\
             <Path>/config/guide.xml</Path></ListingsProviderInfo></ListingProviders>\
             <PrePaddingSeconds>120</PrePaddingSeconds>\
             <PostPaddingSeconds>300</PostPaddingSeconds>\
             <MediaLocationsCreated><string>/media/recordings</string></MediaLocationsCreated>\
             <RecordingPostProcessor>/usr/local/bin/comskip</RecordingPostProcessor>\
             <RecordingPostProcessorArguments>--ini=/etc/comskip.ini \"{path}\"</RecordingPostProcessorArguments>\
             <SaveRecordingNFO>false</SaveRecordingNFO>\
             <SaveRecordingImages>false</SaveRecordingImages>\
             </LiveTvOptions>",
        )
        .expect("livetv.xml");
        FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .expect("load");

        let live_tv: ferrofin_model::live_tv::LiveTvOptions = seeded_named(tmp.path(), "livetv");
        assert_eq!(
            live_tv,
            ferrofin_model::live_tv::LiveTvOptions {
                guide_days: Some(10),
                enable_recording_subfolders: true,
                enable_original_audio_with_encoded_recordings: true,
                pre_padding_seconds: 120,
                post_padding_seconds: 300,
                media_locations_created: vec!["/media/recordings".to_owned()],
                recording_post_processor: Some("/usr/local/bin/comskip".to_owned()),
                recording_post_processor_arguments: Some(
                    "--ini=/etc/comskip.ini \"{path}\"".to_owned()
                ),
                save_recording_nfo: false,
                save_recording_images: false,
                ..ferrofin_model::live_tv::LiveTvOptions::default()
            }
        );
    }
    #[tokio::test]
    async fn observers_see_committed_snapshots_and_no_failed_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = Arc::new(
            FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
                .await
                .unwrap(),
        );
        let updates = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = Arc::clone(&updates);
        let weak = Arc::downgrade(&manager);
        manager.add_configuration_listener(Arc::new(move |update| {
            let manager = weak.upgrade().unwrap();
            if let ConfigurationUpdate::Server(config) = update {
                let saved: ServerConfiguration =
                    serde_json::from_slice(&std::fs::read(&manager.config_file).unwrap()).unwrap();
                assert_eq!(saved.server_name, config.server_name);
                assert_eq!(manager.snapshot_shared().server_name, config.server_name);
                assert_eq!(
                    manager.paths.internal_metadata_path(),
                    manager.paths.default_internal_metadata_path()
                );
            }
            observed.lock().unwrap().push(update.clone());
        }));
        let mut jobs = tokio::task::JoinSet::new();
        for index in 0..20 {
            let manager = Arc::clone(&manager);
            jobs.spawn(async move {
                let mut config = manager.snapshot();
                config.server_name = format!("server-{index}");
                manager.update_configuration(&config).await.unwrap();
            });
        }
        while let Some(result) = jobs.join_next().await {
            result.unwrap();
        }
        assert_eq!(updates.lock().unwrap().len(), 20);
        let previous = manager.snapshot_shared();
        let bytes = std::fs::read(&manager.config_file).unwrap();
        let mut invalid = (*previous).clone();
        invalid.metadata_path = tmp.path().join("missing").to_string_lossy().into_owned();
        assert!(manager.update_configuration(&invalid).await.is_err());
        assert_eq!(std::fs::read(&manager.config_file).unwrap(), bytes);
        // A persistence failure must not publish an event or swap the live Arc.
        std::fs::remove_file(&manager.config_file).unwrap();
        std::fs::create_dir(&manager.config_file).unwrap();
        let mut rejected = (*previous).clone();
        rejected.server_name = "must-not-publish".into();
        assert!(manager.update_configuration(&rejected).await.is_err());
        assert!(Arc::ptr_eq(&previous, &manager.snapshot_shared()));
        assert_eq!(updates.lock().unwrap().len(), 20);
    }

    #[tokio::test]
    async fn branding_and_named_stores_publish_the_committed_body() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = FerrofinServerConfigurationManager::load(test_paths(tmp.path()))
            .await
            .unwrap();
        let updates = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = Arc::clone(&updates);
        manager.add_configuration_listener(Arc::new(move |update| {
            observed.lock().unwrap().push(update.clone());
        }));
        let branding = BrandingOptions {
            custom_css: Some("body { color: red; }".into()),
            ..Default::default()
        };
        manager.update_branding(&branding).await.unwrap();
        manager
            .named_configuration_updated("NETWORK", Arc::from(r#"{"EnableRemoteAccess":false}"#));
        {
            let events = updates.lock().unwrap();
            assert_eq!(events.len(), 2);
            let ConfigurationUpdate::Named { key, json } = &events[0] else {
                panic!("expected named update")
            };
            assert_eq!(key, "branding");
            assert_eq!(
                json.as_bytes(),
                std::fs::read(&manager.branding_file).unwrap()
            );
            let ConfigurationUpdate::Named { key, json } = &events[1] else {
                panic!("expected named update")
            };
            assert_eq!(key, "network");
            assert_eq!(&**json, r#"{"EnableRemoteAccess":false}"#);
        }
        std::fs::remove_file(&manager.branding_file).unwrap();
        std::fs::create_dir(&manager.branding_file).unwrap();
        assert!(manager.update_branding(&branding).await.is_err());
        assert_eq!(updates.lock().unwrap().len(), 2);
    }

    /// Writes a `system.json` recording `completed` into `dir`.
    fn write_system_json(dir: &std::path::Path, completed: bool) {
        std::fs::create_dir_all(dir).expect("config dir");
        let mut config = default_server_configuration();
        config.is_startup_wizard_completed = completed;
        std::fs::write(
            dir.join("system.json"),
            serde_json::to_vec(&config).expect("serialize"),
        )
        .expect("system.json");
    }

    const INCOMPLETE_SYSTEM_XML: &str = "<ServerConfiguration>\
         <IsStartupWizardCompleted>false</IsStartupWizardCompleted>\
         </ServerConfiguration>";

    #[test]
    fn no_configuration_means_setup_was_never_completed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let files = startup_wizard_completed_files(&tmp.path().join("config")).expect("read");
        assert!(files.is_empty());
    }

    #[rstest::rstest]
    #[case::completed(true)]
    #[case::not_completed(false)]
    fn system_json_records_the_flag(#[case] completed: bool) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("config");
        write_system_json(&dir, completed);
        let files = startup_wizard_completed_files(&dir).expect("read");
        let expected = if completed {
            vec![dir.join("system.json")]
        } else {
            Vec::new()
        };
        assert_eq!(files, expected);
    }

    #[rstest::rstest]
    #[case::completed(COMPLETED_SYSTEM_XML, true)]
    #[case::not_completed(INCOMPLETE_SYSTEM_XML, false)]
    fn a_jellyfin_config_not_yet_imported_records_the_flag(
        #[case] xml: &str,
        #[case] completed: bool,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("config");
        std::fs::create_dir_all(&dir).expect("config dir");
        std::fs::write(dir.join("system.xml"), xml).expect("system.xml");
        let files = startup_wizard_completed_files(&dir).expect("read");
        assert_eq!(files.is_empty(), !completed);
        assert!(
            !dir.join("system.json").exists(),
            "reading the flag must not import"
        );
    }

    /// `load`'s repair turns a `false` in `system.json` back into `true` when
    /// Jellyfin's XML says setup was completed, so the XML counts too — and
    /// starting over means resetting both files.
    #[test]
    fn jellyfin_xml_beside_system_json_counts_as_the_repair_source() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("config");
        write_system_json(&dir, false);
        std::fs::write(dir.join("system.xml"), COMPLETED_SYSTEM_XML).expect("system.xml");
        assert_eq!(
            startup_wizard_completed_files(&dir).expect("read"),
            vec![dir.join("system.xml")]
        );
        write_system_json(&dir, true);
        assert_eq!(
            startup_wizard_completed_files(&dir).expect("read"),
            vec![dir.join("system.json"), dir.join("system.xml")]
        );
        // An unreadable XML beside an existing system.json is skipped, as the
        // repair skips it.
        std::fs::write(dir.join("system.xml"), "<ServerConfiguration><A></B>").expect("xml");
        assert_eq!(
            startup_wizard_completed_files(&dir).expect("read"),
            vec![dir.join("system.json")]
        );
    }

    #[test]
    fn an_unreadable_configuration_is_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("config");
        std::fs::create_dir_all(&dir).expect("config dir");
        std::fs::write(dir.join("system.xml"), "<ServerConfiguration><A></B>").expect("xml");
        assert!(startup_wizard_completed_files(&dir).is_err());
        let json = dir.join("system.json");
        std::fs::write(&json, b"{ not json").expect("json");
        let error = startup_wizard_completed_files(&dir).expect_err("invalid system.json");
        // The first boot step to read it, so its error is the one an operator
        // sees for a hand-edited `system.json`.
        assert!(
            error.to_string().contains(&json.display().to_string()),
            "{error}"
        );
    }
}
