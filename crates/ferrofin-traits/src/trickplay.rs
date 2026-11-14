//! Trickplay manager trait — scrubbing-preview tile generation and lookup.
//!
//! Port of `MediaBrowser.Controller.Trickplay.ITrickplayManager`.
//!
//! Port rules applied:
//! - The C# `Video` / `BaseItem` receivers become [`uuid::Uuid`] identity
//!   arguments; callers supply the resolved `LibraryOptions` for refreshes.
//! - Stored trickplay metadata is a persistence concern, so it surfaces as the
//!   [`TrickplayInfoEntity`]
//!   row (the C# `TrickplayInfo` EF entity). Maps keyed by width use `i32`
//!   keys.
//! - `GetTrickplayManifest(BaseItem)` walks the item's media sources, which
//!   are other items' rows, so the DTO service builds the manifest over the
//!   version group it already reads, from
//!   [`TrickplayManager::get_trickplay_resolutions_batch`].
//! - `IReadOnlyList`/`Dictionary` become `Vec`/`HashMap`.
//! - The image-tiling / directory-layout helpers (`CreateTiles`,
//!   `GetTrickplayDirectory`, `GetTrickplayTilePathAsync`,
//!   `MoveGeneratedTrickplayDataAsync`) that operate on the un-ported domain
//!   `Video` and on-disk layout are dropped from the trait; they resurface as
//!   `ferrofin-core` free functions in Wave 6.
//! - `Task<T>` → `async fn -> Result<T, ServiceError>`; `CancellationToken` /
//!   `IProgress` are dropped for v1.
//!
//! The trait is object-safe and carries a `_assert_object_safe_*` assertion.

use std::collections::HashMap;

use async_trait::async_trait;
use ferrofin_db::entities::playback::TrickplayInfoEntity;
use ferrofin_model::configuration::LibraryOptions;
use uuid::Uuid;

use crate::error::ServiceError;

/// A Generate Trickplay Images pass that checks the images already on disk,
/// because the trickplay widths or chroma changed since the last pass that
/// finished. Ferrofin's own: upstream never replaces images unasked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrickplayCheck {
    /// The `WidthResolutions` the last finished pass ran under, those of any
    /// checking pass started since that did not finish, and those of sets
    /// made since under other settings (a library scan, an item refresh). A
    /// stored width among them that is no longer selected is removed once
    /// every selected width has its images.
    pub previous_widths: Vec<i32>,
    /// The settings this pass runs under, in the implementation's own
    /// encoding, recorded by [`TrickplayManager::finish_trickplay_pass`].
    pub settings: String,
}

/// Generates, stores and serves trickplay (scrubbing-preview) tiles.
///
/// Port of `ITrickplayManager`.
#[async_trait]
pub trait TrickplayManager: Send + Sync {
    /// (Re)generates trickplay images and metadata for a video.
    ///
    /// `replace` forces existing data to be regenerated rather than reused;
    /// each width's old images stay until its new ones are in place.
    /// `library_options` are the item's library's options (C#
    /// `ILibraryManager.GetLibraryOptions(item)`): when
    /// `enable_trickplay_image_extraction` is off, existing tiles and rows for
    /// the item are pruned instead of generated.
    async fn refresh_trickplay_data(
        &self,
        item_id: Uuid,
        replace: bool,
        library_options: &LibraryOptions,
    ) -> Result<(), ServiceError>;

    /// Refreshes from the caller's row: the scan's current row before its
    /// final save, or a Generate Trickplay Images page's row.
    /// `Some(streams)` is the item's streams the caller holds (a completed
    /// fresh probe, or the page's stored streams), including an empty result;
    /// `None` uses persisted streams when the probe did not supply new context.
    /// The default preserves identity-only implementations of the seam.
    async fn refresh_trickplay_for_media(
        &self,
        item_id: Uuid,
        entity: &ferrofin_db::entities::base_items::BaseItemEntity,
        streams: Option<&[ferrofin_db::entities::base_items::MediaStreamInfoEntity]>,
        replace: bool,
        library_options: &LibraryOptions,
    ) -> Result<(), ServiceError> {
        let _ = (entity, streams);
        self.refresh_trickplay_data(item_id, replace, library_options)
            .await
    }

    /// Starts a Generate Trickplay Images pass: `Some` when the trickplay
    /// widths or chroma differ from those the last finished pass ran under, so
    /// this pass checks every item's images through
    /// [`Self::check_trickplay_data`], as when a checking pass started since
    /// did not finish, or as when sets were made under other settings since.
    /// `None` when they are unchanged, when no width is selected (nothing is
    /// made or removed, and the recorded settings stay), or when none are
    /// recorded yet (they are recorded now, and nothing is checked). The
    /// default never checks.
    async fn begin_trickplay_pass(&self) -> Result<Option<TrickplayCheck>, ServiceError> {
        Ok(None)
    }

    /// [`Self::refresh_trickplay_data`] on a checking pass: images whose
    /// chroma no longer matches the settings are regenerated and swapped in,
    /// and widths no longer selected are removed. Returns whether the item's
    /// images now match: no set at another chroma and no width an earlier
    /// pass made left behind. The default refreshes without checking.
    async fn check_trickplay_data(
        &self,
        item_id: Uuid,
        check: &TrickplayCheck,
        library_options: &LibraryOptions,
    ) -> Result<bool, ServiceError> {
        let _ = check;
        self.refresh_trickplay_data(item_id, false, library_options)
            .await
            .map(|()| true)
    }

    /// [`Self::check_trickplay_data`] over the caller's row and stored
    /// streams, as [`Self::refresh_trickplay_for_media`] is to
    /// [`Self::refresh_trickplay_data`]: Generate Trickplay Images hands each
    /// video its page's row and streams. The default preserves identity-only
    /// implementations of the seam.
    async fn check_trickplay_for_media(
        &self,
        item_id: Uuid,
        entity: &ferrofin_db::entities::base_items::BaseItemEntity,
        streams: &[ferrofin_db::entities::base_items::MediaStreamInfoEntity],
        check: &TrickplayCheck,
        library_options: &LibraryOptions,
    ) -> Result<bool, ServiceError> {
        let _ = (entity, streams);
        self.check_trickplay_data(item_id, check, library_options)
            .await
    }

    /// Records `check`'s settings as those of the last finished pass, once it
    /// has checked every item. The default records nothing.
    async fn finish_trickplay_pass(&self, check: &TrickplayCheck) -> Result<(), ServiceError> {
        let _ = check;
        Ok(())
    }

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

    /// Batch form of [`Self::get_trickplay_resolutions`], keyed by item; an
    /// item with no trickplay rows is left out. A media source's id is the id
    /// of the item it comes from, so the DTO service builds
    /// `BaseItemDto.Trickplay` (upstream's `GetTrickplayManifest`, which asks
    /// `GetTrickplayResolutions` once per media source) from one call over
    /// every source a page lists. The default loops the single-item form; the
    /// concrete manager overrides it with one query.
    async fn get_trickplay_resolutions_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, HashMap<i32, TrickplayInfoEntity>>, ServiceError> {
        let mut map = HashMap::with_capacity(item_ids.len());
        for &id in item_ids {
            let resolutions = self.get_trickplay_resolutions(id).await?;
            if !resolutions.is_empty() {
                map.insert(id, resolutions);
            }
        }
        Ok(map)
    }

    /// Builds the trickplay HLS (`.m3u8`) playlist text for a resolution.
    async fn get_hls_playlist(
        &self,
        item_id: Uuid,
        width: i32,
        api_key: Option<&str>,
    ) -> Result<Option<String>, ServiceError>;

    /// Resolves the on-disk path of the trickplay tile image at `index` for a
    /// resolution, if that resolution is known.
    ///
    /// Port of `GetTrickplayTilePathAsync`. Returns `None` when the item has no
    /// stored info for `width`; the returned path is not guaranteed to exist on
    /// disk (the caller checks before serving, mirroring the C# `File.Exists`
    /// gate). The implementation resolves the item's live library setting to
    /// choose its internal or media-adjacent trickplay directory.
    async fn get_trickplay_tile_path(
        &self,
        item_id: Uuid,
        width: i32,
        index: i32,
    ) -> Result<Option<String>, ServiceError>;
}

fn _assert_object_safe_trickplay_manager(_: &dyn TrickplayManager) {}
