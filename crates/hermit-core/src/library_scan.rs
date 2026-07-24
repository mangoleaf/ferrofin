//! [`LibraryScanner`] — the filesystem → item-store scan.
//!
//! Walks every virtual folder's media paths and materializes typed
//! [`BaseItemEntity`] rows under the library's `CollectionFolder`, linking each
//! into the `AncestorIds` closure so the library listing
//! (`GET /Items?ParentId=<library>&Recursive=true`) is populated and items
//! direct-play.
//!
//! **v1 scope: movies.** Every video file under a `movies`/`homevideos`/mixed
//! library becomes a `Movie` parented to the collection folder (flattening any
//! nested folders, which is how a movie library presents anyway). The
//! `tvshows` (Series→Season→Episode) and `music` (MusicAlbum→Audio) resolver
//! chains, pruning of deleted files, and remote-metadata refresh are follow-ups
//! (see `brain/plans/PLAN_HERMIT_LIBRARY_SCAN.md`).
//!
//! Two passes: a **synchronous plan** (walk + filename resolution — this is where
//! the `!Sync` [`NamingOptions`] lazy-regex cells live, so they never cross an
//! `.await`), then an **async persist**. The filesystem seam is synchronous, so
//! the whole walk fits the sync pass.

use std::sync::Arc;

use chrono::Utc;
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_model::data::BaseItemKind;
use hermit_model::entities::CollectionTypeOptions;
use hermit_model::entities_media::VirtualFolderInfo;
use hermit_model::io::FileSystemEntryType;
use hermit_naming::common::NamingOptions;
use hermit_naming::video::video_resolver;
use hermit_traits::error::ServiceError;
use hermit_traits::filesystem::FileSystem;
use hermit_traits::library::VirtualFolderManager;
use hermit_traits::persistence::ItemPersistenceService;
use uuid::Uuid;

use crate::item_type_lookup;

/// One item the plan pass resolved, ready to persist.
struct Planned {
    /// The item id (also `entity.id`, kept typed for `set_ancestors`).
    id: Uuid,
    entity: BaseItemEntity,
    /// The ancestor closure (`ParentId` chain up to the collection folder).
    ancestors: Vec<Uuid>,
}

/// Walks configured libraries and persists their contents as item rows.
pub struct LibraryScanner {
    virtual_folders: Arc<dyn VirtualFolderManager>,
    file_system: Arc<dyn FileSystem>,
    persistence: Arc<dyn ItemPersistenceService>,
}

impl std::fmt::Debug for LibraryScanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibraryScanner").finish_non_exhaustive()
    }
}

impl LibraryScanner {
    /// Builds a scanner over the library + filesystem + item-store seams.
    #[must_use]
    pub fn new(
        virtual_folders: Arc<dyn VirtualFolderManager>,
        file_system: Arc<dyn FileSystem>,
        persistence: Arc<dyn ItemPersistenceService>,
    ) -> Self {
        Self {
            virtual_folders,
            file_system,
            persistence,
        }
    }

    /// Scans every configured library; returns the number of items created.
    ///
    /// Idempotent: item ids are deterministic
    /// ([`derive_item_id`](item_type_lookup::derive_item_id)), so re-scanning
    /// upserts rather than duplicates.
    ///
    /// # Errors
    /// Propagates the item-store failure if listing libraries, saving an item,
    /// or writing its ancestor closure fails.
    pub async fn scan_all(&self) -> Result<usize, ServiceError> {
        let folders = self.virtual_folders.get_virtual_folders().await?;
        let planned = self.plan(&folders); // sync: NamingOptions never crosses an await
        for item in &planned {
            self.persistence
                .save_items(std::slice::from_ref(&item.entity))
                .await?;
            self.persistence
                .set_ancestors(item.id, &item.ancestors)
                .await?;
        }
        Ok(planned.len())
    }

    /// The synchronous plan pass: resolve every library's files into [`Planned`]
    /// items. Owns the `NamingOptions` so its `!Sync` cells stay off the async path.
    fn plan(&self, folders: &[VirtualFolderInfo]) -> Vec<Planned> {
        let naming = NamingOptions::new();
        let mut out = Vec::new();
        for folder in folders {
            // `item_id` is the library's CollectionFolder id (projected by the
            // virtual-folder manager); items hang beneath it.
            let Some(cf_id) = folder
                .item_id
                .as_deref()
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                continue;
            };
            for location in &folder.locations {
                self.plan_dir(
                    location,
                    cf_id,
                    &[cf_id],
                    folder.collection_type,
                    &naming,
                    &mut out,
                );
            }
        }
        out
    }

    /// Recursively plans one directory, emitting items parented to the library
    /// `cf` (the first — and, for a flat movie library, only — ancestor).
    fn plan_dir(
        &self,
        dir: &str,
        cf: Uuid,
        ancestors: &[Uuid],
        collection_type: Option<CollectionTypeOptions>,
        naming: &NamingOptions,
        out: &mut Vec<Planned>,
    ) {
        for entry in self.file_system.get_file_system_entries(dir) {
            if entry.type_ == FileSystemEntryType::Directory {
                // Recurse; movies nested in per-title folders still land directly
                // under the collection folder (a flat movie library view).
                self.plan_dir(&entry.path, cf, ancestors, collection_type, naming, out);
                continue;
            }
            let is_video_library = matches!(
                collection_type,
                None | Some(
                    CollectionTypeOptions::movies
                        | CollectionTypeOptions::homevideos
                        | CollectionTypeOptions::musicvideos
                        | CollectionTypeOptions::mixed
                )
            );
            if !(is_video_library && video_resolver::is_video_file(&entry.path, naming)) {
                continue;
            }
            let Some(id) = item_type_lookup::derive_item_id(BaseItemKind::Movie, &entry.path)
            else {
                continue;
            };
            let (name, year) = video_resolver::resolve_file(Some(&entry.path), naming, None)
                .map_or_else(|| (entry.name.clone(), None), |info| (info.name, info.year));
            let entity = BaseItemEntity {
                id: id.to_string(),
                type_: item_type_lookup::stored_type_name(BaseItemKind::Movie)
                    .unwrap_or_default()
                    .to_owned(),
                name: Some(name),
                path: Some(entry.path.clone()),
                parent_id: Some(cf.to_string()),
                top_parent_id: Some(cf.to_string()),
                is_folder: false,
                is_movie: true,
                media_type: Some("Video".to_owned()),
                production_year: year.map(i64::from),
                date_created: Some(Utc::now()),
                ..BaseItemEntity::default()
            };
            out.push(Planned {
                id,
                entity,
                ancestors: ancestors.to_vec(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LibraryScanner;
    use crate::file_system::HermitFileSystem;
    use crate::item_persistence_service::HermitItemPersistenceService;
    use crate::virtual_folder_manager::HermitVirtualFolderManager;
    use hermit_db::Database;
    use hermit_model::configuration::{LibraryOptions, MediaPathInfo};
    use hermit_model::entities::CollectionTypeOptions;
    use hermit_traits::library::VirtualFolderManager;
    use std::sync::Arc;

    #[tokio::test]
    async fn scan_creates_movie_rows_with_parent_and_ancestors() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::write(media.join("The Matrix (1999).mkv"), b"").unwrap();
        // A nested per-title folder — still flattens under the library.
        std::fs::create_dir_all(media.join("Dune (2021)")).unwrap();
        std::fs::write(media.join("Dune (2021)/Dune (2021).mkv"), b"").unwrap();
        std::fs::write(media.join("poster.jpg"), b"").unwrap(); // non-video: ignored

        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let persistence = Arc::new(HermitItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            HermitVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "Movies",
            Some(CollectionTypeOptions::movies),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: media.to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .unwrap();

        let scanner =
            LibraryScanner::new(vf.clone(), Arc::new(HermitFileSystem::new()), persistence);
        assert_eq!(
            scanner.scan_all().await.unwrap(),
            2,
            "two movies (flat + nested), poster ignored"
        );

        let cf = vf.get_virtual_folders().await.unwrap()[0]
            .item_id
            .clone()
            .unwrap();
        // Both movies parent to the collection folder and carry an ancestor row.
        let movie_rows: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "BaseItems"
               WHERE "Type" = 'MediaBrowser.Controller.Entities.Movies.Movie'
                 AND "ParentId" = ?1"#,
        )
        .bind(&cf)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(movie_rows, 2);
        let ancestor_rows: i64 =
            sqlx::query_scalar(r#"SELECT COUNT(*) FROM "AncestorIds" WHERE "ParentItemId" = ?1"#)
                .bind(&cf)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(ancestor_rows, 2);

        // Deterministic ids → re-scan upserts, does not duplicate.
        assert_eq!(scanner.scan_all().await.unwrap(), 2);
        let total: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Type" LIKE '%Movies.Movie'"#,
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(total, 2, "re-scan did not duplicate");
    }
}
