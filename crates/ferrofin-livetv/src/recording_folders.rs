//! The recording folders as libraries: port of `RecordingsManager`'s
//! `GetRecordingFolders`, `CreateRecordingFolders` and
//! `RemovePathFromLibraryAsync`.
//!
//! Each folder recordings are written to becomes a library, so what is
//! recorded is scanned like any other media. A folder becomes one only when its
//! directory exists and no library holds the location already, so an adopted
//! Jellyfin database keeps the libraries it has. `MediaLocationsCreated` lists
//! the locations made this way: one that stops being a recording folder leaves
//! the library again, and nothing else ever does.
//!
//! ## Adoption
//!
//! Adoption carries `MediaLocationsCreated` over but not the three recording
//! paths (`LIVETV_XML_DENY` in `ferrofin-core`), so an adopted list can name a
//! location that is not a recording folder here, only because its path was
//! denied. Upstream's rule would remove that library, recordings and watch
//! state included, on the first boot. So the first pass on a data directory
//! keeps the entries that are recording folders and forgets the rest, which
//! stay ordinary libraries. Every later entry was made here, and upstream's
//! rule applies to it.

use std::path::{Path, PathBuf};

use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::entities_media::VirtualFolderInfo;
use ferrofin_model::live_tv::LiveTvOptions;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::RecordingLibrary;

use crate::dvr::default_recording_path;

/// Stamps a data directory whose inherited `MediaLocationsCreated` has been
/// settled, under `{data}/livetv`. Its presence is all that is read.
const SETTLED_MARKER: &str = "media-locations-settled";

/// `Directory.Exists`.
async fn is_directory(path: &str) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|meta| meta.is_dir())
}

/// A configured custom path, as `GetRecordingFolders` takes one: set, not the
/// default path (`OrdinalIgnoreCase`) and an existing directory.
async fn custom_folder(path: Option<&str>, default: &str) -> Option<String> {
    let path = path.filter(|path| !path.trim().is_empty())?;
    (!path.eq_ignore_ascii_case(default) && is_directory(path).await).then(|| path.to_owned())
}

/// Port of `RecordingsManager.GetRecordingFolders`.
pub(crate) async fn recording_folders(
    options: &LiveTvOptions,
    data_dir: &Path,
) -> Vec<VirtualFolderInfo> {
    let default = default_recording_path(options, data_dir)
        .to_string_lossy()
        .into_owned();
    let folder = |name: &str, location: String, collection_type| VirtualFolderInfo {
        name: Some(name.to_owned()),
        locations: vec![location],
        collection_type,
        ..VirtualFolderInfo::default()
    };
    let mut folders = Vec::with_capacity(3);
    if is_directory(&default).await {
        folders.push(folder("Recordings", default.clone(), None));
    }
    if let Some(path) = custom_folder(options.movie_recording_path.as_deref(), &default).await {
        folders.push(folder(
            "Recorded Movies",
            path,
            Some(CollectionTypeOptions::movies),
        ));
    }
    if let Some(path) = custom_folder(options.series_recording_path.as_deref(), &default).await {
        folders.push(folder(
            "Recorded Shows",
            path,
            Some(CollectionTypeOptions::tvshows),
        ));
    }
    folders
}

/// `IFileSystem.AreEqual` on Linux: ordinal, past one trailing separator
/// (`Path.TrimEndingDirectorySeparator`, which leaves a root alone).
fn same_path(a: &str, b: &str) -> bool {
    let trimmed = |path: &'_ str| -> String {
        match path.strip_suffix('/') {
            Some(rest) if !rest.is_empty() => rest.to_owned(),
            _ => path.to_owned(),
        }
    };
    !a.trim().is_empty() && !b.trim().is_empty() && trimmed(a) == trimmed(b)
}

/// Whether `path` is a location of `folders` (`Except` compares ordinally).
fn is_recording_folder(folders: &[VirtualFolderInfo], path: &str) -> bool {
    folders
        .iter()
        .flat_map(|folder| &folder.locations)
        .any(|location| location == path)
}

/// One pass of `CreateRecordingFolders` over the current options.
pub(crate) struct RecordingFolders<'a> {
    /// The library side of the pass.
    pub library: &'a dyn RecordingLibrary,
    /// The server data directory, for the default path and the marker.
    pub data_dir: &'a Path,
}

impl RecordingFolders<'_> {
    /// `{data}/livetv/media-locations-settled`.
    fn marker(&self) -> PathBuf {
        self.data_dir.join("livetv").join(SETTLED_MARKER)
    }

    /// Whether a pass has settled what this data directory inherited.
    pub(crate) async fn is_settled(&self) -> bool {
        tokio::fs::try_exists(self.marker()).await.unwrap_or(false)
    }

    /// The first pass on a data directory: see the [module docs](self). Keeps
    /// the entries of `created` that are recording folders.
    pub(crate) fn settle_inherited_locations(
        created: Vec<String>,
        folders: &[VirtualFolderInfo],
    ) -> Vec<String> {
        created
            .into_iter()
            .filter(|path| {
                let current = is_recording_folder(folders, path);
                if !current {
                    tracing::info!(
                        path,
                        "live tv: an inherited recording location is not a recording folder \
                         here; it stays an ordinary library"
                    );
                }
                current
            })
            .collect()
    }

    /// Stamps the data directory as settled. The list is safe by now, so a
    /// marker that cannot be written costs a repeat of the check and nothing
    /// else.
    pub(crate) async fn mark_settled(&self) {
        let marker = self.marker();
        let written = match marker.parent() {
            Some(parent) => tokio::fs::create_dir_all(parent).await,
            None => Ok(()),
        };
        if let Err(error) = match written {
            Ok(()) => tokio::fs::write(&marker, b"").await,
            Err(error) => Err(error),
        } {
            tracing::warn!(
                path = %marker.display(),
                %error,
                "live tv: could not mark the recording locations settled"
            );
        }
    }

    /// The first half of `CreateRecordingFolders`: adds a library for each
    /// folder with a location no library holds, and answers those locations.
    ///
    /// The existing locations are read once, as upstream reads them, so two
    /// folders on one path each get a library. A failed add is logged and its
    /// locations still count as added, so the next pass tries again and a
    /// later folder is not held up.
    pub(crate) async fn add_missing(
        &self,
        folders: &[VirtualFolderInfo],
    ) -> Result<Vec<String>, ServiceError> {
        let existing: Vec<String> = self
            .library
            .virtual_folders()
            .await?
            .into_iter()
            .flat_map(|folder| folder.locations)
            .collect();
        let mut added = Vec::new();
        for folder in folders {
            let to_create: Vec<String> = folder
                .locations
                .iter()
                .filter(|location| !existing.iter().any(|path| same_path(path, location)))
                .cloned()
                .collect();
            if to_create.is_empty() {
                continue;
            }
            let options = LibraryOptions {
                path_infos: to_create
                    .iter()
                    .map(|path| MediaPathInfo { path: path.clone() })
                    .collect(),
                ..LibraryOptions::default()
            };
            let name = folder.name.as_deref().unwrap_or_default();
            if let Err(error) = self
                .library
                .add_virtual_folder(name, folder.collection_type, &options)
                .await
            {
                tracing::error!(name, %error, "live tv: could not add a recording library");
            }
            added.extend(to_create);
        }
        Ok(added)
    }

    /// `RemovePathFromLibraryAsync`: takes `path` out of every library holding
    /// it, removing a library it was the only location of.
    pub(crate) async fn remove_from_libraries(&self, path: &str) -> Result<(), ServiceError> {
        tracing::debug!(
            path,
            "live tv: removing a recording location from the library"
        );
        let mut requires_refresh = false;
        for folder in self.library.virtual_folders().await? {
            if !folder
                .locations
                .iter()
                .any(|location| location.eq_ignore_ascii_case(path))
            {
                continue;
            }
            let name = folder.name.as_deref().unwrap_or_default();
            if folder.locations.len() == 1 {
                if let Err(error) = self.library.remove_virtual_folder(name).await {
                    tracing::error!(name, %error, "live tv: could not remove a recording library");
                }
            } else {
                match self.library.remove_media_path(name, path).await {
                    Ok(()) => requires_refresh = true,
                    Err(error) => {
                        tracing::error!(name, path, %error, "live tv: could not remove a recording location");
                    }
                }
            }
        }
        if requires_refresh {
            self.library.queue_library_scan().await?;
        }
        Ok(())
    }
}

/// `MediaLocationsCreated` after a pass, and the locations to remove from the
/// libraries: `created` entries that are no longer recording folders are
/// removed, and those added are appended (`Except` then `Distinct`). `None`
/// when nothing changed, which upstream does not save.
pub(crate) fn updated_locations(
    created: &[String],
    added: Vec<String>,
    folders: &[VirtualFolderInfo],
) -> Option<(Vec<String>, Vec<String>)> {
    let mut to_remove: Vec<String> = Vec::new();
    for path in created {
        if !is_recording_folder(folders, path) && !to_remove.contains(path) {
            to_remove.push(path.clone());
        }
    }
    if added.is_empty() && to_remove.is_empty() {
        return None;
    }
    let mut locations: Vec<String> = Vec::with_capacity(created.len() + added.len());
    for path in created.iter().cloned().chain(added) {
        if !to_remove.contains(&path) && !locations.contains(&path) {
            locations.push(path);
        }
    }
    Some((locations, to_remove))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn options(default: Option<&str>, movies: Option<&str>, series: Option<&str>) -> LiveTvOptions {
        LiveTvOptions {
            recording_path: default.map(ToOwned::to_owned),
            movie_recording_path: movies.map(ToOwned::to_owned),
            series_recording_path: series.map(ToOwned::to_owned),
            ..LiveTvOptions::default()
        }
    }

    fn names(folders: &[VirtualFolderInfo]) -> Vec<(&str, Option<CollectionTypeOptions>)> {
        folders
            .iter()
            .map(|folder| (folder.name.as_deref().unwrap(), folder.collection_type))
            .collect()
    }

    fn folder(location: &str) -> VirtualFolderInfo {
        VirtualFolderInfo {
            locations: vec![location.to_owned()],
            ..VirtualFolderInfo::default()
        }
    }

    #[tokio::test]
    async fn a_folder_exists_only_once_its_directory_does() {
        let tmp = tempfile::tempdir().unwrap();
        let movies = tmp.path().join("movies");
        let shows = tmp.path().join("shows");
        let configured = options(None, movies.to_str(), shows.to_str());

        let value = recording_folders(&configured, tmp.path()).await;
        assert!(value.is_empty(), "{value:?}");

        std::fs::create_dir_all(tmp.path().join("livetv/recordings")).unwrap();
        std::fs::create_dir_all(&movies).unwrap();
        std::fs::create_dir_all(&shows).unwrap();
        let folders = recording_folders(&configured, tmp.path()).await;
        assert_eq!(
            names(&folders),
            [
                ("Recordings", None),
                ("Recorded Movies", Some(CollectionTypeOptions::movies)),
                ("Recorded Shows", Some(CollectionTypeOptions::tvshows)),
            ]
        );
        assert_eq!(
            folders[0].locations,
            [tmp.path().join("livetv/recordings").display().to_string()]
        );
        assert_eq!(folders[1].locations, [movies.display().to_string()]);
        assert_eq!(folders[2].locations, [shows.display().to_string()]);
    }

    #[tokio::test]
    async fn the_configured_recording_path_replaces_the_default_one() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("dvr");
        std::fs::create_dir_all(&root).unwrap();
        let folders = recording_folders(&options(root.to_str(), None, None), tmp.path()).await;
        assert_eq!(names(&folders), [("Recordings", None)]);
        assert_eq!(folders[0].locations, [root.display().to_string()]);
    }

    /// A custom path equal to the default one is that folder, whatever its
    /// case, and a blank one is unset.
    #[rstest]
    #[case::same("DVR")]
    #[case::other_case("dvr")]
    #[case::blank("   ")]
    #[tokio::test]
    async fn a_custom_path_that_is_the_default_one_adds_no_folder(#[case] custom: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("DVR");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(tmp.path().join("dvr")).unwrap();
        let custom = if custom.trim().is_empty() {
            custom.to_owned()
        } else {
            tmp.path().join(custom).display().to_string()
        };
        let configured = options(root.to_str(), Some(&custom), Some(&custom));
        let folders = recording_folders(&configured, tmp.path()).await;
        assert_eq!(names(&folders), [("Recordings", None)]);
    }

    #[rstest]
    #[case("/a/b", "/a/b", true)]
    #[case("/a/b/", "/a/b", true)]
    #[case("/a/b", "/a/b//", false)]
    #[case("/", "/", true)]
    #[case("/a/B", "/a/b", false)]
    #[case("/a/b", "/a/c", false)]
    #[case("", "", false)]
    #[case("  ", "  ", false)]
    fn paths_compare_as_are_equal(#[case] a: &str, #[case] b: &str, #[case] same: bool) {
        assert_eq!(same_path(a, b), same);
    }

    #[test]
    fn a_pass_that_changes_nothing_is_not_saved() {
        let folders = [folder("/rec")];
        assert_eq!(updated_locations(&["/rec".into()], vec![], &folders), None);
        assert_eq!(updated_locations(&[], vec![], &[]), None);
    }

    #[test]
    fn locations_keep_their_order_with_the_new_ones_last() {
        let folders = [folder("/a"), folder("/b"), folder("/c")];
        let (locations, removed) = updated_locations(
            &["/a".to_owned(), "/gone".to_owned(), "/gone".to_owned()],
            vec!["/c".to_owned(), "/b".to_owned(), "/a".to_owned()],
            &folders,
        )
        .expect("changed");
        assert_eq!(locations, ["/a", "/c", "/b"]);
        assert_eq!(removed, ["/gone"]);
    }

    #[test]
    fn a_location_that_stopped_being_a_folder_is_compared_exactly() {
        // `Except` is ordinal, even though the removal below it is not.
        let (locations, removed) =
            updated_locations(&["/REC".to_owned()], vec![], &[folder("/rec")]).expect("changed");
        assert!(locations.is_empty(), "{locations:?}");
        assert_eq!(removed, ["/REC"]);
    }
}
