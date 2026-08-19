//! [`FerrofinPeopleRepository`] — the concrete [`PeopleRepository`] over
//! `ferrofin-db`.
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
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::PeopleEntity;
use ferrofin_db::store::{guid_to_db, opt_datetime_to_db};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::querying::QueryResult;
use sqlx::QueryBuilder;
use sqlx::Sqlite;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::InternalPeopleQuery;
use ferrofin_traits::persistence::{PeopleRepository, PersonMetadata, WrittenPerson};

use crate::db_error::db_err;
use crate::item_type_lookup;
use crate::item_type_lookup::stored_type_name;

/// The stored `Type` name of a `Person` item, used by the `is_favorite`
/// user-data join (C# `itemTypeLookup.BaseItemKindNames[Person]`).
const PERSON_TYPE_NAME: &str = "MediaBrowser.Controller.Entities.Person";

/// The concrete people repository.
#[derive(Clone)]
pub struct FerrofinPeopleRepository {
    db: Database,
    /// The per-database id derivation + the People metadata path, together
    /// yielding the deterministic per-NAME `Person` item id
    /// ([`item_type_lookup::person_item_id`]). `None` (unit tests) falls back
    /// to per-(name, type) `Peoples` row ids — the pre-unification behavior.
    identity: Option<(item_type_lookup::IdDerivation, String)>,
}

impl std::fmt::Debug for FerrofinPeopleRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinPeopleRepository")
            .finish_non_exhaustive()
    }
}

impl FerrofinPeopleRepository {
    /// Creates a people repository over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db, identity: None }
    }

    /// Wires the id derivation + People metadata path, so person by-name items
    /// materialize with Jellyfin's one-id-per-name identity.
    #[must_use]
    pub fn with_identity(
        mut self,
        mode: item_type_lookup::IdDerivation,
        people_path: String,
    ) -> Self {
        self.identity = Some((mode, people_path));
        self
    }

    /// The deterministic per-name `Person` item id, in stored (uppercase) form,
    /// when the identity seam is wired.
    fn person_item_id(&self, name: &str) -> Option<String> {
        let (mode, people_path) = self.identity.as_ref()?;
        item_type_lookup::person_item_id(mode, people_path, name).map(guid_to_db)
    }

    /// Collapses one duplicate `Person` row onto `target` inside `tx`:
    /// ensures the target exists, copies still-empty enrichment columns,
    /// repoints user data + images (clashes keep the target's rows), and
    /// drops the duplicate.
    async fn collapse_person_row(
        tx: &mut sqlx::Transaction<'_, Sqlite>,
        person_type: &str,
        name: &str,
        old_id: &str,
        target: &str,
        create_target: bool,
    ) -> Result<(), ServiceError> {
        if create_target {
            let clean = crate::text_util::get_clean_value(name);
            sqlx::query(
                r#"INSERT OR IGNORE INTO "BaseItems"
                   ("Id","Type","Name","CleanName","IsFolder","IsInMixedFolder",
                    "IsLocked","IsMovie","IsRepeat","IsSeries","IsVirtualItem")
                   VALUES (?1,?2,?3,?4,0,0,0,0,0,0,0)"#,
            )
            .bind(target)
            .bind(person_type)
            .bind(name)
            .bind(&clean)
            .execute(&mut **tx)
            .await
            .map_err(db_err)?;
        }
        sqlx::query(
            r#"UPDATE "BaseItems" SET
                 "Overview" = COALESCE("Overview",
                    (SELECT "Overview" FROM "BaseItems" WHERE "Id" = ?1)),
                 "PremiereDate" = COALESCE("PremiereDate",
                    (SELECT "PremiereDate" FROM "BaseItems" WHERE "Id" = ?1)),
                 "EndDate" = COALESCE("EndDate",
                    (SELECT "EndDate" FROM "BaseItems" WHERE "Id" = ?1)),
                 "ProductionLocations" = COALESCE("ProductionLocations",
                    (SELECT "ProductionLocations" FROM "BaseItems" WHERE "Id" = ?1))
               WHERE "Id" = ?2"#,
        )
        .bind(old_id)
        .bind(target)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;
        for sql in [
            r#"UPDATE OR IGNORE "UserData" SET "ItemId" = ?2 WHERE "ItemId" = ?1"#,
            r#"UPDATE OR IGNORE "BaseItemImageInfos" SET "ItemId" = ?2 WHERE "ItemId" = ?1"#,
        ] {
            sqlx::query(sql)
                .bind(old_id)
                .bind(target)
                .execute(&mut **tx)
                .await
                .map_err(db_err)?;
        }
        for sql in [
            r#"DELETE FROM "UserData" WHERE "ItemId" = ?1"#,
            r#"DELETE FROM "BaseItemImageInfos" WHERE "ItemId" = ?1"#,
            r#"DELETE FROM "BaseItems" WHERE "Id" = ?1"#,
        ] {
            sqlx::query(sql)
                .bind(old_id)
                .execute(&mut **tx)
                .await
                .map_err(db_err)?;
        }
        Ok(())
    }

    /// One-shot startup pass: collapses the pre-unification per-(name, type)
    /// `Person` items onto the deterministic per-name id, repointing user data
    /// and images, and records completion in `HermitMeta` so subsequent boots
    /// skip it. On a database adopted from Jellyfin (already one id per name)
    /// every group resolves to its existing row and nothing changes.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] when the rewrite transaction fails; the
    /// marker is only written after a successful pass.
    pub async fn unify_person_identities(&self) -> Result<u64, ServiceError> {
        const META_KEY: &str = "person_identity_unified";
        if self.identity.is_none() {
            return Ok(0);
        }
        let done: Option<String> =
            sqlx::query_scalar(r#"SELECT "Value" FROM "HermitMeta" WHERE "Key" = ?1 LIMIT 1"#)
                .bind(META_KEY)
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)?;
        if done.as_deref() == Some("1") {
            return Ok(0);
        }

        let person_type = stored_type_name(BaseItemKind::Person).unwrap_or_default();
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            r#"SELECT "Id", "Name" FROM "BaseItems" WHERE "Type" = ?1 ORDER BY "Id""#,
        )
        .bind(person_type)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;

        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        let mut collapsed: u64 = 0;
        let mut seen_targets: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (old_id, name) in rows {
            let Some(name) = name.as_deref().map(str::trim).filter(|n| !n.is_empty()) else {
                continue;
            };
            let Some(target) = self.person_item_id(name) else {
                continue;
            };
            if old_id.eq_ignore_ascii_case(&target) {
                seen_targets.insert(target);
                continue;
            }
            // The target row must exist BEFORE any child rows repoint at it
            // (FK BaseItems.Id) — collapse_person_row handles the ordering.
            let create_target = seen_targets.insert(target.clone());
            Self::collapse_person_row(&mut tx, person_type, name, &old_id, &target, create_target)
                .await?;
            collapsed += 1;
        }
        sqlx::query(
            r#"INSERT INTO "HermitMeta" ("Key", "Value") VALUES (?1, '1')
               ON CONFLICT("Key") DO UPDATE SET "Value" = '1'"#,
        )
        .bind(META_KEY)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(collapsed)
    }
}

/// Dedupes credited people case-insensitively by `(name, person_type)`, matching
/// the C# `DistinctBy(name.ToLower + "-" + type)` and preserving first-seen order
/// (which becomes the credit `ListOrder`).
fn dedupe_people(people: &[PeopleEntity]) -> Vec<&PeopleEntity> {
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
    deduped
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
        qb.push_bind(guid_to_db(filter.item_id));
        qb.push(")");
    }
    if let Some(parent) = filter.parent_id {
        qb.push(
            r#" AND EXISTS (SELECT 1 FROM "PeopleBaseItemMap" mp
                JOIN "AncestorIds" a ON a."ItemId" = mp."ItemId"
                WHERE mp."PeopleId" = p."Id" AND a."ParentItemId" = "#,
        );
        qb.push_bind(guid_to_db(parent));
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
        qb.push_bind(guid_to_db(filter.item_id));
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

/// Opens `SELECT <cols> FROM <from> p WHERE 1 = 1`, applying the optional
/// `is_favorite` user-data restriction as an `EXISTS` sub-select (C# joins
/// `UserData` on the person item's name). `from` is either the raw
/// `"Peoples"` table (the listing + item-scoped reads) or the deduped derived
/// table [`DEDUP_PEOPLE_FROM`] (the pagination total) — the predicates only
/// reference `p."Name"`/`p."Id"`/`p."PersonType"`, which both shapes expose.
fn base_query_from<'a>(
    cols: &str,
    from: &str,
    filter: &InternalPeopleQuery,
) -> QueryBuilder<'a, Sqlite> {
    let mut qb = QueryBuilder::new(format!("SELECT {cols} FROM {from} p WHERE 1 = 1"));
    if let (Some(user_id), Some(is_favorite)) = (filter.user_id, filter.is_favorite) {
        qb.push(
            r#" AND EXISTS (SELECT 1 FROM "UserData" ud
                JOIN "BaseItems" bi ON bi."Id" = ud."ItemId"
                WHERE bi."Type" = "#,
        );
        qb.push_bind(PERSON_TYPE_NAME);
        qb.push(r#" AND bi."Name" = p."Name" AND ud."UserId" = "#);
        qb.push_bind(guid_to_db(user_id));
        qb.push(r#" AND ud."IsFavorite" = "#);
        qb.push_bind(i64::from(is_favorite));
        qb.push(")");
    }
    qb
}

fn base_query<'a>(cols: &str, filter: &InternalPeopleQuery) -> QueryBuilder<'a, Sqlite> {
    base_query_from(cols, r#""Peoples""#, filter)
}

/// The deduped-people derived table: one representative row per lower-cased
/// name. SQLite's documented single-`MIN` bare-column semantics make the
/// non-aggregated columns come from the `MIN("Id")` row — the same
/// representative the previous `p."Id" IN (SELECT MIN(...) GROUP BY ...)`
/// shape selected (verified row-identical on the bench library), but in ONE
/// aggregation pass that the `HermitIX_Peoples_LowerName_Cover` index serves
/// as an index-only scan: 28 ms → 0.85 ms per query on 7.5k people.
const DEDUP_PEOPLE_FROM: &str = r#"(SELECT MIN(p2."Id") AS "Id", p2."Name", p2."PersonType"
     FROM "Peoples" p2 GROUP BY LOWER(p2."Name"))"#;

/// The row-wise spelling of the same dedup: a `"Peoples"` row survives iff its
/// `"Id"` is the `MIN("Id")` of every row sharing its lower-cased name — exactly
/// the row [`DEDUP_PEOPLE_FROM`]'s aggregate projects (SQLite's documented
/// single-`MIN` bare-column rule picks the `MIN("Id")` row's `"Name"` /
/// `"PersonType"`). Set-, column- and order-identical to the aggregate, verified
/// row-by-row on a 9 100-row `Peoples` table containing case-variant duplicates.
///
/// The point of the rewrite is the *plan*: because the outer table is plain
/// `"Peoples"`, SQLite walks `IX_Peoples_Name` in `"Name"` order, so
/// `ORDER BY p."Name"` needs no temp B-tree over every person and `LIMIT` stops
/// the scan once a page is filled. The aggregate form had to build, sort and
/// materialize the ENTIRE deduped set for every request no matter how small the
/// page: 2.3 ms per `limit=100` request on 7 500 people vs 0.08 ms here.
const REPRESENTATIVE_ROW: &str = r#" AND p."Id" = (SELECT MIN(p2."Id") FROM "Peoples" p2
     WHERE LOWER(p2."Name") = LOWER(p."Name"))"#;

impl FerrofinPeopleRepository {
    /// Fallback total when the by-name page is empty (past the last row).
    async fn count_people_total(&self, filter: &InternalPeopleQuery) -> Result<i64, ServiceError> {
        let mut qb = base_query_from("COUNT(*)", DEDUP_PEOPLE_FROM, filter);
        push_predicates(&mut qb, filter);
        let count: i64 = qb
            .build_query_scalar()
            .fetch_one(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(count)
    }

    /// The `/Persons` by-name listing: the deduped representative rows
    /// ([`REPRESENTATIVE_ROW`]) with the predicates applied to the
    /// representative, ordered by `"Name"`, paged in SQL.
    ///
    /// **A page-sized request does page-sized work.** Both previous shapes were
    /// O(every person) per request whatever the page size: the first
    /// materialized the whole deduped table through sqlx and sliced in Rust; the
    /// second pushed `LIMIT`/`OFFSET` down but kept a `GROUP BY LOWER("Name")`
    /// derived table (full aggregate pass), a `ORDER BY "Name"` that could only
    /// be served by a temp B-tree over every group, and a `COUNT(*) OVER()`
    /// whose window forced the entire match set to be built anyway. On a 7 500-row
    /// `Peoples` table that is 2.3 ms of SQL for a `limit=100` page — 1.4 CPU-cores
    /// of pure SQLite at the endpoint's calibrated 608 req/s, which is what made
    /// `/Persons` collapse under load while looking healthy when idle.
    ///
    /// The total stays exact. A short page *is* the end of the result set, so
    /// `start_index + page length` is the count — no second query. Only a full
    /// page (more rows may follow) or an empty page past the end (nothing to
    /// infer from) falls back to [`Self::count_people_total`], which is still a
    /// full aggregate pass: measured end to end on those 7 500 rows a
    /// `limit=100` request is 0.09 ms (page) + 1.0 ms (count) ≈ 1.1 ms against
    /// the old 2.3–3.0 ms, and every request that does NOT need the count is
    /// ~20× cheaper.
    ///
    /// Counting distinct names cannot be made sub-linear without a materialized
    /// view, and `InternalPeopleQuery` has no `enable_total_record_count` knob to
    /// turn it off — worth adding, since the only production callers
    /// (`LibraryManager::get_people` → `.items`, `get_people_items`) throw the
    /// total away and `PersonsController` reports `items.len()` on the wire.
    async fn get_people_by_name(
        &self,
        filter: &InternalPeopleQuery,
    ) -> Result<QueryResult<PeopleEntity>, ServiceError> {
        let mut qb = base_query(r#"p."Id", p."Name", p."PersonType""#, filter);
        qb.push(REPRESENTATIVE_ROW);
        push_predicates(&mut qb, filter);
        qb.push(r#" ORDER BY p."Name""#);
        let start = filter.start_index.unwrap_or(0).max(0);
        let limit = filter.limit.max(0);
        if limit > 0 || start > 0 {
            qb.push(" LIMIT ");
            qb.push_bind(if limit > 0 { i64::from(limit) } else { -1 });
            if start > 0 {
                qb.push(" OFFSET ");
                qb.push_bind(i64::from(start));
            }
        }
        let items = qb
            .build_query_as::<PeopleEntity>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;

        // `role` / `primary_image_url` / `provider_id` are write-path fields, not
        // `Peoples` columns: `#[sqlx(default)]` leaves them `None` here, as the
        // by-name listing has always returned them.
        let page_len = i32::try_from(items.len()).unwrap_or(i32::MAX);
        let full_page = limit > 0 && page_len >= limit;
        let total = if full_page || (items.is_empty() && start > 0) {
            self.count_people_total(filter).await?
        } else {
            i64::from(start) + i64::from(page_len)
        };
        Ok(QueryResult::new(
            Some(start),
            Some(i32::try_from(total).unwrap_or(i32::MAX)),
            items,
        ))
    }
}

#[async_trait]
impl PeopleRepository for FerrofinPeopleRepository {
    async fn get_people(
        &self,
        filter: &InternalPeopleQuery,
    ) -> Result<QueryResult<PeopleEntity>, ServiceError> {
        if filter.item_id.is_nil() {
            return self.get_people_by_name(filter).await;
        }
        let mut rows = {
            // Item-scoped credits carry the credited role (a character name)
            // from the map row, like the C# `GetPeople` projection — without it
            // every cast entry renders roleless on the detail page. NULLIF folds
            // the write path's empty-string "no role" back to NULL. The item id
            // is a canonically formatted GUID (hyphenated hex), so inlining it
            // is injection-safe.
            let cols = format!(
                r#"p."Id", p."Name", p."PersonType",
                   (SELECT NULLIF(mr."Role", '') FROM "PeopleBaseItemMap" mr
                    WHERE mr."PeopleId" = p."Id" AND mr."ItemId" = '{}'
                    ORDER BY mr."ListOrder" LIMIT 1) AS "Role""#,
                guid_to_db(filter.item_id)
            );
            let mut qb = base_query(&cols, filter);
            push_predicates(&mut qb, filter);
            qb.push(
                r#" ORDER BY (SELECT MIN(mo."ListOrder") FROM "PeopleBaseItemMap" mo
                    WHERE mo."PeopleId" = p."Id" AND mo."ItemId" = "#,
            );
            qb.push_bind(guid_to_db(filter.item_id));
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

    async fn get_people_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<PeopleEntity>>, ServiceError> {
        // The credits for the whole page in one query, tagged with their item so
        // they group back per item in the same ListOrder the single-item read uses.
        #[derive(sqlx::FromRow)]
        struct Row {
            #[sqlx(rename = "ItemId")]
            item_id: String,
            #[sqlx(flatten)]
            person: PeopleEntity,
        }
        let mut out: HashMap<Uuid, Vec<PeopleEntity>> =
            item_ids.iter().map(|&id| (id, Vec::new())).collect();
        if item_ids.is_empty() {
            return Ok(out);
        }
        for chunk in item_ids.chunks(500) {
            let mut qb = QueryBuilder::<Sqlite>::new(
                r#"SELECT m."ItemId", p."Id", p."Name", p."PersonType",
                          NULLIF(m."Role", '') AS "Role"
                   FROM "PeopleBaseItemMap" m JOIN "Peoples" p ON p."Id" = m."PeopleId"
                   WHERE m."ItemId" IN ("#,
            );
            let mut sep = qb.separated(", ");
            for id in chunk {
                sep.push_bind(guid_to_db(*id));
            }
            qb.push(r#") ORDER BY m."ItemId", m."ListOrder", p."PersonType", p."Name""#);
            let rows = qb
                .build_query_as::<Row>()
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?;
            for row in rows {
                if let Ok(id) = Uuid::parse_str(&row.item_id) {
                    out.entry(id).or_default().push(row.person);
                }
            }
        }
        Ok(out)
    }

    async fn update_people(
        &self,
        item_id: Uuid,
        people: &[PeopleEntity],
    ) -> Result<Vec<WrittenPerson>, ServiceError> {
        let deduped = dedupe_people(people);
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;

        // Clear this item's credit rows first. Doing a write as the transaction's
        // first statement takes SQLite's write lock upfront, so a concurrent writer
        // (e.g. a page load's playstate update during a scan) can't invalidate a
        // read snapshot and trip `SQLITE_BUSY_SNAPSHOT` (which busy_timeout won't
        // retry). The map is rebuilt below.
        sqlx::query(r#"DELETE FROM "PeopleBaseItemMap" WHERE "ItemId" = ?1"#)
            .bind(guid_to_db(item_id))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;

        // Ensure a Peoples row exists for each (case-insensitive name + type),
        // reusing an existing id where present so credits share one person row.
        // Each person is also materialized as a browsable `Person` BaseItems row
        // (id = the Peoples id) so `/Persons/{name}` and `/Items/{personId}`
        // resolve (else the client's person page spins forever), and so its image
        // rows have an FK target. Collect the profile-image URLs to hand back for
        // download.
        let person_type_name = stored_type_name(BaseItemKind::Person);
        let mut people_ids: Vec<String> = Vec::with_capacity(deduped.len());
        let mut written: Vec<WrittenPerson> = Vec::with_capacity(deduped.len());
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
                    guid_to_db(Uuid::new_v4())
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

            // Materialize the browsable Person item: ONE row per name with the
            // deterministic Jellyfin id (Person.GetPath-derived), shared by
            // every credit type — favorites written against it read back from
            // every surface. Falls back to the Peoples row id when the
            // identity seam is not wired (unit tests).
            let item_id = self.person_item_id(name).unwrap_or_else(|| id.clone());
            if let Some(type_name) = person_type_name {
                let clean = crate::text_util::get_clean_value(name);
                sqlx::query(
                    r#"INSERT OR IGNORE INTO "BaseItems"
                       ("Id","Type","Name","CleanName","IsFolder","IsInMixedFolder",
                        "IsLocked","IsMovie","IsRepeat","IsSeries","IsVirtualItem")
                       VALUES (?1,?2,?3,?4,0,0,0,0,0,0,0)"#,
                )
                .bind(&item_id)
                .bind(type_name)
                .bind(name)
                .bind(&clean)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            }

            // Enrich (fetch a biography for) any person whose item has no overview
            // yet — new people and those scanned before biographies existed.
            let overview: Option<String> =
                sqlx::query_scalar(r#"SELECT "Overview" FROM "BaseItems" WHERE "Id" = ?1"#)
                    .bind(&item_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db_err)?
                    .flatten();
            let needs_details = overview.is_none_or(|o| o.is_empty());

            if let Ok(pid) = Uuid::parse_str(&item_id) {
                written.push(WrittenPerson {
                    id: pid,
                    needs_details,
                    image_url: person
                        .primary_image_url
                        .as_deref()
                        .filter(|u| !u.is_empty())
                        .map(str::to_owned),
                    provider_id: person.provider_id.filter(|id| *id > 0),
                });
            }
            people_ids.push(id);
        }

        // Rebuild this item's credit rows, preserving each credited role.
        for (list_order, (people_id, person)) in people_ids.iter().zip(deduped.iter()).enumerate() {
            sqlx::query(
                r#"INSERT INTO "PeopleBaseItemMap"
                   ("ItemId", "PeopleId", "Role", "ListOrder", "SortOrder")
                   VALUES (?1, ?2, ?3, ?4, ?5)"#,
            )
            .bind(guid_to_db(item_id))
            .bind(people_id)
            .bind(person.role.clone().unwrap_or_default())
            .bind(i64::try_from(list_order).unwrap_or(i64::MAX))
            .bind(i64::try_from(list_order).unwrap_or(i64::MAX))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }

        tx.commit().await.map_err(db_err)?;
        Ok(written)
    }

    async fn set_person_metadata(
        &self,
        person_id: Uuid,
        metadata: PersonMetadata,
    ) -> Result<(), ServiceError> {
        sqlx::query(
            r#"UPDATE "BaseItems"
               SET "Overview" = ?2, "PremiereDate" = ?3, "EndDate" = ?4,
                   "ProductionLocations" = ?5
               WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(person_id))
        .bind(metadata.overview)
        .bind(opt_datetime_to_db(metadata.premiere_date))
        .bind(opt_datetime_to_db(metadata.end_date))
        .bind(metadata.birthplace)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
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
                sep.push_bind(guid_to_db(*id));
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
    use super::FerrofinPeopleRepository;
    use crate::test_support::{seed_item, test_db};
    use ferrofin_db::entities::base_items::PeopleEntity;
    use ferrofin_db::store::guid_to_db;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_traits::options::InternalPeopleQuery;
    use ferrofin_traits::persistence::{PeopleRepository, PersonMetadata};
    use uuid::Uuid;

    fn person(name: &str, person_type: &str) -> PeopleEntity {
        PeopleEntity {
            id: String::new(),
            name: name.to_owned(),
            person_type: Some(person_type.to_owned()),
            ..Default::default()
        }
    }

    /// Inserts a `Peoples` row at an exact id, so a test can pin which row of a
    /// case-variant name group is the `MIN("Id")` representative.
    async fn seed_people_row(db: &ferrofin_db::Database, id: u128, name: &str, person_type: &str) {
        sqlx::query(r#"INSERT INTO "Peoples" ("Id", "Name", "PersonType") VALUES (?1, ?2, ?3)"#)
            .bind(guid_to_db(Uuid::from_u128(id)))
            .bind(name)
            .bind(person_type)
            .execute(db.writer())
            .await
            .expect("seed people row");
    }

    /// The fixture used by the by-name listing tests: six distinct names, two of
    /// them spelled in two cases across two `Peoples` rows (so the dedup has to
    /// pick a representative), plus one name whose only row is a `Director`.
    ///
    /// `"Name"` order (binary, the listing's ORDER BY) is
    /// `Ana < Bob < Dave < Zed < carol < Éve`.
    async fn seed_by_name_fixture(db: &ferrofin_db::Database) {
        seed_people_row(db, 0x1001, "Ana", "Actor").await;
        seed_people_row(db, 0x1002, "ana", "Director").await; // same name, higher id
        seed_people_row(db, 0x1003, "Bob", "Actor").await;
        seed_people_row(db, 0x1005, "Carol", "Writer").await; // higher id than "carol"
        seed_people_row(db, 0x1004, "carol", "Actor").await;
        seed_people_row(db, 0x1006, "Dave", "Actor").await;
        seed_people_row(db, 0x1007, "Éve", "Actor").await;
        seed_people_row(db, 0x1008, "Zed", "Director").await;
    }

    fn listed(result: &ferrofin_model::querying::QueryResult<PeopleEntity>) -> Vec<String> {
        result
            .items
            .iter()
            .map(|p| {
                format!(
                    "{}|{}|{}",
                    p.id,
                    p.name,
                    p.person_type.clone().unwrap_or_default()
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn by_name_pages_reassemble_the_unpaged_listing_with_an_exact_total() {
        let db = test_db().await;
        seed_by_name_fixture(&db).await;
        let repo = FerrofinPeopleRepository::new(db);

        let all = repo
            .get_people(&InternalPeopleQuery::default())
            .await
            .expect("unpaged");
        assert_eq!(
            all.items.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
            vec!["Ana", "Bob", "Dave", "Zed", "carol", "Éve"],
            "one row per lower-cased name, ordered by Name"
        );
        assert_eq!(all.total_record_count, 6);

        // Every page of two, re-assembled, must be the unpaged listing — and each
        // page must report the same total (the full-page branch runs the COUNT,
        // the short/empty pages derive it from the offset).
        let mut paged: Vec<String> = Vec::new();
        for start in [0, 2, 4, 6] {
            let page = repo
                .get_people(&InternalPeopleQuery {
                    limit: 2,
                    start_index: Some(start),
                    ..Default::default()
                })
                .await
                .expect("page");
            assert_eq!(
                page.total_record_count, 6,
                "total must be exact on the page at offset {start}"
            );
            assert_eq!(page.start_index, start);
            paged.extend(listed(&page));
        }
        assert_eq!(paged, listed(&all), "paged slices == unpaged listing");

        // A *partial* last page is where the total is derived rather than
        // counted: offset 4 of a page of 4 returns the last two rows, so the
        // total must be 4 + 2, not 2.
        let tail = repo
            .get_people(&InternalPeopleQuery {
                limit: 4,
                start_index: Some(4),
                ..Default::default()
            })
            .await
            .expect("partial last page");
        assert_eq!(
            tail.items
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
            vec!["carol", "Éve"]
        );
        assert_eq!(tail.total_record_count, 6);

        // Same for an unpaged read that starts part-way in.
        let unpaged_tail = repo
            .get_people(&InternalPeopleQuery {
                start_index: Some(5),
                ..Default::default()
            })
            .await
            .expect("unpaged tail");
        assert_eq!(
            unpaged_tail
                .items
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
            vec!["Éve"]
        );
        assert_eq!(unpaged_tail.total_record_count, 6);

        // A page that lands entirely past the end still reports the real total.
        let past = repo
            .get_people(&InternalPeopleQuery {
                limit: 2,
                start_index: Some(50),
                ..Default::default()
            })
            .await
            .expect("past end");
        assert!(past.items.is_empty());
        assert_eq!(past.total_record_count, 6);
    }

    #[tokio::test]
    async fn by_name_representative_is_the_min_id_row_of_the_name_group() {
        let db = test_db().await;
        seed_by_name_fixture(&db).await;
        let repo = FerrofinPeopleRepository::new(db);

        let all = repo
            .get_people(&InternalPeopleQuery::default())
            .await
            .expect("unpaged");
        // "Ana"/"ana": the MIN(Id) row is 0x1001 → its id, ITS spelling and ITS
        // person type are what the listing returns.
        let ana = &all.items[0];
        assert_eq!(ana.id, guid_to_db(Uuid::from_u128(0x1001)));
        assert_eq!(ana.name, "Ana");
        assert_eq!(ana.person_type.as_deref(), Some("Actor"));
        // "carol"/"Carol": here the LOWER-cased spelling holds the MIN(Id), so the
        // representative is the lower-cased row — and it sorts after "Zed".
        let carol = all
            .items
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case("carol"))
            .expect("carol listed");
        assert_eq!(carol.id, guid_to_db(Uuid::from_u128(0x1004)));
        assert_eq!(carol.name, "carol");
        assert_eq!(carol.person_type.as_deref(), Some("Actor"));
    }

    #[tokio::test]
    async fn by_name_filters_apply_to_the_representative_row_only() {
        let db = test_db().await;
        seed_by_name_fixture(&db).await;
        let repo = FerrofinPeopleRepository::new(db);

        // "ana" HAS a Director row (0x1002) — but it is not the group's
        // representative, so the Director filter must not surface the name.
        // Only "Zed", whose sole (and therefore representative) row is a
        // Director, matches.
        let directors = repo
            .get_people(&InternalPeopleQuery {
                person_types: vec!["Director".to_owned()],
                ..Default::default()
            })
            .await
            .expect("directors");
        assert_eq!(
            directors
                .items
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
            vec!["Zed"]
        );
        assert_eq!(directors.total_record_count, 1);

        // The same restriction, paged: the total stays the filtered total, not
        // the unfiltered one.
        let page = repo
            .get_people(&InternalPeopleQuery {
                person_types: vec!["Actor".to_owned()],
                limit: 2,
                ..Default::default()
            })
            .await
            .expect("actors page");
        assert_eq!(
            page.items
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
            vec!["Ana", "Bob"]
        );
        assert_eq!(
            page.total_record_count, 5,
            "Ana, Bob, Dave, carol, Éve are the Actor representatives"
        );

        // A name-substring filter pages the same way.
        let contains = repo
            .get_people(&InternalPeopleQuery {
                name_contains: Some("a".to_owned()),
                limit: 1,
                ..Default::default()
            })
            .await
            .expect("contains");
        assert_eq!(
            contains
                .items
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
            vec!["Ana"]
        );
        assert_eq!(
            contains.total_record_count, 3,
            "Ana, Dave, carol contain an 'a'"
        );
    }

    #[tokio::test]
    async fn identity_wired_repo_materializes_one_person_item_per_name() {
        use crate::item_type_lookup::{IdDerivation, person_item_id};

        let db = test_db().await;
        let mode = IdDerivation::Jellyfin {
            program_data_path: Some("/data".to_owned()),
        };
        let repo = FerrofinPeopleRepository::new(db.clone())
            .with_identity(mode.clone(), "/data/metadata/People".to_owned());
        let movie_a = Uuid::from_u128(0x11);
        let movie_b = Uuid::from_u128(0x12);
        seed_item(&db, movie_a, BaseItemKind::Movie).await;
        seed_item(&db, movie_b, BaseItemKind::Movie).await;

        // The same person credited as Actor on one item and Director on the
        // other: TWO Peoples rows (per credit type, as upstream), but exactly
        // ONE browsable Person item, at the deterministic per-name id.
        repo.update_people(movie_a, &[person("Steve Carell", "Actor")])
            .await
            .expect("credits a");
        repo.update_people(movie_b, &[person("Steve Carell", "Director")])
            .await
            .expect("credits b");

        let expected = guid_to_db(
            person_item_id(&mode, "/data/metadata/People", "Steve Carell").expect("derived"),
        );
        let rows: Vec<String> = sqlx::query_scalar(
            r#"SELECT "Id" FROM "BaseItems"
               WHERE "Type" = 'MediaBrowser.Controller.Entities.Person'"#,
        )
        .fetch_all(db.pool())
        .await
        .expect("person items");
        assert_eq!(rows, vec![expected], "one item row, at the derived id");

        let people_rows: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "Peoples""#)
            .fetch_one(db.pool())
            .await
            .expect("peoples");
        assert_eq!(people_rows, 2, "credit rows stay per (name, type)");
    }

    #[tokio::test]
    async fn unify_collapses_duplicate_person_items_and_repoints_user_data() {
        use crate::item_type_lookup::{IdDerivation, person_item_id};
        use crate::test_support::seed_named_item;

        let db = test_db().await;
        let mode = IdDerivation::Jellyfin {
            program_data_path: Some("/data".to_owned()),
        };
        // Two pre-unification duplicates of one person (random per-type ids)…
        let dup_a = Uuid::from_u128(0xA1);
        let dup_b = Uuid::from_u128(0xA2);
        seed_named_item(&db, dup_a, BaseItemKind::Person, "Uma Thurman").await;
        seed_named_item(&db, dup_b, BaseItemKind::Person, "Uma Thurman").await;
        // …one carrying a favorite.
        let user = crate::test_support::seed_user(&db, Uuid::from_u128(0x9)).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();
        crate::test_support::seed_user_data(&db, user_id, dup_a, false, None).await;
        sqlx::query(r#"UPDATE "UserData" SET "IsFavorite" = 1 WHERE "ItemId" = ?1"#)
            .bind(guid_to_db(dup_a))
            .execute(db.writer())
            .await
            .expect("favorite");

        let repo = FerrofinPeopleRepository::new(db.clone())
            .with_identity(mode.clone(), "/data/metadata/People".to_owned());
        let collapsed = repo.unify_person_identities().await.expect("unify");
        assert_eq!(collapsed, 2);

        let target = guid_to_db(
            person_item_id(&mode, "/data/metadata/People", "Uma Thurman").expect("derived"),
        );
        let rows: Vec<String> = sqlx::query_scalar(
            r#"SELECT "Id" FROM "BaseItems"
               WHERE "Type" = 'MediaBrowser.Controller.Entities.Person'"#,
        )
        .fetch_all(db.pool())
        .await
        .expect("person items");
        assert_eq!(
            rows,
            vec![target.clone()],
            "duplicates collapsed onto the derived id"
        );
        // The favorite followed the survivor.
        let fav: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "UserData" WHERE "ItemId" = ?1 AND "IsFavorite" = 1"#,
        )
        .bind(&target)
        .fetch_one(db.pool())
        .await
        .expect("fav");
        assert_eq!(fav, 1);
        // Idempotent: the marker short-circuits the second run.
        assert_eq!(repo.unify_person_identities().await.expect("again"), 0);
    }

    #[tokio::test]
    async fn update_and_get_people_for_item_in_credit_order() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = FerrofinPeopleRepository::new(db);

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
    async fn update_people_materializes_person_items_and_returns_image_refs() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = FerrofinPeopleRepository::new(db.clone());

        let with_image = PeopleEntity {
            id: String::new(),
            name: "Zendaya".to_owned(),
            person_type: Some("Actor".to_owned()),
            role: Some("Chani".to_owned()),
            primary_image_url: Some("https://img/zendaya.jpg".to_owned()),
            provider_id: Some(505_710),
        };
        let refs = repo
            .update_people(item, &[with_image, person("No Photo", "Director")])
            .await
            .expect("update");

        // Both credits come back needing details; the one with a URL/provider id
        // carries them.
        assert_eq!(refs.len(), 2);
        let zendaya = refs.iter().find(|w| w.image_url.is_some()).expect("z");
        assert_eq!(
            zendaya.image_url.as_deref(),
            Some("https://img/zendaya.jpg")
        );
        assert_eq!(zendaya.provider_id, Some(505_710));
        assert!(zendaya.needs_details);

        // A browsable Person BaseItems row exists (id = the Peoples id), so the
        // person page / image endpoint can resolve it.
        let (base_type, base_name): (String, String) =
            sqlx::query_as(r#"SELECT bi."Type", bi."Name" FROM "BaseItems" bi WHERE bi."Id" = ?1"#)
                .bind(guid_to_db(zendaya.id))
                .fetch_one(db.pool())
                .await
                .expect("person base item");
        assert!(base_type.ends_with(".Person"));
        assert_eq!(base_name, "Zendaya");

        // The credited role is persisted on the map.
        let role: String = sqlx::query_scalar(
            r#"SELECT "Role" FROM "PeopleBaseItemMap" WHERE "ItemId" = ?1 AND "Role" <> '' LIMIT 1"#,
        )
        .bind(guid_to_db(item))
        .fetch_one(db.pool())
        .await
        .expect("role");
        assert_eq!(role, "Chani");

        // Once the person has a biography, re-crediting reports needs_details=false
        // so the scan won't re-fetch it.
        repo.set_person_metadata(
            zendaya.id,
            PersonMetadata {
                overview: Some("An actor.".to_owned()),
                ..PersonMetadata::default()
            },
        )
        .await
        .expect("set metadata");
        let item2 = Uuid::new_v4();
        seed_item(&db, item2, BaseItemKind::Movie).await;
        let again = repo
            .update_people(item2, &[person("Zendaya", "Actor")])
            .await
            .expect("re-update");
        assert!(!again[0].needs_details);
    }

    #[tokio::test]
    async fn person_type_filter_and_names() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = FerrofinPeopleRepository::new(db);
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
        let repo = FerrofinPeopleRepository::new(db);
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
        let repo = FerrofinPeopleRepository::new(db);
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

    #[tokio::test]
    async fn people_total_survives_offset_past_end() {
        let db = test_db().await;
        let item_a = Uuid::from_u128(0x9001);
        seed_item(&db, item_a, BaseItemKind::Movie).await;
        let repo = FerrofinPeopleRepository::new(db);
        repo.update_people(
            item_a,
            &[
                person("Alice", "Actor"),
                person("Bob", "Actor"),
                person("Cara", "Actor"),
            ],
        )
        .await
        .expect("a");

        let past = InternalPeopleQuery {
            limit: 2,
            start_index: Some(10),
            ..Default::default()
        };
        let r = repo.get_people(&past).await.expect("past");
        assert!(r.items.is_empty());
        assert_eq!(
            r.total_record_count, 3,
            "total must survive an offset past the end"
        );

        let first = InternalPeopleQuery {
            limit: 2,
            start_index: Some(0),
            ..Default::default()
        };
        let f = repo.get_people(&first).await.expect("first");
        assert_eq!(f.items.len(), 2);
        assert_eq!(f.total_record_count, 3);

        let empty = InternalPeopleQuery {
            limit: 2,
            name_contains: Some("zzzz".to_owned()),
            ..Default::default()
        };
        let none = repo.get_people(&empty).await.expect("none");
        assert_eq!(none.total_record_count, 0, "genuine empty is 0");
    }
}
