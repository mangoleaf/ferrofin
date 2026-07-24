//! [`ItemTypeLookup`] — the static kind → stored-type-name tables.
//!
//! Port of `Emby.Server.Implementations.Data.ItemTypeLookup`. The `BaseItems.Type`
//! column stores the fully-qualified .NET class name of the item's `BaseItem`
//! subclass (e.g. `MediaBrowser.Controller.Entities.Movies.Movie`). Hermit keeps
//! those exact strings so a Jellyfin database round-trips: the query translator
//! turns [`BaseItemKind`] filters into `Type = '<fqn>'` predicates, and the
//! mappers read the column back into a kind.
//!
//! The names are copied verbatim from the C# `typeof(T).FullName!` values so the
//! on-disk representation is byte-for-byte identical.

use std::collections::HashMap;
use std::sync::OnceLock;

use hermit_model::data::BaseItemKind;
use hermit_traits::persistence::ItemTypeLookup as ItemTypeLookupTrait;

/// The fully-qualified stored type names for the music-related kinds
/// (C# `ItemTypeLookup.MusicGenreTypes`).
const MUSIC_GENRE_TYPES: &[&str] = &[
    "MediaBrowser.Controller.Entities.Audio.Audio",
    "MediaBrowser.Controller.Entities.MusicVideo",
    "MediaBrowser.Controller.Entities.Audio.MusicAlbum",
    "MediaBrowser.Controller.Entities.Audio.MusicArtist",
];

/// The `(kind, stored-type-name)` pairs, copied from the C#
/// `ItemTypeLookup.BaseItemKindNames` dictionary. Kinds with no dedicated
/// `BaseItem` subclass in the C# table (e.g. `AudioBook`, `Program`) are omitted,
/// exactly as upstream omits them.
const BASE_ITEM_KIND_NAMES: &[(BaseItemKind, &str)] = &[
    (
        BaseItemKind::AggregateFolder,
        "MediaBrowser.Controller.Entities.AggregateFolder",
    ),
    (
        BaseItemKind::Audio,
        "MediaBrowser.Controller.Entities.Audio.Audio",
    ),
    (
        BaseItemKind::AudioBook,
        "MediaBrowser.Controller.Entities.AudioBook",
    ),
    (
        BaseItemKind::BasePluginFolder,
        "MediaBrowser.Controller.Entities.BasePluginFolder",
    ),
    (BaseItemKind::Book, "MediaBrowser.Controller.Entities.Book"),
    (
        BaseItemKind::BoxSet,
        "MediaBrowser.Controller.Entities.Movies.BoxSet",
    ),
    (
        BaseItemKind::Channel,
        "MediaBrowser.Controller.Channels.Channel",
    ),
    (
        BaseItemKind::CollectionFolder,
        "MediaBrowser.Controller.Entities.CollectionFolder",
    ),
    (
        BaseItemKind::Episode,
        "MediaBrowser.Controller.Entities.TV.Episode",
    ),
    (
        BaseItemKind::Folder,
        "MediaBrowser.Controller.Entities.Folder",
    ),
    (
        BaseItemKind::Genre,
        "MediaBrowser.Controller.Entities.Genre",
    ),
    (
        BaseItemKind::Movie,
        "MediaBrowser.Controller.Entities.Movies.Movie",
    ),
    (
        BaseItemKind::LiveTvChannel,
        "MediaBrowser.Controller.LiveTv.LiveTvChannel",
    ),
    (
        BaseItemKind::LiveTvProgram,
        "MediaBrowser.Controller.LiveTv.LiveTvProgram",
    ),
    (
        BaseItemKind::MusicAlbum,
        "MediaBrowser.Controller.Entities.Audio.MusicAlbum",
    ),
    (
        BaseItemKind::MusicArtist,
        "MediaBrowser.Controller.Entities.Audio.MusicArtist",
    ),
    (
        BaseItemKind::MusicGenre,
        "MediaBrowser.Controller.Entities.Audio.MusicGenre",
    ),
    (
        BaseItemKind::MusicVideo,
        "MediaBrowser.Controller.Entities.MusicVideo",
    ),
    (
        BaseItemKind::Person,
        "MediaBrowser.Controller.Entities.Person",
    ),
    (
        BaseItemKind::Photo,
        "MediaBrowser.Controller.Entities.Photo",
    ),
    (
        BaseItemKind::PhotoAlbum,
        "MediaBrowser.Controller.Entities.PhotoAlbum",
    ),
    (
        BaseItemKind::Playlist,
        "MediaBrowser.Controller.Playlists.Playlist",
    ),
    (
        BaseItemKind::PlaylistsFolder,
        "Emby.Server.Implementations.Playlists.PlaylistsFolder",
    ),
    (
        BaseItemKind::Season,
        "MediaBrowser.Controller.Entities.TV.Season",
    ),
    (
        BaseItemKind::Series,
        "MediaBrowser.Controller.Entities.TV.Series",
    ),
    (
        BaseItemKind::Studio,
        "MediaBrowser.Controller.Entities.Studio",
    ),
    (
        BaseItemKind::Trailer,
        "MediaBrowser.Controller.Entities.Trailer",
    ),
    (
        BaseItemKind::TvChannel,
        "MediaBrowser.Controller.LiveTv.LiveTvChannel",
    ),
    (
        BaseItemKind::TvProgram,
        "MediaBrowser.Controller.LiveTv.LiveTvProgram",
    ),
    (
        BaseItemKind::UserRootFolder,
        "MediaBrowser.Controller.Entities.UserRootFolder",
    ),
    (
        BaseItemKind::UserView,
        "MediaBrowser.Controller.Entities.UserView",
    ),
    (
        BaseItemKind::Video,
        "MediaBrowser.Controller.Entities.Video",
    ),
    (BaseItemKind::Year, "MediaBrowser.Controller.Entities.Year"),
];

/// Returns the shared kind → stored-type-name map, built once on first use.
///
/// The query translator and mappers hit this on nearly every query, so it is
/// memoized in a [`OnceLock`] rather than rebuilt per call.
fn kind_names() -> &'static HashMap<BaseItemKind, String> {
    static MAP: OnceLock<HashMap<BaseItemKind, String>> = OnceLock::new();
    MAP.get_or_init(|| {
        BASE_ITEM_KIND_NAMES
            .iter()
            .map(|(kind, name)| (*kind, (*name).to_owned()))
            .collect()
    })
}

/// Looks up the stored `BaseItems.Type` name for a kind, or [`None`] if the kind
/// has no dedicated stored type (matching the C# dictionary's coverage).
#[must_use]
pub fn stored_type_name(kind: BaseItemKind) -> Option<&'static str> {
    BASE_ITEM_KIND_NAMES
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, name)| *name)
}

/// Derives a scanned item's stable `Guid` from its kind + filesystem path — the
/// port of Jellyfin's `LibraryManager.GetNewItemIdInternal`
/// (`key = TypeFullName + path`, MD5 over the UTF-16LE bytes → `Guid`).
///
/// The path is lowercased, matching Jellyfin's default
/// `EnableCaseSensitiveItemIds = false`. Returns [`None`] for a kind with no
/// stored type name.
///
/// ponytail: no `ProgramDataPath`-relative rewrite / backslash normalization
/// (those matter only for cross-install id parity on Windows); add if we ever
/// import a foreign Jellyfin database.
#[must_use]
pub fn derive_item_id(kind: BaseItemKind, path: &str) -> Option<uuid::Uuid> {
    let type_name = stored_type_name(kind)?;
    let key = format!("{type_name}{}", path.to_lowercase());
    Some(hermit_common::extensions::get_md5(&key))
}

/// The inverse of [`stored_type_name`]: maps a stored `BaseItems.Type` name back
/// to its [`BaseItemKind`], or [`None`] for an unrecognized name.
///
/// The single reverse lookup over [`BASE_ITEM_KIND_NAMES`], shared by every
/// consumer that materializes a kind from a persisted row (the user-data
/// heuristics, search-hint mapping, …) so the mapping is spelled out once.
#[must_use]
pub fn kind_from_type_name(type_name: &str) -> Option<BaseItemKind> {
    BASE_ITEM_KIND_NAMES
        .iter()
        .find(|(_, name)| *name == type_name)
        .map(|(kind, _)| *kind)
}

/// The static item-kind lookup tables.
///
/// Concrete implementation of [`ItemTypeLookupTrait`]. Zero-sized: all data is
/// the shared static tables, so it is trivially cloneable and shareable behind an
/// `Arc<dyn ItemTypeLookup>`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ItemTypeLookup;

impl ItemTypeLookup {
    /// Creates the lookup.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl ItemTypeLookupTrait for ItemTypeLookup {
    fn music_genre_types(&self) -> Vec<String> {
        MUSIC_GENRE_TYPES.iter().map(|s| (*s).to_owned()).collect()
    }

    fn base_item_kind_names(&self) -> HashMap<BaseItemKind, String> {
        kind_names().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{ItemTypeLookup, stored_type_name};
    use hermit_model::data::BaseItemKind;
    use hermit_traits::persistence::ItemTypeLookup as _;

    #[test]
    fn maps_known_kinds_to_fqns() {
        assert_eq!(
            stored_type_name(BaseItemKind::Movie),
            Some("MediaBrowser.Controller.Entities.Movies.Movie")
        );
        assert_eq!(
            stored_type_name(BaseItemKind::Episode),
            Some("MediaBrowser.Controller.Entities.TV.Episode")
        );
    }

    #[test]
    fn omits_kinds_without_a_stored_type() {
        // Program/Recording/ChannelFolderItem/ManualPlaylistsFolder are absent
        // upstream too.
        assert_eq!(stored_type_name(BaseItemKind::Program), None);
        assert_eq!(stored_type_name(BaseItemKind::ManualPlaylistsFolder), None);
    }

    #[test]
    fn trait_surface_matches_static_tables() {
        let lookup = ItemTypeLookup::new();
        let names = lookup.base_item_kind_names();
        assert_eq!(
            names.get(&BaseItemKind::Series).map(String::as_str),
            Some("MediaBrowser.Controller.Entities.TV.Series")
        );
        assert_eq!(lookup.music_genre_types().len(), 4);
    }

    #[test]
    fn tv_channel_aliases_live_tv_channel() {
        assert_eq!(
            stored_type_name(BaseItemKind::TvChannel),
            stored_type_name(BaseItemKind::LiveTvChannel)
        );
    }
}
