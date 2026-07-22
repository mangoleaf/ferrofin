//! Minimal lyrics manager trait (deferred subsystem).
//!
//! Port of a representative slice of
//! `MediaBrowser.Controller.Lyrics.ILyricManager`. Lyrics are deferred, so the
//! `ILyricProvider` per-backend strategy interface is **not** ported. The
//! domain `Audio` receiver becomes an item [`uuid::Uuid`].
//!
//! Port rules applied: lyric payloads reuse the [`LyricDto`] /
//! [`RemoteLyricInfoDto`] wire DTOs and the [`LyricProviderInfo`] descriptor;
//! `Task<T>` → `async fn -> Result<T, ServiceError>`; `CancellationToken` is
//! dropped for v1.

use async_trait::async_trait;
use hermit_model::lyrics::{LyricDto, RemoteLyricInfoDto};
use hermit_model::providers::LyricProviderInfo;
use uuid::Uuid;

use crate::error::ServiceError;

/// The (deferred) lyrics manager.
///
/// Port of `ILyricManager` (minimal slice). The overloaded `SearchLyricsAsync` /
/// `DownloadLyricsAsync` / `SaveLyricAsync` methods each collapse to one form.
#[async_trait]
pub trait LyricManager: Send + Sync {
    /// Gets the lyrics already stored for an audio item, if any.
    async fn get_lyrics(&self, item_id: Uuid) -> Result<Option<LyricDto>, ServiceError>;

    /// Searches remote providers for lyrics matching an audio item.
    async fn search_lyrics(&self, item_id: Uuid) -> Result<Vec<RemoteLyricInfoDto>, ServiceError>;

    /// Downloads a remote lyric by id and attaches it to the audio item.
    async fn download_lyrics(
        &self,
        item_id: Uuid,
        lyric_id: &str,
    ) -> Result<Option<LyricDto>, ServiceError>;

    /// Saves caller-supplied lyric text for an audio item.
    async fn save_lyric(
        &self,
        item_id: Uuid,
        format: &str,
        lyrics: &str,
    ) -> Result<Option<LyricDto>, ServiceError>;

    /// Deletes the lyrics stored for an audio item.
    async fn delete_lyrics(&self, item_id: Uuid) -> Result<(), ServiceError>;

    /// Lists the lyric providers that support the item.
    async fn get_supported_providers(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<LyricProviderInfo>, ServiceError>;
}

fn _assert_object_safe_lyric_manager(_: &dyn LyricManager) {}
