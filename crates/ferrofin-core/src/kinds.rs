//! `BaseItem` domain behavior, ported as free functions over [`BaseItemKind`].
//!
//! In Jellyfin the `BaseItem` → `Folder` → `Video` (etc.) class hierarchy carries
//! virtual boolean properties — `IsFolder`, `SupportsPeople`,
//! `SupportsThemeMedia`, … — that each concrete subclass overrides. Ferrofin has no
//! `BaseItem` object graph: item rows are [`ferrofin_db::entities::base_items::BaseItemEntity`]
//! and their *kind* is a [`BaseItemKind`]. The OOP behavior therefore lives here
//! as pure functions keyed on the kind.
//!
//! Each function encodes the C# override table exactly:
//! - "folder" kinds are the `Folder` subclasses
//!   (`Folder`/`AggregateFolder`/`BoxSet`/`Series`/`Season`/`MusicAlbum`/… plus the
//!   `BasePluginFolder` line);
//! - `Video` and its subclasses (`Movie`/`Episode`/`MusicVideo`/`Trailer`/`Video`)
//!   override `SupportsPeople`, `SupportsThemeMedia`, and
//!   `SupportsInheritedParentImages` to `true`;
//! - the `IItemByName` kinds (`Genre`/`MusicGenre`/`Studio`/`Year`/`Person`/
//!   `MusicArtist`) are the by-name items.
//!
//! These are the queries the library/DTO layers make against an item's kind; the
//! richer per-instance behavior (which depends on runtime fields, not just the
//! kind) is not part of this seam.

use ferrofin_model::data::BaseItemKind;

/// Whether items of this kind are folders (C# `BaseItem.IsFolder`, overridden to
/// `true` by every `Folder` subclass).
///
/// Note this is the *class-level* default; a persisted row's `IsFolder` column is
/// authoritative for a specific item (e.g. a `Video` that happens to be a DVD
/// folder). Use the column when you have a row, this when you only have the kind.
#[must_use]
pub fn is_folder(kind: BaseItemKind) -> bool {
    matches!(
        kind,
        BaseItemKind::AggregateFolder
            | BaseItemKind::BasePluginFolder
            | BaseItemKind::BoxSet
            | BaseItemKind::Channel
            | BaseItemKind::ChannelFolderItem
            | BaseItemKind::CollectionFolder
            | BaseItemKind::Folder
            | BaseItemKind::ManualPlaylistsFolder
            | BaseItemKind::MusicAlbum
            | BaseItemKind::MusicArtist
            | BaseItemKind::PhotoAlbum
            | BaseItemKind::Playlist
            | BaseItemKind::PlaylistsFolder
            | BaseItemKind::Season
            | BaseItemKind::Series
            | BaseItemKind::UserRootFolder
            | BaseItemKind::UserView
    )
}

/// Whether items of this kind are displayed as folders in the UI
/// (C# `BaseItem.IsDisplayedAsFolder`).
///
/// Identical to [`is_folder`] in the base hierarchy — every `Folder` overrides
/// both together — but kept as its own function so the two concepts can diverge
/// as more per-kind behavior is ported.
#[must_use]
pub fn is_displayed_as_folder(kind: BaseItemKind) -> bool {
    is_folder(kind)
}

/// Whether this kind is a video (`Video` or one of its subclasses).
///
/// The `Video` subclasses drive several of the `Supports*` overrides below.
#[must_use]
pub fn is_video(kind: BaseItemKind) -> bool {
    matches!(
        kind,
        BaseItemKind::Video
            | BaseItemKind::Movie
            | BaseItemKind::Episode
            | BaseItemKind::MusicVideo
            | BaseItemKind::Trailer
    )
}

/// Whether this kind is an "item by name" — a genre, studio, year, person, or
/// artist that groups other items rather than being real media
/// (C# `IItemByName`).
#[must_use]
pub fn is_item_by_name(kind: BaseItemKind) -> bool {
    matches!(
        kind,
        BaseItemKind::Genre
            | BaseItemKind::MusicGenre
            | BaseItemKind::Studio
            | BaseItemKind::Year
            | BaseItemKind::Person
            | BaseItemKind::MusicArtist
    )
}

/// Whether items of this kind can own media sources — the rows in
/// `MediaStreamInfos`, `Chapters` and `TrickplayInfos`, and the alternate
/// versions that point at them through `PrimaryVersionId`.
///
/// Stated as an exclusion (`not a folder, not an item-by-name`) rather than a
/// list of media kinds, so a leaf kind can never lose its relations by being
/// forgotten here: only the kinds that provably own none — every `Folder`
/// subclass and every `IItemByName` — are excluded. Upstream never asks the
/// question because a C# `Folder`/`Person` simply has no streams in memory;
/// here the answer decides whether a page pays four DB round trips to learn
/// the same thing.
#[must_use]
pub fn has_media_sources(kind: BaseItemKind) -> bool {
    !is_folder(kind) && !is_item_by_name(kind)
}

/// Whether items of this kind carry cast/crew people
/// (C# `BaseItem.SupportsPeople`, `false` by default, `true` for `Video`).
#[must_use]
pub fn supports_people(kind: BaseItemKind) -> bool {
    is_video(kind)
}

/// Whether items of this kind can have theme songs/videos
/// (C# `BaseItem.SupportsThemeMedia`, `true` for `Folder` and `Video`).
#[must_use]
pub fn supports_theme_media(kind: BaseItemKind) -> bool {
    is_folder(kind) || is_video(kind)
}

/// Whether items of this kind inherit parent images
/// (C# `BaseItem.SupportsInheritedParentImages`, `true` for `Folder` and
/// `Video`).
#[must_use]
pub fn supports_inherited_parent_images(kind: BaseItemKind) -> bool {
    is_folder(kind) || is_video(kind)
}

/// Whether items of this kind participate in the ancestor closure
/// (C# `BaseItem.SupportsAncestors`, `true` by default; the by-name grouping
/// kinds do not).
#[must_use]
pub fn supports_ancestors(kind: BaseItemKind) -> bool {
    !is_item_by_name(kind)
}

/// Whether an item of this kind may be deleted at all — the kind half of
/// C# `BaseItem.CanDelete()` (the per-user content-deletion permission is the
/// caller's concern).
///
/// Structural and by-name rows return `false` and the overrides say why:
/// `Folder.CanDelete` refuses the root (`UserRootFolder.IsRoot`),
/// `AggregateFolder`/`CollectionFolder`/`UserView`/`BasePluginFolder`
/// (`ManualPlaylistsFolder`) are hard `false`, and so are `Genre`/
/// `MusicGenre`/`Studio`/`Year` (the `Person` row is metadata-only as well).
/// `MusicArtist.CanDelete` is `!IsAccessedByName`, i.e. only a physically
/// parented artist folder is deletable — hence `has_parent`.
///
/// Deleting one of these rows is never harmless: `BaseItems.ParentId` is a
/// cascading foreign key, so deleting the `UserRootFolder` would take every
/// library and all of its items with it.
#[must_use]
pub fn can_delete(kind: BaseItemKind, has_parent: bool) -> bool {
    match kind {
        BaseItemKind::UserRootFolder
        | BaseItemKind::AggregateFolder
        | BaseItemKind::CollectionFolder
        | BaseItemKind::UserView
        | BaseItemKind::ManualPlaylistsFolder
        | BaseItemKind::Genre
        | BaseItemKind::MusicGenre
        | BaseItemKind::Studio
        | BaseItemKind::Year
        | BaseItemKind::Person => false,
        BaseItemKind::MusicArtist => has_parent,
        _ => true,
    }
}

/// Whether items of this kind track played/unplayed status
/// (C# `BaseItem.SupportsPlayedStatus`). Real playable media and the folders
/// that aggregate it support it; the by-name grouping kinds do not.
#[must_use]
pub fn supports_played_status(kind: BaseItemKind) -> bool {
    !is_item_by_name(kind)
}

/// Whether items of this kind can resume from a saved position tick
/// (C# `BaseItem.SupportsPositionTicksResume`, `false` by default, overridden to
/// `true` for `Video` and its subclasses plus `Audio`, `AudioBook`, and `Book`).
///
/// The C# `Video` override additionally returns `false` for `ExtraType.Sample`
/// clips; that sub-case is deferred here because the stored item row does not
/// surface `ExtraType` at this layer.
#[must_use]
pub fn supports_position_ticks_resume(kind: BaseItemKind) -> bool {
    is_video(kind)
        || matches!(
            kind,
            BaseItemKind::Audio | BaseItemKind::AudioBook | BaseItemKind::Book
        )
}

/// Whether this kind is an audio item (`Audio` or `AudioBook`).
///
/// The music/instant-mix code branches on audio-ness; a small helper keeps the
/// match in one place rather than duplicated at each call site.
#[must_use]
pub fn is_audio(kind: BaseItemKind) -> bool {
    matches!(kind, BaseItemKind::Audio | BaseItemKind::AudioBook)
}

/// Whether this kind carries artist/album-artist DTO fields — the kinds whose
/// C# classes implement `IHasArtist`/`IHasAlbumArtist` (`Audio`, `AudioBook`,
/// `MusicAlbum`, `MusicVideo`). Jellyfin's `DtoService` only attaches
/// `Artists`/`ArtistItems`/`AlbumArtists` behind those interface tests.
#[must_use]
pub fn has_artist_fields(kind: BaseItemKind) -> bool {
    matches!(
        kind,
        BaseItemKind::Audio
            | BaseItemKind::AudioBook
            | BaseItemKind::MusicAlbum
            | BaseItemKind::MusicVideo
    )
}

/// Whether this kind is one of the music container/leaf kinds
/// (`Audio`/`MusicAlbum`/`MusicArtist`/`MusicGenre`/`MusicVideo`).
///
/// Used by [`crate`]'s music manager to decide which seeds and which item types
/// participate in an instant mix.
#[must_use]
pub fn is_music(kind: BaseItemKind) -> bool {
    matches!(
        kind,
        BaseItemKind::Audio
            | BaseItemKind::MusicAlbum
            | BaseItemKind::MusicArtist
            | BaseItemKind::MusicGenre
            | BaseItemKind::MusicVideo
    )
}

/// Whether a "similar items" request about an item of this kind runs at all —
/// the C# `LibraryController.GetSimilarItems` guard, which answers an empty
/// result for an `Episode` or for any `IItemByName` other than a
/// `MusicArtist` (genres, studios, years, people, music genres) before a
/// provider is consulted.
///
/// Every other kind passes; whether a local provider then serves it is the
/// similar-items manager's per-kind decision.
#[must_use]
pub fn supports_similarity(kind: BaseItemKind) -> bool {
    kind != BaseItemKind::Episode && !(is_item_by_name(kind) && kind != BaseItemKind::MusicArtist)
}

#[cfg(test)]
mod tests {
    use super::{
        is_displayed_as_folder, is_folder, is_item_by_name, is_video, supports_ancestors,
        supports_inherited_parent_images, supports_people, supports_played_status,
        supports_similarity, supports_theme_media,
    };
    use ferrofin_model::data::BaseItemKind;

    // The controller guard: `item is Episode || (item is IItemByName &&
    // item is not MusicArtist)` short-circuits; everything else proceeds.
    #[test]
    fn similarity_guard_matches_the_controller_rule() {
        for kind in [
            BaseItemKind::Episode,
            BaseItemKind::Genre,
            BaseItemKind::MusicGenre,
            BaseItemKind::Studio,
            BaseItemKind::Year,
            BaseItemKind::Person,
        ] {
            assert!(!supports_similarity(kind), "{kind:?} must short-circuit");
        }
        for kind in [
            BaseItemKind::MusicArtist,
            BaseItemKind::Movie,
            BaseItemKind::Series,
            BaseItemKind::MusicAlbum,
            BaseItemKind::Audio,
            BaseItemKind::Trailer,
            BaseItemKind::BoxSet,
        ] {
            assert!(supports_similarity(kind), "{kind:?} must proceed");
        }
    }

    #[test]
    fn folders_match_the_csharp_hierarchy() {
        assert!(is_folder(BaseItemKind::Folder));
        assert!(is_folder(BaseItemKind::Series));
        assert!(is_folder(BaseItemKind::MusicAlbum));
        assert!(is_folder(BaseItemKind::BoxSet));
        assert!(!is_folder(BaseItemKind::Movie));
        assert!(!is_folder(BaseItemKind::Audio));
        assert!(!is_folder(BaseItemKind::Genre));
    }

    #[test]
    fn displayed_as_folder_tracks_is_folder() {
        for kind in [
            BaseItemKind::Series,
            BaseItemKind::Movie,
            BaseItemKind::Genre,
        ] {
            assert_eq!(is_displayed_as_folder(kind), is_folder(kind));
        }
    }

    #[test]
    fn videos_support_people_and_theme_media() {
        for kind in [
            BaseItemKind::Movie,
            BaseItemKind::Episode,
            BaseItemKind::Video,
            BaseItemKind::MusicVideo,
            BaseItemKind::Trailer,
        ] {
            assert!(is_video(kind));
            assert!(supports_people(kind));
            assert!(supports_theme_media(kind));
            assert!(supports_inherited_parent_images(kind));
        }
        // Audio is not a video and does not support people.
        assert!(!supports_people(BaseItemKind::Audio));
    }

    #[test]
    fn folders_support_theme_media_but_not_people() {
        assert!(supports_theme_media(BaseItemKind::Series));
        assert!(!supports_people(BaseItemKind::Series));
    }

    #[test]
    fn audio_music_and_similarity_helpers() {
        use super::{is_audio, is_music, supports_similarity};
        assert!(is_audio(BaseItemKind::Audio));
        assert!(is_audio(BaseItemKind::AudioBook));
        assert!(!is_audio(BaseItemKind::Movie));

        assert!(is_music(BaseItemKind::Audio));
        assert!(is_music(BaseItemKind::MusicAlbum));
        assert!(is_music(BaseItemKind::MusicArtist));
        assert!(!is_music(BaseItemKind::Movie));

        assert!(supports_similarity(BaseItemKind::Movie));
        assert!(supports_similarity(BaseItemKind::MusicAlbum));
        assert!(!supports_similarity(BaseItemKind::Genre));
        // The controller guard stops only episodes and by-name kinds; a plain
        // folder proceeds (and then finds no local provider).
        assert!(supports_similarity(BaseItemKind::Folder));
    }

    #[test]
    fn structural_and_by_name_kinds_cannot_be_deleted() {
        use super::can_delete;
        for kind in [
            BaseItemKind::UserRootFolder,
            BaseItemKind::AggregateFolder,
            BaseItemKind::CollectionFolder,
            BaseItemKind::UserView,
            BaseItemKind::ManualPlaylistsFolder,
            BaseItemKind::Genre,
            BaseItemKind::Year,
            BaseItemKind::Person,
        ] {
            assert!(!can_delete(kind, true), "{kind:?}");
        }
        // `MusicArtist.CanDelete => !IsAccessedByName`.
        assert!(!can_delete(BaseItemKind::MusicArtist, false));
        assert!(can_delete(BaseItemKind::MusicArtist, true));
        for kind in [
            BaseItemKind::Movie,
            BaseItemKind::Episode,
            BaseItemKind::Series,
            BaseItemKind::BoxSet,
            BaseItemKind::Playlist,
            BaseItemKind::Folder,
        ] {
            assert!(can_delete(kind, false), "{kind:?}");
        }
    }

    #[test]
    fn by_name_kinds_do_not_support_ancestors_or_played_status() {
        for kind in [
            BaseItemKind::Genre,
            BaseItemKind::MusicGenre,
            BaseItemKind::Studio,
            BaseItemKind::Year,
            BaseItemKind::Person,
            BaseItemKind::MusicArtist,
        ] {
            assert!(is_item_by_name(kind));
            assert!(!supports_ancestors(kind));
            assert!(!supports_played_status(kind));
        }
        assert!(supports_ancestors(BaseItemKind::Movie));
        assert!(supports_played_status(BaseItemKind::Movie));
    }
}
