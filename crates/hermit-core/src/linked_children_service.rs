//! [`HermitLinkedChildrenService`] — the concrete [`LinkedChildrenService`] over
//! `hermit-db`.
//!
//! Port of `LinkedChildrenService`. Operates on the `LinkedChildren` table (a
//! directed `(ParentId, ChildId, ChildType)` edge, e.g. a playlist entry or an
//! alternate version). The C# service resolves `FindArtists` to deserialized
//! `MusicArtist` domain objects via `IItemQueryHelpers`; the trait here returns
//! the raw [`BaseItemEntity`] artist rows (deserialization is a DTO-layer
//! concern), so no query-helper sibling is needed.
//!
//! `ChildType` is the stored discriminant of `LinkedChildType` (`Manual = 0`,
//! `Shortcut = 1`). The manual-only operations (`get_manual_linked_parent_ids`,
//! `reroute_linked_children`) filter on `ChildType = 0`, matching C#'s
//! `DbLinkedChildType.Manual`.

use std::collections::HashMap;

use async_trait::async_trait;
use hermit_db::Database;
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_model::data::BaseItemKind;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::persistence::LinkedChildrenService;

use crate::db_error::db_err;
use crate::item_type_lookup::stored_type_name;

/// The stored `LinkedChildren.ChildType` discriminant for a manually linked
/// child (C# `LinkedChildType.Manual`).
const MANUAL_CHILD_TYPE: i32 = 0;

/// The concrete linked-children service.
#[derive(Clone)]
pub struct HermitLinkedChildrenService {
    db: Database,
}

impl std::fmt::Debug for HermitLinkedChildrenService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitLinkedChildrenService")
            .finish_non_exhaustive()
    }
}

impl HermitLinkedChildrenService {
    /// Creates a linked-children service over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl LinkedChildrenService for HermitLinkedChildrenService {
    async fn get_linked_children_ids(
        &self,
        parent_id: Uuid,
        child_type: Option<i32>,
    ) -> Result<Vec<Uuid>, ServiceError> {
        let mut sql =
            String::from(r#"SELECT "ChildId" FROM "LinkedChildren" WHERE "ParentId" = ?1"#);
        if child_type.is_some() {
            sql.push_str(r#" AND "ChildType" = ?2"#);
        }
        sql.push_str(r#" ORDER BY "SortOrder""#);
        let mut query = sqlx::query_scalar::<_, String>(&sql).bind(parent_id.to_string());
        if let Some(ct) = child_type {
            query = query.bind(i64::from(ct));
        }
        let ids = query.fetch_all(self.db.pool()).await.map_err(db_err)?;
        Ok(ids.iter().filter_map(|s| Uuid::parse_str(s).ok()).collect())
    }

    async fn find_artists(
        &self,
        artist_names: &[String],
    ) -> Result<HashMap<String, Vec<BaseItemEntity>>, ServiceError> {
        let mut result: HashMap<String, Vec<BaseItemEntity>> = HashMap::new();
        if artist_names.is_empty() {
            return Ok(result);
        }
        let Some(artist_type) = stored_type_name(BaseItemKind::MusicArtist) else {
            return Ok(result);
        };
        let lower_names: Vec<String> = artist_names.iter().map(|n| n.to_lowercase()).collect();

        // All placeholders are anonymous `?`: SQLite forbids mixing numbered
        // (`?1`) and anonymous placeholders in one statement, and this query
        // builds a dynamic `IN (?, …)` list.
        let mut sql =
            String::from(r#"SELECT * FROM "BaseItems" WHERE "Type" = ? AND LOWER("Name") IN ("#);
        for i in 0..lower_names.len() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push('?');
        }
        sql.push(')');
        let mut query = sqlx::query_as::<_, BaseItemEntity>(&sql).bind(artist_type);
        for name in &lower_names {
            query = query.bind(name.clone());
        }
        let rows = query.fetch_all(self.db.pool()).await.map_err(db_err)?;

        // Group the matched artist rows back onto each requested name
        // (case-insensitively), only emitting names that matched at least one row.
        for name in artist_names {
            let lower = name.to_lowercase();
            let matches: Vec<BaseItemEntity> = rows
                .iter()
                .filter(|r| r.name.as_deref().map(str::to_lowercase) == Some(lower.clone()))
                .cloned()
                .collect();
            if !matches.is_empty() {
                result.insert(name.clone(), matches);
            }
        }
        Ok(result)
    }

    async fn get_manual_linked_parent_ids(
        &self,
        child_id: Uuid,
        parent_type: Option<BaseItemKind>,
    ) -> Result<Vec<Uuid>, ServiceError> {
        let parent_type_name = match parent_type {
            Some(kind) => match stored_type_name(kind) {
                Some(name) => Some(name),
                // A kind with no stored type name can have no matching parents.
                None => return Ok(Vec::new()),
            },
            None => None,
        };

        let ids: Vec<String> = if let Some(type_name) = parent_type_name {
            sqlx::query_scalar(
                r#"SELECT DISTINCT lc."ParentId" FROM "LinkedChildren" lc
                   JOIN "BaseItems" bi ON bi."Id" = lc."ParentId"
                   WHERE lc."ChildId" = ?1 AND lc."ChildType" = ?2 AND bi."Type" = ?3"#,
            )
            .bind(child_id.to_string())
            .bind(i64::from(MANUAL_CHILD_TYPE))
            .bind(type_name)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?
        } else {
            sqlx::query_scalar(
                r#"SELECT DISTINCT "ParentId" FROM "LinkedChildren"
                   WHERE "ChildId" = ?1 AND "ChildType" = ?2"#,
            )
            .bind(child_id.to_string())
            .bind(i64::from(MANUAL_CHILD_TYPE))
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?
        };
        Ok(ids.iter().filter_map(|s| Uuid::parse_str(s).ok()).collect())
    }

    async fn reroute_linked_children(
        &self,
        from_child_id: Uuid,
        to_child_id: Uuid,
    ) -> Result<Vec<Uuid>, ServiceError> {
        let mut tx = self.db.pool().begin().await.map_err(db_err)?;

        let affected: Vec<String> = sqlx::query_scalar(
            r#"SELECT DISTINCT "ParentId" FROM "LinkedChildren"
               WHERE "ChildId" = ?1 AND "ChildType" = ?2"#,
        )
        .bind(from_child_id.to_string())
        .bind(i64::from(MANUAL_CHILD_TYPE))
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;

        if affected.is_empty() {
            tx.commit().await.map_err(db_err)?;
            return Ok(Vec::new());
        }

        // Delete edges whose parent already links the target (would collide on the
        // (ParentId, ChildId) primary key), then retarget the rest.
        sqlx::query(
            r#"DELETE FROM "LinkedChildren"
               WHERE "ChildId" = ?1 AND "ChildType" = ?2
                 AND "ParentId" IN (SELECT "ParentId" FROM "LinkedChildren"
                     WHERE "ChildId" = ?3 AND "ChildType" = ?2)"#,
        )
        .bind(from_child_id.to_string())
        .bind(i64::from(MANUAL_CHILD_TYPE))
        .bind(to_child_id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"UPDATE "LinkedChildren" SET "ChildId" = ?1
               WHERE "ChildId" = ?2 AND "ChildType" = ?3"#,
        )
        .bind(to_child_id.to_string())
        .bind(from_child_id.to_string())
        .bind(i64::from(MANUAL_CHILD_TYPE))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;

        tx.commit().await.map_err(db_err)?;
        Ok(affected
            .iter()
            .filter_map(|s| Uuid::parse_str(s).ok())
            .collect())
    }

    async fn upsert_linked_child(
        &self,
        parent_id: Uuid,
        child_id: Uuid,
        child_type: i32,
    ) -> Result<(), ServiceError> {
        // Insert a new edge, or update its ChildType if the (parent, child) pair
        // already exists (C# find-or-add on the composite key).
        sqlx::query(
            r#"INSERT INTO "LinkedChildren" ("ParentId", "ChildId", "ChildType", "SortOrder")
               VALUES (?1, ?2, ?3, NULL)
               ON CONFLICT("ParentId", "ChildId") DO UPDATE SET "ChildType" = excluded."ChildType""#,
        )
        .bind(parent_id.to_string())
        .bind(child_id.to_string())
        .bind(i64::from(child_type))
        .execute(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{HermitLinkedChildrenService, MANUAL_CHILD_TYPE};
    use crate::test_support::{seed_item, seed_named_item, test_db};
    use hermit_model::data::BaseItemKind;
    use hermit_traits::persistence::LinkedChildrenService;
    use uuid::Uuid;

    #[tokio::test]
    async fn upsert_get_and_type_filter() {
        let db = test_db().await;
        let parent = Uuid::new_v4();
        let child_a = Uuid::new_v4();
        let child_b = Uuid::new_v4();
        for id in [parent, child_a, child_b] {
            seed_item(&db, id, BaseItemKind::Playlist).await;
        }
        let svc = HermitLinkedChildrenService::new(db);

        svc.upsert_linked_child(parent, child_a, MANUAL_CHILD_TYPE)
            .await
            .expect("insert a");
        svc.upsert_linked_child(parent, child_b, 1)
            .await
            .expect("insert b (shortcut)");

        let all = svc
            .get_linked_children_ids(parent, None)
            .await
            .expect("all");
        assert_eq!(all.len(), 2);

        let manual = svc
            .get_linked_children_ids(parent, Some(MANUAL_CHILD_TYPE))
            .await
            .expect("manual");
        assert_eq!(manual, vec![child_a]);

        // Upserting an existing pair updates its type rather than duplicating.
        svc.upsert_linked_child(parent, child_b, MANUAL_CHILD_TYPE)
            .await
            .expect("update b");
        let manual = svc
            .get_linked_children_ids(parent, Some(MANUAL_CHILD_TYPE))
            .await
            .expect("manual");
        assert_eq!(manual.len(), 2);
    }

    #[tokio::test]
    async fn manual_linked_parents_and_reroute() {
        let db = test_db().await;
        let parent1 = Uuid::new_v4();
        let parent2 = Uuid::new_v4();
        let from_child = Uuid::new_v4();
        let to_child = Uuid::new_v4();
        seed_item(&db, parent1, BaseItemKind::Playlist).await;
        seed_item(&db, parent2, BaseItemKind::BoxSet).await;
        seed_item(&db, from_child, BaseItemKind::Movie).await;
        seed_item(&db, to_child, BaseItemKind::Movie).await;
        let svc = HermitLinkedChildrenService::new(db);

        svc.upsert_linked_child(parent1, from_child, MANUAL_CHILD_TYPE)
            .await
            .expect("p1");
        svc.upsert_linked_child(parent2, from_child, MANUAL_CHILD_TYPE)
            .await
            .expect("p2");

        let all_parents = svc
            .get_manual_linked_parent_ids(from_child, None)
            .await
            .expect("parents");
        assert_eq!(all_parents.len(), 2);

        // Filter by parent kind (only the Playlist parent).
        let only_playlists = svc
            .get_manual_linked_parent_ids(from_child, Some(BaseItemKind::Playlist))
            .await
            .expect("playlist parents");
        assert_eq!(only_playlists, vec![parent1]);

        let affected = svc
            .reroute_linked_children(from_child, to_child)
            .await
            .expect("reroute");
        assert_eq!(affected.len(), 2);
        // The old child now has no manual parents; the new child has both.
        assert!(
            svc.get_manual_linked_parent_ids(from_child, None)
                .await
                .expect("old")
                .is_empty()
        );
        assert_eq!(
            svc.get_manual_linked_parent_ids(to_child, None)
                .await
                .expect("new")
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn find_artists_matches_case_insensitively() {
        let db = test_db().await;
        let a1 = Uuid::new_v4();
        let a2 = Uuid::new_v4();
        seed_named_item(&db, a1, BaseItemKind::MusicArtist, "The Beatles").await;
        seed_named_item(&db, a2, BaseItemKind::MusicArtist, "Queen").await;
        let svc = HermitLinkedChildrenService::new(db);

        let found = svc
            .find_artists(&["the beatles".to_owned(), "Unknown".to_owned()])
            .await
            .expect("find");
        assert_eq!(found.len(), 1);
        assert_eq!(found.get("the beatles").map(Vec::len), Some(1));
        assert!(!found.contains_key("Unknown"));
    }
}
