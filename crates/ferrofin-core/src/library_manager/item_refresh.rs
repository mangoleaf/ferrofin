//! What an item's refresh covers: upstream's `ProviderManager.RefreshItem`,
//! `RefreshArtist` and `RefreshFullItem` (`ProviderManager.cs:1209-1295`),
//! which [`LibraryManager::queue_item_refresh`],
//! [`LibraryManager::refresh_item_metadata`] and
//! [`LibraryManager::refresh_full_item`] run. An item with a path refreshes
//! through the library scan of that path, an item with no file of its own
//! through the provider manager.

use std::collections::HashSet;
use std::sync::Arc;

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager as _, ScanTarget};
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::providers::{MetadataRefreshOptions, ProviderManager, RefreshPriority};
use uuid::Uuid;

use super::FerrofinLibraryManager;

impl FerrofinLibraryManager {
    /// The scan `item`'s refresh is (`ProviderManager.RefreshItem`, or
    /// `RefreshArtist` for a music artist), or `None` when no scan covers it
    /// and it refreshes through the provider manager.
    pub(super) async fn refresh_target(
        &self,
        item: &BaseItemEntity,
    ) -> Result<Option<ScanTarget>, ServiceError> {
        let id = item_id(item)?;
        if BaseItemKind::from_stored_type_name(&item.type_) == Some(BaseItemKind::MusicArtist) {
            return self.artist_target(item, id).await.map(Some);
        }
        Ok(scan_target(item, id))
    }

    /// Queues `item`'s refresh through the provider manager at high
    /// priority, as `QueueRefresh` does for an item no scan covers.
    pub(super) async fn queue_provider_refresh(
        &self,
        item: &BaseItemEntity,
        options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        self.providers()?
            .queue_refresh(item_id(item)?, options, RefreshPriority::High)
            .await
    }

    /// The provider manager, which the composition root always sets.
    pub(super) fn providers(&self) -> Result<&Arc<dyn ProviderManager>, ServiceError> {
        self.providers.as_ref().ok_or_else(|| {
            ServiceError::backend(
                "an item refresh needs a provider manager, which this library manager has none of",
            )
        })
    }

    /// `ProviderManager.RefreshArtist` (`ProviderManager.cs:1258-1290`): the
    /// albums credited to the artist (`ArtistIds`) each name their
    /// `MusicArtist` — the nearest artist folder above them
    /// (`MusicAlbum.GetMusicArtist`) — and those artist folders validate their
    /// CHILDREN with the request's options (`ValidateChildren` refreshes a
    /// folder's children, never the folder itself, `Folder.cs:831-870`); then
    /// the artist refreshes itself (`item.RefreshMetadata`).
    ///
    /// A folder-backed artist refreshes itself in the same scan
    /// ([`ScanTarget::Artist`]'s `path`); its own folder is validated only when
    /// one of its credited albums sits under it. An artist known only by name
    /// has no folder (`MusicArtist.ValidateChildrenInternal` returns on
    /// `IsAccessedByName`): the scanner refreshes it after its albums' folders
    /// (a `path` of `None`), its music providers under the request's options —
    /// through the scan queue's priority lane when there is no folder to
    /// validate, so it runs inside a running scan and never beside one. An album
    /// with no artist folder above it names the by-name artist, whose validation
    /// is that no-op, so it is not scanned — as upstream.
    async fn artist_target(
        &self,
        item: &BaseItemEntity,
        id: Uuid,
    ) -> Result<ScanTarget, ServiceError> {
        let own_folder = item
            .path
            .clone()
            .filter(|p| !p.is_empty())
            .filter(|_| item.top_parent_id.as_deref().is_some_and(|t| !t.is_empty()));
        let albums = self
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::MusicAlbum],
                artist_ids: vec![id],
                ..InternalItemsQuery::default()
            })
            .await?;
        // Albums in one folder have the same folders above them, so each
        // album folder's ancestors are read once.
        let mut walked: HashSet<&str> = HashSet::new();
        let mut folders: Vec<String> = Vec::new();
        for album in &albums {
            let Some(parent) = album.parent_id.as_deref() else {
                continue;
            };
            if !walked.insert(parent) {
                continue;
            }
            if let Some(path) = self.artist_folder_above(album).await?
                && !folders.contains(&path)
            {
                folders.push(path);
            }
        }
        Ok(ScanTarget::Artist {
            id,
            path: own_folder,
            folders,
        })
    }

    /// The path of the nearest folder-backed `MusicArtist` above `album` (the
    /// parent chain `MusicAlbum.GetMusicArtist` walks), if any, from one read
    /// of its ancestors.
    async fn artist_folder_above(
        &self,
        album: &BaseItemEntity,
    ) -> Result<Option<String>, ServiceError> {
        let Ok(album_id) = Uuid::parse_str(&album.id) else {
            return Ok(None);
        };
        for parent in self.get_ancestors(album_id).await?.unwrap_or_default() {
            match BaseItemKind::from_stored_type_name(&parent.type_) {
                Some(BaseItemKind::MusicArtist) => {
                    return Ok(parent.path.filter(|p| !p.is_empty()));
                }
                Some(
                    BaseItemKind::CollectionFolder
                    | BaseItemKind::AggregateFolder
                    | BaseItemKind::UserRootFolder,
                ) => break,
                _ => {}
            }
        }
        Ok(None)
    }
}

/// The scan Identify's refresh of `item` is (`RefreshFullItem`), or `None`
/// when it refreshes through the provider manager. An artist known only by
/// name has no folder of its own: the scanner refreshes it as
/// `RefreshFullItem` → `RefreshItem` does — itself, with the chosen ids — in
/// the scan queue's priority lane (its by-name folder validation is a no-op
/// upstream).
///
/// Nothing is refused: `ApplySearchCriteria` refreshes any item
/// `GetItemById` returns. An item whose path no library location covers
/// refreshes through the scan of its path all the same, which plans it
/// outside every library (the scanner's `libraries_outside`).
pub(super) fn identify_target(item: &BaseItemEntity) -> Result<Option<ScanTarget>, ServiceError> {
    let id = item_id(item)?;
    let by_name_artist = BaseItemKind::from_stored_type_name(&item.type_)
        == Some(BaseItemKind::MusicArtist)
        && item.top_parent_id.as_deref().is_none_or(str::is_empty);
    if by_name_artist {
        return Ok(Some(ScanTarget::Artist {
            id,
            path: None,
            folders: Vec::new(),
        }));
    }
    Ok(scan_target(item, id))
}

/// `item`'s id.
pub(super) fn item_id(item: &BaseItemEntity) -> Result<Uuid, ServiceError> {
    Uuid::parse_str(&item.id)
        .map_err(|_| ServiceError::backend(format!("item id {} is not a GUID", item.id)))
}

/// The scan an item's refresh is (`ProviderManager.RefreshItem`), if any. A
/// CollectionFolder refreshes its physical folders
/// (`RefreshCollectionFolderChildren`), the root every library, and any
/// other folder validates its own children — its subtree, not its whole
/// library — which the scan refreshes along with the folder itself.
///
/// A file item (a movie, an episode, a track, a book, a photo, a video…)
/// refreshes itself (`RefreshSingleItem`) through the same metadata service
/// the scan runs: here the scan of its one path, so it gets the scan's
/// refresh decision, merge, field locks, probe, NFO and every metadata
/// provider, and nothing else of its library is walked.
///
/// No scan covers an item upstream validates no children for and that has
/// no file of its own: a box set's members and a playlist's entries are
/// linked, not physical (`BoxSet.GetNonCachedChildren` returns none,
/// `Playlist.ValidateChildrenInternal` is a no-op), a view's folder holds
/// none, a folder with no path of its own — a virtual season, whose episodes
/// sit in the series folder — is not `IsFileProtocol`, so
/// `ValidateChildrenInternal2` validates and refreshes no child
/// (`Folder.cs:430-436,780-812`), and a person, genre, studio or channel
/// item has no file. Those refresh through the provider manager.
fn scan_target(item: &BaseItemEntity, id: Uuid) -> Option<ScanTarget> {
    if !item.is_folder {
        return refresh_scan_path(item).map(|path| ScanTarget::Items(vec![path]));
    }
    match BaseItemKind::from_stored_type_name(&item.type_) {
        Some(BaseItemKind::CollectionFolder) => Some(ScanTarget::Library(id)),
        Some(BaseItemKind::AggregateFolder | BaseItemKind::UserRootFolder) => Some(ScanTarget::All),
        Some(
            BaseItemKind::BoxSet
            | BaseItemKind::Playlist
            | BaseItemKind::ManualPlaylistsFolder
            | BaseItemKind::PlaylistsFolder
            | BaseItemKind::UserView,
        ) => None,
        _ => item
            .path
            .clone()
            .filter(|p| !p.is_empty())
            .map(|path| ScanTarget::Paths(vec![path])),
    }
}

/// The path an item refreshes through the library scan by: its own
/// filesystem path, when it has one and a top parent (a library's item, or
/// one whose location left its library, which the scan plans outside every
/// library). `None` for an item with no file of its own — a person, a genre
/// or studio, a channel or live-TV item streamed from a URL — or a row with
/// no top parent, which the provider manager refreshes (gl-2030).
pub(super) fn refresh_scan_path(item: &BaseItemEntity) -> Option<String> {
    let in_library = item
        .top_parent_id
        .as_deref()
        .is_some_and(|top| !top.is_empty());
    item.path
        .clone()
        .filter(|path| in_library && !path.is_empty() && !path.contains("://"))
}
