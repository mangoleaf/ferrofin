//! Curated, compiled-in **extensions** for Hermit — the Rust answer to Jellyfin
//! plugins (Rust cannot load .NET assemblies at runtime; see
//! `brain/plans/PLAN_HERMIT_EXTENSIONS.md`).
//!
//! An [`Extension`] is a repo-curated capability that **surfaces as a plugin** on
//! the frozen `/Plugins` API (enable/disable toggle, config page) via the
//! existing [`PluginManager`](hermit_traits::plugins::PluginManager), and can
//! contribute background [`ScheduledTask`]s. The set is static —
//! [`builtin_extensions`] — but each is enable/disable-able at runtime; a task
//! self-gates on its plugin's enabled flag, so toggling needs no restart.
//!
//! The composition root drives this: it builds [`RegisteredPlugin`]s from the
//! descriptors ([`registered_plugins`]) and registers each extension's tasks
//! ([`register_tasks`]).

use std::path::PathBuf;
use std::sync::Arc;

use hermit_core::{HermitTaskManager, PluginConfigPage, RegisteredPlugin, ScheduledTask};
use hermit_traits::library::LibraryManager;
use hermit_traits::media_segments::MediaSegmentManager;
use hermit_traits::merge_versions::MergeVersionsManager;
use hermit_traits::plugins::{PluginDescriptor, PluginManager};
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
    task_manager: &HermitTaskManager,
) {
    for ext in extensions {
        for task in ext.tasks(cx) {
            task_manager.register(task);
        }
    }
}
