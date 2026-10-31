//! The in-memory folder sort computes each row's keys once (gl-1002):
//! `SortNameComparer`/`NameComparer`'s ordinal case-insensitive key and
//! `AiredEpisodeOrderComparer`'s fields, instead of on every comparison. The
//! order it returns must not change, so the comparator it replaced is kept
//! here as the oracle and both are run over every key list a client can send.

use super::*;
use crate::tv_series_manager::aired_order;
use ferrofin_model::dto::SortOrder;
use ferrofin_model::entities_media::AiredEpisodeOrder;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_util::stable_sort::stable_sort_by;

/// `folder_ordinal_compare` as it was before gl-1002, verbatim, so the
/// oracle does not share the key the sort now computes.
fn ordinal_compare_before(left: Option<&str>, right: Option<&str>) -> std::cmp::Ordering {
    match (left, right) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(left), Some(right)) => upper_invariant(left).cmp(&upper_invariant(right)),
    }
}

/// The in-memory sort as it was before its keys were computed once per
/// row (gl-1002): the same comparator, recomputing every key on every
/// comparison. The oracle the keyed sort must match exactly.
fn sorted_per_comparison(
    rows: Vec<BaseItemEntity>,
    order_by: &[(ItemSortBy, SortOrder)],
) -> Vec<BaseItemEntity> {
    type Keyed = (Option<AiredEpisodeOrder>, BaseItemEntity);
    let aired = order_by
        .iter()
        .any(|(by, _)| *by == ItemSortBy::AiredEpisodeOrder);
    let episode = stored_type_name(BaseItemKind::Episode);
    let mut keyed: Vec<Keyed> = rows
        .into_iter()
        .map(|row| {
            let key = (aired && Some(row.type_.as_str()) == episode).then(|| aired_order(&row));
            (key, row)
        })
        .collect();
    let compare = |(a_aired, a): &Keyed, (b_aired, b): &Keyed| {
        for (by, direction) in order_by {
            let ordering = match by {
                ItemSortBy::Name => ordinal_compare_before(a.name.as_deref(), b.name.as_deref()),
                ItemSortBy::SortName => {
                    ordinal_compare_before(Some(&folder_sort_name(a)), Some(&folder_sort_name(b)))
                }
                ItemSortBy::AiredEpisodeOrder => match (a_aired, b_aired) {
                    (Some(a), Some(b)) => a.compare(b),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                },
                _ => continue,
            };
            let ordering = if *direction == SortOrder::Descending {
                ordering.reverse()
            } else {
                ordering
            };
            if !ordering.is_eq() {
                return ordering;
            }
        }
        std::cmp::Ordering::Equal
    };
    if aired {
        stable_sort_by(&mut keyed, compare);
    } else {
        keyed.sort_by(compare);
    }
    keyed.into_iter().map(|(_, row)| row).collect()
}

/// Children that exercise every branch of the keys: case and accents,
/// characters whose invariant upper case differs from their full one,
/// supplementary-plane characters, empty and missing names, missing,
/// empty, stale and forced sort names, seasons keyed by number, episodes
/// and specials in aired order, and rows that tie on every key.
fn children() -> Vec<BaseItemEntity> {
    let kind = |kind: BaseItemKind| stored_type_name(kind).unwrap_or_default().to_owned();
    let named = |id: &str, name: Option<&str>, sort_name: Option<&str>| BaseItemEntity {
        id: id.to_owned(),
        type_: kind(BaseItemKind::Folder),
        is_folder: true,
        name: name.map(str::to_owned),
        sort_name: sort_name.map(str::to_owned),
        ..Default::default()
    };
    let episode = |id: &str, name: &str, season: Option<i64>, number: Option<i64>| BaseItemEntity {
        type_: kind(BaseItemKind::Episode),
        is_folder: false,
        parent_index_number: season,
        index_number: number,
        ..named(id, Some(name), Some(&name.to_lowercase()))
    };
    vec![
        named("lower", Some("alpha"), Some("alpha")),
        named("title", Some("Alpha"), Some("alpha")),
        named("upper", Some("ALPHA"), Some("ALPHA")),
        named("twin-a", Some("Twin"), Some("twin")),
        named("twin-b", Some("Twin"), Some("twin")),
        named("acute", Some("Émile"), Some("émile")),
        named("acute-lower", Some("émile"), Some("Émile")),
        named("plain-e", Some("Eve"), Some("eve")),
        named("diaeresis", Some("Zoë"), Some("zoë")),
        named("zoe", Some("zoe"), Some("zoe")),
        named("sharp-s", Some("straße"), Some("straße")),
        named("double-s", Some("STRASSE"), Some("strasse")),
        named("dotless-i", Some("ılık"), Some("ılık")),
        named("dotted-i", Some("İstanbul"), Some("istanbul")),
        named("final-sigma", Some("λόγος"), Some("λόγος")),
        named("capital-sigma", Some("ΛΌΓΟΣ"), Some("ΛΌΓΟΣ")),
        named("fullwidth", Some("\u{FF21}bc"), Some("\u{FF21}bc")),
        named("fraktur", Some("\u{1D504}lpha"), Some("\u{1D504}lpha")),
        named("emoji", Some("\u{1F600} smile"), Some("\u{1F600} smile")),
        named("empty-name", Some(""), Some("")),
        named("no-name", None, Some("nameless")),
        named("no-name-no-key", None, None),
        named("no-key", Some("Keyless"), None),
        named("empty-key", Some("Blank key"), Some("")),
        BaseItemEntity {
            forced_sort_name: Some("The Spider-Man".to_owned()),
            ..named("forced", Some("Original"), Some("aaa stale"))
        },
        BaseItemEntity {
            forced_sort_name: Some(String::new()),
            ..named("forced-empty", Some("Kept"), Some("kept"))
        },
        BaseItemEntity {
            type_: kind(BaseItemKind::Season),
            index_number: Some(3),
            ..named("season-3", Some("Season 3"), None)
        },
        BaseItemEntity {
            type_: kind(BaseItemKind::Season),
            index_number: Some(12),
            ..named("season-12", Some("Season 12"), None)
        },
        BaseItemEntity {
            type_: kind(BaseItemKind::Movie),
            is_folder: false,
            ..named("movie", Some("alpha"), Some("alpha"))
        },
        episode("s01e02", "Pilot, part 2", Some(1), Some(2)),
        episode("s01e01", "pilot", Some(1), Some(1)),
        episode("s02e01", "Return", Some(2), Some(1)),
        episode("unnumbered", "Lost tape", None, None),
        BaseItemEntity {
            data: Some(r#"{"AirsBeforeSeasonNumber":2}"#.to_owned()),
            ..episode("special", "Special", Some(0), Some(1))
        },
        BaseItemEntity {
            data: Some(r#"{"AirsAfterSeasonNumber":1}"#.to_owned()),
            ..episode("special-after", "Alpha special", Some(0), Some(2))
        },
    ]
}

/// Every key list a client can send to this sort: one key or two in each
/// direction, plus mixed lists of three with keys the sort skips.
fn key_lists() -> Vec<Vec<(ItemSortBy, SortOrder)>> {
    let keys = [
        ItemSortBy::Name,
        ItemSortBy::SortName,
        ItemSortBy::AiredEpisodeOrder,
        ItemSortBy::Default,
        ItemSortBy::Unrecognized(99),
    ];
    let directions = [SortOrder::Ascending, SortOrder::Descending];
    let mut lists = Vec::new();
    for first in keys {
        for first_direction in directions {
            lists.push(vec![(first, first_direction)]);
            for second in keys.into_iter().filter(|key| *key != first) {
                for second_direction in directions {
                    lists.push(vec![(first, first_direction), (second, second_direction)]);
                }
            }
        }
    }
    lists.push(vec![
        (ItemSortBy::SortName, SortOrder::Ascending),
        (ItemSortBy::Default, SortOrder::Descending),
        (ItemSortBy::Name, SortOrder::Descending),
    ]);
    lists.push(vec![
        (ItemSortBy::AiredEpisodeOrder, SortOrder::Descending),
        (ItemSortBy::Name, SortOrder::Ascending),
        (ItemSortBy::SortName, SortOrder::Descending),
    ]);
    lists.push(vec![
        (ItemSortBy::Name, SortOrder::Ascending),
        (ItemSortBy::Name, SortOrder::Descending),
        (ItemSortBy::SortName, SortOrder::Ascending),
    ]);
    lists
}

/// Computing the sort keys once per row (gl-1002) must not move a single
/// row: for every key list and several input orders, the keyed sort
/// returns exactly what the per-comparison comparator returned.
#[tokio::test]
async fn keyed_sort_orders_rows_exactly_as_the_per_comparison_comparator() {
    let repo = FerrofinItemRepository::new(
        crate::test_support::test_db().await,
        Arc::new(crate::item_type_lookup::ItemTypeLookup::new()),
    );
    let ids = |rows: &[BaseItemEntity]| rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
    let forward = children();
    let mut inputs = vec![forward.clone(), forward.iter().rev().cloned().collect()];
    for step in [5, 7, 11] {
        let mut rotated = forward.clone();
        rotated.rotate_left(step);
        inputs.push(rotated);
    }
    for input in inputs {
        for order_by in key_lists() {
            let expected = sorted_per_comparison(input.clone(), &order_by);
            let sorted = repo
                .sort_folder_children(
                    input.clone(),
                    &InternalItemsQuery {
                        order_by: order_by.clone(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(ids(&sorted), ids(&expected), "{order_by:?}");
        }
    }
}
