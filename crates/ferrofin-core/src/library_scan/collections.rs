//! `CollectionPostScanTask` (Jellyfin 4910aafa1a): add eligible movies to named
//! collections after library validation. Membership is additive; turning the
//! option off does not undo previous additions or delete collections.

use std::collections::{BTreeMap, BTreeSet};

use ferrofin_model::data::{BaseItemKind, MediaType};
use ferrofin_model::dto::SortOrder;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_traits::collections::{CollectionCreationOptions, CollectionManager};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::InternalItemsQuery;
use uuid::Uuid;

use super::{LibraryScanner, ScanRun};

const PAGE_SIZE: i32 = 1000;

impl LibraryScanner {
    pub(super) async fn refresh_automatic_collections(
        &self,
        run: ScanRun<'_>,
    ) -> Result<(), ServiceError> {
        let (Some(items), Some(collections)) = (
            &self.item_repository,
            self.collections.get().and_then(std::sync::Weak::upgrade),
        ) else {
            return Ok(());
        };
        // Post-scan tasks consider every configured library, including when
        // the validation itself was scoped to one library.
        let Some(folders) = run
            .cancel
            .unless_cancelled(self.virtual_folders.get_virtual_folders())
            .await
        else {
            return Ok(());
        };
        let mut groups: BTreeMap<String, BTreeSet<Uuid>> = BTreeMap::new();
        for folder in folders? {
            if !folder
                .library_options
                .as_ref()
                .is_some_and(|o| o.automatically_add_to_collection)
            {
                continue;
            }
            let Some(parent_id) = folder.item_id.as_deref().and_then(|s| s.parse().ok()) else {
                continue;
            };
            let mut start = 0;
            loop {
                Box::pin(self.serve_lane(run)).await;
                let query = InternalItemsQuery {
                    parent_id,
                    recursive: true,
                    include_item_types: vec![BaseItemKind::Movie],
                    media_types: vec![MediaType::Video],
                    is_virtual_item: Some(false),
                    order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
                    start_index: Some(start),
                    limit: Some(PAGE_SIZE),
                    ..Default::default()
                };
                let Some(movies) = run
                    .cancel
                    .unless_cancelled(items.get_item_list(&query))
                    .await
                else {
                    return Ok(());
                };
                let movies = movies?;
                for movie in &movies {
                    if movie.primary_version_id.is_some() {
                        continue;
                    }
                    let name = crate::item_data::read_data_string(
                        &crate::item_data::parse_data(movie.data.as_deref()),
                        "CollectionName",
                    );
                    if let Some(name) = name.filter(|name| !name.is_empty())
                        && let Ok(id) = movie.id.parse()
                    {
                        groups.entry(name).or_default().insert(id);
                    }
                }
                if movies.len() < PAGE_SIZE as usize {
                    break;
                }
                start += PAGE_SIZE;
            }
        }
        if groups.is_empty() {
            return Ok(());
        }
        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::BoxSet],
            collapse_box_set_items: Some(false),
            recursive: true,
            ..Default::default()
        };
        let Some(boxes) = run
            .cancel
            .unless_cancelled(items.get_item_list(&query))
            .await
        else {
            return Ok(());
        };
        let boxes = boxes?;
        for (name, ids) in groups {
            Box::pin(self.serve_lane(run)).await;
            if run.cancel.is_cancelled() {
                return Ok(());
            }
            let update = add_group(collections.as_ref(), &boxes, &name, ids);
            if let Err(err) = update.await {
                tracing::warn!(%err, collection = %name, "automatic collection update failed");
            }
        }
        Ok(())
    }
}

/// Create a missing collection only for multiple movies; existing collections
/// accept one. Exact names and additive membership match CollectionPostScanTask.
async fn add_group(
    collections: &dyn CollectionManager,
    boxes: &[ferrofin_db::entities::base_items::BaseItemEntity],
    name: &str,
    ids: BTreeSet<Uuid>,
) -> Result<(), ServiceError> {
    let row = if let Some(row) = boxes.iter().find(|b| b.name.as_deref() == Some(name)) {
        row.clone()
    } else if ids.len() >= 2 {
        collections
            .create_collection(&CollectionCreationOptions {
                name: name.to_owned(),
                ..Default::default()
            })
            .await?
    } else {
        return Ok(());
    };
    let id = Uuid::parse_str(&row.id).map_err(|err| ServiceError::Backend(err.to_string()))?;
    collections
        .add_to_collection(id, &ids.into_iter().collect::<Vec<_>>())
        .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferrofin_db::entities::base_items::BaseItemEntity;
    use ferrofin_db::store::guid_to_db;
    use ferrofin_model::configuration::LibraryOptions;
    use ferrofin_model::entities::CollectionTypeOptions;
    use ferrofin_traits::library::VirtualFolderManager;
    use ferrofin_traits::providers::MetadataRefreshOptions;

    use super::*;
    use crate::test_support::{item_repository_over, library_manager_over, test_db};

    struct Fixture {
        root: tempfile::TempDir,
        scanner: LibraryScanner,
        collections: Arc<dyn CollectionManager>,
        enabled: Uuid,
        disabled: Uuid,
    }

    impl Fixture {
        async fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let db = test_db().await;
            let persistence = Arc::new(crate::FerrofinItemPersistenceService::new(db.clone()));
            let vf: Arc<dyn VirtualFolderManager> = Arc::new(
                crate::FerrofinVirtualFolderManager::new(root.path().join("views"))
                    .with_item_store(persistence.clone()),
            );
            for (name, enabled) in [("On", true), ("Off", false)] {
                vf.add_virtual_folder(
                    name,
                    Some(CollectionTypeOptions::movies),
                    &LibraryOptions {
                        automatically_add_to_collection: enabled,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            }
            let folders = vf.get_virtual_folders().await.unwrap();
            let id = |name| {
                folders
                    .iter()
                    .find(|f| f.name.as_deref() == Some(name))
                    .unwrap()
                    .item_id
                    .as_ref()
                    .unwrap()
                    .parse()
                    .unwrap()
            };
            let enabled = id("On");
            let disabled = id("Off");
            let path = root.path().to_string_lossy().into_owned();
            let paths = Arc::new(crate::app_paths::FerrofinServerApplicationPaths::new(
                path.clone(),
                format!("{path}/log"),
                format!("{path}/config"),
                format!("{path}/cache"),
                format!("{path}/web"),
            ));
            let collections: Arc<dyn CollectionManager> =
                Arc::new(crate::collection_manager::FerrofinCollectionManager::new(
                    db.clone(),
                    library_manager_over(db.clone()),
                    Arc::new(
                        crate::linked_children_service::FerrofinLinkedChildrenService::new(
                            db.clone(),
                        ),
                    ),
                    paths,
                ));
            let scanner =
                LibraryScanner::new(vf, Arc::new(crate::FerrofinFileSystem::new()), persistence)
                    .with_items(item_repository_over(db));
            scanner.attach_collections(&collections);
            Self {
                root,
                scanner,
                collections,
                enabled,
                disabled,
            }
        }

        fn movie(group: &str, library: Uuid) -> BaseItemEntity {
            BaseItemEntity {
                id: guid_to_db(Uuid::new_v4()),
                type_: crate::item_type_lookup::stored_type_name(BaseItemKind::Movie)
                    .unwrap()
                    .to_owned(),
                media_type: Some("Video".to_owned()),
                name: Some("Movie".to_owned()),
                parent_id: Some(guid_to_db(library)),
                top_parent_id: Some(guid_to_db(library)),
                data: Some(serde_json::json!({"CollectionName":group}).to_string()),
                ..Default::default()
            }
        }

        async fn run(&self) {
            self.scanner
                .refresh_automatic_collections(ScanRun::uncancelled(
                    &MetadataRefreshOptions::default(),
                ))
                .await
                .unwrap();
        }

        async fn snapshot(&self) -> BTreeMap<String, BTreeSet<String>> {
            let items = self.scanner.item_repository.as_ref().unwrap();
            let boxes = items
                .get_item_list(&InternalItemsQuery {
                    include_item_types: vec![BaseItemKind::BoxSet],
                    recursive: true,
                    ..Default::default()
                })
                .await
                .unwrap();
            let mut result = BTreeMap::new();
            for row in boxes {
                let children = items
                    .get_item_list(&InternalItemsQuery {
                        parent_id: row.id.parse().unwrap(),
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                result.insert(
                    row.name.unwrap(),
                    children.into_iter().map(|row| row.id).collect(),
                );
            }
            result
        }
    }

    #[tokio::test]
    async fn automatic_collections_use_names_read_from_nfo_during_scan() {
        let f = Fixture::new().await;
        let media = f.root.path().join("movies");
        for name in ["First", "Second"] {
            let folder = media.join(name);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join(format!("{name}.mkv")), b"fixture").unwrap();
            std::fs::write(
                folder.join("movie.nfo"),
                format!("<movie><title>{name}</title><set><name>Saga</name></set></movie>"),
            )
            .unwrap();
        }
        f.scanner
            .virtual_folders
            .update_library_options(
                "On",
                &LibraryOptions {
                    automatically_add_to_collection: true,
                    path_infos: vec![ferrofin_model::configuration::MediaPathInfo {
                        path: media.to_string_lossy().into_owned(),
                    }],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        f.scanner.scan_all().await.unwrap();
        let first = f.snapshot().await;
        assert_eq!(first.len(), 1);
        assert_eq!(first["Saga"].len(), 2);
        f.scanner.scan_all().await.unwrap();
        assert_eq!(f.snapshot().await, first);
    }

    #[tokio::test]
    async fn automatic_collections_filter_scope_versions_and_keep_membership_when_disabled() {
        let f = Fixture::new().await;
        let a = Fixture::movie("Saga", f.enabled);
        let b = Fixture::movie("Saga", f.enabled);
        let mut version = Fixture::movie("Saga", f.enabled);
        version.primary_version_id = Some(a.id.clone());
        let mut virtual_movie = Fixture::movie("Saga", f.enabled);
        virtual_movie.is_virtual_item = true;
        f.scanner
            .persistence
            .save_items(&[
                a.clone(),
                b.clone(),
                version,
                virtual_movie,
                Fixture::movie("Saga", f.disabled),
                Fixture::movie("saga", f.enabled),
                Fixture::movie("", f.enabled),
            ])
            .await
            .unwrap();
        f.run().await;
        let expected = BTreeMap::from([("Saga".to_owned(), BTreeSet::from([a.id, b.id]))]);
        assert_eq!(f.snapshot().await, expected);
        f.run().await;
        assert_eq!(
            f.snapshot().await,
            expected,
            "repeated additions remain unique"
        );
        f.scanner
            .virtual_folders
            .update_library_options(
                "On",
                &LibraryOptions {
                    automatically_add_to_collection: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        f.scanner
            .persistence
            .save_items(&[Fixture::movie("Saga", f.enabled)])
            .await
            .unwrap();
        f.run().await;
        assert_eq!(
            f.snapshot().await,
            expected,
            "disabled means no new additions or removals"
        );
    }

    #[tokio::test]
    async fn automatic_collections_add_one_movie_to_an_existing_exact_name() {
        let f = Fixture::new().await;
        let one = Fixture::movie("Single", f.enabled);
        f.scanner
            .persistence
            .save_items(std::slice::from_ref(&one))
            .await
            .unwrap();
        f.run().await;
        assert!(f.snapshot().await.is_empty());
        for name in ["Single", "single"] {
            f.collections
                .create_collection(&CollectionCreationOptions {
                    name: name.to_owned(),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        f.run().await;
        assert_eq!(
            f.snapshot().await,
            BTreeMap::from([
                ("Single".to_owned(), BTreeSet::from([one.id])),
                ("single".to_owned(), BTreeSet::new()),
            ])
        );
    }

    #[tokio::test]
    async fn automatic_collections_page_past_one_thousand_and_obey_cancellation() {
        let f = Fixture::new().await;
        let movies: Vec<_> = (0..1001)
            .map(|index| {
                let mut row = Fixture::movie("Large", f.enabled);
                row.name = Some(format!("Movie {index:04}"));
                row
            })
            .collect();
        f.scanner.persistence.save_items(&movies).await.unwrap();
        let cancel = super::super::ScanCancel::new();
        cancel.cancel();
        let options = MetadataRefreshOptions::default();
        f.scanner
            .refresh_automatic_collections(ScanRun::new(&options, &options, &cancel))
            .await
            .unwrap();
        assert!(f.snapshot().await.is_empty());
        f.run().await;
        assert_eq!(
            f.snapshot().await["Large"],
            movies.into_iter().map(|row| row.id).collect()
        );
    }
}
