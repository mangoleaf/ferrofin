//! [`ByNameStore`] — materializes `Genre` / `MusicGenre` / `Studio` /
//! `MusicArtist` by-name item rows on demand.
//!
//! Port of `LibraryManager.CreateItemByName<T>` (`LibraryManager.cs:1289`), the
//! shared body behind `GetGenre`, `GetMusicGenre`, `GetStudio` and `GetArtist`.
//! Those are **not** read-only lookups in Jellyfin: the item lives at
//! `{metadata}/<Kind>/{name}` and, when no row matches, the C# creates the
//! directory and persists the row *as a side effect of the GET*. That is why
//! `GET /MusicGenres/{anything}` is a 200 upstream and never a 404 — the
//! controller's `if (item is null) return NotFound()` arm is unreachable here.
//!
//! Ferrofin had ported that for `Year` only ([`crate::years::YearStore`]); the
//! rest of the family 404'd. This store closes it, keeping `YearStore`'s shape:
//! derive the path, create the directory, write the row. The row write itself
//! lives in [`ItemPersistenceService::ensure_by_name_item`], which owns the id
//! rule — the id a *scan* would mint for the same name, so the two paths
//! converge on one row instead of listing the name twice.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use uuid::Uuid;

use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::persistence::ItemPersistenceService;

use crate::item_type_lookup;

/// The by-name kinds this store provisions — the four `CreateItemByName`
/// callers other than `Year`, which [`crate::years::YearStore`] owns.
pub const PROVISIONED_KINDS: &[BaseItemKind] = &[
    BaseItemKind::Genre,
    BaseItemKind::MusicGenre,
    BaseItemKind::Studio,
    BaseItemKind::MusicArtist,
];

/// Creates by-name item rows (`Genre`, `MusicGenre`, `Studio`, `MusicArtist`).
#[derive(Clone)]
pub struct ByNameStore {
    persistence: Arc<dyn ItemPersistenceService>,
    roots: HashMap<BaseItemKind, PathBuf>,
}

impl std::fmt::Debug for ByNameStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ByNameStore")
            .field("roots", &self.roots)
            .finish_non_exhaustive()
    }
}

impl ByNameStore {
    /// Creates the store over the item writer and the four metadata directories
    /// (`ApplicationPaths.{Genre,MusicGenre,Studio,Artists}Path`).
    #[must_use]
    pub fn new(
        persistence: Arc<dyn ItemPersistenceService>,
        genre_path: impl Into<PathBuf>,
        music_genre_path: impl Into<PathBuf>,
        studio_path: impl Into<PathBuf>,
        artists_path: impl Into<PathBuf>,
    ) -> Self {
        let roots = HashMap::from([
            (BaseItemKind::Genre, genre_path.into()),
            (BaseItemKind::MusicGenre, music_genre_path.into()),
            (BaseItemKind::Studio, studio_path.into()),
            (BaseItemKind::MusicArtist, artists_path.into()),
        ]);
        Self { persistence, roots }
    }

    /// The metadata path of `name`'s item for `kind` (`<T>.GetPath(name)`), or
    /// [`None`] for a kind this store does not provision.
    #[must_use]
    pub fn path_of(&self, kind: BaseItemKind, name: &str) -> Option<String> {
        let root = self.roots.get(&kind)?;
        Some(item_type_lookup::by_name_path(
            &root.to_string_lossy(),
            name.trim(),
        ))
    }

    /// `CreateItemByName<T>(name)`: the id of `name`'s row for `kind`, creating
    /// the metadata directory and the row when nothing matches yet.
    ///
    /// Returns [`None`] for an unprovisioned kind or a blank name, so the caller
    /// falls through to its own behaviour.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created or the row cannot be
    /// written.
    pub async fn ensure(
        &self,
        kind: BaseItemKind,
        name: &str,
    ) -> Result<Option<Uuid>, ServiceError> {
        let name = name.trim();
        let Some(path) = self.path_of(kind, name).filter(|_| !name.is_empty()) else {
            return Ok(None);
        };
        let id = self
            .persistence
            .ensure_by_name_item(kind, name, &path)
            .await?;
        if id.is_some() {
            // `Directory.CreateDirectory(path)` — the C# does it before the
            // write; doing it after keeps a filesystem failure from being the
            // reason a lookup 404s.
            tokio::fs::create_dir_all(&path)
                .await
                .map_err(|e| ServiceError::backend(format!("create by-name directory: {e}")))?;
        }
        Ok(id)
    }

    /// Fills in the metadata `Path` of by-name rows that have none.
    ///
    /// A scan materializes `Genre`/`Studio`/`MusicArtist` rows keyed by their
    /// `ItemValues` id and `MusicGenre` rows by a derived id, and neither insert
    /// wrote a `Path` — but Jellyfin's equivalent row always has one
    /// (`/config/metadata/MusicGenre/Jazz`) and `DtoService` emits it
    /// unconditionally, so the field was missing from every by-name DTO.
    /// Idempotent: only `Path IS NULL` rows are read, so the steady state is one
    /// indexed probe per kind.
    ///
    /// # Errors
    ///
    /// Returns an error if a row cannot be read or written.
    pub async fn backfill_paths(&self) -> Result<usize, ServiceError> {
        let mut filled = 0;
        for &kind in PROVISIONED_KINDS {
            for (id, name) in self.persistence.by_name_rows_without_path(kind).await? {
                let Some(path) = self.path_of(kind, &name) else {
                    continue;
                };
                self.persistence.set_item_path(id, &path).await?;
                filled += 1;
            }
        }
        if filled > 0 {
            tracing::debug!(count = filled, "backfilled by-name item paths");
        }
        Ok(filled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_persistence_service::FerrofinItemPersistenceService;
    use crate::test_support::{fetch_item, test_db};

    async fn store(tmp: &tempfile::TempDir) -> (ferrofin_db::Database, ByNameStore) {
        let db = test_db().await;
        let persistence: Arc<dyn ItemPersistenceService> =
            Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let meta = tmp.path().join("metadata");
        let s = ByNameStore::new(
            persistence,
            meta.join("Genre"),
            meta.join("MusicGenre"),
            meta.join("Studio"),
            meta.join("artists"),
        );
        (db, s)
    }

    #[tokio::test]
    async fn ensure_creates_the_row_and_its_metadata_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (db, store) = store(&tmp).await;

        let id = store
            .ensure(BaseItemKind::MusicGenre, "Jazz")
            .await
            .expect("ensure")
            .expect("a row");
        assert!(tmp.path().join("metadata/MusicGenre/Jazz").is_dir());
        let row = fetch_item(&db, id).await;
        assert_eq!(
            row.type_,
            "MediaBrowser.Controller.Entities.Audio.MusicGenre"
        );
        assert_eq!(row.name.as_deref(), Some("Jazz"));
        assert_eq!(row.sort_name.as_deref(), Some("jazz"));
        assert_eq!(row.clean_name.as_deref(), Some("jazz"));
        assert_eq!(
            row.path.as_deref(),
            store.path_of(BaseItemKind::MusicGenre, "Jazz").as_deref()
        );
        assert_eq!(
            row.presentation_unique_key.as_deref(),
            Some("MusicGenre-Jazz")
        );
        // `MusicGenre : BaseItem`, not `Folder`.
        assert!(!row.is_folder);
    }

    #[tokio::test]
    async fn ensure_is_idempotent_and_covers_the_whole_family() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (_db, store) = store(&tmp).await;
        for &kind in PROVISIONED_KINDS {
            let first = store.ensure(kind, "Zzznope").await.expect("ensure");
            let second = store.ensure(kind, "Zzznope").await.expect("ensure again");
            assert!(first.is_some(), "{kind:?} is provisioned");
            assert_eq!(first, second, "{kind:?} resolves to one row");
        }
        // A kind this store does not own falls through rather than inventing a row.
        assert!(
            store
                .ensure(BaseItemKind::Movie, "Zzznope")
                .await
                .expect("ensure")
                .is_none()
        );
        assert!(
            store
                .ensure(BaseItemKind::MusicGenre, "   ")
                .await
                .expect("ensure")
                .is_none()
        );
    }

    #[tokio::test]
    async fn ensure_adopts_a_row_the_scan_already_made() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (db, store) = store(&tmp).await;
        // The shape a scan writes: a row under its own id, keyed by CleanName.
        // Written through the production upsert, which is what derives
        // `CleanName` — the column this adoption check matches on.
        let scanned = Uuid::from_u128(0x2001);
        let persistence: Arc<dyn ItemPersistenceService> =
            Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        persistence
            .save_items(&[ferrofin_db::entities::base_items::BaseItemEntity {
                id: ferrofin_db::store::guid_to_db(scanned),
                type_: item_type_lookup::stored_type_name(BaseItemKind::Genre)
                    .expect("type")
                    .to_owned(),
                name: Some("Action".to_owned()),
                ..Default::default()
            }])
            .await
            .expect("seed");

        // Same cleaned name, different spelling — the existing row still wins,
        // so `/Genres` cannot list "Action" twice.
        assert_eq!(
            store
                .ensure(BaseItemKind::Genre, "action")
                .await
                .expect("ensure"),
            Some(scanned)
        );
    }

    #[tokio::test]
    async fn ensures_id_is_the_one_a_later_scan_would_mint() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (db, store) = store(&tmp).await;
        let lazy = store
            .ensure(BaseItemKind::Genre, "Zzznope")
            .await
            .expect("ensure")
            .expect("a row");
        // Now content carrying that genre is scanned. The convergence rule:
        // the value's `ItemValueId` IS the row's id, so the scan's by-name
        // insert is a no-op instead of a second "Zzznope" in `/Genres`.
        let movie = Uuid::from_u128(0x3001);
        crate::test_support::seed_named_item(&db, movie, BaseItemKind::Movie, "A Movie").await;
        crate::test_support::seed_item_genre(&db, movie, "Zzznope").await;
        let rows = crate::test_support::item_repository_over(db.clone())
            .get_item_list(&ferrofin_traits::options::InternalItemsQuery {
                names: vec!["Zzznope".to_owned()],
                include_item_types: vec![BaseItemKind::Genre],
                ..ferrofin_traits::options::InternalItemsQuery::default()
            })
            .await
            .expect("rows");
        assert_eq!(rows.len(), 1, "one Genre row for the name, not two");
        assert_eq!(rows[0].id, ferrofin_db::store::guid_to_db(lazy));
    }

    #[tokio::test]
    async fn backfill_paths_fills_scanner_written_rows_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (db, store) = store(&tmp).await;
        let id = Uuid::from_u128(0x2001);
        crate::test_support::seed_named_item(&db, id, BaseItemKind::Genre, "Action").await;
        assert_eq!(store.backfill_paths().await.expect("backfill"), 1);
        let row = fetch_item(&db, id).await;
        assert_eq!(
            row.path.as_deref(),
            store.path_of(BaseItemKind::Genre, "Action").as_deref()
        );
        // Idempotent: the second pass has nothing left to fill.
        assert_eq!(store.backfill_paths().await.expect("backfill"), 0);
    }
}
