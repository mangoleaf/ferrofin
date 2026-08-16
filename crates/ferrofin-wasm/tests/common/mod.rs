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
        // The canned movie, for the id it advertises; None otherwise (the
        // analysis capabilities resolve items through this).
        if _id.to_string() != "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeff01" {
            return Ok(None);
        }
        Ok(Some(BaseItemEntity {
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
        }))
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

// ── stubs for the user/tv collaborator seams (0.3.0 capabilities) ──
#[allow(unused_imports)]
use ferrofin_db::entities::base_items::BaseItemEntity as _StubBI;
use ferrofin_db::entities::users::UserEntity;
#[allow(unused_imports)]
use ferrofin_model::configuration::UserConfiguration;
#[allow(unused_imports)]
use ferrofin_model::dto::NameIdPair;
#[allow(unused_imports)]
use ferrofin_model::dto::{BaseItemDto, UpdateUserItemDataDto, UserDto, UserItemDataDto};
#[allow(unused_imports)]
use ferrofin_model::querying::QueryResult;
#[allow(unused_imports)]
use ferrofin_model::users::UserPolicy;
#[allow(unused_imports)]
use ferrofin_traits::options::DtoOptions;
#[allow(unused_imports)]
use ferrofin_traits::tv::NextUpQuery;

/// Panic-on-call stub — only the methods a test exercises matter.
pub struct StubUsers;

#[async_trait::async_trait]
impl ferrofin_traits::library::UserManager for StubUsers {
    async fn get_users(&self) -> Result<Vec<UserEntity>, ServiceError> {
        unimplemented!("stub")
    }
    async fn get_user_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
        unimplemented!("stub")
    }
    async fn initialize(&self) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
    async fn get_user_by_id(&self, _id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
        Ok(Some(test_user(_id)))
    }
    async fn get_first_user(&self) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!("stub")
    }
    async fn get_user_by_name(&self, _name: &str) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!("stub")
    }
    async fn rename_user(
        &self,
        _user_id: Uuid,
        _old_name: &str,
        _new_name: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
    async fn update_user(&self, _user: &UserEntity) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
    async fn create_user(&self, _name: &str) -> Result<UserEntity, ServiceError> {
        unimplemented!("stub")
    }
    async fn delete_user(&self, _user_id: Uuid) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
    async fn reset_password(&self, _user_id: Uuid) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
    async fn change_password(
        &self,
        _user_id: Uuid,
        _new_password: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
    async fn authenticate_user(
        &self,
        _username: &str,
        _password: &str,
        _remote_endpoint: &str,
        _is_user_session: bool,
    ) -> Result<Option<UserEntity>, ServiceError> {
        unimplemented!("stub")
    }
    async fn get_authentication_providers(&self) -> Result<Vec<NameIdPair>, ServiceError> {
        unimplemented!("stub")
    }
    async fn get_password_reset_providers(&self) -> Result<Vec<NameIdPair>, ServiceError> {
        unimplemented!("stub")
    }
    async fn get_user_dto(
        &self,
        _user: &UserEntity,
        _server_id: Option<String>,
    ) -> Result<UserDto, ServiceError> {
        unimplemented!("stub")
    }
    async fn update_configuration(
        &self,
        _user_id: Uuid,
        _config: &UserConfiguration,
    ) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
    async fn update_policy(
        &self,
        _user_id: Uuid,
        _policy: &UserPolicy,
    ) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
    async fn clear_profile_image(&self, _user: &UserEntity) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
}

/// Panic-on-call stub — only the methods a test exercises matter.
pub struct StubUserData;

#[async_trait::async_trait]
impl ferrofin_traits::library::UserDataManager for StubUserData {
    async fn save_user_data(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _user_data: &UpdateUserItemDataDto,
    ) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
    async fn get_user_data_dto(
        &self,
        _item_id: Uuid,
        _user_id: Uuid,
    ) -> Result<Option<UserItemDataDto>, ServiceError> {
        unimplemented!("stub")
    }
    async fn get_user_data_batch(
        &self,
        _item_ids: &[Uuid],
        _user_id: Uuid,
    ) -> Result<std::collections::HashMap<Uuid, UserItemDataDto>, ServiceError> {
        Ok(_item_ids
            .iter()
            .map(|id| {
                (
                    *id,
                    UserItemDataDto {
                        rating: None,
                        played_percentage: None,
                        unplayed_item_count: None,
                        playback_position_ticks: 1230,
                        play_count: 2,
                        is_favorite: true,
                        likes: None,
                        last_played_date: None,
                        played: true,
                        key: String::new(),
                        item_id: *id,
                    },
                )
            })
            .collect())
    }
    async fn update_play_state(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _reported_position_ticks: Option<i64>,
    ) -> Result<bool, ServiceError> {
        unimplemented!("stub")
    }
    async fn mark_played(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _date_played: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<UserItemDataDto, ServiceError> {
        unimplemented!("stub")
    }
    async fn mark_unplayed(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
    ) -> Result<UserItemDataDto, ServiceError> {
        unimplemented!("stub")
    }
    async fn reset_playback_stream_selections(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
    ) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
}

/// Panic-on-call stub — only the methods a test exercises matter.
pub struct StubTv;

#[async_trait::async_trait]
impl ferrofin_traits::tv::TvSeriesManager for StubTv {
    async fn get_next_up(
        &self,
        _query: &NextUpQuery,
        _options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        Ok(QueryResult {
            items: vec![BaseItemDto {
                id: Uuid::parse_str("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEFF01").unwrap(),
                ..BaseItemDto::default()
            }],
            total_record_count: 1,
            start_index: 0,
        })
    }
}

/// A minimal-but-complete user entity for user-scoped query tests.
#[must_use]
pub fn test_user(id: Uuid) -> UserEntity {
    UserEntity {
        id: id.to_string(),
        audio_language_preference: None,
        authentication_provider_id: String::new(),
        cast_receiver_id: None,
        display_collections_view: false,
        display_missing_episodes: false,
        enable_auto_login: false,
        enable_local_password: false,
        enable_next_episode_auto_play: false,
        enable_user_preference_access: false,
        hide_played_in_latest: false,
        internal_id: 0,
        invalid_login_attempt_count: 0,
        last_activity_date: None,
        last_login_date: None,
        login_attempts_before_lockout: None,
        max_active_sessions: 0,
        max_parental_rating_score: None,
        max_parental_rating_sub_score: None,
        must_update_password: false,
        password: Some("hashed".to_owned()),
        password_reset_provider_id: String::new(),
        play_default_audio_track: false,
        remember_audio_selections: false,
        remember_subtitle_selections: false,
        remote_client_bitrate_limit: None,
        row_version: 0,
        subtitle_language_preference: None,
        subtitle_mode: 0,
        sync_play_access: 0,
        username: "plugin-test-user".to_owned(),
    }
}

/// A recording extractor: returns a deterministic tone/frame so capability
/// tests can assert caps + plumbing without ffmpeg.
#[derive(Default)]
pub struct StubExtractor {
    /// The (path, start, duration) of the last audio request.
    pub last_audio: Mutex<Option<(String, f64, f64)>>,
}

#[async_trait::async_trait]
impl ferrofin_traits::media_analysis::MediaExtractor for StubExtractor {
    async fn extract_audio(
        &self,
        path: &str,
        start_seconds: f64,
        duration_seconds: f64,
        spec: ferrofin_traits::media_analysis::AudioSpec,
    ) -> Result<Vec<i16>, ServiceError> {
        *self.last_audio.lock().unwrap() = Some((path.to_owned(), start_seconds, duration_seconds));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // bounded test durations
        let samples =
            (duration_seconds * f64::from(spec.sample_rate)) as usize * usize::from(spec.channels);
        Ok(vec![7i16; samples.min(1024)])
    }

    async fn extract_subtitle(
        &self,
        _path: &str,
        stream_index: u32,
    ) -> Result<Vec<u8>, ServiceError> {
        Ok(format!("1\n00:00:00,000 --> 00:00:01,000\nstream {stream_index}\n").into_bytes())
    }

    async fn extract_frames(
        &self,
        _path: &str,
        timestamps_seconds: &[f64],
        max_dimension: u32,
        jpeg: bool,
    ) -> Result<Vec<ferrofin_traits::media_analysis::ExtractedFrame>, ServiceError> {
        Ok(timestamps_seconds
            .iter()
            .map(|&t| ferrofin_traits::media_analysis::ExtractedFrame {
                seconds: t,
                width: max_dimension,
                height: max_dimension,
                jpeg,
                data: vec![0u8; 16],
            })
            .collect())
    }
}

/// Stream stub: one audio + one video row for any item.
pub struct StubStreams;

#[async_trait::async_trait]
#[allow(clippy::unimplemented)]
impl ferrofin_traits::persistence::MediaStreamRepository for StubStreams {
    async fn get_media_streams(
        &self,
        filter: &ferrofin_traits::persistence::MediaStreamQuery,
    ) -> Result<Vec<ferrofin_db::entities::base_items::MediaStreamInfoEntity>, ServiceError> {
        let mut audio = ferrofin_db::entities::base_items::MediaStreamInfoEntity {
            item_id: filter.item_id.to_string(),
            ..Default::default()
        };
        audio.stream_type = 0;
        let mut video = audio.clone();
        video.stream_type = 1;
        Ok(vec![audio, video])
    }

    async fn get_media_stream_languages(
        &self,
        _stream_type: ferrofin_model::entities::MediaStreamType,
    ) -> Result<Vec<String>, ServiceError> {
        unimplemented!("stub")
    }

    async fn save_media_streams(
        &self,
        _item_id: Uuid,
        _streams: &[ferrofin_db::entities::base_items::MediaStreamInfoEntity],
    ) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }

    async fn get_media_streams_batch(
        &self,
        _item_ids: &[Uuid],
    ) -> Result<
        std::collections::HashMap<
            Uuid,
            Vec<ferrofin_db::entities::base_items::MediaStreamInfoEntity>,
        >,
        ServiceError,
    > {
        unimplemented!("stub")
    }
}

/// Like [`EnabledStub`] but every plugin reads as DISABLED.
/// A plugin manager stub: every plugin enabled, canned config JSON.
pub struct DisabledStub(pub Vec<u8>);

#[async_trait::async_trait]
impl PluginManager for DisabledStub {
    async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_plugin(&self, id: Uuid) -> Result<Option<PluginDescriptor>, ServiceError> {
        Ok(Some(PluginDescriptor {
            id,
            enabled: false,
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

// ── G2 write-capability stubs ──
#[allow(unused_imports)]
use ferrofin_model::lyrics::{LyricDto, RemoteLyricInfoDto};
#[allow(unused_imports)]
use ferrofin_model::providers::{LyricProviderInfo, RemoteSubtitleInfo, SubtitleProviderInfo};
#[allow(unused_imports)]
use ferrofin_traits::subtitles::{SubtitleResponse, SubtitleSearchRequest};

/// Recording/panic stub for the G2 write capabilities.
#[derive(Default)]
pub struct StubLyrics {
    /// The recorded write calls (method, item-id, detail).
    pub writes: Mutex<Vec<(String, String, String)>>,
}

#[async_trait::async_trait]
#[allow(clippy::unimplemented)]
impl ferrofin_traits::stubs::LyricManager for StubLyrics {
    async fn get_lyrics(&self, _item_id: Uuid) -> Result<Option<LyricDto>, ServiceError> {
        unimplemented!("stub")
    }
    async fn search_lyrics(&self, _item_id: Uuid) -> Result<Vec<RemoteLyricInfoDto>, ServiceError> {
        unimplemented!("stub")
    }
    async fn download_lyrics(
        &self,
        _item_id: Uuid,
        _lyric_id: &str,
    ) -> Result<Option<LyricDto>, ServiceError> {
        unimplemented!("stub")
    }
    async fn save_lyric(
        &self,
        item_id: Uuid,
        format: &str,
        lyrics: &str,
    ) -> Result<Option<LyricDto>, ServiceError> {
        self.writes.lock().unwrap().push((
            "lyric".into(),
            item_id.to_string(),
            format!("{format}:{}", lyrics.len()),
        ));
        Ok(None)
    }
    async fn delete_lyrics(&self, _item_id: Uuid) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
    async fn get_supported_providers(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<LyricProviderInfo>, ServiceError> {
        unimplemented!("stub")
    }
}

/// Recording/panic stub for the G2 write capabilities.
#[derive(Default)]
pub struct StubSubtitles {
    /// The recorded write calls (method, item-id, detail).
    pub writes: Mutex<Vec<(String, String, String)>>,
}

#[async_trait::async_trait]
#[allow(clippy::unimplemented)]
impl ferrofin_traits::subtitles::SubtitleManager for StubSubtitles {
    async fn search_subtitles(
        &self,
        _request: &SubtitleSearchRequest,
    ) -> Result<Vec<RemoteSubtitleInfo>, ServiceError> {
        unimplemented!("stub")
    }
    async fn download_subtitles(
        &self,
        _item_id: Uuid,
        _subtitle_id: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
    async fn upload_subtitle(
        &self,
        item_id: Uuid,
        response: &ferrofin_traits::subtitles::SubtitleResponse,
    ) -> Result<(), ServiceError> {
        self.writes.lock().unwrap().push((
            "subtitle".into(),
            item_id.to_string(),
            format!(
                "{}:{}:{}",
                response.language,
                response.format,
                response.content.len()
            ),
        ));
        Ok(())
    }
    async fn get_remote_subtitles(&self, _id: &str) -> Result<SubtitleResponse, ServiceError> {
        unimplemented!("stub")
    }
    async fn delete_subtitles(&self, _item_id: Uuid, _index: i32) -> Result<(), ServiceError> {
        unimplemented!("stub")
    }
    async fn get_supported_providers(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<SubtitleProviderInfo>, ServiceError> {
        unimplemented!("stub")
    }
}

/// Recording/panic stub for the G2 write capabilities.
#[derive(Default)]
pub struct StubCollections {
    /// The recorded write calls (method, item-id, detail).
    pub writes: Mutex<Vec<(String, String, String)>>,
}

#[async_trait::async_trait]
#[allow(clippy::unimplemented)]
impl ferrofin_traits::collections::CollectionManager for StubCollections {
    async fn create_collection(
        &self,
        options: &ferrofin_traits::collections::CollectionCreationOptions,
    ) -> Result<BaseItemEntity, ServiceError> {
        self.writes.lock().unwrap().push((
            "create".into(),
            options.name.clone(),
            options.item_id_list.len().to_string(),
        ));
        Ok(BaseItemEntity {
            id: "12121212-3434-5656-7878-909090909090".to_owned(),
            ..BaseItemEntity::default()
        })
    }
    async fn add_to_collection(
        &self,
        collection_id: Uuid,
        item_ids: &[Uuid],
    ) -> Result<(), ServiceError> {
        self.writes.lock().unwrap().push((
            "add".into(),
            collection_id.to_string(),
            item_ids.len().to_string(),
        ));
        Ok(())
    }
    async fn remove_from_collection(
        &self,
        collection_id: Uuid,
        item_ids: &[Uuid],
    ) -> Result<(), ServiceError> {
        self.writes.lock().unwrap().push((
            "remove".into(),
            collection_id.to_string(),
            item_ids.len().to_string(),
        ));
        Ok(())
    }
    async fn get_collections_containing_item(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        unimplemented!("stub")
    }
    async fn get_collections_folder(
        &self,
        _create_if_needed: bool,
    ) -> Result<Option<BaseItemEntity>, ServiceError> {
        unimplemented!("stub")
    }
}
