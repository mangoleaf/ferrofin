//! Scheduled-task manager trait — the DI seam for `ScheduledTasksController`.
//!
//! Port of the *read/run* slice of `MediaBrowser.Model.Tasks.ITaskManager`
//! (plus the `ScheduledTaskHelpers.GetTaskInfo` projection the controller calls
//! per task). A handler holds an `Arc<dyn TaskManager>` in
//! [`AppState`](../../ferrofin_api/state) and never names the concrete
//! `ferrofin-core` registry.
//!
//! The trait covers the whole controller: enumerate tasks (filtered as
//! `GetTasks` filters), fetch one, run one, cancel a running one
//! (`ITaskManager.Cancel` → `DELETE /ScheduledTasks/Running/{taskId}`) and
//! replace a task's triggers (`IConfigurableScheduledTask.Triggers = …` →
//! `POST /ScheduledTasks/{taskId}/Triggers`). Both of those are implemented, not
//! stubs, and the `ITaskTrigger` timers behind them run in the `ferrofin-core`
//! registry's own scheduler.
//!
//! Every `taskId` on this trait is the wire id — `ScheduledTaskWorker.Id`,
//! reproduced by [`task_id_for_key`] — not the task key, matching the C#
//! controller's `FirstOrDefault(i => string.Equals(i.Id, taskId, …))` lookup.
//!
//! Reuses the `ferrofin-model` wire DTO [`TaskInfo`] verbatim.

use async_trait::async_trait;
use ferrofin_model::tasks::{TaskInfo, TaskTriggerInfo};

use crate::error::ServiceError;

/// Enumerates the server's scheduled tasks and runs one on demand.
///
/// Port of `ITaskManager` (read + manual-run slice). Object-safe so it can be
/// held as `Arc<dyn TaskManager>`.
#[async_trait]
pub trait TaskManager: Send + Sync {
    /// Lists every registered task as a wire [`TaskInfo`].
    ///
    /// The unfiltered listing, in the C# wire order
    /// (`_taskManager.ScheduledTasks.OrderBy(o => o.Name)`). The query filters
    /// live on [`get_tasks_filtered`](TaskManager::get_tasks_filtered).
    async fn get_tasks(&self) -> Result<Vec<TaskInfo>, ServiceError>;

    /// Lists the tasks the `isHidden`/`isEnabled` query filters keep.
    ///
    /// Ports the loop in `ScheduledTasksController.GetTasks`, where the two
    /// filters are applied **only** inside
    /// `if (task.ScheduledTask is IConfigurableScheduledTask scheduledTask)` — a
    /// task that does not implement the interface is yielded whatever the caller
    /// asked for. Configurability is a property of the concrete task, so it is
    /// resolved here rather than by the handler.
    ///
    /// The default implementation is the answer for a registry whose tasks are
    /// all non-configurable: both filters are skipped and every task is listed.
    ///
    /// # Errors
    ///
    /// Whatever [`get_tasks`](TaskManager::get_tasks) returns.
    async fn get_tasks_filtered(
        &self,
        _is_hidden: Option<bool>,
        _is_enabled: Option<bool>,
    ) -> Result<Vec<TaskInfo>, ServiceError> {
        self.get_tasks().await
    }

    /// Fetches a single task by its id (the C# `IScheduledTaskWorker.Id`), or
    /// `None` when no task has that id.
    async fn get_task(&self, task_id: &str) -> Result<Option<TaskInfo>, ServiceError>;

    /// Queues the named task to run now, recording the outcome when it ends.
    ///
    /// Ports `ScheduledTasksController.StartTask` → `ITaskManager.Execute`: the
    /// call returns as soon as the task is `Running`, and the dashboard tracks
    /// it from there.
    ///
    /// # Errors
    ///
    /// [`ServiceError::NotFound`] when no task has that id;
    /// [`ServiceError::InvalidInput`] when it is already running; or whatever
    /// error the task body itself returns.
    async fn start_task(&self, task_id: &str) -> Result<(), ServiceError>;

    /// Cancels a running task.
    ///
    /// Ports `ScheduledTasksController.StopTask` → `ITaskManager.Cancel`: a
    /// running task is aborted and recorded `Cancelled`; cancelling an idle task
    /// is a no-op. A missing task is [`ServiceError::NotFound`], otherwise it
    /// succeeds.
    ///
    /// # Errors
    ///
    /// [`ServiceError::NotFound`] when no task has that id.
    async fn cancel_task(&self, task_id: &str) -> Result<(), ServiceError>;

    /// Replaces a task's configured triggers.
    ///
    /// Ports `ScheduledTasksController.UpdateTask` → `task.Triggers = triggerInfos`.
    /// The stored triggers are surfaced in the task's [`TaskInfo`], persisted, and
    /// armed on the registry's scheduler, exactly as the C# assignment reloads
    /// the configurable task's trigger events.
    ///
    /// # Errors
    ///
    /// [`ServiceError::NotFound`] when no task has that id.
    async fn update_triggers(
        &self,
        task_id: &str,
        triggers: &[TaskTriggerInfo],
    ) -> Result<(), ServiceError>;
}

fn _assert_object_safe_task_manager(_: &dyn TaskManager) {}

// ---------------------------------------------------------------------------
// Task id derivation (`ScheduledTaskWorker.Id`)
// ---------------------------------------------------------------------------

/// The C# type `FullName` behind each task key Ferrofin registers.
///
/// This table is the input to [`task_id_for_key`], which reproduces
/// `ScheduledTaskWorker.Id` — `ScheduledTask.GetType().FullName.GetMD5()`
/// (`Emby.Server.Implementations/ScheduledTasks/ScheduledTaskWorker.cs:219` at
/// `v10.11.8`). Ferrofin has no .NET type to reflect over, so the type name a
/// task *would* have upstream is recorded here instead; the id is otherwise
/// derived exactly as upstream derives it, which is what makes a task id stable
/// across the two servers.
///
/// The first twenty rows are Jellyfin 10.11.8's own task classes (namespace +
/// class name read from the `v10.11.8` tag). The last three are the two
/// third-party plugins Ferrofin ports as Tier-1a extensions, at the upstream
/// commits pinned in `docs/PLUGINS_UPSTREAM.md`, so a dashboard link made
/// against a Jellyfin install carrying those plugins keeps working.
///
/// A key that is not listed (a WASM plugin's task, a test task) has no upstream
/// type at all; [`task_id_for_key`] hashes a `Ferrofin.ScheduledTasks.`-prefixed
/// name for it, so its id has the same 32-hex shape and is equally stable,
/// without pretending to be an upstream id.
const CSHARP_TASK_TYPE_NAMES: &[(&str, &str)] = &[
    // Emby.Server.Implementations/ScheduledTasks/Tasks/*.cs
    (
        "AudioNormalization",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.AudioNormalizationTask",
    ),
    (
        "CleanActivityLog",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.CleanActivityLogTask",
    ),
    (
        "DeleteCacheFiles",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.DeleteCacheFileTask",
    ),
    (
        "CleanLogFiles",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.DeleteLogFileTask",
    ),
    (
        "DeleteTranscodeFiles",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.DeleteTranscodeFileTask",
    ),
    (
        "CleanCollectionsAndPlaylists",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.CleanupCollectionAndPlaylistPathsTask",
    ),
    (
        "RefreshChapterImages",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.ChapterImagesTask",
    ),
    (
        "TaskExtractMediaSegments",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.MediaSegmentExtractionTask",
    ),
    (
        "OptimizeDatabaseTask",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.OptimizeDatabaseTask",
    ),
    (
        "RefreshPeople",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.PeopleValidationTask",
    ),
    (
        "RefreshLibrary",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.RefreshMediaLibraryTask",
    ),
    (
        "PluginUpdates",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.PluginUpdateTask",
    ),
    (
        "CleanupUserDataTask",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.CleanupUserDataTask",
    ),
    // MediaBrowser.Providers/**
    (
        "DownloadLyrics",
        "MediaBrowser.Providers.Lyric.LyricScheduledTask",
    ),
    (
        "DownloadSubtitles",
        "MediaBrowser.Providers.MediaInfo.SubtitleScheduledTask",
    ),
    (
        "RefreshTrickplayImages",
        "MediaBrowser.Providers.Trickplay.TrickplayImagesTask",
    ),
    (
        "MoveTrickplayImages",
        "MediaBrowser.Providers.Trickplay.TrickplayMoveImagesTask",
    ),
    // src/Jellyfin.*/**
    (
        "KeyframeExtraction",
        "Jellyfin.MediaEncoding.Hls.ScheduledTasks.KeyframeExtractionScheduledTask",
    ),
    (
        "RefreshGuide",
        "Jellyfin.LiveTv.Guide.RefreshGuideScheduledTask",
    ),
    (
        "RefreshInternetChannels",
        "Jellyfin.LiveTv.Channels.RefreshChannelsScheduledTask",
    ),
    // Tier-1a extensions: the upstream plugins' own task classes.
    // intro-skipper @ db09359 — IntroSkipper/ScheduledTasks/DetectSegmentsTask.cs.
    // Ferrofin registers it under its own key, so the mapping is by key here.
    (
        "IntroSkipper.Detect",
        "IntroSkipper.ScheduledTasks.DetectSegmentsTask",
    ),
    // jellyfin-plugin-mergeversions @ e6f58d6 —
    // Jellyfin.Plugin.MergeVersions/ScheduledTasks/RefreshLibraryTask.cs.
    (
        "MergeMoviesTask",
        "Jellyfin.Plugin.MergeVersions.ScheduledTasks.MergeMoviesTask",
    ),
    (
        "MergeEpisodesTask",
        "Jellyfin.Plugin.MergeVersions.ScheduledTasks.MergeEpisodesTask",
    ),
];

/// The wire id of the task registered under `key` — the port of
/// `IScheduledTaskWorker.Id`.
///
/// Upstream is `ScheduledTask.GetType().FullName.GetMD5().ToString("N")`:
/// MD5 over the UTF-16LE bytes of the .NET type name, wrapped in a `Guid` and
/// rendered as 32 lowercase hex digits. It is a *portable* value — two Jellyfin
/// installs agree on it — so it is what a stored dashboard link, a bookmark and
/// `GET|POST|DELETE /ScheduledTasks/{taskId}` all address. Emitting the task key
/// there instead made Ferrofin's ids mutually incompatible with Jellyfin's.
///
/// `TaskInfo.Key` still carries the key, exactly as
/// `ScheduledTaskHelpers.GetTaskInfo` sets both fields.
#[must_use]
pub fn task_id_for_key(key: &str) -> String {
    let type_name = CSHARP_TASK_TYPE_NAMES
        .iter()
        .find(|(k, _)| *k == key)
        .map_or_else(
            || format!("Ferrofin.ScheduledTasks.{key}"),
            |(_, name)| (*name).to_owned(),
        );
    ferrofin_common::extensions::get_md5(&type_name)
        .simple()
        .to_string()
}

#[cfg(test)]
mod task_id_tests {
    use super::{CSHARP_TASK_TYPE_NAMES, task_id_for_key};

    /// The ids a real Jellyfin 10.11.8 server returns from `GET /ScheduledTasks`
    /// for its twenty built-in tasks, captured from the parity lab pair on
    /// 2026-08-30. This is the oracle: the derivation is only correct if it
    /// reproduces these byte for byte.
    const JELLYFIN_10_11_8_TASK_IDS: &[(&str, &str)] = &[
        ("AudioNormalization", "ec2f221fd8e7706b3d3afd2c4591b4d7"),
        ("CleanActivityLog", "b461ef918ab28520928183e794350e3c"),
        ("DeleteCacheFiles", "241d4fcb19a1d557ee62428e411da609"),
        ("CleanLogFiles", "1c8ede62c521bea0bf851344f5b8ca40"),
        ("DeleteTranscodeFiles", "7d8088c10902f1bf4072ded42437bcfb"),
        (
            "CleanCollectionsAndPlaylists",
            "3a025083141d3c17dd96d5f9b951287b",
        ),
        ("DownloadLyrics", "26649fe0aad57557245351f220da916c"),
        ("DownloadSubtitles", "2c66a88bca43e565d7f8099f825478f1"),
        ("RefreshChapterImages", "4e6637c832ed644d1af3370a2506e80a"),
        ("RefreshTrickplayImages", "64f5f44cd30dc273cb9890205473bbcc"),
        ("KeyframeExtraction", "f302d80f31bcacf76f979d277d448581"),
        (
            "TaskExtractMediaSegments",
            "f861734dd71b37f9482b52a820e39013",
        ),
        ("MoveTrickplayImages", "ebb6e58c4e9a8f5d1f77ab6ddefb3143"),
        ("OptimizeDatabaseTask", "31de9ce83b9223d338c77b1a635e144b"),
        ("RefreshGuide", "bea9b218c97bbf98c5dc1303bdb9a0ca"),
        ("RefreshPeople", "866456ed0d44e15468124ce33d85961e"),
        ("RefreshLibrary", "7738148ffcd07979c7ceb148e06b3aed"),
        (
            "RefreshInternetChannels",
            "0c9ee3a88fc15547c6852205480da1fd",
        ),
        ("PluginUpdates", "f9b057c054e9e6daee4a88ffd146a403"),
        ("CleanupUserDataTask", "8240e309407dbbf43c99d30a6cbd239e"),
    ];

    #[test]
    fn every_jellyfin_task_id_is_reproduced_exactly() {
        for (key, expected) in JELLYFIN_10_11_8_TASK_IDS {
            assert_eq!(&task_id_for_key(key), expected, "task id for {key}");
        }
    }

    #[test]
    fn an_id_is_32_lowercase_hex_digits() {
        // C# `Guid.ToString("N")`: no dashes, no braces, lowercase.
        for (key, _) in CSHARP_TASK_TYPE_NAMES {
            let id = task_id_for_key(key);
            assert_eq!(id.len(), 32, "{key} -> {id}");
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{key} -> {id}"
            );
        }
    }

    #[test]
    fn an_unmapped_key_gets_a_stable_ferrofin_namespaced_id() {
        // A WASM plugin's task has no upstream .NET type; it still gets a
        // guid-shaped id, and the same one every boot.
        let id = task_id_for_key("wasm:acme.hello:sweep");
        assert_eq!(id.len(), 32);
        assert_eq!(id, task_id_for_key("wasm:acme.hello:sweep"));
        assert_ne!(id, task_id_for_key("wasm:acme.hello:other"));
        // And it is NOT the key itself — the bug this derivation replaced.
        assert_ne!(id, "wasm:acme.hello:sweep");
    }

    #[test]
    fn the_type_name_table_has_no_duplicate_keys() {
        let mut keys: Vec<&str> = CSHARP_TASK_TYPE_NAMES.iter().map(|(k, _)| *k).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), before);
    }
}
