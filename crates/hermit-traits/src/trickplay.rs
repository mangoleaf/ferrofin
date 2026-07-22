//! Trickplay manager trait — scrubbing-preview tile generation and lookup.
//!
//! Port of `MediaBrowser.Controller.Trickplay.ITrickplayManager`.
//!
//! Port rules applied:
//! - The C# `Video` / `BaseItem` receivers become [`uuid::Uuid`] identity
//!   arguments; the `LibraryOptions` argument (impl-resolved) is dropped.
//! - Stored trickplay metadata is a persistence concern, so it surfaces as the
//!   [`TrickplayInfoEntity`](hermit_db::entities::playback::TrickplayInfoEntity)
//!   row (the C# `TrickplayInfo` EF entity). Maps keyed by width use `i32`
//!   keys; the manifest keys by media-source id.
//! - `IReadOnlyList`/`Dictionary` become `Vec`/`HashMap`.
//! - The image-tiling / directory-layout helpers (`CreateTiles`,
//!   `GetTrickplayDirectory`, `GetTrickplayTilePathAsync`,
//!   `MoveGeneratedTrickplayDataAsync`) that operate on the un-ported domain
//!   `Video` and on-disk layout are dropped from the trait; they resurface as
//!   `hermit-core` free functions in Wave 6.
//! - `Task<T>` → `async fn -> Result<T, ServiceError>`; `CancellationToken` /
//!   `IProgress` are dropped for v1.
//!
//! The trait is object-safe and carries a `_assert_object_safe_*` assertion.

use std::collections::HashMap;

use async_trait::async_trait;
use hermit_db::entities::playback::TrickplayInfoEntity;
use uuid::Uuid;

use crate::error::ServiceError;

/// Generates, stores and serves trickplay (scrubbing-preview) tiles.
///
/// Port of `ITrickplayManager`.
#[async_trait]
pub trait TrickplayManager: Send + Sync {
    /// (Re)generates trickplay images and metadata for a video.
    ///
    /// `replace` forces existing data to be regenerated rather than reused.
    async fn refresh_trickplay_data(
        &self,
        item_id: Uuid,
        replace: bool,
    ) -> Result<(), ServiceError>;

    /// Gets the available trickplay resolutions for an item, keyed by the width
    /// of a single thumbnail.
    async fn get_trickplay_resolutions(
        &self,
        item_id: Uuid,
    ) -> Result<HashMap<i32, TrickplayInfoEntity>, ServiceError>;

    /// Lists trickplay infos across items, paged by `limit`/`offset`.
    async fn get_trickplay_items(
        &self,
        limit: i32,
        offset: i32,
    ) -> Result<Vec<TrickplayInfoEntity>, ServiceError>;

    /// Persists a trickplay info row.
    async fn save_trickplay_info(&self, info: &TrickplayInfoEntity) -> Result<(), ServiceError>;

    /// Deletes all trickplay data for an item.
    async fn delete_trickplay_data(&self, item_id: Uuid) -> Result<(), ServiceError>;

    /// Gets the full trickplay manifest for an item: media-source id → (tile
    /// width → info).
    async fn get_trickplay_manifest(
        &self,
        item_id: Uuid,
    ) -> Result<HashMap<String, HashMap<i32, TrickplayInfoEntity>>, ServiceError>;

    /// Builds the trickplay HLS (`.m3u8`) playlist text for a resolution.
    async fn get_hls_playlist(
        &self,
        item_id: Uuid,
        width: i32,
        api_key: Option<&str>,
    ) -> Result<Option<String>, ServiceError>;
}

fn _assert_object_safe_trickplay_manager(_: &dyn TrickplayManager) {}
