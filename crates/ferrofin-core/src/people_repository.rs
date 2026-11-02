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
//! - otherwise collapse to one representative row per lower-cased name, the
//!   lowest id among the rows every predicate keeps (v12.2 dedups the filtered
//!   candidates), ordered by name;
//! - `parent_id`, `person_types`/`exclude_person_types` (alphanumeric only),
//!   `max_list_order`, and the name range/substring predicates are applied as in
//!   C#. `is_favorite` keeps the names of the Person items the user has a
//!   `UserData` row for with that flag, and the `access_filter` keeps only
//!   people credited on an item the user can see.

use ferrofin_util::string_extensions::{lower_invariant, upper_invariant};

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::PeopleEntity;
use ferrofin_db::store::{guid_to_db, opt_datetime_to_db};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::querying::QueryResult;
use sqlx::QueryBuilder;

/// A `Person` row's `SortName`.
///
/// `Person` is the only 10.11.8 entity that overrides
/// `EnableAlphaNumericSorting => false` (`MediaBrowser.Controller/Entities/
/// Person.cs`), which sends `BaseItem.CreateSortName` down its first branch:
/// `return Name.TrimStart();` — the name verbatim, NOT the lowercased,
/// article-stripped, number-padded alphanumeric key every other kind gets. A
/// person stored as `alice parity` instead of `Alice Parity` sorts and searches
/// in a different place than Jellyfin puts it.
fn person_sort_name(name: &str) -> String {
    crate::kinds::sort_name_for(BaseItemKind::Person, name)
}
use sqlx::Sqlite;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::InternalPeopleQuery;
use ferrofin_traits::persistence::{PeopleRepository, PersonMetadata, WrittenPerson};

use crate::db_error::db_err;
use crate::item_type_lookup;
use crate::item_type_lookup::stored_type_name;

/// `PeopleEntity` + the `COUNT(*) OVER()` total, so a single query returns
/// both the page and the pagination total without a separate round-trip.
#[derive(sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
struct PeopleWithCount {
    id: String,
    name: String,
    person_type: Option<String>,
    total_count: i64,
}

/// The stored `Type` name of a `Person` item, used by the `is_favorite`
/// user-data set (C# `itemTypeLookup.BaseItemKindNames[Person]`).
const PERSON_TYPE_NAME: &str = "MediaBrowser.Controller.Entities.Person";

/// A stored `Person` row, as [`partition_credits_by_person_id`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredPerson {
    /// The item id.
    pub(crate) id: Uuid,
    /// `BaseItems.Name`, which a refresh may have changed.
    pub(crate) name: Option<String>,
    /// `BaseItems.CleanName`.
    pub(crate) clean_name: Option<String>,
    /// `BaseItems.IsLocked`.
    pub(crate) is_locked: bool,
}

/// What the Refresh People task has to do to the `Person` items
/// ([`partition_credits_by_person_id`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct PersonPartition {
    /// The credit names that need an item, one per id.
    pub(crate) new_names: Vec<String>,
    /// The unlocked items nothing credits.
    pub(crate) dead: Vec<StoredPerson>,
    /// How many items nothing credits are kept because they are locked.
    pub(crate) locked: usize,
}

/// `PeopleValidator.PartitionCreditsByPersonId`: splits the person items into
/// the ones a credit still calls for and the ones nothing does, keyed on the
/// id a credit name derives (`person_ids`, the current id first), never on the
/// name the item carries now: a provider may have renamed it. Keying on the
/// name put upstream's two halves in a loop that created, refreshed and
/// deleted the same people on every run.
///
/// Upstream's rule alone cannot be applied here, because Ferrofin's identity
/// has more paths to a row. So a row also lives, and a credit needs no item,
/// where:
///
/// - the id is the older derivation the write path keeps
///   ([`resolved_person_item_id`](FerrofinPeopleRepository::resolved_person_item_id));
/// - the row carries the credit's exact name. That keeps a database whose
///   stored ids are not the derived ones (a metadata path moved outside the
///   data dir, a changed id setting) from reading every person as dead and
///   new at once, and leaves what this deletes a subset of what the name-keyed
///   sweep it replaced deleted;
/// - the row is the one a credit's clean name resolves to
///   ([`person_item_by_clean_name`]: the first by `SortName`, `Id`). The write
///   path and every person read path treat spellings sharing a clean name as
///   one person on that row. Upstream gives each spelling its own item, and
///   `GetPerson` reads it by id; here a second row would be shown for the
///   first by those paths, so no spelling gets one until they read by id
///   (`MustHaveItem` still compares the exact name, so a spelling nobody else
///   carries stays out of `/Persons`).
///
/// `stored` is in `SortName`, `Id` order. A blank credit needs no item and keeps
/// only a row as blank as itself, which the name-keyed sweep kept too.
fn partition_credits_by_person_id(
    credit_names: &[String],
    person_ids: impl Fn(&str) -> Vec<Uuid>,
    stored: &[StoredPerson],
) -> PersonPartition {
    let stored_ids: HashSet<Uuid> = stored.iter().map(|person| person.id).collect();
    let stored_names: HashSet<&str> = stored
        .iter()
        .filter_map(|person| person.name.as_deref().map(str::trim))
        .collect();
    let mut by_clean_name: HashMap<&str, Uuid> = HashMap::new();
    for person in stored {
        if let Some(clean_name) = person.clean_name.as_deref() {
            by_clean_name.entry(clean_name).or_insert(person.id);
        }
    }

    let mut new_names = Vec::new();
    let mut new_clean_names: HashSet<String> = HashSet::new();
    let mut credited: HashSet<&str> = HashSet::new();
    let mut live_ids: HashSet<Uuid> = HashSet::new();
    for credit in credit_names {
        let name = credit.trim();
        credited.insert(name);
        if name.is_empty() {
            continue;
        }
        let clean_name = crate::text_util::get_clean_value(name);
        let resolved = by_clean_name.get(clean_name.as_str()).copied();
        live_ids.extend(resolved);
        let ids = person_ids(name);
        // Distinct credit names can normalize onto one id; only the first of
        // them needs an item.
        let first_of_its_id = ids.first().is_some_and(|id| !live_ids.contains(id));
        let has_item = resolved.is_some()
            || stored_names.contains(name)
            || ids.iter().any(|id| stored_ids.contains(id));
        live_ids.extend(ids);
        // A spelling sharing a clean name with a credit just listed resolves to
        // the item that credit gets.
        if first_of_its_id && !has_item && new_clean_names.insert(clean_name) {
            new_names.push(name.to_owned());
        }
    }

    let (locked, dead): (Vec<_>, Vec<_>) = stored
        .iter()
        .filter(|person| {
            !live_ids.contains(&person.id)
                && !person
                    .name
                    .as_deref()
                    .is_some_and(|name| credited.contains(name.trim()))
        })
        .cloned()
        .partition(|person| person.is_locked);
    PersonPartition {
        new_names,
        dead,
        locked: locked.len(),
    }
}

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

    /// Keep a previous Ferrofin ID when it exists. No primary key or reference
    /// is rewritten merely because the Unicode mapping has been corrected.
    async fn resolved_person_item_id(
        &self,
        tx: &mut sqlx::Transaction<'_, Sqlite>,
        name: &str,
    ) -> Result<Option<String>, ServiceError> {
        let Some(current) = self.person_item_id(name) else {
            return Ok(None);
        };
        let Some((mode, root)) = self.identity.as_ref() else {
            return Ok(None);
        };
        let path = item_type_lookup::person_path(root, name);
        let previous =
            item_type_lookup::previous_by_name_item_id(mode, BaseItemKind::Person, &path)
                .map(guid_to_db);
        if let Some(previous) = previous.filter(|previous| previous != &current) {
            let exists: bool = sqlx::query_scalar(
                r#"SELECT EXISTS(SELECT 1 FROM "BaseItems" WHERE "Id" = ?1 AND "Type" = ?2)"#,
            )
            .bind(&previous)
            .bind(PERSON_TYPE_NAME)
            .fetch_one(&mut **tx)
            .await
            .map_err(db_err)?;
            if exists {
                return Ok(Some(previous));
            }
        }
        Ok(Some(current))
    }

    /// Reuse the library resolver's indexed clean-name match before minting
    /// an ID. A refresh may change the spelling, so an old hash alone is not
    /// sufficient to find every previously written item.
    async fn person_item_id_for_write(
        &self,
        tx: &mut sqlx::Transaction<'_, Sqlite>,
        name: &str,
        fallback: &str,
    ) -> Result<String, ServiceError> {
        if self.identity.is_some()
            && let Some(stored) = person_item_by_clean_name(&mut **tx, name).await?
        {
            return Ok(stored);
        }
        Ok(self
            .resolved_person_item_id(tx, name)
            .await?
            .unwrap_or_else(|| fallback.to_owned()))
    }

    /// The `Path` column of `name`'s `Person` row — `Person.GetPath(name)`,
    /// `{PeoplePath}/{first alphanumeric}/{valid filename}`.
    ///
    /// Jellyfin writes it when it creates the row
    /// (`LibraryManager.SavePeopleMetadataAsync`, v10.11.8 LibraryManager.cs:3105-3131
    /// and master:3556-3592 — the two bodies diff empty), and `DtoService` emits
    /// `Path` unconditionally, so a row without it is a `missing` field on
    /// `GET /Persons/{name}`. [`None`] when the identity seam is unwired (unit
    /// tests), which leaves the column NULL exactly as before.
    fn person_metadata_path(&self, name: &str) -> Option<String> {
        let (_, people_path) = self.identity.as_ref()?;
        Some(item_type_lookup::person_path(people_path, name))
    }

    /// Every id `name`'s credit may have an item under: the current derivation
    /// first, then the older one the write path keeps. Empty when the identity
    /// seam is unwired.
    fn person_ids(&self, name: &str) -> Vec<Uuid> {
        let Some((mode, root)) = self.identity.as_ref() else {
            return Vec::new();
        };
        let path = item_type_lookup::person_path(root, name);
        let current = item_type_lookup::person_item_id(mode, root, name);
        let previous =
            item_type_lookup::previous_by_name_item_id(mode, BaseItemKind::Person, &path);
        let mut ids: Vec<Uuid> = current.into_iter().chain(previous).collect();
        ids.dedup();
        ids
    }

    /// Reads what the Refresh People task has to do to the `Person` items
    /// ([`partition_credits_by_person_id`]).
    ///
    /// The rows are read before the credits: a scan commits a new person's row
    /// with its credit, so every row read here has its credit in the names read
    /// next, where the other order could read a person just added as dead. A
    /// scan that credits a row again after these reads can still lose that row
    /// to the deletion that follows them; the next run makes its item again.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] when a read fails.
    pub(crate) async fn partition_person_items(&self) -> Result<PersonPartition, ServiceError> {
        let rows: Vec<(String, Option<String>, Option<String>, bool)> = sqlx::query_as(
            r#"SELECT "Id", "Name", "CleanName", "IsLocked" FROM "BaseItems"
               WHERE "Type" = ?1 ORDER BY "SortName", "Id""#,
        )
        .bind(PERSON_TYPE_NAME)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        let stored: Vec<StoredPerson> = rows
            .into_iter()
            .filter_map(|(id, name, clean_name, is_locked)| {
                let Ok(parsed) = Uuid::parse_str(&id) else {
                    tracing::debug!(id, "skipping a person row whose id is not a guid");
                    return None;
                };
                Some(StoredPerson {
                    id: parsed,
                    name,
                    clean_name,
                    is_locked,
                })
            })
            .collect();
        let names: Vec<String> =
            sqlx::query_scalar(r#"SELECT DISTINCT "Name" FROM "Peoples" ORDER BY "Name""#)
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?;
        Ok(partition_credits_by_person_id(
            &names,
            |name| self.person_ids(name),
            &stored,
        ))
    }

    /// `LibraryManager.GetOrCreatePerson` for each of `names`, which had no item
    /// when they were read: the row
    /// [`insert_person_item`](Self::insert_person_item) writes, unrefreshed,
    /// and its metadata folder. Each name resolves as the write path resolves
    /// it, so a row a scan made in the meantime is reused. Each row commits on
    /// its own, as upstream's `CreateItem` does, so a long list never holds the
    /// writer. Returns how many rows were written.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] when a read or write fails.
    pub(crate) async fn create_person_items(
        &self,
        names: &[String],
    ) -> Result<usize, ServiceError> {
        if self.identity.is_none() {
            return Ok(0);
        }
        let mut created = 0;
        for name in names {
            let mut tx = self.db.writer().begin().await.map_err(db_err)?;
            let item_id = self.person_item_id_for_write(&mut tx, name, "").await?;
            let inserted = self
                .insert_person_item(&mut tx, PERSON_TYPE_NAME, name, &item_id)
                .await?;
            tx.commit().await.map_err(db_err)?;
            if inserted {
                self.create_person_directory(name).await;
                created += 1;
            }
        }
        Ok(created)
    }

    /// Collapses one duplicate `Person` row onto `target` inside `tx`:
    /// ensures the target exists, copies still-empty enrichment columns,
    /// repoints user data + images (clashes keep the target's rows), and
    /// drops the duplicate.
    async fn collapse_person_row(
        tx: &mut sqlx::Transaction<'_, Sqlite>,
        person_type: &str,
        name: &str,
        path: Option<&str>,
        old_id: &str,
        target: &str,
        create_target: bool,
    ) -> Result<(), ServiceError> {
        if create_target {
            let clean = crate::text_util::get_clean_value(name);
            sqlx::query(
                // `SortName` is written here, not derived on read. C#
                // `BaseItem.SortName` lazily computes `CreateSortName()` and is
                // never null for a named item, and `BaseItemRepository` persists
                // it on every save. Leaving it NULL makes `ORDER BY SortName` a
                // no-op for these rows AND makes `nameStartsWith` — which
                // filters `lower(SortName)`, faithfully to C# `ApplyNameFilters`
                // — match nothing at all.
                // `PresentationUniqueKey` too — `Person-{Name}` with the
                // diacritics removed, exactly as Jellyfin writes it. This
                // insert bypasses `upsert_item`, so nothing else would set it.
                r#"INSERT OR IGNORE INTO "BaseItems"
                   ("Id","Type","Name","CleanName","SortName","PresentationUniqueKey","Path",
                    "IsFolder","IsInMixedFolder",
                    "IsLocked","IsMovie","IsRepeat","IsSeries","IsVirtualItem")
                   VALUES (?1,?2,?3,?4,?5,?6,?7,0,0,0,0,0,0,0)"#,
            )
            .bind(target)
            .bind(person_type)
            .bind(name)
            .bind(&clean)
            .bind(person_sort_name(name))
            .bind(person_presentation_key(target, name))
            .bind(path)
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
    /// and images, and records completion in `FerrofinMeta` so subsequent boots
    /// skip it. On a database adopted from Jellyfin (already one id per name)
    /// every group resolves to its existing row and nothing changes.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] when the rewrite transaction fails; the
    /// marker is only written after a successful pass.
    pub async fn unify_person_identities(&self) -> Result<u64, ServiceError> {
        // `_v2`: the first pass hashed the case-sensitive path; Jellyfin's
        // by-name ids are case-normalized, so the pass re-runs once to
        // collapse onto the ids an adopted database already carries.
        const META_KEY: &str = "person_identity_unified_v2";
        if self.identity.is_none() {
            return Ok(0);
        }
        let done: Option<String> =
            sqlx::query_scalar(r#"SELECT "Value" FROM "FerrofinMeta" WHERE "Key" = ?1 LIMIT 1"#)
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
            let Some(target) = self.resolved_person_item_id(&mut tx, name).await? else {
                continue;
            };
            if old_id.eq_ignore_ascii_case(&target) {
                seen_targets.insert(target);
                continue;
            }
            // The target row must exist BEFORE any child rows repoint at it
            // (FK BaseItems.Id) — collapse_person_row handles the ordering.
            let create_target = seen_targets.insert(target.clone());
            let path = self.person_metadata_path(name);
            Self::collapse_person_row(
                &mut tx,
                person_type,
                name,
                path.as_deref(),
                &old_id,
                &target,
                create_target,
            )
            .await?;
            collapsed += 1;
        }
        sqlx::query(
            r#"INSERT INTO "FerrofinMeta" ("Key", "Value") VALUES (?1, '1')
               ON CONFLICT("Key") DO UPDATE SET "Value" = '1'"#,
        )
        .bind(META_KEY)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(collapsed)
    }

    /// Materializes the browsable `Person` item row for `name` at `item_id`.
    ///
    /// Port of the row C# `LibraryManager.SavePeopleMetadataAsync` creates
    /// (v10.11.8 `LibraryManager.cs:3105-3131`; master's body at `:3556-3592`
    /// diffs empty against it): `Path = Person.GetPath(name)`, the derived id,
    /// and `PresentationUniqueKey = CreatePresentationUniqueKey()`.
    ///
    /// `SortName` is persisted rather than derived on read — it is what
    /// `ORDER BY SortName` and `nameStartsWith` (which filters
    /// `lower(SortName)`, faithfully to `ApplyNameFilters`) actually read — and
    /// it uses the `Person` branch of `CreateSortName`, the verbatim name.
    /// Returns whether a row was written: `false` when `item_id` has one.
    async fn insert_person_item(
        &self,
        tx: &mut sqlx::Transaction<'_, Sqlite>,
        type_name: &str,
        name: &str,
        item_id: &str,
    ) -> Result<bool, ServiceError> {
        let inserted = sqlx::query(
            r#"INSERT OR IGNORE INTO "BaseItems"
               ("Id","Type","Name","CleanName","SortName","PresentationUniqueKey","Path",
                "IsFolder","IsInMixedFolder",
                "IsLocked","IsMovie","IsRepeat","IsSeries","IsVirtualItem")
               VALUES (?1,?2,?3,?4,?5,?6,?7,0,0,0,0,0,0,0)"#,
        )
        .bind(item_id)
        .bind(type_name)
        .bind(name)
        .bind(crate::text_util::get_clean_value(name))
        .bind(person_sort_name(name))
        .bind(person_presentation_key(item_id, name))
        .bind(self.person_metadata_path(name))
        .execute(&mut **tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        Ok(inserted > 0)
    }

    /// Repairs `Person` rows written before their columns were complete:
    /// a NULL `Path`, a NULL `PresentationUniqueKey`, or a `SortName` that is
    /// not the verbatim name.
    ///
    /// Ferrofin needs this and Jellyfin does not: C# recomputes `SortName`,
    /// `PresentationUniqueKey` and `Path` on every `SaveItems`, so a row heals
    /// itself on the next refresh, whereas both of Ferrofin's `Person` inserts
    /// are `INSERT OR IGNORE` and nothing rewrites an existing row. Without
    /// this pass, every install whose people were scanned before those columns
    /// were written stays wrong forever, and an upgrade does not fix it.
    ///
    /// Idempotent, and a no-op on a database adopted from Jellyfin: Jellyfin
    /// stores exactly `Name.TrimStart()` in `SortName`, `Person-{Name}` (with
    /// diacritics removed) in `PresentationUniqueKey`, and the same
    /// `{PeoplePath}/{A}/{Name}` in `Path`, so all three predicates are already
    /// false there and the round trip is untouched. Returns the number of rows
    /// rewritten.
    ///
    /// A row carrying a non-empty `ForcedSortName` keeps its `SortName`
    /// untouched. That column is the USER'S sort-title override, and C# derives
    /// `SortName` from it instead of `CreateSortName` (12.0 `BaseItem.cs:549`:
    /// `GetSortName(ForcedSortName, EnableAlphaNumericSorting, config)`, which
    /// for a `Person` is `ForcedSortName.TrimStart()`), so rewriting it to the
    /// verbatim name would both destroy the override and — because the pass
    /// would never converge on it — re-fire on every boot. Moving a forced key
    /// itself between generations is
    /// `FerrofinItemPersistenceService::repair_forced_sort_names`'s job. Migration `0012_hermit_episode_sort_names.sql`
    /// guards the same way (`coalesce("ForcedSortName", '') = ''`), as does
    /// `upsert_item`; this pass is the third path over the same rule and must
    /// not be the one that disagrees. Its `Path`/`PresentationUniqueKey` halves
    /// still run for such a row: those are derived columns no user owns.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] when the read or the rewrite fails.
    pub async fn repair_person_items(&self) -> Result<usize, ServiceError> {
        if self.identity.is_none() {
            return Ok(0);
        }
        let Some(person_type) = stored_type_name(BaseItemKind::Person) else {
            return Ok(0);
        };
        let rows: Vec<PersonRepairRow> = sqlx::query_as(
            r#"SELECT "Id", "Name", "SortName", "Path", "PresentationUniqueKey",
                      coalesce("ForcedSortName", '')
                   FROM "BaseItems" WHERE "Type" = ?1"#,
        )
        .bind(person_type)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;

        let mut pending: Vec<PersonRepairWrite> = Vec::new();
        for (id, name, sort_name, path, key, forced) in rows {
            let Some(name) = name.as_deref().map(str::trim).filter(|n| !n.is_empty()) else {
                continue;
            };
            let want_sort = person_sort_name(name);
            let want_path = self.person_metadata_path(name);
            let want_key = Uuid::parse_str(&id).ok().map(|pid| {
                crate::kinds::presentation_unique_key(
                    BaseItemKind::Person,
                    pid,
                    Some(name),
                    None,
                    None,
                    None,
                )
            });
            let new_path = want_path.filter(|_| path.is_none());
            let new_key = want_key.filter(|_| key.is_none());
            // `ForcedSortName` wins over the derived name — see the doc above.
            let new_sort = forced
                .trim()
                .is_empty()
                .then_some(want_sort)
                .filter(|want| sort_name.as_deref() != Some(want.as_str()));
            if new_sort.is_none() && new_path.is_none() && new_key.is_none() {
                continue;
            }
            pending.push((id, new_sort, new_path, new_key));
        }
        if pending.is_empty() {
            return Ok(0);
        }

        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        for (id, sort_name, path, key) in &pending {
            sqlx::query(
                r#"UPDATE "BaseItems"
                   SET "SortName" = COALESCE(?2, "SortName"),
                       "Path" = COALESCE("Path", ?3),
                       "PresentationUniqueKey" = COALESCE("PresentationUniqueKey", ?4)
                   WHERE "Id" = ?1"#,
            )
            .bind(id)
            .bind(sort_name)
            .bind(path)
            .bind(key)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)?;
        for (_, _, path, _) in &pending {
            if let Some(path) = path
                && let Err(e) = tokio::fs::create_dir_all(path).await
            {
                tracing::debug!(%path, error = %e, "could not create person metadata directory");
            }
        }
        Ok(pending.len())
    }
}

/// One rewrite [`FerrofinPeopleRepository::repair_person_items`] has decided on:
/// `(Id, new SortName or None, new Path or None, new PresentationUniqueKey or
/// None)`. Each column is `None` when the row's current value is already right
/// — or, for `SortName`, when the row carries a `ForcedSortName` the pass must
/// not touch.
type PersonRepairWrite = (String, Option<String>, Option<String>, Option<String>);

/// One `Person` row as [`FerrofinPeopleRepository::repair_person_items`] reads
/// it: `(Id, Name, SortName, Path, PresentationUniqueKey, ForcedSortName)`.
///
/// `ForcedSortName` is coalesced to `''` in the SELECT, so an absent override
/// and an empty one are the same value here — the one the pass must not
/// overwrite is the non-empty one.
type PersonRepairRow = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
);

/// Dedupes credits case-insensitively by `(name, person_type, role)`, matching
/// v12.2 `UpdatePeople`'s `DistinctBy((LoweredName, PersonType, LoweredRole))`
/// over trimmed names and roles (jellyfin/jellyfin#17466: one person can hold
/// several credits of one type, a Writer for the Novel and the Screenplay).
/// Keeps first-seen order and casing; the order becomes the credit `ListOrder`.
pub(crate) fn dedupe_people(people: &[PeopleEntity]) -> Vec<&PeopleEntity> {
    let mut seen = HashSet::new();
    people
        .iter()
        .filter(|person| seen.insert((person_key(person), credit_role(person).to_lowercase())))
        .collect()
}

/// A credit's person: `(lower-cased trimmed name, person type)`, the key of
/// one `Peoples` row.
fn person_key(person: &PeopleEntity) -> (String, String) {
    (
        person.name.trim().to_lowercase(),
        person.person_type.clone().unwrap_or_default(),
    )
}

/// A credit's role as `UpdatePeople` stores it: trimmed, `""` when missing.
fn credit_role(person: &PeopleEntity) -> &str {
    person.role.as_deref().map(str::trim).unwrap_or_default()
}

/// A credit's identity: its person key (see `person_key`) and lower-cased
/// role, as [`dedupe_people`] keys a credit.
type CreditKey = ((String, String), String);

/// One credit row an item already maps.
struct StoredCredit {
    /// The credit's [`CreditKey`].
    key: CreditKey,
    /// The role in its stored casing.
    role: String,
    /// The `Peoples` row the credit maps to.
    people_id: String,
    list_order: Option<i64>,
    sort_order: Option<i64>,
}

/// v12.2 `UpdatePeople`'s `mappingsAreUnchanged`: as many stored credits as
/// incoming ones, and every stored credit matches an incoming one by
/// (lower name, person type, lower role) with the same `ListOrder` (its
/// position in `credits`) and the same `SortOrder`.
fn credits_unchanged(stored: &[StoredCredit], credits: &[&PeopleEntity]) -> bool {
    if stored.len() != credits.len() {
        return false;
    }
    let incoming: HashMap<CreditKey, (i64, Option<i64>)> = credits
        .iter()
        .enumerate()
        .map(|(list_order, person)| {
            (
                (person_key(person), credit_role(person).to_lowercase()),
                (
                    i64::try_from(list_order).unwrap_or(i64::MAX),
                    person.sort_order,
                ),
            )
        })
        .collect();
    stored.iter().all(|credit| {
        incoming
            .get(&credit.key)
            .is_some_and(|&(list_order, sort_order)| {
                credit.list_order == Some(list_order) && credit.sort_order == sort_order
            })
    })
}

/// The `Person` item already stored under `name`'s clean name, the match the
/// library resolver uses.
async fn person_item_by_clean_name<'e>(
    executor: impl sqlx::Executor<'e, Database = Sqlite>,
    name: &str,
) -> Result<Option<String>, ServiceError> {
    sqlx::query_scalar(
        r#"SELECT "Id" FROM "BaseItems" WHERE "Type" = ?1 AND "CleanName" = ?2
           ORDER BY "SortName", "Id" LIMIT 1"#,
    )
    .bind(PERSON_TYPE_NAME)
    .bind(crate::text_util::get_clean_value(name))
    .fetch_optional(executor)
    .await
    .map_err(db_err)
}

/// A person item's `Overview`: `None` when the item does not exist,
/// `Some(None)` when it has no biography.
async fn person_overview<'e>(
    executor: impl sqlx::Executor<'e, Database = Sqlite>,
    item_id: &str,
) -> Result<Option<Option<String>>, ServiceError> {
    sqlx::query_scalar(r#"SELECT "Overview" FROM "BaseItems" WHERE "Id" = ?1"#)
        .bind(item_id)
        .fetch_optional(executor)
        .await
        .map_err(db_err)
}

/// What `update_people` hands back for one credited person: the item, whether
/// it still lacks a biography (new people and those scanned before
/// biographies existed get one fetched), and the credit's artwork and
/// provider id.
fn written_person(
    item_id: &str,
    person: &PeopleEntity,
    overview: Option<&str>,
) -> Option<WrittenPerson> {
    Some(WrittenPerson {
        id: Uuid::parse_str(item_id).ok()?,
        needs_details: overview.is_none_or(str::is_empty),
        image_url: person
            .primary_image_url
            .as_deref()
            .filter(|u| !u.is_empty())
            .map(str::to_owned),
        provider_id: person.provider_id.filter(|id| *id > 0),
    })
}

/// Inserts `item_id`'s credit rows, one per credit, in credit order, each
/// `credits[i]` mapped to the `Peoples` row `people_ids[i]`.
async fn insert_credit_rows(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    item_id: Uuid,
    people_ids: &[String],
    credits: &[&PeopleEntity],
    stored_roles: &HashMap<CreditKey, String>,
) -> Result<(), ServiceError> {
    for (list_order, (people_id, person)) in people_ids.iter().zip(credits).enumerate() {
        // `person.Role = person.Role?.Trim() ?? string.Empty`
        // (v12.2 `PeopleRepository.cs:117`), in its stored casing.
        let role = credit_role(person);
        let role = stored_roles
            .get(&(person_key(person), role.to_lowercase()))
            .map_or(role, String::as_str);
        sqlx::query(
            r#"INSERT INTO "PeopleBaseItemMap"
               ("ItemId", "PeopleId", "Role", "ListOrder", "SortOrder")
               VALUES (?1, ?2, ?3, ?4, ?5)"#,
        )
        .bind(guid_to_db(item_id))
        .bind(people_id)
        .bind(role)
        .bind(i64::try_from(list_order).unwrap_or(i64::MAX))
        // `SortOrder = credit.Person.SortOrder` (`PeopleRepository.cs:
        // 146,218,231`): the credit's own order (TMDB billing, NFO
        // `<sortorder>`), NULL when it has none — not the list position.
        .bind(person.sort_order)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;
    }
    Ok(())
}

/// Whether a person-type filter value is valid (C# `IsValidPersonType` =
/// alphanumeric, non-blank). Non-alphanumeric filter values are ignored rather
/// than injected into the query.
fn is_valid_person_type(value: &str) -> bool {
    !value.trim().is_empty() && value.chars().all(char::is_alphanumeric)
}

/// Appends the shared `TranslateQuery` predicates (favorite, item scope,
/// parent scope, appears-in item, person-type in/out, max list order, name
/// range/substring, must-have-item, access filter) on the `Peoples` row aliased
/// `alias`, onto a builder with a `WHERE` clause already open. The by-name dedup applies them twice: to the
/// candidate rows its representative is chosen from (`d`) and to the outer row
/// (`p`).
#[allow(clippy::too_many_lines)] // one arm per C# `TranslateQuery` predicate
fn push_predicates(qb: &mut QueryBuilder<Sqlite>, filter: &InternalPeopleQuery, alias: &str) {
    // C# `IsFavorite` (v12.2 `PeopleRepository.cs:402-416`): the names of the
    // `Person` items the user has a row for with that flag, as one
    // uncorrelated set (`favoriteNames.Contains(e.Name)`). SQLite builds it
    // once per statement and probes it per row. As a correlated `EXISTS` it
    // seeded every probe from the user's whole favourite (or non-favourite)
    // set and joined back by name, for every `Peoples` row and again for every
    // dedup candidate: 1.4 s for an empty page on the gl-637 corpus, past 10 s
    // for `false`.
    if let (Some(user_id), Some(is_favorite)) = (filter.user_id, filter.is_favorite) {
        qb.push(format!(
            r#" AND {alias}."Name" IN (SELECT bi."Name" FROM "BaseItems" bi
                WHERE bi."Type" = "#
        ));
        qb.push_bind(PERSON_TYPE_NAME);
        qb.push(r#" AND bi."Id" IN (SELECT ud."ItemId" FROM "UserData" ud WHERE ud."UserId" = "#);
        qb.push_bind(guid_to_db(user_id));
        qb.push(r#" AND ud."IsFavorite" = "#);
        qb.push_bind(i64::from(is_favorite));
        qb.push("))");
    }
    if !filter.item_id.is_nil() {
        qb.push(format!(
            r#" AND EXISTS (SELECT 1 FROM "PeopleBaseItemMap" mx WHERE mx."PeopleId" = {alias}."Id" AND mx."ItemId" = "#
        ));
        qb.push_bind(guid_to_db(filter.item_id));
        qb.push(")");
    }
    if let Some(parent) = filter.parent_id {
        qb.push(format!(
            r#" AND EXISTS (SELECT 1 FROM "PeopleBaseItemMap" mp
                JOIN "AncestorIds" a ON a."ItemId" = mp."ItemId"
                WHERE mp."PeopleId" = {alias}."Id" AND a."ParentItemId" = "#
        ));
        qb.push_bind(guid_to_db(parent));
        qb.push(")");
    }
    if !filter.appears_in_item_id.is_nil() {
        qb.push(" AND ");
        qb.push(appears_in_item(filter.appears_in_item_id, alias));
    }
    let include: Vec<&String> = filter
        .person_types
        .iter()
        .filter(|t| is_valid_person_type(t))
        .collect();
    if !include.is_empty() {
        qb.push(format!(r#" AND {alias}."PersonType" IN ("#));
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
        qb.push(format!(
            r#" AND ({alias}."PersonType" IS NULL OR {alias}."PersonType" NOT IN ("#
        ));
        let mut sep = qb.separated(", ");
        for t in exclude {
            sep.push_bind(t.clone());
        }
        qb.push("))");
    }
    if let Some(max_order) = filter.max_list_order
        && !filter.item_id.is_nil()
    {
        qb.push(format!(
            r#" AND EXISTS (SELECT 1 FROM "PeopleBaseItemMap" mo WHERE mo."PeopleId" = {alias}."Id" AND mo."ItemId" = "#
        ));
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
        qb.push(format!(
            r#" AND ferrofin_upper_invariant({alias}."Name") LIKE "#
        ));
        qb.push_bind(format!("%{}%", like_literal(&upper_invariant(contains))));
        qb.push(r" ESCAPE '\'");
    }
    if let Some(prefix) = filter
        .name_starts_with
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    {
        qb.push(format!(
            r#" AND ferrofin_upper_invariant({alias}."Name") LIKE "#
        ));
        qb.push_bind(format!("{}%", like_literal(&upper_invariant(prefix))));
        qb.push(r" ESCAPE '\'");
    }
    if let Some(less) = filter
        .name_less_than
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    {
        qb.push(format!(
            r#" AND ferrofin_lower_invariant({alias}."Name") < "#
        ));
        qb.push_bind(lower_invariant(less));
    }
    if let Some(ge) = filter
        .name_starts_with_or_greater
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    {
        qb.push(format!(
            r#" AND ferrofin_lower_invariant({alias}."Name") >= "#
        ));
        qb.push_bind(lower_invariant(ge));
    }
    if filter.must_have_item {
        qb.push(" AND ");
        qb.push(has_person_item(alias));
    }
    if filter.access_filter.is_some() {
        qb.push(" AND ").push(accessible_person(alias));
    }
}

/// Opens the statement. Under an access filter (C# `AccessFilter`) it starts
/// with `AccessiblePeople`: the people credited on an item the filter keeps — in
/// the user's libraries and within their parental and tag restrictions
/// (`ApplyAccessFiltering`). The filter sets `IncludeOwnedItems`, so a credit
/// on an alternate version or an extra counts.
///
/// Upstream tests each person with a correlated `Any()` over their credits. Run
/// for every `Peoples` row the total visits, that measured ~8x slower on the
/// bench library than building the set once, from the libraries' items
/// (`IX_BaseItems_TopParentId_Id`) to their credits (the map's primary key).
fn open_query(filter: &InternalPeopleQuery) -> QueryBuilder<Sqlite> {
    let mut qb = QueryBuilder::new("");
    let Some(access) = &filter.access_filter else {
        return qb;
    };
    debug_assert!(
        access.include_owned_items,
        "the people access filter always admits owned items"
    );
    qb.push(
        r#"WITH "AccessiblePeople" AS MATERIALIZED (SELECT acm."PeopleId" FROM "BaseItems" aci
            CROSS JOIN "PeopleBaseItemMap" acm ON acm."ItemId" = aci."Id" WHERE 1 = 1"#,
    );
    if !access.top_parent_ids.is_empty() {
        qb.push(" AND ");
        crate::translate_query::push_in_list(
            &mut qb,
            r#"aci."TopParentId""#,
            &crate::translate_query::to_guid_strings(&access.top_parent_ids),
        );
    }
    crate::translate_query::append_parental_restrictions(&mut qb, access, "aci");
    qb.push(") ");
    qb
}

/// The access filter on the `Peoples` row aliased `alias`: one of the
/// [`open_query`] statement's `AccessiblePeople`. The `+` keeps the planner
/// from driving the query off the set, which would lose the name index's order
/// and dedup every person before the `LIMIT`.
fn accessible_person(alias: &str) -> String {
    format!(r#"+{alias}."Id" IN "AccessiblePeople""#)
}

/// C# `MustHaveItem`: a `Person` item with exactly this credit's name exists
/// (`BaseItems.Any(b => b.Type == personType && b.Name == e.Name)`).
///
/// The `=` is BINARY, like EF's `==` on the uncollated column. The lookup seeks
/// `IX_BaseItems_Name` and reads the row to check `Type` (the `+` keeps the
/// planner off the `Type`-led indexes); the type literal is a constant, so
/// inlining it is injection-safe.
fn has_person_item(alias: &str) -> String {
    format!(
        r#"EXISTS (SELECT 1 FROM "BaseItems" b WHERE b."Name" = {alias}."Name" AND +b."Type" = '{PERSON_TYPE_NAME}')"#
    )
}

/// C# `AppearsInItemId`: the credit is on that item
/// (`PeopleBaseItemMap.Where(m => m.ItemId == appearsInItemId).Select(m =>
/// m.PeopleId).Contains(e.Id)`). As an `IN` list it drives the query from the
/// item's credits (an `ItemId` seek on the map's primary key, then a `Peoples`
/// lookup by id); a correlated `EXISTS` would scan every `Peoples` row and
/// probe the map per row. The id is a canonically formatted GUID, so inlining
/// it is injection-safe.
fn appears_in_item(item_id: Uuid, alias: &str) -> String {
    format!(
        r#"{alias}."Id" IN (SELECT ma."PeopleId" FROM "PeopleBaseItemMap" ma WHERE ma."ItemId" = '{}')"#,
        guid_to_db(item_id)
    )
}

/// Opens `SELECT <cols> FROM "Peoples" p WHERE 1 = 1` (after the access
/// filter's set, [`open_query`]), ready for [`push_predicates`].
fn base_query(cols: &str, filter: &InternalPeopleQuery) -> QueryBuilder<Sqlite> {
    let mut qb = open_query(filter);
    qb.push(format!(r#"SELECT {cols} FROM "Peoples" p WHERE 1 = 1"#));
    qb
}

/// Restricts a plain `"Peoples"` scan to one representative row per lower-cased
/// name: the lowest id among the rows that pass every predicate. This is
/// upstream's own formulation (v12.2 `PeopleRepository.GetPeople`), which dedups
/// the already-filtered candidates — `!candidates.Any(other => same name &&
/// other.Id < p.Id)` — so a `john doe` credit with no item must not hide the
/// `John Doe` one that has it, and an Actor row must not hide the same person's
/// Director row from `personTypes=Director`.
///
/// As a *predicate* rather than a derived table, the page can be driven from
/// `FerrofinIX_Peoples_NameNoCase_Cover` — which already supplies the
/// `ORDER BY p."Name" COLLATE NOCASE` — so the `LIMIT` stops the scan after a
/// page's worth of representatives. The derived table had to aggregate every
/// person and then sort all of them through a temp B-tree before the `LIMIT`
/// could discard 98% of the result: 1.26 ms → 0.31 ms per query on the bench
/// library (7.5k people, `limit=100`). The dedup test per candidate row is a
/// covering-index seek on `FerrofinIX_Peoples_NameNoCase_Cover`.
///
/// `NOT EXISTS … d."Id" < p."Id"` rather than `p."Id" = (SELECT MIN(d."Id") …)`:
/// the index check on `Id` runs before the per-row predicates and the probe
/// stops at the first lower passing row, where `MIN` would run every predicate
/// on every row of the name group. Callers emit it *after* the outer
/// predicates: SQLite evaluates correlated sub-selects in written order, so a
/// row that fails a predicate (`isFavorite`, a selective `parentId`) is
/// rejected before its name group is probed.
fn push_dedup_predicate(qb: &mut QueryBuilder<Sqlite>, filter: &InternalPeopleQuery) {
    qb.push(
        r#" AND NOT EXISTS (SELECT 1 FROM "Peoples" d
     WHERE d."Name" = p."Name" COLLATE NOCASE AND d."Id" < p."Id""#,
    );
    push_predicates(qb, filter, "d");
    qb.push(")");
}

/// The total for an **unnarrowed** by-name listing: the deduped set is then
/// exactly the distinct lower-cased names, so the total comes off
/// `FerrofinIX_Peoples_NameNoCase_Cover` as a plain index-only distinct count —
/// no `MIN("Id")` aggregation, no row materialization, and (being an
/// uncorrelated scalar sub-select) evaluated **once** per statement rather
/// than per row. Under `must_have_item` (every `/Persons` request) it counts
/// only the names with an item (upstream's
/// `candidates.Select(Name.ToLower).Distinct().Count()`), which is no longer
/// index-only: each `Peoples` row costs an `IX_BaseItems_Name` seek plus a
/// `BaseItems` row read to check `Type`, the same per-row probe upstream's
/// `MustHaveItem` pays. Under the access filter it counts only the names with
/// an accessible credit, a lookup in the set [`open_query`] built once.
fn distinct_names_count(filter: &InternalPeopleQuery) -> String {
    let conditions: Vec<String> = [
        filter.must_have_item.then(|| has_person_item("c")),
        filter
            .access_filter
            .is_some()
            .then(|| accessible_person("c")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if conditions.is_empty() {
        r#"(SELECT COUNT(DISTINCT "Name" COLLATE NOCASE) FROM "Peoples")"#.to_owned()
    } else {
        format!(
            r#"(SELECT COUNT(DISTINCT c."Name" COLLATE NOCASE) FROM "Peoples" c WHERE {})"#,
            conditions.join(" AND ")
        )
    }
}

/// The total column for a **narrowed** by-name listing: the deduped set depends
/// on the predicates, so the total can only be counted over the
/// already-filtered result. `COUNT(*) OVER()` costs one extra pass over the
/// *filtered* rows, which for a narrowed query is far cheaper than re-running
/// the dedup a second time (measured below).
const TOTAL_FILTERED_WINDOW: &str = r#"COUNT(*) OVER() AS "TotalCount""#;

/// Whether `filter` narrows the by-name listing at all — i.e. whether any
/// predicate that [`push_predicates`] can emit is active.
///
/// When nothing narrows it, the deduped set is the whole `Peoples` table
/// collapsed by lower-cased name, so its size is a distinct count that needs no
/// aggregate over the representative rows ([`distinct_names_count`]).
/// `max_list_order` is deliberately absent: it is only emitted for an
/// item-scoped query, which never reaches the by-name path. So are
/// `must_have_item` and the access filter: the distinct count applies them
/// itself, keeping the default `/Persons` page off the window function.
fn narrows_by_name(filter: &InternalPeopleQuery) -> bool {
    let name_bound = [
        &filter.name_contains,
        &filter.name_starts_with,
        &filter.name_less_than,
        &filter.name_starts_with_or_greater,
    ]
    .into_iter()
    .any(|v| v.as_ref().is_some_and(|s| !s.trim().is_empty()));

    !filter.item_id.is_nil()
        || filter.parent_id.is_some()
        || !filter.appears_in_item_id.is_nil()
        || (filter.user_id.is_some() && filter.is_favorite.is_some())
        || filter.person_types.iter().any(|t| is_valid_person_type(t))
        || filter
            .exclude_person_types
            .iter()
            .any(|t| is_valid_person_type(t))
        || name_bound
}

/// Builds the by-name page query: the deduped representatives, the predicates,
/// the total column chosen by [`narrows_by_name`], the name ordering, and the
/// page window.
///
/// Split out of [`FerrofinPeopleRepository::get_people_by_name`] so tests can
/// assert the emitted SQL and its `EXPLAIN QUERY PLAN` — the only way to pin
/// "this request does not pay for a second full-table aggregate", which no
/// response-body assertion can see.
fn by_name_page_query(filter: &InternalPeopleQuery) -> QueryBuilder<Sqlite> {
    let total = if narrows_by_name(filter) {
        TOTAL_FILTERED_WINDOW.to_owned()
    } else {
        format!(r#"{} AS "TotalCount""#, distinct_names_count(filter))
    };
    let mut qb = base_query(
        &format!(r#"p."Id", p."Name", p."PersonType", {total}"#),
        filter,
    );
    push_predicates(&mut qb, filter, "p");
    push_dedup_predicate(&mut qb, filter);
    // Upstream orders by `Name.ToLower()`, which EF sends to SQLite as
    // `lower("Name")`. In the e_sqlite3 Jellyfin bundles (no ICU) that folds
    // ASCII only before a byte comparison, the same order `NOCASE` gives on
    // every build — and `NOCASE` keeps the page on the covering index.
    qb.push(r#" ORDER BY p."Name" COLLATE NOCASE"#);
    let start = filter.start_index.unwrap_or(0);
    if filter.limit > 0 || start > 0 {
        qb.push(" LIMIT ");
        qb.push_bind(if filter.limit > 0 {
            i64::from(filter.limit)
        } else {
            -1
        });
        if start > 0 {
            qb.push(" OFFSET ");
            qb.push_bind(i64::from(start));
        }
    }
    qb
}

impl FerrofinPeopleRepository {
    /// `SelectMany(MapCredits)` (v12.2 `PeopleRepository.cs:84,360-368`): a
    /// page of `item_id`'s people, each expanded into one entry per credit it
    /// holds on the item, in `ListOrder` — a Writer of the Novel and the
    /// Screenplay is two entries. A person with no credit row stays as is.
    ///
    /// The role is read back VERBATIM: `UpdatePeople` stores a missing role as
    /// `string.Empty` and `Map` reads `Role = mapping?.Role` with no
    /// coalescing, so a roleless credit comes back as `""` and is serialized
    /// as `"Role": ""`. Folding `''` to NULL would drop the field off the wire.
    async fn credits_of(
        &self,
        item_id: Uuid,
        people: Vec<PeopleEntity>,
    ) -> Result<Vec<PeopleEntity>, ServiceError> {
        /// One credit row: its role and sort order.
        type Credit = (Option<String>, Option<i64>);
        let credits: Vec<(String, Option<String>, Option<i64>)> = sqlx::query_as(
            r#"SELECT "PeopleId", "Role", "SortOrder" FROM "PeopleBaseItemMap"
               WHERE "ItemId" = ?1 ORDER BY "ListOrder""#,
        )
        .bind(guid_to_db(item_id))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        let mut by_person: HashMap<String, Vec<Credit>> = HashMap::new();
        for (people_id, role, sort_order) in credits {
            by_person
                .entry(people_id)
                .or_default()
                .push((role, sort_order));
        }
        Ok(people
            .into_iter()
            .flat_map(|person| match by_person.remove(&person.id) {
                Some(credits) => credits
                    .into_iter()
                    .map(|(role, sort_order)| PeopleEntity {
                        role,
                        sort_order,
                        ..person.clone()
                    })
                    .collect(),
                None => vec![person],
            })
            .collect())
    }

    /// The credits `item_id` already maps, keyed like the incoming ones
    /// (person key and lower-cased role).
    ///
    /// Two readers: the unchanged check ([`credits_unchanged`]), and the write
    /// path, where a credit whose role differs from its stored one only in
    /// case keeps the stored casing, as upstream keeps the existing mapping
    /// (`UpdatePeople_RoleCaseChanged_KeepsExistingMapping`).
    async fn stored_credits(&self, item_id: Uuid) -> Result<Vec<StoredCredit>, ServiceError> {
        #[allow(clippy::type_complexity)] // one row of the read
        let stored: Vec<(
            String,
            Option<String>,
            Option<String>,
            String,
            Option<i64>,
            Option<i64>,
        )> = sqlx::query_as(
            r#"SELECT p."Name", p."PersonType", m."Role", m."PeopleId", m."ListOrder", m."SortOrder"
               FROM "PeopleBaseItemMap" m
               JOIN "Peoples" p ON p."Id" = m."PeopleId" WHERE m."ItemId" = ?1"#,
        )
        .bind(guid_to_db(item_id))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(stored
            .into_iter()
            .map(
                |(name, person_type, role, people_id, list_order, sort_order)| {
                    let role = role.unwrap_or_default();
                    StoredCredit {
                        key: (
                            (name.trim().to_lowercase(), person_type.unwrap_or_default()),
                            role.to_lowercase(),
                        ),
                        role,
                        people_id,
                        list_order,
                        sort_order,
                    }
                },
            )
            .collect())
    }

    /// The people an unchanged credit list names, in credit order, read
    /// without a write: each `Person` item the write path would resolve
    /// ([`person_item_id_for_write`](Self::person_item_id_for_write)) with its
    /// biography state. `None` when one of them has no item yet, which only
    /// the write path creates.
    async fn unchanged_people(
        &self,
        credits: &[&PeopleEntity],
        stored: &[StoredCredit],
    ) -> Result<Option<Vec<WrittenPerson>>, ServiceError> {
        // The unwired seam keys a person's item on its `Peoples` row.
        let people_ids: HashMap<&(String, String), &str> = stored
            .iter()
            .map(|credit| (&credit.key.0, credit.people_id.as_str()))
            .collect();
        let mut written = Vec::with_capacity(credits.len());
        let mut seen = HashSet::new();
        for person in credits {
            let key = person_key(person);
            if !seen.insert(key.clone()) {
                continue;
            }
            let item_id = if self.identity.is_some() {
                person_item_by_clean_name(self.db.pool(), person.name.trim()).await?
            } else {
                people_ids.get(&key).map(|id| (*id).to_owned())
            };
            let Some(item_id) = item_id else {
                return Ok(None);
            };
            let Some(overview) = person_overview(self.db.pool(), &item_id).await? else {
                return Ok(None);
            };
            written.extend(written_person(&item_id, person, overview.as_deref()));
        }
        Ok(Some(written))
    }

    /// `Directory.CreateDirectory(path)` for each credited person of a written
    /// update: C# creates the person's metadata folder as part of writing the
    /// row (`SavePeopleMetadataAsync`), and a real 10.11.8 install has
    /// `/config/metadata/People/A/Alice Parity` on disk. Made after the
    /// commit, the ordering `by_name_store::ensure` uses, so a filesystem
    /// failure can never be the reason a person lookup 404s. Best-effort.
    async fn create_person_directories(&self, credits: &[&PeopleEntity]) {
        let mut seen = HashSet::new();
        for person in credits {
            if !seen.insert(person_key(person)) {
                continue;
            }
            self.create_person_directory(person.name.trim()).await;
        }
    }

    /// `Directory.CreateDirectory(path)` for one person. Best-effort.
    async fn create_person_directory(&self, name: &str) {
        let Some(path) = self.person_metadata_path(name) else {
            return;
        };
        if let Err(e) = tokio::fs::create_dir_all(&path).await {
            tracing::debug!(%path, error = %e, "could not create person metadata directory");
        }
    }

    /// Fallback total when the by-name page is empty (past the last row).
    ///
    /// Uses the same two shapes as the page query, for the same reason: an
    /// unnarrowed count is a distinct count off the covering index, not an
    /// aggregate over the representative rows.
    async fn count_people_total(&self, filter: &InternalPeopleQuery) -> Result<i64, ServiceError> {
        if !narrows_by_name(filter) {
            let mut qb = open_query(filter);
            qb.push(format!("SELECT {}", distinct_names_count(filter)));
            return qb
                .build_query_scalar()
                .fetch_one(self.db.pool())
                .await
                .map_err(db_err);
        }
        let mut qb = base_query("COUNT(*)", filter);
        push_predicates(&mut qb, filter, "p");
        push_dedup_predicate(&mut qb, filter);
        let count: i64 = qb
            .build_query_scalar()
            .fetch_one(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(count)
    }

    /// The `/Persons` by-name listing: one representative per name among the
    /// rows the predicates keep ([`push_dedup_predicate`]) and paging pushed
    /// into SQL, with the exact total inlined in the same statement (fallback
    /// COUNT on empty pages past the last row).
    ///
    /// The pre-paging shape materialized the ENTIRE deduped table through sqlx
    /// on every request (7k rows for a `limit=100` page) and sliced in Rust —
    /// 28 ms of SQL+decode per request that collapsed `/Persons` to a 29.8 s
    /// p50 at its calibrated 608 req/s in the benchmark (P0.5 family).
    ///
    /// Pushing the page into SQL left a second cost behind: `COUNT(*) OVER()`
    /// made SQLite re-materialize every deduped row through a second aggregate
    /// pass before the `LIMIT` could discard them, which on the *unnarrowed*
    /// listing (`/Persons?userId=…&limit=100`, the benchmark's request and the
    /// client's default People browse) is the whole table. The total is still
    /// exact, but no longer bought by re-materializing every deduped row.
    /// Measured on the bench library (7.5k names / 9.4k rows, `limit=100`,
    /// before `must_have_item`): 5.5 ms → 2.7 ms p50 (3.3 ms → 1.6 ms min);
    /// unpaged: 11.2 ms → 6.6 ms. Since Jellyfin 12.2 `/Persons` reports this
    /// total and sets `must_have_item`, so its unnarrowed count probes
    /// `BaseItems` once per `Peoples` row ([`distinct_names_count`]) — still one
    /// scalar pass, not the window's. A narrowed query keeps the window
    /// function, which measures fastest there (e.g. a name substring: 3.2 ms window vs 5.9 ms
    /// for any second-pass shape, since the window only spans the matches).
    ///
    /// The third cost was the dedup itself: as a derived table it had to
    /// aggregate all 7.5k people and sort them through a temp B-tree before the
    /// `LIMIT` could drop 98% of the result. [`push_dedup_predicate`] moves it
    /// onto a plain `"Peoples"` scan driven by the name index, which already
    /// supplies the ordering — 1.26 ms → 0.31 ms per query, and `/Persons`
    /// 2.8 ms → 1.4 ms p50 at its calibrated rate.
    async fn get_people_by_name(
        &self,
        filter: &InternalPeopleQuery,
    ) -> Result<QueryResult<PeopleEntity>, ServiceError> {
        // Single query: the total travels alongside each data row, so a page
        // costs one pool acquire and one statement.
        let mut qb = by_name_page_query(filter);
        let start = filter.start_index.unwrap_or(0);
        let rows = qb
            .build_query_as::<PeopleWithCount>()
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        let total = match rows.first() {
            Some(r) => r.total_count,
            None if start > 0 => self.count_people_total(filter).await?,
            None => 0,
        };
        let items = rows
            .into_iter()
            .map(|r| PeopleEntity {
                id: r.id,
                name: r.name,
                person_type: r.person_type,
                role: None,
                primary_image_url: None,
                provider_id: None,
                sort_order: None,
            })
            .collect();
        Ok(QueryResult::new(
            Some(start),
            Some(i32::try_from(total).unwrap_or(i32::MAX)),
            items,
        ))
    }
}

/// `Person-{Name}` — the `PresentationUniqueKey` a materialized Person row
/// carries, exactly as `kinds::presentation_unique_key` builds it.
///
/// Both inserts that materialize a Person go through this. They produce the
/// same row shape, and the scan-path one used to forget the column — so a
/// person the scanner created was keyless where one the credit path created was
/// not, and where a real 10.11.8 writes the key on both. The column is what
/// `GROUP BY PresentationUniqueKey` reads, and SQLite groups NULLs together.
fn person_presentation_key(item_id: &str, name: &str) -> String {
    crate::kinds::presentation_unique_key(
        BaseItemKind::Person,
        Uuid::parse_str(item_id).unwrap_or_default(),
        Some(name),
        None,
        None,
        None,
    )
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
            // One row per person here; [`Self::credits_of`] then expands each
            // into its credits on the item, carrying the credited role (a
            // character name) and sort order from the map rows.
            let cols = r#"p."Id", p."Name", p."PersonType", NULL AS "Role""#;
            let mut qb = base_query(cols, filter);
            push_predicates(&mut qb, filter, "p");
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
        let rows = self.credits_of(filter.item_id, rows).await?;
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
        for chunk in item_ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let mut qb = QueryBuilder::<Sqlite>::new(
                // The role is read back VERBATIM, as in `credits_of`:
                // `UpdatePeople` normalizes a null role to
                // `string.Empty` on write (v10.11.8 `PeopleRepository.cs:77-80`)
                // and `Map` reads `Role = mapping?.Role` with no coalescing
                // (`:157`), so a roleless credit is serialized as `"Role": ""`.
                // `SortOrder` too: `AttachPeople` orders the DTO's people by
                // it (`Map` reads `SortOrder = mapping?.SortOrder`, `:378`).
                r#"SELECT m."ItemId", p."Id", p."Name", p."PersonType", m."Role", m."SortOrder"
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

    async fn get_person_item_ids(
        &self,
        names: &[String],
    ) -> Result<Vec<Option<Uuid>>, ServiceError> {
        // GetPerson derives the configured current identity and reads that
        // exact row. The write-side legacy-row preservation is deliberately
        // not a fallback to another identity or a generic matching name.
        Ok(names
            .iter()
            .map(|name| {
                self.person_item_id(name)
                    .and_then(|id| Uuid::parse_str(&id).ok())
            })
            .collect())
    }

    async fn update_people(
        &self,
        item_id: Uuid,
        people: &[PeopleEntity],
    ) -> Result<Vec<WrittenPerson>, ServiceError> {
        let deduped = dedupe_people(people);
        // Read before the write transaction: its first statement must be the write.
        let stored = self.stored_credits(item_id).await?;
        // v12.2 `UpdatePeople`: "Most library scans refresh unchanged local
        // metadata. Avoid opening a write transaction when the item's people
        // mappings, order and roles are unchanged." What upstream's caller
        // still does then (`SavePeopleMetadataAsync`) is answered by reads;
        // like it, nothing is created for a person whose item already exists,
        // folder included.
        if credits_unchanged(&stored, &deduped)
            && let Some(written) = self.unchanged_people(&deduped, &stored).await?
        {
            return Ok(written);
        }
        let stored_roles: HashMap<CreditKey, String> = stored
            .into_iter()
            .map(|credit| (credit.key, credit.role))
            .collect();
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
        // One `Peoples` row per (name, type), whatever number of credits
        // (roles) it holds on this item.
        let mut person_ids: HashMap<(String, String), String> = HashMap::new();
        for person in &deduped {
            let key = person_key(person);
            if let Some(id) = person_ids.get(&key) {
                people_ids.push(id.clone());
                continue;
            }
            let name = person.name.trim();
            // `COLLATE NOCASE`, not `LOWER(…) = LOWER(…)`: the latter is an
            // expression whose meaning depends on whether SQLite was built with
            // ICU, so an index written by one engine and probed by another
            // misses rows — and a MISS here mints a duplicate person. See
            // migration 0022.
            let existing: Option<String> = sqlx::query_scalar(
                r#"SELECT "Id" FROM "Peoples"
                   WHERE "Name" = ?1 COLLATE NOCASE
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
            let item_id = self.person_item_id_for_write(&mut tx, name, &id).await?;
            if let Some(type_name) = person_type_name {
                self.insert_person_item(&mut tx, type_name, name, &item_id)
                    .await?;
            }

            let overview = person_overview(&mut *tx, &item_id).await?.flatten();
            written.extend(written_person(&item_id, person, overview.as_deref()));
            person_ids.insert(key, id.clone());
            people_ids.push(id);
        }

        insert_credit_rows(&mut tx, item_id, &people_ids, &deduped, &stored_roles).await?;
        tx.commit().await.map_err(db_err)?;
        self.create_person_directories(&deduped).await;
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
        push_predicates(&mut qb, filter, "p");
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

/// Test-only: a `Peoples` row with no item link, so the raw SQL stays inside
/// the repository boundary.
#[cfg(test)]
pub(crate) async fn seed_people_row(db: &Database, id: uuid::Uuid, name: &str, person_type: &str) {
    sqlx::query(r#"INSERT INTO "Peoples" ("Id", "Name", "PersonType") VALUES (?1, ?2, ?3)"#)
        .bind(guid_to_db(id))
        .bind(name)
        .bind(person_type)
        .execute(db.writer())
        .await
        .expect("seed people row");
}

/// Test-only: a credit of `name` on `item`, with no `Person` item behind it.
#[cfg(test)]
pub(crate) async fn seed_credit(db: &Database, item: uuid::Uuid, id: uuid::Uuid, name: &str) {
    seed_people_row(db, id, name, "Actor").await;
    sqlx::query(
        r#"INSERT INTO "PeopleBaseItemMap" ("ItemId", "PeopleId", "Role") VALUES (?1, ?2, '')"#,
    )
    .bind(guid_to_db(item))
    .bind(guid_to_db(id))
    .execute(db.writer())
    .await
    .expect("seed credit");
}

/// Test-only: locks an item (`LockData`).
#[cfg(test)]
pub(crate) async fn lock_item(db: &Database, id: uuid::Uuid) {
    sqlx::query(r#"UPDATE "BaseItems" SET "IsLocked" = 1 WHERE "Id" = ?1"#)
        .bind(guid_to_db(id))
        .execute(db.writer())
        .await
        .expect("lock item");
}

/// Test-only: the names of the stored `Person` rows, sorted.
#[cfg(test)]
pub(crate) async fn person_names(db: &Database) -> Vec<String> {
    sqlx::query_scalar(r#"SELECT "Name" FROM "BaseItems" WHERE "Type" = ?1 ORDER BY "Name""#)
        .bind(PERSON_TYPE_NAME)
        .fetch_all(db.pool())
        .await
        .expect("person names")
}

/// Test-only: how many `Peoples` rows there are, so the raw SQL stays inside
/// the repository boundary.
#[cfg(test)]
pub(crate) async fn people_row_count(db: &Database) -> i64 {
    sqlx::query_scalar(r#"SELECT COUNT(*) FROM "Peoples""#)
        .fetch_one(db.pool())
        .await
        .expect("count people rows")
}

/// `term` as the body of a `LIKE … ESCAPE '\'` pattern that matches it
/// literally: `\`, `%` and `_` are escaped, as EF Core's literal
/// `Contains`/`StartsWith` translations treat them (`PeopleRepository`).
fn like_literal(term: &str) -> String {
    term.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    /// `Person` is the only 10.11.8 entity with
    /// `EnableAlphaNumericSorting => false`, so `BaseItem.CreateSortName`
    /// returns `Name.TrimStart()` — the name verbatim, not the lowercased
    /// alphanumeric key every other kind gets.
    #[test]
    fn a_person_sorts_by_its_name_verbatim() {
        assert_eq!(super::person_sort_name("Alice Parity"), "Alice Parity");
        assert_eq!(super::person_sort_name("  The Rock"), "The Rock");
        // NOT the alphanumeric pipeline (which lowercases and strips "the ").
        assert_ne!(
            super::person_sort_name("The Rock"),
            ferrofin_util::sort_name::create_sort_name("The Rock")
        );
    }

    use super::{
        FerrofinPeopleRepository, PERSON_TYPE_NAME, PersonPartition, StoredPerson,
        partition_credits_by_person_id,
    };
    use crate::test_support::{seed_item, seed_named_item, test_db};
    use ferrofin_db::entities::base_items::PeopleEntity;
    use ferrofin_db::store::guid_to_db;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_traits::options::InternalPeopleQuery;
    use ferrofin_traits::persistence::{PeopleRepository, PersonMetadata};
    use uuid::Uuid;

    #[tokio::test]
    async fn nfo_actor_image_ids_follow_configured_path_instead_of_credit_ids() {
        use crate::item_type_lookup::IdDerivation;
        use crate::test_support::item_repository_over;
        let db = test_db().await;
        let movie = Uuid::from_u128(0x9910);
        seed_item(&db, movie, BaseItemKind::Movie).await;
        let temporary = tempfile::tempdir().unwrap();
        let mode = IdDerivation::Jellyfin {
            program_data_path: Some(temporary.path().to_string_lossy().into_owned()),
        };
        let repo = FerrofinPeopleRepository::new(db.clone()).with_identity(
            mode,
            temporary
                .path()
                .join("metadata/People")
                .to_string_lossy()
                .into_owned(),
        );
        let names = ["NFO Actor".to_owned(), "  Éva:III...  ".to_owned()];
        repo.update_people(
            movie,
            &[person(&names[0], "Actor"), person(&names[1], "GuestStar")],
        )
        .await
        .unwrap();
        let resolved = repo.get_person_item_ids(&names).await.unwrap();
        // Literal source path/type keys after the configured program-data rewrite,
        // not the write implementation's per-(name,type) credit UUIDs.
        let expected = [
            ferrofin_common::extensions::get_md5(
                "MediaBrowser.Controller.Entities.Personmetadata\\people\\n\\nfo actor",
            ),
            ferrofin_common::extensions::get_md5(
                "MediaBrowser.Controller.Entities.Personmetadata\\people\\é\\éva iii",
            ),
        ];
        assert_eq!(resolved, expected.map(Some));
        let credits = repo
            .get_people_batch(&[movie])
            .await
            .unwrap()
            .remove(&movie)
            .unwrap();
        assert_eq!(credits.len(), 2);
        for (credit, id) in credits.iter().zip(expected) {
            assert_ne!(Uuid::parse_str(&credit.id).unwrap(), id);
            let row = item_repository_over(db.clone())
                .retrieve_item(id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.type_, PERSON_TYPE_NAME);
        }
        let duplicate = [names[1].clone(), names[0].clone(), names[1].clone()];
        assert_eq!(
            repo.get_person_item_ids(&duplicate).await.unwrap(),
            vec![Some(expected[1]), Some(expected[0]), Some(expected[1])]
        );
    }

    #[tokio::test]
    async fn nfo_actor_image_ids_do_not_adopt_matching_or_previous_rows() {
        use crate::item_type_lookup::{
            IdDerivation, person_item_id, person_path, previous_by_name_item_id,
        };
        use crate::test_support::{item_repository_over, seed_named_item};
        let db = test_db().await;
        let mode = IdDerivation::Jellyfin {
            program_data_path: Some("/config".into()),
        };
        let root = "/config/metadata/People";
        let name = "İpek Actor";
        let current = person_item_id(&mode, root, name).unwrap();
        let previous =
            previous_by_name_item_id(&mode, BaseItemKind::Person, &person_path(root, name))
                .unwrap();
        assert_ne!(current, previous);
        seed_named_item(&db, previous, BaseItemKind::Person, name).await;
        let matching = Uuid::from_u128(0x9920);
        seed_named_item(&db, matching, BaseItemKind::Person, name).await;
        let repo = FerrofinPeopleRepository::new(db.clone()).with_identity(mode, root.into());
        assert_eq!(
            repo.get_person_item_ids(&[name.to_owned()]).await.unwrap(),
            vec![Some(current)]
        );
        let items = item_repository_over(db.clone());
        assert!(items.retrieve_item(current).await.unwrap().is_none());
        // A name match or retained older identity does not manufacture the
        // current configured row; an actual wrong-kind row remains wrong-kind.
        seed_named_item(&db, current, BaseItemKind::Movie, name).await;
        let actual = items.retrieve_item(current).await.unwrap().unwrap();
        assert_ne!(actual.type_, PERSON_TYPE_NAME);
        assert_eq!(repo.get_person_item_ids(&[]).await.unwrap(), Vec::new());
        let unwired = FerrofinPeopleRepository::new(db);
        assert_eq!(
            unwired
                .get_person_item_ids(&[name.to_owned()])
                .await
                .unwrap(),
            vec![None]
        );
    }

    fn person(name: &str, person_type: &str) -> PeopleEntity {
        PeopleEntity {
            id: String::new(),
            name: name.to_owned(),
            person_type: Some(person_type.to_owned()),
            ..Default::default()
        }
    }

    /// Stands in for the real item-by-name id, as upstream's
    /// `PeopleValidatorPartitionTests.PersonId` does: derived from the credit
    /// name, case-insensitively, and from nothing else. The property that
    /// matters is that it does not depend on the item's own name.
    fn partition_id(name: &str) -> Uuid {
        ferrofin_common::extensions::get_md5(&ferrofin_util::string_extensions::lower_invariant(
            name,
        ))
    }

    fn stored(id: Uuid, name: &str) -> StoredPerson {
        StoredPerson {
            id,
            name: Some(name.to_owned()),
            clean_name: Some(crate::text_util::get_clean_value(name)),
            is_locked: false,
        }
    }

    fn partition(credits: &[&str], rows: &[StoredPerson]) -> PersonPartition {
        let credits: Vec<String> = credits.iter().map(|name| (*name).to_owned()).collect();
        partition_credits_by_person_id(&credits, |name| vec![partition_id(name)], rows)
    }

    /// `PartitionCreditsByPersonId_ProviderRenamedThePerson_KeepsThemAndCreatesNothing`,
    /// with the rename that also changes the clean name beside upstream's.
    #[rstest::rstest]
    #[case("Aurora")]
    #[case("Aurora Aksnes")]
    fn partition_keeps_a_renamed_person_and_creates_nothing(#[case] renamed: &str) {
        let rows = [stored(partition_id("AURORA"), renamed)];
        assert_eq!(partition(&["AURORA"], &rows), PersonPartition::default());
    }

    /// `PartitionCreditsByPersonId_CreditWithAnItem_IsNeverBothCreatedAndDeleted`.
    #[rstest::rstest]
    #[case("AURORA")]
    #[case("Amir AboulEla")]
    #[case("Miguel Ángel Fuentes")]
    #[case("a‐ha")]
    #[case("윤현민")]
    fn partition_never_creates_and_deletes_a_credited_person(#[case] credit: &str) {
        let rows = [stored(partition_id(credit), credit)];
        assert_eq!(partition(&[credit], &rows), PersonPartition::default());
    }

    /// `PartitionCreditsByPersonId_CreditWithNoItem_IsCreated`.
    #[test]
    fn partition_creates_an_item_for_a_credit_with_none() {
        let found = partition(&["Wanted Person"], &[]);
        assert_eq!(found.new_names, ["Wanted Person"]);
        assert_eq!(found.dead, []);
    }

    /// `PartitionCreditsByPersonId_ItemNoCreditNames_IsDead`.
    #[test]
    fn partition_finds_an_item_no_credit_names_dead() {
        let orphan = stored(partition_id("Nobody Credits Me"), "Nobody Credits Me");
        let rows = [stored(partition_id("Credited"), "Credited"), orphan.clone()];
        let found = partition(&["Credited"], &rows);
        assert_eq!(found.new_names, Vec::<String>::new());
        assert_eq!(found.dead, [orphan]);
    }

    /// `PartitionCreditsByPersonId_CreditsNormalizingOntoOneId_CreateOneItem`.
    #[test]
    fn partition_creates_one_item_for_credits_normalizing_onto_one_id() {
        let found = partition(&["AURORA", "Aurora", "aurora"], &[]);
        assert_eq!(found.new_names, ["AURORA"]);
        assert_eq!(found.dead, []);
    }

    /// `PartitionCreditsByPersonId_SecondRunAfterCreating_AsksForNothingFurther`.
    #[test]
    fn partition_asks_for_nothing_further_on_the_run_after_creating() {
        let credits = ["AURORA", "Amir AboulEla", "Miguel Ángel Fuentes"];
        let first = partition(&credits, &[]);
        assert_eq!(first.new_names.len(), 3);
        assert_eq!(first.dead, []);
        let created: Vec<StoredPerson> = first
            .new_names
            .iter()
            .map(|name| stored(partition_id(name), name))
            .collect();
        assert_eq!(partition(&credits, &created), PersonPartition::default());
    }

    /// "A person somebody locked is theirs, not ours": left out of the dead,
    /// and counted so the run can say so.
    #[test]
    fn partition_spares_a_locked_item_no_credit_names() {
        let mut locked = stored(partition_id("Locked Lena"), "Locked Lena");
        locked.is_locked = true;
        let found = partition(&[], &[locked]);
        assert_eq!(found.dead, []);
        assert_eq!(found.locked, 1);
    }

    /// The older derivation the write path keeps: the row lives under it, and
    /// the credit needs no item of the current id.
    #[test]
    fn partition_counts_the_older_derivation_as_the_credits_item() {
        let older = Uuid::from_u128(0x0D1);
        let rows = [stored(older, "Renamed")];
        let credits = ["Credited".to_owned()];
        let found =
            partition_credits_by_person_id(&credits, |name| vec![partition_id(name), older], &rows);
        assert_eq!(found, PersonPartition::default());
    }

    /// A stored id that is not the derived one, as a moved metadata folder
    /// leaves every id: the exact name keeps the row and is its item, so the
    /// library is neither deleted nor made again.
    #[test]
    fn partition_keeps_a_row_of_the_credits_exact_name_under_another_id() {
        let rows = [stored(Uuid::from_u128(0x0D2), "Moved Mary")];
        assert_eq!(
            partition(&["Moved Mary"], &rows),
            PersonPartition::default()
        );
    }

    /// Spellings sharing a clean name are one person on one row, whichever of
    /// them is credited: no second item, and the row lives while any is.
    #[test]
    fn partition_keeps_a_second_spelling_on_the_row_it_resolves_to() {
        let first = stored(partition_id("H. Jon Benjamin"), "H. Jon Benjamin");
        assert_eq!(
            partition(&["H Jon Benjamin"], std::slice::from_ref(&first)),
            PersonPartition::default()
        );
    }

    /// Two rows share a clean name, as an adopted Jellyfin database holds them.
    /// A credit keeps its own row by id, and the row it resolves to: the first
    /// by `SortName`, `Id`, the order `stored` comes in.
    #[test]
    fn partition_keeps_the_row_a_credit_resolves_to_beside_its_own() {
        let resolved = stored(partition_id("Jose"), "Jose");
        let own = stored(partition_id("José"), "José");
        assert_eq!(
            partition(&["José"], &[resolved, own]),
            PersonPartition::default()
        );
    }

    /// Two new credits sharing a clean name are one person, as they are with a
    /// row behind them: one item, under the first of them.
    #[test]
    fn partition_creates_one_item_for_new_spellings_of_one_clean_name() {
        let found = partition(&["H Jon Benjamin", "H. Jon Benjamin"], &[]);
        assert_eq!(found.new_names, ["H Jon Benjamin"]);
    }

    /// A blank credit needs no item, and no other row lives by it.
    #[test]
    fn partition_ignores_a_blank_credit() {
        let orphan = stored(partition_id("Orphan"), "Orphan");
        let found = partition(&["  "], std::slice::from_ref(&orphan));
        assert_eq!(found.new_names, Vec::<String>::new());
        assert_eq!(found.dead, [orphan]);
    }

    /// Without the identity seam a credit derives no id: names alone keep a
    /// row, and nothing is created.
    #[test]
    fn partition_creates_nothing_without_an_identity() {
        let rows = [stored(Uuid::from_u128(0x0D3), "Credited")];
        let credits = ["Credited".to_owned(), "Wanted".to_owned()];
        let found = partition_credits_by_person_id(&credits, |_| Vec::new(), &rows);
        assert_eq!(found, PersonPartition::default());
    }

    /// What this deletes is a subset of what the name-keyed sweep it replaced
    /// deleted (a `NULL` name, or no credit of exactly that name), so the
    /// change of key costs a long-lived database no row.
    #[test]
    fn partition_deletes_nothing_the_name_keyed_sweep_kept() {
        let credits = ["Exact", " Untrimmed ", "Credited Id", "H Jon Benjamin"];
        let nameless = StoredPerson {
            name: None,
            clean_name: None,
            ..stored(Uuid::from_u128(0x0D4), "")
        };
        let mut locked = stored(Uuid::from_u128(0x0D5), "Locked");
        locked.is_locked = true;
        let rows = [
            stored(Uuid::from_u128(0x0D6), "Exact"),
            stored(Uuid::from_u128(0x0D7), " Untrimmed "),
            stored(Uuid::from_u128(0x0D8), "Untrimmed"),
            stored(partition_id("Credited Id"), "Renamed"),
            stored(Uuid::from_u128(0x0D9), "H. Jon Benjamin"),
            stored(Uuid::from_u128(0x0DA), "Orphan"),
            nameless.clone(),
            locked,
        ];
        let found = partition(&credits, &rows);
        for dead in &found.dead {
            assert!(
                dead.name
                    .as_deref()
                    .is_none_or(|name| !credits.contains(&name)),
                "{dead:?} was kept by the name-keyed sweep"
            );
        }
        assert_eq!(found.dead, [rows[5].clone(), nameless]);
        assert_eq!(found.locked, 1);
    }

    /// A row whose id is no guid can be neither judged nor deleted. It is
    /// passed over, and the people beside it are still judged.
    #[tokio::test]
    async fn partition_person_items_passes_over_a_row_whose_id_is_no_guid() {
        let db = test_db().await;
        let parent = Uuid::from_u128(0x0DB);
        seed_item(&db, parent, BaseItemKind::Folder).await;
        crate::test_support::seed_child_with_raw_id(&db, "no-guid", BaseItemKind::Person, parent)
            .await;
        let orphan = Uuid::from_u128(0x0DC);
        seed_named_item(&db, orphan, BaseItemKind::Person, "Orphan").await;

        let found = FerrofinPeopleRepository::new(db)
            .partition_person_items()
            .await
            .unwrap();

        let dead: Vec<Uuid> = found.dead.iter().map(|person| person.id).collect();
        assert_eq!(dead, [orphan]);
    }

    /// A scan made the person's row after the partition read: creating the
    /// credit's item reuses it, as the write path does, whatever its spelling.
    #[tokio::test]
    async fn create_person_items_reuses_a_row_a_scan_made_meanwhile() {
        let db = test_db().await;
        let people = tempfile::tempdir().unwrap();
        let repo = FerrofinPeopleRepository::new(db.clone()).with_identity(
            crate::item_type_lookup::IdDerivation::Jellyfin {
                program_data_path: None,
            },
            people.path().to_string_lossy().into_owned(),
        );
        let scanned = Uuid::from_u128(0x0DD);
        seed_named_item(&db, scanned, BaseItemKind::Person, "H. Jon Benjamin").await;
        crate::test_support::set_clean_name(&db, scanned, "H. Jon Benjamin").await;

        let created = repo
            .create_person_items(&["H Jon Benjamin".to_owned()])
            .await
            .unwrap();

        assert_eq!(created, 0);
        assert_eq!(super::person_names(&db).await, ["H. Jon Benjamin"]);
    }

    /// A credit with no role round-trips as `Some("")`, NOT `None`.
    ///
    /// C# `PeopleRepository.UpdatePeople` normalizes a null role to
    /// `string.Empty` on write (v10.11.8 `:77-80`) and `Map` reads
    /// `Role = mapping?.Role` with no coalescing (`:157`), so Jellyfin always
    /// puts `"Role": ""` on the wire for a roleless director. Both read
    /// projections used to wrap the column in `NULLIF(…, '')`, which folded that
    /// back to NULL and dropped the field off the DTO entirely — an
    /// `Option::is_none` skip on `BaseItemPerson::role`.
    #[tokio::test]
    async fn a_roleless_credit_reads_back_as_the_empty_string() {
        let db = test_db().await;
        let repo = FerrofinPeopleRepository::new(db.clone());
        let movie = Uuid::from_u128(0x51);
        seed_item(&db, movie, BaseItemKind::Movie).await;

        let mut lead = person("Bob Parity", "Actor");
        lead.role = Some("Lead".to_owned());
        repo.update_people(movie, &[lead, person("Carol Ferrofin", "Director")])
            .await
            .expect("credits");

        // The single-item projection (`get_people` with an item scope).
        let scoped = repo
            .get_people(&InternalPeopleQuery {
                item_id: movie,
                ..Default::default()
            })
            .await
            .expect("scoped people");
        let role_of = |people: &[PeopleEntity], name: &str| {
            people
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("{name} credited"))
                .role
                .clone()
        };
        assert_eq!(
            role_of(&scoped.items, "Bob Parity"),
            Some("Lead".to_owned())
        );
        assert_eq!(
            role_of(&scoped.items, "Carol Ferrofin"),
            Some(String::new()),
            "a roleless credit is the empty string, not null"
        );

        // ...and the batch projection the DTO service actually calls.
        let batched = repo.get_people_batch(&[movie]).await.expect("batch");
        let people = batched.get(&movie).expect("item present");
        assert_eq!(role_of(people, "Bob Parity"), Some("Lead".to_owned()));
        assert_eq!(role_of(people, "Carol Ferrofin"), Some(String::new()));
    }

    /// Upstream filters with `Name.ToUpper().Contains(x)` / `StartsWith(x)`
    /// (`PeopleRepository.ApplyQueryFilter`), literal matches: a `%`, `_` or
    /// `\` in the term matches only itself, never as a `LIKE` wildcard.
    #[tokio::test]
    async fn people_name_filters_match_wildcard_characters_literally() {
        let db = test_db().await;
        let movie = Uuid::new_v4();
        crate::test_support::seed_named_item(&db, movie, BaseItemKind::Movie, "Movie").await;
        let repo = FerrofinPeopleRepository::new(db);
        repo.update_people(
            movie,
            &[
                person("Ann_Lee", "Actor"),
                person("AnnXLee", "Actor"),
                person("100% Pure", "Actor"),
                person("1000 Pure", "Actor"),
                person(r"Back\Slash", "Actor"),
            ],
        )
        .await
        .unwrap();
        let names = |query: InternalPeopleQuery| {
            let repo = &repo;
            async move {
                let mut names: Vec<String> = repo
                    .get_people(&query)
                    .await
                    .unwrap()
                    .items
                    .into_iter()
                    .map(|p| p.name)
                    .collect();
                names.sort();
                names
            }
        };
        let contains = |term: &str| InternalPeopleQuery {
            name_contains: Some(term.to_owned()),
            ..Default::default()
        };
        let starts = |term: &str| InternalPeopleQuery {
            name_starts_with: Some(term.to_owned()),
            ..Default::default()
        };
        assert_eq!(names(contains("n_l")).await, ["Ann_Lee"]);
        assert_eq!(names(contains("%")).await, ["100% Pure"]);
        assert_eq!(names(contains("_")).await, ["Ann_Lee"]);
        assert_eq!(names(contains(r"k\s")).await, [r"Back\Slash"]);
        assert_eq!(names(starts("100%")).await, ["100% Pure"]);
        assert_eq!(names(starts("ann_")).await, ["Ann_Lee"]);
    }

    #[rstest::rstest]
    #[case("Élodie", "él")]
    #[case("ΟΣ", "οσ")]
    #[case("𐐀 Star", "𐐨")]
    #[tokio::test]
    async fn unicode_people_filters(#[case] name: &str, #[case] term: &str) {
        let db = test_db().await;
        let movie = Uuid::new_v4();
        crate::test_support::seed_named_item(&db, movie, BaseItemKind::Movie, "Movie").await;
        let repo = FerrofinPeopleRepository::new(db);
        repo.update_people(movie, &[person(name, "Actor")])
            .await
            .unwrap();
        for prefix in [true, false] {
            let query = InternalPeopleQuery {
                name_starts_with: prefix.then(|| term.to_owned()),
                name_contains: (!prefix).then(|| term.to_owned()),
                ..Default::default()
            };
            let result = repo.get_people(&query).await.unwrap();
            assert_eq!(result.items.len(), 1);
            assert_eq!(result.items[0].name, name);
        }
    }

    #[rstest::rstest]
    #[case("İpek")]
    #[case("ΟΣ")]
    #[tokio::test]
    async fn unicode_identity_retains_existing_favorite(#[case] name: &str) {
        use crate::item_type_lookup::{
            IdDerivation, person_item_id, person_path, previous_by_name_item_id,
        };
        use crate::test_support::{seed_named_item, seed_user, seed_user_data};
        let db = test_db().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("People").to_string_lossy().into_owned();
        let mode = IdDerivation::Jellyfin {
            program_data_path: Some(tmp.path().to_string_lossy().into_owned()),
        };
        let old = previous_by_name_item_id(&mode, BaseItemKind::Person, &person_path(&root, name))
            .unwrap();
        let new = person_item_id(&mode, &root, name).unwrap();
        assert_ne!(old, new);
        seed_named_item(&db, old, BaseItemKind::Person, name).await;
        crate::test_support::set_clean_name(&db, old, name).await;
        let movie = Uuid::new_v4();
        seed_named_item(&db, movie, BaseItemKind::Movie, "Movie").await;
        let user_id = Uuid::new_v4();
        seed_user(&db, user_id).await;
        seed_user_data(&db, user_id, old, false, None).await;
        sqlx::query(r#"UPDATE "UserData" SET "IsFavorite" = 1 WHERE "ItemId" = ?1"#)
            .bind(guid_to_db(old))
            .execute(db.writer())
            .await
            .unwrap();
        let repo = FerrofinPeopleRepository::new(db.clone()).with_identity(mode, root);
        assert_eq!(repo.unify_person_identities().await.unwrap(), 0);
        for spelling in [
            name.to_owned(),
            ferrofin_util::string_extensions::lower_invariant(name),
        ] {
            let written = repo
                .update_people(movie, &[person(&spelling, "Actor")])
                .await
                .unwrap();
            assert_eq!(written[0].id, old);
        }
        let favorite: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "UserData" WHERE "ItemId" = ?1 AND "IsFavorite" = 1"#,
        )
        .bind(guid_to_db(old))
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(favorite, 1);
        let count: i64 =
            sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Type" = ?1"#)
                .bind(PERSON_TYPE_NAME)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(count, 1);
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

    /// The `Person` row carries the columns C# `SavePeopleMetadataAsync` sets:
    /// `Path = Person.GetPath(name)` (the first-letter subfolder form) and
    /// `PresentationUniqueKey = CreatePresentationUniqueKey()`. Ferrofin used
    /// to leave both NULL, which is a `Path` missing from every
    /// `GET /Persons/{name}` body.
    #[tokio::test]
    async fn a_person_row_carries_its_metadata_path_and_presentation_key() {
        use crate::item_type_lookup::IdDerivation;

        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tmp");
        let people_path = tmp.path().join("People").to_string_lossy().into_owned();
        let repo = FerrofinPeopleRepository::new(db.clone()).with_identity(
            IdDerivation::Jellyfin {
                program_data_path: Some(tmp.path().to_string_lossy().into_owned()),
            },
            people_path.clone(),
        );
        let movie = Uuid::from_u128(0x71);
        seed_item(&db, movie, BaseItemKind::Movie).await;
        repo.update_people(movie, &[person("Alice Parity", "Actor")])
            .await
            .expect("credits");

        let (path, key, sort): (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
            r#"SELECT "Path", "PresentationUniqueKey", "SortName" FROM "BaseItems"
                   WHERE "Type" = 'MediaBrowser.Controller.Entities.Person'"#,
        )
        .fetch_one(db.pool())
        .await
        .expect("person row");
        assert_eq!(
            path.as_deref(),
            Some(format!("{people_path}/A/Alice Parity").as_str())
        );
        assert_eq!(key.as_deref(), Some("Person-Alice Parity"));
        assert_eq!(sort.as_deref(), Some("Alice Parity"));
        // C# creates the directory as part of writing the row.
        assert!(
            std::path::Path::new(&format!("{people_path}/A/Alice Parity")).is_dir(),
            "the person metadata directory is created"
        );
    }

    /// A row written by an older build — lower-cased `SortName`, NULL `Path`,
    /// NULL `PresentationUniqueKey` — is repaired at startup, and the repair is
    /// idempotent (which is also what makes it a no-op on a database adopted
    /// from Jellyfin, where all three already hold the right values).
    #[tokio::test]
    async fn repair_person_items_fixes_a_stale_row_and_then_does_nothing() {
        use crate::item_type_lookup::{IdDerivation, person_item_id};
        use crate::test_support::seed_named_item;

        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tmp");
        let people_path = tmp.path().join("People").to_string_lossy().into_owned();
        let mode = IdDerivation::Jellyfin {
            program_data_path: Some(tmp.path().to_string_lossy().into_owned()),
        };
        let id = person_item_id(&mode, &people_path, "Alice Parity").expect("derived");
        seed_named_item(&db, id, BaseItemKind::Person, "Alice Parity").await;
        sqlx::query(
            r#"UPDATE "BaseItems" SET "SortName" = 'alice parity',
                 "Path" = NULL, "PresentationUniqueKey" = NULL WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id))
        .execute(db.writer())
        .await
        .expect("stale row");

        let repo =
            FerrofinPeopleRepository::new(db.clone()).with_identity(mode, people_path.clone());
        assert_eq!(repo.repair_person_items().await.expect("repair"), 1);
        let (path, key, sort): (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
            r#"SELECT "Path", "PresentationUniqueKey", "SortName" FROM "BaseItems"
                   WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id))
        .fetch_one(db.pool())
        .await
        .expect("person row");
        assert_eq!(sort.as_deref(), Some("Alice Parity"));
        assert_eq!(
            path.as_deref(),
            Some(format!("{people_path}/A/Alice Parity").as_str())
        );
        assert_eq!(key.as_deref(), Some("Person-Alice Parity"));

        assert_eq!(
            repo.repair_person_items().await.expect("second pass"),
            0,
            "idempotent: a correct row is not rewritten"
        );
    }

    /// …and it must NOT touch a row whose `SortName` came from the user's
    /// `ForcedSortName`. That column is the sort-title override, and C# derives
    /// `SortName` from it instead of `CreateSortName` (12.0 `BaseItem.cs:549`),
    /// so a Jellyfin database legitimately holds a `SortName` that is neither
    /// the verbatim name nor anything this pass can compute. Rewriting it would destroy the
    /// override AND make the pass re-fire on every boot, since the row could
    /// never converge. Migration `0012` and `upsert_item` guard the same way.
    #[tokio::test]
    async fn repair_person_items_never_overwrites_a_forced_sort_name() {
        use crate::item_type_lookup::{IdDerivation, person_item_id};
        use crate::test_support::seed_named_item;

        let db = test_db().await;
        let tmp = tempfile::tempdir().expect("tmp");
        let people_path = tmp.path().join("People").to_string_lossy().into_owned();
        let mode = IdDerivation::Jellyfin {
            program_data_path: Some(tmp.path().to_string_lossy().into_owned()),
        };
        let id = person_item_id(&mode, &people_path, "Alice Parity").expect("derived");
        seed_named_item(&db, id, BaseItemKind::Person, "Alice Parity").await;
        // The shape an adopted Jellyfin 12.0 database has: an override, and
        // the SortName Jellyfin derived from it (`TrimStart()` for a Person) —
        // neither equal to the name.
        sqlx::query(
            r#"UPDATE "BaseItems" SET "ForcedSortName" = 'Parity, Alice',
                 "SortName" = 'Parity, Alice',
                 "Path" = NULL, "PresentationUniqueKey" = NULL WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id))
        .execute(db.writer())
        .await
        .expect("forced row");

        let repo =
            FerrofinPeopleRepository::new(db.clone()).with_identity(mode, people_path.clone());
        // The row IS repaired — its Path and key are derived columns nobody
        // owns — but the sort name survives untouched.
        assert_eq!(repo.repair_person_items().await.expect("repair"), 1);
        let (path, key, sort): (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
            r#"SELECT "Path", "PresentationUniqueKey", "SortName" FROM "BaseItems"
                   WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id))
        .fetch_one(db.pool())
        .await
        .expect("person row");
        assert_eq!(
            sort.as_deref(),
            Some("Parity, Alice"),
            "the user's sort-title override must survive the repair"
        );
        assert!(
            path.is_some() && key.is_some(),
            "derived columns still filled"
        );

        // …and now it converges: nothing is left to do on the next boot.
        assert_eq!(
            repo.repair_person_items().await.expect("second pass"),
            0,
            "a forced-sort-name row must not re-fire the pass every startup"
        );
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

    /// v12.2 `GetPeople` for an item: one `PersonInfo` per credit
    /// (`MapCredits`, in `ListOrder`), paged and counted by person.
    #[tokio::test]
    async fn get_people_for_item_returns_every_credit_of_a_person() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = FerrofinPeopleRepository::new(db);
        repo.update_people(
            item,
            &[
                credit("Ann", "Writer", "Novel"),
                credit("Bo", "Director", ""),
                credit("Ann", "Writer", "Screenplay"),
            ],
        )
        .await
        .expect("update");

        let query = InternalPeopleQuery {
            item_id: item,
            ..Default::default()
        };
        let result = repo.get_people(&query).await.expect("get");
        assert_eq!(result.total_record_count, 2, "two people");
        let credits: Vec<(&str, &str)> = result
            .items
            .iter()
            .map(|p| (p.name.as_str(), p.role.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(
            credits,
            [("Ann", "Novel"), ("Ann", "Screenplay"), ("Bo", "")]
        );
    }

    fn credit(name: &str, person_type: &str, role: &str) -> PeopleEntity {
        PeopleEntity {
            role: Some(role.to_owned()),
            ..person(name, person_type)
        }
    }

    /// Runs `update_people` once per credit list on one item, then reads back
    /// the item's credit rows in `ListOrder`, every `Peoples` id (after each
    /// run) and how many people the last run reported written.
    async fn update_people_runs(
        runs: &[&[PeopleEntity]],
    ) -> (
        Vec<(String, String, String)>,
        Vec<String>,
        Vec<Vec<String>>,
        usize,
    ) {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = FerrofinPeopleRepository::new(db.clone());
        let mut ids_after_each = Vec::new();
        let mut written = 0;
        for people in runs {
            written = repo
                .update_people(item, people)
                .await
                .expect("update")
                .len();
            let mut ids: Vec<String> = sqlx::query_scalar(r#"SELECT "Id" FROM "Peoples""#)
                .fetch_all(db.pool())
                .await
                .expect("people");
            ids.sort();
            ids_after_each.push(ids);
        }
        let maps: Vec<(String, String, String)> = sqlx::query_as(
            r#"SELECT p."Name", p."PersonType", m."Role" FROM "PeopleBaseItemMap" m
               JOIN "Peoples" p ON p."Id" = m."PeopleId"
               WHERE m."ItemId" = ?1 ORDER BY m."ListOrder""#,
        )
        .bind(guid_to_db(item))
        .fetch_all(db.pool())
        .await
        .expect("maps");
        let people = ids_after_each.last().cloned().unwrap_or_default();
        (maps, people, ids_after_each, written)
    }

    fn row(name: &str, person_type: &str, role: &str) -> (String, String, String) {
        (name.to_owned(), person_type.to_owned(), role.to_owned())
    }

    /// `UpdatePeople_SamePersonAndTypeWithDifferentRoles_KeepsEveryCredit`
    /// (v12.2 `PeopleRepositoryUpdatePeopleTests`): a credit is
    /// (name, type, role), one person row per (name, type).
    #[tokio::test]
    async fn update_people_keeps_every_role_of_one_person() {
        let (maps, people, _, written) = update_people_runs(&[&[
            credit("Person A", "Writer", "Novel"),
            credit("Person A", "Writer", "Screenplay"),
        ]])
        .await;
        assert_eq!(people.len(), 1);
        assert_eq!(written, 1, "one person to enrich, whatever its credits");
        assert_eq!(
            maps,
            [
                row("Person A", "Writer", "Novel"),
                row("Person A", "Writer", "Screenplay")
            ]
        );
    }

    /// `UpdatePeople_CreditsDifferingOnlyInCase_AreDeduped`.
    #[tokio::test]
    async fn update_people_dedupes_credits_differing_only_in_case() {
        let (maps, people, _, _) = update_people_runs(&[&[
            credit("Person A", "Actor", "Hero"),
            credit("person a", "Actor", "hero"),
        ]])
        .await;
        assert_eq!(people.len(), 1);
        assert_eq!(maps, [row("Person A", "Actor", "Hero")]);
    }

    /// `UpdatePeople_SamePersonAsDifferentTypes_CreatesOnePersonPerType`.
    #[tokio::test]
    async fn update_people_creates_one_person_per_type() {
        let (maps, people, _, _) = update_people_runs(&[&[
            credit("Person A", "Actor", "Hero"),
            credit("Person A", "Director", ""),
        ]])
        .await;
        assert_eq!(people.len(), 2);
        assert_eq!(maps.len(), 2);
    }

    /// `UpdatePeople_RepeatedUpdate_ReusesMappingsAndRefreshesOrder`,
    /// `UpdatePeople_CreditRemoved_DropsOnlyThatMapping` and
    /// `UpdatePeople_RoleCaseChanged_KeepsExistingMapping`.
    #[tokio::test]
    async fn update_people_rewrites_order_drops_removed_and_keeps_role_casing() {
        let (maps, _, ids, _) = update_people_runs(&[
            &[
                credit("Person A", "Actor", "Hero"),
                credit("Person B", "Actor", "Sidekick"),
            ],
            &[
                credit("Person B", "Actor", "Sidekick"),
                credit("Person A", "Actor", "Hero"),
            ],
        ])
        .await;
        assert_eq!(ids[0], ids[1], "the person rows are reused");
        assert_eq!(
            maps,
            [
                row("Person B", "Actor", "Sidekick"),
                row("Person A", "Actor", "Hero")
            ]
        );

        let (maps, _, _, _) = update_people_runs(&[
            &[
                credit("Person A", "Writer", "Novel"),
                credit("Person A", "Writer", "Screenplay"),
            ],
            &[credit("Person A", "Writer", "Novel")],
        ])
        .await;
        assert_eq!(maps, [row("Person A", "Writer", "Novel")]);

        let (maps, _, _, _) = update_people_runs(&[
            &[credit("Person A", "Actor", "Hero")],
            &[credit("Person A", "Actor", "HERO")],
        ])
        .await;
        assert_eq!(maps, [row("Person A", "Actor", "Hero")]);
    }

    /// A repository over a fresh database with one movie, whose person ids
    /// follow Jellyfin's derivation when `identity` is set (as the composition
    /// root wires it) and the `Peoples` row id otherwise (the unwired seam).
    async fn people_lab(identity: bool) -> (ferrofin_db::Database, Uuid, FerrofinPeopleRepository) {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = FerrofinPeopleRepository::new(db.clone());
        let repo = if identity {
            repo.with_identity(
                crate::item_type_lookup::IdDerivation::Jellyfin {
                    program_data_path: Some("/data".to_owned()),
                },
                "/data/metadata/People".to_owned(),
            )
        } else {
            repo
        };
        (db, item, repo)
    }

    /// Logs every write to the tables a credit update touches
    /// (`PeopleBaseItemMap`, `Peoples`, `BaseItems`) into `people_writes`, as
    /// `"{table} {op}"`.
    async fn log_people_writes(db: &ferrofin_db::Database) {
        sqlx::query(r#"CREATE TABLE "people_writes" ("Op" TEXT NOT NULL)"#)
            .execute(db.writer())
            .await
            .expect("log table");
        for trigger in [
            r#"CREATE TRIGGER "log_map_insert" AFTER INSERT ON "PeopleBaseItemMap"
               BEGIN INSERT INTO "people_writes" VALUES ('map INSERT'); END"#,
            r#"CREATE TRIGGER "log_map_update" AFTER UPDATE ON "PeopleBaseItemMap"
               BEGIN INSERT INTO "people_writes" VALUES ('map UPDATE'); END"#,
            r#"CREATE TRIGGER "log_map_delete" AFTER DELETE ON "PeopleBaseItemMap"
               BEGIN INSERT INTO "people_writes" VALUES ('map DELETE'); END"#,
            r#"CREATE TRIGGER "log_peoples_insert" AFTER INSERT ON "Peoples"
               BEGIN INSERT INTO "people_writes" VALUES ('Peoples INSERT'); END"#,
            r#"CREATE TRIGGER "log_peoples_update" AFTER UPDATE ON "Peoples"
               BEGIN INSERT INTO "people_writes" VALUES ('Peoples UPDATE'); END"#,
            r#"CREATE TRIGGER "log_items_insert" AFTER INSERT ON "BaseItems"
               BEGIN INSERT INTO "people_writes" VALUES ('BaseItems INSERT'); END"#,
            r#"CREATE TRIGGER "log_items_update" AFTER UPDATE ON "BaseItems"
               BEGIN INSERT INTO "people_writes" VALUES ('BaseItems UPDATE'); END"#,
        ] {
            sqlx::query(trigger)
                .execute(db.writer())
                .await
                .expect("log trigger");
        }
    }

    async fn people_writes(db: &ferrofin_db::Database) -> Vec<String> {
        sqlx::query_scalar(r#"SELECT "Op" FROM "people_writes""#)
            .fetch_all(db.pool())
            .await
            .expect("people writes")
    }

    /// `UnchangedCredits_DoNotWriteOrLookUpAllPeople` (v12.2
    /// `PeopleUpdateQueryTests`, jellyfin/jellyfin#18004): credits that match
    /// the stored ones (name and role compared case-insensitively, the same
    /// order) leave the item's credit rows untouched, and the stored role keeps
    /// its casing.
    #[rstest::rstest]
    #[case("Hero", false)]
    #[case("HERO", false)]
    #[case("Hero", true)]
    #[case("HERO", true)]
    #[tokio::test]
    async fn unchanged_credits_do_not_rewrite_the_items_credits(
        #[case] role: &str,
        #[case] identity: bool,
    ) {
        let (db, item, repo) = people_lab(identity).await;
        repo.update_people(item, &[credit("Actor", "Actor", "Hero")])
            .await
            .expect("first update");
        log_people_writes(&db).await;

        repo.update_people(item, &[credit("actor", "Actor", role)])
            .await
            .expect("second update");
        assert_eq!(people_writes(&db).await, Vec::<String>::new());
        let roles: Vec<String> =
            sqlx::query_scalar(r#"SELECT "Role" FROM "PeopleBaseItemMap" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(item))
                .fetch_all(db.pool())
                .await
                .expect("roles");
        assert_eq!(roles, ["Hero"]);
    }

    /// `SortOrderChange_IsPersisted`: the credit's own order is part of what
    /// "unchanged" compares.
    #[tokio::test]
    async fn a_sort_order_change_is_persisted() {
        let (db, item, repo) = people_lab(true).await;
        let ordered = |sort_order| PeopleEntity {
            sort_order: Some(sort_order),
            ..person("Actor", "Actor")
        };
        repo.update_people(item, &[ordered(1)])
            .await
            .expect("first");
        repo.update_people(item, &[ordered(2)])
            .await
            .expect("second");
        let sort_orders: Vec<Option<i32>> = sqlx::query_scalar(
            r#"SELECT "SortOrder" FROM "PeopleBaseItemMap" WHERE "ItemId" = ?1"#,
        )
        .bind(guid_to_db(item))
        .fetch_all(db.pool())
        .await
        .expect("sort orders");
        assert_eq!(sort_orders, [Some(2)]);
    }

    /// An unchanged update still answers what a written one would: every
    /// credited person, in credit order, with its enrichment inputs, so the
    /// caller's biography and artwork fetch behaves as before. A credit list
    /// that empties the item still clears it.
    #[rstest::rstest]
    #[case(false)]
    #[case(true)]
    #[tokio::test]
    async fn an_unchanged_update_returns_the_people_a_write_would(#[case] identity: bool) {
        let (db, item, repo) = people_lab(identity).await;
        let credits = [
            PeopleEntity {
                primary_image_url: Some("https://img/a.jpg".to_owned()),
                provider_id: Some(7),
                ..credit("Person A", "Actor", "Hero")
            },
            credit("Person A", "Actor", "Villain"),
            credit("Person B", "Director", ""),
        ];
        let project = |written: Vec<ferrofin_traits::persistence::WrittenPerson>| {
            written
                .into_iter()
                .map(|w| (w.id, w.needs_details, w.image_url, w.provider_id))
                .collect::<Vec<_>>()
        };
        let first = project(repo.update_people(item, &credits).await.expect("first"));
        log_people_writes(&db).await;
        let again = project(repo.update_people(item, &credits).await.expect("again"));
        assert_eq!(people_writes(&db).await, Vec::<String>::new());
        assert_eq!(again, first);
        assert_eq!(first.len(), 2, "one per person, whatever its credits");

        repo.update_people(item, &[]).await.expect("cleared");
        assert_eq!(
            people_writes(&db).await,
            ["map DELETE", "map DELETE", "map DELETE"]
        );
    }

    /// Unchanged credits whose person item has gone (a by-name cleanup, a
    /// hand edit) take the write path, which recreates the item: only a write
    /// creates one.
    #[tokio::test]
    async fn unchanged_credits_recreate_a_missing_person_item() {
        let (db, item, repo) = people_lab(true).await;
        let credits = [credit("Actor", "Actor", "Hero")];
        let first = repo.update_people(item, &credits).await.expect("first");
        let person = guid_to_db(first[0].id);
        sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(&person)
            .execute(db.writer())
            .await
            .expect("drop the person item");

        let again = repo.update_people(item, &credits).await.expect("again");
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].id, first[0].id);
        let restored: Option<String> =
            sqlx::query_scalar(r#"SELECT "Name" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(&person)
                .fetch_optional(db.pool())
                .await
                .expect("person item");
        assert_eq!(restored.as_deref(), Some("Actor"));
    }

    /// `UpdatePeople` trims every credit's role and stores a missing one as
    /// `""` (v12.2 `PeopleRepository.cs:114-118`).
    #[rstest::rstest]
    #[case(Some(" Himself "), "Himself")]
    #[case(Some("   "), "")]
    #[case(None, "")]
    #[tokio::test]
    async fn update_people_stores_the_role_trimmed(
        #[case] role: Option<&str>,
        #[case] stored: &str,
    ) {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = FerrofinPeopleRepository::new(db.clone());
        let mut credit = person("Narrator", "Actor");
        credit.role = role.map(str::to_owned);
        repo.update_people(item, &[credit]).await.expect("update");
        let roles: Vec<String> =
            sqlx::query_scalar(r#"SELECT "Role" FROM "PeopleBaseItemMap" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(item))
                .fetch_all(db.pool())
                .await
                .expect("roles");
        assert_eq!(roles, [stored]);
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
            sort_order: None,
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
        assert!(r.items.is_empty(), "{:?}", r.items);
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

    /// The unnarrowed `/Persons` page must not pay for a second pass over every
    /// person. `COUNT(*) OVER()` looks free in a response body — the total is
    /// identical either way — so only the emitted SQL and its query plan can
    /// pin it: the window function forces SQLite to re-materialize the whole
    /// deduped set through an extra `CO-ROUTINE (subquery-N)` / `SCAN
    /// (subquery-N)` pair before `LIMIT` can discard it.
    #[tokio::test]
    async fn unnarrowed_by_name_page_has_no_second_aggregate_pass() {
        let db = test_db().await;
        let item = Uuid::from_u128(0x9101);
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = FerrofinPeopleRepository::new(db.clone());
        repo.update_people(item, &[person("Alice", "Actor"), person("Bob", "Director")])
            .await
            .expect("credits");

        // The paged request the benchmark issues (`/Persons?userId=…&limit=100`
        // → no narrowing predicate) and its unpaged sibling must both take the
        // index-only distinct count, never the window function.
        for query in [
            InternalPeopleQuery {
                limit: 100,
                user_id: Some(Uuid::from_u128(0x9102)),
                ..Default::default()
            },
            InternalPeopleQuery::default(),
        ] {
            let sql = super::by_name_page_query(&query)
                .into_sql()
                .as_str()
                .to_owned();
            assert!(
                !sql.contains("OVER()"),
                "unnarrowed page must not window-count every deduped row: {sql}"
            );
            assert!(
                sql.contains(r#"COUNT(DISTINCT "Name" COLLATE NOCASE)"#),
                "unnarrowed total must come off the covering index: {sql}"
            );
            assert!(
                !sql.contains("LOWER("),
                "this query must not fold case with LOWER(): its meaning depends \
                 on whether SQLite was built with ICU, so an index keyed on it \
                 is unportable between engines (migration 0022). Only the \
                 persisted index key is portability-sensitive — the UPPER() on \
                 the narrowed path is a transient comparison and is fine: {sql}"
            );
        }

        // …and SQLite agrees: planning the unpaged form (identical plan to the
        // paged one, which only adds a bound LIMIT) shows a single aggregate
        // pass, and — the whole point of the dedup *predicate* — no temp B-tree
        // sort, so the paged form's `LIMIT` can stop the scan early instead of
        // ordering every person in the library first. `EXPLAIN QUERY PLAN` names
        // each extra materialization pass `(subquery-N)`.
        let sql = super::by_name_page_query(&InternalPeopleQuery::default())
            .into_sql()
            .as_str()
            .to_owned();
        let plan: Vec<(i64, i64, i64, String)> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")))
                .fetch_all(db.pool())
                .await
                .expect("plan");
        let details: Vec<&str> = plan.iter().map(|r| r.3.as_str()).collect();
        assert!(
            !details.iter().any(|d| d.contains("(subquery")),
            "unnarrowed page plans one aggregate pass, got {details:?}"
        );
        assert!(
            details
                .iter()
                .any(|d| d.contains("FerrofinIX_Peoples_NameNoCase_Cover")),
            "the dedup pass must stay index-only, got {details:?}"
        );
        assert!(
            !details.iter().any(|d| d.contains("TEMP B-TREE")),
            "the page must come off the name index in order, not a sort of \
             every person, got {details:?}"
        );

        // A narrowed query deliberately keeps the window function: it spans
        // only the matching rows there, which measures faster than any shape
        // that re-runs the dedup.
        let narrowed = InternalPeopleQuery {
            limit: 100,
            person_types: vec!["Actor".to_owned()],
            ..Default::default()
        };
        assert!(super::narrows_by_name(&narrowed));
        assert!(
            super::by_name_page_query(&narrowed)
                .into_sql()
                .as_str()
                .to_owned()
                .contains("OVER()")
        );
    }

    /// Upstream 12.2 `GetPeople` orders the by-name listing by `Name.ToLower()`,
    /// so a lowercase name pages among its letter, not after every capitalised
    /// one — and the case-insensitive order must not cost the page its index
    /// scan. Jellyfin's bundled (non-ICU) SQLite `lower()` folds ASCII only, so
    /// `Émile` (`É` is 0xC3 0x89) still sorts before `élodie` (0xC3 0xA9), as a
    /// live 12.2 returns them; a Unicode-aware fold here would swap the pair.
    #[tokio::test]
    async fn by_name_page_orders_case_insensitively() {
        let db = test_db().await;
        let item = Uuid::from_u128(0x9111);
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = FerrofinPeopleRepository::new(db.clone());
        repo.update_people(
            item,
            &[
                person("Zoe", "Actor"),
                person("de Niro", "Actor"),
                person("Bob", "Actor"),
                person("alice", "Actor"),
                person("élodie", "Actor"),
                person("Émile", "Actor"),
            ],
        )
        .await
        .expect("credits");

        for must_have_item in [false, true] {
            let mut names = Vec::new();
            for start in [0, 2, 4] {
                let page = repo
                    .get_people(&InternalPeopleQuery {
                        limit: 2,
                        start_index: Some(start),
                        must_have_item,
                        ..Default::default()
                    })
                    .await
                    .expect("page");
                assert_eq!(page.items.len(), 2);
                names.extend(page.items.into_iter().map(|p| p.name));
            }
            assert_eq!(names, ["alice", "Bob", "de Niro", "Zoe", "Émile", "élodie"]);
        }

        // The default `/Persons` page. (A narrowed page sorts its window-counted
        // matches through a temp B-tree with either collation.)
        let sql = super::by_name_page_query(&InternalPeopleQuery {
            must_have_item: true,
            ..Default::default()
        })
        .into_sql()
        .as_str()
        .to_owned();
        let plan: Vec<(i64, i64, i64, String)> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")))
                .fetch_all(db.pool())
                .await
                .expect("plan");
        let details: Vec<&str> = plan.iter().map(|r| r.3.as_str()).collect();
        assert!(
            details.contains(&"SCAN p USING COVERING INDEX FerrofinIX_Peoples_NameNoCase_Cover"),
            "the page is read off the NOCASE index in order: {details:?}"
        );
        assert!(
            !details.iter().any(|d| d.contains("TEMP B-TREE")),
            "{details:?}"
        );
    }

    /// Whichever shape computes it, the total is the number of deduped people
    /// the filter matches — never the page length. A repository that reported
    /// `items.len()` would look right on page 0 of a short library and lie on
    /// every real page.
    ///
    /// This is also the test that guards the **read-path case folding**
    /// (`COLLATE NOCASE`, migration 0022): the `Dan`/`dan` pair below differs in
    /// `PersonType`, which is what lets it reach SQL as two rows at all — the
    /// write path's `dedupe_people` folds same-type variants away in Rust before
    /// any query runs. Revert the three `COLLATE NOCASE` sites to a plain `=`
    /// and this fails 6 vs 5.
    #[tokio::test]
    async fn by_name_total_is_the_deduped_match_count_not_the_page_length() {
        let db = test_db().await;
        let item_a = Uuid::from_u128(0x9201);
        let item_b = Uuid::from_u128(0x9202);
        seed_item(&db, item_a, BaseItemKind::Movie).await;
        seed_item(&db, item_b, BaseItemKind::Movie).await;
        let repo = FerrofinPeopleRepository::new(db);
        repo.update_people(
            item_a,
            &[
                person("Alice", "Actor"),
                person("Bob", "Actor"),
                person("Cara", "Actor"),
                person("Dan", "Director"),
                person("Eve", "Writer"),
            ],
        )
        .await
        .expect("a");
        // "dan" collapses onto "Dan" (one deduped person, two Peoples rows), so
        // the total counts names, not rows. The duplicate is a non-Actor on both
        // rows, keeping the person-type assertion below independent of which row
        // has the lower id and so represents the group.
        repo.update_people(item_b, &[person("dan", "Producer")])
            .await
            .expect("b");

        // Every page of the unnarrowed listing reports 5, including the full
        // pages whose length says nothing about what follows.
        for start in [0, 2, 4] {
            let page = repo
                .get_people(&InternalPeopleQuery {
                    limit: 2,
                    start_index: Some(start),
                    ..Default::default()
                })
                .await
                .expect("page");
            assert_eq!(
                page.total_record_count, 5,
                "page at {start} must report the whole deduped set"
            );
            assert_eq!(page.start_index, start);
        }

        // The unpaged listing agrees with the paged totals.
        let all = repo
            .get_people(&InternalPeopleQuery::default())
            .await
            .expect("all");
        assert_eq!(all.items.len(), 5);
        assert_eq!(all.total_record_count, 5);

        // A narrowed page still reports the filtered total, not its length.
        let actors = repo
            .get_people(&InternalPeopleQuery {
                limit: 1,
                person_types: vec!["Actor".to_owned()],
                ..Default::default()
            })
            .await
            .expect("actors");
        assert_eq!(actors.items.len(), 1);
        assert_eq!(actors.total_record_count, 3, "Alice, Bob, Cara");

        // …and so does the empty-page-past-the-end fallback, in both shapes.
        let past_unnarrowed = repo
            .get_people(&InternalPeopleQuery {
                limit: 2,
                start_index: Some(50),
                ..Default::default()
            })
            .await
            .expect("past unnarrowed");
        assert!(
            past_unnarrowed.items.is_empty(),
            "{:?}",
            past_unnarrowed.items
        );
        assert_eq!(past_unnarrowed.total_record_count, 5);

        let past_narrowed = repo
            .get_people(&InternalPeopleQuery {
                limit: 2,
                start_index: Some(50),
                person_types: vec!["Actor".to_owned()],
                ..Default::default()
            })
            .await
            .expect("past narrowed");
        assert!(past_narrowed.items.is_empty(), "{:?}", past_narrowed.items);
        assert_eq!(past_narrowed.total_record_count, 3);
    }

    /// Upstream `PeopleRepositoryMustHaveItemTests` (v12.2): a movie credits
    /// two actors, and only "With Item" has a by-name `Person` item behind it.
    /// `update_people` materializes one for every credit, so the other is
    /// deleted to reproduce upstream's fixture.
    async fn must_have_item_repo() -> FerrofinPeopleRepository {
        let db = test_db().await;
        let movie = Uuid::from_u128(0x9301);
        seed_item(&db, movie, BaseItemKind::Movie).await;
        let repo = FerrofinPeopleRepository::new(db.clone());
        repo.update_people(
            movie,
            &[
                person("With Item", "Actor"),
                person("Without Item", "Actor"),
            ],
        )
        .await
        .expect("credits");
        sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Type" = ?1 AND "Name" = 'Without Item'"#)
            .bind(PERSON_TYPE_NAME)
            .execute(db.writer())
            .await
            .expect("drop item");
        repo
    }

    #[tokio::test]
    async fn without_must_have_item_returns_and_counts_every_credit() {
        let result = must_have_item_repo()
            .await
            .get_people(&InternalPeopleQuery::default())
            .await
            .expect("people");
        assert_eq!(result.total_record_count, 2);
        assert_eq!(result.items.len(), 2);
    }

    #[tokio::test]
    async fn must_have_item_drops_the_credit_without_an_item() {
        let result = must_have_item_repo()
            .await
            .get_people(&InternalPeopleQuery {
                must_have_item: true,
                ..Default::default()
            })
            .await
            .expect("people");
        let names: Vec<&str> = result.items.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["With Item"]);
    }

    #[tokio::test]
    async fn must_have_item_counts_only_what_it_can_return() {
        let result = must_have_item_repo()
            .await
            .get_people(&InternalPeopleQuery {
                must_have_item: true,
                ..Default::default()
            })
            .await
            .expect("people");
        assert_eq!(result.total_record_count, 1);
    }

    #[tokio::test]
    async fn must_have_item_count_agrees_with_the_page_when_limited() {
        let result = must_have_item_repo()
            .await
            .get_people(&InternalPeopleQuery {
                must_have_item: true,
                limit: 10,
                ..Default::default()
            })
            .await
            .expect("people");
        assert_eq!(
            usize::try_from(result.total_record_count).ok(),
            Some(result.items.len())
        );
    }

    /// Upstream dedups the *filtered* candidates, so a case variant with no
    /// item must not become the group's representative and hide the person.
    /// Here the lowest-id row (`john doe`) has no item; `John Doe` does. Every
    /// query shape returns that person once and counts one: the unnarrowed
    /// scalar count, the narrowed window, and the past-the-end fallback.
    #[tokio::test]
    async fn must_have_item_picks_the_representative_among_rows_with_an_item() {
        let db = test_db().await;
        super::seed_people_row(&db, Uuid::from_u128(1), "john doe", "Producer").await;
        super::seed_people_row(&db, Uuid::from_u128(2), "John Doe", "Actor").await;
        super::seed_people_row(&db, Uuid::from_u128(3), "Nobody", "Actor").await;
        seed_named_item(
            &db,
            Uuid::from_u128(0x9401),
            BaseItemKind::Person,
            "John Doe",
        )
        .await;
        let repo = FerrofinPeopleRepository::new(db);

        for (label, query) in [
            ("unnarrowed", InternalPeopleQuery::default()),
            (
                "narrowed",
                InternalPeopleQuery {
                    name_contains: Some("doe".to_owned()),
                    ..Default::default()
                },
            ),
        ] {
            let query = InternalPeopleQuery {
                must_have_item: true,
                ..query
            };
            let page = repo.get_people(&query).await.expect("page");
            let names: Vec<&str> = page.items.iter().map(|p| p.name.as_str()).collect();
            assert_eq!(names, ["John Doe"], "{label}");
            assert_eq!(page.total_record_count, 1, "{label}");

            let past = repo
                .get_people(&InternalPeopleQuery {
                    start_index: Some(5),
                    limit: 1,
                    ..query
                })
                .await
                .expect("past");
            assert!(past.items.is_empty(), "{label}");
            assert_eq!(past.total_record_count, 1, "{label} past the end");
        }
    }

    /// Two movies with different casts, every credit backed by a `Person`
    /// item (`update_people` materializes them).
    async fn two_casts_repo() -> (FerrofinPeopleRepository, Uuid, Uuid) {
        let db = test_db().await;
        let (first, second) = (Uuid::from_u128(0x9501), Uuid::from_u128(0x9502));
        seed_item(&db, first, BaseItemKind::Movie).await;
        seed_item(&db, second, BaseItemKind::Movie).await;
        let repo = FerrofinPeopleRepository::new(db);
        // Alice is credited twice on the first movie, as two `Peoples` rows.
        repo.update_people(
            first,
            &[
                person("Alice", "Actor"),
                person("Bob", "Director"),
                person("Alice", "Producer"),
            ],
        )
        .await
        .expect("first cast");
        repo.update_people(second, &[person("Carol", "Actor"), person("Dave", "Actor")])
            .await
            .expect("second cast");
        (repo, first, second)
    }

    /// Upstream `TranslateQuery`'s `AppearsInItemId`: `/Persons?appearsInItemId=`
    /// returns and counts only the people credited on that item, once each
    /// however often they are credited, on the page, a shorter page (the window
    /// total) and past the end (the fallback count).
    #[tokio::test]
    async fn appears_in_item_returns_and_counts_only_that_items_people() {
        let (repo, first, second) = two_casts_repo().await;
        for (item, expected) in [(first, ["Alice", "Bob"]), (second, ["Carol", "Dave"])] {
            let query = InternalPeopleQuery {
                appears_in_item_id: item,
                must_have_item: true,
                ..Default::default()
            };
            assert!(super::narrows_by_name(&query));
            let page = repo.get_people(&query).await.expect("page");
            let names: Vec<&str> = page.items.iter().map(|p| p.name.as_str()).collect();
            assert_eq!(names, expected);
            assert_eq!(page.total_record_count, 2);
            // `GetPeopleNames` runs the same `TranslateQuery`.
            assert_eq!(
                repo.get_people_names(&query).await.expect("names"),
                expected
            );

            let short = repo
                .get_people(&InternalPeopleQuery {
                    limit: 1,
                    ..query.clone()
                })
                .await
                .expect("short page");
            assert_eq!(short.items.len(), 1);
            assert_eq!(short.total_record_count, 2, "window total");

            let past = repo
                .get_people(&InternalPeopleQuery {
                    start_index: Some(5),
                    limit: 1,
                    ..query
                })
                .await
                .expect("past");
            assert!(past.items.is_empty(), "{:?}", past.items);
            assert_eq!(past.total_record_count, 2, "past-the-end total");
        }
    }

    #[tokio::test]
    async fn appears_in_an_unknown_item_returns_nothing() {
        let (repo, _, _) = two_casts_repo().await;
        let page = repo
            .get_people(&InternalPeopleQuery {
                appears_in_item_id: Uuid::from_u128(0x9599),
                must_have_item: true,
                ..Default::default()
            })
            .await
            .expect("page");
        assert!(page.items.is_empty(), "{:?}", page.items);
        assert_eq!(page.total_record_count, 0);
    }

    /// Upstream dedups the filtered candidates, so the name group's
    /// representative is chosen among the rows credited on the item: the
    /// lowest-id `john doe` row is credited elsewhere and must not hide the
    /// `John Doe` one.
    #[tokio::test]
    async fn appears_in_item_picks_the_representative_among_its_credits() {
        let db = test_db().await;
        let (first, second) = (Uuid::from_u128(0x9601), Uuid::from_u128(0x9602));
        seed_item(&db, first, BaseItemKind::Movie).await;
        seed_item(&db, second, BaseItemKind::Movie).await;
        super::seed_people_row(&db, Uuid::from_u128(1), "john doe", "Producer").await;
        super::seed_people_row(&db, Uuid::from_u128(2), "John Doe", "Actor").await;
        for (item, person) in [(second, 1), (first, 2)] {
            sqlx::query(
                r#"INSERT INTO "PeopleBaseItemMap" ("ItemId", "PeopleId", "Role", "ListOrder")
                   VALUES (?1, ?2, '', 0)"#,
            )
            .bind(guid_to_db(item))
            .bind(guid_to_db(Uuid::from_u128(person)))
            .execute(db.writer())
            .await
            .expect("credit");
        }
        let repo = FerrofinPeopleRepository::new(db);
        for (item, expected) in [(first, "John Doe"), (second, "john doe")] {
            let query = InternalPeopleQuery {
                appears_in_item_id: item,
                ..Default::default()
            };
            let page = repo.get_people(&query).await.expect("page");
            let names: Vec<&str> = page.items.iter().map(|p| p.name.as_str()).collect();
            assert_eq!(names, [expected]);
            assert_eq!(page.total_record_count, 1);
            let past = repo
                .get_people(&InternalPeopleQuery {
                    start_index: Some(5),
                    limit: 1,
                    ..query
                })
                .await
                .expect("past");
            assert_eq!(past.total_record_count, 1, "{expected} past the end");
        }
    }

    /// `AppearsInItemId` drives the page from the item's credits: the map is
    /// sought by `ItemId` and the people looked up by id, never a scan of every
    /// `Peoples` row with a per-row probe.
    #[tokio::test]
    async fn appears_in_item_is_driven_by_the_items_credits() {
        let db = test_db().await;
        let query = InternalPeopleQuery {
            appears_in_item_id: Uuid::from_u128(0x9701),
            must_have_item: true,
            ..Default::default()
        };
        let sql = super::by_name_page_query(&query)
            .into_sql()
            .as_str()
            .to_owned();
        let plan: Vec<(i64, i64, i64, String)> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")))
                .fetch_all(db.pool())
                .await
                .expect("plan");
        let details: Vec<&str> = plan.iter().map(|r| r.3.as_str()).collect();
        assert!(
            !details
                .iter()
                .any(|d| *d == "SCAN p" || d.starts_with("SCAN p ")),
            "no scan of every person: {details:?}"
        );
        let probes: Vec<&&str> = details.iter().filter(|d| d.contains(" ma ")).collect();
        assert!(!probes.is_empty(), "{details:?}");
        assert!(
            probes.iter().all(|d| d.starts_with("SEARCH ma ")),
            "{details:?}"
        );
    }

    /// The default `/Persons` page sets `must_have_item` and nothing else. It
    /// must keep the unnarrowed shape (a scalar distinct count, no window, no
    /// temp B-tree), and every `MustHaveItem` probe must seek
    /// `IX_BaseItems_Name` rather than walk the Person rows off a `Type` index.
    #[tokio::test]
    async fn must_have_item_page_seeks_the_name_index() {
        let db = test_db().await;
        let query = InternalPeopleQuery {
            limit: 100,
            must_have_item: true,
            ..Default::default()
        };
        assert!(!super::narrows_by_name(&query));
        let sql = super::by_name_page_query(&query)
            .into_sql()
            .as_str()
            .to_owned();
        assert!(!sql.contains("OVER()"), "{sql}");
        assert!(
            sql.contains(r#"COUNT(DISTINCT c."Name" COLLATE NOCASE)"#),
            "{sql}"
        );

        // The bound LIMIT is planned the same as none; plan the unpaged form.
        let unpaged = InternalPeopleQuery {
            must_have_item: true,
            ..Default::default()
        };
        let sql = super::by_name_page_query(&unpaged)
            .into_sql()
            .as_str()
            .to_owned();
        let plan: Vec<(i64, i64, i64, String)> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")))
                .fetch_all(db.pool())
                .await
                .expect("plan");
        let details: Vec<&str> = plan.iter().map(|r| r.3.as_str()).collect();
        let item_probes: Vec<&&str> = details
            .iter()
            .filter(|d| d.contains(" b ") || d.ends_with(" b"))
            .collect();
        assert_eq!(item_probes.len(), 3, "outer, dedup and count: {details:?}");
        assert!(
            item_probes.iter().all(|d| d.starts_with("SEARCH b ")
                && d.ends_with("USING INDEX IX_BaseItems_Name (Name=?)")),
            "every item probe seeks the name index: {details:?}"
        );
        assert!(
            !details.iter().any(|d| d.contains("TEMP B-TREE")),
            "the page still comes off the name index in order: {details:?}"
        );
    }

    /// C# `IsFavorite` (v12.2 `PeopleRepository.cs:402-416`): the people
    /// named like a `Person` item the user has a `UserData` row for with that
    /// flag. A person with no row is in neither answer — the set is built from
    /// the rows — and a person whose item is a favourite of another user only
    /// is not this user's.
    #[tokio::test]
    async fn favorite_filter_follows_the_users_person_rows() {
        use crate::test_support::{seed_named_item, seed_named_user, seed_user_data};
        let db = test_db().await;
        let user = Uuid::from_u128(0x8660);
        let other = Uuid::from_u128(0x8661);
        seed_named_user(&db, user, "user").await;
        seed_named_user(&db, other, "other").await;
        let set_favorite = |owner: Uuid, item: Uuid, favorite: bool| {
            let db = db.clone();
            async move {
                seed_user_data(&db, owner, item, false, None).await;
                sqlx::query(
                    r#"UPDATE "UserData" SET "IsFavorite" = ?1 WHERE "ItemId" = ?2 AND "UserId" = ?3"#,
                )
                .bind(favorite)
                .bind(guid_to_db(item))
                .bind(guid_to_db(owner))
                .execute(db.writer())
                .await
                .unwrap();
            }
        };
        for (n, name) in [
            (1, "Ann Favorite"),
            (2, "Ben Seen"),
            (3, "Cat Unseen"),
            (4, "Dan Elsewhere"),
        ] {
            let item = Uuid::from_u128(0x8670 + n);
            seed_named_item(&db, item, BaseItemKind::Person, name).await;
            super::seed_people_row(&db, Uuid::from_u128(0x8680 + n), name, "Actor").await;
            match n {
                1 => set_favorite(user, item, true).await,
                2 => set_favorite(user, item, false).await,
                4 => set_favorite(other, item, true).await,
                _ => {}
            }
        }
        let repo = FerrofinPeopleRepository::new(db.clone());
        for (favorite, expected) in [(true, "Ann Favorite"), (false, "Ben Seen")] {
            let page = repo
                .get_people_by_name(&InternalPeopleQuery {
                    user_id: Some(user),
                    is_favorite: Some(favorite),
                    must_have_item: true,
                    limit: 20,
                    ..Default::default()
                })
                .await
                .expect("page");
            let names: Vec<&str> = page.items.iter().map(|p| p.name.as_str()).collect();
            assert_eq!(names, [expected], "isFavorite={favorite}");
            assert_eq!(page.total_record_count, 1, "isFavorite={favorite}");
        }
    }

    /// The favourite set is read once per statement, never per person: every
    /// `UserData` probe hangs off a `LIST SUBQUERY`, not a correlated one.
    /// Correlated, SQLite seeds each probe from the user's whole favourite (or
    /// non-favourite) set and joins back by name, once for every `Peoples` row
    /// and again for every dedup candidate — 1.4 s for an empty page on the
    /// gl-637 corpus, and past 10 s for `isFavorite=false`.
    #[rstest::rstest]
    #[case(true)]
    #[case(false)]
    #[tokio::test]
    async fn favorite_filter_reads_the_favourite_set_once(#[case] favorite: bool) {
        let db = test_db().await;
        let query = InternalPeopleQuery {
            user_id: Some(Uuid::from_u128(0x8690)),
            is_favorite: Some(favorite),
            must_have_item: true,
            limit: 20,
            ..Default::default()
        };
        let sql = super::by_name_page_query(&query)
            .into_sql()
            .as_str()
            .to_owned();
        let plan: Vec<(i64, i64, i64, String)> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")))
                .fetch_all(db.pool())
                .await
                .expect("plan");
        let detail_of: std::collections::HashMap<i64, (i64, &str)> = plan
            .iter()
            .map(|(id, parent, _, detail)| (*id, (*parent, detail.as_str())))
            .collect();
        let user_data: Vec<i64> = plan
            .iter()
            .filter(|(_, _, _, d)| d.starts_with("SEARCH ud "))
            .map(|(id, ..)| *id)
            .collect();
        assert!(!user_data.is_empty(), "{plan:?}");
        for id in user_data {
            let (parent, _) = detail_of[&id];
            let (_, opened_by) = detail_of
                .get(&parent)
                .unwrap_or_else(|| panic!("UserData read at the plan root: {plan:?}"));
            assert!(
                opened_by.starts_with("LIST SUBQUERY"),
                "UserData read under `{opened_by}`: {plan:?}"
            );
        }
    }

    /// Issue #58: upstream (v12.2 `PeopleRepository.GetPeople`) filters first
    /// and then keeps one row per name, so the representative is chosen among
    /// the rows that pass. Ann has an Actor row (lowest id, credited outside the
    /// folder), then Director and Writer rows (credited inside it); each filter
    /// below drops her Actor row and must still return and count her, by her
    /// Director row. Picking the representative before filtering lands on the
    /// Actor row and loses her.
    #[tokio::test]
    async fn by_name_filters_pick_the_representative_among_matching_rows() {
        let db = test_db().await;
        let folder = Uuid::from_u128(0x9501);
        let inside = Uuid::from_u128(0x9502);
        let outside = Uuid::from_u128(0x9503);
        for (id, kind) in [
            (folder, BaseItemKind::Folder),
            (inside, BaseItemKind::Movie),
            (outside, BaseItemKind::Movie),
        ] {
            seed_item(&db, id, kind).await;
        }
        sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
            .bind(guid_to_db(inside))
            .bind(guid_to_db(folder))
            .execute(db.writer())
            .await
            .expect("ancestor");
        for (row, name, person_type, item) in [
            (1, "Ann", "Actor", outside),
            (2, "Ann", "Director", inside),
            (3, "Bob", "Actor", inside),
            (4, "Ann", "Writer", inside),
        ] {
            let row = Uuid::from_u128(row);
            super::seed_people_row(&db, row, name, person_type).await;
            sqlx::query(
                r#"INSERT INTO "PeopleBaseItemMap" ("ItemId", "PeopleId", "Role") VALUES (?1, ?2, '')"#,
            )
            .bind(guid_to_db(item))
            .bind(guid_to_db(row))
            .execute(db.writer())
            .await
            .expect("credit");
        }
        for (id, name) in [(0x9504, "Ann"), (0x9505, "Bob")] {
            seed_named_item(&db, Uuid::from_u128(id), BaseItemKind::Person, name).await;
        }
        let repo = FerrofinPeopleRepository::new(db);

        for (label, query, expected) in [
            (
                "personTypes",
                InternalPeopleQuery {
                    person_types: vec!["Director".to_owned()],
                    ..Default::default()
                },
                &["Ann"][..],
            ),
            (
                "excludePersonTypes",
                InternalPeopleQuery {
                    exclude_person_types: vec!["Actor".to_owned()],
                    ..Default::default()
                },
                &["Ann"][..],
            ),
            (
                "parentId",
                InternalPeopleQuery {
                    parent_id: Some(folder),
                    ..Default::default()
                },
                &["Ann", "Bob"][..],
            ),
        ] {
            let query = InternalPeopleQuery {
                must_have_item: true,
                ..query
            };
            let page = repo.get_people(&query).await.expect("page");
            let names: Vec<&str> = page.items.iter().map(|p| p.name.as_str()).collect();
            assert_eq!(names, expected, "{label}");
            assert_eq!(
                usize::try_from(page.total_record_count).ok(),
                Some(expected.len()),
                "{label}: the total counts what the page returns"
            );
            // Ann's Writer row passes the last two filters too: the lowest
            // passing id represents her, never a higher one.
            assert_eq!(page.items[0].id, guid_to_db(Uuid::from_u128(2)), "{label}");

            let past = repo
                .get_people(&InternalPeopleQuery {
                    start_index: Some(5),
                    limit: 1,
                    ..query
                })
                .await
                .expect("past");
            assert!(past.items.is_empty(), "{label}");
            assert_eq!(
                usize::try_from(past.total_record_count).ok(),
                Some(expected.len()),
                "{label} past the end"
            );
        }
    }

    /// The narrowed listings keep their plan once the predicates also run
    /// inside the dedup sub-select: the dedup is still a covering seek on
    /// `FerrofinIX_Peoples_NameNoCase_Cover` (never a scan), and the only sort is
    /// the one the `COUNT(*) OVER()` window already paid to order its
    /// materialized matches before the fix.
    #[tokio::test]
    async fn narrowed_by_name_dedup_keeps_the_name_index_plan() {
        let db = test_db().await;
        for query in [
            InternalPeopleQuery {
                person_types: vec!["Director".to_owned()],
                ..Default::default()
            },
            InternalPeopleQuery {
                exclude_person_types: vec!["Actor".to_owned()],
                ..Default::default()
            },
            InternalPeopleQuery {
                parent_id: Some(Uuid::from_u128(0x9601)),
                ..Default::default()
            },
            InternalPeopleQuery {
                name_contains: Some("ann".to_owned()),
                ..Default::default()
            },
        ] {
            let query = InternalPeopleQuery {
                must_have_item: true,
                ..query
            };
            let sql = super::by_name_page_query(&query)
                .into_sql()
                .as_str()
                .to_owned();
            let plan: Vec<(i64, i64, i64, String)> =
                sqlx::query_as(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")))
                    .fetch_all(db.pool())
                    .await
                    .expect("plan");
            let details: Vec<&str> = plan.iter().map(|r| r.3.as_str()).collect();
            let dedup: Vec<&&str> = details.iter().filter(|d| d.contains(" d ")).collect();
            assert!(
                !dedup.is_empty()
                    && dedup.iter().all(|d| d.starts_with(
                        "SEARCH d USING COVERING INDEX FerrofinIX_Peoples_NameNoCase_Cover (Name=?"
                    )),
                "{query:?}: the dedup sub-select seeks the case-folded name index: {details:?}"
            );
            let sorts: Vec<&&str> = details
                .iter()
                .filter(|d| d.contains("TEMP B-TREE"))
                .collect();
            assert_eq!(
                sorts,
                [&"USE TEMP B-TREE FOR ORDER BY"],
                "{query:?}: only the window's ordering sort, as before the fix: {details:?}"
            );
            if query.parent_id.is_some() {
                // The outer predicates are written before the dedup, so a row
                // outside the parent is rejected before its name group is
                // probed: the outer `mp` probe is planned first.
                let first = |prefix: &str| details.iter().position(|d| d.starts_with(prefix));
                let (Some(outer), Some(dedup)) = (first("SEARCH mp "), first("SEARCH d ")) else {
                    panic!("{query:?}: both probes planned: {details:?}");
                };
                assert!(
                    outer < dedup,
                    "{query:?}: outer predicates before the dedup: {details:?}"
                );
            }
        }
    }

    /// Issue #61: under an access filter a person is kept only when one of
    /// their credits is on an item the filter admits, and — since upstream
    /// dedups the filtered candidates — the representative is chosen among the
    /// rows that pass. Ann's lowest-id row is an Actor credit in the closed
    /// library; her Director credit is in the open one, so she must still be
    /// listed and counted once. Bob is credited only in the closed library.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn access_filter_keeps_people_credited_on_an_admitted_item() {
        let db = test_db().await;
        let (open, closed) = (Uuid::from_u128(0x6101), Uuid::from_u128(0x6102));
        let (open_movie, closed_movie) = (Uuid::from_u128(0x6103), Uuid::from_u128(0x6104));
        crate::test_support::seed_top_parented_item(
            &db,
            open_movie,
            BaseItemKind::Movie,
            "o",
            open,
        )
        .await;
        crate::test_support::seed_top_parented_item(
            &db,
            closed_movie,
            BaseItemKind::Movie,
            "c",
            closed,
        )
        .await;
        for (row, name, person_type, item) in [
            (1, "Ann", "Actor", closed_movie),
            (2, "Ann", "Director", open_movie),
            (3, "Bob", "Actor", closed_movie),
        ] {
            let row = Uuid::from_u128(row);
            super::seed_people_row(&db, row, name, person_type).await;
            sqlx::query(
                r#"INSERT INTO "PeopleBaseItemMap" ("ItemId", "PeopleId", "Role") VALUES (?1, ?2, '')"#,
            )
            .bind(guid_to_db(item))
            .bind(guid_to_db(row))
            .execute(db.writer())
            .await
            .expect("credit");
        }
        for (id, name) in [(0x6105, "Ann"), (0x6106, "Bob")] {
            seed_named_item(&db, Uuid::from_u128(id), BaseItemKind::Person, name).await;
        }
        let repo = FerrofinPeopleRepository::new(db.clone());
        let query = |top_parent_ids: Vec<Uuid>, limit| InternalPeopleQuery {
            limit,
            must_have_item: true,
            access_filter: Some(ferrofin_traits::options::InternalItemsQuery {
                top_parent_ids,
                include_owned_items: true,
                ..Default::default()
            }),
            ..Default::default()
        };

        for limit in [0, 1] {
            let page = repo
                .get_people(&query(vec![open], limit))
                .await
                .expect("people");
            let rows: Vec<_> = page
                .items
                .iter()
                .map(|p| (p.name.as_str(), p.person_type.as_deref()))
                .collect();
            assert_eq!(rows, [("Ann", Some("Director"))], "limit {limit}");
            assert_eq!(page.total_record_count, 1, "limit {limit}");
        }
        let names = repo
            .get_people_names(&query(vec![open], 0))
            .await
            .expect("names");
        assert_eq!(names, ["Ann"]);
        let page = repo
            .get_people(&query(vec![open, closed], 0))
            .await
            .expect("people");
        assert_eq!(page.total_record_count, 2);

        // Every statement shape opens with the set: the narrowed page (window
        // total) and, past the last row, both fallback counts.
        let narrowed = |start_index| InternalPeopleQuery {
            person_types: vec!["Director".to_owned()],
            start_index,
            ..query(vec![open], 10)
        };
        let page = repo.get_people(&narrowed(None)).await.expect("narrowed");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.total_record_count, 1);
        for past_the_end in [
            narrowed(Some(5)),
            InternalPeopleQuery {
                start_index: Some(5),
                ..query(vec![open], 10)
            },
        ] {
            let page = repo.get_people(&past_the_end).await.expect("past the end");
            assert!(page.items.is_empty(), "{:?}", page.items);
            assert_eq!(page.total_record_count, 1);
        }

        // The accessible set is built once, from the libraries' items to their
        // credits; the page still walks the name index in order and the total
        // stays the scalar distinct count, so the `LIMIT` still ends the walk.
        let sql = super::by_name_page_query(&query(vec![open], 100))
            .into_sql()
            .as_str()
            .to_owned();
        assert!(!sql.contains("OVER()"), "{sql}");
        let plan: Vec<(i64, i64, i64, String)> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")))
                .fetch_all(db.pool())
                .await
                .expect("plan");
        let details: Vec<&str> = plan.iter().map(|r| r.3.as_str()).collect();
        let count = |needle: &str| details.iter().filter(|d| d.contains(needle)).count();
        assert_eq!(count("MATERIALIZE AccessiblePeople"), 1, "{details:?}");
        assert_eq!(
            count("SEARCH aci USING COVERING INDEX IX_BaseItems_TopParentId_Id (TopParentId=?)"),
            1,
            "{details:?}"
        );
        assert!(
            details
                .iter()
                .any(|d| d.starts_with("SEARCH acm ") && d.ends_with("(ItemId=?)")),
            "{details:?}"
        );
        assert!(
            details.iter().any(|d| d
                .starts_with("SCAN p USING COVERING INDEX FerrofinIX_Peoples_NameNoCase_Cover")),
            "{details:?}"
        );
        assert!(
            !details
                .iter()
                .any(|d| d.contains("TEMP B-TREE FOR ORDER BY")),
            "{details:?}"
        );
    }
}
