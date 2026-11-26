//! A box set's `collection.xml` and a playlist folder's `playlist.xml` —
//! port of `BoxSetXmlProvider` and `PlaylistXmlProvider` (the local readers,
//! `BaseXmlProvider`) and of `BoxSetXmlSaver` and `PlaylistXmlSaver`
//! (`BaseXmlSaver`), with the save gate `ProviderManager.IsSaverEnabledForItem`
//! applies to them.
//!
//! The document itself is [`crate::local_xml`]'s. This module decides which
//! file, whether it is read, written, and where its members count:
//!
//! - **Read** in the item's refresh, as its local metadata provider: the file
//!   in the box set's or playlist's folder, when the item supports local
//!   metadata (a local path, no channel). Its change monitor (`HasChanged`,
//!   no tolerance) is the refresh plan's [`LocalMetadataFormat::Xml`]. A
//!   locked box set or playlist reads nothing (`RefreshWithProviders`
//!   returns before its local providers).
//! - **Members** (`BoxSetMetadataService.MergeData` and
//!   `PlaylistMetadataService.MergeData`, v12.4): the file's item list joins
//!   the container's links only for one outside the data directory. For one
//!   in it (`{data}/collections`, `{data}/playlists`) the `LinkedChildren`
//!   table is the source of truth, and its file's members are never read
//!   (owner decision 2026-11-14: follow v12.4).
//! - **Shares** (`PlaylistMetadataService.MergeData`): a playlist's file
//!   shares replace the stored ones when the refresh replaces (a Default
//!   refresh, "Replace all metadata", a file with `<LockData>`) or none is
//!   stored, an empty list included; otherwise the file's come first and
//!   the stored shares of the users it does not name follow. Its
//!   `<PlaylistMediaType>` always replaces the stored one (`Unknown` when the
//!   file names none). The caller merges both.
//! - **Write** after any update from `MetadataDownload` up for a box set
//!   (`BoxSetXmlSaver.IsEnabledFor`), and any update at all for a playlist
//!   (`PlaylistXmlSaver.IsEnabledFor`, `MetadataImport` up): a refresh that
//!   read the file (a playlist), downloaded metadata or was forced, a
//!   metadata edit, every member added or removed, every share change.
//!   `BoxSet.IsSaveLocalMetadataEnabled()` and
//!   `Playlist.IsSaveLocalMetadataEnabled()` are always true upstream; here
//!   that holds only for a container whose folder is in the server's own
//!   directories (owner decision 2026-10-17, CLAUDE.md "Current scope"). One
//!   inside a media library follows the library's "Save artwork into media
//!   folders" (`SaveLocalMetadata`), so nothing is written into a media
//!   folder the library did not open to it.
//!
//! ACCEPTED DIVERGENCE (upstream bug, not ported; owner decision 2026-11-24,
//! gl-1915): a playlist FILE (`.m3u`, …) has no `<name>.xml` here: none is
//! written and none is read. Jellyfin 12.4 writes and reads one beside it
//! (`PlaylistXmlSaver.GetSavePath`) holding the playlist's fields but no
//! `PlaylistItems`, and `PlaylistXmlProvider` outranks
//! `PlaylistItemsProvider`, so a replacing refresh empties the playlist.
//! Here the members come from the playlist file alone (`ferrofin-core`'s
//! `library_scan/playlists.rs`). Writing the file would also leave it in the
//! media folder for a Jellyfin to read back.
//!
//! ACCEPTED DIVERGENCE (owner decision 2026-11-19, gl-1870): an edit or a
//! member or share change (`MetadataEdit`) never rewrites a `collection.xml`
//! or `playlist.xml` already in a media folder the library did not open. In
//! a library with no saver list upstream writes them whatever
//! `SaveLocalMetadata` says (the override above), and for any other item
//! `IsSaverEnabledForItem` rewrites a saver's existing file on an edit.
//! A file left in place can be read again at a refresh until the library
//! opts in.
//!
//! ACCEPTED DIVERGENCE (upstream bug, not ported): a playlist whose file is
//! already there when the playlist is first refreshed (a playlist folder
//! copied into a library, or one another server wrote) is read from it.
//! Jellyfin 12.4's first refresh saves the new item before its local
//! providers run (`RefreshMetadata`'s `if (isFirstRefresh) SaveItemAsync(…,
//! MetadataImport)`), and `PlaylistXmlSaver`, enabled from `MetadataImport`
//! up, writes the folder's name with no members and no shares over the file,
//! which the refresh then reads (measured on Jellyfin 12.4.0).
//!
//! ACCEPTED DIVERGENCE (upstream bug, not ported): the members written are
//! the container's stored links, each by its item's path. Jellyfin 12.4's
//! `AddLinkedChildren` writes a path only for a link that carries an item id,
//! so the save right after a refresh took the members from a file by path
//! writes an empty `<PlaylistItems />` (measured on Jellyfin 12.4.0), which
//! the next read of the file takes for a playlist with no members.
//!
//! ACCEPTED DIVERGENCE (upstream bug, not ported; owner decision
//! 2026-11-14): the file's `<DisplayOrder>` applies like any other field it
//! carries, and no refresh resets a box set's display order. Upstream's
//! refresh starts its provider result from a new `BoxSet`, whose
//! `DisplayOrder` defaults to `PremiereDate`: `MergeDisplayOrder` never lets
//! the file's value fill it, and every refresh a provider answered (the file
//! or TheMovieDb) replaces the box set's display order with `PremiereDate`.
//! The dates and tagline the file round-trips are [`crate::local_xml`]'s.
//!
//! [`LocalMetadataFormat::Xml`]: crate::refresh_plan::LocalMetadataFormat::Xml

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::configuration::{LibraryOptions, MetadataOptions};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities_media::PlaylistUserPermissions;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{SelfWrite, SelfWriteGuard, VirtualFolderManager};
use ferrofin_traits::persistence::{
    ItemPersistenceService, ItemRepository, LinkedChildrenService, PeopleRepository,
};
use ferrofin_traits::providers::ItemUpdateType;
use uuid::Uuid;

use crate::local_xml::{LocalContainerXml, LocalLinkedChild, parse_container_xml};
use crate::metadata_merge::MetadataResult;

/// The saver's and the reader's name (`XmlProviderUtils.Name`): what a
/// library's metadata savers list names it by.
pub const SAVER_NAME: &str = "Emby Xml";

/// `LinkedChildType.Manual`, the links the saver writes.
const MANUAL_LINK: i32 = 0;

/// A box set's `collection.xml` or a playlist folder's `playlist.xml` on
/// disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalXmlFile {
    /// The file.
    pub path: PathBuf,
    /// Its last write time, which `HasChanged` compares with the item's
    /// `DateLastSaved`.
    pub mtime: DateTime<Utc>,
}

/// A box set or a playlist, the kinds with a file here.
fn container_kind(row: &BaseItemEntity) -> Option<BaseItemKind> {
    BaseItemKind::from_stored_type_name(&row.type_)
        .filter(|kind| matches!(kind, BaseItemKind::BoxSet | BaseItemKind::Playlist))
}

/// The file of a box set (`collection.xml`) or a playlist folder
/// (`playlist.xml`, `PlaylistXmlSaver.GetSavePath`) that supports local
/// metadata (`SupportsLocalMetadata`: a local path, not a channel item), else
/// `None`. A playlist FILE has none here (see the module doc).
#[must_use]
pub fn xml_path(row: &BaseItemEntity) -> Option<PathBuf> {
    let kind = container_kind(row)?;
    let path = Path::new(row.path.as_deref().filter(|path| !path.is_empty())?);
    let channel = row.channel_id.as_deref().is_some_and(|channel| {
        !channel.is_empty() && Uuid::parse_str(channel).map_or(true, |id| !id.is_nil())
    });
    if !path.is_absolute() || channel {
        return None;
    }
    match kind {
        // `Playlist.IsPlaylistFile`: an extension, and no directory.
        BaseItemKind::Playlist if path.extension().is_some() && !path.is_dir() => None,
        BaseItemKind::Playlist => Some(crate::playlist_file::playlist_xml_path(
            &path.to_string_lossy(),
            false,
        )),
        _ => Some(path.join("collection.xml")),
    }
}

/// The item's file when it exists (`GetXmlFile` through the directory
/// service, then the stat `HasChanged` makes).
pub async fn local_file(row: &BaseItemEntity) -> Option<LocalXmlFile> {
    let path = xml_path(row)?;
    let meta = tokio::fs::metadata(&path)
        .await
        .ok()
        .filter(std::fs::Metadata::is_file)?;
    Some(LocalXmlFile {
        mtime: DateTime::<Utc>::from(meta.modified().ok()?),
        path,
    })
}

/// `BoxSetXmlProvider.GetMetadata` / `PlaylistXmlProvider.GetMetadata` for
/// an item of `kind`: the file parsed with the item's external id keys.
/// `None` (`HasMetadata` false) when it cannot be read; a document that does
/// not parse is logged and is no metadata either, as upstream's
/// `XmlException` is.
pub async fn read(
    path: &Path,
    kind: BaseItemKind,
    provider_keys: &[String],
) -> Option<LocalContainerXml> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "could not read the item's local XML metadata");
            return None;
        }
    };
    let text = ferrofin_util::stream_extensions::decode_stream_text(bytes);
    let parsed = parse_container_xml(&text, kind, provider_keys);
    if parsed.is_none() {
        tracing::warn!(path = %path.display(), "the item's local XML metadata is not well-formed XML; it is not read");
    }
    parsed
}

/// Whether a box set or playlist at `path` takes its file's members: it lies
/// outside `data_path` (`!FileSystem.ContainsSubPath(DataPath, targetPath)`).
#[must_use]
pub fn takes_members_from_file(data_path: &str, path: &Path) -> bool {
    data_path.is_empty() || !path.starts_with(data_path) || path == Path::new(data_path)
}

/// Whether the library scan's "Replace all metadata" skips a box set's or
/// playlist folder's file (`isSavingMetadata`): its saver is enabled for a
/// metadata edit. The scan reaches only items in a media library, which
/// never take the own-directory override.
#[must_use]
pub fn saves_metadata(
    row: &BaseItemEntity,
    library: Option<&LibraryOptions>,
    global: Option<&MetadataOptions>,
) -> bool {
    xml_path(row).is_some()
        && container_kind(row)
            .is_some_and(|kind| enabled(kind, library, global, ItemUpdateType::MetadataEdit, false))
}

/// `IsSaverEnabledForItem(saver, item, libraryOptions, update, false)` for
/// `BoxSetXmlSaver` or `PlaylistXmlSaver` (by `kind`), past
/// `SupportsLocalMetadata` ([`xml_path`]):
///
/// - `IsEnabledFor`: an update from `MetadataDownload` up for a box set, any
///   update for a playlist (`MetadataImport` up);
/// - a library that lists its metadata savers must list [`SAVER_NAME`];
/// - otherwise the server's options for the kind must not disable it, and
///   `IsSaveLocalMetadataEnabled()` must hold — `own_directory` (the
///   override, see the module doc) or the library's `SaveLocalMetadata` —
///   whether or not the file already exists (the module doc's divergence).
fn enabled(
    kind: BaseItemKind,
    library: Option<&LibraryOptions>,
    global: Option<&MetadataOptions>,
    update: ItemUpdateType,
    own_directory: bool,
) -> bool {
    let wanted = match kind {
        BaseItemKind::Playlist => update != ItemUpdateType::None,
        _ => matches!(
            update,
            ItemUpdateType::MetadataDownload | ItemUpdateType::MetadataEdit
        ),
    };
    if !wanted {
        return false;
    }
    if let Some(savers) = library.and_then(|library| library.metadata_savers.as_ref()) {
        return savers
            .iter()
            .any(|saver| saver.eq_ignore_ascii_case(SAVER_NAME));
    }
    if global.is_some_and(|global| {
        global
            .disabled_metadata_savers
            .iter()
            .any(|saver| saver.eq_ignore_ascii_case(SAVER_NAME))
    }) {
        return false;
    }
    own_directory || library.is_some_and(|library| library.save_local_metadata)
}

/// The registered `BoxSetXmlSaver` and `PlaylistXmlSaver`, reading the item
/// through the repositories. One save at a time, so two concurrent updates
/// never interleave their replacements of a file.
pub struct ContainerXmlSaver {
    items: Arc<dyn ItemRepository>,
    persistence: Arc<dyn ItemPersistenceService>,
    people: Arc<dyn PeopleRepository>,
    linked_children: Arc<dyn LinkedChildrenService>,
    folders: Arc<dyn VirtualFolderManager>,
    configuration: Arc<dyn ServerConfigurationManager>,
    lock: tokio::sync::Mutex<()>,
    /// Brackets each file written (`ILibraryMonitor`). `None` in unit tests.
    self_writes: Option<Arc<dyn SelfWriteGuard>>,
}

impl ContainerXmlSaver {
    /// Creates the saver over the repositories a box set's or playlist's
    /// document is made from.
    #[must_use]
    pub fn new(
        items: Arc<dyn ItemRepository>,
        persistence: Arc<dyn ItemPersistenceService>,
        people: Arc<dyn PeopleRepository>,
        linked_children: Arc<dyn LinkedChildrenService>,
        folders: Arc<dyn VirtualFolderManager>,
        configuration: Arc<dyn ServerConfigurationManager>,
    ) -> Self {
        Self {
            items,
            persistence,
            people,
            linked_children,
            folders,
            configuration,
            lock: tokio::sync::Mutex::new(()),
            self_writes: None,
        }
    }

    /// Brackets each file this saver writes on `guard`, as upstream does
    /// (`ReportFileSystemChangeBeginning`/`Complete` in
    /// `ProviderManager.SaveMetadataAsync`). Called once by the composition
    /// root.
    #[must_use]
    pub fn with_self_writes(mut self, guard: Arc<dyn SelfWriteGuard>) -> Self {
        self.self_writes = Some(guard);
        self
    }

    /// The server's data directory: a box set or playlist outside it takes
    /// its file's members ([`takes_members_from_file`]).
    #[must_use]
    pub fn data_path(&self) -> String {
        self.configuration.application_paths().data_path()
    }

    /// Whether the saver is enabled for `row` at `update`
    /// (`IsSaverEnabledForItem`). With `MetadataEdit` this is upstream's
    /// `isSavingMetadata`, under which "Replace all metadata" skips the
    /// local reader.
    ///
    /// # Errors
    /// A library or configuration read that failed.
    pub async fn is_enabled_for(
        &self,
        row: &BaseItemEntity,
        update: ItemUpdateType,
    ) -> Result<bool, ServiceError> {
        let (Some(kind), Some(_)) = (container_kind(row), xml_path(row)) else {
            return Ok(false);
        };
        // The libraries' snapshot, as upstream's `GetLibraryOptions(item)`
        // reads them from memory.
        let folders = self.folders.library_folders().await?;
        let library = ferrofin_model::entities_media::owning_library(
            &folders,
            row.top_parent_id.as_deref(),
            row.path.as_deref(),
        )
        .and_then(|folder| folder.library_options.as_ref());
        let server = self.configuration.configuration().await?;
        let global = crate::library_options::global_metadata_options(
            &server.metadata_options,
            if kind == BaseItemKind::Playlist {
                "Playlist"
            } else {
                "BoxSet"
            },
        );
        let own_directory = row.path.as_deref().is_some_and(|folder| {
            crate::image_save::in_server_directories(
                self.configuration.application_paths().as_ref(),
                Path::new(folder),
            )
        });
        Ok(enabled(kind, library, global, update, own_directory))
    }

    /// `ProviderManager.SaveMetadataAsync` with this saver: writes the box
    /// set's `collection.xml` or the playlist's `playlist.xml` when
    /// [`is_enabled_for`](Self::is_enabled_for) says so. Returns whether a
    /// file was written, after which the caller stamps the item's
    /// `DateLastSaved` past the file's write time
    /// (`LibraryManager.UpdateItemsAsync`), so its own write is no change to
    /// read back.
    ///
    /// # Errors
    /// A repository, configuration or filesystem failure. The caller logs it
    /// without undoing the metadata change, as upstream does.
    pub async fn save(&self, item_id: Uuid, update: ItemUpdateType) -> Result<bool, ServiceError> {
        if update == ItemUpdateType::None {
            return Ok(false);
        }
        match self.items.retrieve_item(item_id).await? {
            Some(row) => self.save_read(item_id, &row, update).await,
            None => Ok(false),
        }
    }

    /// [`save`](Self::save) for an item whose row the caller has just read
    /// (`read`), so the savers of one save share that read: anything but a
    /// box set or a playlist folder returns at once. It only gates: the row
    /// is read again under the lock, and that one is written.
    ///
    /// # Errors
    /// As [`save`](Self::save).
    pub async fn save_read(
        &self,
        item_id: Uuid,
        read: &BaseItemEntity,
        update: ItemUpdateType,
    ) -> Result<bool, ServiceError> {
        // Every item's metadata save comes here: only a box set's or a
        // playlist's waits on the lock, and reads its row again under it, so
        // the last writer writes the newest row.
        if update == ItemUpdateType::None || xml_path(read).is_none() {
            return Ok(false);
        }
        let _guard = self.lock.lock().await;
        let Some(row) = self.items.retrieve_item(item_id).await? else {
            return Ok(false);
        };
        let (Some(path), Some(kind)) = (xml_path(&row), container_kind(&row)) else {
            return Ok(false);
        };
        if !self.is_enabled_for(&row, update).await? {
            return Ok(false);
        }
        let document = self.document(item_id, row).await?;
        let xml = if kind == BaseItemKind::Playlist {
            crate::local_xml::save_playlist_xml(&document)
        } else {
            crate::local_xml::save_collection_xml(&document)
        };
        let _write = SelfWrite::begin(self.self_writes.as_ref(), path.to_string_lossy());
        tokio::task::spawn_blocking(move || {
            ferrofin_util::file_helper::atomic_write_media(&path, xml.as_bytes())
        })
        .await
        .map_err(|err| ServiceError::backend(err.to_string()))?
        .map_err(|err| ServiceError::backend(format!("write local XML metadata: {err}")))?;
        Ok(true)
    }

    /// The item as `BaseXmlSaver` writes it: its row, provider ids, locked
    /// fields and people, and its manual links with the path each resolves
    /// to (`AddLinkedChildren`); a playlist's owner, shares and media type
    /// besides, the owner and shares from its permission rows.
    async fn document(
        &self,
        item_id: Uuid,
        row: BaseItemEntity,
    ) -> Result<LocalContainerXml, ServiceError> {
        let provider_ids = self
            .persistence
            .provider_ids_for_items(&[item_id])
            .await?
            .remove(&item_id)
            .unwrap_or_default();
        let locked_fields = self
            .persistence
            .locked_fields_for_items(&[item_id])
            .await?
            .remove(&item_id)
            .unwrap_or_default();
        let people = self
            .people
            .get_people_batch(&[item_id])
            .await?
            .remove(&item_id)
            .unwrap_or_default();
        let links = self
            .linked_children
            .get_linked_children_ids(item_id, Some(MANUAL_LINK))
            .await?;
        let mut paths = std::collections::HashMap::new();
        for child in self.items.retrieve_items(&links).await? {
            if let Ok(id) = Uuid::parse_str(&child.id) {
                paths.insert(id, child.path);
            }
        }
        let children = links
            .iter()
            .map(|id| LocalLinkedChild {
                path: paths.get(id).cloned().flatten(),
                item_id: None,
            })
            .collect();
        let mut document = LocalContainerXml {
            children,
            ..LocalContainerXml::default()
        };
        if container_kind(&row) == Some(BaseItemKind::Playlist) {
            // The permission rows, the source of truth; a playlist without
            // them keeps its owner and shares in `Data` alone.
            if let Some(ownership) = self.persistence.playlist_ownership(item_id).await? {
                document.owner_user_id = ownership.owner;
                document.shares = ownership.shares;
            } else {
                let data = ferrofin_db::item_data::parse_data(row.data.as_deref());
                let id = |value: Option<&serde_json::Value>| {
                    value
                        .and_then(serde_json::Value::as_str)
                        .and_then(|id| Uuid::parse_str(id).ok())
                };
                document.owner_user_id = id(data.get("OwnerUserId"));
                document.shares = data
                    .get("Shares")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|share| {
                        Some(PlaylistUserPermissions::new(
                            id(share.get("UserId"))?,
                            share
                                .get("CanEdit")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false),
                        ))
                    })
                    .collect();
            }
            // `PlaylistMediaType`, which upstream leaves out when `Unknown`.
            document.playlist_media_type = row
                .media_type
                .clone()
                .filter(|media_type| !media_type.is_empty() && media_type != "Unknown");
        }
        document.item = MetadataResult {
            item: row,
            people: Some(people),
            provider_ids,
            locked_fields,
        };
        Ok(document)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_set(path: Option<&str>) -> BaseItemEntity {
        BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.Movies.BoxSet".into(),
            path: path.map(str::to_owned),
            ..Default::default()
        }
    }

    #[test]
    fn only_a_local_box_set_has_a_collection_xml() {
        assert_eq!(
            xml_path(&box_set(Some("/data/collections/Saga [boxset]"))),
            Some(PathBuf::from(
                "/data/collections/Saga [boxset]/collection.xml"
            ))
        );
        // No folder (an older Ferrofin's box set, gl-738), a relative or
        // remote path, a channel item, another kind.
        assert_eq!(xml_path(&box_set(None)), None);
        assert_eq!(xml_path(&box_set(Some(""))), None);
        assert_eq!(xml_path(&box_set(Some("https://example/set"))), None);
        let mut channel = box_set(Some("/data/collections/Saga [boxset]"));
        channel.channel_id = Some(Uuid::from_u128(1).to_string());
        assert_eq!(xml_path(&channel), None);
        let mut movie = box_set(Some("/media/Movie"));
        movie.type_ = "MediaBrowser.Controller.Entities.Movies.Movie".into();
        assert_eq!(xml_path(&movie), None);
    }

    /// `ContainsSubPath(DataPath, path)` is an anchored, component-wise
    /// prefix: only a box set below the data directory keeps its links.
    #[rstest::rstest]
    #[case::in_data("/data/collections/Saga [boxset]", false)]
    #[case::elsewhere("/media/movies/Saga [boxset]", true)]
    #[case::sibling_prefix("/data2/collections/Saga [boxset]", true)]
    #[case::the_data_directory("/data", true)]
    fn members_come_from_the_file_only_outside_the_data_directory(
        #[case] path: &str,
        #[case] expected: bool,
    ) {
        assert_eq!(takes_members_from_file("/data", Path::new(path)), expected);
    }

    #[test]
    fn the_saver_writes_only_from_a_metadata_download_up() {
        for update in [
            ItemUpdateType::None,
            ItemUpdateType::MetadataImport,
            ItemUpdateType::ImageUpdate,
        ] {
            assert!(
                !enabled(BaseItemKind::BoxSet, None, None, update, true),
                "{update:?}"
            );
        }
        for update in [
            ItemUpdateType::MetadataDownload,
            ItemUpdateType::MetadataEdit,
        ] {
            assert!(
                enabled(BaseItemKind::BoxSet, None, None, update, true),
                "{update:?}"
            );
        }
    }

    /// A library that lists its savers decides alone; otherwise the
    /// server-wide disable list, then `IsSaveLocalMetadataEnabled`.
    #[rstest::rstest]
    fn a_library_listing_its_savers_must_list_emby_xml(
        #[values(BaseItemKind::BoxSet, BaseItemKind::Playlist)] kind: BaseItemKind,
    ) {
        let mut library = LibraryOptions {
            save_local_metadata: true,
            metadata_savers: Some(vec!["Nfo".into()]),
            ..Default::default()
        };
        let download = ItemUpdateType::MetadataDownload;
        assert!(!enabled(kind, Some(&library), None, download, true));
        library.metadata_savers = Some(vec!["emby xml".into()]);
        assert!(enabled(kind, Some(&library), None, download, false));
        library.metadata_savers = None;
        let global = MetadataOptions {
            disabled_metadata_savers: vec!["Emby Xml".into()],
            ..Default::default()
        };
        assert!(!enabled(
            kind,
            Some(&library),
            Some(&global),
            download,
            true
        ));
    }

    /// The owner rule: the always-true override holds in the server's own
    /// directories; a box set or playlist folder in a media library follows
    /// its library's `SaveLocalMetadata`, an edit included, whether or not
    /// its file is already there (gl-1870).
    #[rstest::rstest]
    fn outside_the_servers_directories_the_library_decides(
        #[values(BaseItemKind::BoxSet, BaseItemKind::Playlist)] kind: BaseItemKind,
    ) {
        let mut library = LibraryOptions {
            save_local_metadata: false,
            ..Default::default()
        };
        let download = ItemUpdateType::MetadataDownload;
        let edit = ItemUpdateType::MetadataEdit;
        assert!(enabled(kind, Some(&library), None, download, true));
        assert!(!enabled(kind, Some(&library), None, download, false));
        assert!(!enabled(kind, Some(&library), None, edit, false));
        assert!(!enabled(kind, None, None, download, false));
        library.save_local_metadata = true;
        assert!(enabled(kind, Some(&library), None, download, false));
    }

    /// `PlaylistXmlSaver.IsEnabledFor`: `updateType >= MetadataImport`, so
    /// an import (a refresh that read the file) and an image update write a
    /// playlist's file too, where they write no box set's.
    #[test]
    fn a_playlists_saver_writes_from_a_metadata_import_up() {
        assert!(!enabled(
            BaseItemKind::Playlist,
            None,
            None,
            ItemUpdateType::None,
            true
        ));
        for update in [
            ItemUpdateType::MetadataImport,
            ItemUpdateType::ImageUpdate,
            ItemUpdateType::MetadataDownload,
            ItemUpdateType::MetadataEdit,
        ] {
            assert!(
                enabled(BaseItemKind::Playlist, None, None, update, true),
                "{update:?}"
            );
        }
    }

    fn playlist(path: &str) -> BaseItemEntity {
        BaseItemEntity {
            type_: "MediaBrowser.Controller.Playlists.Playlist".into(),
            path: Some(path.to_owned()),
            ..Default::default()
        }
    }

    /// `PlaylistXmlSaver.GetSavePath`: a folder's `playlist.xml`; a playlist
    /// FILE (an extension, no directory) has none here (its `<name>.xml`
    /// sidecar is an accepted divergence: see the module doc), while a folder
    /// whose name has a dot is still a folder.
    #[test]
    fn a_playlist_folder_has_a_playlist_xml_and_a_file_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dotted = dir.path().join("Mix.2024");
        std::fs::create_dir(&dotted).expect("mkdir");
        let dotted = dotted.to_string_lossy().into_owned();
        assert_eq!(
            xml_path(&playlist(&dotted)),
            Some(PathBuf::from(format!("{dotted}/playlist.xml")))
        );
        assert_eq!(
            xml_path(&playlist("/data/playlists/Road Trip")),
            Some(PathBuf::from("/data/playlists/Road Trip/playlist.xml"))
        );
        assert_eq!(xml_path(&playlist("/media/music/Road Trip.m3u")), None);
        // The scan's "Replace all metadata" skips the file only where the
        // saver writes it.
        let mut library = LibraryOptions::default();
        let folder = playlist("/media/music/Mix [playlist]");
        assert!(!saves_metadata(&folder, Some(&library), None));
        library.save_local_metadata = true;
        assert!(saves_metadata(&folder, Some(&library), None));
        assert!(!saves_metadata(
            &playlist("/media/music/Mix.m3u"),
            Some(&library),
            None
        ));
    }
}
