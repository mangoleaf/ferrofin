//! Foundational provider container types — port of the
//! `MediaBrowser.Controller.Providers` value types used by the refresh
//! pipeline and the local (NFO/image) metadata parsers.
//!
//! These are pure data types (no I/O). Ports:
//! - [`MetadataResult`] — `MediaBrowser.Controller.Providers.MetadataResult<T>`.
//! - [`PersonInfo`] — `MediaBrowser.Controller.Entities.PersonInfo` (the small
//!   person stub attached to library items).
//! - [`ItemInfo`] — `MediaBrowser.Controller.Providers.ItemInfo` (the save-path
//!   resolution wrapper handed to providers).
//! - [`LocalImageInfo`] — `MediaBrowser.Controller.Providers.LocalImageInfo`.
//! - [`FileSystemMetadata`] — `MediaBrowser.Model.IO.FileSystemMetadata`
//!   (deliberately dropped from `hermit-model` as server-side plumbing, so it is
//!   re-created locally here for [`LocalImageInfo`]).
//! - [`NfoItem`] — the union field-bag the NFO parsers populate, with
//!   [`NfoItem::set_provider_id`] applying the parser's provider-id
//!   normalization (`SetProviderId`).
//! - [`RefreshResult`] — `MediaBrowser.Providers.Manager.RefreshResult`.
//!
//! `PersonInfo`/`ItemInfo`/`LocalImageInfo`/`MetadataResult` are
//! `MediaBrowser.Controller.Providers` types **not** present in `hermit-model`,
//! so they are created locally here. The person-add merge/normalize logic is
//! ported as [`add_person`] (C# `PeopleHelper.AddPerson`).

use std::collections::HashMap;
use std::hash::BuildHasher;

use hermit_model::data::PersonKind;
use hermit_model::entities::{ImageType, VideoType};
use hermit_model::entities_media::MetadataProvider;
use uuid::Uuid;

/// The default result language upstream `MetadataResult` initializes to.
///
/// Port of the `ResultLanguage = "en"` set in the C# constructor.
const DEFAULT_RESULT_LANGUAGE: &str = "en";

/// A person stub attached to a library item.
///
/// Port of `MediaBrowser.Controller.Entities.PersonInfo` (implements
/// `IHasProviderIds`). Provider-id keys are matched case-insensitively upstream;
/// this port normalizes keys to the canonical [`MetadataProvider`] spelling on
/// insert via [`PersonInfo::set_provider_id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonInfo {
    /// The person id (`Id`). Upstream defaults to a fresh `Guid` per instance.
    pub id: Uuid,
    /// The owning item id (`ItemId`).
    pub item_id: Uuid,
    /// The person's name.
    pub name: String,
    /// The role the person played (free text, e.g. an actor's character).
    pub role: Option<String>,
    /// The kind of person (`Type`).
    pub type_: PersonKind,
    /// The ascending sort order.
    pub sort_order: Option<i32>,
    /// A URL to an image of the person.
    pub image_url: Option<String>,
    /// External provider ids keyed by provider name.
    pub provider_ids: HashMap<String, String>,
}

impl Default for PersonInfo {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            item_id: Uuid::nil(),
            name: String::new(),
            role: None,
            type_: PersonKind::Unknown,
            sort_order: None,
            image_url: None,
            provider_ids: HashMap::new(),
        }
    }
}

impl PersonInfo {
    /// Creates a named person with a fresh id and no other metadata.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Sets a provider id, normalizing the key to the canonical
    /// [`MetadataProvider`] spelling when it matches one case-insensitively.
    ///
    /// Port of `ProviderIdsExtensions.SetProviderId(string, string)`. Empty or
    /// whitespace keys/values, and keys containing `'='`, are ignored (upstream
    /// throws; a pure data-type port drops them instead).
    pub fn set_provider_id(&mut self, name: &str, value: &str) {
        set_provider_id(&mut self.provider_ids, name, value);
    }

    /// Returns `true` if this person is of `type`, or its `role` names `type`
    /// (case-insensitive).
    ///
    /// Port of `PersonInfo.IsType`.
    #[must_use]
    pub fn is_type(&self, type_: PersonKind) -> bool {
        self.type_ == type_
            || self
                .role
                .as_deref()
                .is_some_and(|r| r.eq_ignore_ascii_case(&format!("{type_:?}")))
    }
}

/// A metadata lookup result for an item of type `T`.
///
/// Port of `MediaBrowser.Controller.Providers.MetadataResult<T>`. Upstream lazily
/// allocates `Images`/`RemoteImages`/`People`; this port keeps `people` as an
/// `Option` so callers can distinguish "not queried" (`None`) from "queried, no
/// people" (`Some(vec![])`), matching `ResetPeople`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataResult<T> {
    /// The looked-up item.
    pub item: T,
    /// The people (cast/crew) found. `None` until first populated.
    pub people: Option<Vec<PersonInfo>>,
    /// Remote image URLs paired with their [`ImageType`] (`RemoteImages`).
    pub remote_images: Vec<(String, ImageType)>,
    /// Local images discovered on disk (`Images`).
    pub images: Vec<LocalImageInfo>,
    /// The language of the result (`ResultLanguage`; defaults to `"en"`).
    pub result_language: String,
    /// The provider that produced the result (`Provider`).
    pub provider: Option<String>,
    /// Whether the item was queried by id (`QueriedById`).
    pub queried_by_id: bool,
    /// Whether any metadata was found (`HasMetadata`).
    pub has_metadata: bool,
}

impl<T: Default> Default for MetadataResult<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> MetadataResult<T> {
    /// Creates a result wrapping `item` with upstream constructor defaults
    /// (`ResultLanguage = "en"`, no people, no metadata).
    #[must_use]
    pub fn new(item: T) -> Self {
        Self {
            item,
            people: None,
            remote_images: Vec::new(),
            images: Vec::new(),
            result_language: DEFAULT_RESULT_LANGUAGE.to_owned(),
            provider: None,
            queried_by_id: false,
            has_metadata: false,
        }
    }

    /// Adds a person, applying the merge/normalize rules.
    ///
    /// Port of `MetadataResult.AddPerson`: ensures the `people` list exists, then
    /// delegates to [`add_person`].
    pub fn add_person(&mut self, person: PersonInfo) {
        add_person(self.people.get_or_insert_with(Vec::new), person);
    }

    /// Clears the people list while keeping it non-`None`, so callers can tell a
    /// null list from zero people.
    ///
    /// Port of `MetadataResult.ResetPeople`.
    pub fn reset_people(&mut self) {
        match &mut self.people {
            Some(people) => people.clear(),
            none => *none = Some(Vec::new()),
        }
    }
}

/// A lightweight, item-shape-agnostic view used to resolve save paths.
///
/// Port of `MediaBrowser.Controller.Providers.ItemInfo`. The C# constructor
/// copies these fields off a `BaseItem` (and, for `Video`, its `VideoType` /
/// `IsPlaceHolder`); the `ItemType` reflection field is dropped as it has no
/// Rust analogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemInfo {
    /// The item's path.
    pub path: Option<String>,
    /// The parent item id.
    pub parent_id: Uuid,
    /// The episode/track index number, when applicable.
    pub index_number: Option<i32>,
    /// The folder that contains the item.
    pub containing_folder_path: Option<String>,
    /// The video type (defaults to [`VideoType::VideoFile`] for non-videos).
    pub video_type: VideoType,
    /// Whether the item lives in a folder mixed with other content.
    pub is_in_mixed_folder: bool,
    /// Whether the item is a placeholder (e.g. an ISO stub).
    pub is_placeholder: bool,
}

impl Default for ItemInfo {
    fn default() -> Self {
        // `VideoType` has no `Default` in `hermit-model`; the C# enum default is
        // value 0 (`VideoFile`), which is what a non-video item resolves to.
        Self {
            path: None,
            parent_id: Uuid::nil(),
            index_number: None,
            containing_folder_path: None,
            video_type: VideoType::VideoFile,
            is_in_mixed_folder: false,
            is_placeholder: false,
        }
    }
}

/// Metadata about a single file-system entry.
///
/// Port of `MediaBrowser.Model.IO.FileSystemMetadata`. This namespace is
/// server-side filesystem plumbing dropped from `hermit-model`, so it is
/// re-created here for [`LocalImageInfo`]. `LastWriteTimeUtc` / `CreationTimeUtc`
/// are carried as RFC-3339 strings to avoid a chrono dependency in this
/// pure-data unit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileSystemMetadata {
    /// Whether the entry exists (`Exists`).
    pub exists: bool,
    /// The full path (`FullName`).
    pub full_name: String,
    /// The entry name (`Name`).
    pub name: String,
    /// The file extension including the leading dot, if any (`Extension`).
    pub extension: Option<String>,
    /// The length in bytes (`Length`).
    pub length: i64,
    /// The last write time, UTC, RFC-3339 (`LastWriteTimeUtc`).
    pub last_write_time_utc: Option<String>,
    /// The creation time, UTC, RFC-3339 (`CreationTimeUtc`).
    pub creation_time_utc: Option<String>,
    /// Whether the entry is a directory (`IsDirectory`).
    pub is_directory: bool,
}

/// A local image found on disk alongside a library item.
///
/// Port of `MediaBrowser.Controller.Providers.LocalImageInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocalImageInfo {
    /// The file the image lives in (`FileInfo`).
    pub file_info: FileSystemMetadata,
    /// The kind of image (`Type`).
    pub type_: ImageType,
}

/// The outcome of a single refresh pass.
///
/// Port of `MediaBrowser.Providers.Manager.RefreshResult`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RefreshResult {
    /// What parts of the item changed (`UpdateType`).
    pub update_type: hermit_traits::providers::ItemUpdateType,
    /// A human-readable error message, if the refresh failed (`ErrorMessage`).
    pub error_message: Option<String>,
    /// The number of provider failures encountered (`Failures`).
    pub failures: i32,
}

/// The union field-bag the NFO parsers populate.
///
/// The XbmcMetadata NFO parsers write into a heterogeneous set of item fields; a
/// faithful port keeps them in one bag here (a later unit drives the parse/write
/// over it). [`NfoItem::set_provider_id`] applies the parser's provider-id
/// normalization: the additional mappings `collectionnumber` / `tmdbcolid` /
/// `tmdbcol` → `TmdbCollection` and `imdb_id` → `Imdb`, layered on top of the
/// canonical [`MetadataProvider`] case-insensitive matching.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NfoItem {
    /// External provider ids keyed by (normalized) provider name.
    pub provider_ids: HashMap<String, String>,
}

impl NfoItem {
    /// Creates an empty NFO item.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a provider id, applying the NFO parser's normalization.
    ///
    /// First maps the parser's additional aliases to their canonical provider
    /// name (`collectionnumber`/`tmdbcolid`/`tmdbcol` → `TmdbCollection`,
    /// `imdb_id` → `Imdb`), then delegates to the canonical
    /// [`set_provider_id`] which case-insensitively matches [`MetadataProvider`]
    /// spellings. Empty/whitespace keys or values are ignored.
    pub fn set_provider_id(&mut self, name: &str, value: &str) {
        let normalized = normalize_nfo_provider_key(name);
        set_provider_id(&mut self.provider_ids, normalized, value);
    }
}

/// Maps an NFO parser provider-id alias to its canonical provider name.
///
/// Port of the "Additional Mappings" registered in
/// `BaseNfoParser` (`_validProviderIds`). Matching is case-insensitive.
fn normalize_nfo_provider_key(name: &str) -> &str {
    if name.eq_ignore_ascii_case("collectionnumber")
        || name.eq_ignore_ascii_case("tmdbcolid")
        || name.eq_ignore_ascii_case("tmdbcol")
    {
        "TmdbCollection"
    } else if name.eq_ignore_ascii_case("imdb_id") {
        "Imdb"
    } else {
        name
    }
}

/// Sets a provider id in `provider_ids`, normalizing the key to the canonical
/// [`MetadataProvider`] spelling when it matches one case-insensitively.
///
/// Port of `ProviderIdsExtensions.SetProviderId(string, string)`. Upstream
/// throws on null/whitespace keys or values and on keys containing `'='`; a
/// pure-data-type port drops such calls instead. Existing keys that differ only
/// in case are replaced so the map never holds case-variant duplicates (upstream
/// uses an `OrdinalIgnoreCase` dictionary).
pub fn set_provider_id<S: BuildHasher>(
    provider_ids: &mut HashMap<String, String, S>,
    name: &str,
    value: &str,
) {
    if name.trim().is_empty() || value.trim().is_empty() || name.contains('=') {
        return;
    }

    let key = canonical_provider_key(name);

    // Emulate the OrdinalIgnoreCase dictionary: drop any case-variant of the key
    // before inserting the canonical spelling.
    provider_ids.retain(|existing, _| !existing.eq_ignore_ascii_case(&key));
    provider_ids.insert(key, value.to_owned());
}

/// Returns the canonical [`MetadataProvider`] spelling for `name` if it matches
/// one case-insensitively, otherwise `name` unchanged.
///
/// Port of the `_metadataProviderEnumDictionary` lookup in `SetProviderId`.
fn canonical_provider_key(name: &str) -> String {
    const PROVIDERS: [MetadataProvider; 17] = [
        MetadataProvider::Custom,
        MetadataProvider::Imdb,
        MetadataProvider::Tmdb,
        MetadataProvider::Tvdb,
        MetadataProvider::Tvcom,
        MetadataProvider::TmdbCollection,
        MetadataProvider::MusicBrainzAlbum,
        MetadataProvider::MusicBrainzAlbumArtist,
        MetadataProvider::MusicBrainzArtist,
        MetadataProvider::MusicBrainzReleaseGroup,
        MetadataProvider::Zap2It,
        MetadataProvider::TvRage,
        MetadataProvider::AudioDbArtist,
        MetadataProvider::AudioDbAlbum,
        MetadataProvider::MusicBrainzTrack,
        MetadataProvider::TvMaze,
        MetadataProvider::MusicBrainzRecording,
    ];

    for provider in PROVIDERS {
        let spelling = format!("{provider:?}");
        if spelling.eq_ignore_ascii_case(name) {
            return spelling;
        }
    }
    name.to_owned()
}

/// Adds a person to `people`, applying the C# normalize/merge/dedupe rules.
///
/// Port of `MediaBrowser.Controller.Entities.PeopleHelper.AddPerson`. Callers
/// must pass a person with a non-empty name (upstream throws otherwise); an
/// empty-named person is dropped here.
pub fn add_person(people: &mut Vec<PersonInfo>, mut person: PersonInfo) {
    if person.name.trim().is_empty() {
        return;
    }
    let trimmed_len = person.name.trim_end().len();
    person.name.truncate(trimmed_len);
    let leading = person.name.len() - person.name.trim_start().len();
    person.name.drain(..leading);

    // Normalize the type from a role string that names a well-known role.
    if let Some(role) = person.role.as_deref() {
        if role.eq_ignore_ascii_case("GuestStar") {
            person.type_ = PersonKind::GuestStar;
        } else if role.eq_ignore_ascii_case("Director") {
            person.type_ = PersonKind::Director;
        } else if role.eq_ignore_ascii_case("Producer") {
            person.type_ = PersonKind::Producer;
        } else if role.eq_ignore_ascii_case("Writer") {
            person.type_ = PersonKind::Writer;
        }
    }

    // GuestStar promotes an existing Actor entry of the same name.
    if person.type_ == PersonKind::GuestStar
        && let Some(existing) = people
            .iter_mut()
            .find(|p| p.name.eq_ignore_ascii_case(&person.name) && p.type_ == PersonKind::Actor)
    {
        existing.type_ = PersonKind::GuestStar;
        merge_existing(existing, &person);
        return;
    }

    if person.type_ == PersonKind::Actor {
        // An Actor de-dupes against an existing Actor/GuestStar of the same name.
        match people.iter_mut().find(|p| {
            p.name.eq_ignore_ascii_case(&person.name)
                && (p.type_ == PersonKind::Actor || p.type_ == PersonKind::GuestStar)
        }) {
            None => people.push(person),
            Some(existing) => {
                // Fill in a missing role if we have one.
                if existing.role.as_deref().is_none_or(str::is_empty)
                    && let Some(role) = person.role.as_deref()
                    && !role.is_empty()
                {
                    existing.role = Some(role.to_owned());
                }
                merge_existing(existing, &person);
            }
        }
    } else {
        // Everything else de-dupes on (Name, Type).
        match people
            .iter_mut()
            .find(|p| p.name.eq_ignore_ascii_case(&person.name) && p.type_ == person.type_)
        {
            None => people.push(person),
            Some(existing) => merge_existing(existing, &person),
        }
    }
}

/// Merges the mergeable fields of `person` into `existing`.
///
/// Port of `PeopleHelper.MergeExisting`: sort order and image url are filled from
/// `person` when it has them, and its provider ids are set on `existing`.
fn merge_existing(existing: &mut PersonInfo, person: &PersonInfo) {
    if person.sort_order.is_some() {
        existing.sort_order = person.sort_order;
    }
    if person.image_url.is_some() {
        existing.image_url.clone_from(&person.image_url);
    }
    for (key, value) in &person.provider_ids {
        set_provider_id(&mut existing.provider_ids, key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_result_defaults_match_csharp_constructor() {
        let result = MetadataResult::new(0_u32);
        assert_eq!(result.result_language, "en");
        assert!(result.people.is_none());
        assert!(!result.has_metadata);
        assert!(!result.queried_by_id);
        assert!(result.remote_images.is_empty());
        assert!(result.images.is_empty());
    }

    #[test]
    fn reset_people_makes_list_non_null() {
        let mut result = MetadataResult::new(0_u32);
        assert!(result.people.is_none());
        result.reset_people();
        assert_eq!(result.people, Some(Vec::new()));

        result.add_person(PersonInfo::new("Alice"));
        result.reset_people();
        assert_eq!(result.people, Some(Vec::new()));
    }

    #[test]
    fn set_provider_id_normalizes_to_canonical_enum_spelling() {
        let mut ids = HashMap::new();
        set_provider_id(&mut ids, "imdb", "tt1");
        set_provider_id(&mut ids, "TMDB", "42");
        assert_eq!(ids.get("Imdb"), Some(&"tt1".to_owned()));
        assert_eq!(ids.get("Tmdb"), Some(&"42".to_owned()));
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn set_provider_id_drops_invalid_input() {
        let mut ids = HashMap::new();
        set_provider_id(&mut ids, "", "v");
        set_provider_id(&mut ids, "k", "  ");
        set_provider_id(&mut ids, "a=b", "v");
        assert!(ids.is_empty());
    }

    #[test]
    fn set_provider_id_replaces_case_variants() {
        let mut ids = HashMap::new();
        // An arbitrary (non-enum) provider name keeps its spelling but is still
        // de-duped case-insensitively.
        set_provider_id(&mut ids, "MyProvider", "1");
        set_provider_id(&mut ids, "myprovider", "2");
        assert_eq!(ids.len(), 1);
        assert_eq!(ids.get("myprovider"), Some(&"2".to_owned()));
    }

    #[test]
    fn nfo_set_provider_id_applies_additional_mappings() {
        let mut item = NfoItem::new();
        item.set_provider_id("collectionnumber", "10");
        item.set_provider_id("imdb_id", "tt99");
        item.set_provider_id("tmdbcolid", "20");
        assert_eq!(
            item.provider_ids.get("TmdbCollection"),
            Some(&"20".to_owned())
        );
        assert_eq!(item.provider_ids.get("Imdb"), Some(&"tt99".to_owned()));
        // collectionnumber and tmdbcolid both map to TmdbCollection (last wins).
        assert_eq!(item.provider_ids.len(), 2);
    }

    #[test]
    fn add_person_promotes_role_string_to_type() {
        let mut people = Vec::new();
        let mut director = PersonInfo::new("Bob");
        director.role = Some("Director".to_owned());
        add_person(&mut people, director);
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].type_, PersonKind::Director);
    }

    #[test]
    fn add_person_dedupes_actors_and_fills_role() {
        let mut people = Vec::new();
        add_person(&mut people, PersonInfo::new("Carol")); // Unknown type by default
        let mut actor = PersonInfo {
            type_: PersonKind::Actor,
            ..PersonInfo::new("Carol")
        };
        actor.role = Some("Herself".to_owned());
        add_person(&mut people, actor.clone());
        // First Carol was Unknown, so the Actor is a distinct (Name,Type) entry.
        assert_eq!(people.len(), 2);

        // A second Actor Carol with no role de-dupes onto the existing Actor.
        let bare = PersonInfo {
            type_: PersonKind::Actor,
            ..PersonInfo::new("Carol")
        };
        add_person(&mut people, bare);
        assert_eq!(people.len(), 2);
        let actor_entry = people
            .iter()
            .find(|p| p.type_ == PersonKind::Actor)
            .expect("actor present");
        assert_eq!(actor_entry.role.as_deref(), Some("Herself"));
    }

    #[test]
    fn add_person_guest_star_promotes_existing_actor() {
        let mut people = Vec::new();
        add_person(
            &mut people,
            PersonInfo {
                type_: PersonKind::Actor,
                ..PersonInfo::new("Dave")
            },
        );
        add_person(
            &mut people,
            PersonInfo {
                type_: PersonKind::GuestStar,
                ..PersonInfo::new("Dave")
            },
        );
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].type_, PersonKind::GuestStar);
    }

    #[test]
    fn add_person_drops_empty_name() {
        let mut people = Vec::new();
        add_person(&mut people, PersonInfo::new("   "));
        assert!(people.is_empty());
    }

    #[test]
    fn person_is_type_matches_role_string() {
        let mut person = PersonInfo::new("Eve");
        person.role = Some("director".to_owned());
        assert!(person.is_type(PersonKind::Director));
        assert!(!person.is_type(PersonKind::Writer));
    }
}
