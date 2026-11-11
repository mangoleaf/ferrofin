//! What a playback report reads from the library. A stop looks its item up
//! once: the row decides the save and names the item in the "user stopped
//! playback" line, as C# `OnPlaybackStopped`'s one `libraryItem` does
//! (gl-1315).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use ferrofin_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use ferrofin_model::data::{BaseItemKind, CollectionType};
use ferrofin_model::dto::ItemCounts;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::querying::{QueryFiltersLegacy, QueryResult};
use ferrofin_model::session::PlaybackStopInfo;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, UserDataManager as _};
use ferrofin_traits::options::{DeleteOptions, InternalItemsQuery, InternalPeopleQuery};
use ferrofin_traits::persistence::ItemWithCounts;
use ferrofin_traits::session::SessionManager;
use rstest::rstest;
use uuid::Uuid;

use super::super::FerrofinSessionManager;
use super::{FixedConfig, UnusedDtoService, library_manager, seed_named_user, test_db};
use crate::configuration_manager::default_server_configuration;
use crate::device_manager::FerrofinDeviceManager;
use crate::event_manager::FerrofinEventManager;
use crate::library_manager::FerrofinLibraryManager;
use crate::user_data_manager::FerrofinUserDataManager;
use crate::user_manager::FerrofinUserManager;

/// The real library manager, counting its item lookups. A playback report
/// reaches nothing else of it.
struct CountingLibrary {
    inner: Arc<FerrofinLibraryManager>,
    lookups: AtomicUsize,
}

const UNREACHED: &str = "not reached from a playback report";

#[async_trait]
impl LibraryManager for CountingLibrary {
    async fn get_item_by_id(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        self.inner.get_item_by_id(id).await
    }
    async fn query_items(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        unreachable!("{UNREACHED}")
    }
    async fn get_item_ids(&self, _query: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        unreachable!("{UNREACHED}")
    }
    async fn get_item_list(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unreachable!("{UNREACHED}")
    }
    async fn get_latest_item_list(
        &self,
        _query: &InternalItemsQuery,
        _collection_type: CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unreachable!("{UNREACHED}")
    }
    async fn update_items(
        &self,
        _items: &[BaseItemEntity],
        _parent_id: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        unreachable!("{UNREACHED}")
    }
    async fn delete_item(&self, _id: Uuid, _options: &DeleteOptions) -> Result<(), ServiceError> {
        unreachable!("{UNREACHED}")
    }
    async fn get_people(
        &self,
        _query: &InternalPeopleQuery,
    ) -> Result<Vec<PeopleEntity>, ServiceError> {
        unreachable!("{UNREACHED}")
    }
    async fn get_people_names(
        &self,
        _query: &InternalPeopleQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unreachable!("{UNREACHED}")
    }
    async fn get_count(&self, _query: &InternalItemsQuery) -> Result<i32, ServiceError> {
        unreachable!("{UNREACHED}")
    }
    async fn get_item_counts(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<ItemCounts, ServiceError> {
        unreachable!("{UNREACHED}")
    }
    async fn get_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unreachable!("{UNREACHED}")
    }
    async fn get_studios(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unreachable!("{UNREACHED}")
    }
    async fn get_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unreachable!("{UNREACHED}")
    }
    async fn get_music_genres(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unreachable!("{UNREACHED}")
    }
    async fn get_album_artists(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        unreachable!("{UNREACHED}")
    }
    async fn get_query_filters_legacy(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        unreachable!("{UNREACHED}")
    }
    async fn get_media_stream_languages(
        &self,
        _stream_type: MediaStreamType,
        _query: &InternalItemsQuery,
    ) -> Result<Vec<String>, ServiceError> {
        unreachable!("{UNREACHED}")
    }
    async fn queue_library_scan(&self) -> Result<(), ServiceError> {
        unreachable!("{UNREACHED}")
    }
}

// A stop reads its item once, whether it saves the play state or (a failed
// playback) saves nothing, and its log line names the item either way. The
// item has no run time, so a stop that saves marks it played.
#[rstest]
#[case::saved(false)]
#[case::failed(true)]
#[tokio::test]
async fn a_stop_looks_its_item_up_once(#[case] failed: bool) {
    let db = test_db().await;
    let config: Arc<dyn ServerConfigurationManager> = Arc::new(FixedConfig {
        config: default_server_configuration(),
    });
    let user_data = Arc::new(FerrofinUserDataManager::new(db.clone(), config));
    let library = Arc::new(CountingLibrary {
        inner: library_manager(&db),
        lookups: AtomicUsize::new(0),
    });
    let mgr = FerrofinSessionManager::new(
        Arc::new(FerrofinUserManager::new(db.clone())),
        Arc::new(FerrofinDeviceManager::new(db.clone())),
        Arc::clone(&user_data) as _,
        Arc::clone(&library) as _,
        Arc::new(UnusedDtoService),
        Arc::new(FerrofinEventManager::new()),
        db.clone(),
        "server-1".to_owned(),
    );
    let user_id = Uuid::new_v4();
    let user = seed_named_user(&db, user_id, "watcher").await;
    let session_id = mgr
        .log_session_activity("Web", "1.0", "dev-stop-reads", "TV", "e", Some(&user))
        .await
        .expect("session")
        .id
        .expect("session id");
    let item = Uuid::new_v4();
    crate::test_support::seed_named_item(&db, item, BaseItemKind::Movie, "Film").await;

    let logs = crate::test_support::CapturedLogs::default();
    let guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .finish(),
    );
    mgr.on_playback_stopped(&PlaybackStopInfo {
        session_id: Some(session_id),
        item_id: item,
        position_ticks: Some(30),
        failed,
        ..PlaybackStopInfo::default()
    })
    .await
    .expect("stop");
    drop(guard);

    assert_eq!(
        library.lookups.load(Ordering::SeqCst),
        1,
        "a stop looks its item up once"
    );
    let text = logs.text();
    assert!(
        text.contains("user stopped playback") && text.contains(r#"item="Film""#),
        "the stop's log line names the item: {text}"
    );
    let played = user_data
        .get_user_data_dto(item, user_id)
        .await
        .expect("user data")
        .is_some_and(|data| data.played);
    assert_eq!(played, !failed);
}
