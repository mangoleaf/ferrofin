//! Merge Versions manager trait — the DI seam for the `/MergeVersions/*`
//! routes and the Merge Versions extension's scheduled tasks.
//!
//! Port of the public surface of the `MergeVersions` plugin's
//! `MergeVersionsManager` (`MergeMovies` / `SplitMovies` / `MergeEpisodesAsync`
//! / `SplitEpisodesAsync`). The concrete implementation lives with the
//! extension (`hermit-extensions::merge_versions`) so every piece of the
//! plugin is grouped there; `hermit-api` handlers and the extension's tasks
//! reach it only through this object-safe trait.

use async_trait::async_trait;

use crate::error::ServiceError;

/// A progress sink for the bulk library scans: called with a completion
/// percentage in `0.0..=100.0` (the C# `IProgress<double>.Report`).
pub type MergeProgress<'a> = &'a (dyn Fn(f64) + Send + Sync);

/// Bulk merge/split of duplicate video versions across the whole library.
///
/// Every method self-gates on the Merge Versions plugin's enabled flag,
/// reporting [`ServiceError::NotFound`] while it is disabled — the observable
/// behavior of a Jellyfin server whose disabled plugin's controller is not
/// registered.
#[async_trait]
pub trait MergeVersionsManager: Send + Sync {
    /// Scans every movie and merges duplicate versions into one group.
    ///
    /// Port of `MergeVersionsManager.MergeMovies`: groups the eligible
    /// non-virtual movies by their `Tmdb` provider id and merges each group of
    /// two or more in which at least one member is not already an alternate.
    /// Movies without a `Tmdb` id, in an excluded location (plugin config), or
    /// outside every virtual-folder location are skipped.
    async fn merge_movies(&self, progress: Option<MergeProgress<'_>>) -> Result<(), ServiceError>;

    /// Scans every movie and splits any merged version groups apart.
    ///
    /// Port of `MergeVersionsManager.SplitMovies`: clears the version-group
    /// link for every eligible `Tmdb`-carrying movie (idempotent for movies
    /// that are not part of a group).
    async fn split_movies(&self, progress: Option<MergeProgress<'_>>) -> Result<(), ServiceError>;

    /// Scans every episode and merges duplicate versions into one group.
    ///
    /// Port of `MergeVersionsManager.MergeEpisodesAsync`: groups the eligible
    /// non-virtual episodes by the upstream 12.0 merge key (provider id first —
    /// `Tvdb`/`Tmdb`/`Imdb` — then season/episode numbers, then title fields,
    /// compared case-insensitively) and merges each group of two or more.
    async fn merge_episodes(&self, progress: Option<MergeProgress<'_>>)
    -> Result<(), ServiceError>;

    /// Scans every episode and splits any merged version groups apart.
    ///
    /// Port of `MergeVersionsManager.SplitEpisodesAsync`: clears the
    /// version-group link for every eligible non-virtual episode.
    async fn split_episodes(&self, progress: Option<MergeProgress<'_>>)
    -> Result<(), ServiceError>;
}

fn _assert_object_safe_merge_versions_manager(_: &dyn MergeVersionsManager) {}
