//! Subtitle-layer manager trait — search, download, upload, delete subtitles.
//!
//! Port of `MediaBrowser.Controller.Subtitles.ISubtitleManager`.
//!
//! Port rules applied:
//! - The C# `Video` / `BaseItem` receivers become [`uuid::Uuid`] identity
//!   arguments; a downloaded/uploaded subtitle references the item by id.
//! - Overloaded `SearchSubtitles` (a `Video` form and a request form) collapse
//!   to one method taking a [`SubtitleSearchRequest`]; overloaded
//!   `DownloadSubtitles` collapse to one `download_subtitles`.
//! - The `.NET` `SubtitleDownloadFailure` event is dropped (event wiring lives
//!   in `hermit-core`).
//! - Remote subtitle candidates reuse the [`RemoteSubtitleInfo`] wire DTO;
//!   provider descriptors reuse [`SubtitleProviderInfo`]. The raw subtitle
//!   payload returned by `GetRemoteSubtitles` surfaces as a [`SubtitleResponse`]
//!   value type defined here (the C# `SubtitleResponse` lives under
//!   `MediaBrowser.Model.Providers` but is a service-layer stream envelope).
//! - `Task<T>` → `async fn -> Result<T, ServiceError>`; `CancellationToken` is
//!   dropped for v1.
//!
//! The trait is object-safe and carries a `_assert_object_safe_*` assertion.

use async_trait::async_trait;
use hermit_model::providers::{RemoteSubtitleInfo, SubtitleProviderInfo};
use uuid::Uuid;

use crate::error::ServiceError;

/// The kind of media a subtitle search targets (C# `VideoContentType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubtitleMediaType {
    /// A movie (or any non-episodic video).
    #[default]
    Movie,
    /// A TV episode (uses `series_name` + season/episode numbers).
    Episode,
}

/// A request to search remote providers for subtitles matching an item.
///
/// Port of Jellyfin's `SubtitleSearchRequest`. The [`SubtitleManager`] enriches
/// it from the resolved item (name/year/imdb/season/episode/media path) before
/// fanning it out to the providers, which need that metadata to query — the thin
/// `(item_id, language)` form the API layer builds is filled in by the manager.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubtitleSearchRequest {
    /// The item the subtitles are being searched for.
    pub item_id: Uuid,
    /// The desired subtitle language (a three-letter ISO code).
    pub language: String,
    /// When set, require a perfect match; when `None`, accept any match.
    pub is_perfect_match: Option<bool>,
    /// Whether the request was triggered automatically (vs. user-initiated).
    pub is_automated: bool,
    /// The kind of media (movie vs. episode).
    pub content_type: SubtitleMediaType,
    /// The item's display name / title.
    pub name: Option<String>,
    /// The series name, for episodes.
    pub series_name: Option<String>,
    /// The production year.
    pub production_year: Option<i32>,
    /// The season number, for episodes.
    pub parent_index_number: Option<i32>,
    /// The episode number, for episodes.
    pub index_number: Option<i32>,
    /// The item's runtime, in ticks (helps rank matches).
    pub runtime_ticks: Option<i64>,
    /// The on-disk media path (enables hash-based matching).
    pub media_path: Option<String>,
    /// The IMDb id (e.g. `tt1234567`), when known.
    pub imdb_id: Option<String>,
}

/// The raw content of a downloaded subtitle.
///
/// Port of `SubtitleResponse` — the C# stream-plus-metadata envelope returned by
/// `GetRemoteSubtitles`. The C# `Stream` becomes owned bytes so the value stays
/// `Send`-safe across the trait boundary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubtitleResponse {
    /// The subtitle language (a three-letter ISO code).
    pub language: String,
    /// The subtitle format (e.g. `srt`, `ass`).
    pub format: String,
    /// Whether the subtitle is forced.
    pub is_forced: bool,
    /// Whether the subtitle is hearing-impaired (SDH).
    pub is_hearing_impaired: bool,
    /// The raw subtitle file bytes.
    pub content: Vec<u8>,
}

/// Searches, downloads, uploads and deletes subtitles for library items.
///
/// Port of `ISubtitleManager`.
#[async_trait]
pub trait SubtitleManager: Send + Sync {
    /// Searches remote providers for subtitles matching the request.
    async fn search_subtitles(
        &self,
        request: &SubtitleSearchRequest,
    ) -> Result<Vec<RemoteSubtitleInfo>, ServiceError>;

    /// Downloads a remote subtitle and attaches it to the item.
    async fn download_subtitles(
        &self,
        item_id: Uuid,
        subtitle_id: &str,
    ) -> Result<(), ServiceError>;

    /// Uploads a caller-supplied subtitle for the item.
    async fn upload_subtitle(
        &self,
        item_id: Uuid,
        response: &SubtitleResponse,
    ) -> Result<(), ServiceError>;

    /// Fetches the raw content of a remote subtitle by id.
    async fn get_remote_subtitles(&self, id: &str) -> Result<SubtitleResponse, ServiceError>;

    /// Deletes the subtitle stream at `index` from the item.
    async fn delete_subtitles(&self, item_id: Uuid, index: i32) -> Result<(), ServiceError>;

    /// Lists the subtitle providers that support the item.
    async fn get_supported_providers(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<SubtitleProviderInfo>, ServiceError>;
}

fn _assert_object_safe_subtitle_manager(_: &dyn SubtitleManager) {}

/// A single subtitle provider — the `ISubtitleProvider` strategy (OpenSubtitles,
/// et al.).
///
/// The [`SubtitleManager`] owns a registry of these: it fans
/// [`search_subtitles`](SubtitleManager::search_subtitles) out across every
/// provider, and routes [`download_subtitles`](SubtitleManager::download_subtitles)
/// / [`get_remote_subtitles`](SubtitleManager::get_remote_subtitles) back to the
/// owning provider. Following Jellyfin, a [`RemoteSubtitleInfo::id`] is namespaced
/// as `"{provider_name}_{provider_local_id}"`, so the manager selects the provider
/// by the id's prefix and hands it the remainder via [`get_subtitles`](SubtitleProvider::get_subtitles).
///
/// [`RemoteSubtitleInfo::id`]: hermit_model::providers::RemoteSubtitleInfo
#[async_trait]
pub trait SubtitleProvider: Send + Sync {
    /// The provider's stable display name; also the id namespace prefix.
    fn name(&self) -> &str;

    /// Searches this provider for subtitles matching `request`. Each returned
    /// [`RemoteSubtitleInfo::id`](hermit_model::providers::RemoteSubtitleInfo) must
    /// be namespaced with `name()`.
    async fn search(
        &self,
        request: &SubtitleSearchRequest,
    ) -> Result<Vec<RemoteSubtitleInfo>, ServiceError>;

    /// Fetches the raw subtitle content for `provider_local_id` (the id with the
    /// `"{name}_"` prefix already stripped by the manager).
    async fn get_subtitles(
        &self,
        provider_local_id: &str,
    ) -> Result<SubtitleResponse, ServiceError>;
}

fn _assert_object_safe_subtitle_provider(_: &dyn SubtitleProvider) {}

#[cfg(test)]
mod tests {
    use super::{SubtitleResponse, SubtitleSearchRequest};

    #[test]
    fn search_request_default_is_empty() {
        let r = SubtitleSearchRequest::default();
        assert!(r.language.is_empty());
        assert!(r.is_perfect_match.is_none());
        assert!(!r.is_automated);
    }

    #[test]
    fn response_default_is_empty() {
        let r = SubtitleResponse::default();
        assert!(r.content.is_empty());
        assert!(!r.is_forced);
        assert!(!r.is_hearing_impaired);
    }
}
