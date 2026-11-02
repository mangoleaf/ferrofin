//! The inherited-tag filters a user's blocked and allowed tags turn into
//! (`ApplyParentalRestrictions`, v12.2 `BaseItemRepository.QueryBuilding.cs:606-644`).
//! A tag reaches an item four ways (on the item, on its series, on an
//! ancestor and on its library), and each has to keep answering the same now
//! that the ancestor arm is one uncorrelated set (`ItemsBelowTaggedAncestor`)
//! rather than a per-row correlated subquery.
//!
//! The fixture and every `[Fact]` are upstream's
//! `BaseItemRepositoryInheritedTagTests` (v12.2), with the C# expectations as
//! the oracle.

use std::collections::BTreeSet;
use std::sync::Arc;

use ferrofin_core::item_type_lookup::{ItemTypeLookup, stored_type_name};
use ferrofin_core::{FerrofinItemPersistenceService, FerrofinItemRepository};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::enums::ItemValueType;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{ItemPersistenceService as _, ItemRepository as _};
use uuid::Uuid;

const TAG: &str = "Adult";

/// Upstream's fixture: the item ids by role.
struct Fixture {
    db: Database,
    library: Uuid,
    other_library: Uuid,
    /// Tagged on the item itself.
    tagged_series: Uuid,
    /// Inherits the tag from its series, without an ancestor row for it.
    episode_of_tagged_series: Uuid,
    /// Tagged in the middle of its episode's ancestor chain.
    tagged_season: Uuid,
    episode_under_tagged_season: Uuid,
    /// Inherits the tag from the library above it.
    tagged_library_series: Uuid,
    /// Carry the tag nowhere: the control the assertions are read against.
    untagged_series: Uuid,
    untagged_episode: Uuid,
}

fn item(id: Uuid, kind: BaseItemKind, name: &str, series: Option<Uuid>) -> BaseItemEntity {
    BaseItemEntity {
        id: guid_to_db(id),
        type_: stored_type_name(kind).expect("stored type").to_owned(),
        name: Some(name.to_owned()),
        is_folder: kind != BaseItemKind::Episode,
        series_id: series.map(guid_to_db),
        ..BaseItemEntity::default()
    }
}

async fn fixture() -> Fixture {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let f = Fixture {
        db,
        library: Uuid::new_v4(),
        other_library: Uuid::new_v4(),
        tagged_series: Uuid::new_v4(),
        episode_of_tagged_series: Uuid::new_v4(),
        tagged_season: Uuid::new_v4(),
        episode_under_tagged_season: Uuid::new_v4(),
        tagged_library_series: Uuid::new_v4(),
        untagged_series: Uuid::new_v4(),
        untagged_episode: Uuid::new_v4(),
    };
    let persist = FerrofinItemPersistenceService::new(f.db.clone());
    persist
        .save_items(&[
            item(f.library, BaseItemKind::Folder, "Shows", None),
            item(
                f.other_library,
                BaseItemKind::Folder,
                "Blocked library",
                None,
            ),
            item(f.tagged_series, BaseItemKind::Series, "Tagged series", None),
            item(
                f.episode_of_tagged_series,
                BaseItemKind::Episode,
                "Episode of tagged series",
                Some(f.tagged_series),
            ),
            item(f.tagged_season, BaseItemKind::Season, "Tagged season", None),
            item(
                f.episode_under_tagged_season,
                BaseItemKind::Episode,
                "Episode under tagged season",
                None,
            ),
            item(
                f.tagged_library_series,
                BaseItemKind::Series,
                "Series in blocked library",
                None,
            ),
            item(
                f.untagged_series,
                BaseItemKind::Series,
                "Untagged series",
                None,
            ),
            item(
                f.untagged_episode,
                BaseItemKind::Episode,
                "Untagged episode",
                Some(f.untagged_series),
            ),
        ])
        .await
        .expect("save items");

    // AncestorIds is a closure: one row per ancestor, not just the parent. The
    // episode of the tagged series deliberately has none, so the series arm is
    // what has to catch it.
    for (child, ancestors) in [
        (f.tagged_series, vec![f.library]),
        (f.tagged_season, vec![f.library]),
        (
            f.episode_under_tagged_season,
            vec![f.tagged_season, f.library],
        ),
        (f.tagged_library_series, vec![f.other_library]),
        (f.untagged_series, vec![f.library]),
        (f.untagged_episode, vec![f.untagged_series, f.library]),
    ] {
        persist
            .set_ancestors(child, &ancestors)
            .await
            .expect("ancestors");
    }
    for tagged in [f.tagged_series, f.tagged_season, f.other_library] {
        persist
            .save_item_values(tagged, &[(i32::from(ItemValueType::Tags), TAG.to_owned())])
            .await
            .expect("tag");
    }
    f
}

impl Fixture {
    async fn ids(&self, filter: InternalItemsQuery) -> BTreeSet<Uuid> {
        FerrofinItemRepository::new(self.db.clone(), Arc::new(ItemTypeLookup::new()))
            .get_item_ids(&filter)
            .await
            .expect("ids")
            .into_iter()
            .collect()
    }
}

#[tokio::test]
async fn exclude_inherited_tags_drops_every_item_the_tag_reaches() {
    let f = fixture().await;
    let ids = f
        .ids(InternalItemsQuery {
            exclude_inherited_tags: vec![TAG.into()],
            ..Default::default()
        })
        .await;
    // The blocked library carries the tag itself, so it goes with everything under it.
    assert_eq!(
        ids,
        BTreeSet::from([f.library, f.untagged_series, f.untagged_episode])
    );
}

#[tokio::test]
async fn include_inherited_tags_keeps_exactly_the_items_the_tag_reaches() {
    let f = fixture().await;
    let ids = f
        .ids(InternalItemsQuery {
            include_inherited_tags: vec![TAG.into()],
            ..Default::default()
        })
        .await;
    assert_eq!(
        ids,
        BTreeSet::from([
            f.other_library,
            f.tagged_series,
            f.episode_of_tagged_series,
            f.tagged_season,
            f.episode_under_tagged_season,
            f.tagged_library_series,
        ])
    );
}

#[tokio::test]
async fn exclude_inherited_tags_drops_an_item_reached_only_through_an_ancestor() {
    let f = fixture().await;
    let ids = f
        .ids(InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Episode],
            exclude_inherited_tags: vec![TAG.into()],
            ..Default::default()
        })
        .await;
    assert_eq!(ids, BTreeSet::from([f.untagged_episode]));
}

#[tokio::test]
async fn exclude_inherited_tags_with_an_unused_tag_keeps_everything() {
    let f = fixture().await;
    let ids = f
        .ids(InternalItemsQuery {
            exclude_inherited_tags: vec!["Unused".into()],
            ..Default::default()
        })
        .await;
    assert_eq!(ids.len(), 9);
}
