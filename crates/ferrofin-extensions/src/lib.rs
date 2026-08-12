//! Curated, compiled-in **extensions** for Ferrofin — the Rust answer to Jellyfin
//! plugins (Rust cannot load .NET assemblies at runtime; see
//! `docs/EXTENSIONS.md`).
//!
//! An [`Extension`] is a repo-curated capability that **surfaces as a plugin** on
//! the frozen `/Plugins` API (enable/disable toggle, config page) via the
//! existing [`PluginManager`](ferrofin_traits::plugins::PluginManager), and can
//! contribute background [`ScheduledTask`]s. The set is static —
//! [`builtin_extensions`] — but each is enable/disable-able at runtime; a task
//! self-gates on its plugin's enabled flag, so toggling needs no restart.
//!
//! The composition root drives this: it builds [`RegisteredPlugin`]s from the
//! descriptors ([`registered_plugins`]) and registers each extension's tasks
//! ([`register_tasks`]).

use std::path::PathBuf;
use std::sync::Arc;

use ferrofin_core::{FerrofinTaskManager, PluginConfigPage, RegisteredPlugin, ScheduledTask};
use ferrofin_traits::library::LibraryManager;
use ferrofin_traits::media_segments::MediaSegmentManager;
use ferrofin_traits::merge_versions::MergeVersionsManager;
use ferrofin_traits::plugins::{PluginDescriptor, PluginManager};
use uuid::Uuid;

use crate::fingerprint::Fingerprinter;

pub mod file_transformation;
pub mod fingerprint;
pub mod intro_skipper;
pub mod merge_versions;

/// The collaborators an extension's tasks are allowed to touch (trait objects
/// only, so extensions stay decoupled from the concrete managers).
#[derive(Clone)]
pub struct ExtensionContext {
    /// Enumerate library items (episodes to analyze).
    pub library: Arc<dyn LibraryManager>,
    /// Persist detected intro/credit segments.
    pub media_segments: Arc<dyn MediaSegmentManager>,
    /// Read the extension's enabled flag + JSON configuration.
    pub plugins: Arc<dyn PluginManager>,
    /// The audio fingerprinter, or `None` when Chromaprint (`fpcalc`) is absent —
    /// the intro skipper then reports unavailable.
    pub fingerprinter: Option<Arc<dyn Fingerprinter>>,
    /// Root for per-extension caches (fingerprints): `{cache}/extensions`.
    pub cache_dir: PathBuf,
    /// Bulk merge/split of duplicate versions — the Merge Versions extension's
    /// service, shared by its scheduled tasks and the `/MergeVersions/*` routes.
    pub merge_versions: Arc<dyn MergeVersionsManager>,
}

/// A curated, compiled-in capability that surfaces as a Jellyfin plugin.
pub trait Extension: Send + Sync {
    /// Stable id — also the plugin id in `/Plugins`.
    fn id(&self) -> Uuid;
    /// The `/Plugins` presentation (name, version, description). `enabled` is set
    /// by the plugin manager from persisted state, so the value here is ignored.
    fn descriptor(&self) -> PluginDescriptor;
    /// The seed configuration written on first run (the plugin config schema).
    fn default_config(&self) -> Vec<u8>;
    /// The dashboard pages/resources this extension ships; the first entry is
    /// the main settings page (jellyfin-web links the first page whose
    /// `PluginId` matches). Providing any makes a Settings link appear on the
    /// plugin in the dashboard (served via `GET /web/ConfigurationPage`).
    /// Defaults to none.
    fn config_pages(&self) -> Vec<PluginConfigPage> {
        Vec::new()
    }
    /// The background tasks this extension contributes.
    fn tasks(&self, cx: &ExtensionContext) -> Vec<Arc<dyn ScheduledTask>>;
}

/// The curated set of extensions — the only place they are listed.
#[must_use]
pub fn builtin_extensions() -> Vec<Arc<dyn Extension>> {
    vec![
        Arc::new(intro_skipper::IntroSkipperExtension::new()),
        Arc::new(file_transformation::FileTransformationExtension),
        Arc::new(merge_versions::MergeVersionsExtension),
    ]
}

/// Builds the [`RegisteredPlugin`]s the plugin manager needs, so every extension
/// appears in `/Plugins` with its seed config.
#[must_use]
pub fn registered_plugins(extensions: &[Arc<dyn Extension>]) -> Vec<RegisteredPlugin> {
    extensions
        .iter()
        .map(|ext| {
            let mut plugin = RegisteredPlugin::new(ext.descriptor(), None)
                .with_default_config(ext.default_config());
            for page in ext.config_pages() {
                plugin = plugin.with_config_page(page);
            }
            plugin
        })
        .collect()
}

/// Registers every extension's tasks with the task manager.
pub fn register_tasks(
    extensions: &[Arc<dyn Extension>],
    cx: &ExtensionContext,
    task_manager: &FerrofinTaskManager,
) {
    for ext in extensions {
        for task in ext.tasks(cx) {
            task_manager.register(task);
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::merge_versions::{MergeProgress, MergeVersionsManager};
    use ferrofin_traits::plugins::DisabledPluginManager;
    use ferrofin_traits::tasks::TaskManager;

    use super::*;

    struct NoMerges;

    #[async_trait]
    impl MergeVersionsManager for NoMerges {
        async fn merge_movies(&self, _p: Option<MergeProgress<'_>>) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn split_movies(&self, _p: Option<MergeProgress<'_>>) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn merge_episodes(&self, _p: Option<MergeProgress<'_>>) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn split_episodes(&self, _p: Option<MergeProgress<'_>>) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    async fn context() -> ExtensionContext {
        let db = ferrofin_db::Database::connect_in_memory()
            .await
            .expect("connect");
        db.run_migrations().await.expect("migrations");
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ferrofin_core::item_type_lookup::ItemTypeLookup::new());
        let library: Arc<dyn LibraryManager> =
            Arc::new(ferrofin_core::FerrofinLibraryManager::new(
                Arc::new(ferrofin_core::FerrofinItemRepository::new(
                    db.clone(),
                    lookup,
                )),
                Arc::new(ferrofin_core::FerrofinItemCountService::new(db.clone())),
                Arc::new(ferrofin_core::FerrofinItemPersistenceService::new(
                    db.clone(),
                )),
                Arc::new(ferrofin_core::FerrofinPeopleRepository::new(db.clone())),
            ));
        ExtensionContext {
            media_segments: Arc::new(ferrofin_core::FerrofinMediaSegmentManager::new(
                db.clone(),
                Arc::clone(&library),
            )),
            library,
            plugins: Arc::new(DisabledPluginManager),
            fingerprinter: None,
            cache_dir: PathBuf::from("/tmp/ferrofin-extensions-test-cache"),
            merge_versions: Arc::new(NoMerges),
        }
    }

    #[test]
    fn registered_plugins_carry_each_extensions_seed_config_and_pages() {
        let extensions = builtin_extensions();
        assert_eq!(extensions.len(), 3);
        let plugins = registered_plugins(&extensions);
        assert_eq!(plugins.len(), extensions.len());
        for (plugin, ext) in plugins.iter().zip(&extensions) {
            assert_eq!(plugin.descriptor.id, ext.id());
            assert!(
                serde_json::from_slice::<serde_json::Value>(&plugin.default_config)
                    .expect("seed config is JSON")
                    .is_object(),
                "{} must seed an object config",
                plugin.descriptor.name
            );
            assert_eq!(plugin.config_pages.len(), ext.config_pages().len());
        }
    }

    #[test]
    fn extension_ids_are_unique() {
        let ids: std::collections::HashSet<Uuid> =
            builtin_extensions().iter().map(|e| e.id()).collect();
        assert_eq!(ids.len(), 3, "plugin ids double as /Plugins keys");
    }

    #[tokio::test]
    async fn register_tasks_registers_every_extensions_tasks() {
        let cx = context().await;
        let extensions = builtin_extensions();
        let expected: Vec<String> = extensions
            .iter()
            .flat_map(|e| e.tasks(&cx))
            .map(|t| t.key().to_owned())
            .collect();
        assert!(!expected.is_empty());

        let tasks = FerrofinTaskManager::new();
        register_tasks(&extensions, &cx, &tasks);
        let registered: std::collections::HashSet<String> = tasks
            .get_tasks()
            .await
            .expect("tasks")
            .into_iter()
            .filter_map(|t| t.key)
            .collect();
        for key in expected {
            assert!(registered.contains(&key), "{key} was not registered");
        }
    }
}
