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

/// The library set's rows, in one read: the `CollectionFolder` rows, then the
/// `UserView` and playlists-folder rows an unscoped query's scope lists beside
/// them (`ferrofin_db::row_cache::LibrarySet::views`).
pub(crate) async fn libraries(
    db: &Database,
) -> Result<(Vec<BaseItemEntity>, Vec<BaseItemEntity>), ServiceError> {
    let library = stored_type_name(BaseItemKind::CollectionFolder).unwrap_or_default();
    let rows: Vec<BaseItemEntity> = sqlx::query_as(LIBRARIES_SQL)
        .bind(library)
        .bind(stored_type_name(BaseItemKind::UserView).unwrap_or_default())
        .bind(stored_type_name(BaseItemKind::PlaylistsFolder).unwrap_or_default())
        .bind(stored_type_name(BaseItemKind::ManualPlaylistsFolder).unwrap_or_default())
        .fetch_all(db.pool())
        .await
        .map_err(db_err)?;
    Ok(rows.into_iter().partition(|row| row.type_ == library))
}

/// [`libraries`]' statement.
const LIBRARIES_SQL: &str = r#"SELECT * FROM "BaseItems" WHERE "Type" IN (?1, ?2, ?3, ?4)"#;

/// The `Playlist` rows under `top_parents`, or every one when none are named:
/// the rows a listing's SQL can reach, from the `(Type, TopParentId)` index.
pub(crate) async fn playlists(
    db: &Database,
    top_parents: &[Uuid],
) -> Result<Vec<BaseItemEntity>, ServiceError> {
    playlists_statement(top_parents)
        .build_query_as()
        .fetch_all(db.pool())
        .await
        .map_err(db_err)
}

/// [`playlists`]' statement.
fn playlists_statement(top_parents: &[Uuid]) -> sqlx::QueryBuilder<sqlx::Sqlite> {
    let mut query = sqlx::QueryBuilder::new(r#"SELECT * FROM "BaseItems" WHERE "Type" = "#);
    query.push_bind(stored_type_name(BaseItemKind::Playlist).unwrap_or_default());
    if !top_parents.is_empty() {
        query.push(r#" AND "TopParentId" IN ("#);
        let mut separated = query.separated(",");
        for id in ferrofin_db::padded_in_list(top_parents) {
            separated.push_bind(guid_to_db(*id));
        }
        separated.push_unseparated(")");
    }
    query
}

pub(crate) async fn linked_children(
    db: &Database,
    parent_ids: &[Uuid],
) -> Result<Vec<(String, String)>, ServiceError> {
    let mut pairs = Vec::new();
    for chunk in ferrofin_db::padded_in_chunks(parent_ids) {
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
    for chunk in ferrofin_db::padded_in_chunks(playlist_ids) {
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

    /// The playlists a listing can reach come from a `(Type, TopParentId)` seek, or a
    /// `Type` seek without a scope: this read runs on mixed listings, and its cost must
    /// follow the playlists, never the library.
    #[tokio::test]
    async fn the_playlist_candidates_seek_their_type() {
        use sqlx::{Execute, Row};
        let db = test_db().await;
        // No scope, and every length a padded scope list takes (gl-1243).
        let scopes = std::iter::once(0)
            .chain((1..=ferrofin_db::PADDED_LIST_MAX.ilog2()).map(|k| 1_u128 << k));
        for top_parents in scopes.map(|n| (1..=n).map(Uuid::from_u128).collect::<Vec<_>>()) {
            let mut builder = playlists_statement(&top_parents);
            let mut query = builder.build();
            let args = query.take_arguments().expect("arguments").expect("binds");
            let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
            let plan: Vec<String> = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                .fetch_all(db.pool())
                .await
                .expect("explain query plan")
                .iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect();
            let wanted = if top_parents.is_empty() {
                "(Type=?)"
            } else {
                "(Type=? AND TopParentId=?)"
            };
            assert!(
                plan.iter()
                    .any(|step| step.starts_with("SEARCH") && step.contains(wanted)),
                "{plan:?}"
            );
            assert!(
                !plan.iter().any(|step| step.starts_with("SCAN")),
                "{plan:?}"
            );
        }
    }

    /// The library set is read in one `Type` seek, never a scan of `BaseItems`, and its rows
    /// are split into the libraries and the views and playlists folder.
    #[tokio::test]
    async fn the_library_set_is_one_type_seek() {
        let db = test_db().await;
        let [library, view, playlists, movie] = [0x111, 0x112, 0x113, 0x114].map(Uuid::from_u128);
        seed_item(&db, library, BaseItemKind::CollectionFolder).await;
        seed_item(&db, view, BaseItemKind::UserView).await;
        seed_item(&db, playlists, BaseItemKind::PlaylistsFolder).await;
        seed_item(&db, movie, BaseItemKind::Movie).await;
        let (rows, views) = libraries(&db).await.unwrap();
        let ids = |rows: &[BaseItemEntity]| -> Vec<Uuid> {
            let mut ids: Vec<Uuid> = rows
                .iter()
                .filter_map(|r| Uuid::parse_str(&r.id).ok())
                .collect();
            ids.sort();
            ids
        };
        assert_eq!(ids(&rows), [library]);
        assert_eq!(ids(&views), [view, playlists]);

        let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "EXPLAIN QUERY PLAN {LIBRARIES_SQL}"
        )))
        .bind("a")
        .bind("b")
        .bind("c")
        .bind("d")
        .fetch_all(db.pool())
        .await
        .unwrap();
        let steps: Vec<&str> = plan.iter().map(|row| row.3.as_str()).collect();
        assert!(
            steps
                .iter()
                .any(|s| s.starts_with("SEARCH") && s.contains("(Type=?)")),
            "{steps:?}"
        );
        assert!(!steps.iter().any(|s| s.starts_with("SCAN")), "{steps:?}");
    }

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
