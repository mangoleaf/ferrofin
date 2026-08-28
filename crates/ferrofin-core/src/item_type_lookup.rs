//! [`ItemTypeLookup`] — the static kind → stored-type-name tables.
//!
//! Port of `Emby.Server.Implementations.Data.ItemTypeLookup`. The `BaseItems.Type`
//! column stores the fully-qualified .NET class name of the item's `BaseItem`
//! subclass (e.g. `MediaBrowser.Controller.Entities.Movies.Movie`). Ferrofin keeps
//! those exact strings so a Jellyfin database round-trips: the query translator
//! turns [`BaseItemKind`] filters into `Type = '<fqn>'` predicates, and the
//! mappers read the column back into a kind.
//!
//! The names are copied verbatim from the C# `typeof(T).FullName!` values so the
//! on-disk representation is byte-for-byte identical.

use std::collections::HashMap;
use std::sync::OnceLock;

use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::persistence::ItemTypeLookup as ItemTypeLookupTrait;

/// The fully-qualified stored type names for the music-related kinds
/// (C# `ItemTypeLookup.MusicGenreTypes`).
pub(crate) const MUSIC_GENRE_TYPES: &[&str] = &[
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
        BaseItemKind::ManualPlaylistsFolder,
        "Emby.Server.Implementations.Playlists.ManualPlaylistsFolder",
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

/// How stable item ids are derived from kind + path — a **per-database** mode.
///
/// Jellyfin 10.11.8 derives `MD5(TypeFullName + key)` over UTF-16LE where
/// `key` keeps its case (`EnableCaseSensitiveItemIds` defaults to **true**)
/// and paths under `ProgramDataPath` are rewritten relative (`strip prefix`,
/// trim `/\`, `/`→`\`) for machine independence — verified byte-for-byte
/// against a real 10.11.8 database. Early Ferrofin lowercased the path and
/// skipped the rewrite; databases scanned that way keep their ids via
/// [`IdDerivation::LegacyLowercase`] (stored in `FerrofinMeta`,
/// `item_id_derivation`), while fresh and adopted databases use
/// [`IdDerivation::Jellyfin`] so scans converge on Jellyfin's ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdDerivation {
    /// Jellyfin 10.11.8 parity: case-sensitive, data-dir-relative rewrite.
    Jellyfin {
        /// The `ProgramDataPath` equivalent (Ferrofin's data dir), if known.
        program_data_path: Option<String>,
    },
    /// Pre-parity Ferrofin behavior: lowercased path, no rewrite. Grandfathered
    /// for databases that already carry ids derived this way.
    LegacyLowercase,
}

impl IdDerivation {
    /// The `FerrofinMeta.item_id_derivation` value naming this mode.
    #[must_use]
    pub fn meta_value(&self) -> &'static str {
        match self {
            Self::Jellyfin { .. } => "jellyfin-10.11.8",
            Self::LegacyLowercase => "legacy-lowercase",
        }
    }

    /// Resolves the mode from a stored `FerrofinMeta` value (`None`/unknown ⇒
    /// Jellyfin parity, the correct default for fresh and adopted databases).
    #[must_use]
    pub fn from_meta(value: Option<&str>, program_data_path: Option<String>) -> Self {
        match value {
            Some("legacy-lowercase") => Self::LegacyLowercase,
            _ => Self::Jellyfin { program_data_path },
        }
    }
}

/// Derives a scanned item's stable `Guid` from its kind + filesystem path
/// under the given [`IdDerivation`] — the port of Jellyfin's
/// `LibraryManager.GetNewItemIdInternal` (`key = TypeFullName + path`, MD5
/// over the UTF-16LE bytes → `Guid`). Returns [`None`] for a kind with no
/// stored type name.
#[must_use]
pub fn derive_item_id_with(
    mode: &IdDerivation,
    kind: BaseItemKind,
    path: &str,
) -> Option<uuid::Uuid> {
    let type_name = stored_type_name(kind)?;
    let key = match mode {
        IdDerivation::Jellyfin { program_data_path } => {
            let rewritten = program_data_path
                .as_deref()
                .and_then(|data| path.strip_prefix(data))
                .map(|rel| rel.trim_start_matches(['/', '\\']).replace('/', "\\"));
            rewritten.unwrap_or_else(|| path.to_owned())
        }
        IdDerivation::LegacyLowercase => path.to_lowercase(),
    };
    Some(ferrofin_common::extensions::get_md5(&format!(
        "{type_name}{key}"
    )))
}

/// The virtual metadata path identifying a person by name — port of
/// `Person.GetPath(name)`: the filename-sanitized name (invalid path chars →
/// space, trimmed, trailing periods dropped), prefixed by its first
/// alphanumeric character as a subfolder, under the People metadata dir.
///
/// This path is the *identity key* for a person's by-name item: Jellyfin
/// derives the item id from it (`GetItemByNameId<Person>`), so one name maps
/// to exactly one id — the property Ferrofin's per-(name, type) random ids
/// broke (favoriting wrote to one credit's id while every other surface
/// resolved a different one).
#[must_use]
pub fn person_path(people_path: &str, name: &str) -> String {
    let valid = valid_filename(name);
    match valid.chars().find(|c| c.is_alphanumeric()) {
        Some(prefix) => format!("{people_path}/{prefix}/{valid}"),
        None => format!("{people_path}/{valid}"),
    }
}

/// The deterministic by-name `Person` item id for `name` — Jellyfin's
/// `GetItemByNameId<Person>(Person.GetPath(name))`, one id per name. Like
/// every by-name id the key is case-normalized (see [`by_name_item_id`];
/// verified against a real 10.11.8 row: `Bob Parity` at
/// `/config/metadata/People/B/Bob Parity` is `F6FE1D96-…`, the lowercased key).
#[must_use]
pub fn person_item_id(mode: &IdDerivation, people_path: &str, name: &str) -> Option<uuid::Uuid> {
    by_name_item_id(mode, BaseItemKind::Person, &person_path(people_path, name))
}

/// Derives a *by-name* item's id from its metadata path — the port of
/// `LibraryManager.GetItemByNameId<T>(path)`, which is
/// `GetNewItemIdInternal(path, typeof(T), forceCaseInsensitive:
/// EnableNormalizedItemByNameIds)`. That setting defaults to `true`, so unlike
/// a scanned item's id the key is lowercased *after* the data-dir rewrite
/// (verified against a 10.11.8 database: `Year` `2026` at
/// `/config/metadata/Year/2026` hashes `metadata\year\2026`, not
/// `metadata\Year\2026`). Under [`IdDerivation::LegacyLowercase`] the key is
/// already lowercase, so both modes agree on the normalization.
#[must_use]
pub fn by_name_item_id(mode: &IdDerivation, kind: BaseItemKind, path: &str) -> Option<uuid::Uuid> {
    let type_name = stored_type_name(kind)?;
    let key = match mode {
        IdDerivation::Jellyfin { program_data_path } => {
            let rewritten = program_data_path
                .as_deref()
                .and_then(|data| path.strip_prefix(data))
                .map(|rel| rel.trim_start_matches(['/', '\\']).replace('/', "\\"));
            rewritten.unwrap_or_else(|| path.to_owned()).to_lowercase()
        }
        IdDerivation::LegacyLowercase => path.to_lowercase(),
    };
    Some(ferrofin_common::extensions::get_md5(&format!(
        "{type_name}{key}"
    )))
}

/// The metadata path identifying a `Year` by-name item — port of
/// `Year.GetPath(name)`: the filename-sanitized name (invalid path chars →
/// space, trimmed, trailing periods dropped) directly under the Year metadata
/// dir (`{metadata}/Year/{name}`; no first-letter subfolder, unlike people).
#[must_use]
pub fn year_path(year_root: &str, name: &str) -> String {
    let valid = valid_filename(name);
    format!("{year_root}/{valid}")
}

/// The deterministic by-name `Year` item id for `name` (the production year
/// as a string) — Jellyfin's `GetItemByNameId<Year>(Year.GetPath(name))`.
#[must_use]
pub fn year_item_id(mode: &IdDerivation, year_root: &str, name: &str) -> Option<uuid::Uuid> {
    by_name_item_id(mode, BaseItemKind::Year, &year_path(year_root, name))
}

/// The `UserRootFolder` item id for the default user-views directory —
/// `GetNewItemId(DefaultUserViewsPath, typeof(UserRootFolder))`, the one row
/// `Items/Root` resolves to (case-sensitive, like every scanned folder; an
/// adopted 10.11.8 database already carries exactly this id).
#[must_use]
pub fn user_root_folder_id(
    mode: &IdDerivation,
    default_user_views_path: &str,
) -> Option<uuid::Uuid> {
    derive_item_id_with(mode, BaseItemKind::UserRootFolder, default_user_views_path)
}

/// `FileSystem.GetValidFilename(name).Trim().TrimEnd('.')` — the sanitized
/// single path segment every by-name `GetPath` builds from an item name.
fn valid_filename(name: &str) -> String {
    // `ManagedFileSystem._invalidPathCharacters`, verbatim.
    const INVALID: &[char] = &['"', '<', '>', '|', ':', '*', '?', '\\', '/'];
    let sanitized: String = name
        .chars()
        .map(|c| {
            if INVALID.contains(&c) || (c as u32) <= 31 {
                ' '
            } else {
                c
            }
        })
        .collect();
    sanitized.trim().trim_end_matches('.').to_owned()
}

/// [`derive_item_id_with`] under [`IdDerivation::LegacyLowercase`] — kept for
/// call sites that have not been handed a per-database mode.
#[must_use]
pub fn derive_item_id(kind: BaseItemKind, path: &str) -> Option<uuid::Uuid> {
    derive_item_id_with(&IdDerivation::LegacyLowercase, kind, path)
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
    #[test]
    fn person_path_matches_person_get_path() {
        use super::person_path;
        // Plain name: subfoldered by its first alphanumeric char.
        assert_eq!(
            person_path("/data/metadata/People", "Steve Carell"),
            "/data/metadata/People/S/Steve Carell"
        );
        // Invalid path characters become spaces (GetValidFilename), trailing
        // periods drop, and the prefix is the first ALPHANUMERIC char.
        assert_eq!(
            person_path("/m/People", "A.C./D:C Jr."),
            "/m/People/A/A.C. D C Jr"
        );
        // A digit prefixes too (IsLetterOrDigit).
        assert_eq!(person_path("/m/People", "50 Cent"), "/m/People/5/50 Cent");
    }

    #[test]
    fn person_item_id_is_one_per_name() {
        use super::{IdDerivation, person_item_id};
        let mode = IdDerivation::Jellyfin {
            program_data_path: Some("/data".to_owned()),
        };
        let a = person_item_id(&mode, "/data/metadata/People", "Steve Carell");
        let b = person_item_id(&mode, "/data/metadata/People", "Steve Carell");
        let c = person_item_id(&mode, "/data/metadata/People", "Uma Thurman");
        assert!(a.is_some());
        assert_eq!(a, b, "same name, same id — regardless of credit type");
        assert_ne!(a, c);
    }

    use super::{
        IdDerivation, ItemTypeLookup, derive_item_id, derive_item_id_with, stored_type_name,
    };
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_traits::persistence::ItemTypeLookup as _;

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
        // Program/Recording/ChannelFolderItem are absent upstream too.
        assert_eq!(stored_type_name(BaseItemKind::Program), None);
    }

    #[test]
    fn manual_playlists_folder_has_a_stored_type() {
        // The auto-provisioned Playlists media folder is persisted, so it needs a
        // stored `BaseItems.Type` name (matching upstream's
        // `Emby.Server.Implementations.Playlists.ManualPlaylistsFolder`).
        assert_eq!(
            stored_type_name(BaseItemKind::ManualPlaylistsFolder),
            Some("Emby.Server.Implementations.Playlists.ManualPlaylistsFolder")
        );
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

    /// Oracle values captured from a real `jellyfin/jellyfin:10.11.8` database
    /// (the drop-in spike): the derivation must reproduce them byte-for-byte.
    #[test]
    fn jellyfin_derivation_matches_a_real_database() {
        let mode = IdDerivation::Jellyfin {
            program_data_path: Some("/config".to_owned()),
        };
        // A scanned movie: case-sensitive path, no rewrite (outside /config).
        assert_eq!(
            derive_item_id_with(
                &mode,
                BaseItemKind::Movie,
                "/media/synth/movies/Movie 0001 (2020)/Movie 0001 (2020).mkv",
            ),
            Some(uuid::Uuid::parse_str("D37ECB9D-75B0-C0A8-E9EC-B0A864EC670E").expect("uuid")),
        );
        // A collection folder under the data dir: relative + backslash rewrite.
        assert_eq!(
            derive_item_id_with(
                &mode,
                BaseItemKind::CollectionFolder,
                "/config/root/default/Movies",
            ),
            Some(uuid::Uuid::parse_str("F137A2DD-21BB-C1B9-9AA5-C0F6BF02A805").expect("uuid")),
        );
    }

    /// Oracle values from the same real 10.11.8 database: the `UserRootFolder`
    /// row at `/config/root/default` and a `Year` row at
    /// `/config/metadata/Year/2026` (stored as `%MetadataPath%/Year/2026`).
    #[test]
    fn root_and_year_ids_match_a_real_database() {
        use super::{by_name_item_id, user_root_folder_id, year_item_id, year_path};
        let mode = IdDerivation::Jellyfin {
            program_data_path: Some("/config".to_owned()),
        };
        assert_eq!(
            user_root_folder_id(&mode, "/config/root/default"),
            Some(uuid::Uuid::parse_str("E9D5075A-555C-1CBC-394E-EC4CEF295274").expect("uuid")),
        );
        assert_eq!(
            year_path("/config/metadata/Year", "2026"),
            "/config/metadata/Year/2026"
        );
        assert_eq!(
            year_item_id(&mode, "/config/metadata/Year", "2026"),
            Some(uuid::Uuid::parse_str("FEF3DB72-E066-FC0B-F454-F305935D09B6").expect("uuid")),
        );
        // Person rows from the same database (`%MetadataPath%/People/B/Bob Parity`).
        assert_eq!(
            super::person_item_id(&mode, "/config/metadata/People", "Bob Parity"),
            Some(uuid::Uuid::parse_str("F6FE1D96-890D-8ADF-ADCE-23D3E1D98AF8").expect("uuid")),
        );
        assert_eq!(
            super::person_item_id(&mode, "/config/metadata/People", "Alice Parity"),
            Some(uuid::Uuid::parse_str("712EF9D6-0328-A6AA-F7E3-61C34BA4D087").expect("uuid")),
        );
        // The by-name key is case-normalized (EnableNormalizedItemByNameIds);
        // the case-sensitive scanned-item derivation gives a different id.
        assert_ne!(
            by_name_item_id(&mode, BaseItemKind::Year, "/config/metadata/Year/2026"),
            derive_item_id_with(&mode, BaseItemKind::Year, "/config/metadata/Year/2026"),
        );
    }

    #[test]
    fn year_path_sanitizes_like_get_valid_filename() {
        use super::year_path;
        assert_eq!(year_path("/m/Year", " 19:99. "), "/m/Year/19 99");
    }

    #[test]
    fn legacy_derivation_is_the_old_lowercase_form() {
        // The grandfathered mode must keep producing pre-parity ids so
        // existing Ferrofin libraries stay self-consistent.
        let legacy = derive_item_id_with(
            &IdDerivation::LegacyLowercase,
            BaseItemKind::Movie,
            "/Media/Film.mkv",
        );
        assert_eq!(
            legacy,
            derive_item_id(BaseItemKind::Movie, "/media/film.mkv")
        );
        let jellyfin = derive_item_id_with(
            &IdDerivation::Jellyfin {
                program_data_path: None,
            },
            BaseItemKind::Movie,
            "/Media/Film.mkv",
        );
        assert_ne!(legacy, jellyfin, "case must matter in parity mode");
    }

    #[test]
    fn id_derivation_meta_round_trips() {
        assert_eq!(
            IdDerivation::from_meta(Some("legacy-lowercase"), None),
            IdDerivation::LegacyLowercase
        );
        let parity = IdDerivation::from_meta(None, Some("/data".to_owned()));
        assert_eq!(parity.meta_value(), "jellyfin-10.11.8");
        assert_eq!(
            IdDerivation::LegacyLowercase.meta_value(),
            "legacy-lowercase"
        );
    }
}
