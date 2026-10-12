//! Batched equivalent of Video/Series.GetExtraOwnerIds (Jellyfin 12.1).
use crate::db_error::db_err;
use ferrofin_db::{Database, entities::base_items::BaseItemEntity, store::guid_to_db};
use ferrofin_traits::error::ServiceError;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

const VIDEO_TYPES: &str = "'MediaBrowser.Controller.Entities.Video','MediaBrowser.Controller.Entities.Movies.Movie','MediaBrowser.Controller.Entities.TV.Episode','MediaBrowser.Controller.Entities.MusicVideo','MediaBrowser.Controller.Entities.Trailer'";

pub(crate) async fn get_extra_owner_ids_batch(
    db: &Database,
    items: &[BaseItemEntity],
    grouped_series: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Uuid>>, ServiceError> {
    let mut result: HashMap<Uuid, Vec<Uuid>> = items
        .iter()
        .filter_map(|item| {
            let id = Uuid::parse_str(&item.id).ok()?;
            Some((id, vec![id]))
        })
        .collect();
    let videos: Vec<Uuid> = items
        .iter()
        .filter(|item| {
            matches!(
                crate::item_type_lookup::kind_from_type_name(&item.type_),
                Some(
                    ferrofin_model::data::BaseItemKind::Movie
                        | ferrofin_model::data::BaseItemKind::Video
                        | ferrofin_model::data::BaseItemKind::Episode
                        | ferrofin_model::data::BaseItemKind::MusicVideo
                        | ferrofin_model::data::BaseItemKind::Trailer
                )
            )
        })
        .filter_map(|item| Uuid::parse_str(&item.id).ok())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    for chunk in videos.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
        let sql = video_owners_sql(chunk.len());
        let mut query = sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(sql));
        for id in chunk {
            query = query.bind(guid_to_db(*id));
        }
        for (item, owner) in query.fetch_all(db.pool()).await.map_err(db_err)? {
            if let (Ok(item), Ok(owner)) = (Uuid::parse_str(&item), Uuid::parse_str(&owner)) {
                result.entry(item).or_default().push(owner);
            }
        }
    }
    let grouped: HashSet<Uuid> = grouped_series.iter().copied().collect();
    let mut series_by_key: HashMap<&str, Vec<Uuid>> = HashMap::new();
    for item in items {
        if let Ok(id) = Uuid::parse_str(&item.id)
            && grouped.contains(&id)
            && let Some(key) = item
                .presentation_unique_key
                .as_deref()
                .filter(|key| !key.is_empty())
        {
            series_by_key.entry(key).or_default().push(id);
        }
    }
    let keys: Vec<&str> = series_by_key.keys().copied().collect();
    for chunk in keys.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
        let sql = format!(
            "SELECT PresentationUniqueKey, Id FROM BaseItems WHERE PresentationUniqueKey IN ({}) AND Type='MediaBrowser.Controller.Entities.TV.Series'",
            vec!["?"; chunk.len()].join(",")
        );
        let mut query = sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(sql));
        for key in chunk {
            query = query.bind(key);
        }
        for (key, owner) in query.fetch_all(db.pool()).await.map_err(db_err)? {
            if let Ok(owner) = Uuid::parse_str(&owner)
                && let Some(ids) = series_by_key.get(key.as_str())
            {
                for id in ids {
                    result.entry(*id).or_default().push(owner);
                }
            }
        }
    }
    for owners in result.values_mut() {
        owners.sort_unstable();
        owners.dedup();
    }
    Ok(result)
}

fn video_owners_sql(count: usize) -> String {
    // Two bounded link steps, matching GetAllItemsForMediaSources: this +
    // primary, their linked versions, then local versions of that group.
    // UNION deduplicates cycles and overlapping links without recursive walks.
    format!(
        r"WITH requested(Id) AS (VALUES {}),
        roots(ItemId, OwnerId) AS (
            SELECT Id, Id FROM requested
            UNION SELECT r.Id, p.Id FROM requested r
                CROSS JOIN BaseItems v ON v.Id=r.Id
                CROSS JOIN BaseItems p ON p.Id=v.PrimaryVersionId AND p.Type IN ({VIDEO_TYPES})
        ), grouped(ItemId, OwnerId) AS (
            SELECT ItemId, OwnerId FROM roots
            UNION SELECT r.ItemId, c.Id FROM roots r
                CROSS JOIN LinkedChildren l ON l.ParentId=r.OwnerId AND l.ChildType=3
                CROSS JOIN BaseItems c ON c.Id=l.ChildId AND c.Type IN ({VIDEO_TYPES})
        )
        SELECT ItemId, OwnerId FROM grouped
        UNION SELECT g.ItemId, c.Id FROM grouped g
            CROSS JOIN LinkedChildren l ON l.ParentId=g.OwnerId AND l.ChildType=2
            CROSS JOIN BaseItems c ON c.Id=l.ChildId",
        vec!["(?)"; count].join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrofin_traits::persistence::ItemPersistenceService;

    async fn fixture() -> (Database, crate::FerrofinItemPersistenceService) {
        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let store = crate::FerrofinItemPersistenceService::new(db.clone());
        (db, store)
    }

    fn item(id: u128, kind: &str) -> BaseItemEntity {
        BaseItemEntity {
            id: guid_to_db(Uuid::from_u128(id)),
            type_: kind.into(),
            name: Some(format!("Item {id}")),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn video_groups_follow_linked_then_local_versions_and_deduplicate_cycles() {
        let (db, store) = fixture().await;
        let movie = "MediaBrowser.Controller.Entities.Movies.Movie";
        let mut rows: Vec<_> = (10..17).map(|id| item(id, movie)).collect();
        rows[1].primary_version_id = Some(rows[0].id.clone());
        rows[6].type_ = "MediaBrowser.Controller.Entities.Audio".into();
        store.save_items(&rows).await.unwrap();
        for (parent, child, kind) in [
            (10, 11, 3),
            (11, 10, 3),
            (10, 12, 3),
            (12, 13, 2),
            (10, 14, 2),
            (13, 15, 3),
            (10, 16, 3),
        ] {
            sqlx::query("INSERT INTO LinkedChildren (ParentId, ChildId, ChildType, SortOrder) VALUES (?,?,?,?)")
                .bind(guid_to_db(Uuid::from_u128(parent))).bind(guid_to_db(Uuid::from_u128(child))).bind(kind)
                .bind(i64::try_from(child).unwrap())
                .execute(db.pool()).await.unwrap();
        }
        let got = get_extra_owner_ids_batch(&db, &rows[..2], &[])
            .await
            .unwrap();
        for id in [10, 11] {
            assert_eq!(
                got[&Uuid::from_u128(id)],
                (10..15).map(Uuid::from_u128).collect::<Vec<_>>()
            );
        }
        // A primary pointer by itself doesn't invent a forward version link.
        sqlx::query("DELETE FROM LinkedChildren WHERE ChildType=3")
            .execute(db.pool())
            .await
            .unwrap();
        let got = get_extra_owner_ids_batch(&db, &rows[..2], &[])
            .await
            .unwrap();
        assert_eq!(
            got[&Uuid::from_u128(10)],
            vec![Uuid::from_u128(10), Uuid::from_u128(14)]
        );
        assert_eq!(
            got[&Uuid::from_u128(11)],
            vec![
                Uuid::from_u128(10),
                Uuid::from_u128(11),
                Uuid::from_u128(14)
            ]
        );
    }

    #[tokio::test]
    async fn series_groups_require_enabled_options_and_preserve_unrelated_items() {
        let (db, store) = fixture().await;
        let rows = [
            item(20, "MediaBrowser.Controller.Entities.TV.Series"),
            item(21, "MediaBrowser.Controller.Entities.TV.Series"),
            item(22, "MediaBrowser.Controller.Entities.TV.Season"),
        ];
        store.save_items(&rows).await.unwrap();
        sqlx::query("UPDATE BaseItems SET PresentationUniqueKey='same' WHERE Id IN (?,?,?)")
            .bind(&rows[0].id)
            .bind(&rows[1].id)
            .bind(&rows[2].id)
            .execute(db.pool())
            .await
            .unwrap();
        let mut rows = rows;
        for row in &mut rows {
            row.presentation_unique_key = Some("same".into());
        }
        let got = get_extra_owner_ids_batch(&db, &rows, &[Uuid::from_u128(20)])
            .await
            .unwrap();
        assert_eq!(
            got[&Uuid::from_u128(20)],
            vec![Uuid::from_u128(20), Uuid::from_u128(21)]
        );
        assert_eq!(got[&Uuid::from_u128(21)], vec![Uuid::from_u128(21)]);
        assert_eq!(got[&Uuid::from_u128(22)], vec![Uuid::from_u128(22)]);
    }

    #[tokio::test]
    async fn empty_large_and_missing_inputs_and_query_plan() {
        let (db, store) = fixture().await;
        assert!(
            get_extra_owner_ids_batch(&db, &[], &[])
                .await
                .unwrap()
                .is_empty()
        );
        let rows: Vec<_> = (100..1105)
            .map(|id| item(id, "MediaBrowser.Controller.Entities.Video"))
            .collect();
        store.save_items(&rows).await.unwrap();
        let got = get_extra_owner_ids_batch(&db, &rows, &[]).await.unwrap();
        assert_eq!(got.len(), rows.len());
        assert!(got.iter().all(|(id, owners)| owners == &vec![*id]));
        let sql = format!("EXPLAIN QUERY PLAN {}", video_owners_sql(1));
        let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(&rows[0].id)
            .fetch_all(db.pool())
            .await
            .unwrap();
        assert!(
            !plan.iter().any(|(_, _, _, line)| line.contains("SCAN l")
                || line.contains("SCAN c")
                || line.contains("SCAN p")
                || line.contains("SCAN v")),
            "{plan:?}"
        );
        let missing = item(2000, "MediaBrowser.Controller.Entities.Video");
        assert_eq!(
            get_extra_owner_ids_batch(&db, &[missing], &[])
                .await
                .unwrap()[&Uuid::from_u128(2000)],
            vec![Uuid::from_u128(2000)]
        );
        db.pool().close().await;
        assert!(get_extra_owner_ids_batch(&db, &rows, &[]).await.is_err());
    }
}
