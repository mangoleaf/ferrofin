//! The item filters over an item's own metadata that `GET /Items` binds
//! straight onto the query (v12.2 `ItemsController.GetItems`) and
//! `BaseItemRepository.TranslateQuery` translates:
//! - `SeriesStatuses`: a substring of the `Data` blob (`TranslateQuery.cs:1167-1172`);
//! - `OfficialRatings`, `HasOfficialRating`, `VideoTypes`, `Is3D` and
//!   `IsPlaceHolder`: the item, or an item below it (`AncestorIds`) or linked
//!   to it (`LinkedChildren`), matches
//!   (`FolderAwareFilterExtensions.WhereItemOrDescendantMatches`,
//!   `TranslateQuery.cs:684-688, 761-768, 1181-1208`);
//! - `HasTrailer`, `HasThemeSong`, `HasThemeVideo` and `HasSpecialFeature`:
//!   the same roll-up over the owners of such an extra, each owning version
//!   standing for its primary too (`WithPrimaryVersions`,
//!   `TranslateQuery.cs:1210-1274`);
//! - `MinParentalRating`, `MaxParentalRating` and `HasParentalRating`
//!   (`TranslateQuery.cs:690-759`), with the maximum clamped to the user's own
//!   limit;
//! - `TrailerTypes`, `HasDeadParentId` and the provider-id maps
//!   (`ExcludeProviderIds`, `HasAnyProviderId`, `HasAnyProviderIds`;
//!   `TranslateQuery.cs:427-431, 970-974, 1057-1070`).

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use ferrofin_core::item_type_lookup::{ItemTypeLookup, stored_type_name};
use ferrofin_core::translate_query::{QueryShape, build_query};
use ferrofin_core::{FerrofinItemPersistenceService, FerrofinItemRepository};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::enums::LinkedChildType;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::{SeriesStatus, TrailerType};
use ferrofin_model::entities_media::ParentalRatingScore;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{ItemPersistenceService as _, ItemRepository as _};
use sqlx::{Execute as _, Row as _};
use uuid::Uuid;

/// A continuing series with no rating of its own; its one episode is rated.
const CONTINUING: Uuid = Uuid::from_u128(0x3001);
const RATED_EPISODE: Uuid = Uuid::from_u128(0x3002);
/// An ended series rated on its own, with no episodes.
const ENDED_RATED: Uuid = Uuid::from_u128(0x3003);
/// An ended series whose one episode is unrated too.
const ENDED_UNRATED: Uuid = Uuid::from_u128(0x3004);
const UNRATED_EPISODE: Uuid = Uuid::from_u128(0x3005);
/// A rated offline disc stub (`"IsPlaceHolder":true`).
const STUB: Uuid = Uuid::from_u128(0x3006);
/// A movie whose `OfficialRating` is the empty string.
const EMPTY_RATING: Uuid = Uuid::from_u128(0x3007);
/// A box set with no rating of its own that links the stub.
const COLLECTION: Uuid = Uuid::from_u128(0x3008);
/// A movie whose `Data` holds "extended" in lower case: `Contains("Ended")`
/// is ordinal, so it is no ended series.
const EXTENDED_CUT: Uuid = Uuid::from_u128(0x3009);

const ALL: [Uuid; 9] = [
    CONTINUING,
    RATED_EPISODE,
    ENDED_RATED,
    ENDED_UNRATED,
    UNRATED_EPISODE,
    STUB,
    EMPTY_RATING,
    COLLECTION,
    EXTENDED_CUT,
];

async fn fresh_db() -> Database {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    db
}

fn row(
    id: Uuid,
    kind: BaseItemKind,
    rating: Option<(&str, i64)>,
    data: Option<&str>,
) -> BaseItemEntity {
    BaseItemEntity {
        id: guid_to_db(id),
        type_: stored_type_name(kind).expect("stored type").to_owned(),
        name: Some(format!("{id}")),
        is_folder: matches!(kind, BaseItemKind::Series | BaseItemKind::BoxSet),
        official_rating: rating.map(|(rating, _)| rating.to_owned()),
        inherited_parental_rating_value: rating.map(|(_, score)| score),
        data: data.map(str::to_owned),
        ..BaseItemEntity::default()
    }
}

async fn seeded() -> Database {
    use BaseItemKind::{BoxSet, Episode, Movie, Series};
    let db = fresh_db().await;
    let rows = [
        row(CONTINUING, Series, None, Some(r#"{"Status":"Continuing"}"#)),
        row(RATED_EPISODE, Episode, Some(("TV-14", 14)), None),
        row(
            ENDED_RATED,
            Series,
            Some(("TV-MA", 18)),
            Some(r#"{"Status":"Ended"}"#),
        ),
        row(ENDED_UNRATED, Series, None, Some(r#"{"Status":"Ended"}"#)),
        row(UNRATED_EPISODE, Episode, None, None),
        row(
            STUB,
            Movie,
            Some(("PG", 10)),
            Some(r#"{"IsPlaceHolder":true,"VideoType":"VideoFile"}"#),
        ),
        BaseItemEntity {
            official_rating: Some(String::new()),
            ..row(EMPTY_RATING, Movie, None, None)
        },
        row(COLLECTION, BoxSet, None, None),
        row(
            EXTENDED_CUT,
            Movie,
            None,
            Some(r#"{"LocalAlternateVersions":["/m/extended.mkv"]}"#),
        ),
    ];
    FerrofinItemPersistenceService::new(db.clone())
        .save_items(&rows)
        .await
        .expect("save");
    for (item, parent) in [
        (RATED_EPISODE, CONTINUING),
        (UNRATED_EPISODE, ENDED_UNRATED),
    ] {
        sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
            .bind(guid_to_db(item))
            .bind(guid_to_db(parent))
            .execute(db.writer())
            .await
            .expect("ancestor");
    }
    sqlx::query(
        r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
           VALUES (?1, 0, ?2, ?3)"#,
    )
    .bind(guid_to_db(COLLECTION))
    .bind(guid_to_db(STUB))
    .bind(i32::from(LinkedChildType::Manual))
    .execute(db.writer())
    .await
    .expect("link");
    db
}

/// The fixture items `GetItemIdsList(query)` returns.
async fn ids(db: &Database, query: &InternalItemsQuery) -> BTreeSet<Uuid> {
    ids_among(db, query, &ALL).await
}

/// The items of `universe` that `GetItemIdsList(query)` returns.
async fn ids_among(db: &Database, query: &InternalItemsQuery, universe: &[Uuid]) -> BTreeSet<Uuid> {
    FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()))
        .get_item_ids(query)
        .await
        .expect("query")
        .into_iter()
        .filter(|id| universe.contains(id))
        .collect()
}

fn set<const N: usize>(ids: [Uuid; N]) -> BTreeSet<Uuid> {
    ids.into_iter().collect()
}

/// Every fixture item but `ids`.
fn all_but<const N: usize>(ids: [Uuid; N]) -> BTreeSet<Uuid> {
    ALL.into_iter().filter(|id| !ids.contains(id)).collect()
}

fn statuses(statuses: &[SeriesStatus]) -> InternalItemsQuery {
    InternalItemsQuery {
        series_statuses: statuses.to_vec(),
        ..InternalItemsQuery::default()
    }
}

/// `seriesStatus.Any(f => e.Data!.Contains(f))`: the status name anywhere in
/// the item's `Data`, compared ordinally.
#[tokio::test]
async fn series_status_matches_the_status_name_in_the_data_blob() {
    let db = seeded().await;
    assert_eq!(
        ids(&db, &statuses(&[SeriesStatus::Continuing])).await,
        set([CONTINUING])
    );
    assert_eq!(
        ids(&db, &statuses(&[SeriesStatus::Ended])).await,
        set([ENDED_RATED, ENDED_UNRATED]),
        "`extended` in a path is not `Ended`"
    );
    assert!(
        ids(&db, &statuses(&[SeriesStatus::Unreleased]))
            .await
            .is_empty()
    );
    assert_eq!(
        ids(
            &db,
            &statuses(&[SeriesStatus::Continuing, SeriesStatus::Ended])
        )
        .await,
        set([CONTINUING, ENDED_RATED, ENDED_UNRATED])
    );
}

/// `HasOfficialRating` rolls up: an unrated series with a rated episode is
/// rated, and so is a box set linking a rated movie. `true` and `false`
/// split every item between them, as on Jellyfin (3253 + 17207 = 20460 on the
/// benchmark corpus).
#[tokio::test]
async fn has_official_rating_rolls_up_from_descendants_and_linked_children() {
    let db = seeded().await;
    let rated = set([CONTINUING, RATED_EPISODE, ENDED_RATED, STUB, COLLECTION]);
    let query = |has| InternalItemsQuery {
        has_official_rating: Some(has),
        ..InternalItemsQuery::default()
    };
    assert_eq!(ids(&db, &query(true)).await, rated);
    assert_eq!(
        ids(&db, &query(false)).await,
        all_but([CONTINUING, RATED_EPISODE, ENDED_RATED, STUB, COLLECTION]),
        "an empty OfficialRating is no rating"
    );
}

/// `IsPlaceHolder` takes the same roll-up over `"IsPlaceHolder":true`.
#[tokio::test]
async fn is_place_holder_rolls_up_from_linked_children() {
    let db = seeded().await;
    let query = |is| InternalItemsQuery {
        is_place_holder: Some(is),
        ..InternalItemsQuery::default()
    };
    assert_eq!(ids(&db, &query(true)).await, set([STUB, COLLECTION]));
    assert_eq!(ids(&db, &query(false)).await, all_but([STUB, COLLECTION]));
}

/// `OfficialRatings` rolls up as `HasOfficialRating` does: an unrated series
/// takes its episode's rating, and a box set its member's.
#[tokio::test]
async fn official_ratings_roll_up_from_descendants_and_linked_children() {
    let db = seeded().await;
    let query = |ratings: &[&str]| InternalItemsQuery {
        official_ratings: ratings.iter().map(|&rating| rating.to_owned()).collect(),
        ..InternalItemsQuery::default()
    };
    assert_eq!(
        ids(&db, &query(&["TV-14"])).await,
        set([CONTINUING, RATED_EPISODE])
    );
    assert_eq!(ids(&db, &query(&["PG"])).await, set([STUB, COLLECTION]));
    assert_eq!(
        ids(&db, &query(&["PG", "TV-MA"])).await,
        set([STUB, COLLECTION, ENDED_RATED])
    );
}

/// A movie with a second version (`Film - 4K`) whose trailer is named after
/// that version, so the version owns it (`owner_for_extra`), plus a
/// featurette the movie owns itself.
const FILM: Uuid = Uuid::from_u128(0x3101);
const FILM_4K: Uuid = Uuid::from_u128(0x3102);
const FILM_4K_TRAILER: Uuid = Uuid::from_u128(0x3103);
const FILM_FEATURETTE: Uuid = Uuid::from_u128(0x3104);
/// A box set whose only member is the movie.
const FILM_SET: Uuid = Uuid::from_u128(0x3105);
/// A series that owns a theme video, with one 3D DVD episode that owns a
/// theme song.
const SHOW: Uuid = Uuid::from_u128(0x3106);
const SHOW_EPISODE: Uuid = Uuid::from_u128(0x3107);
const SHOW_THEME_VIDEO: Uuid = Uuid::from_u128(0x3108);
const EPISODE_THEME_SONG: Uuid = Uuid::from_u128(0x3109);
/// A movie with no extras whose path holds "video3dformat" in lower case:
/// `Contains("Video3DFormat")` is ordinal, so it is not 3D.
const PLAIN: Uuid = Uuid::from_u128(0x310A);
/// A Blu-ray disc image: `VideoType` `Iso`, `IsoType` `BluRay`.
const DISC: Uuid = Uuid::from_u128(0x310B);

/// The items a browse lists: no extra, no alternate version.
const LISTED: [Uuid; 6] = [FILM, FILM_SET, SHOW, SHOW_EPISODE, PLAIN, DISC];

async fn seeded_extras() -> Database {
    use BaseItemKind::{Audio, BoxSet, Episode, Movie, Series, Trailer, Video};
    let db = fresh_db().await;
    let extra = |id, kind, extra_type, owner| BaseItemEntity {
        extra_type: Some(extra_type),
        owner_id: Some(guid_to_db(owner)),
        ..row(id, kind, None, None)
    };
    let rows = [
        row(FILM, Movie, None, Some(r#"{"VideoType":"VideoFile"}"#)),
        BaseItemEntity {
            primary_version_id: Some(guid_to_db(FILM)),
            ..row(FILM_4K, Movie, None, None)
        },
        extra(FILM_4K_TRAILER, Trailer, 2, FILM_4K),
        extra(FILM_FEATURETTE, Video, 10, FILM),
        row(FILM_SET, BoxSet, None, None),
        row(SHOW, Series, None, None),
        row(
            SHOW_EPISODE,
            Episode,
            None,
            Some(r#"{"VideoType":"Dvd","Video3DFormat":"FullSideBySide"}"#),
        ),
        extra(SHOW_THEME_VIDEO, Video, 9, SHOW),
        extra(EPISODE_THEME_SONG, Audio, 8, SHOW_EPISODE),
        row(
            PLAIN,
            Movie,
            None,
            Some(r#"{"Path":"/m/plain-video3dformat.mkv","VideoType":"VideoFile"}"#),
        ),
        row(
            DISC,
            Movie,
            None,
            Some(r#"{"VideoType":"Iso","IsoType":"BluRay"}"#),
        ),
    ];
    FerrofinItemPersistenceService::new(db.clone())
        .save_items(&rows)
        .await
        .expect("save");
    sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
        .bind(guid_to_db(SHOW_EPISODE))
        .bind(guid_to_db(SHOW))
        .execute(db.writer())
        .await
        .expect("ancestor");
    sqlx::query(
        r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
           VALUES (?1, 0, ?2, ?3)"#,
    )
    .bind(guid_to_db(FILM_SET))
    .bind(guid_to_db(FILM))
    .bind(i32::from(LinkedChildType::Manual))
    .execute(db.writer())
    .await
    .expect("link");
    db
}

/// The listed items but `ids`.
fn listed_but<const N: usize>(ids: [Uuid; N]) -> BTreeSet<Uuid> {
    LISTED.into_iter().filter(|id| !ids.contains(id)).collect()
}

/// The four extras filters: an extra owned by an alternate version counts
/// for its primary (`WithPrimaryVersions`), and a series or box set has one
/// when an item below it or linked to it does. `false` is the exact
/// complement.
#[tokio::test]
async fn extras_filters_count_a_versions_extras_and_roll_up_to_folders() {
    type Field = fn(&mut InternalItemsQuery) -> &mut Option<bool>;
    let db = seeded_extras().await;
    let cases: [(&str, Field, BTreeSet<Uuid>); 4] = [
        ("trailer", |q| &mut q.has_trailer, set([FILM, FILM_SET])),
        (
            "theme song",
            |q| &mut q.has_theme_song,
            set([SHOW, SHOW_EPISODE]),
        ),
        ("theme video", |q| &mut q.has_theme_video, set([SHOW])),
        (
            "special feature",
            |q| &mut q.has_special_feature,
            set([FILM, FILM_SET]),
        ),
    ];
    for (name, field, with) in cases {
        for want in [true, false] {
            let mut query = InternalItemsQuery::default();
            *field(&mut query) = Some(want);
            let expected = if want {
                with.clone()
            } else {
                LISTED.into_iter().filter(|id| !with.contains(id)).collect()
            };
            assert_eq!(
                ids_among(&db, &query, &LISTED).await,
                expected,
                "{name} = {want}"
            );
        }
    }
}

/// `VideoTypes` and `Is3D` roll up from the episode to its series, and match
/// `Data` ordinally (EF's `Contains` is `instr`, not `LIKE`).
#[tokio::test]
async fn video_types_and_3d_roll_up_and_match_ordinally() {
    use ferrofin_model::entities::VideoType;
    let db = seeded_extras().await;
    let types = |types: &[VideoType]| InternalItemsQuery {
        video_types: types.to_vec(),
        ..InternalItemsQuery::default()
    };
    assert_eq!(
        ids_among(&db, &types(&[VideoType::Dvd]), &LISTED).await,
        set([SHOW, SHOW_EPISODE])
    );
    assert_eq!(
        ids_among(&db, &types(&[VideoType::VideoFile]), &LISTED).await,
        set([FILM, FILM_SET, PLAIN])
    );
    assert_eq!(
        ids_among(&db, &types(&[VideoType::Dvd, VideoType::BluRay]), &LISTED).await,
        set([SHOW, SHOW_EPISODE, DISC]),
        "a disc image matches on its `IsoType`"
    );
    let three_d = |is| InternalItemsQuery {
        is_3d: Some(is),
        ..InternalItemsQuery::default()
    };
    assert_eq!(
        ids_among(&db, &three_d(true), &LISTED).await,
        set([SHOW, SHOW_EPISODE]),
        "a lower-case `video3dformat` in a path is not 3D"
    );
    assert_eq!(
        ids_among(&db, &three_d(false), &LISTED).await,
        listed_but([SHOW, SHOW_EPISODE])
    );
}

/// `MinParentalRating` keeps unrated items and those at or above the
/// minimum, the sub-score breaking a tie on the score.
#[tokio::test]
async fn minimum_rating_keeps_unrated_items() {
    let db = seeded().await;
    let query = |score, sub| InternalItemsQuery {
        min_parental_rating: Some(ParentalRatingScore::new(score, sub)),
        ..InternalItemsQuery::default()
    };
    assert_eq!(ids(&db, &query(14, None)).await, all_but([STUB]));
    assert_eq!(
        ids(&db, &query(14, Some(1))).await,
        all_but([STUB, RATED_EPISODE])
    );
}

/// `HasParentalRating`: `true` adds nothing of its own, so on its own it
/// filters nothing; `false` keeps the unrated items, but only when no other rating rule
/// applies (the last arm of `TranslateQuery.cs:709-759`).
#[tokio::test]
async fn has_parental_rating_false_keeps_unrated_items_when_no_bound_applies() {
    let db = seeded().await;
    let query = |has| InternalItemsQuery {
        has_parental_rating: Some(has),
        ..InternalItemsQuery::default()
    };
    assert_eq!(ids(&db, &query(true)).await, set(ALL));
    let true_with_min = InternalItemsQuery {
        min_parental_rating: Some(ParentalRatingScore::new(14, None)),
        ..query(true)
    };
    assert_eq!(
        ids(&db, &true_with_min).await,
        all_but([STUB]),
        "the `true` arm applies the bounds too"
    );
    let unrated = all_but([RATED_EPISODE, ENDED_RATED, STUB]);
    assert_eq!(ids(&db, &query(false)).await, unrated);
    // A bound takes the arm before it: the bound applies, not the null test.
    let bounded = InternalItemsQuery {
        min_parental_rating: Some(ParentalRatingScore::new(14, None)),
        ..query(false)
    };
    assert_eq!(ids(&db, &bounded).await, all_but([STUB]));
    let capped = InternalItemsQuery {
        max_parental_rating: Some(ParentalRatingScore::new(17, None)),
        ..query(false)
    };
    assert_eq!(ids(&db, &capped).await, all_but([ENDED_RATED]));
}

/// The fixture items the translated statement for `filter` selects, without
/// the repository's library-access scoping (the fixture has no libraries).
async fn translated_ids(db: &Database, filter: &InternalItemsQuery) -> BTreeSet<Uuid> {
    translated_all(db, filter)
        .await
        .into_iter()
        .filter(|id| ALL.contains(id))
        .collect()
}

/// Every item the translated statement for `filter` selects.
async fn translated_all(db: &Database, filter: &InternalItemsQuery) -> BTreeSet<Uuid> {
    let mut builder = build_query(filter, QueryShape::IdsOnly);
    let mut query = builder.build();
    let args = query.take_arguments().expect("arguments").expect("binds");
    let sql = query.sql().as_str().to_owned();
    sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
        .fetch_all(db.pool())
        .await
        .expect("query")
        .iter()
        .map(|row| Uuid::parse_str(&row.get::<String, _>(0)).expect("id"))
        .collect()
}

/// A user row as the store reads it back.
async fn stored_user(db: &Database) -> UserEntity {
    let key = guid_to_db(Uuid::from_u128(0xB0B));
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
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, 'kid', 'KID')"#,
    )
    .bind(key.clone())
    .execute(db.writer())
    .await
    .expect("seed user");
    sqlx::query_as(r#"SELECT * FROM "Users" WHERE "Id" = ?1"#)
        .bind(key)
        .fetch_one(db.pool())
        .await
        .expect("read user")
}

/// `hasParentalRating=false` drops its null test when the user's own limit
/// or an unrated-kind block applies: those arms come first in upstream's
/// chain, and they keep rated items within the limit.
#[tokio::test]
async fn has_parental_rating_false_yields_to_the_users_limit_and_unrated_block() {
    use ferrofin_model::data::UnratedItem;
    let db = seeded().await;
    let limited = UserEntity {
        max_parental_rating_score: Some(14),
        ..stored_user(&db).await
    };
    let by_policy = InternalItemsQuery {
        user: Some(Arc::new(limited)),
        has_parental_rating: Some(false),
        ..InternalItemsQuery::default()
    };
    assert_eq!(
        translated_ids(&db, &by_policy).await,
        all_but([ENDED_RATED]),
        "the user's limit, not the null test"
    );
    let blocked = InternalItemsQuery {
        block_unrated_items: vec![UnratedItem::Movie],
        has_parental_rating: Some(false),
        ..InternalItemsQuery::default()
    };
    let found = translated_ids(&db, &blocked).await;
    for rated in [RATED_EPISODE, ENDED_RATED, STUB] {
        assert!(
            found.contains(&rated),
            "{rated}: the unrated block, not the null test"
        );
    }
    for unrated_movie in [EMPTY_RATING, EXTENDED_CUT] {
        assert!(!found.contains(&unrated_movie), "{unrated_movie}");
    }
}

/// `hasParentalRating=true` keeps an unrated-kind block, where upstream's
/// `HasParentalRating` arm skips `BlockUnratedItems`: a request may narrow a
/// parental restriction, never lift it (gl-954).
#[tokio::test]
async fn has_parental_rating_true_keeps_the_unrated_block() {
    use ferrofin_model::data::UnratedItem;
    let db = seeded().await;
    let blocked = InternalItemsQuery {
        block_unrated_items: vec![UnratedItem::Movie],
        has_parental_rating: Some(true),
        ..InternalItemsQuery::default()
    };
    let found = translated_ids(&db, &blocked).await;
    for rated in [RATED_EPISODE, ENDED_RATED, STUB] {
        assert!(found.contains(&rated), "{rated}");
    }
    for unrated_movie in [EMPTY_RATING, EXTENDED_CUT] {
        assert!(!found.contains(&unrated_movie), "{unrated_movie}");
    }
}

/// The stricter limit compares the sub-score when the scores tie: a request
/// of `(13, none)` is stricter than a policy of `(13, 5)`.
#[tokio::test]
async fn the_stricter_limit_breaks_a_score_tie_on_the_sub_score() {
    let db = seeded().await;
    let tie = Uuid::from_u128(0x300A);
    FerrofinItemPersistenceService::new(db.clone())
        .save_items(&[BaseItemEntity {
            inherited_parental_rating_sub_value: Some(3),
            ..row(tie, BaseItemKind::Movie, Some(("PG-13", 13)), None)
        }])
        .await
        .expect("save");
    let user = UserEntity {
        max_parental_rating_score: Some(13),
        max_parental_rating_sub_score: Some(5),
        ..stored_user(&db).await
    };
    let policy_only = InternalItemsQuery {
        user: Some(Arc::new(user.clone())),
        ..InternalItemsQuery::default()
    };
    assert!(translated_all(&db, &policy_only).await.contains(&tie));
    let stricter_request = InternalItemsQuery {
        max_parental_rating: Some(ParentalRatingScore::new(13, None)),
        ..policy_only
    };
    assert!(!translated_all(&db, &stricter_request).await.contains(&tie));
}

/// A requested maximum can lower the user's own limit but not raise it
/// (upstream's request value replaces the one `SetUser` copied from the
/// user's policy).
#[tokio::test]
async fn maximum_rating_never_exceeds_the_users_limit() {
    let db = seeded().await;
    let user = UserEntity {
        max_parental_rating_score: Some(10),
        ..stored_user(&db).await
    };
    let capped = |score: Option<i32>| InternalItemsQuery {
        user: Some(Arc::new(user.clone())),
        max_parental_rating: score.map(|score| ParentalRatingScore::new(score, None)),
        ..InternalItemsQuery::default()
    };
    let within_ten = all_but([RATED_EPISODE, ENDED_RATED]);
    assert_eq!(translated_ids(&db, &capped(None)).await, within_ten);
    assert_eq!(
        translated_ids(&db, &capped(Some(17))).await,
        within_ten,
        "a looser request keeps the user's limit"
    );
    assert_eq!(
        translated_ids(&db, &capped(Some(0))).await,
        all_but([RATED_EPISODE, ENDED_RATED, STUB, COLLECTION]),
        "a stricter request applies; the box set's only member is over it"
    );
    // Without a user the request is the only limit.
    let anonymous = InternalItemsQuery {
        user: None,
        ..capped(Some(17))
    };
    assert_eq!(
        translated_ids(&db, &anonymous).await,
        all_but([ENDED_RATED])
    );
    assert_eq!(
        ids(&db, &anonymous).await,
        all_but([ENDED_RATED]),
        "the repository path agrees"
    );
}

/// `EXPLAIN QUERY PLAN` of `filter` as `GetItemIdsList` would run it.
async fn plan(db: &Database, filter: &InternalItemsQuery) -> String {
    let mut builder = build_query(filter, QueryShape::IdsOnly);
    let mut query = builder.build();
    let args = query.take_arguments().expect("arguments").expect("binds");
    let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
    sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
        .fetch_all(db.pool())
        .await
        .expect("explain")
        .iter()
        .map(|row| row.get::<String, _>("detail"))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The roll-up is one uncorrelated set whose matching items are scanned for
/// once and then probed by id, so a typed browse keeps its own index seek.
#[tokio::test]
async fn descendant_roll_up_plan_builds_its_set_once() {
    let db = seeded().await;
    for filter in [
        InternalItemsQuery {
            has_official_rating: Some(true),
            ..InternalItemsQuery::default()
        },
        InternalItemsQuery {
            has_official_rating: Some(false),
            ..InternalItemsQuery::default()
        },
        InternalItemsQuery {
            is_place_holder: Some(true),
            ..InternalItemsQuery::default()
        },
        InternalItemsQuery {
            official_ratings: vec!["PG".to_owned(), "TV-14".to_owned()],
            ..InternalItemsQuery::default()
        },
        InternalItemsQuery {
            video_types: vec![ferrofin_model::entities::VideoType::BluRay],
            ..InternalItemsQuery::default()
        },
        InternalItemsQuery {
            is_3d: Some(false),
            ..InternalItemsQuery::default()
        },
    ] {
        let filter = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Series],
            ..filter
        };
        let plan = plan(&db, &filter).await;
        assert!(plan.contains("SEARCH bi USING INDEX"), "{plan}");
        assert!(plan.contains("(Type=?"), "{plan}");
        assert!(plan.contains("LIST SUBQUERY"), "{plan}");
        assert!(plan.contains("MATERIALIZE direct"), "{plan}");
        assert_eq!(format!("{plan} |").matches("SCAN d |").count(), 1, "{plan}");
        assert!(!plan.contains("SCAN a "), "ancestors are a seek: {plan}");
        assert!(!plan.contains("SCAN lc "), "links are a seek: {plan}");
        assert!(!plan.contains("CORRELATED LIST SUBQUERY"), "{plan}");
    }
}

/// `e.TrailerTypes!.Any(w => trailerTypes.Contains(w.Id))`: an item with a
/// trailer-type row of any listed type.
#[tokio::test]
async fn trailer_types_match_the_items_trailer_type_rows() {
    let db = seeded().await;
    for kind in [
        TrailerType::ComingSoonToTheaters,
        TrailerType::ComingSoonToDvd,
    ] {
        sqlx::query(r#"INSERT INTO "BaseItemTrailerTypes" ("Id", "ItemId") VALUES (?1, ?2)"#)
            .bind(kind as i64)
            .bind(guid_to_db(STUB))
            .execute(db.writer())
            .await
            .expect("trailer type");
    }
    let query = |types: &[TrailerType]| InternalItemsQuery {
        trailer_types: types.to_vec(),
        ..InternalItemsQuery::default()
    };
    assert_eq!(
        ids(&db, &query(&[TrailerType::ComingSoonToTheaters])).await,
        set([STUB])
    );
    assert!(ids(&db, &query(&[TrailerType::Archive])).await.is_empty());
    assert_eq!(
        ids(
            &db,
            &query(&[TrailerType::Archive, TrailerType::ComingSoonToDvd])
        )
        .await,
        set([STUB])
    );
}

/// `HasDeadParentId = true`: the item names a parent that no row but the
/// placeholder has. `false` is no filter, as upstream tests only `true`.
#[tokio::test]
async fn dead_parent_ids_name_a_parent_no_real_row_has() {
    let db = seeded().await;
    let (live, placeholder, missing) = (
        Uuid::from_u128(0x3101),
        Uuid::from_u128(0x3102),
        Uuid::from_u128(0x3103),
    );
    let child = |id, parent: &str| BaseItemEntity {
        parent_id: Some(parent.to_owned()),
        ..row(id, BaseItemKind::Movie, None, None)
    };
    // A dangling parent only exists where foreign keys were off when it was
    // written, as in an adopted database.
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(db.writer())
        .await
        .expect("keys off");
    FerrofinItemPersistenceService::new(db.clone())
        .save_items(&[
            child(live, &guid_to_db(CONTINUING)),
            child(placeholder, "00000000-0000-0000-0000-000000000001"),
            child(missing, &guid_to_db(Uuid::from_u128(0x3199))),
        ])
        .await
        .expect("save");
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(db.writer())
        .await
        .expect("keys on");
    let query = |dead| InternalItemsQuery {
        has_dead_parent_id: Some(dead),
        ..InternalItemsQuery::default()
    };
    let dead = translated_all(&db, &query(true)).await;
    assert!(
        dead.contains(&placeholder) && dead.contains(&missing),
        "{dead:?}"
    );
    assert!(!dead.contains(&live));
    assert!(
        ALL.iter().all(|id| !dead.contains(id)),
        "no parent, no match"
    );
    let any = translated_all(&db, &query(false)).await;
    assert!(any.contains(&live) && any.contains(&missing));
}

/// The provider-id maps (`WhereProviderMatch`): an empty value asks for any
/// value of the provider, the others match `ProviderId:ProviderValue`
/// ordinally; the excluding form is `NOT EXISTS`.
#[tokio::test]
async fn provider_id_maps_match_like_where_provider_match() {
    let db = seeded().await;
    for (item, provider, value) in [
        (STUB, "Tmdb", "603"),
        (STUB, "Imdb", "tt0133093"),
        (EMPTY_RATING, "Tmdb", "604"),
    ] {
        sqlx::query(
            r#"INSERT INTO "BaseItemProviders" ("ItemId", "ProviderId", "ProviderValue")
               VALUES (?1, ?2, ?3)"#,
        )
        .bind(guid_to_db(item))
        .bind(provider)
        .bind(value)
        .execute(db.writer())
        .await
        .expect("provider id");
    }
    let one = |pairs: &[(&str, &str)]| -> Option<HashMap<String, String>> {
        Some(
            pairs
                .iter()
                .map(|&(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        )
    };
    let any_of = |map| InternalItemsQuery {
        has_any_provider_id: map,
        ..InternalItemsQuery::default()
    };
    assert_eq!(
        ids(&db, &any_of(one(&[("Tmdb", "603")]))).await,
        set([STUB])
    );
    assert_eq!(
        ids(&db, &any_of(one(&[("Tmdb", "")]))).await,
        set([STUB, EMPTY_RATING])
    );
    assert!(
        ids(&db, &any_of(one(&[("tmdb", "603")]))).await.is_empty(),
        "ordinal"
    );
    assert_eq!(
        ids(&db, &any_of(Some(HashMap::new()))).await,
        set(ALL),
        "an empty map is no filter"
    );
    let many = InternalItemsQuery {
        has_any_provider_ids: Some(HashMap::from([
            ("Tmdb".to_owned(), vec!["604".to_owned(), "999".to_owned()]),
            ("Imdb".to_owned(), Vec::new()),
        ])),
        ..InternalItemsQuery::default()
    };
    assert_eq!(ids(&db, &many).await, set([STUB, EMPTY_RATING]));
    let excluded = InternalItemsQuery {
        exclude_provider_ids: one(&[("Tmdb", "603")]),
        ..InternalItemsQuery::default()
    };
    assert_eq!(ids(&db, &excluded).await, all_but([STUB]));
    let excluded_any = InternalItemsQuery {
        exclude_provider_ids: one(&[("Tmdb", "")]),
        ..InternalItemsQuery::default()
    };
    assert_eq!(
        ids(&db, &excluded_any).await,
        all_but([STUB, EMPTY_RATING]),
        "an empty value excludes any value of the provider"
    );
}

/// `BoxSetLibraryFolders`: a folder id, in its "N" form, anywhere in the
/// item's `Data` (`boxsetFolders.Any(f => e.Data!.Contains(f))`).
#[tokio::test]
async fn box_set_library_folders_match_the_folder_id_in_the_data_blob() {
    let db = seeded().await;
    let (folder, other_folder) = (Uuid::from_u128(0xF01D), Uuid::from_u128(0xF02D));
    let collection = Uuid::from_u128(0x3201);
    FerrofinItemPersistenceService::new(db.clone())
        .save_items(&[row(
            collection,
            BaseItemKind::BoxSet,
            None,
            Some(&format!(
                r#"{{"LibraryFolderIds":["{}"]}}"#,
                folder.simple()
            )),
        )])
        .await
        .expect("save");
    let query = |folders: Vec<Uuid>| InternalItemsQuery {
        box_set_library_folders: folders,
        ..InternalItemsQuery::default()
    };
    assert_eq!(
        translated_all(&db, &query(vec![folder])).await,
        BTreeSet::from([collection])
    );
    assert_eq!(
        translated_all(&db, &query(vec![other_folder, folder])).await,
        BTreeSet::from([collection])
    );
    assert!(
        translated_all(&db, &query(vec![other_folder]))
            .await
            .is_empty()
    );
}

/// The provider match is a seek on the item's own provider rows.
#[tokio::test]
async fn provider_match_plan_seeks_the_items_rows() {
    let db = seeded().await;
    let filter = InternalItemsQuery {
        include_item_types: vec![BaseItemKind::Movie],
        has_any_provider_id: Some(HashMap::from([("Tmdb".to_owned(), "603".to_owned())])),
        ..InternalItemsQuery::default()
    };
    let plan = plan(&db, &filter).await;
    assert!(plan.contains("SEARCH p USING"), "{plan}");
    assert!(plan.contains("(ItemId=?"), "{plan}");
    assert!(!plan.contains("SCAN p "), "{plan}");
    assert!(plan.contains("(Type=?"), "{plan}");
}

/// An extras filter reads the extras once (`MATERIALIZE o`) from
/// `IX_BaseItems_ExtraType_OwnerId`, then seeks each owner's row for its
/// primary and the owners themselves by id, so a typed browse keeps its own
/// index seek.
#[tokio::test]
async fn extras_roll_up_plan_reads_the_extras_once() {
    let db = seeded_extras().await;
    for filter in [
        InternalItemsQuery {
            has_trailer: Some(true),
            ..InternalItemsQuery::default()
        },
        InternalItemsQuery {
            has_special_feature: Some(false),
            ..InternalItemsQuery::default()
        },
    ] {
        let filter = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Movie],
            ..filter
        };
        let plan = plan(&db, &filter).await;
        assert!(plan.contains("SEARCH bi USING INDEX"), "{plan}");
        assert!(plan.contains("(Type=?"), "{plan}");
        assert!(plan.contains("MATERIALIZE direct"), "{plan}");
        assert!(plan.contains("MATERIALIZE o"), "{plan}");
        assert_eq!(
            plan.matches(" x USING COVERING INDEX IX_BaseItems_ExtraType_OwnerId")
                .count(),
            1,
            "{plan}"
        );
        let steps = format!("{plan} |");
        assert!(!steps.contains("SCAN d |"), "owners are a seek: {plan}");
        assert!(!steps.contains("SCAN v |"), "versions are a seek: {plan}");
        assert!(!plan.contains("CORRELATED LIST SUBQUERY"), "{plan}");
    }
}
