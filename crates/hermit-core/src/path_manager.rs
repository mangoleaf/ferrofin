//! [`HermitPathManager`] — the concrete [`PathManager`].
//!
//! Port of `Emby.Server.Implementations.Library.PathManager`. Computes the
//! on-disk locations for an item's derived/extracted data (subtitles,
//! attachments, trickplay tiles, chapter images) from the shared application
//! paths.
//!
//! Port departures dictated by the trait shape (the C# methods take a
//! `BaseItem`; the trait passes an `item_id` plus the item's `media_path`):
//! - `GetChapterImagePath` names the file `{DateModified.Ticks}_{position}.jpg`
//!   in C#. The trait carries no modified timestamp, so the file is named by the
//!   chapter position ticks alone (`{position}.jpg`). This still uniquely
//!   addresses a chapter image within an item's folder.
//! - `GetInternalMetadataPath` special-cases channel items; without the source
//!   type the non-channel layout (`{metadata}/library/{id2}/{id}`) is always
//!   used, which is the case for every item that has chapter images.
//! - The GUID id-string casing matches C#: attachment/subtitle/trickplay folders
//!   split on the hyphenated (`D`) form; the internal-metadata layout uses the
//!   dashless (`N`) form.
//!
//! Only the media-source id needs to be a GUID for subtitle/attachment paths
//! (C# `Guid.TryParse`); a non-GUID id yields `None`, matching the C# `null`.

use std::path::PathBuf;
use std::sync::Arc;

use hermit_traits::system::{PathManager, ServerApplicationPaths};
use uuid::Uuid;

use crate::app_paths::HermitServerApplicationPaths;

/// The concrete path manager.
///
/// Holds the shared application paths (as the concrete type, for the trickplay
/// path accessor that is not on the trait) plus the data path used for the
/// subtitle/attachment caches.
pub struct HermitPathManager {
    paths: Arc<HermitServerApplicationPaths>,
}

impl std::fmt::Debug for HermitPathManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitPathManager").finish_non_exhaustive()
    }
}

impl HermitPathManager {
    /// Creates a path manager over the given application paths.
    #[must_use]
    pub fn new(paths: Arc<HermitServerApplicationPaths>) -> Self {
        Self { paths }
    }

    /// The subtitle cache root (`{data}/subtitles`).
    fn subtitle_cache_path(&self) -> PathBuf {
        PathBuf::from(self.paths.data_path()).join("subtitles")
    }

    /// The attachment cache root (`{data}/attachments`).
    fn attachment_cache_path(&self) -> PathBuf {
        PathBuf::from(self.paths.data_path()).join("attachments")
    }

    /// The internal-metadata folder for an item
    /// (`{metadata}/library/{id2}/{idN}`), the non-channel layout.
    fn internal_metadata_path(&self, item_id: Uuid) -> PathBuf {
        let dashless = id_dashless(item_id);
        PathBuf::from(self.paths.internal_metadata_path())
            .join("library")
            .join(&dashless[..2])
            .join(&dashless)
    }

    /// The two-level folder under `root` for a media-source GUID
    /// (`{root}/{id[..2]}/{id-hyphenated}`), or `None` when the id is not a GUID.
    fn guid_folder(root: &std::path::Path, media_source_id: &str) -> Option<PathBuf> {
        let parsed = Uuid::parse_str(media_source_id).ok()?;
        let hyphenated = parsed.hyphenated().to_string();
        Some(root.join(&hyphenated[..2]).join(&hyphenated))
    }
}

impl PathManager for HermitPathManager {
    fn trickplay_directory(
        &self,
        item_id: Uuid,
        media_path: &str,
        save_with_media: bool,
    ) -> String {
        if save_with_media {
            // Alongside the media file: {containing-folder}/{stem}.trickplay
            let media = std::path::Path::new(media_path);
            let folder = media.parent().unwrap_or_else(|| std::path::Path::new(""));
            let stem = media.file_stem().map_or_else(
                || std::ffi::OsString::from("trickplay"),
                std::borrow::ToOwned::to_owned,
            );
            let mut file = PathBuf::from(stem);
            file.set_extension("trickplay");
            return folder.join(file).to_string_lossy().into_owned();
        }
        let hyphenated = id_hyphenated(item_id);
        PathBuf::from(self.paths.trickplay_path())
            .join(&hyphenated[..2])
            .join(&hyphenated)
            .to_string_lossy()
            .into_owned()
    }

    fn subtitle_path(
        &self,
        media_source_id: &str,
        stream_index: i32,
        extension: &str,
    ) -> Option<String> {
        let folder = self.subtitle_folder_path(media_source_id)?;
        Some(
            PathBuf::from(folder)
                .join(format!("{stream_index}{extension}"))
                .to_string_lossy()
                .into_owned(),
        )
    }

    fn subtitle_folder_path(&self, media_source_id: &str) -> Option<String> {
        Self::guid_folder(&self.subtitle_cache_path(), media_source_id)
            .map(|p| p.to_string_lossy().into_owned())
    }

    fn attachment_path(&self, media_source_id: &str, file_name: &str) -> Option<String> {
        let folder = self.attachment_folder_path(media_source_id)?;
        let safe = safe_leaf_file_name(file_name)?;
        Some(
            PathBuf::from(folder)
                .join(safe)
                .to_string_lossy()
                .into_owned(),
        )
    }

    fn attachment_folder_path(&self, media_source_id: &str) -> Option<String> {
        Self::guid_folder(&self.attachment_cache_path(), media_source_id)
            .map(|p| p.to_string_lossy().into_owned())
    }

    fn chapter_image_folder_path(&self, item_id: Uuid, _media_path: &str) -> String {
        self.internal_metadata_path(item_id)
            .join("chapters")
            .to_string_lossy()
            .into_owned()
    }

    fn chapter_image_path(
        &self,
        item_id: Uuid,
        media_path: &str,
        chapter_position_ticks: i64,
    ) -> String {
        // C# prefixes with the item's DateModified ticks; the trait does not
        // carry it, so the position ticks alone name the file.
        let folder = self.chapter_image_folder_path(item_id, media_path);
        PathBuf::from(folder)
            .join(format!("{chapter_position_ticks}.jpg"))
            .to_string_lossy()
            .into_owned()
    }

    fn extracted_data_paths(&self, item_id: Uuid, media_path: &str) -> Vec<String> {
        // C# uses the dashless ("N") id as the media-source id for the folder
        // lookups here.
        let media_source_id = id_dashless(item_id);
        let mut paths = Vec::new();
        if let Some(folder) = self.attachment_folder_path(&media_source_id) {
            paths.push(folder);
        }
        if let Some(folder) = self.subtitle_folder_path(&media_source_id) {
            paths.push(folder);
        }
        paths.push(self.trickplay_directory(item_id, media_path, false));
        if !media_path.is_empty() {
            paths.push(self.trickplay_directory(item_id, media_path, true));
        }
        paths.push(self.chapter_image_folder_path(item_id, media_path));
        paths
    }
}

/// The hyphenated (`D`) form of a GUID, matching C# `Id.ToString("D")`.
fn id_hyphenated(id: Uuid) -> String {
    id.hyphenated().to_string()
}

/// The dashless (`N`) form of a GUID, matching C# `Id.ToString("N")`.
fn id_dashless(id: Uuid) -> String {
    id.simple().to_string()
}

/// A defensive leaf-file-name sanitizer, mirroring `PathHelper.GetSafeLeafFileName`.
///
/// Rejects names that contain a path separator, are `.`/`..`, or are empty —
/// returning `None` so the caller drops the request (matching C# returning
/// `null`). A safe name is returned unchanged.
fn safe_leaf_file_name(file_name: &str) -> Option<String> {
    if file_name.is_empty() || file_name == "." || file_name == ".." {
        return None;
    }
    if file_name.contains('/') || file_name.contains('\\') || file_name.contains('\0') {
        return None;
    }
    Some(file_name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_paths::test_paths;

    fn manager() -> (tempfile::TempDir, HermitPathManager) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(tmp.path());
        (tmp, HermitPathManager::new(paths))
    }

    #[test]
    fn subtitle_and_attachment_paths_require_guid() {
        let (_tmp, m) = manager();
        let id = "0a1b2c3d-4e5f-6789-abcd-ef0123456789";
        let folder = m.subtitle_folder_path(id).expect("guid folder");
        assert!(folder.ends_with("subtitles/0a/0a1b2c3d-4e5f-6789-abcd-ef0123456789"));
        let path = m.subtitle_path(id, 2, ".srt").expect("subtitle path");
        assert!(path.ends_with("/2.srt"));

        assert_eq!(m.subtitle_folder_path("not-a-guid"), None);
        assert_eq!(m.subtitle_path("not-a-guid", 0, ".srt"), None);
    }

    #[test]
    fn attachment_rejects_unsafe_leaf_name() {
        let (_tmp, m) = manager();
        let id = "0a1b2c3d-4e5f-6789-abcd-ef0123456789";
        assert!(m.attachment_path(id, "font.ttf").is_some());
        assert_eq!(m.attachment_path(id, "../escape").into_iter().count(), 0);
        assert_eq!(m.attachment_path(id, "sub/dir"), None);
    }

    #[test]
    fn trickplay_directory_variants() {
        let (_tmp, m) = manager();
        let id = Uuid::parse_str("0a1b2c3d-4e5f-6789-abcd-ef0123456789").unwrap();
        let internal = m.trickplay_directory(id, "/media/movie.mkv", false);
        assert!(internal.ends_with("trickplay/0a/0a1b2c3d-4e5f-6789-abcd-ef0123456789"));

        let with_media = m.trickplay_directory(id, "/media/movie.mkv", true);
        assert_eq!(with_media, "/media/movie.trickplay");
    }

    #[test]
    fn chapter_image_paths_use_internal_metadata_layout() {
        let (_tmp, m) = manager();
        let id = Uuid::parse_str("0a1b2c3d-4e5f-6789-abcd-ef0123456789").unwrap();
        let folder = m.chapter_image_folder_path(id, "/media/movie.mkv");
        assert!(folder.contains("/library/0a/0a1b2c3d4e5f6789abcdef0123456789/chapters"));
        let path = m.chapter_image_path(id, "/media/movie.mkv", 12_345);
        assert!(path.ends_with("/chapters/12345.jpg"));
    }

    #[test]
    fn extracted_data_paths_include_all_folders() {
        let (_tmp, m) = manager();
        let id = Uuid::parse_str("0a1b2c3d-4e5f-6789-abcd-ef0123456789").unwrap();
        let paths = m.extracted_data_paths(id, "/media/movie.mkv");
        // attachments, subtitles, trickplay(internal), trickplay(with-media), chapters
        assert_eq!(paths.len(), 5);
        assert!(paths.iter().any(|p| p.contains("attachments")));
        assert!(paths.iter().any(|p| p.contains("subtitles")));
        assert!(paths.iter().any(|p| p.ends_with("movie.trickplay")));
        assert!(paths.iter().any(|p| p.contains("/chapters")));
    }

    #[test]
    fn extracted_data_paths_skip_with_media_when_no_path() {
        let (_tmp, m) = manager();
        let id = Uuid::new_v4();
        let paths = m.extracted_data_paths(id, "");
        // No media path → no with-media trickplay folder.
        assert_eq!(paths.len(), 4);
    }
}
