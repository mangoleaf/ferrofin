//! A row whose stored `Type` names no kind, such as a removed plugin's item:
//! v12.4 `BaseItemMapper.DeserializeBaseItem` logs it and returns null, and
//! every reader drops it (jellyfin/jellyfin#15956), while `GetItemIdsList` and
//! the counts, which deserialize nothing, still include it. The cases follow
//! upstream's `BaseItemRepositoryTests.DeserializeBaseItem_WithUnknownType_*`.

use super::*;
use ferrofin_model::dto::SortOrder;
use ferrofin_model::live_tv::ItemSortBy;

use crate::test_support::{
    CapturedLogs, fetch_item, item_repository_over, link_alternate_version, save_item,
    seed_child_item, seed_named_item, test_db,
};

const LIBRARY: Uuid = Uuid::from_u128(0x0425_0001);
const KEPT: Uuid = Uuid::from_u128(0x0425_0002);
const GONE: Uuid = Uuid::from_u128(0x0425_0003);
const INSIDE: Uuid = Uuid::from_u128(0x0425_0004);
const LATER: Uuid = Uuid::from_u128(0x0425_0005);

/// A library holding two movies and, between them by name, a folder whose
/// `Type` is rewritten to `type_name`, with a movie inside that folder.
async fn seeded(type_name: &str) -> Database {
    let db = test_db().await;
    seed_named_item(&db, LIBRARY, BaseItemKind::CollectionFolder, "Library").await;
    seed_child_item(&db, KEPT, BaseItemKind::Movie, "Kept", LIBRARY).await;
    seed_child_item(&db, GONE, BaseItemKind::Folder, "Lost", LIBRARY).await;
    seed_child_item(&db, LATER, BaseItemKind::Movie, "Next", LIBRARY).await;
    seed_child_item(&db, INSIDE, BaseItemKind::Movie, "Inside", GONE).await;
    // `ItemSortBy::Name` sorts by `CleanName`: set it, so the order is fixed.
    for (id, clean) in [(KEPT, "kept"), (GONE, "lost"), (LATER, "next")] {
        let mut row = fetch_item(&db, id).await;
        row.clean_name = Some(clean.to_owned());
        if id == GONE {
            row.type_ = type_name.to_owned();
        }
        save_item(&db, &row).await;
    }
    db
}

fn ids(rows: &[BaseItemEntity]) -> Vec<Uuid> {
    rows.iter()
        .map(|row| Uuid::parse_str(&row.id).expect("id"))
        .collect()
}

#[tokio::test]
async fn an_unknown_type_reads_as_absent_and_its_row_is_kept() {
    let db = seeded("Jellyfin.Plugin.Gone.Item").await;
    let repository = item_repository_over(db.clone());
    let children = InternalItemsQuery {
        parent_id: LIBRARY,
        order_by: vec![(ItemSortBy::Name, SortOrder::Ascending)],
        enable_total_record_count: true,
        ..InternalItemsQuery::default()
    };
    let page = |start_index, limit| InternalItemsQuery {
        start_index,
        limit,
        ..children.clone()
    };

    assert!(
        repository
            .retrieve_item(GONE)
            .await
            .expect("retrieve")
            .is_none()
    );
    assert_eq!(
        ids(&repository
            .retrieve_items(&[KEPT, GONE])
            .await
            .expect("retrieve")),
        [KEPT]
    );
    // Ids are not deserialized (`GetItemIdsList`), so the unknown row is listed.
    let order = repository.get_item_ids(&children).await.expect("ids");
    assert_eq!(order, [KEPT, GONE, LATER]);
    let known: Vec<Uuid> = order.iter().copied().filter(|id| *id != GONE).collect();
    // Unpaged, `GetItems` answers from `GetItemList`: the total is the list's.
    let all = repository
        .get_items(&page(None, None))
        .await
        .expect("items");
    assert_eq!(ids(&all.items), known);
    assert_eq!(all.total_record_count, 2);
    // Paged, the count runs in SQL before any row is deserialized, so it holds
    // the unknown row, and a page the skip shortened is not taken for the last.
    let at = order.iter().position(|id| *id == GONE).expect("listed");
    let from = at.saturating_sub(1);
    let pair = page(Some(i32::try_from(from).expect("index")), Some(2));
    let shortened = repository.get_items(&pair).await.expect("items");
    let expected: Vec<Uuid> = order[from..from + 2]
        .iter()
        .copied()
        .filter(|id| *id != GONE)
        .collect();
    assert_eq!(ids(&shortened.items), expected);
    assert_eq!(shortened.total_record_count, 3);
    let short = repository
        .get_items(&page(None, Some(10)))
        .await
        .expect("items");
    assert_eq!(ids(&short.items), known);
    assert_eq!(short.total_record_count, 3);
    // A short page at an offset: the total is the offset plus what SQL read.
    let tail = repository
        .get_items(&page(Some(1), Some(10)))
        .await
        .expect("items");
    let after_first: Vec<Uuid> = order[1..]
        .iter()
        .copied()
        .filter(|id| *id != GONE)
        .collect();
    assert_eq!(ids(&tail.items), after_first);
    assert_eq!(tail.total_record_count, 3);
    // A page of the unknown row alone, and one past the end, still count.
    let alone = page(Some(i32::try_from(at).expect("index")), Some(1));
    let alone = repository.get_items(&alone).await.expect("items");
    assert_eq!(ids(&alone.items), Vec::<Uuid>::new());
    assert_eq!(alone.total_record_count, 3);
    let past = repository
        .get_items(&page(Some(5), Some(10)))
        .await
        .expect("items");
    assert_eq!(ids(&past.items), Vec::<Uuid>::new());
    assert_eq!(past.total_record_count, 3);
    // An `AdjacentTo` window is paged, then the unknown row dropped.
    let middle = order[1];
    let window: Vec<Uuid> = order.iter().copied().filter(|id| *id != GONE).collect();
    let adjacent = InternalItemsQuery {
        adjacent_to: Some(middle),
        ..children.clone()
    };
    let rows = repository.get_item_list(&adjacent).await.expect("window");
    assert_eq!(ids(&rows), window);
    // `GetParents()` ends where `GetParent()` cannot read the unknown folder.
    let chain = repository.get_ancestor_chain(INSIDE).await.expect("chain");
    assert_eq!(chain.map(|rows| ids(&rows)), Some(Vec::new()));
    assert!(repository.item_exists(GONE).await.expect("exists"));

    // The version reads drop it too.
    link_alternate_version(&db, GONE, KEPT).await;
    link_alternate_version(&db, LATER, KEPT).await;
    let versions = repository
        .get_items_by_primary_version_batch(&[KEPT])
        .await
        .expect("versions");
    assert_eq!(ids(&versions[&KEPT]), [LATER]);
    let flagged = repository
        .get_items_by_primary_version_batch_flagged(&[KEPT])
        .await
        .expect("versions");
    let flagged: Vec<BaseItemEntity> = flagged[&KEPT].iter().map(|(row, _)| row.clone()).collect();
    assert_eq!(ids(&flagged), [LATER]);
}

#[tokio::test]
async fn an_unknown_type_is_warned_about_once_then_logged_at_debug() {
    // A type no other test uses: the warned set lives for the process.
    let db = seeded("Jellyfin.Plugin.Gone.Logged").await;
    let repository = item_repository_over(db.clone());
    let logs = CapturedLogs::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish(),
    );

    for _ in 0..2 {
        assert!(
            repository
                .retrieve_item(GONE)
                .await
                .expect("retrieve")
                .is_none()
        );
    }

    let text = logs.text();
    let skipped: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("skipping item with unknown type"))
        .collect();
    assert_eq!(skipped.len(), 2, "{text}");
    assert!(skipped[0].contains("WARN"), "{text}");
    assert!(skipped[0].contains("removed plugin"), "{text}");
    assert!(
        skipped[0].to_lowercase().contains(&GONE.to_string()),
        "{text}"
    );
    assert!(skipped[0].contains("Jellyfin.Plugin.Gone.Logged"), "{text}");
    assert!(skipped[1].contains("DEBUG"), "{text}");
}
