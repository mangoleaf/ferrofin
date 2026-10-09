//! Batched storage reads for request-local item visibility evaluation.

use ferrofin_db::{Database, entities::base_items::BaseItemEntity, store::guid_to_db};
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use uuid::Uuid;

use crate::{db_error::db_err, item_type_lookup::stored_type_name};

pub(crate) async fn permissions(
    db: &Database,
    user_id: &str,
) -> Result<Vec<(i32, bool)>, ServiceError> {
    sqlx::query_as(r#"SELECT "Kind", "Value" FROM "Permissions" WHERE "UserId" = ?"#)
        .bind(user_id)
        .fetch_all(db.pool())
        .await
        .map_err(db_err)
}

pub(crate) async fn preferences(
    db: &Database,
    user_id: &str,
) -> Result<Vec<(i32, String)>, ServiceError> {
    sqlx::query_as(r#"SELECT "Kind", "Value" FROM "Preferences" WHERE "UserId" = ?"#)
        .bind(user_id)
        .fetch_all(db.pool())
        .await
        .map_err(db_err)
}

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
