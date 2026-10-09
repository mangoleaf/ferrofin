//! Who may delete a library item: `BaseItem.CanDelete(User)`
//! (`BaseItem.cs:844-897`) with its overrides, the check upstream's
//! `LibraryController.DeleteItem`/`DeleteItems` (`LibraryController.cs:362-445`
//! at master `96bca6f0bd`) answer `401` on.
//!
//! [`can_delete`] is the one implementation: the DTO's per-user `CanDelete`
//! (which jellyfin-web shows its "Delete" entry by) and the delete endpoints
//! (through `DtoService::can_delete`) both read it, so they agree.
//!
//! TODO(parity, open work item): what a delete removes is not here. Upstream
//! deletes the media files (`DeleteFileLocation = true`); not ported yet, it
//! waits on scanner parity. See `brain/plans/PLAN_ITEM_FILE_DELETION.md`.

use std::collections::{HashMap, HashSet};

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
}
