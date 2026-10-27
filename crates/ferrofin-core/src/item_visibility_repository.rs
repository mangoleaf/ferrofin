//! Batched storage reads for request-local item visibility evaluation.

use ferrofin_db::{Database, entities::base_items::BaseItemEntity, store::guid_to_db};
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use uuid::Uuid;

use crate::{db_error::db_err, item_type_lookup::stored_type_name};

/// The user's `Permissions` rows, from the cross-request policy cache ([`Database::user_policy`]).
pub(crate) async fn permissions(
    db: &Database,
    user_id: &str,
) -> Result<Vec<(i32, bool)>, ServiceError> {
    Ok(db
        .user_policy(user_id)
        .await
        .map_err(ServiceError::from)?
        .permissions
        .clone())
}

/// The user's `Preferences` rows, from the cross-request policy cache ([`Database::user_policy`]).
pub(crate) async fn preferences(
    db: &Database,
    user_id: &str,
) -> Result<Vec<(i32, String)>, ServiceError> {
    Ok(db
        .user_policy(user_id)
        .await
        .map_err(ServiceError::from)?
        .preferences
        .clone())
}

/// The rows of `ids` and every row reachable from them through `ParentId`, `OwnerId`,
/// `ChannelId`, `SeasonId` and `SeriesId`, in one statement: the visibility walk's
/// hierarchy, which took a read per level (gl-973).
pub(crate) async fn reachable(
    db: &Database,
    ids: &[Uuid],
) -> Result<Vec<BaseItemEntity>, ServiceError> {
    let ids: Vec<String> = ids.iter().map(|id| guid_to_db(*id)).collect();
    sqlx::query_as(REACHABLE_SQL)
        .bind(serde_json::to_string(&ids).map_err(ServiceError::backend_source)?)
        .fetch_all(db.pool())
        .await
        .map_err(db_err)
}

/// [`reachable`]'s statement: the walk is one recursive CTE over the primary key.
pub(crate) const REACHABLE_SQL: &str = r#"
    WITH RECURSIVE reach("Id") AS (
        SELECT value FROM json_each(?1)
        UNION
        SELECT ref.value FROM reach
        JOIN "BaseItems" b ON b."Id" = reach."Id"
        CROSS JOIN json_each(json_array(b."ParentId", b."OwnerId", b."ChannelId", b."SeasonId", b."SeriesId")) ref
        WHERE ref.value IS NOT NULL
    )
    SELECT * FROM "BaseItems" WHERE "Id" IN (SELECT "Id" FROM reach)"#;

pub(crate) async fn libraries(db: &Database) -> Result<Vec<BaseItemEntity>, ServiceError> {
    sqlx::query_as(r#"SELECT * FROM "BaseItems" WHERE "Type" = ?"#)
        .bind(stored_type_name(BaseItemKind::CollectionFolder).unwrap_or_default())
        .fetch_all(db.pool())
        .await
        .map_err(db_err)
}

pub(crate) async fn linked_children(
    db: &Database,
    parent_ids: &[Uuid],
) -> Result<Vec<(String, String)>, ServiceError> {
    let mut pairs = Vec::new();
    for chunk in parent_ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
        let mut query = sqlx::QueryBuilder::new(
            r#"SELECT "ParentId", "ChildId" FROM "LinkedChildren" WHERE "ParentId" IN ("#,
        );
        let mut separated = query.separated(",");
        for id in chunk {
            separated.push_bind(guid_to_db(*id));
        }
        separated.push_unseparated(")");
        pairs.extend(
            query
                .build_query_as()
                .fetch_all(db.pool())
                .await
                .map_err(db_err)?,
        );
    }
    Ok(pairs)
}

/// The official ratings of a container's linked children that exist, in link
/// order (`SortOrder`) — only the column `UpdateRatingToItems` reads.
pub(crate) async fn member_ratings(
    db: &Database,
    parent_id: Uuid,
) -> Result<Vec<Option<String>>, ServiceError> {
    sqlx::query_scalar(
        r#"SELECT bi."OfficialRating" FROM "LinkedChildren" lc
           JOIN "BaseItems" bi ON bi."Id" = lc."ChildId"
           WHERE lc."ParentId" = ?1 ORDER BY lc."SortOrder""#,
    )
    .bind(guid_to_db(parent_id))
    .fetch_all(db.pool())
    .await
    .map_err(db_err)
}

pub(crate) async fn playlist_access(
    db: &Database,
    user_id: &str,
    playlist_ids: &[Uuid],
) -> Result<Vec<(String, Option<String>, bool, bool)>, ServiceError> {
    let mut access = Vec::new();
    for chunk in playlist_ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
        let mut query = sqlx::QueryBuilder::new(
            r#"SELECT p."PlaylistId", p."OwnerUserId", p."OpenAccess",
            EXISTS(SELECT 1 FROM "FerrofinPlaylistShares" s WHERE s."PlaylistId" = p."PlaylistId" AND s."UserId" = "#,
        );
        query
            .push_bind(user_id)
            .push(r#") FROM "FerrofinPlaylists" p WHERE p."PlaylistId" IN ("#);
        let mut separated = query.separated(",");
        for id in chunk {
            separated.push_bind(guid_to_db(*id));
        }
        separated.push_unseparated(")");
        access.extend(
            query
                .build_query_as()
                .fetch_all(db.pool())
                .await
                .map_err(db_err)?,
        );
    }
    Ok(access)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{seed_child_item, seed_item, test_db};

    /// One read returns the whole chain above an item, and reaches every row through the
    /// primary key: no scan of `BaseItems` (`EXPLAIN QUERY PLAN`).
    #[tokio::test]
    async fn reachable_returns_the_whole_chain_by_primary_key() {
        let db = test_db().await;
        let [root, library, folder, movie, other] =
            [0x101, 0x102, 0x103, 0x104, 0x105].map(Uuid::from_u128);
        seed_item(&db, root, BaseItemKind::AggregateFolder).await;
        seed_child_item(
            &db,
            library,
            BaseItemKind::CollectionFolder,
            "Library",
            root,
        )
        .await;
        seed_child_item(&db, folder, BaseItemKind::Folder, "Folder", library).await;
        seed_child_item(&db, movie, BaseItemKind::Movie, "Movie", folder).await;
        seed_child_item(&db, other, BaseItemKind::Movie, "Other", folder).await;
        let mut found: Vec<Uuid> = reachable(&db, &[movie])
            .await
            .unwrap()
            .iter()
            .filter_map(|row| Uuid::parse_str(&row.id).ok())
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![root, library, folder, movie],
            "the chain, not the sibling"
        );

        let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "EXPLAIN QUERY PLAN {REACHABLE_SQL}"
        )))
        .bind(r#"["x"]"#)
        .fetch_all(db.pool())
        .await
        .unwrap();
        let steps: Vec<&str> = plan.iter().map(|row| row.3.as_str()).collect();
        assert!(
            !steps
                .iter()
                .any(|s| s.starts_with("SCAN b") || s.starts_with("SCAN BaseItems")),
            "{steps:?}"
        );
        assert!(
            steps
                .iter()
                .filter(|s| s.contains("USING INDEX sqlite_autoindex_BaseItems_1 (Id=?)"))
                .count()
                >= 2,
            "the walk and the final read seek the primary key: {steps:?}"
        );
    }
}
