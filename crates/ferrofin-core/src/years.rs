//! [`YearStore`] — materializes `Year` by-name item rows.
//!
//! Port of `LibraryManager.GetYear(value)` → `CreateItemByName<Year>(
//! Year.GetPath, name, …)`: a year's item lives at
//! `{metadata}/Year/{name}` (`ApplicationPaths.YearPath`), its id is
//! `GetItemByNameId<Year>(path)` ([`item_type_lookup::year_item_id`]), and on
//! first use the directory is created and the row persisted with
//! `Name = "1999"`, `Path`, `DateCreated`/`DateModified` from the directory.
//! Jellyfin creates them lazily — `GET /Years` maps every distinct
//! `ProductionYear` through `GetYear`, and `GET /Years/{year}` calls it for
//! one — so a year always resolves, scanned or not.
//!
//! Ferrofin does the same on read ([`crate::library_manager`] resolves a
//! missing `Year` through [`YearStore::ensure_missing`]) and, additionally,
//! materializes every scanned production year at the end of a library scan so
//! the read path is write-free in the steady state.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::persistence::ItemPersistenceService;

use crate::item_type_lookup::{self, IdDerivation};
use crate::library_scan::create_sort_name;

/// Creates `Year` item rows by production year.
#[derive(Clone)]
pub struct YearStore {
    persistence: Arc<dyn ItemPersistenceService>,
    id_derivation: IdDerivation,
    year_root: PathBuf,
}

impl std::fmt::Debug for YearStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("YearStore")
            .field("year_root", &self.year_root)
            .finish_non_exhaustive()
    }
}

impl YearStore {
    /// Creates the store over the item writer, the database's id derivation
    /// mode, and the Year metadata directory (`ApplicationPaths.YearPath`).
    #[must_use]
    pub fn new(
        persistence: Arc<dyn ItemPersistenceService>,
        id_derivation: IdDerivation,
        year_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            persistence,
            id_derivation,
            year_root: year_path.into(),
        }
    }

    /// The metadata path of the year's item (`Year.GetPath(name)`).
    fn path_of(&self, name: &str) -> String {
        item_type_lookup::year_path(&self.year_root.to_string_lossy(), name)
    }

    /// The deterministic id of the year's item (`GetItemByNameId<Year>`).
    #[must_use]
    pub fn id_of(&self, year: i32) -> Option<Uuid> {
        item_type_lookup::year_item_id(
            &self.id_derivation,
            &self.year_root.to_string_lossy(),
            &year.to_string(),
        )
    }

    /// The row `CreateItemByName<Year>` persists: the year as `Name`, the
    /// sort key Jellyfin's `CreateSortName` pads digits to (`0000001999`), the
    /// metadata path, not a folder.
    fn entity(&self, id: Uuid, year: i32) -> BaseItemEntity {
        let name = year.to_string();
        let now = Utc::now();
        BaseItemEntity {
            id: guid_to_db(id),
            type_: item_type_lookup::stored_type_name(BaseItemKind::Year)
                .unwrap_or_default()
                .to_owned(),
            sort_name: Some(create_sort_name(&name)),
            path: Some(self.path_of(&name)),
            name: Some(name),
            is_folder: false,
            date_created: Some(now),
            date_modified: Some(now),
            ..BaseItemEntity::default()
        }
    }

    /// Creates the rows (and directories) for every year in `years` that has
    /// none yet, returning the rows it created. Non-positive years are
    /// skipped (`GetYear` rejects them). Existing rows are left untouched, so
    /// this is safe to call on every read.
    ///
    /// # Errors
    ///
    /// Returns an error if a directory cannot be created or a row cannot be
    /// written.
    pub async fn ensure_missing(&self, years: &[i32]) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let mut created = Vec::new();
        for &year in years {
            if year <= 0 {
                continue;
            }
            let Some(id) = self.id_of(year) else {
                continue;
            };
            if self.persistence.item_exists(id).await? {
                continue;
            }
            let entity = self.entity(id, year);
            if let Some(path) = entity.path.as_deref() {
                tokio::fs::create_dir_all(path)
                    .await
                    .map_err(|e| ServiceError::backend(format!("create year directory: {e}")))?;
            }
            self.persistence
                .save_items(std::slice::from_ref(&entity))
                .await?;
            created.push(entity);
        }
        if !created.is_empty() {
            tracing::debug!(count = created.len(), "materialized year items");
        }
        Ok(created)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_persistence_service::FerrofinItemPersistenceService;
    use crate::test_support::{fetch_item, item_repository_over, test_db};
    use ferrofin_traits::options::InternalItemsQuery;

    async fn store(tmp: &tempfile::TempDir) -> (ferrofin_db::Database, YearStore) {
        let db = test_db().await;
        let persistence: Arc<dyn ItemPersistenceService> =
            Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let mode = IdDerivation::Jellyfin {
            program_data_path: Some(tmp.path().to_string_lossy().into_owned()),
        };
        let s = YearStore::new(persistence, mode, tmp.path().join("metadata/Year"));
        (db, s)
    }

    #[tokio::test]
    async fn ensure_missing_creates_year_rows_with_path_derived_ids() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (db, store) = store(&tmp).await;
        let created = store
            .ensure_missing(&[1999, 0, -5, 2004])
            .await
            .expect("ensure");
        assert_eq!(created.len(), 2, "non-positive years are skipped");
        assert!(tmp.path().join("metadata/Year/1999").is_dir());
        // The id is Jellyfin's normalized by-name derivation, data-dir relative.
        let expected = item_type_lookup::by_name_item_id(
            &IdDerivation::Jellyfin {
                program_data_path: Some("/any".to_owned()),
            },
            BaseItemKind::Year,
            "/any/metadata/Year/1999",
        )
        .expect("derived");
        assert_eq!(store.id_of(1999), Some(expected));
        let row = fetch_item(&db, expected).await;
        assert_eq!(row.type_, "MediaBrowser.Controller.Entities.Year");
        assert_eq!(row.name.as_deref(), Some("1999"));
        assert_eq!(row.sort_name.as_deref(), Some("0000001999"));
        assert!(!row.is_folder);
        assert_eq!(row.clean_name.as_deref(), Some("1999"));
        // Idempotent: nothing new for years that already have rows.
        let again = store.ensure_missing(&[1999, 2004]).await.expect("again");
        assert!(again.is_empty());
        let years = item_repository_over(db.clone())
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Year],
                ..InternalItemsQuery::default()
            })
            .await
            .expect("list");
        assert_eq!(years.len(), 2);
    }
}
