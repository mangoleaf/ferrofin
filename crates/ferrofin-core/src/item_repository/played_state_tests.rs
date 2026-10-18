//! The played state the `IsPlayed` filter and the `IsPlayed`/`IsUnplayed`
//! sort keys read: v12.2 `BuildIsPlayedFilter`, which rolls a folder up from
//! its leaves and a leaf up from its version group.
//!
//! The first two groups transliterate upstream's xUnit oracles (v12.2), fixture
//! for fixture: all of `BaseItemRepositoryPlayedOrderingTests.cs`, and the four
//! `IsPlayed` facts of `BaseItemRepositoryPlayedVersionTests.cs`. Its other two
//! facts test other code and are open work items there:
//! `GetIsPlayed_CountsASeriesWatchedThroughAnEpisodeAlternateVersion`
//! (`ItemRepository::get_is_played`) and
//! `IsResumable_DropsASeriesWhoseLastEpisodeWasPlayedThroughAnAlternateVersion`
//! (the version-group rule in the resumable roll-up). The last group covers
//! what Ferrofin's data adds: a folder's own played row, a series reached
//! through a box set, versions whose primary is in another library, the
//! access-filtered leaf set, and the by-name counts scope.

use super::*;
use crate::test_support::{
    fetch_item, item_repository_over, link_alternate_version, save_item, seed_folder_item,
    seed_item_genre, seed_item_value, seed_library_over, seed_named_item, seed_top_parented_item,
    seed_user_data, seed_user_with_defaults, test_db,
};
use ferrofin_model::dto::SortOrder;
use ferrofin_model::live_tv::ItemSortBy;
use std::collections::HashSet;

/// A second library, for versions whose primary lives elsewhere.
const OTHER_LIBRARY: Uuid = Uuid::from_u128(0x0022_B000_02B0);

/// Items seeded as rows first, then put in one library, then related: the
/// closure, the box-set links, the version links and the user's played rows.
#[derive(Default)]
struct Seed {
    items: Vec<Uuid>,
    ancestors: Vec<(Uuid, Uuid)>,
    links: Vec<(Uuid, Uuid)>,
    versions: Vec<(Uuid, Uuid)>,
    played: Vec<Uuid>,
}

impl Seed {
    async fn folder(&mut self, db: &Database, kind: BaseItemKind, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        seed_folder_item(db, id, kind, name, None).await;
        self.items.push(id);
        id
    }

    async fn leaf(&mut self, db: &Database, kind: BaseItemKind, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        seed_named_item(db, id, kind, name).await;
        self.items.push(id);
        id
    }

    /// Upstream `AddSeries`: the first `played` of `played + unplayed`
    /// episodes carry a played row.
    async fn series(&mut self, db: &Database, name: &str, played: usize, unplayed: usize) -> Uuid {
        let series = self.folder(db, BaseItemKind::Series, name).await;
        for index in 0..played + unplayed {
            let episode = self
                .leaf(db, BaseItemKind::Episode, &format!("{name} {index}"))
                .await;
            self.ancestors.push((episode, series));
            if index < played {
                self.played.push(episode);
            }
        }
        series
    }

    /// Upstream `AddAlternateVersion`: a version of `primary`, played or not.
    async fn version(&mut self, db: &Database, primary: Uuid, kind: BaseItemKind, played: bool) {
        let version = self.leaf(db, kind, "version").await;
        self.versions.push((version, primary));
        if played {
            self.played.push(version);
        }
    }

    /// Through the production write paths, as the test-support seeders do.
    async fn write(self, db: &Database, user_id: Uuid) {
        use ferrofin_traits::persistence::{
            ItemPersistenceService as _, LinkedChildrenService as _,
        };
        seed_library_over(db, &self.items).await;
        let persistence = crate::FerrofinItemPersistenceService::new(db.clone());
        for (item, parent) in self.ancestors {
            persistence
                .set_ancestors(item, &[parent])
                .await
                .expect("ancestor");
        }
        let links = crate::linked_children_service::FerrofinLinkedChildrenService::new(db.clone());
        let manual = i32::from(ferrofin_db::enums::LinkedChildType::Manual);
        for (set, member) in self.links {
            links
                .upsert_linked_child(set, member, manual)
                .await
                .expect("link");
        }
        for (version, primary) in self.versions {
            link_alternate_version(db, version, primary).await;
        }
        for item in self.played {
            seed_user_data(db, user_id, item, true, None).await;
        }
    }
}

/// Rewrites a seeded row through the production upsert, before
/// [`Seed::write`] puts it in the library.
async fn edit(db: &Database, id: Uuid, change: impl FnOnce(&mut BaseItemEntity)) {
    let mut row = fetch_item(db, id).await;
    change(&mut row);
    save_item(db, &row).await;
}

/// A fresh database and the user who queries it.
async fn library() -> (Database, Arc<dyn ItemRepository>, UserEntity, Uuid) {
    let db = test_db().await;
    let user = seed_user_with_defaults(&db, Uuid::new_v4()).await;
    let user_id = Uuid::parse_str(&user.id).expect("user id");
    (db.clone(), item_repository_over(db), user, user_id)
}

fn query(user: &UserEntity, kind: BaseItemKind) -> InternalItemsQuery {
    InternalItemsQuery {
        user: Some(user.clone()),
        include_item_types: vec![kind],
        ..Default::default()
    }
}

async fn ids(repository: &Arc<dyn ItemRepository>, query: &InternalItemsQuery) -> Vec<Uuid> {
    repository
        .get_item_list(query)
        .await
        .expect("list")
        .iter()
        .filter_map(|item| Uuid::parse_str(&item.id).ok())
        .collect()
}

async fn sorted(
    repository: &Arc<dyn ItemRepository>,
    user: &UserEntity,
    kind: BaseItemKind,
    order_by: &[ItemSortBy],
) -> Vec<Uuid> {
    let order_by = order_by
        .iter()
        .map(|by| (*by, SortOrder::Ascending))
        .collect();
    ids(
        repository,
        &InternalItemsQuery {
            order_by,
            ..query(user, kind)
        },
    )
    .await
}

async fn played_set(
    repository: &Arc<dyn ItemRepository>,
    user: &UserEntity,
    kind: BaseItemKind,
    played: bool,
) -> HashSet<Uuid> {
    let filter = InternalItemsQuery {
        is_played: Some(played),
        ..query(user, kind)
    };
    ids(repository, &filter).await.into_iter().collect()
}

/// `BaseItemRepositoryPlayedOrderingTests`' library. Names run A..F so name
/// order interleaves the two groups (spelled out: the save path's sort name
/// drops a leading "A "); box sets reach their movie through `LinkedChildren`.
struct Ordering {
    repository: Arc<dyn ItemRepository>,
    user: UserEntity,
    series: [Uuid; 6],
    watched_set: Uuid,
    unwatched_set: Uuid,
}

impl Ordering {
    async fn new() -> Self {
        let (db, repository, user, user_id) = library().await;
        let mut seed = Seed::default();
        let mut series = [Uuid::nil(); 6];
        for (slot, (name, played, unplayed)) in series.iter_mut().zip([
            ("Alpha watched", 1, 0),
            ("Bravo unwatched", 0, 1),
            ("Charlie partially watched", 1, 1),
            ("Delta unwatched", 0, 1),
            ("Echo unwatched", 0, 1),
            ("Foxtrot watched", 1, 0),
        ]) {
            *slot = seed.series(&db, name, played, unplayed).await;
        }
        let mut box_set = async |name: &str, played: bool| {
            let set = seed.folder(&db, BaseItemKind::BoxSet, name).await;
            let movie = seed
                .leaf(&db, BaseItemKind::Movie, &format!("{name} movie"))
                .await;
            seed.links.push((set, movie));
            if played {
                seed.played.push(movie);
            }
            set
        };
        let watched_set = box_set("Alpha watched set", true).await;
        let unwatched_set = box_set("Bravo unwatched set", false).await;
        seed.write(&db, user_id).await;
        Self {
            repository,
            user,
            series,
            watched_set,
            unwatched_set,
        }
    }

    /// The series in upstream's `_unwatchedSeriesIds`: B, C, D, E.
    fn unwatched(&self) -> HashSet<Uuid> {
        self.series[1..5].iter().copied().collect()
    }
}

#[tokio::test]
async fn is_played_orders_unwatched_series_before_watched_ones() {
    let lib = Ordering::new().await;
    let [
        watched,
        unwatched,
        partial,
        second_unwatched,
        third_unwatched,
        second_watched,
    ] = lib.series;
    let order = sorted(
        &lib.repository,
        &lib.user,
        BaseItemKind::Series,
        &[ItemSortBy::IsPlayed],
    );
    assert_eq!(
        order.await,
        [
            unwatched,
            partial,
            second_unwatched,
            third_unwatched,
            watched,
            second_watched
        ]
    );
}

#[tokio::test]
async fn is_played_counts_a_partially_watched_series_as_unwatched() {
    let lib = Ordering::new().await;
    let [watched, _, partial, ..] = lib.series;
    let order = sorted(
        &lib.repository,
        &lib.user,
        BaseItemKind::Series,
        &[ItemSortBy::IsPlayed],
    )
    .await;
    let at = |id| order.iter().position(|x| *x == id).expect("listed");
    assert!(at(partial) < at(watched));
}

#[tokio::test]
async fn is_unplayed_reverses_the_groups() {
    let lib = Ordering::new().await;
    let [
        watched,
        unwatched,
        partial,
        second_unwatched,
        third_unwatched,
        second_watched,
    ] = lib.series;
    let order = sorted(
        &lib.repository,
        &lib.user,
        BaseItemKind::Series,
        &[ItemSortBy::IsUnplayed],
    );
    assert_eq!(
        order.await,
        [
            watched,
            second_watched,
            unwatched,
            partial,
            second_unwatched,
            third_unwatched
        ]
    );
}

#[tokio::test]
async fn is_played_orders_an_unwatched_box_set_before_a_watched_one() {
    let lib = Ordering::new().await;
    let order = sorted(
        &lib.repository,
        &lib.user,
        BaseItemKind::BoxSet,
        &[ItemSortBy::IsPlayed],
    );
    assert_eq!(order.await, [lib.unwatched_set, lib.watched_set]);
}

#[tokio::test]
async fn is_played_then_random_still_places_every_unwatched_series_first() {
    let lib = Ordering::new().await;
    let by = [ItemSortBy::IsPlayed, ItemSortBy::Random];
    let order = sorted(&lib.repository, &lib.user, BaseItemKind::Series, &by).await;
    assert_eq!(
        order[..4].iter().copied().collect::<HashSet<_>>(),
        lib.unwatched()
    );
}

#[tokio::test]
async fn is_played_then_random_fills_a_page_with_unwatched_series() {
    let lib = Ordering::new().await;
    let page = lib
        .repository
        .get_items(&InternalItemsQuery {
            order_by: vec![
                (ItemSortBy::IsPlayed, SortOrder::Ascending),
                (ItemSortBy::Random, SortOrder::Ascending),
            ],
            limit: Some(4),
            enable_total_record_count: true,
            ..query(&lib.user, BaseItemKind::Series)
        })
        .await
        .expect("page");
    assert_eq!(page.total_record_count, 6);
    let items: HashSet<Uuid> = page
        .items
        .iter()
        .filter_map(|item| Uuid::parse_str(&item.id).ok())
        .collect();
    assert_eq!(items, lib.unwatched());
}

/// `BaseItemRepositoryPlayedVersionTests`' library: movies A..D and series
/// E..H whose played state sits on an alternate version.
struct Versions {
    repository: Arc<dyn ItemRepository>,
    user: UserEntity,
    /// A (played through its alternate), B (played on the primary), C (a
    /// version, neither played), D (no version).
    movies: [Uuid; 4],
    /// E (its episode played through the alternate), F (not played), G (one
    /// episode played, the other through its alternate), H (the other not).
    series: [Uuid; 4],
}

impl Versions {
    async fn new() -> Self {
        let (db, repository, user, user_id) = library().await;
        let mut seed = Seed::default();
        let mut movies = [Uuid::nil(); 4];
        for (slot, (name, played_primary, played_version)) in movies.iter_mut().zip([
            ("A", false, Some(true)),
            ("B", true, Some(false)),
            ("C", false, Some(false)),
            ("D", false, None),
        ]) {
            let movie = seed.leaf(&db, BaseItemKind::Movie, name).await;
            if played_primary {
                seed.played.push(movie);
            }
            if let Some(played) = played_version {
                seed.version(&db, movie, BaseItemKind::Movie, played).await;
            }
            *slot = movie;
        }
        let mut series = [Uuid::nil(); 4];
        for (slot, (name, first_played, version_played)) in series.iter_mut().zip([
            ("E", None, true),
            ("F", None, false),
            ("G", Some(true), true),
            ("H", Some(true), false),
        ]) {
            let show = seed.folder(&db, BaseItemKind::Series, name).await;
            if let Some(played) = first_played {
                let first = seed
                    .leaf(&db, BaseItemKind::Episode, &format!("{name} 1"))
                    .await;
                seed.ancestors.push((first, show));
                if played {
                    seed.played.push(first);
                }
            }
            let episode = seed
                .leaf(&db, BaseItemKind::Episode, &format!("{name} 2"))
                .await;
            seed.ancestors.push((episode, show));
            seed.version(&db, episode, BaseItemKind::Episode, version_played)
                .await;
            *slot = show;
        }
        seed.write(&db, user_id).await;
        Self {
            repository,
            user,
            movies,
            series,
        }
    }
}

#[tokio::test]
async fn is_played_counts_a_movie_played_through_its_alternate_version() {
    let lib = Versions::new().await;
    let [a, b, ..] = lib.movies;
    let played = played_set(&lib.repository, &lib.user, BaseItemKind::Movie, true).await;
    assert_eq!(played, HashSet::from([a, b]));
}

#[tokio::test]
async fn is_unplayed_drops_a_movie_played_through_its_alternate_version() {
    let lib = Versions::new().await;
    let [_, _, c, d] = lib.movies;
    let unplayed = played_set(&lib.repository, &lib.user, BaseItemKind::Movie, false).await;
    assert_eq!(unplayed, HashSet::from([c, d]));
}

#[tokio::test]
async fn is_played_keeps_a_played_primary_whose_alternate_has_no_row_of_its_own() {
    let lib = Versions::new().await;
    let played = played_set(&lib.repository, &lib.user, BaseItemKind::Movie, true).await;
    assert!(played.contains(&lib.movies[1]));
}

#[tokio::test]
async fn is_played_counts_a_series_watched_through_an_episode_alternate_version() {
    let lib = Versions::new().await;
    let [e, f, g, h] = lib.series;
    let played = played_set(&lib.repository, &lib.user, BaseItemKind::Series, true).await;
    assert_eq!(played, HashSet::from([e, g]));
    let unplayed = played_set(&lib.repository, &lib.user, BaseItemKind::Series, false).await;
    assert_eq!(unplayed, HashSet::from([f, h]));
}

/// The sort's leaf arm is the filter's: a movie watched only through its
/// alternate sorts among the played ones.
#[tokio::test]
async fn is_played_sorts_a_movie_played_through_its_alternate_version_as_played() {
    let lib = Versions::new().await;
    let [a, b, c, d] = lib.movies;
    let by = [ItemSortBy::IsPlayed, ItemSortBy::SortName];
    let order = sorted(&lib.repository, &lib.user, BaseItemKind::Movie, &by);
    assert_eq!(order.await, [c, d, a, b]);
}

/// Upstream ignores a folder's own `UserData` row (earlier Ferrofin builds
/// wrote one when a container was marked), counts a series reached through a
/// box set's link, and treats a version whose primary is in another library
/// as a leaf of its own (`ApplyAlternateVersionFiltering`).
#[tokio::test]
async fn a_folder_rolls_up_from_its_leaves_alone() {
    let (db, repository, user, user_id) = library().await;
    let mut seed = Seed::default();
    let watched = seed.series(&db, "Watched", 1, 0).await;
    let partial = seed.series(&db, "Partial", 1, 1).await;
    let done = seed.folder(&db, BaseItemKind::BoxSet, "Done").await;
    let seen_movie = seed.leaf(&db, BaseItemKind::Movie, "Seen").await;
    seed.links.extend([(done, watched), (done, seen_movie)]);
    seed.played.push(seen_movie);
    let through_set = seed.folder(&db, BaseItemKind::BoxSet, "Through set").await;
    seed.links.push((through_set, partial));
    let elsewhere = seed.series(&db, "Elsewhere", 1, 0).await;
    let version = seed.leaf(&db, BaseItemKind::Episode, "Elsewhere 2").await;
    seed.ancestors.push((version, elsewhere));
    seed.write(&db, user_id).await;
    // The version's primary lives in another library.
    let primary = Uuid::new_v4();
    seed_top_parented_item(
        &db,
        primary,
        BaseItemKind::Episode,
        "primary",
        OTHER_LIBRARY,
    )
    .await;
    link_alternate_version(&db, version, primary).await;
    // Stale container rows: played, though leaves below them are not.
    for container in [partial, through_set] {
        seed_user_data(&db, user_id, container, true, None).await;
    }

    let kinds = vec![BaseItemKind::Series, BaseItemKind::BoxSet];
    for (played, expected) in [
        (true, HashSet::from([watched, done])),
        (false, HashSet::from([partial, through_set, elsewhere])),
    ] {
        let filter = InternalItemsQuery {
            is_played: Some(played),
            include_item_types: kinds.clone(),
            ..query(&user, BaseItemKind::Series)
        };
        let got: HashSet<Uuid> = ids(&repository, &filter).await.into_iter().collect();
        assert_eq!(got, expected, "IsPlayed={played}");
    }
    let order = ids(
        &repository,
        &InternalItemsQuery {
            include_item_types: kinds,
            order_by: vec![(ItemSortBy::IsUnplayed, SortOrder::Ascending)],
            ..query(&user, BaseItemKind::Series)
        },
    )
    .await;
    let played_first: HashSet<Uuid> = order[..2].iter().copied().collect();
    assert_eq!(played_first, HashSet::from([watched, done]));
}

/// Only access-filtered countable leaves hold a folder unplayed
/// (`GetAccessFilteredLeafItemsQuery`): a missing (virtual) episode does not,
/// nor does one above the user's parental rating. A folder with no such leaf
/// at all, a series of missing episodes or an empty box set, is played.
#[tokio::test]
async fn only_the_leaves_the_user_can_play_hold_a_folder_unplayed() {
    let (db, repository, user, user_id) = library().await;
    let mut seed = Seed::default();
    let watched = seed.series(&db, "Watched", 1, 1).await;
    let rated = seed.series(&db, "Rated", 1, 1).await;
    let missing = seed.series(&db, "Missing", 0, 1).await;
    let empty_set = seed.folder(&db, BaseItemKind::BoxSet, "Empty set").await;
    // The unplayed episode of each series is the last one seeded below it.
    let unplayed_of = |series: Uuid| {
        seed.ancestors
            .iter()
            .rev()
            .find(|(_, parent)| *parent == series)
            .map(|(episode, _)| *episode)
            .expect("an episode")
    };
    let (watched_missing, rated_adult, missing_only) = (
        unplayed_of(watched),
        unplayed_of(rated),
        unplayed_of(missing),
    );
    for episode in [watched_missing, missing_only] {
        edit(&db, episode, |row| row.is_virtual_item = true).await;
    }
    edit(&db, rated_adult, |row| {
        row.inherited_parental_rating_value = Some(18);
    })
    .await;
    seed.write(&db, user_id).await;

    let mut restricted = user.clone();
    restricted.max_parental_rating_score = Some(12);
    for (viewer, expected) in [
        (&user, HashSet::from([watched, missing, empty_set])),
        (
            &restricted,
            HashSet::from([watched, rated, missing, empty_set]),
        ),
    ] {
        let filter = InternalItemsQuery {
            is_played: Some(true),
            include_item_types: vec![BaseItemKind::Series, BaseItemKind::BoxSet],
            ..query(viewer, BaseItemKind::Series)
        };
        let got: HashSet<Uuid> = ids(&repository, &filter).await.into_iter().collect();
        assert_eq!(
            got, expected,
            "max rating {:?}",
            viewer.max_parental_rating_score
        );
    }
}

/// The resumable roll-up shares the leaf set: a version whose primary is in
/// another library is a leaf of its own there, so a series of such versions,
/// one played and one not, is partially watched (12.1
/// `ApplyAlternateVersionFiltering`).
#[tokio::test]
async fn a_series_of_versions_from_another_library_is_resumable_there() {
    let (db, repository, user, user_id) = library().await;
    let mut seed = Seed::default();
    let series = seed.series(&db, "Elsewhere", 1, 1).await;
    let episodes: Vec<Uuid> = seed.ancestors.iter().map(|(episode, _)| *episode).collect();
    seed.write(&db, user_id).await;
    for episode in episodes {
        let primary = Uuid::new_v4();
        seed_top_parented_item(
            &db,
            primary,
            BaseItemKind::Episode,
            "primary",
            OTHER_LIBRARY,
        )
        .await;
        link_alternate_version(&db, episode, primary).await;
    }
    let resumable = ids(
        &repository,
        &InternalItemsQuery {
            is_resumable: Some(true),
            ..query(&user, BaseItemKind::Series)
        },
    )
    .await;
    assert_eq!(resumable, [series]);
}

/// The by-name counts scope (`ByName.cs:338-350`, `IsPlayed` on the content
/// through `TranslateQuery`) counts a series by its episodes. Only
/// `IsPlayed=false` lists the genre: the page's `IsPlayed` applies to the
/// genre row itself (`outerQueryFilter`), a leaf with no played row.
#[tokio::test]
async fn by_name_counts_roll_a_series_up_from_its_episodes() {
    let (db, repository, user, user_id) = library().await;
    let mut seed = Seed::default();
    for (name, played, unplayed) in [("Watched", 1, 0), ("Unwatched", 0, 1)] {
        let series = seed.series(&db, name, played, unplayed).await;
        seed_item_genre(&db, series, "Drama").await;
    }
    seed.write(&db, user_id).await;
    let mut filter = InternalItemsQuery {
        user: Some(user),
        is_played: Some(false),
        ..Default::default()
    };
    filter.dto_options.fields = vec![ferrofin_model::querying::ItemFields::ItemCounts];
    let genres = repository.get_genres(&filter).await.expect("genres");
    let drama = genres
        .items
        .iter()
        .find(|genre| genre.item.name.as_deref() == Some("Drama"))
        .expect("the genre is listed");
    assert_eq!(drama.counts.series_count, 1, "only the unwatched series");
}

/// A by-name artist is a leaf: `MusicArtist.IsFolder => !IsAccessedByName`
/// (v12.2 `MusicArtist.cs:30-33`), so the scan stores it with `IsFolder = 0`
/// and the page's `IsPlayed` reads the artist row's own played state. Stored
/// as a folder, it had no leaves below it and always read played.
#[tokio::test]
async fn a_by_name_artist_reads_its_own_played_state() {
    let (db, repository, user, user_id) = library().await;
    let mut seed = Seed::default();
    let song = seed.leaf(&db, BaseItemKind::Audio, "Song").await;
    seed.write(&db, user_id).await;
    seed_item_value(
        &db,
        song,
        ferrofin_db::enums::ItemValueType::Artist,
        "Miles Davis",
    )
    .await;
    let artists = async |played: bool| -> Vec<String> {
        let filter = InternalItemsQuery {
            user: Some(user.clone()),
            is_played: Some(played),
            ..Default::default()
        };
        let page = repository.get_artists(&filter).await.expect("artists");
        page.items
            .iter()
            .map(|artist| artist.item.id.clone())
            .collect()
    };
    let unplayed = artists(false).await;
    assert_eq!(unplayed.len(), 1, "the unplayed artist is listed");
    assert!(artists(true).await.is_empty());

    let artist = Uuid::parse_str(&unplayed[0]).expect("artist id");
    seed_user_data(&db, user_id, artist, true, None).await;
    assert!(artists(false).await.is_empty());
    assert_eq!(artists(true).await, unplayed, "its own played row decides");
}
