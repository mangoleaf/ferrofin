//! `BaseItem` domain behavior, ported as free functions over [`BaseItemKind`].
//!
//! In Jellyfin the `BaseItem` → `Folder` → `Video` (etc.) class hierarchy carries
//! virtual boolean properties — `IsFolder`, `SupportsPeople`,
//! `SupportsThemeMedia`, … — that each concrete subclass overrides. Hermit has no
//! `BaseItem` object graph: item rows are [`hermit_db::entities::base_items::BaseItemEntity`]
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

use hermit_model::data::BaseItemKind;

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

/// Whether items of this kind participate in "similar items" / recommendation
/// queries (C# similarity providers register for `Movie`/`Series`/`Album`/
/// `Artist`/`Playlist`/`Audio`).
///
/// The by-name grouping kinds and pure containers are excluded — similarity is
/// only meaningful for real, comparable media (and the music album/artist
/// aggregates the instant-mix path treats as seeds).
#[must_use]
pub fn supports_similarity(kind: BaseItemKind) -> bool {
    matches!(
        kind,
        BaseItemKind::Movie
            | BaseItemKind::Series
            | BaseItemKind::MusicAlbum
            | BaseItemKind::MusicArtist
            | BaseItemKind::Playlist
            | BaseItemKind::Audio
    )
}

#[cfg(test)]
mod tests {
    use super::{
        is_displayed_as_folder, is_folder, is_item_by_name, is_video, supports_ancestors,
        supports_inherited_parent_images, supports_people, supports_played_status,
        supports_theme_media,
    };
    use hermit_model::data::BaseItemKind;

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
        assert!(!supports_similarity(BaseItemKind::Folder));
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
