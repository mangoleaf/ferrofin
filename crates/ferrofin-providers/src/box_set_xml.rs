//! A box set's `collection.xml` — port of `BoxSetXmlProvider` (the local
//! reader, `BaseXmlProvider`) and `BoxSetXmlSaver` (`BaseXmlSaver`), with the
//! save gate `ProviderManager.IsSaverEnabledForItem` applies to it.
//!
//! The document itself is [`crate::local_xml`]'s. This module decides which
//! file, whether it is read, written, and where its members count:
//!
//! - **Read** in the box set's refresh, as its local metadata provider: the
//!   file in the box set's folder, when the box set supports local metadata
//!   (a local path, no channel). Its change monitor (`HasChanged`, no
//!   tolerance) is the refresh plan's [`LocalMetadataFormat::Xml`].
//! - **Members** (`BoxSetMetadataService.MergeData`, v12.4): the file's
//!   `<CollectionItems>` join the box set's links only for a box set outside
//!   the data directory. For one in it (`{data}/collections`) the
//!   `LinkedChildren` table is the source of truth, and its file's members
//!   are never read (owner decision 2026-11-14: follow v12.4).
//! - **Write** after any update from `MetadataDownload` up: a refresh that
//!   downloaded metadata or was forced, a metadata edit, every member added
//!   or removed. `BoxSet.IsSaveLocalMetadataEnabled()` is always true
//!   upstream; here that holds only for a box set whose folder is in the
//!   server's own directories (owner decision 2026-10-17, CLAUDE.md "Current
//!   scope"). A box-set folder inside a media library follows the library's
//!   "Save artwork into media folders" (`SaveLocalMetadata`), so nothing is
//!   written into a media folder the library did not open to it.
//!
//! ACCEPTED DIVERGENCE (owner decision 2026-11-19, gl-1870): an edit or a
//! member change (`MetadataEdit`) never rewrites a `collection.xml` already
//! in a media folder the library did not open. In a library with no saver
//! list upstream writes a box set's file whatever `SaveLocalMetadata` says
//! (the override above), and for any other item `IsSaverEnabledForItem`
//! rewrites a saver's existing file on an edit.
//! A file left in place can be read again at a refresh until the library
//! opts in.
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

/// The file's name in the box set's folder (`BoxSetXmlProvider.GetXmlFile`).
const FILE_NAME: &str = "collection.xml";

/// `LinkedChildType.Manual`, the links the saver writes.
const MANUAL_LINK: i32 = 0;

/// A box set's `collection.xml` on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalXmlFile {
    /// The file.
    pub path: PathBuf,
    /// Its last write time, which `HasChanged` compares with the box set's
    /// `DateLastSaved`.
    pub mtime: DateTime<Utc>,
}

/// The `collection.xml` path of a box set that supports local metadata
/// (`SupportsLocalMetadata`: a local path, not a channel item), else `None`.
#[must_use]
pub fn xml_path(row: &BaseItemEntity) -> Option<PathBuf> {
    if BaseItemKind::from_stored_type_name(&row.type_) != Some(BaseItemKind::BoxSet) {
        return None;
    }
    let path = Path::new(row.path.as_deref().filter(|path| !path.is_empty())?);
    let channel = row.channel_id.as_deref().is_some_and(|channel| {
        !channel.is_empty() && Uuid::parse_str(channel).map_or(true, |id| !id.is_nil())
    });
    (path.is_absolute() && !channel).then(|| path.join(FILE_NAME))
}

/// The box set's `collection.xml` when it exists (`GetXmlFile` through the
/// directory service, then the stat `HasChanged` makes).
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

/// `BoxSetXmlProvider.GetMetadata`: the file parsed with the box set's
/// external id keys. `None` (`HasMetadata` false) when it cannot be read; a
/// document that does not parse is logged and is no metadata either, as
/// upstream's `XmlException` is.
pub async fn read(path: &Path, provider_keys: &[String]) -> Option<LocalContainerXml> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "could not read the box set's collection.xml");
            return None;
        }
    };
    let text = ferrofin_util::stream_extensions::decode_stream_text(bytes);
    let parsed = parse_container_xml(&text, BaseItemKind::BoxSet, provider_keys);
    if parsed.is_none() {
        tracing::warn!(path = %path.display(), "the box set's collection.xml is not well-formed XML; it is not read");
    }
    parsed
}

/// Whether a box set at `path` takes its file's members: it lies outside
/// `data_path` (`!FileSystem.ContainsSubPath(DataPath, targetPath)`).
#[must_use]
pub fn takes_members_from_file(data_path: &str, path: &Path) -> bool {
    data_path.is_empty() || !path.starts_with(data_path) || path == Path::new(data_path)
}

/// `IsSaverEnabledForItem(saver, item, libraryOptions, update, false)` for
/// `BoxSetXmlSaver`, past `SupportsLocalMetadata` ([`xml_path`]):
///
/// - `IsEnabledFor`: an update from `MetadataDownload` up;
/// - a library that lists its metadata savers must list [`SAVER_NAME`];
/// - otherwise the server's options for box sets must not disable it, and
///   `IsSaveLocalMetadataEnabled()` must hold — `own_directory` (the
///   override, see the module doc) or the library's `SaveLocalMetadata` —
///   whether or not the file already exists (the module doc's divergence).
fn enabled(
    library: Option<&LibraryOptions>,
    global: Option<&MetadataOptions>,
    update: ItemUpdateType,
    own_directory: bool,
) -> bool {
    if !matches!(
        update,
        ItemUpdateType::MetadataDownload | ItemUpdateType::MetadataEdit
    ) {
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

/// The registered `BoxSetXmlSaver`, reading the box set through the
/// repositories. One save at a time, so two concurrent updates never
/// interleave their replacements of a file.
pub struct BoxSetXmlSaver {
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

impl BoxSetXmlSaver {
    /// Creates the saver over the repositories a box set's document is made
    /// from.
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

    /// The server's data directory: a box set outside it takes its file's
    /// members ([`takes_members_from_file`]).
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
        if xml_path(row).is_none() {
            return Ok(false);
        }
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
        let global =
            crate::library_options::global_metadata_options(&server.metadata_options, "BoxSet");
        let own_directory = row.path.as_deref().is_some_and(|folder| {
            crate::image_save::in_server_directories(
                self.configuration.application_paths().as_ref(),
                Path::new(folder),
            )
        });
        Ok(enabled(library, global, update, own_directory))
    }

    /// `ProviderManager.SaveMetadataAsync` with this saver: writes the box
    /// set's `collection.xml` when [`is_enabled_for`](Self::is_enabled_for)
    /// says so. Returns whether a file was written, after which the caller
    /// stamps the box set's `DateLastSaved` past the file's write time
    /// (`LibraryManager.UpdateItemsAsync`), so its own write is no change to
    /// read back.
    ///
    /// # Errors
    /// A repository, configuration or filesystem failure. The caller logs it
    /// without undoing the metadata change, as upstream does.
    pub async fn save(&self, item_id: Uuid, update: ItemUpdateType) -> Result<bool, ServiceError> {
        if !matches!(
            update,
            ItemUpdateType::MetadataDownload | ItemUpdateType::MetadataEdit
        ) {
            return Ok(false);
        }
        // Every item's metadata save comes here: only a box set's waits on
        // the lock, and reads its row again under it, so the last writer
        // writes the newest row.
        if self
            .items
            .retrieve_item(item_id)
            .await?
            .as_ref()
            .and_then(xml_path)
            .is_none()
        {
            return Ok(false);
        }
        let _guard = self.lock.lock().await;
        let Some(row) = self.items.retrieve_item(item_id).await? else {
            return Ok(false);
        };
        let Some(path) = xml_path(&row) else {
            return Ok(false);
        };
        if !self.is_enabled_for(&row, update).await? {
            return Ok(false);
        }
        let xml = crate::local_xml::save_collection_xml(&self.document(item_id, row).await?);
        let _write = SelfWrite::begin(self.self_writes.as_ref(), path.to_string_lossy());
        tokio::task::spawn_blocking(move || {
            ferrofin_util::file_helper::atomic_write_media(&path, xml.as_bytes())
        })
        .await
        .map_err(|err| ServiceError::backend(err.to_string()))?
        .map_err(|err| ServiceError::backend(format!("write collection.xml: {err}")))?;
        Ok(true)
    }

    /// The box set as `BaseXmlSaver` writes it: its row, provider ids,
    /// locked fields and people, and its manual links with the path each
    /// resolves to (`AddLinkedChildren`).
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
        Ok(LocalContainerXml {
            item: MetadataResult {
                item: row,
                people: Some(people),
                provider_ids,
                locked_fields,
            },
            playlist_media_type: None,
            children,
        })
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
            assert!(!enabled(None, None, update, true), "{update:?}");
        }
        for update in [
            ItemUpdateType::MetadataDownload,
            ItemUpdateType::MetadataEdit,
        ] {
            assert!(enabled(None, None, update, true), "{update:?}");
        }
    }

    /// A library that lists its savers decides alone; otherwise the
    /// server-wide disable list, then `IsSaveLocalMetadataEnabled`.
    #[test]
    fn a_library_listing_its_savers_must_list_emby_xml() {
        let mut library = LibraryOptions {
            save_local_metadata: true,
            metadata_savers: Some(vec!["Nfo".into()]),
            ..Default::default()
        };
        let download = ItemUpdateType::MetadataDownload;
        assert!(!enabled(Some(&library), None, download, true));
        library.metadata_savers = Some(vec!["emby xml".into()]);
        assert!(enabled(Some(&library), None, download, false));
        library.metadata_savers = None;
        let global = MetadataOptions {
            disabled_metadata_savers: vec!["Emby Xml".into()],
            ..Default::default()
        };
        assert!(!enabled(Some(&library), Some(&global), download, true));
    }

    /// The owner rule: the always-true override holds in the server's own
    /// directories; a box set folder in a media library follows its
    /// library's `SaveLocalMetadata`, an edit included, whether or not its
    /// file is already there (gl-1870).
    #[test]
    fn outside_the_servers_directories_the_library_decides() {
        let mut library = LibraryOptions {
            save_local_metadata: false,
            ..Default::default()
        };
        let download = ItemUpdateType::MetadataDownload;
        let edit = ItemUpdateType::MetadataEdit;
        assert!(enabled(Some(&library), None, download, true));
        assert!(!enabled(Some(&library), None, download, false));
        assert!(!enabled(Some(&library), None, edit, false));
        assert!(!enabled(None, None, download, false));
        library.save_local_metadata = true;
        assert!(enabled(Some(&library), None, download, false));
    }
}
