//! The item filters over an item's own metadata that `GET /Items` binds
//! straight onto the query (v12.2 `ItemsController.GetItems`) and
//! `BaseItemRepository.TranslateQuery` translates:
//! - `SeriesStatuses`: a substring of the `Data` blob (`TranslateQuery.cs:1167-1172`);
//! - `HasOfficialRating` and `IsPlaceHolder`: the item, or an item below it
//!   (`AncestorIds`) or linked to it (`LinkedChildren`), matches
//!   (`FolderAwareFilterExtensions.WhereItemOrDescendantMatches`,
//!   `TranslateQuery.cs:761-768, 1201-1208`);
//! - `MinParentalRating`, `MaxParentalRating` and `HasParentalRating`
//!   (`TranslateQuery.cs:690-759`), with the maximum clamped to the user's own
//!   limit.

use std::collections::BTreeSet;
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
use ferrofin_model::entities::SeriesStatus;
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
    FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()))
        .get_item_ids(query)
        .await
        .expect("query")
        .into_iter()
        .filter(|id| ALL.contains(id))
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

/// `HasParentalRating`: `true` only picks the branch that applies the rating
/// bounds without the unrated-kind clause, so on its own it filters nothing;
/// `false` keeps the unrated items, but only when no other rating rule
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
        user: Some(limited),
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
        user: Some(user.clone()),
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
        user: Some(user.clone()),
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
