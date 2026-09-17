//! One-shot consolidation of localized `UserView` rows onto their
//! name-independent ids — the port of Jellyfin 12.0's
//! `Jellyfin.Server/Migrations/Routines/20260825200000_ConsolidateLocalizedUserViews.cs`.
//!
//! Up to 10.11 `LibraryManager.GetNamedView(name, viewType, sortName)` derived
//! a named view's id from `path + "_namedview_" + name`, where `name` is the
//! **localized** display string ("Live TV", "TV en direct", "ライブTV"). A
//! translation update or a change of UI culture therefore minted a fresh view
//! and left every channel and program parented to one nothing looks up any
//! more. 12.0 derives the id from the view *type* instead
//! (`LibraryManager.cs:2997`), and this routine moves whatever the old views
//! accumulated onto that canonical id: children, ancestor rows, per-user
//! display preferences and the `OrderedViews`/`MyMediaExcludes` lists.
//!
//! Ferrofin runs it as a boot repair keyed [`USER_VIEWS_CONSOLIDATED_META_KEY`]
//! in `FerrofinMeta` — once per database, retried on the next boot if it
//! fails (the marker is written inside the same transaction as the moves).
//!
//! Where Ferrofin's view storage differs from what the C# assumes:
//!
//! - a `UserView`'s `ViewType` is not a column but the `ViewType` member of the
//!   `BaseItems.Data` JSON blob (the way both Ferrofin and Jellyfin persist it;
//!   `dto_service` reads the same key). A view whose blob carries no parseable
//!   `ViewType` is the C# `!ViewType.HasValue` case and is skipped;
//! - the C# `Path.GetFileName` splits on the platform separator only; a
//!   database adopted from a Windows Jellyfin stores `…\views\livetv`, so the
//!   last folder is taken after **either** separator, the stance every other
//!   view-path match in this crate takes;
//! - `Guid.TryParse` also accepts the `(…)` and `{0x…}` forms that
//!   [`Uuid::parse_str`] does not; such a token is kept verbatim, which is
//!   exactly what the C# does with a token it cannot parse.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::enums::PreferenceKind;
use ferrofin_db::store::{guid_to_db, opt_datetime_to_db};
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use sqlx::{Sqlite, Transaction};
use uuid::Uuid;

use crate::db_error::db_err;
use crate::item_type_lookup::{self, IdDerivation, stored_type_name};

/// The `FerrofinMeta` key recording that the consolidation has run.
pub const USER_VIEWS_CONSOLIDATED_META_KEY: &str = "user_views_consolidated_v12";

/// The directory a type-wide named view lives in — C# `Path.Combine(
/// InternalMetadataPath, "views", GetValidFilename(viewType.ToString()))`
/// (`LibraryManager.cs:2991-2994`).
#[must_use]
pub fn named_view_path(metadata_path: &Path, view_type: &str) -> PathBuf {
    metadata_path
        .join("views")
        .join(item_type_lookup::valid_filename(view_type))
}

/// The name-independent id of the type-wide named view for `view_type` —
/// `GetNewItemId(path + "_namedview_" + viewType.ToString(), typeof(UserView))`
/// (`LibraryManager.cs:2997`), which is also what
/// `ConsolidateLocalizedUserViews` computes as the canonical id.
///
/// `GetNewItemIdInternal` strips the program-data path before hashing, so the
/// id is the same on two servers with different data directories.
#[must_use]
pub fn named_view_id(mode: &IdDerivation, metadata_path: &Path, view_type: &str) -> Option<Uuid> {
    named_view_id_at(mode, &named_view_path(metadata_path, view_type), view_type)
}

/// [`named_view_id`] over an already-built view directory.
#[must_use]
pub fn named_view_id_at(mode: &IdDerivation, view_path: &Path, view_type: &str) -> Option<Uuid> {
    item_type_lookup::derive_item_id_with(
        mode,
        BaseItemKind::UserView,
        &format!("{}_namedview_{view_type}", view_path.to_string_lossy()),
    )
}

/// Consolidates every localized `UserView` onto its canonical id, once per
/// database. Returns the number of stale views dropped (`0` once the marker
/// is set).
///
/// Per `ViewType` group (`PerformAsync`): the candidates are the views whose
/// path's last folder is the type's valid filename — only the type-wide views
/// are named after it; the per-user and per-parent ones get a folder of their
/// own and carry no children to lose. The canonical id is [`named_view_id`];
/// every other candidate is stale and handed to `consolidate`.
///
/// # Errors
///
/// Returns a [`ServiceError`] when a query or the transaction fails; the
/// marker is written inside that transaction, so a failure retries on the
/// next boot.
pub async fn consolidate_localized_user_views(
    db: &Database,
    mode: &IdDerivation,
    metadata_path: &Path,
) -> Result<u64, ServiceError> {
    let done = db
        .meta_get(USER_VIEWS_CONSOLIDATED_META_KEY)
        .await
        .map_err(|e| ServiceError::Backend(e.to_string()))?;
    if done.as_deref() == Some("1") {
        return Ok(0);
    }

    let mut tx = db.writer().begin().await.map_err(db_err)?;
    let mut dropped = 0u64;
    for (view_type, group) in typed_views(&mut tx).await? {
        let folder = item_type_lookup::valid_filename(&view_type);
        let candidates: Vec<BaseItemEntity> = group
            .into_iter()
            .filter(|view| {
                view.path
                    .as_deref()
                    .and_then(last_folder)
                    .is_some_and(|last| last.eq_ignore_ascii_case(&folder))
            })
            .collect();
        if candidates.is_empty() {
            continue;
        }
        let path = metadata_path.join("views").join(&folder);
        let Some(canonical) = named_view_id_at(mode, &path, &view_type) else {
            continue;
        };
        let stale: Vec<&BaseItemEntity> = candidates
            .iter()
            .filter(|view| stored_id(view) != Some(canonical))
            .collect();
        if stale.is_empty() {
            continue;
        }
        dropped += consolidate(&mut tx, &view_type, &path, canonical, &candidates, &stale).await?;
    }
    sqlx::query(
        r#"INSERT INTO "FerrofinMeta" ("Key", "Value") VALUES (?1, '1')
           ON CONFLICT("Key") DO UPDATE SET "Value" = '1'"#,
    )
    .bind(USER_VIEWS_CONSOLIDATED_META_KEY)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    Ok(dropped)
}

/// Every `UserView` row that has a `ViewType`, grouped by it — the C#
/// `GetItemList(IncludeItemTypes = [UserView]).Where(view => view.ViewType.HasValue)
/// .GroupBy(view => view.ViewType)`. Groups are keyed by the enum's own
/// (lowercase) name, which is what `viewType.ToString()` yields.
async fn typed_views(
    tx: &mut Transaction<'_, Sqlite>,
) -> Result<BTreeMap<String, Vec<BaseItemEntity>>, ServiceError> {
    let Some(user_view) = stored_type_name(BaseItemKind::UserView) else {
        return Ok(BTreeMap::new());
    };
    let rows: Vec<BaseItemEntity> =
        sqlx::query_as(r#"SELECT * FROM "BaseItems" WHERE "Type" = ?1 ORDER BY "Id""#)
            .bind(user_view)
            .fetch_all(&mut **tx)
            .await
            .map_err(db_err)?;
    let mut groups: BTreeMap<String, Vec<BaseItemEntity>> = BTreeMap::new();
    for row in rows {
        if let Some(view_type) = view_type_of(row.data.as_deref()) {
            groups.entry(view_type).or_default().push(row);
        }
    }
    Ok(groups)
}

/// The `ViewType` a stored view carries, as the `CollectionType` member name
/// — read from the `Data` JSON the way the DTO projection reads it. Jellyfin
/// serializes the enum by name (`JsonStringEnumConverter`); the numeric form
/// is accepted too, mapped over the public discriminants, so a blob written
/// by a converter-less serializer still classifies.
fn view_type_of(data: Option<&str>) -> Option<String> {
    /// `Jellyfin.Data.Enums.CollectionType`, discriminants 0–12 in order.
    const PUBLIC_NAMES: [&str; 13] = [
        "unknown",
        "movies",
        "tvshows",
        "music",
        "musicvideos",
        "trailers",
        "homevideos",
        "boxsets",
        "books",
        "photos",
        "livetv",
        "playlists",
        "folders",
    ];
    let blob: serde_json::Value = serde_json::from_str(data?).ok()?;
    match blob.get("ViewType")? {
        serde_json::Value::String(name) if !name.trim().is_empty() => {
            Some(name.trim().to_ascii_lowercase())
        }
        serde_json::Value::Number(disc) => {
            let index = usize::try_from(disc.as_i64()?).ok()?;
            PUBLIC_NAMES.get(index).map(|name| (*name).to_owned())
        }
        _ => None,
    }
}

/// `Path.GetFileName(path.TrimEnd(DirectorySeparatorChar))` after either
/// separator (see the module docs).
fn last_folder(path: &str) -> Option<&str> {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|segment| !segment.is_empty())
}

/// The row's `Id` as a [`Uuid`] (`None` for a malformed stored id, which can
/// then never equal the canonical one).
fn stored_id(view: &BaseItemEntity) -> Option<Uuid> {
    Uuid::parse_str(&view.id).ok()
}

/// Builds a `?, ?, …` placeholder list of length `n` (at least one).
fn placeholders(n: usize) -> String {
    if n == 0 {
        return "NULL".to_owned();
    }
    let mut s = String::with_capacity(n * 3);
    for i in 0..n {
        if i > 0 {
            s.push_str(", ");
        }
        s.push('?');
    }
    s
}

/// `ConsolidateAsync`: promotes a source view onto the canonical id when no
/// candidate carries it, reparents every child, moves the ancestor rows and
/// the user settings, and — last, because `BaseItems` cascades on `ParentId`
/// — deletes the stale views. Returns the number of stale views dropped.
async fn consolidate(
    tx: &mut Transaction<'_, Sqlite>,
    view_type: &str,
    path: &Path,
    canonical: Uuid,
    candidates: &[BaseItemEntity],
    stale: &[&BaseItemEntity],
) -> Result<u64, ServiceError> {
    let stale_ids: Vec<String> = stale.iter().map(|view| view.id.clone()).collect();
    let canonical_db = guid_to_db(canonical);
    let mut source_id: Option<String> = None;

    if !candidates
        .iter()
        .any(|view| stored_id(view) == Some(canonical))
    {
        // Whichever of the old views the items ended up under is the one worth
        // keeping, so give the canonical id a copy of it.
        let source = pick_source(tx, stale, &stale_ids).await?;
        source_id = Some(source.id.clone());
        create_canonical_copy(tx, source, &canonical_db, path).await?;
    }

    let marks = placeholders(stale_ids.len());
    let reparent_sql =
        format!(r#"UPDATE "BaseItems" SET "ParentId" = ? WHERE "ParentId" IN ({marks})"#);
    let mut reparent = sqlx::query(&reparent_sql).bind(&canonical_db);
    for id in &stale_ids {
        reparent = reparent.bind(id);
    }
    let reparented = reparent
        .execute(&mut **tx)
        .await
        .map_err(db_err)?
        .rows_affected();

    let retop_sql =
        format!(r#"UPDATE "BaseItems" SET "TopParentId" = ? WHERE "TopParentId" IN ({marks})"#);
    let mut retop = sqlx::query(&retop_sql).bind(&canonical_db);
    for id in &stale_ids {
        retop = retop.bind(id);
    }
    retop.execute(&mut **tx).await.map_err(db_err)?;

    move_ancestors(tx, &canonical_db, &stale_ids).await?;
    move_user_settings(tx, canonical, source_id.as_deref(), &stale_ids).await?;
    move_remaining_references(tx, &canonical_db, &stale_ids).await?;

    // Nothing points at them any more, and BaseItems cascades on ParentId, so
    // this has to come last.
    let delete_sql = format!(r#"DELETE FROM "BaseItems" WHERE "Id" IN ({marks})"#);
    let mut delete = sqlx::query(&delete_sql);
    for id in &stale_ids {
        delete = delete.bind(id);
    }
    delete.execute(&mut **tx).await.map_err(db_err)?;

    tracing::info!(
        reparented,
        stale = stale_ids.len(),
        view_type,
        canonical_id = %canonical,
        "moved items and dropped stale views in favour of the canonical view"
    );
    Ok(stale_ids.len() as u64)
}

/// 12.1's `MoveRemainingReferencesAsync`: items owned by a stale view are
/// re-owned by the canonical one, and `LinkedChildren` rows naming a stale
/// view on either side are dropped — keyed by `(ParentId, SortOrder)`, they
/// cannot be re-pointed without risking a collision, and a view listing
/// linked children is meaningless anyway.
async fn move_remaining_references(
    tx: &mut sqlx::SqliteConnection,
    canonical_db: &str,
    stale_ids: &[String],
) -> Result<(), ServiceError> {
    let marks = placeholders(stale_ids.len());
    let reown_sql = format!(r#"UPDATE "BaseItems" SET "OwnerId" = ? WHERE "OwnerId" IN ({marks})"#);
    let mut reown = sqlx::query(&reown_sql).bind(canonical_db);
    for id in stale_ids {
        reown = reown.bind(id);
    }
    reown.execute(&mut *tx).await.map_err(db_err)?;
    let unlink_sql = format!(
        r#"DELETE FROM "LinkedChildren" WHERE "ParentId" IN ({marks}) OR "ChildId" IN ({marks})"#
    );
    let mut unlink = sqlx::query(&unlink_sql);
    for id in stale_ids.iter().chain(stale_ids) {
        unlink = unlink.bind(id);
    }
    unlink.execute(&mut *tx).await.map_err(db_err)?;
    Ok(())
}

/// `PickSourceAsync`: the stale view with the most **direct** children
/// (`BaseItems.ParentId`), ties broken by the oldest `DateCreated`. Both
/// orderings are stable, so full ties keep the query order.
async fn pick_source<'a>(
    tx: &mut Transaction<'_, Sqlite>,
    stale: &[&'a BaseItemEntity],
    stale_ids: &[String],
) -> Result<&'a BaseItemEntity, ServiceError> {
    let count_sql = format!(
        r#"SELECT "ParentId", COUNT(*) FROM "BaseItems"
           WHERE "ParentId" IN ({}) GROUP BY "ParentId""#,
        placeholders(stale_ids.len())
    );
    let mut count = sqlx::query_as::<_, (String, i64)>(&count_sql);
    for id in stale_ids {
        count = count.bind(id);
    }
    let child_counts: HashMap<String, i64> = count
        .fetch_all(&mut **tx)
        .await
        .map_err(db_err)?
        .into_iter()
        .collect();
    let mut ordered: Vec<&'a BaseItemEntity> = stale.to_vec();
    ordered.sort_by_key(|view| {
        (
            std::cmp::Reverse(child_counts.get(&view.id).copied().unwrap_or(0)),
            view.date_created,
        )
    });
    ordered
        .first()
        .copied()
        .ok_or_else(|| ServiceError::backend("no stale view to promote"))
}

/// The C# `_libraryManager.CreateItem(new UserView { Path, Id, DateCreated,
/// DateModified, Name, ViewType, ForcedSortName })`: a fresh row carrying only
/// what the routine copies, plus the columns the save path would derive —
/// `SortName`, `PresentationUniqueKey` (the id in `N` form, the default for
/// an item with no override) and the folder flags. `ViewType` travels in the
/// `Data` blob, so the source's blob is what carries it. `TopParentId` is
/// copied too: for the Live TV view it is the view itself, and the caller's
/// stale → canonical rewrite then lands it on the new id, which is what
/// `GetTopParent()` computes for a fresh one.
async fn create_canonical_copy(
    tx: &mut Transaction<'_, Sqlite>,
    source: &BaseItemEntity,
    canonical_db: &str,
    path: &Path,
) -> Result<(), ServiceError> {
    let sort_name = source.sort_name.clone().or_else(|| {
        source
            .forced_sort_name
            .as_deref()
            .map(ferrofin_util::sort_name::forced_sort_key)
            .or_else(|| {
                source
                    .name
                    .as_deref()
                    .map(ferrofin_util::sort_name::create_sort_name)
            })
    });
    let presentation_key = Uuid::parse_str(canonical_db)
        .map(|id| id.as_simple().to_string())
        .map_err(|e| ServiceError::backend(format!("canonical view id: {e}")))?;
    sqlx::query(
        r#"INSERT INTO "BaseItems"
           ("Id", "Type", "Name", "SortName", "ForcedSortName", "Path",
            "DateCreated", "DateModified", "Data", "TopParentId",
            "PresentationUniqueKey", "IsFolder", "IsInMixedFolder", "IsLocked",
            "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem")
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1, 0, 0, 0, 0, 0, 0)"#,
    )
    .bind(canonical_db)
    .bind(&source.type_)
    .bind(&source.name)
    .bind(&sort_name)
    .bind(&source.forced_sort_name)
    .bind(path.to_string_lossy().into_owned())
    .bind(opt_datetime_to_db(source.date_created))
    .bind(opt_datetime_to_db(source.date_modified))
    .bind(&source.data)
    .bind(&source.top_parent_id)
    .bind(&presentation_key)
    .execute(&mut **tx)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// `MoveAncestorsAsync`: every item recorded under a stale view is recorded
/// under the canonical one instead. The pair is the primary key, so anything
/// already recorded against the canonical view stays put.
async fn move_ancestors(
    tx: &mut Transaction<'_, Sqlite>,
    canonical_db: &str,
    stale_ids: &[String],
) -> Result<(), ServiceError> {
    let marks = placeholders(stale_ids.len());
    let select_sql =
        format!(r#"SELECT DISTINCT "ItemId" FROM "AncestorIds" WHERE "ParentItemId" IN ({marks})"#);
    let mut select = sqlx::query_scalar::<_, String>(&select_sql);
    for id in stale_ids {
        select = select.bind(id);
    }
    let items = select.fetch_all(&mut **tx).await.map_err(db_err)?;

    let delete_sql = format!(r#"DELETE FROM "AncestorIds" WHERE "ParentItemId" IN ({marks})"#);
    let mut delete = sqlx::query(&delete_sql);
    for id in stale_ids {
        delete = delete.bind(id);
    }
    delete.execute(&mut **tx).await.map_err(db_err)?;

    if items.is_empty() {
        return Ok(());
    }
    let existing: HashSet<Uuid> = sqlx::query_scalar::<_, String>(
        r#"SELECT "ItemId" FROM "AncestorIds" WHERE "ParentItemId" = ?1"#,
    )
    .bind(canonical_db)
    .fetch_all(&mut **tx)
    .await
    .map_err(db_err)?
    .iter()
    .filter_map(|id| Uuid::parse_str(id).ok())
    .collect();
    for item in items {
        if Uuid::parse_str(&item).is_ok_and(|id| existing.contains(&id)) {
            continue;
        }
        sqlx::query(
            r#"INSERT OR IGNORE INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#,
        )
        .bind(&item)
        .bind(canonical_db)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;
    }
    Ok(())
}

/// The three tables keyed by a view's id that hold per-user display settings.
const DISPLAY_SETTING_TABLES: [&str; 3] = [
    "DisplayPreferences",
    "ItemDisplayPreferences",
    "CustomItemDisplayPreferences",
];

/// `MoveUserSettingsAsync`: a view holding no children still holds the
/// ordering it was given and whether it was hidden. Only the promoted source
/// can hand its display-preference rows over — the rest would collide on the
/// one row per user, item and client — so every other stale view's rows are
/// dropped. Then every `OrderedViews` / `MyMediaExcludes` preference has its
/// stale ids rewritten to the canonical one ([`rewrite_view_preference`]).
async fn move_user_settings(
    tx: &mut Transaction<'_, Sqlite>,
    canonical: Uuid,
    source_id: Option<&str>,
    stale_ids: &[String],
) -> Result<(), ServiceError> {
    let canonical_db = guid_to_db(canonical);
    let dropped: Vec<&String> = stale_ids
        .iter()
        .filter(|id| Some(id.as_str()) != source_id)
        .collect();

    if let Some(moved) = source_id {
        for table in DISPLAY_SETTING_TABLES {
            sqlx::query(&format!(
                r#"UPDATE "{table}" SET "ItemId" = ?1 WHERE "ItemId" = ?2"#
            ))
            .bind(&canonical_db)
            .bind(moved)
            .execute(&mut **tx)
            .await
            .map_err(db_err)?;
        }
    }

    if !dropped.is_empty() {
        let marks = placeholders(dropped.len());
        for table in DISPLAY_SETTING_TABLES {
            let delete_sql = format!(r#"DELETE FROM "{table}" WHERE "ItemId" IN ({marks})"#);
            let mut delete = sqlx::query(&delete_sql);
            for id in &dropped {
                delete = delete.bind(id.as_str());
            }
            delete.execute(&mut **tx).await.map_err(db_err)?;
        }
    }

    let stale: Vec<Uuid> = stale_ids
        .iter()
        .filter_map(|id| Uuid::parse_str(id).ok())
        .collect();
    let preferences: Vec<(i64, String)> =
        sqlx::query_as(r#"SELECT "Id", "Value" FROM "Preferences" WHERE "Kind" IN (?1, ?2)"#)
            .bind(i32::from(PreferenceKind::OrderedViews))
            .bind(i32::from(PreferenceKind::MyMediaExcludes))
            .fetch_all(&mut **tx)
            .await
            .map_err(db_err)?;
    for (id, value) in preferences {
        let Some(rewritten) = rewrite_view_preference(&value, &stale, canonical) else {
            continue;
        };
        // `RowVersion` is the entity's concurrency token, bumped on every
        // save the way `OnSavingChanges` does it.
        sqlx::query(
            r#"UPDATE "Preferences" SET "Value" = ?2, "RowVersion" = "RowVersion" + 1
               WHERE "Id" = ?1"#,
        )
        .bind(id)
        .bind(rewritten)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;
    }
    Ok(())
}

/// Rewrites one `OrderedViews` / `MyMediaExcludes` value: split on `,`,
/// trimmed, empties dropped; each token that parses as a `Guid` and names a
/// stale view becomes the canonical id, de-duplicated **after** substitution
/// (the same view can be listed twice once both of its ids point at the same
/// place); a rewritten token keeps its input's form — dashed (`D`) if it had
/// dashes, else plain (`N`) — while untouched tokens are re-emitted as they
/// were, unparseable ones included. `None` when nothing was stale, so the row
/// is left alone exactly as the C# `if (!touched) continue;` leaves it.
#[must_use]
pub fn rewrite_view_preference(value: &str, stale: &[Uuid], canonical: Uuid) -> Option<String> {
    let mut rewritten: Vec<String> = Vec::new();
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut touched = false;
    for raw in value.split(',') {
        let token = raw.trim();
        if token.is_empty() {
            continue;
        }
        // Clients write these in both the dashed and the plain form, so
        // compare them parsed.
        let Ok(mut parsed) = Uuid::parse_str(token) else {
            rewritten.push(token.to_owned());
            continue;
        };
        let is_stale = stale.contains(&parsed);
        if is_stale {
            parsed = canonical;
            touched = true;
        }
        if !seen.insert(parsed) {
            continue;
        }
        rewritten.push(if is_stale {
            if token.contains('-') {
                parsed.as_hyphenated().to_string()
            } else {
                parsed.as_simple().to_string()
            }
        } else {
            token.to_owned()
        });
    }
    touched.then(|| rewritten.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fetch_item_opt, seed_named_user, test_db};
    use chrono::{DateTime, TimeZone, Utc};
    use ferrofin_db::store::datetime_to_db;
    use rstest::rstest;

    const DATA_DIR: &str = "/config";
    const METADATA: &str = "/config/metadata";

    fn mode() -> IdDerivation {
        IdDerivation::Jellyfin {
            program_data_path: Some(DATA_DIR.to_owned()),
        }
    }

    fn metadata_path() -> PathBuf {
        PathBuf::from(METADATA)
    }

    /// The 10.11 id of a type-wide view: the localized name in the key.
    fn legacy_named_view_id(view_path: &str, name: &str) -> Uuid {
        item_type_lookup::derive_item_id_with(
            &mode(),
            BaseItemKind::UserView,
            &format!("{view_path}_namedview_{name}"),
        )
        .expect("user view id")
    }

    fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 12, 0, 0).unwrap()
    }

    /// A `UserView` row the way 10.11.8 / Ferrofin store one.
    struct ViewRow<'a> {
        id: Uuid,
        name: &'a str,
        path: &'a str,
        view_type: &'a str,
        created: DateTime<Utc>,
    }

    async fn seed_view(db: &Database, row: &ViewRow<'_>) {
        sqlx::query(
            r#"INSERT INTO "BaseItems"
               ("Id", "Type", "Name", "SortName", "ForcedSortName", "Path", "DateCreated",
                "DateModified", "Data", "TopParentId", "IsFolder", "IsInMixedFolder",
                "IsLocked", "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem")
               VALUES (?1, ?2, ?3, lower(?3), ?3, ?4, ?5, ?5, ?6, ?1, 1, 0, 0, 0, 0, 0, 0)"#,
        )
        .bind(guid_to_db(row.id))
        .bind(stored_type_name(BaseItemKind::UserView).unwrap())
        .bind(row.name)
        .bind(row.path)
        .bind(datetime_to_db(row.created))
        .bind(format!(r#"{{"ViewType":"{}"}}"#, row.view_type))
        .execute(db.writer())
        .await
        .expect("insert view");
    }

    async fn seed_child(db: &Database, id: Uuid, kind: BaseItemKind, parent: Uuid) {
        sqlx::query(
            r#"INSERT INTO "BaseItems"
               ("Id", "Type", "Name", "ParentId", "TopParentId", "IsFolder", "IsInMixedFolder",
                "IsLocked", "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem")
               VALUES (?1, ?2, ?3, ?4, ?4, 0, 0, 0, 0, 0, 0, 0)"#,
        )
        .bind(guid_to_db(id))
        .bind(stored_type_name(kind).unwrap())
        .bind(format!("child {id}"))
        .bind(guid_to_db(parent))
        .execute(db.writer())
        .await
        .expect("insert child");
        sqlx::query(r#"INSERT INTO "AncestorIds" ("ItemId", "ParentItemId") VALUES (?1, ?2)"#)
            .bind(guid_to_db(id))
            .bind(guid_to_db(parent))
            .execute(db.writer())
            .await
            .expect("insert ancestor");
    }

    async fn seed_display_settings(db: &Database, user: Uuid, view: Uuid, client: &str) {
        sqlx::query(
            r#"INSERT INTO "DisplayPreferences" ("ChromecastVersion", "Client",
                "EnableNextVideoInfoOverlay", "ItemId", "ScrollDirection", "ShowBackdrop",
                "ShowSidebar", "SkipBackwardLength", "SkipForwardLength", "UserId")
                VALUES (0, ?1, 1, ?2, 0, 0, 1, 10000, 30000, ?3)"#,
        )
        .bind(client)
        .bind(guid_to_db(view))
        .bind(guid_to_db(user))
        .execute(db.writer())
        .await
        .expect("insert display preferences");
        sqlx::query(
            r#"INSERT INTO "ItemDisplayPreferences" ("Client", "ItemId", "RememberIndexing",
                "RememberSorting", "SortBy", "SortOrder", "UserId", "ViewType")
                VALUES (?1, ?2, 0, 1, 'SortName', 0, ?3, 4)"#,
        )
        .bind(client)
        .bind(guid_to_db(view))
        .bind(guid_to_db(user))
        .execute(db.writer())
        .await
        .expect("insert item display preferences");
        sqlx::query(
            r#"INSERT INTO "CustomItemDisplayPreferences" ("Client", "ItemId", "Key",
                "UserId", "Value") VALUES (?1, ?2, 'landing', ?3, 'guide')"#,
        )
        .bind(client)
        .bind(guid_to_db(view))
        .bind(guid_to_db(user))
        .execute(db.writer())
        .await
        .expect("insert custom display preferences");
    }

    async fn seed_preference(db: &Database, user: Uuid, kind: PreferenceKind, value: &str) {
        sqlx::query(
            r#"INSERT INTO "Preferences" ("Kind", "RowVersion", "UserId", "Value")
               VALUES (?1, 1, ?2, ?3)"#,
        )
        .bind(i32::from(kind))
        .bind(guid_to_db(user))
        .bind(value)
        .execute(db.writer())
        .await
        .expect("insert preference");
    }

    async fn preference(db: &Database, user: Uuid, kind: PreferenceKind) -> String {
        sqlx::query_scalar(
            r#"SELECT "Value" FROM "Preferences" WHERE "UserId" = ?1 AND "Kind" = ?2"#,
        )
        .bind(guid_to_db(user))
        .bind(i32::from(kind))
        .fetch_one(db.pool())
        .await
        .expect("preference")
    }

    async fn display_setting_item_ids(db: &Database, table: &str) -> Vec<String> {
        sqlx::query_scalar(&format!(
            r#"SELECT "ItemId" FROM "{table}" ORDER BY "ItemId""#
        ))
        .fetch_all(db.pool())
        .await
        .expect("display setting rows")
    }

    async fn ancestor_pairs(db: &Database) -> Vec<(String, String)> {
        sqlx::query_as(
            r#"SELECT "ItemId", "ParentItemId" FROM "AncestorIds" ORDER BY "ItemId", "ParentItemId""#,
        )
        .fetch_all(db.pool())
        .await
        .expect("ancestor rows")
    }

    /// The Live TV view is the one that hurts: every channel and program is
    /// parented to it, so a localized name used to leave them behind under a
    /// view nothing looks up any more.
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn a_localized_live_tv_view_is_promoted_onto_the_canonical_id() {
        let db = test_db().await;
        let user = Uuid::from_u128(0x71);
        seed_named_user(&db, user, "viewer").await;
        let view_path = "/config/metadata/views/livetv";
        let stale_id = legacy_named_view_id(view_path, "TV en direct");
        let canonical = named_view_id(&mode(), &metadata_path(), "livetv").expect("id");
        assert_ne!(stale_id, canonical);
        seed_view(
            &db,
            &ViewRow {
                id: stale_id,
                name: "TV en direct",
                path: view_path,
                view_type: "livetv",
                created: at(2024, 3, 1),
            },
        )
        .await;
        let channel_a = Uuid::from_u128(0xA1);
        let channel_b = Uuid::from_u128(0xA2);
        let program = Uuid::from_u128(0xA3);
        seed_child(&db, channel_a, BaseItemKind::LiveTvChannel, stale_id).await;
        seed_child(&db, channel_b, BaseItemKind::LiveTvChannel, stale_id).await;
        seed_child(&db, program, BaseItemKind::LiveTvProgram, stale_id).await;
        seed_display_settings(&db, user, stale_id, "emby").await;
        let movies = Uuid::from_u128(0x1111);
        let music = Uuid::from_u128(0x2222);
        seed_preference(
            &db,
            user,
            PreferenceKind::OrderedViews,
            &format!(
                "{}, {} ,{}",
                movies.as_hyphenated(),
                stale_id.as_hyphenated(),
                music.as_simple()
            ),
        )
        .await;
        seed_preference(
            &db,
            user,
            PreferenceKind::MyMediaExcludes,
            &stale_id.as_simple().to_string(),
        )
        .await;

        let dropped = consolidate_localized_user_views(&db, &mode(), &metadata_path())
            .await
            .expect("consolidate");
        assert_eq!(dropped, 1);

        // The canonical view is a copy of the promoted source.
        let view = fetch_item_opt(&db, canonical)
            .await
            .expect("canonical view");
        assert_eq!(view.name.as_deref(), Some("TV en direct"));
        assert_eq!(view.forced_sort_name.as_deref(), Some("TV en direct"));
        assert_eq!(view.path.as_deref(), Some(view_path));
        assert_eq!(view.date_created, Some(at(2024, 3, 1)));
        assert_eq!(view.data.as_deref(), Some(r#"{"ViewType":"livetv"}"#));
        assert_eq!(
            view.top_parent_id.as_deref(),
            Some(guid_to_db(canonical).as_str())
        );
        assert!(
            fetch_item_opt(&db, stale_id).await.is_none(),
            "stale view gone"
        );

        for child in [channel_a, channel_b, program] {
            let row = fetch_item_opt(&db, child).await.expect("child");
            assert_eq!(
                row.parent_id.as_deref(),
                Some(guid_to_db(canonical).as_str())
            );
            assert_eq!(
                row.top_parent_id.as_deref(),
                Some(guid_to_db(canonical).as_str())
            );
        }
        let mut expected: Vec<(String, String)> = [channel_a, channel_b, program]
            .iter()
            .map(|id| (guid_to_db(*id), guid_to_db(canonical)))
            .collect();
        expected.sort();
        assert_eq!(ancestor_pairs(&db).await, expected);

        for table in DISPLAY_SETTING_TABLES {
            assert_eq!(
                display_setting_item_ids(&db, table).await,
                vec![guid_to_db(canonical)],
                "{table} moved to the canonical id"
            );
        }
        assert_eq!(
            preference(&db, user, PreferenceKind::OrderedViews).await,
            format!(
                "{},{},{}",
                movies.as_hyphenated(),
                canonical.as_hyphenated(),
                music.as_simple()
            ),
            "the dashed token stays dashed, the rest are re-emitted as they were"
        );
        assert_eq!(
            preference(&db, user, PreferenceKind::MyMediaExcludes).await,
            canonical.as_simple().to_string(),
            "the plain token stays plain"
        );
        assert_eq!(
            db.meta_get(USER_VIEWS_CONSOLIDATED_META_KEY)
                .await
                .expect("meta")
                .as_deref(),
            Some("1")
        );

        // A second boot is a no-op.
        let again = consolidate_localized_user_views(&db, &mode(), &metadata_path())
            .await
            .expect("second run");
        assert_eq!(again, 0);
        assert_eq!(ancestor_pairs(&db).await, expected);
        assert_eq!(
            preference(&db, user, PreferenceKind::OrderedViews).await,
            format!(
                "{},{},{}",
                movies.as_hyphenated(),
                canonical.as_hyphenated(),
                music.as_simple()
            )
        );
    }

    /// The owner's real 12.0 database: the canonical `Playlists` view exists
    /// beside two stale ones. Nothing is promoted; both duplicates fold into
    /// the canonical view and their display settings are dropped.
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn stale_duplicates_beside_the_canonical_view_are_folded_without_promotion() {
        let db = test_db().await;
        let user = Uuid::from_u128(0x72);
        seed_named_user(&db, user, "viewer").await;
        let canonical = named_view_id(&mode(), &metadata_path(), "playlists").expect("id");
        seed_view(
            &db,
            &ViewRow {
                id: canonical,
                name: "Playlists",
                path: "/config/metadata/views/playlists",
                view_type: "playlists",
                created: at(2026, 9, 1),
            },
        )
        .await;
        // The 10.11 view: folder and key both spelled after the localized name.
        let stale_named = legacy_named_view_id("/config/metadata/views/Playlists", "Playlists");
        seed_view(
            &db,
            &ViewRow {
                id: stale_named,
                name: "Playlists",
                path: "/config/metadata/views/Playlists",
                view_type: "playlists",
                created: at(2025, 1, 1),
            },
        )
        .await;
        // A view adopted from a Windows Jellyfin, trailing separator and all.
        let stale_windows = Uuid::from_u128(0xB2);
        seed_view(
            &db,
            &ViewRow {
                id: stale_windows,
                name: "Wiedergabelisten",
                path: r"C:\ProgramData\Jellyfin\metadata\views\playlists\",
                view_type: "playlists",
                created: at(2025, 6, 1),
            },
        )
        .await;
        // A per-user playlists view lives in a folder named after its id and
        // is never a candidate.
        let per_user = Uuid::from_u128(0xB3);
        seed_view(
            &db,
            &ViewRow {
                id: per_user,
                name: "Playlists",
                path: "/config/metadata/views/000000000000000000000000000000b3",
                view_type: "playlists",
                created: at(2025, 6, 1),
            },
        )
        .await;
        let playlist = Uuid::from_u128(0xC1);
        seed_child(&db, playlist, BaseItemKind::Playlist, stale_named).await;
        seed_display_settings(&db, user, canonical, "web").await;
        seed_display_settings(&db, user, stale_named, "tv").await;
        seed_display_settings(&db, user, stale_windows, "mobile").await;
        seed_preference(
            &db,
            user,
            PreferenceKind::OrderedViews,
            &format!(
                "{},{},{},{}",
                canonical.as_hyphenated(),
                stale_named.as_hyphenated(),
                stale_windows.as_simple(),
                per_user.as_hyphenated()
            ),
        )
        .await;
        seed_preference(&db, user, PreferenceKind::MyMediaExcludes, "").await;

        let dropped = consolidate_localized_user_views(&db, &mode(), &metadata_path())
            .await
            .expect("consolidate");
        assert_eq!(dropped, 2);

        let view = fetch_item_opt(&db, canonical)
            .await
            .expect("canonical view");
        assert_eq!(view.name.as_deref(), Some("Playlists"));
        assert_eq!(view.date_created, Some(at(2026, 9, 1)), "not overwritten");
        assert!(fetch_item_opt(&db, stale_named).await.is_none());
        assert!(fetch_item_opt(&db, stale_windows).await.is_none());
        assert!(
            fetch_item_opt(&db, per_user).await.is_some(),
            "not a candidate"
        );
        let row = fetch_item_opt(&db, playlist).await.expect("playlist");
        assert_eq!(
            row.parent_id.as_deref(),
            Some(guid_to_db(canonical).as_str())
        );
        assert_eq!(
            ancestor_pairs(&db).await,
            vec![(guid_to_db(playlist), guid_to_db(canonical))]
        );
        for table in DISPLAY_SETTING_TABLES {
            assert_eq!(
                display_setting_item_ids(&db, table).await,
                vec![guid_to_db(canonical)],
                "{table}: the canonical view's own row stays, the stale views' rows are dropped"
            );
        }
        assert_eq!(
            preference(&db, user, PreferenceKind::OrderedViews).await,
            format!("{},{}", canonical.as_hyphenated(), per_user.as_hyphenated()),
            "both stale ids fold into the canonical token already listed"
        );
        assert_eq!(
            preference(&db, user, PreferenceKind::MyMediaExcludes).await,
            ""
        );
    }

    /// A view without a `ViewType`, or one whose folder is not the type's, is
    /// outside the routine's remit, and a database with nothing to do still
    /// records the marker.
    #[tokio::test]
    async fn views_outside_the_remit_are_left_alone() {
        let db = test_db().await;
        let untyped = Uuid::from_u128(0xD1);
        sqlx::query(
            r#"INSERT INTO "BaseItems" ("Id", "Type", "Name", "Path", "IsFolder",
               "IsInMixedFolder", "IsLocked", "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem")
               VALUES (?1, ?2, 'Folders', '/config/metadata/views/folders', 1, 0, 0, 0, 0, 0, 0)"#,
        )
        .bind(guid_to_db(untyped))
        .bind(stored_type_name(BaseItemKind::UserView).unwrap())
        .execute(db.writer())
        .await
        .expect("insert untyped view");
        let elsewhere = Uuid::from_u128(0xD2);
        seed_view(
            &db,
            &ViewRow {
                id: elsewhere,
                name: "Movies",
                path: "/config/metadata/views/38_namedview_movies",
                view_type: "movies",
                created: at(2025, 1, 1),
            },
        )
        .await;

        let dropped = consolidate_localized_user_views(&db, &mode(), &metadata_path())
            .await
            .expect("consolidate");
        assert_eq!(dropped, 0);
        assert!(fetch_item_opt(&db, untyped).await.is_some());
        assert!(fetch_item_opt(&db, elsewhere).await.is_some());
        assert_eq!(
            db.meta_get(USER_VIEWS_CONSOLIDATED_META_KEY)
                .await
                .expect("meta")
                .as_deref(),
            Some("1")
        );
    }

    /// `PickSourceAsync`: most direct children wins, ties go to the oldest.
    #[tokio::test]
    async fn the_source_is_the_stale_view_with_the_most_children_then_the_oldest() {
        let db = test_db().await;
        let path = "/config/metadata/views/livetv";
        let older_empty = legacy_named_view_id(path, "Live TV");
        let newer_full = legacy_named_view_id(path, "TV en direct");
        let oldest_empty = legacy_named_view_id(path, "ライブTV");
        for (id, name, created) in [
            (older_empty, "Live TV", at(2023, 1, 1)),
            (newer_full, "TV en direct", at(2025, 1, 1)),
            (oldest_empty, "ライブTV", at(2022, 1, 1)),
        ] {
            seed_view(
                &db,
                &ViewRow {
                    id,
                    name,
                    path,
                    view_type: "livetv",
                    created,
                },
            )
            .await;
        }
        seed_child(
            &db,
            Uuid::from_u128(0xE1),
            BaseItemKind::LiveTvChannel,
            newer_full,
        )
        .await;

        consolidate_localized_user_views(&db, &mode(), &metadata_path())
            .await
            .expect("consolidate");
        let canonical = named_view_id(&mode(), &metadata_path(), "livetv").expect("id");
        let view = fetch_item_opt(&db, canonical).await.expect("canonical");
        assert_eq!(
            view.name.as_deref(),
            Some("TV en direct"),
            "children beat age"
        );

        // And with no children anywhere, the oldest is the source.
        let db = test_db().await;
        for (id, name, created) in [
            (older_empty, "Live TV", at(2023, 1, 1)),
            (oldest_empty, "ライブTV", at(2022, 1, 1)),
        ] {
            seed_view(
                &db,
                &ViewRow {
                    id,
                    name,
                    path,
                    view_type: "livetv",
                    created,
                },
            )
            .await;
        }
        consolidate_localized_user_views(&db, &mode(), &metadata_path())
            .await
            .expect("consolidate");
        let view = fetch_item_opt(&db, canonical).await.expect("canonical");
        assert_eq!(view.name.as_deref(), Some("ライブTV"));
    }

    #[rstest]
    #[case::untouched_row_is_left_alone("aaaaaaaa-0000-0000-0000-000000000001,junk", None)]
    #[case::dashed_stays_dashed(
        "00000000-0000-0000-0000-00000000000a",
        Some("ffffffff-ffff-ffff-ffff-ffffffffffff")
    )]
    #[case::plain_stays_plain(
        "0000000000000000000000000000000a",
        Some("ffffffffffffffffffffffffffffffff")
    )]
    #[case::deduped_after_substitution(
        "0000000000000000000000000000000a,00000000-0000-0000-0000-00000000000b,ffffffff-ffff-ffff-ffff-ffffffffffff",
        Some("ffffffffffffffffffffffffffffffff")
    )]
    #[case::unparseable_and_untouched_tokens_kept_verbatim(
        " junk , 0000000000000000000000000000000A ,, AAAAAAAA-0000-0000-0000-000000000001 ",
        Some("junk,ffffffffffffffffffffffffffffffff,AAAAAAAA-0000-0000-0000-000000000001")
    )]
    fn preference_values_are_rewritten_like_move_user_settings(
        #[case] value: &str,
        #[case] expected: Option<&str>,
    ) {
        let stale = [Uuid::from_u128(0xA), Uuid::from_u128(0xB)];
        let canonical = Uuid::from_u128(u128::MAX);
        assert_eq!(
            rewrite_view_preference(value, &stale, canonical).as_deref(),
            expected
        );
    }

    #[rstest]
    #[case("/config/metadata/views/livetv", Some("livetv"))]
    #[case("/config/metadata/views/livetv/", Some("livetv"))]
    #[case(r"C:\ProgramData\Jellyfin\metadata\views\livetv", Some("livetv"))]
    #[case(r"C:\ProgramData\Jellyfin\metadata\views\livetv\", Some("livetv"))]
    #[case("", None)]
    #[case("/", None)]
    fn last_folder_takes_the_segment_after_either_separator(
        #[case] path: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(last_folder(path), expected);
    }

    #[rstest]
    #[case(Some(r#"{"ViewType":"livetv"}"#), Some("livetv"))]
    #[case(Some(r#"{"ViewType":"LiveTv"}"#), Some("livetv"))]
    #[case(Some(r#"{"ViewType":10}"#), Some("livetv"))]
    #[case(Some(r#"{"ViewType":""}"#), None)]
    #[case(Some(r#"{"ViewType":null}"#), None)]
    #[case(Some(r#"{"DisplayParentId":"x"}"#), None)]
    #[case(Some("not json"), None)]
    #[case(None, None)]
    fn view_type_is_read_from_the_data_blob(
        #[case] data: Option<&str>,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(view_type_of(data).as_deref(), expected);
    }

    /// The canonical id strips the program-data path, so servers with
    /// different data directories agree on it, and it no longer depends on
    /// the localized name.
    #[test]
    fn named_view_id_is_data_dir_independent_and_name_free() {
        let mut ids = HashSet::new();
        for data_dir in ["/config", "/data", "/var/lib/ferrofin"] {
            let mode = IdDerivation::Jellyfin {
                program_data_path: Some(data_dir.to_owned()),
            };
            let metadata = Path::new(data_dir).join("metadata");
            ids.insert(named_view_id(&mode, &metadata, "livetv").expect("id"));
        }
        assert_eq!(ids.len(), 1);
        let canonical = ids.into_iter().next().unwrap();
        assert_ne!(
            canonical,
            legacy_named_view_id("/config/metadata/views/livetv", "Live TV")
        );
        assert_eq!(
            named_view_path(Path::new("/config/metadata"), "livetv"),
            PathBuf::from("/config/metadata/views/livetv")
        );
    }
}
