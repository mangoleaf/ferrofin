//! [`HermitPeopleRepository`] — the concrete [`PeopleRepository`] over
//! `hermit-db`.
//!
//! Port of `PeopleRepository`. People live in the `Peoples` table (one row per
//! `(Name, PersonType)`) and are credited on items through the
//! `PeopleBaseItemMap` join (carrying `Role`/`ListOrder`/`SortOrder`). The C#
//! repository maps to the domain `PersonInfo` (folding the mapping's role/sort
//! onto the person); the trait here returns [`PeopleEntity`] rows, so the join is
//! used only for filtering and ordering, not to populate role/sort fields.
//!
//! Query translation follows the C# `TranslateQuery`:
//! - when scoped to an `item_id`, restrict to people credited on that item and
//!   order by the credit's `ListOrder`, then `PersonType`, then `Name`;
//! - otherwise collapse to one representative row per lower-cased name (the C#
//!   `GroupBy(Name.ToLower).Min(Id)`), ordered by name;
//! - `parent_id`, `person_types`/`exclude_person_types` (alphanumeric only),
//!   `max_list_order`, and the name range/substring predicates are applied as in
//!   C#. The `is_favorite` user-data path is honored via a `UserData` join.

use std::collections::HashMap;

use async_trait::async_trait;
use hermit_db::Database;
use hermit_db::entities::base_items::PeopleEntity;
use hermit_model::querying::QueryResult;
use sqlx::QueryBuilder;
use sqlx::Sqlite;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::options::InternalPeopleQuery;
use hermit_traits::persistence::PeopleRepository;

use crate::db_error::db_err;

/// The stored `Type` name of a `Person` item, used by the `is_favorite`
/// user-data join (C# `itemTypeLookup.BaseItemKindNames[Person]`).
const PERSON_TYPE_NAME: &str = "MediaBrowser.Controller.Entities.Person";

/// The concrete people repository.
#[derive(Clone)]
pub struct HermitPeopleRepository {
    db: Database,
}

impl std::fmt::Debug for HermitPeopleRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitPeopleRepository")
            .finish_non_exhaustive()
    }
}

impl HermitPeopleRepository {
    /// Creates a people repository over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

/// Whether a person-type filter value is valid (C# `IsValidPersonType` =
/// alphanumeric, non-blank). Non-alphanumeric filter values are ignored rather
/// than injected into the query.
fn is_valid_person_type(value: &str) -> bool {
    !value.trim().is_empty() && value.chars().all(char::is_alphanumeric)
}

/// Appends the shared `TranslateQuery` predicates (item scope, parent scope,
/// person-type in/out, max list order, name range/substring) onto a builder
/// whose `FROM "Peoples" p` clause is already open. Returns whether the
/// `PeopleBaseItemMap` join alias `m` is required by the caller's projection.
fn push_predicates(qb: &mut QueryBuilder<'_, Sqlite>, filter: &InternalPeopleQuery) {
    if !filter.item_id.is_nil() {
        qb.push(r#" AND EXISTS (SELECT 1 FROM "PeopleBaseItemMap" mx WHERE mx."PeopleId" = p."Id" AND mx."ItemId" = "#);
        qb.push_bind(filter.item_id.to_string());
        qb.push(")");
    }
    if let Some(parent) = filter.parent_id {
        qb.push(
            r#" AND EXISTS (SELECT 1 FROM "PeopleBaseItemMap" mp
                JOIN "AncestorIds" a ON a."ItemId" = mp."ItemId"
                WHERE mp."PeopleId" = p."Id" AND a."ParentItemId" = "#,
        );
        qb.push_bind(parent.to_string());
        qb.push(")");
    }
    let include: Vec<&String> = filter
        .person_types
        .iter()
        .filter(|t| is_valid_person_type(t))
        .collect();
    if !include.is_empty() {
        qb.push(r#" AND p."PersonType" IN ("#);
        let mut sep = qb.separated(", ");
        for t in include {
            sep.push_bind(t.clone());
        }
        qb.push(")");
    }
    let exclude: Vec<&String> = filter
        .exclude_person_types
        .iter()
        .filter(|t| is_valid_person_type(t))
        .collect();
    if !exclude.is_empty() {
        qb.push(r#" AND (p."PersonType" IS NULL OR p."PersonType" NOT IN ("#);
        let mut sep = qb.separated(", ");
        for t in exclude {
            sep.push_bind(t.clone());
        }
        qb.push("))");
    }
    if let Some(max_order) = filter.max_list_order
        && !filter.item_id.is_nil()
    {
        qb.push(
            r#" AND EXISTS (SELECT 1 FROM "PeopleBaseItemMap" mo WHERE mo."PeopleId" = p."Id" AND mo."ItemId" = "#,
        );
        qb.push_bind(filter.item_id.to_string());
        qb.push(r#" AND mo."ListOrder" <= "#);
        qb.push_bind(i64::from(max_order));
        qb.push(")");
    }
    if let Some(contains) = filter
        .name_contains
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    {
        qb.push(r#" AND UPPER(p."Name") LIKE "#);
        qb.push_bind(format!("%{}%", contains.to_uppercase()));
    }
    if let Some(prefix) = filter
        .name_starts_with
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    {
        qb.push(r#" AND p."Name" LIKE "#);
        qb.push_bind(format!("{}%", prefix.to_lowercase()));
    }
    if let Some(less) = filter
        .name_less_than
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    {
        qb.push(r#" AND p."Name" < "#);
        qb.push_bind(less.to_lowercase());
    }
    if let Some(ge) = filter
        .name_starts_with_or_greater
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    {
        qb.push(r#" AND p."Name" >= "#);
        qb.push_bind(ge.to_lowercase());
    }
}

/// Opens `SELECT <cols> FROM "Peoples" p WHERE 1 = 1`, applying the optional
/// `is_favorite` user-data restriction as an `EXISTS` sub-select (C# joins
/// `UserData` on the person item's name).
fn base_query<'a>(cols: &str, filter: &InternalPeopleQuery) -> QueryBuilder<'a, Sqlite> {
    let mut qb = QueryBuilder::new(format!(r#"SELECT {cols} FROM "Peoples" p WHERE 1 = 1"#));
    if let (Some(user_id), Some(is_favorite)) = (filter.user_id, filter.is_favorite) {
        qb.push(
            r#" AND EXISTS (SELECT 1 FROM "UserData" ud
                JOIN "BaseItems" bi ON bi."Id" = ud."ItemId"
                WHERE bi."Type" = "#,
        );
        qb.push_bind(PERSON_TYPE_NAME);
        qb.push(r#" AND bi."Name" = p."Name" AND ud."UserId" = "#);
        qb.push_bind(user_id.to_string());
        qb.push(r#" AND ud."IsFavorite" = "#);
        qb.push_bind(i64::from(is_favorite));
        qb.push(")");
    }
    qb
}

#[async_trait]
impl PeopleRepository for HermitPeopleRepository {
    async fn get_people(
        &self,
        filter: &InternalPeopleQuery,
    ) -> Result<QueryResult<PeopleEntity>, ServiceError> {
        let mut rows = if filter.item_id.is_nil() {
            // Collapse to one representative id per lower-cased name.
            let mut qb = base_query(r#"p."Id", p."Name", p."PersonType""#, filter);
            push_predicates(&mut qb, filter);
            qb.push(
                r#" AND p."Id" IN (SELECT MIN(p2."Id") FROM "Peoples" p2
                    GROUP BY LOWER(p2."Name")) ORDER BY p."Name""#,
            );
            qb.build_query_as::<PeopleEntity>()
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?
        } else {
            let mut qb = base_query(r#"p."Id", p."Name", p."PersonType""#, filter);
            push_predicates(&mut qb, filter);
            qb.push(
                r#" ORDER BY (SELECT MIN(mo."ListOrder") FROM "PeopleBaseItemMap" mo
                    WHERE mo."PeopleId" = p."Id" AND mo."ItemId" = "#,
            );
            qb.push_bind(filter.item_id.to_string());
            qb.push(r#"), p."PersonType", p."Name""#);
            qb.build_query_as::<PeopleEntity>()
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?
        };

        let total = i32::try_from(rows.len()).unwrap_or(i32::MAX);
        let start = filter.start_index.unwrap_or(0);
        if start > 0 {
            let start_usize = usize::try_from(start).unwrap_or(usize::MAX);
            rows = rows.into_iter().skip(start_usize).collect();
        }
        if filter.limit > 0 {
            let limit = usize::try_from(filter.limit).unwrap_or(usize::MAX);
            rows.truncate(limit);
        }
        Ok(QueryResult::new(Some(start), Some(total), rows))
    }

    async fn update_people(
        &self,
        item_id: Uuid,
        people: &[PeopleEntity],
    ) -> Result<(), ServiceError> {
        // Dedupe case-insensitively by (name, person_type), matching the C#
        // DistinctBy(name.ToLower + "-" + type). Preserve first-seen order for
        // ListOrder.
        let mut seen = std::collections::HashSet::new();
        let mut deduped: Vec<&PeopleEntity> = Vec::new();
        for person in people {
            let key = format!(
                "{}-{}",
                person.name.trim().to_lowercase(),
                person.person_type.clone().unwrap_or_default()
            );
            if seen.insert(key) {
                deduped.push(person);
            }
        }

        let mut tx = self.db.pool().begin().await.map_err(db_err)?;

        // Clear this item's credit rows first. Doing a write as the transaction's
        // first statement takes SQLite's write lock upfront, so a concurrent writer
        // (e.g. a page load's playstate update during a scan) can't invalidate a
        // read snapshot and trip `SQLITE_BUSY_SNAPSHOT` (which busy_timeout won't
        // retry). The map is rebuilt below.
        sqlx::query(r#"DELETE FROM "PeopleBaseItemMap" WHERE "ItemId" = ?1"#)
            .bind(item_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;

        // Ensure a Peoples row exists for each (case-insensitive name + type),
        // reusing an existing id where present so credits share one person row.
        let mut people_ids: Vec<String> = Vec::with_capacity(deduped.len());
        for person in &deduped {
            let name = person.name.trim();
            let existing: Option<String> = sqlx::query_scalar(
                r#"SELECT "Id" FROM "Peoples"
                   WHERE LOWER("Name") = LOWER(?1)
                     AND ("PersonType" IS ?2 OR "PersonType" = ?2)
                   LIMIT 1"#,
            )
            .bind(name)
            .bind(&person.person_type)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;

            let id = if let Some(id) = existing {
                id
            } else {
                let new_id = if person.id.is_empty() {
                    Uuid::new_v4().to_string()
                } else {
                    person.id.clone()
                };
                sqlx::query(
                    r#"INSERT INTO "Peoples" ("Id", "Name", "PersonType") VALUES (?1, ?2, ?3)"#,
                )
                .bind(&new_id)
                .bind(name)
                .bind(&person.person_type)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                new_id
            };
            people_ids.push(id);
        }

        // Replace this item's credit rows (C# removes stale maps and re-adds).
        sqlx::query(r#"DELETE FROM "PeopleBaseItemMap" WHERE "ItemId" = ?1"#)
            .bind(item_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        for (list_order, people_id) in people_ids.iter().enumerate() {
            sqlx::query(
                r#"INSERT INTO "PeopleBaseItemMap"
                   ("ItemId", "PeopleId", "Role", "ListOrder", "SortOrder")
                   VALUES (?1, ?2, '', ?3, ?4)"#,
            )
            .bind(item_id.to_string())
            .bind(people_id)
            .bind(i64::try_from(list_order).unwrap_or(i64::MAX))
            .bind(i64::try_from(list_order).unwrap_or(i64::MAX))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }

        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn get_people_names(
        &self,
        filter: &InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError> {
        let mut qb = base_query(r#"DISTINCT p."Name""#, filter);
        push_predicates(&mut qb, filter);
        qb.push(r#" ORDER BY p."Name""#);
        let mut names: Vec<String> = qb
            .build_query_scalar::<String>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        if let Some(start) = filter.start_index.filter(|s| *s > 0) {
            let start = usize::try_from(start).unwrap_or(usize::MAX);
            names = names.into_iter().skip(start).collect();
        }
        if filter.limit > 0 {
            names.truncate(usize::try_from(filter.limit).unwrap_or(usize::MAX));
        }
        Ok(names)
    }

    async fn get_people_names_by_items(
        &self,
        item_ids: &[Uuid],
        person_types: &[String],
    ) -> Result<HashMap<Uuid, Vec<String>>, ServiceError> {
        if item_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut qb = QueryBuilder::<Sqlite>::new(
            r#"SELECT m."ItemId", p."Name" FROM "PeopleBaseItemMap" m
               JOIN "Peoples" p ON p."Id" = m."PeopleId" WHERE m."ItemId" IN ("#,
        );
        {
            let mut sep = qb.separated(", ");
            for id in item_ids {
                sep.push_bind(id.to_string());
            }
        }
        qb.push(")");
        let types: Vec<&String> = person_types
            .iter()
            .filter(|t| is_valid_person_type(t))
            .collect();
        if !types.is_empty() {
            qb.push(r#" AND p."PersonType" IN ("#);
            let mut sep = qb.separated(", ");
            for t in types {
                sep.push_bind(t.clone());
            }
            qb.push(")");
        }
        qb.push(r#" ORDER BY m."ListOrder""#);

        let rows: Vec<(String, String)> = qb
            .build_query_as::<(String, String)>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;

        let mut result: HashMap<Uuid, Vec<String>> = HashMap::new();
        for (item_id, name) in rows {
            if name.is_empty() {
                continue;
            }
            let Ok(id) = Uuid::parse_str(&item_id) else {
                continue;
            };
            let names = result.entry(id).or_default();
            if !names.contains(&name) {
                names.push(name);
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::HermitPeopleRepository;
    use crate::test_support::{seed_item, test_db};
    use hermit_db::entities::base_items::PeopleEntity;
    use hermit_model::data::BaseItemKind;
    use hermit_traits::options::InternalPeopleQuery;
    use hermit_traits::persistence::PeopleRepository;
    use uuid::Uuid;

    fn person(name: &str, person_type: &str) -> PeopleEntity {
        PeopleEntity {
            id: String::new(),
            name: name.to_owned(),
            person_type: Some(person_type.to_owned()),
        }
    }

    #[tokio::test]
    async fn update_and_get_people_for_item_in_credit_order() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = HermitPeopleRepository::new(db);

        repo.update_people(
            item,
            &[
                person("Alice", "Actor"),
                person("Bob", "Director"),
                person("Carol", "Actor"),
            ],
        )
        .await
        .expect("update");

        let query = InternalPeopleQuery {
            item_id: item,
            ..Default::default()
        };
        let result = repo.get_people(&query).await.expect("get");
        assert_eq!(result.total_record_count, 3);
        // Ordered by ListOrder (insertion order).
        assert_eq!(result.items[0].name, "Alice");
        assert_eq!(result.items[1].name, "Bob");
        assert_eq!(result.items[2].name, "Carol");
    }

    #[tokio::test]
    async fn person_type_filter_and_names() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = HermitPeopleRepository::new(db);
        repo.update_people(item, &[person("Alice", "Actor"), person("Bob", "Director")])
            .await
            .expect("update");

        let query = InternalPeopleQuery {
            item_id: item,
            person_types: vec!["Actor".to_owned()],
            ..Default::default()
        };
        let result = repo.get_people(&query).await.expect("get");
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].name, "Alice");

        let names = repo
            .get_people_names(&InternalPeopleQuery {
                item_id: item,
                ..Default::default()
            })
            .await
            .expect("names");
        assert_eq!(names, vec!["Alice".to_owned(), "Bob".to_owned()]);
    }

    #[tokio::test]
    async fn names_by_items_groups_and_dedupes() {
        let db = test_db().await;
        let item_a = Uuid::new_v4();
        let item_b = Uuid::new_v4();
        seed_item(&db, item_a, BaseItemKind::Movie).await;
        seed_item(&db, item_b, BaseItemKind::Movie).await;
        let repo = HermitPeopleRepository::new(db);
        repo.update_people(item_a, &[person("Alice", "Actor")])
            .await
            .expect("a");
        repo.update_people(
            item_b,
            &[person("Alice", "Actor"), person("Bob", "Director")],
        )
        .await
        .expect("b");

        let map = repo
            .get_people_names_by_items(&[item_a, item_b], &[])
            .await
            .expect("map");
        assert_eq!(map.get(&item_a).map(Vec::len), Some(1));
        assert_eq!(map.get(&item_b).map(Vec::len), Some(2));

        // Filtering by a person type narrows the names.
        let map = repo
            .get_people_names_by_items(&[item_b], &["Director".to_owned()])
            .await
            .expect("filtered");
        assert_eq!(map.get(&item_b).cloned(), Some(vec!["Bob".to_owned()]));
    }

    #[tokio::test]
    async fn no_item_scope_collapses_by_name() {
        let db = test_db().await;
        let item_a = Uuid::new_v4();
        let item_b = Uuid::new_v4();
        seed_item(&db, item_a, BaseItemKind::Movie).await;
        seed_item(&db, item_b, BaseItemKind::Movie).await;
        let repo = HermitPeopleRepository::new(db);
        // Same person credited (as different types) on two items → one Peoples row
        // per (name,type); the un-scoped query collapses to one row per name.
        repo.update_people(item_a, &[person("Alice", "Actor")])
            .await
            .expect("a");
        repo.update_people(item_b, &[person("Alice", "GuestStar")])
            .await
            .expect("b");

        let all = repo
            .get_people(&InternalPeopleQuery::default())
            .await
            .expect("all");
        assert_eq!(all.items.len(), 1);
        assert_eq!(all.items[0].name, "Alice");
    }
}
