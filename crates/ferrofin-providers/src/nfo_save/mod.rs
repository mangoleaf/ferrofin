//! Automatic NFO sidecar saving after a metadata or artwork update.
//! Selection follows Jellyfin's ProviderManager and the per-kind NFO savers.

mod mapping;
mod paths;
mod policy;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::configuration::XbmcMetadataOptions;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{UserDataManager, UserManager, VirtualFolderManager};
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{
    ItemPersistenceService, ItemRepository, MediaStreamRepository, PeopleRepository,
};
use ferrofin_traits::providers::ItemUpdateType;
use uuid::Uuid;

use crate::container_types::{MetadataResult, PersonInfo};
use crate::xbmc::{
    config::NfoConfiguration,
    item::{NfoBaseItem, NfoItemKind},
    saver,
};

type SaveLocks = Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>;

/// The registered Nfo saver, using repository interfaces rather than SQL.
/// A per-path lock keeps concurrent edits from interleaving file replacement.
pub struct NfoSaver {
    items: Arc<dyn ItemRepository>,
    persistence: Arc<dyn ItemPersistenceService>,
    people: Arc<dyn PeopleRepository>,
    streams: Arc<dyn MediaStreamRepository>,
    folders: Arc<dyn VirtualFolderManager>,
    configuration: Arc<dyn ServerConfigurationManager>,
    users: Option<(Arc<dyn UserManager>, Arc<dyn UserDataManager>)>,
    locks: SaveLocks,
}

impl NfoSaver {
    /// Creates the saver with the repositories used to serialize a complete item.
    #[must_use]
    pub fn new(
        items: Arc<dyn ItemRepository>,
        persistence: Arc<dyn ItemPersistenceService>,
        people: Arc<dyn PeopleRepository>,
        streams: Arc<dyn MediaStreamRepository>,
        folders: Arc<dyn VirtualFolderManager>,
        configuration: Arc<dyn ServerConfigurationManager>,
    ) -> Self {
        Self {
            items,
            persistence,
            people,
            streams,
            folders,
            configuration,
            users: None,
            locks: Mutex::default(),
        }
    }

    /// Attaches the selected user's playback/favorite data for NFO export.
    #[must_use]
    pub fn with_users(
        mut self,
        users: Arc<dyn UserManager>,
        data: Arc<dyn UserDataManager>,
    ) -> Self {
        self.users = Some((users, data));
        self
    }

    /// Writes the selected saver after reading the item's current persisted data.
    ///
    /// # Errors
    /// Returns a repository, configuration or filesystem failure. The provider
    /// manager logs this without undoing the successful metadata edit.
    pub async fn save(&self, item_id: Uuid, update: ItemUpdateType) -> Result<(), ServiceError> {
        let Some(row) = self.items.retrieve_item(item_id).await? else {
            return Ok(());
        };
        let Some(path) = policy::save_path(&row) else {
            return Ok(());
        };
        let lock = {
            let mut locks = self
                .locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            locks.retain(|_, lock| lock.strong_count() > 0);
            let lock = locks
                .get(&path)
                .and_then(Weak::upgrade)
                .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
            locks.insert(path.clone(), Arc::downgrade(&lock));
            lock
        };
        let _guard = lock.lock().await;
        // Another editor may have saved while this call waited for the path.
        let Some(row) = self.items.retrieve_item(item_id).await? else {
            return Ok(());
        };
        if policy::save_path(&row).as_ref() != Some(&path) {
            return Err(ServiceError::backend(
                "item path changed while waiting to save NFO",
            ));
        }
        let Some(kind) = policy::kind(&row) else {
            return Ok(());
        };
        let folders = self.folders.get_virtual_folders().await?;
        let library = ferrofin_model::entities_media::owning_library(
            &folders,
            row.top_parent_id.as_deref(),
            row.path.as_deref(),
        )
        .and_then(|folder| folder.library_options.as_ref());
        let server = self.configuration.configuration().await?;
        let global = crate::library_options::global_metadata_options(
            &server.metadata_options,
            row.type_.rsplit('.').next().unwrap_or_default(),
        );
        let options = self.options().await?;
        if !policy::enabled(
            library,
            global,
            kind,
            update,
            options.save_image_paths_in_nfo,
            &path,
        ) {
            return Ok(());
        }
        self.write_document(item_id, &row, &options, &server.path_substitutions, path)
            .await
    }

    async fn write_document(
        &self,
        item_id: Uuid,
        row: &BaseItemEntity,
        options: &XbmcMetadataOptions,
        substitutions: &[ferrofin_model::configuration::PathSubstitution],
        path: PathBuf,
    ) -> Result<(), ServiceError> {
        let mut result = self
            .metadata(item_id, row, options.save_image_paths_in_nfo)
            .await?;
        if let Some(people) = &mut result.people {
            for person in people {
                if let Some(path) = &mut person.image_url {
                    *path = paths::substitute(path, substitutions);
                }
            }
        }
        let config = NfoConfiguration {
            user_id: options.user_id.clone(),
            release_date_format: options.release_date_format.clone(),
        };
        let xml = self.serialize(item_id, &result, &config).await?;
        let mut images = if options.save_image_paths_in_nfo {
            self.items.get_image_infos(item_id).await?
        } else {
            Vec::new()
        };
        for image in &mut images {
            if image.is_local_file() {
                image.path = paths::substitute(&image.path, substitutions);
            }
        }
        let streams = if policy::kind(row).is_some_and(NfoItemKind::is_video) {
            self.streams
                .get_media_streams(&ferrofin_traits::persistence::MediaStreamQuery {
                    item_id,
                    ..Default::default()
                })
                .await?
        } else {
            Vec::new()
        };
        let user = self.user_data(item_id, row, options).await?;
        let old = match tokio::fs::read_to_string(&path).await {
            Ok(old) => Some(old),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(ServiceError::backend(format!(
                    "read NFO {}: {error}",
                    path.display()
                )));
            }
        };
        let xml = saver::complete_document(
            xml,
            saver::DocumentExtras {
                item: &result.item,
                original_language: row.original_language.as_deref(),
                images: &images,
                streams: &streams,
                user: user.as_ref(),
                save_images: options.save_image_paths_in_nfo,
                existing: old.as_deref(),
            },
        )?;
        if old.as_deref() == Some(&xml) {
            return Ok(());
        }
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            ferrofin_util::file_helper::atomic_write(&path, xml.as_bytes())
        })
        .await
        .map_err(|error| ServiceError::backend(error.to_string()))?
        .map_err(|error| ServiceError::backend(format!("write NFO: {error}")))
    }

    async fn options(&self) -> Result<XbmcMetadataOptions, ServiceError> {
        let path = PathBuf::from(
            self.configuration
                .application_paths()
                .user_configuration_directory_path(),
        )
        .join("named/xbmcmetadata.json");
        match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| ServiceError::backend(format!("read NFO options: {error}"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(XbmcMetadataOptions::default())
            }
            Err(error) => Err(ServiceError::backend(format!("read NFO options: {error}"))),
        }
    }

    async fn metadata(
        &self,
        id: Uuid,
        row: &BaseItemEntity,
        save_images: bool,
    ) -> Result<MetadataResult<NfoBaseItem>, ServiceError> {
        let mut item =
            mapping::item(row).ok_or_else(|| ServiceError::backend("unsupported NFO kind"))?;
        for (key, value) in self
            .persistence
            .provider_ids_for_items(&[id])
            .await?
            .remove(&id)
            .unwrap_or_default()
        {
            item.set_provider_id(&key, &value);
        }
        item.locked_fields = self
            .persistence
            .locked_fields_for_items(&[id])
            .await?
            .remove(&id)
            .unwrap_or_default();
        let mut people = Vec::new();
        for person in self
            .people
            .get_people_batch(&[id])
            .await?
            .remove(&id)
            .unwrap_or_default()
        {
            let person_id = Uuid::parse_str(&person.id).unwrap_or_default();
            let images = if save_images {
                self.items.get_image_infos(person_id).await?
            } else {
                Vec::new()
            };
            people.push(PersonInfo {
                id: person_id,
                item_id: id,
                name: person.name,
                role: person.role,
                type_: person
                    .person_type
                    .and_then(|kind| serde_json::from_value(serde_json::Value::String(kind)).ok())
                    .unwrap_or(ferrofin_model::data::PersonKind::Unknown),
                sort_order: person
                    .sort_order
                    .and_then(|order| i32::try_from(order).ok()),
                image_url: images
                    .into_iter()
                    .find(|image| image.image_type == ferrofin_model::entities::ImageType::Primary)
                    .map(|image| image.path),
                ..Default::default()
            });
        }
        Ok(MetadataResult {
            item,
            people: Some(people),
            ..Default::default()
        })
    }

    async fn serialize(
        &self,
        id: Uuid,
        result: &MetadataResult<NfoBaseItem>,
        config: &NfoConfiguration,
    ) -> Result<String, ServiceError> {
        use ferrofin_model::data::BaseItemKind;
        Ok(match result.item.kind {
            NfoItemKind::Episode => saver::save_episode(result, config),
            NfoItemKind::Series => saver::save_series(result, config),
            NfoItemKind::Season => saver::save_season(result, config),
            NfoItemKind::MusicAlbum | NfoItemKind::MusicArtist => {
                let album = result.item.kind == NfoItemKind::MusicAlbum;
                let children = self
                    .items
                    .get_item_list(&InternalItemsQuery {
                        parent_id: id,
                        recursive: true,
                        include_item_types: vec![if album {
                            BaseItemKind::Audio
                        } else {
                            BaseItemKind::MusicAlbum
                        }],
                        ..Default::default()
                    })
                    .await?;
                if album {
                    let tracks: Vec<_> = children
                        .into_iter()
                        .map(|track| saver::NfoTrack {
                            disc: track
                                .parent_index_number
                                .and_then(|n| i32::try_from(n).ok()),
                            position: track.index_number.and_then(|n| i32::try_from(n).ok()),
                            title: track.name,
                            run_time_ticks: track.run_time_ticks,
                            sort_name: track.forced_sort_name.or(track.sort_name),
                        })
                        .collect();
                    saver::save_album(result, &tracks, config)
                } else {
                    let albums: Vec<_> = children
                        .into_iter()
                        .map(|album| saver::NfoAlbum {
                            title: album.name,
                            year: album.production_year.and_then(|n| i32::try_from(n).ok()),
                            sort_name: album.forced_sort_name.or(album.sort_name),
                        })
                        .collect();
                    saver::save_artist(result, &albums, config)
                }
            }
            _ => saver::save_movie(result, config),
        })
    }

    async fn user_data(
        &self,
        id: Uuid,
        row: &BaseItemEntity,
        options: &XbmcMetadataOptions,
    ) -> Result<Option<ferrofin_model::dto::UserItemDataDto>, ServiceError> {
        if row.is_folder {
            return Ok(None);
        }
        let Some(user) = options
            .user_id
            .as_deref()
            .and_then(|user| Uuid::parse_str(user).ok())
        else {
            return Ok(None);
        };
        let Some((users, data)) = &self.users else {
            return Ok(None);
        };
        if users.get_user_by_id(user).await?.is_none() {
            return Ok(None);
        }
        data.get_user_data_dto(id, user).await
    }
}
