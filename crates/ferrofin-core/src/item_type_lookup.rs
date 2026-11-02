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

use ferrofin_model::data::{BASE_ITEM_KIND_NAMES, BaseItemKind};
use ferrofin_traits::persistence::ItemTypeLookup as ItemTypeLookupTrait;

/// The fully-qualified stored type names for the music-related kinds
/// (C# `ItemTypeLookup.MusicGenreTypes`).
pub(crate) const MUSIC_GENRE_TYPES: &[&str] = &[
    "MediaBrowser.Controller.Entities.Audio.Audio",
    "MediaBrowser.Controller.Entities.MusicVideo",
    "MediaBrowser.Controller.Entities.Audio.MusicAlbum",
    "MediaBrowser.Controller.Entities.Audio.MusicArtist",
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
    kind.stored_type_name()
}

/// `ServerConfiguration.EnableCaseSensitiveItemIds` (default **true**): when
/// false every item id key is lowercased (`GetNewItemIdInternal`).
static CASE_SENSITIVE_ITEM_IDS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// `ServerConfiguration.EnableNormalizedItemByNameIds` (default **true**): when
/// true a by-name item's id key is lowercased even if item ids are otherwise
/// case-sensitive (`GetItemByNameId`).
static NORMALIZED_BY_NAME_IDS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// Publishes the two server settings that shape item ids. The configuration
/// manager calls it when the configuration loads and whenever it changes, so
/// the next derivation sees the saved values as upstream's live reads do.
/// Changing them changes the ids derived from then on; nothing rewrites ids
/// already stored.
pub fn set_item_id_policy(case_sensitive_item_ids: bool, normalized_by_name_ids: bool) {
    use std::sync::atomic::Ordering::Relaxed;
    CASE_SENSITIVE_ITEM_IDS.store(case_sensitive_item_ids, Relaxed);
    NORMALIZED_BY_NAME_IDS.store(normalized_by_name_ids, Relaxed);
}

/// Whether an item-id key is lowercased: always for a by-name id when
/// `normalized_by_name_ids`, and for any id when ids are not case-sensitive.
fn lowercases_key(by_name: bool) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    key_is_lowercased(
        CASE_SENSITIVE_ITEM_IDS.load(Relaxed),
        NORMALIZED_BY_NAME_IDS.load(Relaxed),
        by_name,
    )
}

/// `GetNewItemIdInternal`'s `forceCaseInsensitive || !EnableCaseSensitiveItemIds`,
/// where only a by-name id forces case-insensitivity (with
/// `EnableNormalizedItemByNameIds`).
const fn key_is_lowercased(case_sensitive: bool, normalized_by_name: bool, by_name: bool) -> bool {
    !case_sensitive || (by_name && normalized_by_name)
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
            let key = rewritten.unwrap_or_else(|| path.to_owned());
            if lowercases_key(false) {
                ferrofin_util::string_extensions::lower_invariant(&key)
            } else {
                key
            }
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
    let valid = item_by_name_folder_name(name);
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
/// `metadata\Year\2026`). [`IdDerivation::LegacyLowercase`] retains Rust's
/// original full lowercase mapping so existing legacy identities stay stable.
#[must_use]
pub fn by_name_item_id(mode: &IdDerivation, kind: BaseItemKind, path: &str) -> Option<uuid::Uuid> {
    by_name_item_id_with_casing(mode, kind, path, false)
}

/// Previous Rust-full-lowercase identity, used only to retain existing rows.
pub(crate) fn previous_by_name_item_id(
    mode: &IdDerivation,
    kind: BaseItemKind,
    path: &str,
) -> Option<uuid::Uuid> {
    by_name_item_id_with_casing(mode, kind, path, true)
}

fn by_name_item_id_with_casing(
    mode: &IdDerivation,
    kind: BaseItemKind,
    path: &str,
    previous: bool,
) -> Option<uuid::Uuid> {
    let type_name = stored_type_name(kind)?;
    let key = match mode {
        IdDerivation::Jellyfin { program_data_path } => {
            let rewritten = program_data_path
                .as_deref()
                .and_then(|data| path.strip_prefix(data))
                .map(|rel| rel.trim_start_matches(['/', '\\']).replace('/', "\\"));
            let key = rewritten.unwrap_or_else(|| path.to_owned());
            if previous {
                key.to_lowercase()
            } else if lowercases_key(true) {
                ferrofin_util::string_extensions::lower_invariant(&key)
            } else {
                key
            }
        }
        IdDerivation::LegacyLowercase => path.to_lowercase(),
    };
    Some(ferrofin_common::extensions::get_md5(&format!(
        "{type_name}{key}"
    )))
}

/// The metadata path identifying a by-name item — port of the
/// `Genre`/`MusicGenre`/`Studio`/`MusicArtist`/`Year` `GetPath(name)` family,
/// every one of which is
/// `Path.Combine(<Kind>Path, FileSystem.GetValidFilename(name).Trim().TrimEnd('.'))`:
/// the filename-sanitized name directly under the kind's metadata dir, with no
/// first-letter subfolder (unlike people).
#[must_use]
pub fn by_name_path(root: &str, name: &str) -> String {
    let valid = item_by_name_folder_name(name);
    format!("{root}/{valid}")
}

/// [`by_name_path`] for the `Year` metadata dir — `Year.GetPath(name)`.
#[must_use]
pub fn year_path(year_root: &str, name: &str) -> String {
    by_name_path(year_root, name)
}

/// The deterministic by-name `Year` item id for `name` (the production year
/// as a string) — Jellyfin's `GetItemByNameId<Year>(Year.GetPath(name))`.
#[must_use]
pub fn year_item_id(mode: &IdDerivation, year_root: &str, name: &str) -> Option<uuid::Uuid> {
    by_name_item_id(mode, BaseItemKind::Year, &year_path(year_root, name))
}

/// The id of a season with no folder of its own (a series' episodes sit in
/// its folder, or in a folder that names no season) — the port of
/// `SeriesMetadataService.CreateSeasonAsync`, which keys it by its series,
/// number and name rather than a path: `GetNewItemId(series.Id +
/// (seasonNumber ?? -1).ToString(InvariantCulture) + seasonName,
/// typeof(Season))`. `series.Id` concatenates in `Guid`'s
/// default format (lowercase, hyphenated) and the name is the one the season
/// was created with. Under [`IdDerivation::Jellyfin`] the key keeps its case
/// (`EnableCaseSensitiveItemIds`), as every other scanned id; verified
/// against every path-less season of a real 12.1 database.
#[must_use]
pub fn virtual_season_id(
    mode: &IdDerivation,
    series_id: uuid::Uuid,
    number: Option<i32>,
    name: &str,
) -> Option<uuid::Uuid> {
    let key = format!("{series_id}{}{name}", number.unwrap_or(-1));
    derive_item_id_with(mode, BaseItemKind::Season, &key)
}

/// The id older Ferrofin scans gave a season with no folder of its own: the
/// one derived from the synthetic path `{series_dir}/#virtual-season-{n}`
/// (`-1` for no number). The boot repair
/// ([`crate::adoption_repairs::rekey_virtual_seasons`]) recognises such a
/// season by it and moves it to its [`virtual_season_id`].
#[must_use]
pub fn legacy_virtual_season_id(
    mode: &IdDerivation,
    series_dir: &str,
    number: Option<i64>,
) -> Option<uuid::Uuid> {
    let path = format!("{series_dir}/#virtual-season-{}", number.unwrap_or(-1));
    derive_item_id_with(mode, BaseItemKind::Season, &path)
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

/// The `AggregateFolder` item id for the physical root directory —
/// `GetNewItemId(RootFolderPath, typeof(AggregateFolder))`, the row
/// `LibraryManager.CreateRootFolder()` materializes at `{program data}/root`.
///
/// This is the parent every `BasePluginFolder` (the playlists folder) and every
/// physical library folder hangs off on a 10.11.8 database, so the derivation
/// has to reproduce Jellyfin's id exactly — it does: md5 over UTF-16LE of
/// `"MediaBrowser.Controller.Entities.AggregateFolder" + "root"` is
/// `f27caa37-e514-2225-cced-ed48f6553502`, the id a live 10.11.8 serves.
#[must_use]
pub fn aggregate_folder_id(mode: &IdDerivation, root_folder_path: &str) -> Option<uuid::Uuid> {
    derive_item_id_with(mode, BaseItemKind::AggregateFolder, root_folder_path)
}

/// The most UTF-8 bytes a by-name entity's folder name may take
/// (`BaseItem.MaxItemByNameFolderNameBytes`): well under the 255 bytes of
/// common Linux filesystems, so the files inside still fit `MAX_PATH`.
const MAX_ITEM_BY_NAME_FOLDER_NAME_BYTES: usize = 128;

/// `BaseItem.GetItemByNameFolderName`: [`valid_filename`], and — for a name
/// that would not fit a path component — a readable prefix, `-`, and the MD5
/// of the whole sanitized name, in at most 128 UTF-8 bytes. The cut is made on
/// UTF-16 units like the original and never splits a surrogate pair.
pub(crate) fn item_by_name_folder_name(name: &str) -> String {
    bound_folder_name(valid_filename(name))
}

/// The length-bounding half of [`item_by_name_folder_name`], over an already
/// sanitized name.
fn bound_folder_name(valid: String) -> String {
    if valid.len() <= MAX_ITEM_BY_NAME_FOLDER_NAME_BYTES {
        return valid;
    }
    let suffix = format!("-{}", ferrofin_common::extensions::get_md5(&valid).simple());
    let budget = MAX_ITEM_BY_NAME_FOLDER_NAME_BYTES - suffix.len();
    let units: Vec<u16> = valid.encode_utf16().collect();
    // UTF-8 length of a UTF-16 prefix; a lone surrogate counts as U+FFFD.
    let utf8_len = |end: usize| {
        char::decode_utf16(units[..end].iter().copied())
            .map(|unit| unit.map_or(3, char::len_utf8))
            .sum::<usize>()
    };
    let mut length = units.len().min(budget);
    while length > 0 && utf8_len(length) > budget {
        length -= 1;
    }
    if length > 0 && (0xD800..0xDC00).contains(&units[length - 1]) {
        length -= 1;
    }
    let prefix = String::from_utf16_lossy(&units[..length]);
    format!("{}{suffix}", prefix.trim_end().trim_end_matches('.'))
}

/// `FileSystem.GetValidFilename(name).Trim().TrimEnd('.')` — the sanitized
/// single path segment every by-name `GetPath` builds from an item name.
pub(crate) fn valid_filename(name: &str) -> String {
    get_valid_filename(name)
        .trim()
        .trim_end_matches('.')
        .to_owned()
}

/// `ManagedFileSystem.GetValidFilename(name)`: every invalid path character
/// replaced by a space, nothing trimmed.
pub(crate) use ferrofin_util::path_helper::get_valid_filename;

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
    BaseItemKind::from_stored_type_name(type_name)
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
    use super::{bound_folder_name, by_name_path, item_by_name_folder_name, key_is_lowercased};

    /// The four settings combinations: a scanned id is lowercased only when
    /// ids are not case-sensitive; a by-name id also when normalized.
    #[test]
    fn id_key_casing_follows_both_settings() {
        for (case_sensitive, normalized, scanned, by_name) in [
            (true, true, false, true),
            (true, false, false, false),
            (false, true, true, true),
            (false, false, true, true),
        ] {
            assert_eq!(
                key_is_lowercased(case_sensitive, normalized, false),
                scanned,
                "scanned: case_sensitive={case_sensitive} normalized={normalized}"
            );
            assert_eq!(
                key_is_lowercased(case_sensitive, normalized, true),
                by_name,
                "by-name: case_sensitive={case_sensitive} normalized={normalized}"
            );
        }
    }

    /// `BaseItem.GetItemByNameFolderName` against the upstream method run on
    /// .NET 10 (a verbatim copy; the names hold no invalid file-name characters).
    #[test]
    fn by_name_folder_names_match_dotnet() {
        let cases: Vec<(String, String)> = serde_json::from_str(include_str!(
            "../tests/fixtures/item_by_name_folder_names.json"
        ))
        .unwrap();
        assert!(cases.len() > 30);
        for (name, expected) in cases {
            // The oracle's file system passes names through, as upstream's
            // test fixture does; the bound applies to the trimmed name.
            let actual = bound_folder_name(name.trim().trim_end_matches('.').to_owned());
            assert_eq!(actual, expected, "{name:?}");
            assert!(actual.len() <= 128, "{name:?}");
        }
    }

    /// The upstream xUnit cases (`BaseItemTests.GetItemByNameFolderName_*`).
    #[test]
    fn overlong_by_name_folder_names_stay_unique_and_stable() {
        assert_eq!(
            item_by_name_folder_name("Mairghread Scott."),
            "Mairghread Scott"
        );
        let credits = vec!["Jerry Siegel (created by Superman)"; 20].join(", ");
        let folder = item_by_name_folder_name(&credits);
        assert!(folder.len() <= 128);
        assert!(folder.starts_with("Jerry Siegel (created by Superman)"));
        let prefix = "a".repeat(200);
        assert_ne!(
            item_by_name_folder_name(&format!("{prefix}Joe Shuster")),
            item_by_name_folder_name(&format!("{prefix}Bob Kane"))
        );
        let long = "a".repeat(300);
        assert_eq!(
            item_by_name_folder_name(&long),
            item_by_name_folder_name(&long)
        );
        // Paths built from a long name use the bounded folder.
        let path = by_name_path("/meta/Genre", &long);
        assert_eq!(
            path.rsplit('/').next().unwrap(),
            item_by_name_folder_name(&long)
        );
    }

    #[rstest::rstest]
    #[case("İpek", "İpek")]
    #[case("ΟΣ", "οσ")]
    #[case("𐐀", "𐐨")]
    fn unicode_by_name_identity(#[case] name: &str, #[case] lowered: &str) {
        let mode = IdDerivation::Jellyfin {
            program_data_path: Some("/data".to_owned()),
        };
        let path = format!("/data/metadata/Genre/{name}");
        let expected = ferrofin_common::extensions::get_md5(&format!(
            "MediaBrowser.Controller.Entities.Genremetadata\\genre\\{lowered}"
        ));
        assert_eq!(
            super::by_name_item_id(&mode, BaseItemKind::Genre, &path),
            Some(expected)
        );
        let legacy = IdDerivation::LegacyLowercase;
        assert_eq!(
            super::by_name_item_id(&legacy, BaseItemKind::Genre, &path),
            super::previous_by_name_item_id(&legacy, BaseItemKind::Genre, &path)
        );
    }

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

    /// Path-less seasons of a real Jellyfin 12.1 database (the adoption
    /// fixture upgraded from 12.0; created by 10.11.x and 12.x alike), as
    /// stored: `(Id, SeriesId, IndexNumber, Name)`. All 591 of its
    /// path-less seasons derive this way.
    #[rstest::rstest]
    #[case(
        "CC86EE2C-C78D-0F79-0BCE-1F8F8ADBB846",
        "79F46D55-6061-D8B9-EF8A-1C2FFD0D6856",
        Some(0),
        "Specials"
    )]
    #[case(
        "5D4284B7-8FBC-069E-80C2-644AED45F8B1",
        "79F46D55-6061-D8B9-EF8A-1C2FFD0D6856",
        Some(1),
        "Season 1"
    )]
    #[case(
        "59586EC5-7224-BFF1-5BA9-7E133D666050",
        "79F46D55-6061-D8B9-EF8A-1C2FFD0D6856",
        Some(12),
        "Season 12"
    )]
    #[case(
        "B8FFA3AC-6431-B12D-9ACE-B10DA99149BE",
        "C3B1F536-2827-4088-EB52-C3518C9BFB9F",
        Some(0),
        "Specials"
    )]
    #[case(
        "09293A86-BD86-819E-77DB-44AFA1F265EF",
        "27A96C9A-7E57-DD56-A051-40190E9CF47F",
        None,
        "Season Unknown"
    )]
    fn virtual_season_id_matches_a_real_database(
        #[case] id: &str,
        #[case] series: &str,
        #[case] number: Option<i32>,
        #[case] name: &str,
    ) {
        let mode = IdDerivation::Jellyfin {
            program_data_path: Some("/config".to_owned()),
        };
        let series = uuid::Uuid::parse_str(series).expect("uuid");
        assert_eq!(
            super::virtual_season_id(&mode, series, number, name),
            Some(uuid::Uuid::parse_str(id).expect("uuid"))
        );
    }

    /// The id older Ferrofin scans gave a path-less season is the synthetic
    /// path's, under either derivation.
    #[test]
    fn legacy_virtual_season_id_is_the_synthetic_paths() {
        for mode in [
            IdDerivation::Jellyfin {
                program_data_path: None,
            },
            IdDerivation::LegacyLowercase,
        ] {
            assert_eq!(
                super::legacy_virtual_season_id(&mode, "/tv/Show", Some(2)),
                derive_item_id_with(&mode, BaseItemKind::Season, "/tv/Show/#virtual-season-2")
            );
            assert_eq!(
                super::legacy_virtual_season_id(&mode, "/tv/Show", None),
                derive_item_id_with(&mode, BaseItemKind::Season, "/tv/Show/#virtual-season--1")
            );
        }
    }

    /// Oracle values from the same real 10.11.8 database: the `UserRootFolder`
    /// row at `/config/root/default` and a `Year` row at
    /// `/config/metadata/Year/2026` (stored as `%MetadataPath%/Year/2026`).
    #[test]
    fn root_and_year_ids_match_a_real_database() {
        use super::{
            aggregate_folder_id, by_name_item_id, user_root_folder_id, year_item_id, year_path,
        };
        let mode = IdDerivation::Jellyfin {
            program_data_path: Some("/config".to_owned()),
        };
        assert_eq!(
            user_root_folder_id(&mode, "/config/root/default"),
            Some(uuid::Uuid::parse_str("E9D5075A-555C-1CBC-394E-EC4CEF295274").expect("uuid")),
        );
        // The `AggregateFolder` at `{program data}/root` — `CreateRootFolder()`.
        // Read off a live 10.11.8 database (`BaseItems` row `root`).
        assert_eq!(
            aggregate_folder_id(&mode, "/config/root"),
            Some(uuid::Uuid::parse_str("F27CAA37-E514-2225-CCED-ED48F6553502").expect("uuid")),
        );
        // …and the playlists folder it owns. 10.11.8 stores
        // `Emby.Server.Implementations.Playlists.PlaylistsFolder`; the
        // `ManualPlaylistsFolder` spelling is only `GetClientTypeName()` and
        // hashing it yields a DIFFERENT id, which is how a second playlists
        // folder appears beside Jellyfin's on an adopted database.
        assert_eq!(
            derive_item_id_with(
                &mode,
                BaseItemKind::PlaylistsFolder,
                "/config/data/playlists"
            ),
            Some(uuid::Uuid::parse_str("1071671E-7BFF-A053-2E93-0DEBEE501D2E").expect("uuid")),
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
