//! The SQL behind remapping users' library preferences when a library is
//! renamed (C# `LibraryStructureController.ReplaceLibraryIdInUserPreferencesAsync`).

use ferrofin_db::Database;
use ferrofin_db::enums::PreferenceKind;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use uuid::Uuid;

use crate::db_error::db_err;
use crate::item_type_lookup::stored_type_name;
use crate::user_entity_ext::{get_preference, set_preference_tx};

/// The stored `CollectionFolder` row whose path is `path`, compared ignoring
/// case as C# `FindLibrary` does, as `(id, name)`. An exact match wins over a
/// row that differs only in case.
///
/// # Errors
/// Returns [`ServiceError::Db`] if the query fails.
pub(crate) async fn library_at(
    db: &Database,
    path: &str,
) -> Result<Option<(Uuid, Option<String>)>, ServiceError> {
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        r#"SELECT "Id", "Name" FROM "BaseItems"
           WHERE "Type" = ?1 AND "Path" = ?2 COLLATE NOCASE
           ORDER BY "Path" = ?2 DESC LIMIT 1"#,
    )
    .bind(stored_type_name(BaseItemKind::CollectionFolder).unwrap_or_default())
    .bind(path)
    .fetch_optional(db.pool())
    .await
    .map_err(db_err)?;
    Ok(row.and_then(|(id, name)| Some((Uuid::parse_str(&id).ok()?, name))))
}

/// Offers every user's values for each of `kinds` to `rewrite`, and stores the
/// replacement it returns. All users are rewritten in one writer transaction,
/// so a concurrent preference save cannot interleave with the remap.
///
/// Returns the number of users with at least one rewritten preference.
///
/// # Errors
/// Returns [`ServiceError::Db`] if a query fails; nothing is written then.
pub(crate) async fn rewrite_preferences(
    db: &Database,
    kinds: &[PreferenceKind],
    mut rewrite: impl FnMut(Uuid, PreferenceKind, &[String]) -> Option<Vec<String>>,
) -> Result<usize, ServiceError> {
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    let users: Vec<String> = sqlx::query_scalar(r#"SELECT "Id" FROM "Users""#)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
    let mut rewritten = 0;
    for user in &users {
        let Ok(user_id) = Uuid::parse_str(user) else {
            continue;
        };
        let mut changed = false;
        for &kind in kinds {
            let values = get_preference(&mut *tx, user, kind).await?;
            if let Some(replacement) = rewrite(user_id, kind, &values) {
                set_preference_tx(&mut tx, user, kind, &replacement).await?;
                changed = true;
            }
        }
        rewritten += usize::from(changed);
    }
    tx.commit().await.map_err(db_err)?;
    Ok(rewritten)
}
