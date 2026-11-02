//! The item filters that read media streams, chapters or resolution through
//! an item's version group and, for folders, its descendants: 12.1's
//! `AudioLanguages` / `SubtitleLanguages`, `HasSubtitles`, the four
//! `HasNo*TrackWithLanguage` filters and `HasChapterImages` (v12
//! `BaseItemRepository.TranslateQuery.cs:826-968`, with
//! `DescendantQueryHelper.GetPrimaryVersionIdsMatching` /
//! `GetFolderIdsMatching`), and `IsHD` / `Is4K` / `Min`/`Max`
//! `Width`/`Height` (`TranslateQuery.cs:95-196`, `VersionsMatchingDimension`).
//!
//! The fixture is upstream's `BaseItemRepositoryStreamFilterTests.Seed`
//! (`BaseItemRepositoryStreamFilterTests.cs:289-552`), plus a few rows of
//! Ferrofin's own for the cases upstream does not pin (empty-language
//! streams, a video stream carrying a language, an external subtitle, cyclic
//! and alternate-version links). Every upstream `[Fact]` over these filters
//! is transliterated with its C# expectations as the oracle. Upstream's
//! `GetFolderIdsMatching_*` facts
//! (`DescendantQueryHelperTests.cs:222-402`) run each on their own store.

use std::collections::BTreeSet;
use std::sync::Arc;

use ferrofin_core::item_type_lookup::{ItemTypeLookup, stored_type_name};
use ferrofin_core::translate_query::{QueryShape, build_query};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinMediaStreamRepository,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::{BaseItemEntity, MediaStreamInfoEntity};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::enums::{LinkedChildType, MediaStreamTypeEntity};
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::SortOrder;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{
    ItemPersistenceService as _, ItemRepository as _, MediaStreamRepository as _,
};
use rstest::rstest;
use sqlx::{Execute as _, Row as _};
use uuid::Uuid;

// Upstream's fixture ids (`_library`, `_withSubtitles`, …).
const LIBRARY: Uuid = Uuid::from_u128(0x1001);
const WITH_SUBTITLES: Uuid = Uuid::from_u128(0x1002);
const WITHOUT_SUBTITLES: Uuid = Uuid::from_u128(0x1003);
const COLLECTION: Uuid = Uuid::from_u128(0x1004);
const LINKED_SERIES: Uuid = Uuid::from_u128(0x1005);
const LINKED_EPISODE: Uuid = Uuid::from_u128(0x1006);
const VERSION_LIBRARY: Uuid = Uuid::from_u128(0x1007);
const VERSIONED_MOVIE: Uuid = Uuid::from_u128(0x1008);
const ALTERNATE_VERSION: Uuid = Uuid::from_u128(0x1009);
const UNPROBED_MOVIE: Uuid = Uuid::from_u128(0x100A);
const UNPROBED_ALTERNATE: Uuid = Uuid::from_u128(0x100B);
const VERSIONED_SERIES: Uuid = Uuid::from_u128(0x100C);
const VERSIONED_EPISODE: Uuid = Uuid::from_u128(0x100D);
const EPISODE_ALTERNATE: Uuid = Uuid::from_u128(0x100E);
const SD_MOVIE: Uuid = Uuid::from_u128(0x100F);
const HD_ONLY_BY_VERSION: Uuid = Uuid::from_u128(0x1010);
const HD_ONLY_ALTERNATE: Uuid = Uuid::from_u128(0x1011);
const THREE_WAY_MOVIE: Uuid = Uuid::from_u128(0x1012);
const THREE_WAY_HD: Uuid = Uuid::from_u128(0x1013);
const THREE_WAY_4K: Uuid = Uuid::from_u128(0x1014);

// Upstream's fixture dimensions (`Width` / `Height`).
const SD: Option<(i64, i64)> = Some((720, 480));
const HD: Option<(i64, i64)> = Some((1920, 1080));
const UHD: Option<(i64, i64)> = Some((3840, 2160));

// Ferrofin's additions.
/// A movie whose only audio stream has an empty (not NULL) language.
const EMPTY_LANGUAGE_MOVIE: Uuid = Uuid::from_u128(0x2001);
/// A movie whose only `ger` stream is its video stream.
const GERMAN_VIDEO_MOVIE: Uuid = Uuid::from_u128(0x2002);
/// Two box sets linking each other; the first also links `LINKED_SERIES`.
const CYCLE_A: Uuid = Uuid::from_u128(0x2003);
const CYCLE_B: Uuid = Uuid::from_u128(0x2004);
/// A folder whose only link to a matching movie is an alternate-version edge.
const LOCAL_VERSION_OWNER: Uuid = Uuid::from_u128(0x2005);
const LINKED_VERSION_OWNER: Uuid = Uuid::from_u128(0x2006);
/// A movie whose only `ger` subtitle is an external file, and its folder.
const EXTERNAL_SUBTITLE_MOVIE: Uuid = Uuid::from_u128(0x2007);
const EXTERNAL_SUBTITLE_FOLDER: Uuid = Uuid::from_u128(0x2008);

async fn fresh_db() -> Database {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    db
}

fn row(
    id: Uuid,
    kind: BaseItemKind,
    primary: Option<Uuid>,
    dimensions: Option<(i64, i64)>,
) -> BaseItemEntity {
    let is_folder = matches!(
        kind,
        BaseItemKind::Folder | BaseItemKind::BoxSet | BaseItemKind::CollectionFolder
    );
    BaseItemEntity {
        id: guid_to_db(id),
        type_: stored_type_name(kind).expect("stored type").to_owned(),
        name: Some(format!("{id}")),
        is_folder,
        primary_version_id: primary.map(guid_to_db),
        width: dimensions.map(|(width, _)| width),
        height: dimensions.map(|(_, height)| height),
        ..BaseItemEntity::default()
    }
}

async fn ancestor(db: &Database, item: Uuid, parent: Uuid) {
    sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
        .bind(guid_to_db(item))
        .bind(guid_to_db(parent))
        .execute(db.writer())
        .await
        .expect("ancestor");
}

async fn link(db: &Database, parent: Uuid, child: Uuid, kind: LinkedChildType) {
    sqlx::query(
        r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
           VALUES (?1, (SELECT COUNT(*) FROM "LinkedChildren" WHERE "ParentId" = ?1), ?2, ?3)"#,
    )
    .bind(guid_to_db(parent))
    .bind(guid_to_db(child))
    .bind(i32::from(kind))
    .execute(db.writer())
    .await
    .expect("link");
}

/// Stores `item`'s streams, in order, as the probe writes them.
async fn streams(db: &Database, item: Uuid, rows: &[(MediaStreamTypeEntity, Option<&str>)]) {
    let rows: Vec<MediaStreamInfoEntity> = (0_i64..)
        .zip(rows)
        .map(|(index, &(kind, language))| MediaStreamInfoEntity {
            item_id: guid_to_db(item),
            stream_index: index,
            stream_type: i32::from(kind),
            language: language.map(str::to_owned),
            ..MediaStreamInfoEntity::default()
        })
        .collect();
    FerrofinMediaStreamRepository::new(db.clone())
        .save_media_streams(item, &rows)
        .await
        .expect("streams");
}

/// Upstream's two chapter pictures (on `_withSubtitles` and on the alternate
/// version), and Ferrofin's movie whose only `ger` subtitle is external.
async fn seed_chapters_and_external_subtitle(db: &Database) {
    chapter_image(db, WITH_SUBTITLES, "/chapter.jpg").await;
    chapter_image(db, ALTERNATE_VERSION, "/alternate-chapter.jpg").await;
    FerrofinItemPersistenceService::new(db.clone())
        .save_items(&[
            row(EXTERNAL_SUBTITLE_MOVIE, BaseItemKind::Movie, None, None),
            row(EXTERNAL_SUBTITLE_FOLDER, BaseItemKind::Folder, None, None),
        ])
        .await
        .expect("save");
    ancestor(db, EXTERNAL_SUBTITLE_MOVIE, EXTERNAL_SUBTITLE_FOLDER).await;
    FerrofinMediaStreamRepository::new(db.clone())
        .save_media_streams(
            EXTERNAL_SUBTITLE_MOVIE,
            &[MediaStreamInfoEntity {
                item_id: guid_to_db(EXTERNAL_SUBTITLE_MOVIE),
                stream_index: 0,
                stream_type: i32::from(MediaStreamTypeEntity::Subtitle),
                language: Some("ger".to_owned()),
                is_external: true,
                ..MediaStreamInfoEntity::default()
            }],
        )
        .await
        .expect("external subtitle");
}

/// Stores one chapter of `item` with a picture, as chapter extraction does.
async fn chapter_image(db: &Database, item: Uuid, path: &str) {
    sqlx::query(
        r#"INSERT INTO "Chapters" ("ItemId", "ChapterIndex", "StartPositionTicks", "ImagePath")
           VALUES (?1, 0, 0, ?2)"#,
    )
    .bind(guid_to_db(item))
    .bind(path)
    .execute(db.writer())
    .await
    .expect("chapter");
}

/// `BaseItemRepositoryStreamFilterTests.Seed` / `SeedVersionGroup` /
/// `SeedVersionedSeries` / `SeedUnprobedVersionGroup` /
/// `SeedMixedVersionGroups`, plus Ferrofin's extra rows.
async fn seeded() -> Database {
    use BaseItemKind::{BoxSet, Folder, Movie};
    use MediaStreamTypeEntity::{Audio, Subtitle, Video};

    let db = fresh_db().await;
    let rows: Vec<BaseItemEntity> = [
        (LIBRARY, Folder, None, None),
        (WITH_SUBTITLES, Movie, None, None),
        (WITHOUT_SUBTITLES, Movie, None, None),
        (COLLECTION, BoxSet, None, None),
        (LINKED_SERIES, Folder, None, None),
        (LINKED_EPISODE, Movie, None, None),
        (VERSION_LIBRARY, Folder, None, None),
        (VERSIONED_MOVIE, Movie, None, SD),
        (ALTERNATE_VERSION, Movie, Some(VERSIONED_MOVIE), UHD),
        (VERSIONED_SERIES, Folder, None, None),
        (VERSIONED_EPISODE, Movie, None, SD),
        (EPISODE_ALTERNATE, Movie, Some(VERSIONED_EPISODE), UHD),
        (SD_MOVIE, Movie, None, SD),
        (UNPROBED_MOVIE, Movie, None, None),
        (UNPROBED_ALTERNATE, Movie, Some(UNPROBED_MOVIE), SD),
        (THREE_WAY_MOVIE, Movie, None, SD),
        (THREE_WAY_HD, Movie, Some(THREE_WAY_MOVIE), HD),
        (THREE_WAY_4K, Movie, Some(THREE_WAY_MOVIE), UHD),
        (HD_ONLY_BY_VERSION, Movie, None, None),
        (HD_ONLY_ALTERNATE, Movie, Some(HD_ONLY_BY_VERSION), HD),
        (EMPTY_LANGUAGE_MOVIE, Movie, None, None),
        (GERMAN_VIDEO_MOVIE, Movie, None, None),
        (CYCLE_A, BoxSet, None, None),
        (CYCLE_B, BoxSet, None, None),
        (LOCAL_VERSION_OWNER, Folder, None, None),
        (LINKED_VERSION_OWNER, Folder, None, None),
    ]
    .into_iter()
    .map(|(id, kind, primary, dimensions)| row(id, kind, primary, dimensions))
    .collect();
    FerrofinItemPersistenceService::new(db.clone())
        .save_items(&rows)
        .await
        .expect("save");

    for item in [WITH_SUBTITLES, WITHOUT_SUBTITLES] {
        ancestor(&db, item, LIBRARY).await;
    }
    streams(
        &db,
        WITH_SUBTITLES,
        &[(Video, None), (Subtitle, Some("ger"))],
    )
    .await;
    streams(&db, WITHOUT_SUBTITLES, &[(Video, None)]).await;

    // A collection linking a folder: the match is one link then one closure hop.
    ancestor(&db, LINKED_EPISODE, LINKED_SERIES).await;
    link(&db, COLLECTION, LINKED_SERIES, LinkedChildType::Manual).await;
    streams(&db, LINKED_EPISODE, &[(Subtitle, Some("ger"))]).await;

    // An SD primary whose tracks all live on its second file.
    for item in [VERSIONED_MOVIE, ALTERNATE_VERSION] {
        ancestor(&db, item, VERSION_LIBRARY).await;
    }
    streams(
        &db,
        ALTERNATE_VERSION,
        &[(Subtitle, Some("ger")), (Audio, Some("fre"))],
    )
    .await;

    // The same SD primary / 4K second file pair one level down.
    ancestor(&db, VERSIONED_SERIES, VERSION_LIBRARY).await;
    for item in [VERSIONED_EPISODE, EPISODE_ALTERNATE] {
        ancestor(&db, item, VERSIONED_SERIES).await;
    }

    // An unprobed primary whose second file's audio declares no language,
    // the plain SD control, and the mixed version groups.
    for item in [
        SD_MOVIE,
        UNPROBED_MOVIE,
        UNPROBED_ALTERNATE,
        THREE_WAY_MOVIE,
        THREE_WAY_HD,
        THREE_WAY_4K,
        HD_ONLY_BY_VERSION,
        HD_ONLY_ALTERNATE,
    ] {
        ancestor(&db, item, VERSION_LIBRARY).await;
    }
    streams(&db, UNPROBED_ALTERNATE, &[(Audio, None)]).await;

    // Ferrofin's rows.
    streams(&db, EMPTY_LANGUAGE_MOVIE, &[(Audio, Some(""))]).await;
    streams(&db, GERMAN_VIDEO_MOVIE, &[(Video, Some("ger"))]).await;
    seed_chapters_and_external_subtitle(&db).await;
    link(&db, CYCLE_A, CYCLE_B, LinkedChildType::Manual).await;
    link(&db, CYCLE_B, CYCLE_A, LinkedChildType::Manual).await;
    link(&db, CYCLE_A, LINKED_SERIES, LinkedChildType::Manual).await;
    link(
        &db,
        LOCAL_VERSION_OWNER,
        WITH_SUBTITLES,
        LinkedChildType::LocalAlternateVersion,
    )
    .await;
    link(
        &db,
        LINKED_VERSION_OWNER,
        WITH_SUBTITLES,
        LinkedChildType::LinkedAlternateVersion,
    )
    .await;
    db
}

/// `GetItemIdsList(query)`.
async fn query_ids(db: &Database, query: &InternalItemsQuery) -> Vec<Uuid> {
    FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()))
        .get_item_ids(query)
        .await
        .expect("query")
}

/// `GetItemIdsList(new InternalItemsQuery { … })` over the language lists —
/// no user, owned items hidden.
async fn ids(db: &Database, audio: &[&str], subtitles: &[&str]) -> Vec<Uuid> {
    query_ids(
        db,
        &InternalItemsQuery {
            audio_languages: audio.iter().map(|&l| l.to_owned()).collect(),
            subtitle_languages: subtitles.iter().map(|&l| l.to_owned()).collect(),
            ..InternalItemsQuery::default()
        },
    )
    .await
}

/// `SubtitleLanguages_MatchesTheRequestedLanguageOnly`
/// (`BaseItemRepositoryStreamFilterTests.cs:92-97`).
#[tokio::test]
async fn subtitle_languages_matches_the_requested_language_only() {
    let db = seeded().await;
    assert!(ids(&db, &[], &["ger"]).await.contains(&WITH_SUBTITLES));
    assert!(!ids(&db, &[], &["fre"]).await.contains(&WITH_SUBTITLES));
}

/// `SubtitleLanguages_MatchesTheLanguageOnAnAlternateVersion` (`:157-162`).
#[tokio::test]
async fn subtitle_languages_matches_the_language_on_an_alternate_version() {
    let db = seeded().await;
    assert!(ids(&db, &[], &["ger"]).await.contains(&VERSIONED_MOVIE));
    assert!(!ids(&db, &[], &["fre"]).await.contains(&VERSIONED_MOVIE));
}

/// `AudioLanguages_MatchesTheLanguageOnAnAlternateVersion` (`:173-177`).
#[tokio::test]
async fn audio_languages_matches_the_language_on_an_alternate_version() {
    let db = seeded().await;
    assert!(ids(&db, &["fre"], &[]).await.contains(&VERSIONED_MOVIE));
}

/// The folder branch (`GetFolderIdsMatching`), read off upstream's
/// `HasSubtitles` folder facts (`:72-80`, `:110-116`, `:138-146`), which go
/// through the same helper: the library above a match, a collection linking a
/// folder above one, and the library above an alternate version's match all
/// count; the alternate itself stays hidden behind its primary.
#[tokio::test]
async fn languages_match_the_folders_above_a_matching_item() {
    let db = seeded().await;
    let german = ids(&db, &[], &["ger"]).await;
    for folder in [LIBRARY, LINKED_SERIES, COLLECTION, VERSION_LIBRARY] {
        assert!(german.contains(&folder), "{folder}: {german:?}");
    }
    assert!(german.contains(&LINKED_EPISODE));
    assert!(!german.contains(&WITHOUT_SUBTITLES));
    assert!(!german.contains(&ALTERNATE_VERSION));

    let french = ids(&db, &["fre"], &[]).await;
    assert!(french.contains(&VERSION_LIBRARY), "{french:?}");
    assert!(!french.contains(&LIBRARY), "{french:?}");
    assert!(!french.contains(&COLLECTION), "{french:?}");
}

/// Only lower-case `und` adds the no-language arm (an ordinal
/// `Contains("und")`); the unprobed primary matches through its alternate's
/// NULL-language audio.
#[rstest]
#[case(&["und"], true)]
#[case(&["UND"], false)]
#[case(&["eng", "und"], true)]
#[case(&["eng"], false)]
#[tokio::test]
async fn undetermined_matches_a_stream_without_a_language(
    #[case] languages: &[&str],
    #[case] expected: bool,
) {
    let db = seeded().await;
    let found = ids(&db, languages, &[]).await;
    assert_eq!(found.contains(&UNPROBED_MOVIE), expected, "{languages:?}");
    // `string.IsNullOrEmpty`: an empty language is undetermined too.
    assert_eq!(
        found.contains(&EMPTY_LANGUAGE_MOVIE),
        expected,
        "{languages:?}"
    );
}

/// The list filters skip `NormalizeLanguage` and compare under `BINARY`
/// collation, so the code is case-sensitive.
#[tokio::test]
async fn languages_match_case_sensitively() {
    let db = seeded().await;
    assert!(!ids(&db, &["FRE"], &[]).await.contains(&VERSIONED_MOVIE));
    assert!(!ids(&db, &[], &["GER"]).await.contains(&WITH_SUBTITLES));
}

/// The languages in one list are OR-ed, the two lists AND-ed, and only the
/// list's own stream type counts.
#[tokio::test]
async fn lists_combine_like_upstream() {
    let db = seeded().await;
    assert!(
        ids(&db, &["fre"], &["ger"])
            .await
            .contains(&VERSIONED_MOVIE)
    );
    assert!(
        !ids(&db, &["fre"], &["spa"])
            .await
            .contains(&VERSIONED_MOVIE)
    );
    assert!(
        ids(&db, &[], &["fre", "ger"])
            .await
            .contains(&WITH_SUBTITLES)
    );
    let german = ids(&db, &[], &["ger"]).await;
    assert!(!german.contains(&GERMAN_VIDEO_MOVIE), "{german:?}");
    let german_audio = ids(&db, &["ger"], &[]).await;
    assert!(
        !german_audio.contains(&GERMAN_VIDEO_MOVIE),
        "{german_audio:?}"
    );
    assert!(!german_audio.contains(&WITH_SUBTITLES), "{german_audio:?}");
}

/// `ResolveLinkParents` walks container links until nothing new turns up, so
/// two box sets linking each other terminate and both match; an
/// alternate-version link is a second file, never a container edge.
#[tokio::test]
async fn link_walk_survives_cycles_and_skips_version_links() {
    let db = seeded().await;
    let german = ids(&db, &[], &["ger"]).await;
    assert!(german.contains(&CYCLE_A), "{german:?}");
    assert!(german.contains(&CYCLE_B), "{german:?}");
    assert!(!german.contains(&LOCAL_VERSION_OWNER), "{german:?}");
    assert!(!german.contains(&LINKED_VERSION_OWNER), "{german:?}");
}

/// `GetItemIdsList(new InternalItemsQuery { … })` with one
/// `HasNo*TrackWithLanguage` filter set by `set`.
async fn ids_without(db: &Database, set: impl FnOnce(&mut InternalItemsQuery)) -> Vec<Uuid> {
    let mut query = InternalItemsQuery::default();
    set(&mut query);
    query_ids(db, &query).await
}

fn has_chapter_images(want: bool) -> InternalItemsQuery {
    InternalItemsQuery {
        has_chapter_images: Some(want),
        ..InternalItemsQuery::default()
    }
}

/// `HasNoSubtitleTrackWithLanguage_ExcludesTheMatchingItemAndFolder`
/// (`BaseItemRepositoryStreamFilterTests.cs:99-107`).
#[tokio::test]
async fn has_no_subtitle_track_with_language_excludes_the_matching_item_and_folder() {
    let db = seeded().await;
    let found = ids_without(&db, |q| {
        q.has_no_subtitle_track_with_language = Some("ger".to_owned());
    })
    .await;
    assert!(found.contains(&WITHOUT_SUBTITLES));
    assert!(!found.contains(&WITH_SUBTITLES));
    assert!(!found.contains(&LIBRARY));
}

/// `HasChapterImages_MatchesTheItemAndItsParentFolder`
/// (`BaseItemRepositoryStreamFilterTests.cs:127-135`).
#[tokio::test]
async fn has_chapter_images_matches_the_item_and_its_parent_folder() {
    let db = seeded().await;
    let found = query_ids(&db, &has_chapter_images(true)).await;
    assert!(found.contains(&WITH_SUBTITLES));
    assert!(found.contains(&LIBRARY));
    assert!(!found.contains(&WITHOUT_SUBTITLES));
}

/// `HasNoSubtitleTrackWithLanguage_ExcludesAnItemWhoseAlternateVersionHasIt`
/// (`BaseItemRepositoryStreamFilterTests.cs:164-171`).
#[tokio::test]
async fn has_no_subtitle_track_with_language_excludes_an_item_whose_alternate_version_has_it() {
    let db = seeded().await;
    let found = ids_without(&db, |q| {
        q.has_no_subtitle_track_with_language = Some("ger".to_owned());
    })
    .await;
    assert!(!found.contains(&VERSIONED_MOVIE));
    assert!(!found.contains(&VERSION_LIBRARY));
}

/// `HasNoAudioTrackWithLanguage_ExcludesAnItemWhoseAlternateVersionHasIt`
/// (`BaseItemRepositoryStreamFilterTests.cs:179-183`).
#[tokio::test]
async fn has_no_audio_track_with_language_excludes_an_item_whose_alternate_version_has_it() {
    let db = seeded().await;
    let found = ids_without(&db, |q| {
        q.has_no_audio_track_with_language = Some("fre".to_owned());
    })
    .await;
    assert!(!found.contains(&VERSIONED_MOVIE));
}

/// `HasChapterImages_MatchesAnItemWhoseAlternateVersionCarriesThem`
/// (`BaseItemRepositoryStreamFilterTests.cs:185-189`).
#[tokio::test]
async fn has_chapter_images_matches_an_item_whose_alternate_version_carries_them() {
    let db = seeded().await;
    assert!(
        query_ids(&db, &has_chapter_images(true))
            .await
            .contains(&VERSIONED_MOVIE)
    );
}

/// `HasNoAudioTrackWithLanguage_TreatsUndeterminedCaseInsensitively`
/// (`BaseItemRepositoryStreamFilterTests.cs:276-287`): `NormalizeLanguage`
/// folds any casing of `und` onto the code that matches a stream with no
/// language.
#[rstest]
#[case("und")]
#[case("UND")]
#[tokio::test]
async fn has_no_audio_track_with_language_treats_undetermined_case_insensitively(
    #[case] language: &str,
) {
    let db = seeded().await;
    let found = ids_without(&db, |q| {
        q.has_no_audio_track_with_language = Some(language.to_owned());
    })
    .await;
    assert!(!found.contains(&UNPROBED_MOVIE));
    assert!(found.contains(&VERSIONED_MOVIE));
}

/// `HasNoInternal…`/`HasNoExternalSubtitleTrackWithLanguage` read only the
/// subtitles on their side of `IsExternal`; `HasNoSubtitleTrackWithLanguage`
/// reads both (`TranslateQuery.cs:845-895`).
#[tokio::test]
async fn subtitle_absence_filters_split_on_is_external() {
    let db = seeded().await;
    let internal = ids_without(&db, |q| {
        q.has_no_internal_subtitle_track_with_language = Some("ger".to_owned());
    })
    .await;
    assert!(internal.contains(&EXTERNAL_SUBTITLE_MOVIE));
    assert!(internal.contains(&EXTERNAL_SUBTITLE_FOLDER), "folder arm");
    assert!(!internal.contains(&WITH_SUBTITLES));
    let external = ids_without(&db, |q| {
        q.has_no_external_subtitle_track_with_language = Some("ger".to_owned());
    })
    .await;
    assert!(!external.contains(&EXTERNAL_SUBTITLE_MOVIE));
    assert!(!external.contains(&EXTERNAL_SUBTITLE_FOLDER), "folder arm");
    assert!(external.contains(&WITH_SUBTITLES));
    let either = ids_without(&db, |q| {
        q.has_no_subtitle_track_with_language = Some("ger".to_owned());
    })
    .await;
    assert!(!either.contains(&EXTERNAL_SUBTITLE_MOVIE));
    assert!(!either.contains(&WITH_SUBTITLES));
    // A blank language is no filter (`IsNullOrWhiteSpace`).
    let blank = ids_without(&db, |q| {
        q.has_no_subtitle_track_with_language = Some(" ".to_owned());
    })
    .await;
    assert!(blank.contains(&WITH_SUBTITLES));
}

/// `HasChapterImages = false` is the exact complement of `true`.
#[tokio::test]
async fn has_chapter_images_false_is_the_complement() {
    let db = seeded().await;
    let with: BTreeSet<Uuid> = query_ids(&db, &has_chapter_images(true))
        .await
        .into_iter()
        .collect();
    let without: BTreeSet<Uuid> = query_ids(&db, &has_chapter_images(false))
        .await
        .into_iter()
        .collect();
    let all: BTreeSet<Uuid> = query_ids(&db, &InternalItemsQuery::default())
        .await
        .into_iter()
        .collect();
    assert!(with.is_disjoint(&without));
    assert_eq!(&with | &without, all);
    assert!(without.contains(&WITHOUT_SUBTITLES));
    assert!(!without.contains(&VERSIONED_MOVIE));
}

/// Saves `items` as bare rows into a fresh database — upstream
/// `DescendantQueryHelperTests.AddFolder` / `AddItem`, each fact on its own
/// store.
async fn bare_items(items: &[(Uuid, BaseItemKind)]) -> Database {
    let db = fresh_db().await;
    let rows: Vec<BaseItemEntity> = items
        .iter()
        .map(|&(id, kind)| row(id, kind, None, None))
        .collect();
    FerrofinItemPersistenceService::new(db.clone())
        .save_items(&rows)
        .await
        .expect("save");
    db
}

/// `AddAncestors(item, ancestors…)`: one closure row per ancestor.
async fn ancestors(db: &Database, item: Uuid, parents: &[Uuid]) {
    for &parent in parents {
        ancestor(db, item, parent).await;
    }
}

/// The folders a query's folder arm keeps — `GetFolderIdsMatching(criteria)`
/// read back through `IsFolder = true`, since the folder arm is the only one
/// a folder row can match.
async fn matching_folders(db: &Database, query: InternalItemsQuery) -> BTreeSet<Uuid> {
    query_ids(
        db,
        &InternalItemsQuery {
            is_folder: Some(true),
            ..query
        },
    )
    .await
    .into_iter()
    .collect()
}

/// `new HasSubtitles()`.
fn has_subtitles() -> InternalItemsQuery {
    InternalItemsQuery {
        has_subtitles: Some(true),
        ..InternalItemsQuery::default()
    }
}

/// `new HasMediaStreamType(Subtitle, [language])`.
fn subtitles_in(language: &str) -> InternalItemsQuery {
    InternalItemsQuery {
        subtitle_languages: vec![language.to_owned()],
        ..InternalItemsQuery::default()
    }
}

/// `GetFolderIdsMatching_LinkAboveAClosure_ReturnsTheLinkingFolder`
/// (`DescendantQueryHelperTests.cs:222-267`): the box set links the series,
/// not the episode carrying the subtitles, and the collections folder above
/// the box set is reached through the link parent's own ancestors.
#[tokio::test]
async fn folder_ids_matching_link_above_a_closure_returns_the_linking_folder() {
    use BaseItemKind::{BoxSet, Folder, Movie};
    use MediaStreamTypeEntity::{Subtitle, Video};
    let collections = Uuid::from_u128(0x4001);
    let box_set = Uuid::from_u128(0x4002);
    let library = Uuid::from_u128(0x4003);
    let series = Uuid::from_u128(0x4004);
    let episode = Uuid::from_u128(0x4005);
    let other_library = Uuid::from_u128(0x4006);
    let other_box_set = Uuid::from_u128(0x4007);
    let silent_movie = Uuid::from_u128(0x4008);
    let db = bare_items(&[
        (collections, Folder),
        (box_set, BoxSet),
        (library, Folder),
        (series, Folder),
        (episode, Movie),
        (other_library, Folder),
        (other_box_set, BoxSet),
        (silent_movie, Movie),
    ])
    .await;
    ancestors(&db, box_set, &[collections]).await;
    ancestors(&db, series, &[library]).await;
    ancestors(&db, episode, &[series, library]).await;
    link(&db, box_set, series, LinkedChildType::Manual).await;
    streams(&db, episode, &[(Subtitle, None)]).await;
    ancestors(&db, other_box_set, &[collections]).await;
    ancestors(&db, silent_movie, &[other_library]).await;
    link(&db, other_box_set, silent_movie, LinkedChildType::Manual).await;
    // A stream of another type: the criteria, not the mere presence of a
    // stream, decides.
    streams(&db, silent_movie, &[(Video, None)]).await;

    assert_eq!(
        matching_folders(&db, has_subtitles()).await,
        BTreeSet::from([library, series, box_set, collections])
    );
}

/// `GetFolderIdsMatching_NestedLinks_AreFollowedAndCyclesTerminate`
/// (`:269-304`): a link above a link is followed, and the cycle back to the
/// outer set terminates.
#[tokio::test]
async fn folder_ids_matching_nested_links_are_followed_and_cycles_terminate() {
    use BaseItemKind::{BoxSet, Movie};
    use MediaStreamTypeEntity::{Subtitle, Video};
    let outer = Uuid::from_u128(0x4101);
    let inner = Uuid::from_u128(0x4102);
    let movie = Uuid::from_u128(0x4103);
    let silent_set = Uuid::from_u128(0x4104);
    let silent_movie = Uuid::from_u128(0x4105);
    let db = bare_items(&[
        (outer, BoxSet),
        (inner, BoxSet),
        (movie, Movie),
        (silent_set, BoxSet),
        (silent_movie, Movie),
    ])
    .await;
    link(&db, outer, inner, LinkedChildType::Manual).await;
    link(&db, inner, movie, LinkedChildType::Manual).await;
    link(&db, inner, outer, LinkedChildType::Manual).await;
    streams(&db, movie, &[(Subtitle, None)]).await;
    link(&db, silent_set, silent_movie, LinkedChildType::Manual).await;
    streams(&db, silent_movie, &[(Video, None)]).await;

    assert_eq!(
        matching_folders(&db, has_subtitles()).await,
        BTreeSet::from([inner, outer])
    );
}

/// `GetFolderIdsMatching_ClosureSeamAboveTheCollectionFolder_IsCrossed`
/// (`:306-335`): no item records the user root, so only the last closure hop
/// over the matched item's ancestors reaches it.
#[tokio::test]
async fn folder_ids_matching_closure_seam_above_the_collection_folder_is_crossed() {
    use BaseItemKind::{Folder, Movie};
    let user_root = Uuid::from_u128(0x4201);
    let collection_folder = Uuid::from_u128(0x4202);
    let series = Uuid::from_u128(0x4203);
    let episode = Uuid::from_u128(0x4204);
    let db = bare_items(&[
        (user_root, Folder),
        (collection_folder, Folder),
        (series, Folder),
        (episode, Movie),
    ])
    .await;
    ancestors(&db, episode, &[series, collection_folder]).await;
    ancestors(&db, series, &[collection_folder]).await;
    ancestors(&db, collection_folder, &[user_root]).await;
    streams(&db, episode, &[(MediaStreamTypeEntity::Subtitle, None)]).await;

    assert_eq!(
        matching_folders(&db, has_subtitles()).await,
        BTreeSet::from([series, collection_folder, user_root])
    );
}

/// `GetFolderIdsMatching_LinkedFolder_MatchesOnLanguageOnly` (`:337-364`).
#[tokio::test]
async fn folder_ids_matching_linked_folder_matches_on_language_only() {
    use BaseItemKind::{BoxSet, Folder, Movie};
    let box_set = Uuid::from_u128(0x4301);
    let series = Uuid::from_u128(0x4302);
    let episode = Uuid::from_u128(0x4303);
    let db = bare_items(&[(box_set, BoxSet), (series, Folder), (episode, Movie)]).await;
    ancestors(&db, episode, &[series]).await;
    link(&db, box_set, series, LinkedChildType::Manual).await;
    streams(
        &db,
        episode,
        &[(MediaStreamTypeEntity::Subtitle, Some("ger"))],
    )
    .await;

    assert_eq!(
        matching_folders(&db, subtitles_in("ger")).await,
        BTreeSet::from([series, box_set])
    );
    assert!(matching_folders(&db, subtitles_in("fre")).await.is_empty());
}

/// `GetFolderIdsMatching_AlternateVersionLinks_AreNotWalked` (`:366-402`):
/// only the second file carries the subtitles and hangs off the movie by an
/// alternate-version link, so the library matches through the alternate's own
/// closure but the box set linking the movie does not.
#[tokio::test]
async fn folder_ids_matching_alternate_version_links_are_not_walked() {
    use BaseItemKind::{BoxSet, Folder, Movie};
    let collections = Uuid::from_u128(0x4401);
    let box_set = Uuid::from_u128(0x4402);
    let library = Uuid::from_u128(0x4403);
    let movie = Uuid::from_u128(0x4404);
    let alternate_version = Uuid::from_u128(0x4405);
    let db = bare_items(&[
        (collections, Folder),
        (box_set, BoxSet),
        (library, Folder),
        (movie, Movie),
        (alternate_version, Movie),
    ])
    .await;
    ancestors(&db, box_set, &[collections]).await;
    ancestors(&db, movie, &[library]).await;
    ancestors(&db, alternate_version, &[library]).await;
    link(
        &db,
        movie,
        alternate_version,
        LinkedChildType::LocalAlternateVersion,
    )
    .await;
    link(&db, box_set, movie, LinkedChildType::Manual).await;
    streams(
        &db,
        alternate_version,
        &[(MediaStreamTypeEntity::Subtitle, None)],
    )
    .await;

    assert_eq!(
        matching_folders(&db, has_subtitles()).await,
        BTreeSet::from([library])
    );
}

/// `ResolveLinkParents`' `indirectLinkParents` hop
/// (`DescendantQueryHelper.cs:345-355`), which no upstream fact reaches: a
/// container that links the folder *holding* a link parent is a link parent
/// too. The box set linking the series sits in the collections folder; the
/// outer set links that folder, not the box set.
#[tokio::test]
async fn folder_ids_matching_follows_a_link_to_the_folder_holding_a_link_parent() {
    use BaseItemKind::{BoxSet, Folder, Movie};
    let outer = Uuid::from_u128(0x4501);
    let collections = Uuid::from_u128(0x4502);
    let box_set = Uuid::from_u128(0x4503);
    let series = Uuid::from_u128(0x4504);
    let episode = Uuid::from_u128(0x4505);
    let db = bare_items(&[
        (outer, BoxSet),
        (collections, Folder),
        (box_set, BoxSet),
        (series, Folder),
        (episode, Movie),
    ])
    .await;
    ancestors(&db, box_set, &[collections]).await;
    ancestors(&db, episode, &[series]).await;
    link(&db, box_set, series, LinkedChildType::Manual).await;
    link(&db, outer, collections, LinkedChildType::Manual).await;
    streams(&db, episode, &[(MediaStreamTypeEntity::Subtitle, None)]).await;

    assert_eq!(
        matching_folders(&db, has_subtitles()).await,
        BTreeSet::from([series, box_set, collections, outer])
    );
}

/// `GetItemIdsList(new InternalItemsQuery { HasSubtitles = has })`.
async fn with_subtitles(db: &Database, has: bool) -> Vec<Uuid> {
    query_ids(
        db,
        &InternalItemsQuery {
            has_subtitles: Some(has),
            ..InternalItemsQuery::default()
        },
    )
    .await
}

/// `HasSubtitles_MatchesTheItemAndItsParentFolder` (`:71-80`).
#[tokio::test]
async fn has_subtitles_matches_the_item_and_its_parent_folder() {
    let db = seeded().await;
    let found = with_subtitles(&db, true).await;
    assert!(found.contains(&WITH_SUBTITLES), "{found:?}");
    // The library is a folder, and it has a descendant with subtitles.
    assert!(found.contains(&LIBRARY), "{found:?}");
    assert!(!found.contains(&WITHOUT_SUBTITLES), "{found:?}");
}

/// `HasSubtitles_Negated_ExcludesTheItemAndItsParentFolder` (`:82-90`).
#[tokio::test]
async fn has_subtitles_negated_excludes_the_item_and_its_parent_folder() {
    let db = seeded().await;
    let found = with_subtitles(&db, false).await;
    assert!(found.contains(&WITHOUT_SUBTITLES), "{found:?}");
    assert!(!found.contains(&WITH_SUBTITLES), "{found:?}");
    assert!(!found.contains(&LIBRARY), "{found:?}");
}

/// `HasSubtitles_MatchesACollectionLinkingAFolder` (`:109-116`).
#[tokio::test]
async fn has_subtitles_matches_a_collection_linking_a_folder() {
    let db = seeded().await;
    let found = with_subtitles(&db, true).await;
    assert!(found.contains(&LINKED_SERIES), "{found:?}");
    assert!(found.contains(&COLLECTION), "{found:?}");
}

/// `HasSubtitles_Negated_ExcludesACollectionLinkingAFolder` (`:118-125`).
#[tokio::test]
async fn has_subtitles_negated_excludes_a_collection_linking_a_folder() {
    let db = seeded().await;
    let found = with_subtitles(&db, false).await;
    assert!(!found.contains(&LINKED_SERIES), "{found:?}");
    assert!(!found.contains(&COLLECTION), "{found:?}");
}

/// `HasSubtitles_MatchesAnItemWhoseAlternateVersionCarriesThem` (`:137-146`).
#[tokio::test]
async fn has_subtitles_matches_an_item_whose_alternate_version_carries_them() {
    let db = seeded().await;
    let found = with_subtitles(&db, true).await;
    assert!(found.contains(&VERSIONED_MOVIE), "{found:?}");
    assert!(found.contains(&VERSION_LIBRARY), "{found:?}");
    // The second file is never listed on its own, which is why its tracks
    // have to count for the primary.
    assert!(!found.contains(&ALTERNATE_VERSION), "{found:?}");
}

/// `HasSubtitles_Negated_ExcludesAnItemWhoseAlternateVersionCarriesThem`
/// (`:148-155`).
#[tokio::test]
async fn has_subtitles_negated_excludes_an_item_whose_alternate_version_carries_them() {
    let db = seeded().await;
    let found = with_subtitles(&db, false).await;
    assert!(!found.contains(&VERSIONED_MOVIE), "{found:?}");
    assert!(!found.contains(&VERSION_LIBRARY), "{found:?}");
}

/// The ids the resolution filter keeps for `IsHD = is_hd, Is4K = is_4k`.
async fn by_resolution(db: &Database, is_hd: Option<bool>, is_4k: Option<bool>) -> Vec<Uuid> {
    query_ids(
        db,
        &InternalItemsQuery {
            is_hd,
            is_4k,
            ..InternalItemsQuery::default()
        },
    )
    .await
}

/// `Is4K_MatchesAnItemWhoseAlternateVersionIs4K` (`:191-196`): the primary
/// file is SD; the resolution a caller can actually play is the 4K second
/// file's.
#[tokio::test]
async fn is_4k_matches_an_item_whose_alternate_version_is_4k() {
    let db = seeded().await;
    let found = by_resolution(&db, None, Some(true)).await;
    assert!(found.contains(&VERSIONED_MOVIE), "{found:?}");
}

/// `MinWidth_MatchesAnItemWhoseAlternateVersionIsWideEnough` (`:198-203`).
#[tokio::test]
async fn min_width_matches_an_item_whose_alternate_version_is_wide_enough() {
    let db = seeded().await;
    let found = query_ids(
        &db,
        &InternalItemsQuery {
            min_width: Some(3000),
            ..InternalItemsQuery::default()
        },
    )
    .await;
    assert!(found.contains(&VERSIONED_MOVIE), "{found:?}");
    assert!(!found.contains(&WITH_SUBTITLES), "{found:?}");
}

/// `MaxWidth_ExcludesAnItemWhoseAlternateVersionBreachesTheBound`
/// (`:205-211`): the SD primary is narrow enough on its own, but the 4K
/// second file is what a caller would play.
#[tokio::test]
async fn max_width_excludes_an_item_whose_alternate_version_breaches_the_bound() {
    let db = seeded().await;
    let found = query_ids(
        &db,
        &InternalItemsQuery {
            max_width: Some(1920),
            ..InternalItemsQuery::default()
        },
    )
    .await;
    assert!(!found.contains(&VERSIONED_MOVIE), "{found:?}");
    assert!(found.contains(&SD_MOVIE), "{found:?}");
}

/// `MaxHeight_ExcludesAnItemWhoseAlternateVersionBreachesTheBound`
/// (`:213-218`).
#[tokio::test]
async fn max_height_excludes_an_item_whose_alternate_version_breaches_the_bound() {
    let db = seeded().await;
    let found = query_ids(
        &db,
        &InternalItemsQuery {
            max_height: Some(1080),
            ..InternalItemsQuery::default()
        },
    )
    .await;
    assert!(!found.contains(&VERSIONED_MOVIE), "{found:?}");
    assert!(found.contains(&SD_MOVIE), "{found:?}");
}

/// The lower height bound, which upstream pins only through `MinWidth`'s
/// shape (`TranslateQuery.cs:176-181`): the 4K alternate is tall enough, the
/// plain SD movie is not.
#[tokio::test]
async fn min_height_matches_an_item_whose_alternate_version_is_tall_enough() {
    let db = seeded().await;
    let found = query_ids(
        &db,
        &InternalItemsQuery {
            min_height: Some(2000),
            ..InternalItemsQuery::default()
        },
    )
    .await;
    assert!(found.contains(&VERSIONED_MOVIE), "{found:?}");
    assert!(!found.contains(&SD_MOVIE), "{found:?}");
}

/// `IsHD_False_ExcludesAnSdPrimaryWhoseAlternateVersionIsBetter`
/// (`:220-228`): 720x480 on its own, but the version group tops out at 4K.
#[tokio::test]
async fn is_hd_false_excludes_an_sd_primary_whose_alternate_version_is_better() {
    let db = seeded().await;
    let found = by_resolution(&db, Some(false), None).await;
    assert!(!found.contains(&VERSIONED_MOVIE), "{found:?}");
    assert!(found.contains(&SD_MOVIE), "{found:?}");
}

/// `IsHD_False_MatchesAPrimaryPlacedOnlyByItsAlternateVersion` (`:230-235`):
/// the primary carries no dimensions at all; the SD second file is the
/// group's best.
#[tokio::test]
async fn is_hd_false_matches_a_primary_placed_only_by_its_alternate_version() {
    let db = seeded().await;
    let found = by_resolution(&db, Some(false), None).await;
    assert!(found.contains(&UNPROBED_MOVIE), "{found:?}");
}

/// `IsHD_True_ExcludesAnItemWhoseVersionGroupReaches4K` (`:237-246`).
#[tokio::test]
async fn is_hd_true_excludes_an_item_whose_version_group_reaches_4k() {
    let db = seeded().await;
    let found = by_resolution(&db, Some(true), None).await;
    assert!(!found.contains(&VERSIONED_MOVIE), "{found:?}");
    assert!(!found.contains(&UNPROBED_MOVIE), "{found:?}");
    // The 1920-wide second file alone would say HD; the 4K third file is the
    // group's best.
    assert!(!found.contains(&THREE_WAY_MOVIE), "{found:?}");
}

/// `Is4K_MatchesAnItemWhoseVersionGroupHoldsBothHdAnd4K` (`:248-252`).
#[tokio::test]
async fn is_4k_matches_an_item_whose_version_group_holds_both_hd_and_4k() {
    let db = seeded().await;
    let found = by_resolution(&db, None, Some(true)).await;
    assert!(found.contains(&THREE_WAY_MOVIE), "{found:?}");
}

/// `IsHD_True_MatchesAPrimaryPlacedOnlyByItsAlternateVersion` (`:254-259`):
/// the primary carries no dimensions of its own (NULL, which the HD arm's
/// "not itself 4K" must read as not 4K); the HD second file is the group's
/// best.
#[tokio::test]
async fn is_hd_true_matches_a_primary_placed_only_by_its_alternate_version() {
    let db = seeded().await;
    let found = by_resolution(&db, Some(true), None).await;
    assert!(found.contains(&HD_ONLY_BY_VERSION), "{found:?}");
}

/// `Is4K_MatchesTheSeriesOfAnEpisodeWhoseAlternateVersionIs4K` (`:261-266`):
/// the folder branch buckets a descendant the same way the item branch
/// buckets a top-level item.
#[tokio::test]
async fn is_4k_matches_the_series_of_an_episode_whose_alternate_version_is_4k() {
    let db = seeded().await;
    let found = by_resolution(&db, None, Some(true)).await;
    assert!(found.contains(&VERSIONED_SERIES), "{found:?}");
}

/// `IsHD_False_ExcludesTheSeriesOfAnSdEpisodeWithABetterAlternateVersion`
/// (`:268-274`).
#[tokio::test]
async fn is_hd_false_excludes_the_series_of_an_sd_episode_with_a_better_alternate_version() {
    let db = seeded().await;
    let found = by_resolution(&db, Some(false), None).await;
    assert!(!found.contains(&VERSIONED_SERIES), "{found:?}");
}

/// `Is4K = false` alone selects no bucket, so nothing matches — not even a
/// folder (`includeSD`/`includeHD`/`include4K` all false in both arms).
#[tokio::test]
async fn is_4k_false_alone_matches_nothing() {
    let db = seeded().await;
    let found = by_resolution(&db, None, Some(false)).await;
    assert!(found.is_empty(), "{found:?}");
}

/// Seeds a user who sees every library (a missing `Permissions` row reads as
/// `false`).
async fn seed_user(db: &Database, id: Uuid) -> UserEntity {
    let key = guid_to_db(id);
    sqlx::query(
        r#"INSERT INTO "Users"
           ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
            "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
            "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections",
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username", "NormalizedUsername")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, 'bob', 'BOB')"#,
    )
    .bind(key.clone())
    .execute(db.writer())
    .await
    .expect("seed user");
    sqlx::query(
        r#"INSERT INTO "Permissions" ("Kind", "RowVersion", "UserId", "Value")
           VALUES (?1, 1, ?2, 1)"#,
    )
    .bind(i32::from(
        ferrofin_db::enums::PermissionKind::EnableAllFolders,
    ))
    .bind(key.clone())
    .execute(db.writer())
    .await
    .expect("grant library access");
    sqlx::query_as(r#"SELECT * FROM "Users" WHERE "Id" = ?1"#)
        .bind(key)
        .fetch_one(db.pool())
        .await
        .expect("read user")
}

/// The movie-browse `GET /Items` shape: user, `Movie`, recursive, sorted, paged.
fn movie_browse(user: UserEntity) -> InternalItemsQuery {
    InternalItemsQuery {
        user: Some(user),
        include_item_types: vec![BaseItemKind::Movie],
        recursive: true,
        limit: Some(100),
        order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
        ..InternalItemsQuery::default()
    }
}

/// Under `GET /Items`' user browse — `IncludeOwnedItems` set, as the
/// controller does for every media-stream filter, so the alternate version is
/// in the row set and `ApplyGroupingFilter` folds the version group — a merged
/// movie whose subtitles and 4K file live only on its second file is listed
/// once, under the primary's id. With the filters read off the item's own row
/// alone, the alternate was the group's only match and surfaced as a
/// standalone movie.
#[tokio::test]
async fn a_version_group_matching_only_on_its_alternate_lists_the_primary() {
    const PRIMARY: Uuid = Uuid::from_u128(0x3001);
    const ALTERNATE: Uuid = Uuid::from_u128(0x3002);
    const CONTROL: Uuid = Uuid::from_u128(0x3003);
    const MOVIES: Uuid = Uuid::from_u128(0x3000);
    let db = fresh_db().await;
    let user = seed_user(&db, Uuid::from_u128(0xB02)).await;
    let keyed = |entity: BaseItemEntity, key: &str| BaseItemEntity {
        presentation_unique_key: Some(key.to_owned()),
        top_parent_id: Some(guid_to_db(MOVIES)),
        ..entity
    };
    let rows = vec![
        row(MOVIES, BaseItemKind::CollectionFolder, None, None),
        keyed(row(PRIMARY, BaseItemKind::Movie, None, SD), "merged"),
        keyed(
            row(ALTERNATE, BaseItemKind::Movie, Some(PRIMARY), UHD),
            "merged",
        ),
        keyed(row(CONTROL, BaseItemKind::Movie, None, SD), "control"),
    ];
    FerrofinItemPersistenceService::new(db.clone())
        .save_items(&rows)
        .await
        .expect("save");
    streams(
        &db,
        ALTERNATE,
        &[(MediaStreamTypeEntity::Subtitle, Some("eng"))],
    )
    .await;

    let repository = FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()));
    let browse = InternalItemsQuery {
        include_owned_items: true,
        top_parent_ids: vec![MOVIES],
        ..movie_browse(user)
    };
    for filter in [
        InternalItemsQuery {
            has_subtitles: Some(true),
            ..browse.clone()
        },
        InternalItemsQuery {
            is_4k: Some(true),
            ..browse.clone()
        },
    ] {
        let page = repository.get_items(&filter).await.expect("page");
        let listed: Vec<Uuid> = page
            .items
            .iter()
            .map(|item| Uuid::parse_str(&item.id).expect("id"))
            .collect();
        assert_eq!(listed, [PRIMARY], "{filter:?}");
        assert_eq!(page.total_record_count, 1, "{filter:?}");
    }
}

/// `(sql, bind count)` of `filter` in `shape`.
fn sql_of(filter: &InternalItemsQuery, shape: QueryShape) -> (String, usize) {
    let mut builder = build_query(filter, shape);
    let query = builder.build();
    let sql = query.sql().as_str().to_owned();
    let binds = sql.matches('?').count();
    (sql, binds)
}

/// The stream predicate's first and last bytes, as emitted.
const PREDICATE_HEAD: &str =
    r#" AND ((+bi."IsFolder" = 0 AND (EXISTS (SELECT 1 FROM "MediaStreamInfos" ms"#;
const PREDICATE_TAIL: &str = r#"(SELECT "Id" FROM link_ancestors))))"#;

/// Hot-path guard: with no language the statement carries no trace of the
/// filter, and with one the filter is a single self-contained `AND` term —
/// cut it out and the statement is byte-identical to the unfiltered one, in
/// both the page and the grouped-count shapes `GET /Items` runs.
#[tokio::test]
async fn empty_language_lists_leave_the_browse_statement_unchanged() {
    let db = fresh_db().await;
    let user = seed_user(&db, Uuid::from_u128(0xB00)).await;
    for shape in [QueryShape::IdsOnly, QueryShape::GroupedCount] {
        let plain = movie_browse(user.clone());
        let (unfiltered, unfiltered_binds) = sql_of(&plain, shape);
        assert!(!unfiltered.contains("MediaStreamInfos"), "{unfiltered}");

        let filtered = InternalItemsQuery {
            audio_languages: vec!["eng".to_owned()],
            ..plain
        };
        let (sql, binds) = sql_of(&filtered, shape);
        let start = sql.find(PREDICATE_HEAD).expect("predicate emitted");
        let end = sql.find(PREDICATE_TAIL).expect("predicate closed") + PREDICATE_TAIL.len();
        assert_eq!(format!("{}{}", &sql[..start], &sql[end..]), unfiltered);
        // One bind per language in each of the four places it is matched.
        assert_eq!(binds, unfiltered_binds + 4, "{shape:?}");
    }
}

/// `EXPLAIN QUERY PLAN` of `filter` as a full-row page.
async fn plan_of(db: &Database, filter: &InternalItemsQuery) -> Vec<String> {
    let mut builder = build_query(filter, QueryShape::FullRows);
    let mut query = builder.build();
    let args = query.take_arguments().expect("arguments").expect("binds");
    let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
    sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
        .fetch_all(db.pool())
        .await
        .expect("explain")
        .iter()
        .map(|row| row.get::<String, _>("detail"))
        .collect()
}

/// The filtered browse stays index-driven: the item's own streams are a seek
/// on `ItemId`, the version arm reads only the partial `PrimaryVersionId`
/// index, the folder arm's match set seeks the `StreamType`-led stream index,
/// and the outer `BaseItems` access path is the unfiltered browse's.
#[tokio::test]
async fn filtered_browse_plan_is_index_driven() {
    let db = fresh_db().await;
    let user = seed_user(&db, Uuid::from_u128(0xB01)).await;
    let plain = movie_browse(user);
    let filtered = InternalItemsQuery {
        audio_languages: vec!["eng".to_owned()],
        include_owned_items: true,
        ..plain.clone()
    };
    let unfiltered_plan = plan_of(
        &db,
        &InternalItemsQuery {
            include_owned_items: true,
            ..plain
        },
    )
    .await;
    let filtered_plan = plan_of(&db, &filtered).await;
    let joined = filtered_plan.join(" | ");

    assert!(
        !filtered_plan.iter().any(|d| d.starts_with("SCAN ms")),
        "no stream table scan: {joined}"
    );
    assert!(
        filtered_plan
            .iter()
            .any(|d| d.starts_with("SEARCH ms USING") && d.contains("ItemId=?")),
        "own streams are a seek on ItemId: {joined}"
    );
    assert!(
        !filtered_plan.iter().any(|d| d.starts_with("SCAN v")),
        "no BaseItems scan for versions: {joined}"
    );
    assert!(
        filtered_plan
            .iter()
            .any(|d| d.contains(" v USING") && d.contains("IX_BaseItems_PrimaryVersionId")),
        "versions read the partial PrimaryVersionId index: {joined}"
    );
    assert!(
        filtered_plan
            .iter()
            .any(|d| d.starts_with("SEARCH ms USING")
                && d.contains("IX_MediaStreamInfos_StreamType_ItemId_Language_IsExternal")
                && d.contains("StreamType=?")),
        "the match set seeks the StreamType-led index: {joined}"
    );
    // Every `BaseItems` read of the browse — the paged key scan and the
    // hydration seek — is the unfiltered statement's, in order.
    let bi_steps = |steps: &[String]| -> Vec<String> {
        steps
            .iter()
            .filter(|d| d.starts_with("SEARCH bi ") || d.starts_with("SCAN bi"))
            .cloned()
            .collect()
    };
    assert!(
        bi_steps(&filtered_plan)
            .iter()
            .any(|d| d.contains("Type=?")),
        "the browse is Type-led: {joined}"
    );
    assert_eq!(
        bi_steps(&filtered_plan),
        bi_steps(&unfiltered_plan),
        "the filter must not move the browse onto another index: {joined}"
    );
}

/// The language-absence and chapter-image filters take the same roll-up
/// shape: the browse keeps its own `BaseItems` steps, the stream match set
/// seeks the `StreamType`-led index, and an item's own chapters are a seek on
/// the chapter key. The folder arm's chapter set (`ImagePath IS NOT NULL`)
/// scans `Chapters`, which has only its key, as upstream's table does; it is
/// uncorrelated, so it runs once, and only when a folder row reaches it.
#[tokio::test]
async fn absence_and_chapter_filter_plans_keep_the_browse_steps() {
    let db = fresh_db().await;
    let user = seed_user(&db, Uuid::from_u128(0xB02)).await;
    let plain = InternalItemsQuery {
        include_owned_items: true,
        ..movie_browse(user)
    };
    let bi_steps = |steps: &[String]| -> Vec<String> {
        steps
            .iter()
            .filter(|d| d.starts_with("SEARCH bi ") || d.starts_with("SCAN bi"))
            .cloned()
            .collect()
    };
    let unfiltered = bi_steps(&plan_of(&db, &plain).await);
    let streams: [fn(&mut InternalItemsQuery); 4] = [
        |q| q.has_no_audio_track_with_language = Some("eng".to_owned()),
        |q| q.has_no_internal_subtitle_track_with_language = Some("eng".to_owned()),
        |q| q.has_no_external_subtitle_track_with_language = Some("eng".to_owned()),
        |q| q.has_no_subtitle_track_with_language = Some("eng".to_owned()),
    ];
    for set in streams {
        let mut filter = plain.clone();
        set(&mut filter);
        let steps = plan_of(&db, &filter).await;
        let joined = steps.join(" | ");
        assert!(!steps.iter().any(|d| d.starts_with("SCAN ms")), "{joined}");
        assert!(
            steps.iter().any(|d| d.starts_with("SEARCH ms USING")
                && d.contains("IX_MediaStreamInfos_StreamType_ItemId_Language_IsExternal")
                && d.contains("StreamType=?")),
            "{joined}"
        );
        assert_eq!(bi_steps(&steps), unfiltered, "{joined}");
    }
    for want in [true, false] {
        let filter = InternalItemsQuery {
            has_chapter_images: Some(want),
            ..plain.clone()
        };
        let steps = plan_of(&db, &filter).await;
        let joined = steps.join(" | ");
        assert!(
            steps
                .iter()
                .any(|d| d.starts_with("SEARCH ms USING") && d.contains("ItemId=?")),
            "{joined}"
        );
        assert_eq!(bi_steps(&steps), unfiltered, "{joined}");
    }
}
