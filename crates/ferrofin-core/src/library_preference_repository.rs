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
/// case as C# `FindLibrary` does (`OrdinalIgnoreCase`), as `(id, name)`. An
/// exact match wins over a row that differs only in case.
///
/// The comparison runs here, not in SQL: SQLite's `NOCASE` folds ASCII only,
/// and there is one row per library.
///
/// # Errors
/// Returns [`ServiceError::Db`] if the query fails.
pub(crate) async fn library_at(
    db: &Database,
    path: &str,
) -> Result<Option<(Uuid, Option<String>)>, ServiceError> {
    let rows: Vec<(String, Option<String>, Option<String>)> =
        sqlx::query_as(r#"SELECT "Id", "Name", "Path" FROM "BaseItems" WHERE "Type" = ?1"#)
            .bind(stored_type_name(BaseItemKind::CollectionFolder).unwrap_or_default())
            .fetch_all(db.pool())
            .await
            .map_err(db_err)?;
    let at = |exact: bool| {
        rows.iter().find(|(_, _, stored)| {
            stored.as_deref().is_some_and(|stored| {
                if exact {
                    stored == path
                } else {
                    ferrofin_util::string_extensions::equals_ordinal_ignore_case(stored, path)
                }
            })
        })
    };
    Ok(at(true)
        .or_else(|| at(false))
        .and_then(|(id, name, _)| Some((Uuid::parse_str(id).ok()?, name.clone()))))
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
    // It reads before it writes, and a boot repair calls it while the previous
    // version may be writing: take the lock first (CONTRIBUTING.md).
    let mut tx = db
        .writer()
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(db_err)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{seed_named_item, set_item_path, test_db};

    /// C# `FindLibrary` compares the stored path with `OrdinalIgnoreCase`,
    /// which folds every letter, not only ASCII; an exact match still wins.
    #[tokio::test]
    async fn library_at_folds_case_beyond_ascii() {
        let db = test_db().await;
        let (folded, exact) = (Uuid::from_u128(0xE1A1), Uuid::from_u128(0xE1A2));
        seed_named_item(&db, folded, BaseItemKind::CollectionFolder, "Élan").await;
        set_item_path(&db, folded, "/root/default/Élan").await;
        assert_eq!(
            library_at(&db, "/root/default/élan").await.expect("query"),
            Some((folded, Some("Élan".to_owned())))
        );
        seed_named_item(&db, exact, BaseItemKind::CollectionFolder, "élan").await;
        set_item_path(&db, exact, "/root/default/élan").await;
        assert_eq!(
            library_at(&db, "/root/default/élan")
                .await
                .expect("query")
                .map(|(id, _)| id),
            Some(exact)
        );
    }
}
