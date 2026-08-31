//! The Application-category scheduled tasks.
//!
//! Faithful ports of the upstream Application `IScheduledTask`s (names, keys,
//! categories, descriptions and default triggers match
//! `Emby.Server.Implementations/ScheduledTasks/Tasks/*` and the en-US
//! localization strings):
//!
//! - [`PluginUpdateTask`] — `PluginUpdateTask` (`PluginUpdates`)
//!
//! The C# `IProgress<double>` maps to [`TaskProgress`]; `CancellationToken`s are
//! dropped (a queued run is cancelled by aborting its tokio task).

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_model::tasks::{TaskTriggerInfo, TaskTriggerInfoType};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::plugins::PluginManager;

use super::{ScheduledTask, TaskProgress, interval_hours};

/// The upstream Application category display string (`TasksApplicationCategory`).
const APPLICATION: &str = "Application";

/// The share of the run's progress spent fetching the catalog, before any
/// install starts (upstream reports 10 after the package fetch and spreads the
/// remaining 90 across the installs).
const CATALOG_FETCH_PERCENT: f64 = 10.0;

/// "Update Plugins" — installs available updates for the installed plugins.
/// Port of `PluginUpdateTask`.
///
/// Ferrofin's updatable plugins are the Tier-1b WASM plugins staged under
/// `{data_dir}/plugins` (compiled-in extensions ship with the server and are
/// refused by the installer). The run asks the plugin manager which installed
/// plugins have a newer compatible version in the configured repositories
/// (`PluginManager::available_plugin_updates`, the port of upstream's
/// `IInstallationManager.GetAvailablePluginUpdates`) and installs each through
/// the same download → checksum → validate → stage path as
/// `POST /Packages/Installed/{name}`; like upstream, a failed install is logged
/// and the run continues with the next package.
///
/// Accepted divergence: upstream honours a per-plugin `AutoUpdate` flag from
/// the installed plugin's own manifest, which Ferrofin's WASM artifacts do not
/// carry. Here the opt-out is disabling the plugin (or the repository), which
/// this task then skips — see `docs/EXTENSIONS.md`.
pub struct PluginUpdateTask {
    plugins: Arc<dyn PluginManager>,
}

impl PluginUpdateTask {
    /// Builds the task over the plugin-manager seam.
    #[must_use]
    pub fn new(plugins: Arc<dyn PluginManager>) -> Self {
        Self { plugins }
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for PluginUpdateTask {
    fn key(&self) -> &str {
        "PluginUpdates"
    }
    /// C# `PluginUpdateTask` implements `IConfigurableScheduledTask`, so the
    /// `GET /ScheduledTasks` `isHidden`/`isEnabled` filters apply to it.
    fn is_configurable(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "Update Plugins"
    }
    fn description(&self) -> &str {
        "Downloads and installs updates for plugins that are configured to update automatically."
    }
    fn category(&self) -> &str {
        APPLICATION
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        vec![
            TaskTriggerInfo {
                type_: TaskTriggerInfoType::StartupTrigger,
                ..TaskTriggerInfo::default()
            },
            interval_hours(24),
        ]
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        progress.report(0.0);
        let updates = self.plugins.available_plugin_updates().await?;
        progress.report(CATALOG_FETCH_PERCENT);
        let total = updates.len();
        let mut failed = 0usize;
        for (index, update) in updates.iter().enumerate() {
            match self
                .plugins
                .install_package(
                    &update.name,
                    Some(update.id),
                    Some(&update.version),
                    update.repository_url.as_deref(),
                )
                .await
            {
                Ok(()) => tracing::info!(
                    plugin = %update.id,
                    package = update.name,
                    from = update.installed_version,
                    to = update.version,
                    "plugin update installed; restart required to activate"
                ),
                // Upstream swallows download/IO/data errors per package and
                // moves on, so one bad repository cannot block the rest.
                Err(e) => {
                    failed += 1;
                    tracing::error!(
                        plugin = %update.id,
                        package = update.name,
                        version = update.version,
                        error = %e,
                        "plugin update failed"
                    );
                }
            }
            #[allow(clippy::cast_precision_loss)]
            let done = (index + 1) as f64;
            #[allow(clippy::cast_precision_loss)]
            let scale = (100.0 - CATALOG_FETCH_PERCENT) * done / total as f64;
            progress.report(scale + CATALOG_FETCH_PERCENT);
        }
        // The run itself succeeds whatever the packages did (upstream's
        // per-package catch), so a persistently broken repository would be
        // invisible without this one aggregate line.
        if failed > 0 {
            tracing::warn!(failed, total, "some plugin updates failed");
        }
        progress.report(100.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ferrofin_model::updates::{PackageInfo, RepositoryInfo};
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::plugins::{
        PluginDescriptor, PluginImage, PluginManager, PluginUpdateInfo,
    };
    use uuid::Uuid;

    use super::{PluginUpdateTask, ScheduledTask, TaskProgress};

    /// One recorded `install_package` call.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct InstallCall {
        name: String,
        guid: Option<Uuid>,
        version: Option<String>,
        repository: Option<String>,
    }

    /// A plugin manager that offers a fixed update list and records installs.
    struct FakePlugins {
        updates: Vec<PluginUpdateInfo>,
        installed: Mutex<Vec<InstallCall>>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl PluginManager for FakePlugins {
        async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_plugin(&self, _id: Uuid) -> Result<Option<PluginDescriptor>, ServiceError> {
            Ok(None)
        }
        async fn enable_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn disable_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn remove_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_plugin_configuration(&self, _id: Uuid) -> Result<Vec<u8>, ServiceError> {
            Ok(Vec::new())
        }
        async fn set_plugin_configuration(
            &self,
            _id: Uuid,
            _config: Vec<u8>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn plugin_image(&self, _id: Uuid) -> Result<Option<PluginImage>, ServiceError> {
            Ok(None)
        }
        async fn get_repositories(&self) -> Result<Vec<RepositoryInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn set_repositories(
            &self,
            _repositories: Vec<RepositoryInfo>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn list_packages(&self) -> Result<Vec<PackageInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn available_plugin_updates(&self) -> Result<Vec<PluginUpdateInfo>, ServiceError> {
            Ok(self.updates.clone())
        }
        async fn install_package(
            &self,
            name: &str,
            assembly_guid: Option<Uuid>,
            version: Option<&str>,
            repository_url: Option<&str>,
        ) -> Result<(), ServiceError> {
            self.installed.lock().expect("lock").push(InstallCall {
                name: name.to_owned(),
                guid: assembly_guid,
                version: version.map(ToOwned::to_owned),
                repository: repository_url.map(ToOwned::to_owned),
            });
            if self.fail {
                Err(ServiceError::backend("boom"))
            } else {
                Ok(())
            }
        }
    }

    fn task(updates: Vec<PluginUpdateInfo>, fail: bool) -> (PluginUpdateTask, Arc<FakePlugins>) {
        let plugins = Arc::new(FakePlugins {
            updates,
            installed: Mutex::new(Vec::new()),
            fail,
        });
        (PluginUpdateTask::new(plugins.clone()), plugins)
    }

    fn update(name: &str, id: Uuid, to: &str) -> PluginUpdateInfo {
        PluginUpdateInfo {
            id,
            name: name.to_owned(),
            installed_version: "1.0.0".to_owned(),
            version: to.to_owned(),
            repository_url: Some("https://repo.example/manifest.json".to_owned()),
        }
    }

    #[test]
    fn metadata_matches_upstream() {
        let (task, _) = task(Vec::new(), false);
        assert_eq!(task.key(), "PluginUpdates");
        assert_eq!(task.name(), "Update Plugins");
        assert_eq!(task.category(), "Application");
        assert_eq!(
            task.description(),
            "Downloads and installs updates for plugins that are configured to update \
             automatically."
        );
        assert!(!task.is_hidden());
        let triggers = task.default_triggers();
        assert_eq!(triggers.len(), 2);
        assert_eq!(
            triggers[0].type_,
            ferrofin_model::tasks::TaskTriggerInfoType::StartupTrigger
        );
        assert_eq!(
            triggers[1].type_,
            ferrofin_model::tasks::TaskTriggerInfoType::IntervalTrigger
        );
        // 24 hours, matching the oracle's `IntervalTicks`.
        assert_eq!(triggers[1].interval_ticks, Some(864_000_000_000));
    }

    #[tokio::test]
    async fn installs_every_available_update_and_finishes_at_100() {
        let id = Uuid::new_v4();
        let (task, plugins) = task(vec![update("Example", id, "1.1.0")], false);
        let progress = TaskProgress::default();
        task.execute(&progress).await.expect("run");
        assert!((progress.current() - 100.0).abs() < f64::EPSILON);
        let installed = plugins.installed.lock().expect("lock").clone();
        assert_eq!(
            installed,
            vec![InstallCall {
                name: "Example".to_owned(),
                guid: Some(id),
                version: Some("1.1.0".to_owned()),
                // The repository the update was chosen from is pinned, so the
                // install cannot resolve a different entry.
                repository: Some("https://repo.example/manifest.json".to_owned()),
            }]
        );
    }

    #[tokio::test]
    async fn nothing_to_update_is_a_clean_run() {
        let (task, plugins) = task(Vec::new(), false);
        let progress = TaskProgress::default();
        task.execute(&progress).await.expect("run");
        assert!(plugins.installed.lock().expect("lock").is_empty());
        assert!((progress.current() - 100.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn a_failed_install_does_not_fail_the_run() {
        let (task, plugins) = task(
            vec![
                update("One", Uuid::new_v4(), "2.0.0"),
                update("Two", Uuid::new_v4(), "2.0.0"),
            ],
            true,
        );
        let progress = TaskProgress::default();
        task.execute(&progress).await.expect("run survives");
        // Both were attempted — one bad package must not skip the rest.
        assert_eq!(plugins.installed.lock().expect("lock").len(), 2);
    }
}
