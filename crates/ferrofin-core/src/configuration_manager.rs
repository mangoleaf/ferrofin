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
use ferrofin_model::configuration::{EncodingOptions, ServerConfiguration};
use ferrofin_traits::configuration::ServerConfigurationManager;
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

    /// The on-disk network configuration file name (Jellyfin's named
    /// `network` configuration). This one is not merely served back: the
    /// request path enforces it (`ferrofin_api::ip_access`).
    const NETWORK_FILE_NAME: &'static str = "network.json";

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

        // The named configurations live in their own files and are read on
        // demand, so adoption has to materialize them or the imported values
        // are never seen. Seeding runs on EVERY boot, not just the adoption
        // one: it is a no-op once the JSON exists, and running it
        // unconditionally is what repairs an install adopted before this import
        // existed — whose `encoding.json` is still missing and whose hardware
        // transcoding is therefore still off. It also runs BEFORE `system.json`
        // is written below, since that file is the sentinel for the adoption
        // branch and crashing between the two would lose these for good.
        let seeded = async {
            seed_named(
                &config_dir.join("encoding.xml"),
                &encoding_file,
                EncodingOptions::default(),
                "EncodingOptions",
                config_import::ENCODING_XML_DENY,
            )
            .await?;
            seed_named(
                &config_dir.join("branding.xml"),
                &branding_file,
                BrandingOptions::default(),
                "BrandingOptions",
                config_import::BRANDING_XML_DENY,
            )
            .await?;
            // The network policy is enforced from this file (see
            // `ferrofin_api::ip_access`), so dropping it on adoption would
            // quietly take down an operator's remote-IP filter and widen their
            // `LocalNetworkSubnets` back to every private range — the server
            // would come up more permissive than the one it replaced, and
            // nothing would say so.
            seed_named(
                &config_dir.join("network.xml"),
                &named_dir.join(Self::NETWORK_FILE_NAME),
                ferrofin_networking::NetworkConfiguration::default(),
                "NetworkConfiguration",
                config_import::NETWORK_XML_DENY,
            )
            .await
        };
        if let Err(e) = seeded.await {
            // Never fatal, on any boot. The line between refusing to start and
            // carrying on is not "is this the adoption boot" — it is whether
            // the next boot gets another chance. Seeding is skipped only once
            // its JSON exists, so a failure here leaves the JSON absent and is
            // retried forever; the cost of carrying on is hardware transcoding
            // or a splash screen until the operator fixes the file. Refusing to
            // start over that would be worse than the problem.
            tracing::warn!(
                error = %e,
                "could not seed a named configuration from jellyfin's xml; the server is \
                 starting on ferrofin's defaults for it, and will try again on the next boot"
            );
        }

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
                .map_err(|e| ServiceError::Backend(format!("invalid configuration JSON: {e}")))?;
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

    async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
        Ok(self.snapshot_shared())
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

/// Writes `json_path` from the Jellyfin XML at `xml_path`, if that XML exists.
///
/// Named configurations (`encoding`, `branding`) are read on demand and fall
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
    if let Some(parent) = json_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| io_err("create the named configuration directory", parent, &e))?;
    }
    let json = serde_json::to_vec_pretty(&imported)
        .map_err(|e| ServiceError::Backend(format!("serialize the adopted configuration: {e}")))?;
    tokio::fs::write(json_path, json)
        .await
        .map_err(|e| io_err("write the adopted configuration", json_path, &e))
}

/// Restores `IsStartupWizardCompleted` on an install adopted before the XML
/// import existed, where losing it left the `FirstTimeSetupOrAuth` endpoints
/// open to anonymous callers.
///
/// Only this one flag is touched, and only in the one direction: Jellyfin's XML
/// says the wizard was completed while our JSON says it was not. Ferrofin never
/// clears the flag, so that combination has no innocent explanation. Every
/// other setting is left as the operator has it — a late, wholesale re-import
/// would overwrite choices they made after adopting.
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
    match config_import::import_over(
        &default_server_configuration(),
        &xml,
        "ServerConfiguration",
        config_import::SYSTEM_XML_DENY,
    ) {
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
}
