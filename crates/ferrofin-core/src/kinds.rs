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

use ferrofin_model::data::{BaseItemKind, CollectionType};
use ferrofin_model::entities::CollectionTypeOptions;
use uuid::Uuid;

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

/// The `PresentationUniqueKey` a row of `kind` is stored with — C#
/// `BaseItem.CreatePresentationUniqueKey` and its overrides, which
/// `MetadataService` stamps on every refresh.
///
/// Every value below was read back out of a real Jellyfin 10.11.8 database:
///
/// | kind | stored key |
/// |---|---|
/// | `Movie`, `Episode`, `Series`, `MusicAlbum`, … | own id, `N` form (32 lowercase hex) |
/// | a merged alternate version | the *primary's* id, same form (`Video.cs:327`) |
/// | `Season` | `{series key}-{index:000}` (`Season.cs:131`) |
/// | `Genre`, `MusicGenre`, `Person`, `Studio` | `{Type}-{Name}`, diacritics removed (`Genre.cs:45` → `GetUserDataKeys()[0]`) |
/// | `MusicArtist` | `Artist-{Name}`, diacritics removed (`MusicArtist.cs:152`) |
/// | `Year`, and every other by-name kind | own id — C# gives them no override |
///
/// The key is what a query groups on, so it is how one title with four cuts
/// lists once. Ferrofin used to leave it null on most rows and write the bare
/// album *name* on albums — which grouped two same-named albums by different
/// artists into one.
#[must_use]
pub fn presentation_unique_key(
    kind: BaseItemKind,
    id: Uuid,
    name: Option<&str>,
    primary_version_id: Option<&str>,
    series_key: Option<&str>,
    index_number: Option<i64>,
) -> String {
    let own = id.as_simple().to_string();
    if is_video(kind)
        && let Some(primary) = primary_version_id.filter(|p| !p.is_empty())
    {
        // Already stored in the `N` form by `set_primary_version_id`; a value
        // that arrives hyphenated is normalized so the alternate lands in the
        // primary's group rather than a group of one.
        return Uuid::parse_str(primary)
            .map_or_else(|_| primary.to_owned(), |p| p.as_simple().to_string());
    }
    // The by-name prefix is the CLR *type* name, not the stored CLR path — and
    // `MusicArtist` is spelled `Artist` (`MusicArtist.cs:152`). Only these five
    // kinds override the key; `Year` and everything else keep their own id,
    // which is why the list is spelled out rather than derived from
    // `is_item_by_name`.
    let by_name_prefix = match kind {
        BaseItemKind::Genre => Some("Genre"),
        BaseItemKind::MusicGenre => Some("MusicGenre"),
        BaseItemKind::Person => Some("Person"),
        BaseItemKind::Studio => Some("Studio"),
        BaseItemKind::MusicArtist => Some("Artist"),
        _ => None,
    };
    match (kind, by_name_prefix) {
        (BaseItemKind::Season, _) => match (series_key.filter(|k| !k.is_empty()), index_number) {
            (Some(series), Some(index)) => format!("{series}-{index:03}"),
            _ => own,
        },
        (_, Some(prefix)) => match name.filter(|n| !n.is_empty()) {
            // Diacritics are removed and the case is kept, exactly as
            // `GetUserDataKeys()[0]` builds it — a real 10.11.8 stores
            // `Person-H. Jon Benjamin` and `Artist-Red Hot Chili Peppers`.
            Some(name) => format!(
                "{prefix}-{}",
                ferrofin_util::string_extensions::remove_diacritics(name)
            ),
            None => own,
        },
        _ => own,
    }
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

/// Whether `CreateSortName` builds the alphanumeric sort key for this kind
/// (C# `BaseItem.EnableAlphaNumericSorting`, `MediaBrowser.Controller/Entities/
/// BaseItem.cs`).
///
/// `Person` is the ONLY override in either tree — `Person.cs:32` on v10.11.8,
/// `Person.cs:33` on master, both `public override bool
/// EnableAlphaNumericSorting => false` (`git grep EnableAlphaNumericSorting`
/// returns BaseItem.cs and Person.cs and nothing else). It sends
/// `CreateSortName` down its first branch, `return Name.TrimStart();` — the
/// name verbatim, not the lower-cased, article-stripped, digit-padded key.
#[must_use]
pub fn enable_alpha_numeric_sorting(kind: BaseItemKind) -> bool {
    kind != BaseItemKind::Person
}

/// The `SortName` C# would compute for a named item of this kind — the port of
/// `BaseItem.CreateSortName()` including its
/// [`enable_alpha_numeric_sorting`] branch.
///
/// One home for the rule, because three separate write paths
/// (`upsert_item`'s fallback, `backfill_missing_sort_names`, and the people
/// repository's inserts) each have to answer the same question, and a `Person`
/// row that any one of them lower-cases stops matching where Jellyfin sorts and
/// filters it.
#[must_use]
pub fn sort_name_for(kind: BaseItemKind, name: &str) -> String {
    if enable_alpha_numeric_sorting(kind) {
        ferrofin_util::sort_name::create_sort_name(name)
    } else {
        name.trim_start().to_owned()
    }
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
/// (C# `BaseItem.SupportsPlayedStatus`).
///
/// `BaseItem` defaults to `false` and `Folder` overrides it to `true`
/// (`Folder.cs:84`), so ordinary media and the folders that aggregate it
/// support it — but seven container kinds override it back to `false`:
/// `CollectionFolder.cs:74`, `AggregateFolder.cs:50`, `UserRootFolder.cs:39`,
/// `UserView.cs:66`, `PhotoAlbum.cs:14`, `MusicAlbum.cs:51` and
/// `MusicArtist.cs:48`. The by-name grouping kinds are plain `BaseItem`s and
/// keep the `false` default.
///
/// This is the guard `Folder.FillUserDataDtoValues` (`Folder.cs:1973`) puts on
/// `UserData.UnplayedItemCount`, so a kind listed here as `false` must not
/// carry that count.
#[must_use]
pub fn supports_played_status(kind: BaseItemKind) -> bool {
    !matches!(
        kind,
        BaseItemKind::CollectionFolder
            | BaseItemKind::AggregateFolder
            | BaseItemKind::UserRootFolder
            | BaseItemKind::UserView
            | BaseItemKind::PhotoAlbum
            | BaseItemKind::MusicAlbum
    ) && !is_item_by_name(kind)
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

/// The container a "latest media" row groups under — C#
/// `BaseItem.LatestItemsIndexContainer`, which only three subclasses override:
/// `Episode.LatestItemsIndexContainer => Series`,
/// `Audio.LatestItemsIndexContainer => AlbumEntity` (the nearest `MusicAlbum`
/// ancestor) and `Photo.LatestItemsIndexContainer => AlbumEntity` (the nearest
/// `PhotoAlbum` ancestor). Every other kind — including `MusicVideo`, `Movie`
/// and `Book` — inherits the `null` default and is listed on its own.
///
/// This names the *kind* of the container; resolving the actual row is the
/// caller's job (an episode's `SeriesId`, a track's/photo's parent chain).
#[must_use]
pub fn latest_items_index_container_kind(kind: BaseItemKind) -> Option<BaseItemKind> {
    match kind {
        BaseItemKind::Episode => Some(BaseItemKind::Series),
        BaseItemKind::Audio => Some(BaseItemKind::MusicAlbum),
        BaseItemKind::Photo => Some(BaseItemKind::PhotoAlbum),
        _ => None,
    }
}

/// Maps a library's configured [`CollectionTypeOptions`] to the
/// [`CollectionType`] its `CollectionFolder` reports (C#
/// `CollectionFolder.CollectionType`, parsed from the `<type>.collection`
/// marker). `mixed` has no single type and maps to `None` — exactly what
/// Jellyfin does for a mixed-content library.
///
/// The API crate carries the same table (`handlers::user_views::
/// map_collection_type`) because handlers may not import `ferrofin-core`; the
/// one home that would serve both is `ferrofin-model`, which this crate does
/// not own. Keep the two in step (both are pinned by an exhaustive test).
#[must_use]
pub fn collection_type_of(options: CollectionTypeOptions) -> Option<CollectionType> {
    Some(match options {
        CollectionTypeOptions::movies => CollectionType::movies,
        CollectionTypeOptions::tvshows => CollectionType::tvshows,
        CollectionTypeOptions::music => CollectionType::music,
        CollectionTypeOptions::musicvideos => CollectionType::musicvideos,
        CollectionTypeOptions::homevideos => CollectionType::homevideos,
        CollectionTypeOptions::boxsets => CollectionType::boxsets,
        CollectionTypeOptions::books => CollectionType::books,
        CollectionTypeOptions::mixed => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        collection_type_of, is_displayed_as_folder, is_folder, is_item_by_name, is_video,
        latest_items_index_container_kind, presentation_unique_key, supports_ancestors,
        supports_inherited_parent_images, supports_people, supports_played_status,
        supports_similarity, supports_theme_media,
    };
    use ferrofin_model::data::{BaseItemKind, CollectionType};
    use ferrofin_model::entities::CollectionTypeOptions;
    use rstest::rstest;

    /// The series key a `Season` case carries.
    const SERIES_KEY: Option<&str> = Some("595f611d217e8273327033dd5d500d81");

    /// The presentation keys a real Jellyfin 10.11.8 database stores, read out
    /// of one row per kind and pinned here.
    ///
    /// The spellings are not guessable: `MusicArtist` is prefixed `Artist`, not
    /// `MusicArtist`; `Year` has no override at all despite being an item by
    /// name; and the by-name prefix keeps the display case while dropping
    /// diacritics. A wrong key is invisible until a query groups on it and
    /// silently merges two titles.
    #[rstest]
    // media rows: their own id, `N` form
    #[case(
        BaseItemKind::Movie,
        None,
        None,
        None,
        "0000000000000000000000000000002a"
    )]
    #[case(
        BaseItemKind::Episode,
        None,
        None,
        None,
        "0000000000000000000000000000002a"
    )]
    #[case(
        BaseItemKind::MusicAlbum,
        Some("Californication"),
        None,
        None,
        "0000000000000000000000000000002a"
    )]
    #[case(
        BaseItemKind::Series,
        Some("Breaking Bad"),
        None,
        None,
        "0000000000000000000000000000002a"
    )]
    // by-name rows: `{Type}-{Name}`, diacritics removed, case kept
    #[case(BaseItemKind::Genre, Some("Action"), None, None, "Genre-Action")]
    #[case(
        BaseItemKind::MusicGenre,
        Some("Death Metal"),
        None,
        None,
        "MusicGenre-Death Metal"
    )]
    #[case(
        BaseItemKind::Person,
        Some("H. Jon Benjamin"),
        None,
        None,
        "Person-H. Jon Benjamin"
    )]
    #[case(
        BaseItemKind::Studio,
        Some("1.21 Entertainment"),
        None,
        None,
        "Studio-1.21 Entertainment"
    )]
    #[case(
        BaseItemKind::MusicArtist,
        Some("Red Hot Chili Peppers"),
        None,
        None,
        "Artist-Red Hot Chili Peppers"
    )]
    #[case(BaseItemKind::MusicArtist, Some("Björk"), None, None, "Artist-Bjork")]
    // …but `Year` is NOT one of them
    #[case(
        BaseItemKind::Year,
        Some("1999"),
        None,
        None,
        "0000000000000000000000000000002a"
    )]
    // a season keys off its series and its index, zero-padded to three
    #[case(
        BaseItemKind::Season,
        Some("Season 2"),
        SERIES_KEY,
        Some(2),
        "595f611d217e8273327033dd5d500d81-002"
    )]
    // …and falls back to its own id when either half is missing
    #[case(
        BaseItemKind::Season,
        Some("Season 2"),
        None,
        Some(2),
        "0000000000000000000000000000002a"
    )]
    #[case(
        BaseItemKind::Season,
        Some("Specials"),
        SERIES_KEY,
        None,
        "0000000000000000000000000000002a"
    )]
    fn presentation_keys_match_the_ones_jellyfin_stores(
        #[case] kind: BaseItemKind,
        #[case] name: Option<&str>,
        #[case] series_key: Option<&str>,
        #[case] index_number: Option<i64>,
        #[case] expected: &str,
    ) {
        assert_eq!(
            presentation_unique_key(
                kind,
                uuid::Uuid::from_u128(0x2a),
                name,
                None,
                series_key,
                index_number
            ),
            expected
        );
    }

    /// A merged alternate keys off the PRIMARY, which is what puts the two rows
    /// in one group (`Video.cs:327`) — in `N` form whichever form the pointer
    /// column happens to hold.
    #[rstest]
    #[case("0000000000000000000000000000007b")]
    #[case("00000000-0000-0000-0000-00000000007B")]
    fn a_merged_alternate_keys_off_its_primary(#[case] primary: &str) {
        assert_eq!(
            presentation_unique_key(
                BaseItemKind::Movie,
                uuid::Uuid::from_u128(0x2a),
                Some("Blade Runner"),
                Some(primary),
                None,
                None,
            ),
            "0000000000000000000000000000007b"
        );
    }

    /// The `LatestItemsIndexContainer` override table: only `Episode`, `Audio`
    /// and `Photo` group; `MusicVideo` in particular does NOT (it inherits the
    /// `Video` default) — the old handler grouped it under its parent.
    #[rstest]
    #[case(BaseItemKind::Episode, Some(BaseItemKind::Series))]
    #[case(BaseItemKind::Audio, Some(BaseItemKind::MusicAlbum))]
    #[case(BaseItemKind::Photo, Some(BaseItemKind::PhotoAlbum))]
    #[case(BaseItemKind::MusicVideo, None)]
    #[case(BaseItemKind::Movie, None)]
    #[case(BaseItemKind::Book, None)]
    #[case(BaseItemKind::Video, None)]
    #[case(BaseItemKind::Series, None)]
    #[case(BaseItemKind::MusicAlbum, None)]
    fn latest_index_container_follows_the_csharp_overrides(
        #[case] kind: BaseItemKind,
        #[case] expected: Option<BaseItemKind>,
    ) {
        assert_eq!(latest_items_index_container_kind(kind), expected);
    }

    #[rstest]
    #[case(CollectionTypeOptions::movies, Some(CollectionType::movies))]
    #[case(CollectionTypeOptions::tvshows, Some(CollectionType::tvshows))]
    #[case(CollectionTypeOptions::music, Some(CollectionType::music))]
    #[case(CollectionTypeOptions::musicvideos, Some(CollectionType::musicvideos))]
    #[case(CollectionTypeOptions::homevideos, Some(CollectionType::homevideos))]
    #[case(CollectionTypeOptions::boxsets, Some(CollectionType::boxsets))]
    #[case(CollectionTypeOptions::books, Some(CollectionType::books))]
    #[case(CollectionTypeOptions::mixed, None)]
    fn collection_type_maps_library_options_to_the_folder_type(
        #[case] options: CollectionTypeOptions,
        #[case] expected: Option<CollectionType>,
    ) {
        assert_eq!(collection_type_of(options), expected);
    }

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

    /// The seven container kinds that override `SupportsPlayedStatus` back to
    /// `false` (`CollectionFolder.cs:74`, `AggregateFolder.cs:50`,
    /// `UserRootFolder.cs:39`, `UserView.cs:66`, `PhotoAlbum.cs:14`,
    /// `MusicAlbum.cs:51`, `MusicArtist.cs:48`) — the ones whose DTOs must not
    /// carry `UserData.UnplayedItemCount`.
    #[test]
    fn container_kinds_override_played_status_off() {
        for kind in [
            BaseItemKind::CollectionFolder,
            BaseItemKind::AggregateFolder,
            BaseItemKind::UserRootFolder,
            BaseItemKind::UserView,
            BaseItemKind::PhotoAlbum,
            BaseItemKind::MusicAlbum,
            BaseItemKind::MusicArtist,
        ] {
            assert!(!supports_played_status(kind), "{kind:?}");
        }
        // `Folder.SupportsPlayedStatus => true` still holds for the aggregating
        // kinds that do not override it.
        for kind in [
            BaseItemKind::Series,
            BaseItemKind::Season,
            BaseItemKind::BoxSet,
            BaseItemKind::Folder,
            BaseItemKind::ManualPlaylistsFolder,
        ] {
            assert!(supports_played_status(kind), "{kind:?}");
        }
    }
}
