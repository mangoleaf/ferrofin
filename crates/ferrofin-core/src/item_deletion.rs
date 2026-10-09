//! Deleting a library item: who may, and which files go with it.
//!
//! Ports the two halves of upstream's `DELETE /Items/{itemId}`
//! (`LibraryController.DeleteItem`/`DeleteItems`, `LibraryController.cs:362-445`
//! at master `96bca6f0bd`), exactly — no Ferrofin correction (owner decision
//! 2026-10-04):
//!
//! - **Who may** — `BaseItem.CanDelete(User)` (`BaseItem.cs:844-897`) with
//!   its overrides. [`can_delete`] is the one implementation: the DTO's
//!   per-user `CanDelete` (which jellyfin-web shows its "Delete" entry by) and
//!   the endpoints' `401` both read it.
//! - **What goes** — `LibraryManager.DeleteItem` with
//!   `DeleteFileLocation = true` (`LibraryManager.cs:421-625`):
//!   [`delete_paths`] is `BaseItem.GetDeletePaths` (`BaseItem.cs:2598-2618`,
//!   `Video.cs:727-742`, `Episode.cs:288-298`) and [`delete_item_paths`] is
//!   its `DeleteItemPath` loop (`:627-682`): the paths in order, only the
//!   first one's failure rethrown, the rest swallowed.
//!
//! Upstream's behaviour is kept where it surprises: a video not in a mixed
//! folder takes its whole containing folder (a trailer or another version
//! beside it included), and a mixed-folder item's sidecars match by name
//! prefix (`Alien.mkv` takes `Aliens.nfo`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::library::ContentPermissions;
use uuid::Uuid;

use crate::kinds;

/// What `BaseItem.CanDelete(User)` reads beyond the row itself, gathered once
/// for a page of items (or for the one item a delete endpoint names).
#[derive(Debug, Clone, Default)]
pub(crate) struct DeleteFacts {
    /// The requesting user. `None` projects without a user, where upstream
    /// asks the file-level `item.CanDelete()` alone.
    pub(crate) user_id: Option<Uuid>,
    /// That user's permissions. `None` with a user is an unknown policy,
    /// which never grants a deletion.
    pub(crate) permissions: Option<ContentPermissions>,
    /// `Playlist.OwnerUserId` of each playlist among the items.
    pub(crate) playlist_owners: HashMap<Uuid, Uuid>,
    /// The files a recording is being written to (`Video.IsActiveRecording`).
    pub(crate) active_recordings: HashSet<String>,
    /// The libraries (`CollectionFolder` ids) each item belongs to —
    /// `LibraryManager.GetCollectionFolders` — read only when
    /// [`Self::needs_libraries`] says the folder grants decide.
    pub(crate) libraries: HashMap<Uuid, Vec<Uuid>>,
    /// Whether this answers for a page of items (`DtoService.
    /// GetBaseItemDtos`), where upstream calls the non-virtual
    /// `CanDelete(user, allCollectionFolders)` and so skips the playlist
    /// override, rather than for one item (`GetBaseItemDto`) or the delete
    /// endpoints, which call the virtual `CanDelete(user)`.
    pub(crate) list_page: bool,
}

impl DeleteFacts {
    /// Whether the user's "Allow media deletion from" list can decide a
    /// delete: they lack the global permission and the list is not empty.
    /// Only then are the items' libraries worth reading.
    pub(crate) fn needs_libraries(&self) -> bool {
        self.user_id.is_some()
            && self.permissions.as_ref().is_some_and(|p| {
                !p.enable_content_deletion && !p.content_deletion_folders.is_empty()
            })
    }
}

/// A stored id column as a non-nil [`Uuid`].
fn stored_uuid(value: Option<&str>) -> Option<Uuid> {
    value
        .and_then(|v| Uuid::parse_str(v).ok())
        .filter(|id| !id.is_nil())
}

/// The row's own id (nil on a malformed one, which then matches nothing).
fn item_uuid(item: &BaseItemEntity) -> Uuid {
    Uuid::parse_str(&item.id).unwrap_or_else(|_| Uuid::nil())
}

/// `BaseItem.SourceType == SourceType.Channel`: the row carries a channel.
fn channel_of(item: &BaseItemEntity) -> Option<Uuid> {
    stored_uuid(item.channel_id.as_deref())
}

/// C# `BaseItem.IsFileProtocol`: a non-empty path that is a local file.
fn is_file_protocol(path: Option<&str>) -> bool {
    path.is_some_and(|p| !p.is_empty() && crate::media_info_resolver::is_file_protocol(p))
}

/// C# `BaseItem.CanDelete()` with its overrides — whether the item can be
/// deleted at all, before any user is asked.
///
/// - the kind table [`kinds::can_delete`] (`Folder.IsRoot`,
///   `CollectionFolder`, `UserView`, `AggregateFolder`, `BasePluginFolder`,
///   the by-name kinds, `MusicArtist => !IsAccessedByName`);
/// - `Channel`, `LiveTvChannel`, `LiveTvProgram` are hard `false`;
/// - a channel item (`SourceType.Channel`) asks `ChannelManager.CanDelete`,
///   which is `channel is ISupportsDelete` — and Ferrofin has no channel that
///   supports deleting (there is no channel-plugin mechanism), so `false`;
/// - `Video.CanDelete` is `false` while a recording writes its file;
/// - otherwise `IsFileProtocol`: an item with no path, or a streamed one,
///   cannot be deleted. The exceptions are the collections and playlists
///   Ferrofin creates: their rows have no path, where upstream's sit in the
///   data directory (`collections/<name> [boxset]`,
///   `playlists/<name>`, `PlaylistManager.cs:83-141`), so upstream's
///   `CanDelete()` is `true` for them and so is this.
///
/// A virtual item (a missing episode or season) is never deletable: it has
/// nothing on disk, and upstream's has no path.
pub(crate) fn can_delete_file(
    item: &BaseItemEntity,
    kind: BaseItemKind,
    active_recordings: &HashSet<String>,
) -> bool {
    let has_parent = stored_uuid(item.parent_id.as_deref()).is_some();
    if item.is_virtual_item || !kinds::can_delete(kind, has_parent) {
        return false;
    }
    if matches!(
        kind,
        BaseItemKind::Channel
            | BaseItemKind::LiveTvChannel
            | BaseItemKind::TvChannel
            | BaseItemKind::LiveTvProgram
    ) || channel_of(item).is_some()
    {
        return false;
    }
    let path = item.path.as_deref();
    if kinds::is_video(kind) && path.is_some_and(|p| active_recordings.contains(p)) {
        return false;
    }
    is_file_protocol(path)
        || (matches!(kind, BaseItemKind::BoxSet | BaseItemKind::Playlist)
            && path.is_none_or(str::is_empty))
}

/// C# `BaseItem.IsAuthorizedToDelete(user, allCollectionFolders)` and its
/// `BoxSet`/`Playlist` overrides.
fn is_authorized_to_delete(
    item: &BaseItemEntity,
    kind: BaseItemKind,
    permissions: &ContentPermissions,
    libraries: Option<&Vec<Uuid>>,
) -> bool {
    match kind {
        // `BoxSet.cs:107-110`.
        BaseItemKind::BoxSet => {
            permissions.is_administrator || permissions.enable_collection_management
        }
        // `Playlist.cs:121-124`.
        BaseItemKind::Playlist => true,
        _ => {
            if permissions.enable_content_deletion {
                return true;
            }
            let allowed = &permissions.content_deletion_folders;
            // `if (SourceType == SourceType.Channel) return allowed.Contains(ChannelId);`
            if let Some(channel) = channel_of(item) {
                return allowed.contains(&channel);
            }
            libraries.is_some_and(|ids| ids.iter().any(|id| allowed.contains(id)))
        }
    }
}

/// C# `BaseItem.CanDelete(User)` — the DTO's `CanDelete` for `facts.user_id`,
/// and the check `LibraryController.DeleteItem` answers `401` on.
///
/// Without a user it is the file-level [`can_delete_file`] (upstream's
/// `user is null ? item.CanDelete() : …`). With one:
/// - a playlist answers `IsAdministrator || user.Id == OwnerUserId`
///   (`Playlist.CanDelete(User)` overrides the whole check) — except on a
///   page of items ([`DeleteFacts::list_page`]): master's `GetBaseItemDtos`
///   (`DtoService.cs:182-186,422-428`) calls the non-virtual
///   `CanDelete(user, allCollectionFolders)`, which is `CanDelete() &&
///   IsAuthorizedToDelete` and `Playlist.IsAuthorizedToDelete` is `true`, so
///   a list shows every playlist deletable while the endpoint still asks the
///   override;
/// - anything else is `CanDelete() && IsAuthorizedToDelete(user, …)`.
pub(crate) fn can_delete(item: &BaseItemEntity, kind: BaseItemKind, facts: &DeleteFacts) -> bool {
    let file_level = can_delete_file(item, kind, &facts.active_recordings);
    let Some(user_id) = facts.user_id else {
        return file_level;
    };
    let Some(permissions) = facts.permissions.as_ref() else {
        return false;
    };
    if kind == BaseItemKind::Playlist && !facts.list_page {
        return permissions.is_administrator
            || facts.playlist_owners.get(&item_uuid(item)) == Some(&user_id);
    }
    file_level
        && is_authorized_to_delete(
            item,
            kind,
            permissions,
            facts.libraries.get(&item_uuid(item)),
        )
}

/// The extensions `BaseItem.GetLocalMetadataFilesToDelete` removes beside a
/// file in a mixed folder (`BaseItem._supportedExtensions`, `BaseItem.cs:58-77`:
/// `SupportedImageExtensions` plus the metadata, subtitle and lyric ones).
const SIDECAR_EXTENSIONS: [&str; 21] = [
    "png", "jpg", "jpeg", "webp", "tbn", "gif", "svg", "nfo", "xml", "srt", "vtt", "sub", "sup",
    "idx", "txt", "edl", "bif", "smi", "ttml", "lrc", "elrc",
];

/// One path `BaseItem.GetDeletePaths` names — a `FileSystemMetadata`'s
/// `FullName` and `IsDirectory`, which decides `Directory.Delete(path, true)`
/// or `File.Delete(path)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeletePath {
    /// The file or folder.
    pub(crate) path: PathBuf,
    /// Whether it is removed as a folder, with everything in it.
    pub(crate) is_directory: bool,
}

/// `IFileSystem.GetFileSystemInfo(path)` (`ManagedFileSystem.cs:185-215`): an
/// existing path is what it is on disk; a missing one is a file when its
/// name has an extension, else a folder.
fn file_system_info(path: &Path) -> DeletePath {
    let is_directory =
        std::fs::metadata(path).map_or_else(|_| path.extension().is_none(), |meta| meta.is_dir());
    DeletePath {
        path: path.to_path_buf(),
        is_directory,
    }
}

/// `string.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)`: the
/// invariant simple uppercase mapping keeps every character one character,
/// so comparing the mapped strings is comparing character by character.
fn starts_with_ordinal_ignore_case(text: &str, prefix: &str) -> bool {
    use ferrofin_util::string_extensions::upper_invariant;
    upper_invariant(text).starts_with(&upper_invariant(prefix))
}

/// `BaseItem.GetLocalMetadataFilesToDelete` (`BaseItem.cs:2606-2618`): for a
/// file in a mixed folder, every file in its directory with a supported
/// extension whose name (without extension) starts with the item's — so
/// `Alien.mkv` takes `Alien.nfo`, `Alien-poster.jpg` and `Aliens.nfo` alike.
/// Nothing for a folder or for an item not in a mixed folder.
///
/// The listing is `GetFiles(dir, extensions, false, false)` with
/// `IgnoreInaccessible`: a directory that is gone or unreadable lists
/// nothing, and an entry that cannot be read is skipped.
///
/// # Errors
///
/// Any other failure to list the directory.
fn local_metadata_files_to_delete(
    item: &BaseItemEntity,
    path: &Path,
) -> std::io::Result<Vec<DeletePath>> {
    if item.is_folder || !item.is_in_mixed_folder {
        return Ok(Vec::new());
    }
    let (Some(dir), Some(file_name)) = (path.parent(), path.file_stem().and_then(|s| s.to_str()))
    else {
        return Ok(Vec::new());
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(Vec::new());
        }
        Err(err) => return Err(err),
    };
    let mut out: Vec<DeletePath> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|candidate| candidate.is_file())
        .filter(|candidate| {
            candidate
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| {
                    SIDECAR_EXTENSIONS
                        .iter()
                        .any(|s| ferrofin_util::string_extensions::equals_ordinal_ignore_case(s, e))
                })
        })
        .filter(|candidate| {
            candidate
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| starts_with_ordinal_ignore_case(stem, file_name))
        })
        .map(|candidate| DeletePath {
            path: candidate,
            is_directory: false,
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// `BaseItem.GetDeletePaths` and its overrides — what deleting the item
/// removes from disk, the one upstream calls required first:
///
/// - an `Episode` (`Episode.cs:288-298`): its path (a folder only if the
///   row is one) and its local metadata files;
/// - any other video not in a mixed folder (`Video.cs:727-742`): its whole
///   [`crate::video_versions::containing_folder_path_at`], whatever else is
///   in it — nothing for a path with no parent folder (`/`);
/// - anything else (`BaseItem.cs:2598-2604`): its path, as it is on disk,
///   and its local metadata files ([`local_metadata_files_to_delete`]).
///
/// # Errors
///
/// A failure listing the item's directory for its sidecars.
pub(crate) fn delete_paths(
    item: &BaseItemEntity,
    kind: BaseItemKind,
    path: &str,
) -> std::io::Result<Vec<DeletePath>> {
    let file = Path::new(path);
    let mut paths = if kind == BaseItemKind::Episode {
        vec![DeletePath {
            path: file.to_path_buf(),
            is_directory: item.is_folder,
        }]
    } else if kinds::is_video(kind) && !item.is_in_mixed_folder {
        // A path with no parent (`/`) has no containing folder: .NET's
        // `GetDirectoryName` is `null` there, which `DeleteItemPath` finds
        // neither a file nor a folder — nothing is deleted.
        let folder = crate::video_versions::containing_folder_path_at(item, path);
        if folder.is_empty() {
            return Ok(Vec::new());
        }
        return Ok(vec![DeletePath {
            path: PathBuf::from(folder),
            is_directory: true,
        }]);
    } else {
        vec![file_system_info(file)]
    };
    paths.extend(local_metadata_files_to_delete(item, file)?);
    Ok(paths)
}

/// `LibraryManager.DeleteItemPath` (`LibraryManager.cs:627-682`): an existing
/// path is removed — recursively for a folder — and a path that is already
/// gone only leaves the row to delete. Any other failure is returned when
/// the path is `required`, and logged and passed over otherwise.
///
/// # Errors
///
/// The removal's failure, for a `required` path.
fn delete_item_path(target: &DeletePath, required: bool) -> std::io::Result<()> {
    let path = &target.path;
    // `Directory.Exists(path) || File.Exists(path)`, both following links.
    if !(path.is_dir() || path.is_file()) {
        return Ok(());
    }
    tracing::info!(path = %path.display(), "deleting item path");
    let removed = if target.is_directory {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match removed {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                path = %path.display(),
                "not found, only removing it from the database"
            );
            Ok(())
        }
        Err(err) if required => Err(std::io::Error::new(
            err.kind(),
            format!("{}: {err}", path.display()),
        )),
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "could not delete an item's file; deleting the item anyway");
            Ok(())
        }
    }
}

/// `DeleteItem`'s loop over `GetDeletePaths` (`LibraryManager.cs:582-594`):
/// "Assume only the first is required" — the first path's failure is
/// returned (nothing after it, and no row, is deleted then); the others'
/// failures are passed over.
///
/// # Errors
///
/// The first path's removal failure.
pub(crate) fn delete_item_paths(paths: &[DeletePath]) -> std::io::Result<()> {
    for (index, target) in paths.iter().enumerate() {
        delete_item_path(target, index == 0)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn row(kind: BaseItemKind, path: Option<&str>) -> BaseItemEntity {
        BaseItemEntity {
            id: Uuid::from_u128(0xD1).to_string(),
            type_: crate::item_type_lookup::stored_type_name(kind)
                .unwrap_or_default()
                .to_owned(),
            path: path.map(str::to_owned),
            parent_id: Some(Uuid::from_u128(0xD0).to_string()),
            ..BaseItemEntity::default()
        }
    }

    fn user_facts(permissions: ContentPermissions) -> DeleteFacts {
        DeleteFacts {
            user_id: Some(Uuid::from_u128(0xA1)),
            permissions: Some(permissions),
            ..DeleteFacts::default()
        }
    }

    #[rstest]
    #[case::movie(BaseItemKind::Movie, Some("/m/Film/Film.mkv"), true)]
    #[case::no_path(BaseItemKind::Movie, None, false)]
    #[case::empty_path(BaseItemKind::Episode, Some(""), false)]
    #[case::streamed(BaseItemKind::Movie, Some("http://host/film.mkv"), false)]
    #[case::ferrofin_collection(BaseItemKind::BoxSet, None, true)]
    #[case::ferrofin_playlist(BaseItemKind::Playlist, None, true)]
    #[case::adopted_playlist(BaseItemKind::Playlist, Some("%AppDataPath%/playlists/Mix"), true)]
    #[case::pathless_season(BaseItemKind::Season, None, false)]
    #[case::collection_folder(BaseItemKind::CollectionFolder, Some("/m"), false)]
    #[case::genre(BaseItemKind::Genre, Some("/meta/Genre/Drama"), false)]
    #[case::channel(BaseItemKind::Channel, Some("/m/x"), false)]
    #[case::program(BaseItemKind::LiveTvProgram, Some("/m/x"), false)]
    fn the_file_level_half_follows_the_overrides(
        #[case] kind: BaseItemKind,
        #[case] path: Option<&str>,
        #[case] expected: bool,
    ) {
        assert_eq!(
            can_delete_file(&row(kind, path), kind, &HashSet::new()),
            expected
        );
    }

    #[test]
    fn a_virtual_item_a_channel_item_and_an_active_recording_cannot_be_deleted() {
        let mut virtual_item = row(BaseItemKind::Episode, Some("/m/s/e.mkv"));
        virtual_item.is_virtual_item = true;
        assert!(!can_delete_file(
            &virtual_item,
            BaseItemKind::Episode,
            &HashSet::new()
        ));

        let mut channel_item = row(BaseItemKind::Movie, Some("/m/c.mkv"));
        channel_item.channel_id = Some(Uuid::from_u128(0xC1).to_string());
        assert!(!can_delete_file(
            &channel_item,
            BaseItemKind::Movie,
            &HashSet::new()
        ));

        let recording = row(BaseItemKind::Video, Some("/rec/show.ts"));
        let active: HashSet<String> = ["/rec/show.ts".to_owned()].into();
        assert!(!can_delete_file(&recording, BaseItemKind::Video, &active));
        assert!(can_delete_file(
            &recording,
            BaseItemKind::Video,
            &HashSet::new()
        ));
    }

    #[test]
    fn without_a_user_only_the_file_level_half_answers() {
        let movie = row(BaseItemKind::Movie, Some("/m/a.mkv"));
        assert!(can_delete(
            &movie,
            BaseItemKind::Movie,
            &DeleteFacts::default()
        ));
    }

    #[test]
    fn an_unknown_policy_never_grants_a_delete() {
        let movie = row(BaseItemKind::Movie, Some("/m/a.mkv"));
        let facts = DeleteFacts {
            user_id: Some(Uuid::from_u128(0xA1)),
            ..DeleteFacts::default()
        };
        assert!(!can_delete(&movie, BaseItemKind::Movie, &facts));
    }

    #[test]
    fn media_needs_the_deletion_permission_and_admin_alone_is_not_enough() {
        let movie = row(BaseItemKind::Movie, Some("/m/a.mkv"));
        let admin = ContentPermissions {
            is_administrator: true,
            ..ContentPermissions::default()
        };
        assert!(!can_delete(&movie, BaseItemKind::Movie, &user_facts(admin)));
        let deleter = ContentPermissions {
            enable_content_deletion: true,
            ..ContentPermissions::default()
        };
        assert!(can_delete(
            &movie,
            BaseItemKind::Movie,
            &user_facts(deleter)
        ));
    }

    #[test]
    fn the_folder_grants_decide_by_library() {
        let library = Uuid::from_u128(0x11);
        let other = Uuid::from_u128(0x12);
        let movie = row(BaseItemKind::Movie, Some("/m/a.mkv"));
        let mut facts = user_facts(ContentPermissions {
            content_deletion_folders: vec![library],
            ..ContentPermissions::default()
        });
        assert!(facts.needs_libraries());
        assert!(
            !can_delete(&movie, BaseItemKind::Movie, &facts),
            "no library known"
        );
        facts.libraries.insert(item_uuid(&movie), vec![other]);
        assert!(!can_delete(&movie, BaseItemKind::Movie, &facts));
        facts
            .libraries
            .insert(item_uuid(&movie), vec![other, library]);
        assert!(can_delete(&movie, BaseItemKind::Movie, &facts));
    }

    #[test]
    fn a_channel_item_is_granted_by_its_channel_but_its_channel_cannot_delete_it() {
        let channel = Uuid::from_u128(0xC1);
        let mut item = row(BaseItemKind::Movie, Some("/m/c.mkv"));
        item.channel_id = Some(channel.to_string());
        let permissions = ContentPermissions {
            content_deletion_folders: vec![channel],
            ..ContentPermissions::default()
        };
        assert!(is_authorized_to_delete(
            &item,
            BaseItemKind::Movie,
            &permissions,
            None
        ));
        assert!(!can_delete(
            &item,
            BaseItemKind::Movie,
            &user_facts(permissions)
        ));
    }

    #[test]
    fn a_collection_needs_collection_management_or_admin() {
        let boxset = row(BaseItemKind::BoxSet, None);
        for (permissions, expected) in [
            (ContentPermissions::default(), false),
            (
                ContentPermissions {
                    enable_content_deletion: true,
                    ..ContentPermissions::default()
                },
                false,
            ),
            (
                ContentPermissions {
                    enable_collection_management: true,
                    ..ContentPermissions::default()
                },
                true,
            ),
            (
                ContentPermissions {
                    is_administrator: true,
                    ..ContentPermissions::default()
                },
                true,
            ),
        ] {
            assert_eq!(
                can_delete(
                    &boxset,
                    BaseItemKind::BoxSet,
                    &user_facts(permissions.clone())
                ),
                expected,
                "{permissions:?}"
            );
        }
    }

    #[test]
    fn a_playlist_is_its_owners_or_an_administrators_except_on_a_page() {
        let playlist = row(BaseItemKind::Playlist, None);
        let user = Uuid::from_u128(0xA1);
        let mut facts = user_facts(ContentPermissions {
            enable_content_deletion: true,
            ..ContentPermissions::default()
        });
        assert!(
            !can_delete(&playlist, BaseItemKind::Playlist, &facts),
            "not the owner"
        );
        // `GetBaseItemDtos` calls the non-virtual `CanDelete(user,
        // allCollectionFolders)`: `CanDelete() && Playlist.IsAuthorizedToDelete`
        // (`true`), the owner override never asked.
        let page = DeleteFacts {
            list_page: true,
            ..facts.clone()
        };
        assert!(can_delete(&playlist, BaseItemKind::Playlist, &page));
        let nobody_page = DeleteFacts {
            list_page: true,
            ..user_facts(ContentPermissions::default())
        };
        assert!(can_delete(&playlist, BaseItemKind::Playlist, &nobody_page));
        facts.playlist_owners.insert(item_uuid(&playlist), user);
        assert!(can_delete(&playlist, BaseItemKind::Playlist, &facts));
        let admin = user_facts(ContentPermissions {
            is_administrator: true,
            ..ContentPermissions::default()
        });
        assert!(can_delete(&playlist, BaseItemKind::Playlist, &admin));
        // Without a user (an API key's projection) it is `CanDelete()`:
        // upstream's playlist sits in `data/playlists`, a file path.
        assert!(can_delete(
            &playlist,
            BaseItemKind::Playlist,
            &DeleteFacts::default()
        ));
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("dir");
        }
        std::fs::write(path, b"x").expect("write");
    }

    fn rows(paths: &[DeletePath]) -> Vec<(PathBuf, bool)> {
        paths
            .iter()
            .map(|p| (p.path.clone(), p.is_directory))
            .collect()
    }

    /// `Video.GetDeletePaths` (`Video.cs:727-742`): a video not in a mixed
    /// folder names its whole `ContainingFolderPath` — the trailer and the
    /// other version beside it go with it, as upstream.
    #[test]
    fn a_video_not_in_a_mixed_folder_takes_its_whole_folder() {
        let tmp = tempfile::tempdir().expect("tmp");
        let folder = tmp.path().join("Film (2000)");
        let file = folder.join("Film (2000).mkv");
        touch(&file);
        touch(&folder.join("Film (2000)-trailer.mkv"));
        touch(&folder.join("Film (2000) - 4K.mkv"));
        for kind in [
            BaseItemKind::Movie,
            BaseItemKind::Trailer,
            BaseItemKind::Video,
        ] {
            let item = row(kind, file.to_str());
            assert_eq!(
                rows(&delete_paths(&item, kind, file.to_str().unwrap_or_default()).expect("paths")),
                vec![(folder.clone(), true)],
                "{kind:?}"
            );
        }
    }

    /// A video at the root (`/`) has no containing folder (.NET's
    /// `GetDirectoryName("/")` is `null`): nothing is named, never `/`.
    #[rstest]
    #[case::movie(BaseItemKind::Movie)]
    #[case::video(BaseItemKind::Video)]
    fn a_video_at_the_root_names_no_folder(#[case] kind: BaseItemKind) {
        let item = row(kind, Some("/"));
        assert!(delete_paths(&item, kind, "/").expect("paths").is_empty());
    }

    /// `BaseItem.GetLocalMetadataFilesToDelete` (`BaseItem.cs:2606-2618`): a
    /// mixed-folder item's sidecars match by name prefix, ignoring case —
    /// `Alien.mkv` takes `Aliens.nfo` too — with a supported extension only.
    #[test]
    fn a_mixed_folder_item_takes_its_path_then_every_prefix_sidecar() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("Movies");
        let file = dir.join("Alien.mkv");
        for name in [
            "Alien.mkv",
            "Alien.nfo",
            "Alien-poster.jpg",
            "alien.EN.SRT",
            "Aliens.nfo",
            "Aliens.mkv",
            "Alien.mp4",
            "Predator.nfo",
            "Alien",
        ] {
            touch(&dir.join(name));
        }
        std::fs::create_dir_all(dir.join("Alien.trickplay")).expect("dir");
        let mut mixed = row(BaseItemKind::Movie, file.to_str());
        mixed.is_in_mixed_folder = true;
        let paths = delete_paths(
            &mixed,
            BaseItemKind::Movie,
            file.to_str().unwrap_or_default(),
        )
        .expect("paths");
        assert_eq!(
            rows(&paths),
            vec![
                (file.clone(), false),
                (dir.join("Alien-poster.jpg"), false),
                (dir.join("Alien.nfo"), false),
                (dir.join("Aliens.nfo"), false),
                (dir.join("alien.EN.SRT"), false),
            ]
        );
    }

    /// `Episode.GetDeletePaths` (`Episode.cs:288-298`): the episode's own
    /// path, never its season folder; its sidecars only in a mixed folder.
    /// A folder item names its own path, as it is on disk.
    #[test]
    fn an_episode_keeps_its_folder_and_a_series_names_its_own() {
        let tmp = tempfile::tempdir().expect("tmp");
        let file = tmp.path().join("Show/Season 1/S01E01.mkv");
        touch(&file);
        touch(&tmp.path().join("Show/Season 1/S01E01.nfo"));
        let episode = row(BaseItemKind::Episode, file.to_str());
        assert_eq!(
            rows(
                &delete_paths(
                    &episode,
                    BaseItemKind::Episode,
                    file.to_str().unwrap_or_default()
                )
                .expect("paths")
            ),
            vec![(file.clone(), false)],
            "not in a mixed folder: no sidecars"
        );
        let mut mixed = episode.clone();
        mixed.is_in_mixed_folder = true;
        assert_eq!(
            delete_paths(
                &mixed,
                BaseItemKind::Episode,
                file.to_str().unwrap_or_default()
            )
            .expect("paths")
            .len(),
            2
        );
        let series_dir = tmp.path().join("Show");
        let mut series = row(BaseItemKind::Series, series_dir.to_str());
        series.is_folder = true;
        assert_eq!(
            rows(
                &delete_paths(
                    &series,
                    BaseItemKind::Series,
                    series_dir.to_str().unwrap_or_default()
                )
                .expect("paths")
            ),
            vec![(series_dir.clone(), true)]
        );
        // `GetFileSystemInfo`: a missing path with an extension is a file,
        // one without is a folder.
        let gone_book = row(BaseItemKind::Book, Some("/gone/a.epub"));
        assert!(
            !delete_paths(&gone_book, BaseItemKind::Book, "/gone/a.epub").expect("paths")[0]
                .is_directory
        );
        let gone_folder = row(BaseItemKind::Folder, Some("/gone/folder"));
        assert!(
            delete_paths(&gone_folder, BaseItemKind::Folder, "/gone/folder").expect("paths")[0]
                .is_directory
        );
    }

    /// A stacked video in a mixed folder names its first part and its prefix
    /// sidecars only (upstream deletes no other part); a disc rip not in a
    /// mixed folder names its own folder.
    #[test]
    fn a_stack_and_a_disc_rip_follow_upstream() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cd1 = tmp.path().join("Film-cd1.avi");
        let cd2 = tmp.path().join("Film-cd2.avi");
        touch(&cd1);
        touch(&cd2);
        let mut stack = row(BaseItemKind::Movie, cd1.to_str());
        stack.is_in_mixed_folder = true;
        stack.data = Some(r#"{"AdditionalParts":["x"]}"#.to_owned());
        assert_eq!(
            rows(
                &delete_paths(
                    &stack,
                    BaseItemKind::Movie,
                    cd1.to_str().unwrap_or_default()
                )
                .expect("paths")
            ),
            vec![(cd1.clone(), false)]
        );
        let disc_dir = tmp.path().join("Disc (2000)");
        std::fs::create_dir_all(disc_dir.join("BDMV")).expect("dir");
        let mut disc = row(BaseItemKind::Movie, disc_dir.to_str());
        disc.data = Some(r#"{"VideoType":"BluRay"}"#.to_owned());
        assert_eq!(
            rows(
                &delete_paths(
                    &disc,
                    BaseItemKind::Movie,
                    disc_dir.to_str().unwrap_or_default()
                )
                .expect("paths")
            ),
            vec![(disc_dir.clone(), true)]
        );
    }

    /// `DeleteItemPath` (`LibraryManager.cs:627-682`): the first path's
    /// failure is rethrown, the later ones' are swallowed; a missing path is
    /// skipped; a folder goes with its contents.
    #[test]
    fn only_the_first_paths_failure_is_returned() {
        let tmp = tempfile::tempdir().expect("tmp");
        let folder = tmp.path().join("Film");
        touch(&folder.join("sub/a.nfo"));
        let file = tmp.path().join("b.nfo");
        touch(&file);
        let as_file = DeletePath {
            path: folder.clone(),
            is_directory: false,
        };
        // Removing a folder as a file fails: first → returned, nothing after
        // it is touched.
        let err = delete_item_paths(&[
            as_file.clone(),
            DeletePath {
                path: file.clone(),
                is_directory: false,
            },
        ])
        .expect_err("required");
        assert!(err.to_string().contains("Film"), "{err}");
        assert!(folder.is_dir() && file.is_file());
        // Later → swallowed, the rest still deleted.
        delete_item_paths(&[
            DeletePath {
                path: file.clone(),
                is_directory: false,
            },
            as_file,
            DeletePath {
                path: tmp.path().join("gone.srt"),
                is_directory: false,
            },
        ])
        .expect("only the first is required");
        assert!(!file.exists() && folder.is_dir());
        delete_item_paths(&[DeletePath {
            path: folder.clone(),
            is_directory: true,
        }])
        .expect("folder");
        assert!(!folder.exists());
        delete_item_paths(&[DeletePath {
            path: folder,
            is_directory: true,
        }])
        .expect("a missing first path is not a failure");
    }

    #[test]
    fn ordinal_ignore_case_prefix_matches_dotnet() {
        assert!(starts_with_ordinal_ignore_case("ALIENS", "alien"));
        assert!(starts_with_ordinal_ignore_case("Élan vital", "élan"));
        assert!(!starts_with_ordinal_ignore_case("Ali", "Alien"));
    }
}
