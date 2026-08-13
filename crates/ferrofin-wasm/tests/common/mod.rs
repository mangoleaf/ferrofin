//! Shared test support for the ferrofin-wasm integration tests: stub
//! managers (an always-enabled plugin manager, a one-movie library, a
//! recording segment store) and a loopback one-shot HTTP server.
#![allow(dead_code)] // each test binary uses a subset

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::Mutex;

use uuid::Uuid;

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::LibraryManager;
use ferrofin_traits::media_segments::MediaSegmentManager;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::plugins::{PluginDescriptor, PluginImage, PluginManager};

/// A one-shot loopback HTTP server: returns (url, join-handle yielding the
/// raw request bytes). Responds with `status` and `body`.
pub fn one_shot_http(
    status: &'static str,
    body: &'static [u8],
) -> (String, std::thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = vec![0_u8; 65536];
        let n = stream.read(&mut request).expect("read request");
        request.truncate(n);
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-length: {}\r\nx-demo: yes\r\nconnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        request
    });
    (format!("http://{addr}/hook"), handle)
}

/// A plugin manager stub: every plugin enabled, canned config JSON.
pub struct EnabledStub(pub Vec<u8>);

#[async_trait::async_trait]
impl PluginManager for EnabledStub {
    async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_plugin(&self, id: Uuid) -> Result<Option<PluginDescriptor>, ServiceError> {
        Ok(Some(PluginDescriptor {
            id,
            enabled: true,
            ..PluginDescriptor::default()
        }))
    }
    async fn enable_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn disable_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn remove_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_plugin_configuration(&self, _id: Uuid) -> Result<Vec<u8>, ServiceError> {
        Ok(self.0.clone())
    }
    async fn set_plugin_configuration(
        &self,
        _id: Uuid,
        _config: Vec<u8>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn plugin_image(&self, _id: Uuid) -> Result<Option<PluginImage>, ServiceError> {
        Ok(None)
    }
    async fn get_repositories(
        &self,
    ) -> Result<Vec<ferrofin_model::updates::RepositoryInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn set_repositories(
        &self,
        _repositories: Vec<ferrofin_model::updates::RepositoryInfo>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn list_packages(
        &self,
    ) -> Result<Vec<ferrofin_model::updates::PackageInfo>, ServiceError> {
        Ok(Vec::new())
    }
}

/// Library stub: records the query, returns one canned movie row. Only
/// `get_item_list` is reachable from `query-items`; everything else panics.
pub struct OneMovieLibrary {
    pub seen: Mutex<Option<InternalItemsQuery>>,
}

#[async_trait::async_trait]
#[allow(clippy::unimplemented)]
impl LibraryManager for OneMovieLibrary {
    async fn get_item_by_id(&self, _id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        unimplemented!("stub")
    }
    async fn query_items(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<ferrofin_model::querying::QueryResult<BaseItemEntity>, ServiceError> {
        unimplemented!("stub")
    }
    async fn get_item_ids(&self, _query: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!("stub")
    }
    async fn get_item_list(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        *self.seen.lock().unwrap() = Some(query.clone());
        let entity = BaseItemEntity {
            id: "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEFF01".to_owned(),
            name: Some("Big Buck Bunny".to_owned()),
            type_: ferrofin_core::item_type_lookup::stored_type_name(
                ferrofin_model::data::BaseItemKind::Movie,
            )
            .unwrap()
            .to_owned(),
            path: Some("/media/movies/bbb.mkv".to_owned()),
            run_time_ticks: Some(5_000_000_000),
            ..BaseItemEntity::default()
        };
        Ok(vec![entity])
    }
    async fn get_latest_item_list(
        &self,
        _query: &InternalItemsQuery,
        _collection_type: ferrofin_model::data::CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("stub")
    }
    async fn create_items(
        &self,
        _items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
    async fn update_items(
        &self,
        _items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
    async fn delete_item(
        &self,
        _id: Uuid,
        _options: &ferrofin_traits::options::DeleteOptions,
    ) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
    async fn get_people(
        &self,
        _query: &ferrofin_traits::options::InternalPeopleQuery,
    ) -> Result<Vec<ferrofin_db::entities::base_items::PeopleEntity>, ServiceError> {
        unimplemented!("stub")
    }
    async fn get_people_names(
        &self,
        _query: &ferrofin_traits::options::InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!("stub")
    }
    async fn get_count(&self, _query: &InternalItemsQuery) -> Result<i32, ServiceError> {
        unimplemented!("stub")
    }
    async fn get_item_counts(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<ferrofin_model::dto::ItemCounts, ServiceError> {
        unimplemented!("stub")
    }
    async fn get_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!("stub")
    }
    async fn get_studios(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!("stub")
    }
    async fn get_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!("stub")
    }
    async fn get_music_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!("stub")
    }
    async fn get_album_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_traits::persistence::ItemWithCounts>,
        ServiceError,
    > {
        unimplemented!("stub")
    }
    async fn get_query_filters_legacy(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<ferrofin_model::querying::QueryFiltersLegacy, ServiceError> {
        unimplemented!("stub")
    }
    async fn get_media_stream_languages(
        &self,
        _stream_type: ferrofin_model::entities::MediaStreamType,
        _query: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!("stub")
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
}

/// Segment stub: records deletes and creates.
#[derive(Default)]
pub struct RecordingSegments {
    pub deleted: Mutex<Vec<(Uuid, String)>>,
    pub created: Mutex<Vec<(MediaSegmentDto, String)>>,
}

#[async_trait::async_trait]
impl MediaSegmentManager for RecordingSegments {
    async fn is_type_supported(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
        Ok(true)
    }
    async fn create_segment(
        &self,
        segment: &MediaSegmentDto,
        segment_provider_id: &str,
    ) -> Result<MediaSegmentDto, ServiceError> {
        self.created
            .lock()
            .unwrap()
            .push((segment.clone(), segment_provider_id.to_owned()));
        Ok(segment.clone())
    }
    async fn delete_segment(&self, _segment_id: Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn delete_segments(&self, _item_id: Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn delete_provider_segments(
        &self,
        item_id: Uuid,
        provider_id: &str,
        _type_filter: Option<MediaSegmentType>,
    ) -> Result<(), ServiceError> {
        self.deleted
            .lock()
            .unwrap()
            .push((item_id, provider_id.to_owned()));
        Ok(())
    }
    async fn get_segments(
        &self,
        _item_id: Uuid,
        _type_filter: Option<&[MediaSegmentType]>,
        _filter_by_provider: bool,
    ) -> Result<Vec<MediaSegmentDto>, ServiceError> {
        Ok(Vec::new())
    }
    async fn has_segments(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
        Ok(false)
    }
    async fn get_supported_providers(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_traits::media_segments::MediaSegmentProviderInfo>, ServiceError> {
        Ok(Vec::new())
    }
}
