//! Lyrics manager + provider traits.
//!
//! Port of a representative slice of
//! `MediaBrowser.Controller.Lyrics.ILyricManager` plus the `ILyricProvider`
//! per-backend strategy interface (LrcLib, …). The domain `Audio` receiver
//! becomes an item [`uuid::Uuid`].
//!
//! Port rules applied: lyric payloads reuse the [`LyricDto`] /
//! [`RemoteLyricInfoDto`] wire DTOs and the [`LyricProviderInfo`] descriptor;
//! the provider search request reuses the [`LyricSearchRequest`] wire DTO; the
//! C# `LyricResponse` stream envelope becomes an owned-text [`LyricResponse`];
//! `Task<T>` → `async fn -> Result<T, ServiceError>`; `CancellationToken` is
//! dropped for v1.

use async_trait::async_trait;
use hermit_model::lyrics::{LyricDto, LyricMetadata, LyricSearchRequest, RemoteLyricInfoDto};
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

/// The raw content of a fetched remote lyric.
///
/// Port of `MediaBrowser.Controller.Lyrics.LyricResponse` — the C# stream
/// envelope becomes owned text so the value stays `Send`-safe across the trait
/// boundary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LyricResponse {
    /// The lyric format (`lrc` for synced, `txt` for plain) — also the sidecar
    /// file extension the manager saves with.
    pub format: String,
    /// The raw lyric text.
    pub text: String,
}

/// One remote lyric candidate returned by a provider search.
///
/// Port of `MediaBrowser.Controller.Lyrics.RemoteLyricInfo`. The `id` is
/// provider-local (e.g. LrcLib's `"{id}_synced"`); the [`LyricManager`]
/// namespaces it with the provider id before surfacing it to clients.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteLyricInfo {
    /// The provider-local id of this lyric.
    pub id: String,
    /// The display name of the provider that produced this candidate.
    pub provider_name: String,
    /// The candidate's metadata (artist/album/title/length/synced flag).
    pub metadata: LyricMetadata,
    /// The candidate's raw lyric content.
    pub lyrics: LyricResponse,
}

/// A single remote lyric provider — the `ILyricProvider` strategy (LrcLib, …).
///
/// The [`LyricManager`] owns a registry of these: it fans
/// [`search_lyrics`](LyricManager::search_lyrics) out across the providers and
/// routes [`download_lyrics`](LyricManager::download_lyrics) back to the owning
/// provider by the namespaced id prefix, handing it the provider-local
/// remainder via [`get_lyrics`](LyricProvider::get_lyrics).
#[async_trait]
pub trait LyricProvider: Send + Sync {
    /// The provider's stable display name (e.g. `LrcLib`).
    fn name(&self) -> &'static str;

    /// Searches this provider for lyrics matching `request` (song name /
    /// artists / album / duration). Ids in the results are provider-local.
    async fn search(
        &self,
        request: &LyricSearchRequest,
    ) -> Result<Vec<RemoteLyricInfo>, ServiceError>;

    /// Fetches the raw lyric content for `provider_local_id` (the id with the
    /// provider prefix already stripped by the manager), or `None` when the
    /// remote has no lyric for that id.
    async fn get_lyrics(
        &self,
        provider_local_id: &str,
    ) -> Result<Option<LyricResponse>, ServiceError>;
}

fn _assert_object_safe_lyric_provider(_: &dyn LyricProvider) {}
