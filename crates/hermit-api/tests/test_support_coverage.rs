//! Executes the `test_support` fake trait doubles so their (deliberately
//! `unimplemented!()`) bodies are still *reached* — driving the DI-scaffolding
//! lines the routing/handler tests never touch.
//!
//! Each fake method is `unimplemented!("fake")`, so awaiting it panics; every
//! call here is wrapped in [`catch_unwind`] and the panic is asserted. Two
//! managers (`FakeSystem`, `FakeConfig`/`FakeAppHost`/`FakePaths`) have real
//! return values and are called directly. This is pure test scaffolding — it
//! asserts the fakes behave as documented (panic when a stray handler reaches
//! them), which is the property the other tests rely on to catch bugs.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use hermit_api::test_support::{
    FakeAppHost, FakeConfig, FakeDto, FakeLibrary, FakeMediaSources, FakePaths, FakeSessions,
    FakeSystem, FakeUserData, FakeUserViews, FakeUsers,
};
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_db::entities::users::UserEntity;
use hermit_traits::configuration::ServerConfigurationManager;
use hermit_traits::dto::DtoService;
use hermit_traits::library::{
    LibraryManager, MediaSourceManager, UserDataManager, UserManager, UserViewManager,
};
use hermit_traits::net::RequestContext;
use hermit_traits::options::{DeleteOptions, DtoOptions, InternalItemsQuery, InternalPeopleQuery};
use hermit_traits::session::{AuthenticationRequest, SessionManager};
use hermit_traits::system::{ServerApplicationHost, ServerApplicationPaths, SystemManager};
use uuid::Uuid;

/// Runs `fut` to completion on a fresh current-thread runtime, asserting it
/// panics (the fake's `unimplemented!()`), with the default panic hook silenced.
fn assert_panics<F: Future>(fut: F) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(fut)
    }));
    std::panic::set_hook(prev);
    assert!(result.is_err(), "fake method was expected to panic");
}

/// A minimal [`UserEntity`] for methods that take one by reference.
fn user() -> UserEntity {
    UserEntity {
        id: Uuid::nil().to_string(),
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
        password: None,
        password_reset_provider_id: String::new(),
        play_default_audio_track: false,
        remember_audio_selections: false,
        remember_subtitle_selections: false,
        remote_client_bitrate_limit: None,
        row_version: 0,
        subtitle_language_preference: None,
        subtitle_mode: 0,
        sync_play_access: 0,
        username: String::new(),
    }
}

#[test]
fn fake_library_methods_panic() {
    let f = FakeLibrary;
    let q = &InternalItemsQuery::default();
    let people = &InternalPeopleQuery::default();
    assert_panics(f.get_item_by_id(Uuid::nil()));
    assert_panics(f.query_items(q));
    assert_panics(f.get_item_ids(q));
    assert_panics(f.get_item_list(q));
    assert_panics(f.get_latest_item_list(q, hermit_model::data::CollectionType::movies));
    assert_panics(f.create_items(&[], None));
    assert_panics(f.update_items(&[], None));
    assert_panics(f.delete_item(Uuid::nil(), &DeleteOptions::default()));
    assert_panics(f.get_people(people));
    assert_panics(f.get_people_names(people));
    assert_panics(f.get_count(q));
    assert_panics(f.get_item_counts(q));
    assert_panics(f.get_genres(q));
    assert_panics(f.get_studios(q));
    assert_panics(f.get_artists(q));
    assert_panics(f.get_query_filters_legacy(q));
    assert_panics(f.queue_library_scan());
}

#[test]
fn fake_users_methods_panic() {
    let f = FakeUsers;
    assert_panics(f.get_users());
    assert_panics(f.get_user_ids());
    assert_panics(f.initialize());
    assert_panics(f.get_first_user());
    assert_panics(f.get_user_by_name("x"));
    assert_panics(f.rename_user(Uuid::nil(), "a", "b"));
    assert_panics(f.update_user(&user()));
    assert_panics(f.create_user("x"));
    assert_panics(f.delete_user(Uuid::nil()));
    assert_panics(f.reset_password(Uuid::nil()));
    assert_panics(f.change_password(Uuid::nil(), "p"));
    assert_panics(f.authenticate_user("u", "p", "1.2.3.4", true));
    assert_panics(f.get_authentication_providers());
    assert_panics(f.get_password_reset_providers());
    assert_panics(f.update_configuration(
        Uuid::nil(),
        &hermit_model::configuration::UserConfiguration::default(),
    ));
    assert_panics(f.update_policy(Uuid::nil(), &hermit_model::users::UserPolicy::default()));
    assert_panics(f.clear_profile_image(&user()));
}

#[test]
fn fake_user_views_methods_panic() {
    let f = FakeUserViews;
    let opts = DtoOptions::with_all_fields(false);
    assert_panics(f.get_user_views(Uuid::nil()));
    assert_panics(f.get_latest_items(Uuid::nil(), &opts));
}

#[test]
fn fake_user_data_methods_panic() {
    let f = FakeUserData;
    assert_panics(f.save_user_data(
        Uuid::nil(),
        Uuid::nil(),
        &hermit_model::dto::UpdateUserItemDataDto::default(),
    ));
    assert_panics(f.get_user_data_dto(Uuid::nil(), Uuid::nil()));
    assert_panics(f.get_user_data_batch(&[], Uuid::nil()));
    assert_panics(f.update_play_state(Uuid::nil(), Uuid::nil(), None));
    assert_panics(f.reset_playback_stream_selections(Uuid::nil(), Uuid::nil()));
}

#[test]
fn fake_media_sources_methods_panic() {
    let f = FakeMediaSources;
    assert_panics(f.get_media_streams(Uuid::nil()));
    assert_panics(f.get_media_attachments(Uuid::nil()));
    assert_panics(f.get_playback_media_sources(Uuid::nil(), Uuid::nil(), true, true));
    assert_panics(f.get_static_media_sources(Uuid::nil(), true, None));
    assert_panics(f.open_live_stream(&hermit_model::media_info::LiveStreamRequest::default()));
    assert_panics(f.get_live_stream("id"));
    assert_panics(f.close_live_stream("id"));
}

#[test]
fn fake_sessions_methods_panic() {
    let f = FakeSessions;
    assert_panics(f.log_session_activity("a", "v", "d", "n", "e", &user()));
    assert_panics(f.update_device_name("s", "n"));
    assert_panics(f.on_playback_start(&hermit_model::session::PlaybackStartInfo::default()));
    assert_panics(f.on_playback_progress(
        &hermit_model::session::PlaybackProgressInfo::default(),
        false,
    ));
    assert_panics(f.on_playback_stopped(&hermit_model::session::PlaybackStopInfo::default()));
    assert_panics(f.report_session_ended("s"));
    assert_panics(f.send_general_command("c", "s", &general_command()));
    assert_panics(f.send_message_command(
        "c",
        "s",
        &hermit_model::session::MessageCommand::default(),
    ));
    assert_panics(f.send_play_command("c", "s", &hermit_model::session::PlayRequest::default()));
    assert_panics(f.send_playstate_command(
        "c",
        "s",
        &hermit_model::session::PlaystateRequest::default(),
    ));
    assert_panics(f.send_message_to_admin_sessions(
        hermit_model::session::SessionMessageType::ForceKeepAlive,
        "d",
    ));
    // A no-op (not a panic): the user-data handlers push `UserDataChanged`
    // through it best-effort on every played/favorite/rating write.
    assert!(
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f.send_message_to_user_sessions(
                &[],
                hermit_model::session::SessionMessageType::ForceKeepAlive,
                "d",
            ))
            .is_ok()
    );
    assert_panics(f.send_message_to_user_device_sessions(
        "d",
        hermit_model::session::SessionMessageType::ForceKeepAlive,
        "d",
    ));
    assert_panics(f.send_restart_required_notification());
    assert_panics(f.add_additional_user("s", Uuid::nil()));
    assert_panics(f.remove_additional_user("s", Uuid::nil()));
    assert_panics(f.report_now_viewing_item("s", "i"));
    // The authenticate methods intentionally succeed (returning a deterministic
    // token) so handler tests can assert the token is echoed in the
    // `AuthenticationResult`; they do not panic like the rest of the fake.
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let new_session = rt
        .block_on(f.authenticate_new_session(&AuthenticationRequest::default()))
        .expect("fake authenticate returns Ok");
    assert_eq!(
        new_session.access_token.expose(),
        hermit_api::test_support::FAKE_ACCESS_TOKEN
    );
    let direct = rt
        .block_on(f.authenticate_direct(&AuthenticationRequest::default()))
        .expect("fake authenticate returns Ok");
    assert_eq!(
        direct.access_token.expose(),
        hermit_api::test_support::FAKE_ACCESS_TOKEN
    );
    assert_panics(
        f.report_capabilities("s", &hermit_model::session::ClientCapabilities::default()),
    );
    assert_panics(
        f.report_transcoding_info("d", &hermit_model::session::TranscodingInfo::default()),
    );
    assert_panics(f.clear_transcoding_info("d"));
    assert_panics(f.get_sessions(Uuid::nil(), None, None, None, false));
    assert_panics(f.get_session_by_authentication_token("t", "d", "e"));
    assert_panics(f.logout("t"));
    // `logout_device` is omitted: `DeviceEntity` carries non-`Option`
    // `DateTime<Utc>` fields that would pull `chrono` into this test just to
    // build a value the fake panics on before reading; the other 28 session
    // methods cover the impl.
    assert_panics(f.revoke_user_tokens(Uuid::nil(), "t"));
    assert_panics(f.close_live_stream_if_needed("l", "s"));
}

/// A minimal [`GeneralCommand`] (`GeneralCommand` has no `Default`).
fn general_command() -> hermit_model::session::GeneralCommand {
    hermit_model::session::GeneralCommand {
        name: hermit_model::session::GeneralCommandType::MoveUp,
        controlling_user_id: Uuid::nil(),
        arguments: std::collections::HashMap::new(),
    }
}

#[test]
fn fake_system_methods_run_and_panic() {
    let f = FakeSystem;
    let ctx = RequestContext {
        headers: Vec::new(),
        query_string: None,
        remote_endpoint: None,
    };
    // The two info getters return `Ok`, so they run without panicking.
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    assert!(rt.block_on(f.get_system_info(&ctx)).is_ok());
    assert!(rt.block_on(f.get_public_system_info(&ctx)).is_ok());
    // The lifecycle/storage methods stay `unimplemented!`.
    assert_panics(f.restart());
    assert_panics(f.shutdown());
    assert_panics(f.get_system_storage_info());
}

#[test]
fn fake_app_host_methods_run() {
    let f = FakeAppHost;
    assert!(f.core_startup_has_completed());
    assert_eq!(f.http_port(), 8096);
    assert_eq!(f.https_port(), 8920);
    assert!(!f.listen_with_https());
    assert_eq!(f.friendly_name(), "hermit-test");
    assert_eq!(f.expand_virtual_path("/p"), "/p");
    assert_eq!(f.reverse_virtual_path("/p"), "/p");
    let ctx = RequestContext {
        headers: Vec::new(),
        query_string: None,
        remote_endpoint: None,
    };
    assert_panics(f.get_smart_api_url(&ctx));
    assert_panics(f.get_local_api_url("host", None, None));
}

#[test]
fn fake_paths_are_empty_except_the_writable_data_root() {
    let p = FakePaths;
    for s in [
        p.root_folder_path(),
        p.default_user_views_path(),
        p.people_path(),
        p.genre_path(),
        p.music_genre_path(),
        p.studio_path(),
        p.year_path(),
        p.artists_path(),
        p.user_configuration_directory_path(),
        p.internal_metadata_path(),
        p.web_path(),
        p.image_cache_path(),
        p.cache_path(),
        p.log_directory_path(),
    ] {
        assert!(s.is_empty());
    }
    // `program_data_path`/`data_path` return a real temp dir so path-backed
    // handlers (Backup) have somewhere writable to work in tests.
    assert!(!p.program_data_path().is_empty());
    assert_eq!(p.data_path(), p.program_data_path());
}

#[test]
fn fake_config_methods_run() {
    let f = FakeConfig;
    // `application_paths` returns the fake paths.
    let _paths: Arc<dyn ServerApplicationPaths> = f.application_paths();
    // `configuration()` now returns a real default (the `FirstTimeSetupOrAuth`
    // extractor reads it, so the fake must answer rather than panic).
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    assert!(rt.block_on(f.configuration()).is_ok());
    // `update_configuration` stays `unimplemented!` and is left uncalled.
}

#[test]
fn fake_dto_methods_panic() {
    let f = FakeDto;
    let opts = DtoOptions::with_all_fields(false);
    let item = base_item();
    assert_panics(f.get_primary_image_aspect_ratio(Uuid::nil()));
    assert_panics(f.get_base_item_dto(&item, &opts, None, None));
    assert_panics(f.get_base_item_dtos(&[], &opts, None, None, true));
    assert_panics(f.get_item_by_name_dto(&item, &opts, None, None));
}

/// A minimal [`BaseItemEntity`] for the DTO fakes.
fn base_item() -> BaseItemEntity {
    BaseItemEntity {
        id: Uuid::nil().to_string(),
        album: None,
        album_artists: None,
        artists: None,
        audio: None,
        channel_id: None,
        clean_name: None,
        community_rating: None,
        critic_rating: None,
        custom_rating: None,
        data: None,
        date_created: None,
        date_last_media_added: None,
        date_last_refreshed: None,
        date_last_saved: None,
        date_modified: None,
        end_date: None,
        episode_title: None,
        external_id: None,
        external_series_id: None,
        external_service_id: None,
        extra_type: None,
        forced_sort_name: None,
        genres: None,
        height: None,
        index_number: None,
        inherited_parental_rating_sub_value: None,
        inherited_parental_rating_value: None,
        is_folder: false,
        is_in_mixed_folder: false,
        is_locked: false,
        is_movie: false,
        is_repeat: false,
        is_series: false,
        is_virtual_item: false,
        lufs: None,
        media_type: None,
        name: None,
        normalization_gain: None,
        official_rating: None,
        extra_ids: None,
        original_title: None,
        overview: None,
        owner_id: None,
        parent_id: None,
        parent_index_number: None,
        path: None,
        preferred_metadata_country_code: None,
        preferred_metadata_language: None,
        premiere_date: None,
        presentation_unique_key: None,
        primary_version_id: None,
        production_locations: None,
        production_year: None,
        run_time_ticks: None,
        season_id: None,
        season_name: None,
        series_id: None,
        series_name: None,
        series_presentation_unique_key: None,
        show_id: None,
        size: None,
        sort_name: None,
        start_date: None,
        studios: None,
        tagline: None,
        tags: None,
        top_parent_id: None,
        total_bitrate: None,
        type_: String::new(),
        unrated_type: None,
        width: None,
    }
}
