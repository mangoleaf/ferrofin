//! Box sets the library scan resolves off disk — port of `BoxSetResolver`
//! (`Emby.Server.Implementations/Library/Resolvers/Movies/BoxSetResolver.cs`)
//! for the Collections (`boxsets`) library, and of the validation that keeps
//! the library's children in step with its folder
//! (`Folder.ValidateChildrenInternal2`).
//!
//! A directory in the folder box sets are made in (`{data}/collections`)
//! whose name holds `[boxset]` (any case; the marker is cut from the name),
//! or that holds a `collection.xml`, is a box set. Under the data directory
//! it is no legacy box set (`BoxSet.IsLegacyBoxSet`), so nothing below it is
//! walked. A scan of the library stores a box set for each such folder no
//! box set holds, with the id `Folder.AddChild` derives from its path and the
//! `tmdbid` its name carries, and removes the box sets in the folder whose
//! own folder is gone. A box set with no folder (one an older Ferrofin made
//! that the boot repair, `adoption_repairs::rekey_pathless_box_sets`, could
//! not give a folder) has nothing to resolve and is left alone. Then each box set is refreshed (`refresh_box_sets`), the
//! new ones first fetching their metadata.
//!
//! ACCEPTED DIVERGENCE (upstream bug, not ported): validation removes a box
//! set whose folder is still there but no longer resolves, and an ignored
//! name never resolves: a collection named `.Heist` is made with the folder
//! `.Heist [boxset]` (`CreateCollectionAsync`), which `IgnorePatterns`
//! (`**/.*`) hides, so the next scan deletes it with its members. Here only a
//! box set whose folder is gone is removed.
//!
//! ACCEPTED DIVERGENCE (upstream bug, not ported): validation matches a
//! folder to its box set by id alone, so a box set whose folder derives
//! another id than it holds (a database adopted from a server whose data
//! directory was elsewhere, or a data directory that moved) is removed and
//! made again under the new id, and its members, which live only in the
//! database, are lost. Here a box set already holding the folder keeps it,
//! whatever its id.
//!
//! TODO(parity, open work item): `collection.xml` is still neither read
//! (`BoxSetXmlProvider`) nor written (`BoxSetXmlSaver`); a folder holding
//! one resolves all the same, but its title, overview and locks are not
//! applied. Port path: gl-1711. And the resolver runs only here, in the
//! folder box sets are made in: not in other libraries, not for legacy box
//! sets with children on disk, not below a plain folder. Port path: gl-1712.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use ferrofin_model::data::BaseItemKind;
use ferrofin_model::io::FileSystemEntryType;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::filesystem::FileSystem;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::ItemRepository;
use uuid::Uuid;

use super::{LibraryScanner, ScanRun, parse_id};

/// `BoxSetResolver`'s directory marker, matched case-insensitively.
const FOLDER_MARKER: &str = "[boxset]";

/// What one listing of a location says about the folders in it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct BoxSetFolders {
    /// The folders that resolve as box sets.
    pub(super) resolved: Vec<PathBuf>,
    /// Every entry listed, whether it resolves or not: a folder behind a
    /// dangling link, or one whose type could not be read, is still there.
    pub(super) present: HashSet<PathBuf>,
}

/// Lists `location` and sorts its folders as `BoxSetResolver` would: those
/// whose name holds the marker, then those holding a `collection.xml`
/// (`ContainsFileSystemEntryByName`, an exact name). What `IgnoreFile` hides
/// is not resolved, as for every listing a resolver sees; a `collection.xml`
/// that cannot be looked for is not there.
///
/// # Errors
/// The listing's own error: a location that cannot be listed is not known to
/// be empty.
pub(super) fn box_set_folders(
    file_system: &dyn FileSystem,
    location: &str,
) -> Result<BoxSetFolders, ServiceError> {
    let entries = file_system.try_get_file_system_entries(location)?;
    let mut dot_ignore = crate::dot_ignore::DotIgnoreRule::default();
    dot_ignore.begin_listing();
    let mut folders = BoxSetFolders::default();
    for entry in entries {
        let path = PathBuf::from(&entry.path);
        let holds_xml = || std::fs::symlink_metadata(path.join("collection.xml")).is_ok();
        if entry.type_ == FileSystemEntryType::Directory
            && !crate::resolvers::should_ignore_path(&entry.path)
            && !dot_ignore.should_ignore(&entry.path, true)
            && crate::resolvers::marked_folder_name(&entry.path, FOLDER_MARKER, holds_xml).is_some()
        {
            folders.resolved.push(path.clone());
        }
        folders.present.insert(path);
    }
    Ok(folders)
}

/// The box sets of `held` whose folder is gone: not in the listing's
/// `present` entries, and `missing` once more just before the delete, so a
/// folder made since the listing (a collection deleted and made again under
/// its name) keeps its box set. A folder whose state cannot be read is not
/// missing.
fn gone_box_sets(
    held: &[(Uuid, PathBuf)],
    present: &HashSet<PathBuf>,
    missing: impl Fn(&std::path::Path) -> bool,
) -> Vec<Uuid> {
    held.iter()
        .filter(|(_, path)| !present.contains(path) && missing(path))
        .map(|(id, _)| *id)
        .collect()
}

/// The name a resolved box set folder gives its box set: `BoxSetResolver`
/// trims it whether or not the marker was cut, and `EnsureName` gives a name
/// left empty the folder's own.
pub(crate) fn box_set_name(folder: &str) -> Option<String> {
    let name = crate::resolvers::marked_folder_name(folder, FOLDER_MARKER, || true)?;
    let trimmed = name.trim();
    Some(if trimmed.is_empty() || trimmed.len() == name.len() {
        name
    } else {
        trimmed.to_owned()
    })
}

impl LibraryScanner {
    /// Resolves the box sets of each of the Collections libraries'
    /// `locations` (expanded) a scan covers: all of them, or with `scope`
    /// (gl-796) those at or under one of its roots. A scan scoped to one box
    /// set validates that box set's children, of which it has none, so it
    /// resolves nothing. The collection manager stores the new box sets
    /// (`CollectionManager::resolve_box_set_folders`) and says which
    /// location is the one box sets are made in; only there are box sets
    /// removed. Best-effort: a failure is logged and the refresh goes on.
    pub(super) async fn resolve_box_sets(
        &self,
        items: &dyn ItemRepository,
        locations: &[PathBuf],
        scope: Option<&[String]>,
        run: ScanRun<'_>,
    ) {
        let Some(collections) = self.collections.get().and_then(std::sync::Weak::upgrade) else {
            return;
        };
        let due: Vec<&PathBuf> = locations
            .iter()
            .filter(|location| {
                scope.is_none_or(|roots| roots.iter().any(|root| location.starts_with(root)))
            })
            .collect();
        if due.is_empty() {
            return;
        }
        let rows = match items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::BoxSet],
                ..InternalItemsQuery::default()
            })
            .await
        {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(%err, "could not list the box sets to resolve");
                return;
            }
        };
        for location in due {
            if run.cancel.is_cancelled() {
                return;
            }
            let listed = {
                let file_system = Arc::clone(&self.file_system);
                let location = location.to_string_lossy().into_owned();
                tokio::task::spawn_blocking(move || {
                    box_set_folders(file_system.as_ref(), &location)
                })
                .await
            };
            let folders = match listed {
                Ok(Ok(folders)) => folders,
                Ok(Err(err)) => {
                    tracing::warn!(%err, path = %location.display(), "could not list the collections folder; its box sets stay as they are");
                    continue;
                }
                Err(err) => {
                    tracing::warn!(%err, path = %location.display(), "could not list the collections folder; its box sets stay as they are");
                    continue;
                }
            };
            // The box sets directly in the location, by their folder.
            let held: Vec<(Uuid, PathBuf)> = rows
                .iter()
                .filter_map(|row| {
                    let path = PathBuf::from(self.virtual_paths.expand(row.path.as_deref()?));
                    (path.parent() == Some(location.as_path()))
                        .then(|| Some((parse_id(&row.id)?, path)))
                        .flatten()
                })
                .collect();
            let held_paths: HashSet<&PathBuf> = held.iter().map(|(_, path)| path).collect();
            let new: Vec<String> = folders
                .resolved
                .iter()
                .filter(|folder| !held_paths.contains(folder))
                .map(|folder| folder.to_string_lossy().into_owned())
                .collect();
            let added = match collections
                .resolve_box_set_folders(&location.to_string_lossy(), &new)
                .await
            {
                Ok(Some(added)) => added,
                // Not the folder box sets are made in: nothing is resolved
                // there, so nothing there is removed either.
                Ok(None) => continue,
                Err(err) => {
                    tracing::warn!(%err, path = %location.display(), "could not store the box sets of the collections folder");
                    continue;
                }
            };
            if !added.is_empty() {
                tracing::info!(path = %location.display(), added = added.len(), "stored box sets for their folders");
                self.announce_added(items, &added).await;
            }
            let gone = gone_box_sets(&held, &folders.present, |path| {
                std::fs::symlink_metadata(path)
                    .is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound)
            });
            if gone.is_empty() {
                continue;
            }
            match self.delete_announced(&gone).await {
                Ok(deleted) => {
                    tracing::info!(path = %location.display(), removed = deleted.len(), "removed box sets whose folder is gone");
                }
                Err(err) => {
                    tracing::warn!(%err, path = %location.display(), "could not remove box sets whose folder is gone");
                }
            }
        }
    }

    /// Announces the rows `ids` as added (`ItemAdded`), as a scan announces
    /// what it created. No-op without a change notifier.
    async fn announce_added(&self, items: &dyn ItemRepository, ids: &[Uuid]) {
        let Some(notifier) = &self.change_notifier else {
            return;
        };
        match items.retrieve_items(ids).await {
            Ok(rows) => notifier.record_added(&rows),
            Err(err) => {
                tracing::warn!(%err, count = ids.len(), "could not read the new box sets; their addition is not announced");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The marker anywhere in the name, in any case, is cut and the rest
    /// trimmed; a name left empty is the folder's own.
    #[rstest::rstest]
    #[case::suffix("/c/Saga [boxset]", "Saga")]
    #[case::prefix_any_case("/c/[BoxSet] Saga", "Saga")]
    #[case::twice("/c/[boxset]Saga[boxset]", "Saga")]
    #[case::tmdb_attribute_kept("/c/Saga [tmdbid-10] [boxset]", "Saga [tmdbid-10]")]
    #[case::only_the_marker("/c/[boxset]", "[boxset]")]
    #[case::plain("/c/Saga", "Saga")]
    #[case::plain_trimmed("/c/Saga ", "Saga")]
    fn a_box_set_is_named_after_its_folder(#[case] folder: &str, #[case] name: &str) {
        assert_eq!(box_set_name(folder).as_deref(), Some(name));
    }

    /// The marker folders and the `collection.xml` folders resolve; a plain
    /// folder, a file, a dangling link, a hidden folder and an ignored
    /// `metadata` folder holding a `collection.xml` do not. Every entry is
    /// present.
    #[cfg(unix)]
    #[test]
    fn the_listing_takes_marker_and_collection_xml_folders() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        for dir in [
            "Heist [boxset]",
            "Saga",
            "Plain",
            ".Hidden [boxset]",
            "metadata",
        ] {
            std::fs::create_dir_all(root.join(dir)).expect("dir");
        }
        std::fs::write(root.join("Saga/collection.xml"), b"<Item/>").expect("xml");
        std::fs::write(root.join("metadata/collection.xml"), b"<Item/>").expect("xml");
        std::os::unix::fs::symlink(root.join("nowhere"), root.join("Linked [boxset]"))
            .expect("dangling link");
        std::fs::write(root.join("Plain/Collection.XML"), b"<Item/>").expect("xml");
        std::fs::write(root.join("Loose [boxset]"), b"").expect("file");
        let file_system = crate::file_system::FerrofinFileSystem::new();
        let mut folders = box_set_folders(&file_system, &root.to_string_lossy()).expect("listed");
        folders.resolved.sort();
        assert_eq!(
            folders,
            BoxSetFolders {
                resolved: vec![root.join("Heist [boxset]"), root.join("Saga")],
                present: [
                    ".Hidden [boxset]",
                    "Heist [boxset]",
                    "Linked [boxset]",
                    "Loose [boxset]",
                    "Plain",
                    "Saga",
                    "metadata",
                ]
                .iter()
                .map(|dir| root.join(dir))
                .collect(),
            }
        );
    }

    /// Only a box set whose folder is neither listed nor found again is gone.
    #[test]
    fn a_box_set_is_gone_only_when_its_folder_is() {
        let held = [
            (Uuid::from_u128(1), PathBuf::from("/c/Listed [boxset]")),
            (Uuid::from_u128(2), PathBuf::from("/c/Remade [boxset]")),
            (Uuid::from_u128(3), PathBuf::from("/c/Gone [boxset]")),
        ];
        let present = HashSet::from([PathBuf::from("/c/Listed [boxset]")]);
        let gone = gone_box_sets(&held, &present, |path| {
            path != std::path::Path::new("/c/Remade [boxset]")
        });
        assert_eq!(gone, vec![Uuid::from_u128(3)]);
    }

    /// A location that cannot be listed is an error, never an empty folder.
    #[test]
    fn a_missing_location_is_not_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_system = crate::file_system::FerrofinFileSystem::new();
        assert!(box_set_folders(&file_system, &tmp.path().join("gone").to_string_lossy()).is_err());
    }
}
