//! Display-preferences handler tests: GET folds scalar row fields into
//! `CustomPrefs`; POST parses the scalars back out and strips them from custom.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `ferrofin-traits` impls that authenticate and return canned prefs. Managers
//! a given handler never touches reuse the `test_support` panic fakes.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeAdminUsers, FakeApiKeys, FakeAppHost, FakeClientEventLogger, FakeCollections, FakeDevices,
    FakeDto, FakeLibrary, FakeLocalization, FakeLyrics, FakeMediaSegments, FakeMediaSources,
    FakeMusic, FakePlaylists, FakeProviders, FakeQuickConnect, FakeSearch, FakeSessions,
    FakeSimilarItems, FakeSubtitles, FakeTasks, FakeTrickplay, FakeTvSeries, FakeUserData,
    FakeUserViews, FakeUsers,
};
use ferrofin_db::entities::display_preferences::{
    DisplayPreferencesEntity, HomeSectionEntity, ItemDisplayPreferencesEntity,
};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::branding::BrandingOptions;
use ferrofin_model::configuration::ServerConfiguration;
use ferrofin_model::system::{FolderStorageInfo, PublicSystemInfo, SystemInfo, SystemStorageInfo};
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::AuthorizationInfo;
use ferrofin_traits::system::{ServerApplicationPaths, SystemManager};
use tower::ServiceExt;
use uuid::Uuid;

const USER_ID: Uuid = Uuid::from_u128(0x00D1_0000);

/// Another account, never the caller — the target of the cross-user probes.
const OTHER_USER_ID: Uuid = Uuid::from_u128(0x00D1_0001);

/// A minimal authenticated user.
fn user() -> UserEntity {
    UserEntity {
        id: USER_ID.to_string(),
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
        username: "bob".to_owned(),
    }
}

/// An auth stub that authenticates as [`USER_ID`].
struct OkAuth;

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            token: Some("tok".into()),
            user: Some(user()),
            is_authenticated: true,
            ..Default::default()
        })
    }
}

#[async_trait]
impl AuthorizationContext for OkAuth {
    async fn get_authorization_info(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            token: Some("tok".into()),
            user: Some(user()),
            is_authenticated: true,
            ..Default::default()
        })
    }
}

/// A configuration manager returning canned config + branding.
#[derive(Default)]
struct StubConfig {
    branding: Mutex<BrandingOptions>,
    paths: StubPaths,
}

/// Application paths returning a fixed log directory.
#[derive(Default, Clone)]
struct StubPaths {
    log_dir: String,
}

impl ServerApplicationPaths for StubPaths {
    fn root_folder_path(&self) -> String {
        String::new()
    }
    fn default_user_views_path(&self) -> String {
        String::new()
    }
    fn people_path(&self) -> String {
        String::new()
    }
    fn genre_path(&self) -> String {
        String::new()
    }
    fn music_genre_path(&self) -> String {
        String::new()
    }
    fn studio_path(&self) -> String {
        String::new()
    }
    fn year_path(&self) -> String {
        String::new()
    }
    fn artists_path(&self) -> String {
        String::new()
    }
    fn user_configuration_directory_path(&self) -> String {
        String::new()
    }
    fn internal_metadata_path(&self) -> String {
        String::new()
    }
    fn program_data_path(&self) -> String {
        String::new()
    }
    fn web_path(&self) -> String {
        String::new()
    }
    fn data_path(&self) -> String {
        String::new()
    }
    fn image_cache_path(&self) -> String {
        String::new()
    }
    fn cache_path(&self) -> String {
        String::new()
    }
    fn log_directory_path(&self) -> String {
        self.log_dir.clone()
    }
}

#[async_trait]
impl ServerConfigurationManager for StubConfig {
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        Arc::new(self.paths.clone())
    }
    async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
        Ok(Arc::new(ServerConfiguration {
            server_name: "Ferrofin".to_owned(),
            ..Default::default()
        }))
    }
    async fn update_configuration(
        &self,
        _configuration: &ServerConfiguration,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_branding(&self) -> Result<BrandingOptions, ServiceError> {
        Ok(self.branding.lock().unwrap().clone())
    }
    async fn update_branding(&self, branding: &BrandingOptions) -> Result<(), ServiceError> {
        *self.branding.lock().unwrap() = branding.clone();
        Ok(())
    }
}

/// A system manager returning canned public/full info + storage.
struct StubSystem;

#[async_trait]
impl SystemManager for StubSystem {
    async fn get_system_info(&self, _request: &RequestContext) -> Result<SystemInfo, ServiceError> {
        Ok(SystemInfo::default())
    }
    async fn get_public_system_info(
        &self,
        _request: &RequestContext,
    ) -> Result<PublicSystemInfo, ServiceError> {
        Ok(PublicSystemInfo::default())
    }
    async fn restart(&self) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn shutdown(&self) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_system_storage_info(&self) -> Result<SystemStorageInfo, ServiceError> {
        Ok(SystemStorageInfo {
            program_data_folder: FolderStorageInfo {
                path: "/data".to_owned(),
                resolved_path: "/data".to_owned(),
                free_space: 100,
                used_space: 50,
                storage_type: None,
                device_id: None,
            },
            ..Default::default()
        })
    }
}

/// A display-preferences manager returning a canned row + item prefs, capturing
/// the last saved row.
#[derive(Default)]
struct StubDisplayPreferences {
    row: Mutex<Option<DisplayPreferencesEntity>>,
    item: Mutex<Option<ItemDisplayPreferencesEntity>>,
    custom: Mutex<Option<std::collections::HashMap<String, Option<String>>>>,
    sections: Mutex<Option<Vec<(i32, i32)>>>,
}

fn canned_prefs() -> DisplayPreferencesEntity {
    DisplayPreferencesEntity {
        id: 1,
        chromecast_version: 1,
        client: "web".to_owned(),
        dashboard_theme: Some("dark".to_owned()),
        enable_next_video_info_overlay: true,
        index_by: Some(1),
        // Stored the way `guid_to_db` writes it — uppercase hyphenated.
        item_id: "AAAAAAAA-1111-1111-1111-111111111111".to_owned(),
        scroll_direction: 1,
        show_backdrop: true,
        show_sidebar: false,
        skip_backward_length: 10000,
        skip_forward_length: 30000,
        tv_home: Some("mytv".to_owned()),
        user_id: USER_ID.to_string(),
    }
}

fn canned_item_prefs() -> ItemDisplayPreferencesEntity {
    ItemDisplayPreferencesEntity {
        id: 1,
        client: "web".to_owned(),
        index_by: None,
        item_id: "11111111-1111-1111-1111-111111111111".to_owned(),
        remember_indexing: true,
        remember_sorting: false,
        sort_by: "SortName".to_owned(),
        sort_order: 1,
        user_id: USER_ID.to_string(),
        view_type: 0,
    }
}

#[async_trait]
impl ferrofin_traits::configuration::DisplayPreferencesManager for StubDisplayPreferences {
    async fn get_display_preferences(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _client: &str,
    ) -> Result<DisplayPreferencesEntity, ServiceError> {
        Ok(canned_prefs())
    }
    async fn get_item_display_preferences(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _client: &str,
    ) -> Result<ItemDisplayPreferencesEntity, ServiceError> {
        Ok(canned_item_prefs())
    }
    async fn list_item_display_preferences(
        &self,
        _user_id: Uuid,
        _client: &str,
    ) -> Result<Vec<ItemDisplayPreferencesEntity>, ServiceError> {
        Ok(vec![canned_item_prefs()])
    }
    async fn list_custom_item_display_preferences(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _client: &str,
    ) -> Result<std::collections::HashMap<String, Option<String>>, ServiceError> {
        let mut m = std::collections::HashMap::new();
        m.insert("customKey".to_owned(), Some("customVal".to_owned()));
        Ok(m)
    }
    async fn set_custom_item_display_preferences(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _client: &str,
        custom_preferences: &std::collections::HashMap<String, Option<String>>,
    ) -> Result<(), ServiceError> {
        *self.custom.lock().unwrap() = Some(custom_preferences.clone());
        Ok(())
    }
    async fn update_display_preferences(
        &self,
        display_preferences: &DisplayPreferencesEntity,
    ) -> Result<(), ServiceError> {
        *self.row.lock().unwrap() = Some(display_preferences.clone());
        Ok(())
    }
    async fn update_item_display_preferences(
        &self,
        item_display_preferences: &ItemDisplayPreferencesEntity,
    ) -> Result<(), ServiceError> {
        *self.item.lock().unwrap() = Some(item_display_preferences.clone());
        Ok(())
    }
    async fn list_home_sections(
        &self,
        _display_preferences_id: i64,
    ) -> Result<Vec<HomeSectionEntity>, ServiceError> {
        Ok(Vec::new())
    }
    async fn set_home_sections(
        &self,
        _display_preferences_id: i64,
        sections: &[(i32, i32)],
    ) -> Result<(), ServiceError> {
        *self.sections.lock().unwrap() = Some(sections.to_vec());
        Ok(())
    }
}

/// Builds an [`AppState`] whose display-preferences manager is `prefs`, with the
/// caller reporting as an ordinary (non-administrator) account.
fn state_with_display_prefs(
    prefs: Arc<dyn ferrofin_traits::configuration::DisplayPreferencesManager>,
) -> AppState {
    state_with_display_prefs_as(prefs, Arc::new(FakeUsers))
}

/// Builds an [`AppState`] whose display-preferences manager is `prefs` and whose
/// caller's role comes from `users` — [`FakeUsers`] for an ordinary account,
/// [`FakeAdminUsers`] for an administrator.
fn state_with_display_prefs_as(
    prefs: Arc<dyn ferrofin_traits::configuration::DisplayPreferencesManager>,
    users: Arc<dyn ferrofin_traits::library::UserManager>,
) -> AppState {
    let auth = Arc::new(OkAuth);
    AppState::new(
        Arc::new(FakeLibrary),
        users,
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(StubSystem),
        Arc::new(FakeAppHost),
        Arc::new(StubConfig::default()),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        auth.clone(),
        auth,
        Arc::new(FakeQuickConnect),
        Arc::new(FakePlaylists),
        Arc::new(FakeCollections),
        Arc::new(FakeTvSeries),
        Arc::new(FakeSubtitles),
        Arc::new(FakeLyrics),
        Arc::new(FakeMediaSegments),
        Arc::new(FakeTrickplay),
        Arc::new(FakeDevices),
        Arc::new(FakeClientEventLogger),
        Arc::new(FakeApiKeys),
        Arc::new(FakeLocalization),
        prefs,
        Arc::new(ferrofin_api::test_support::FakeActivity),
        Arc::new(ferrofin_api::test_support::FakeFileSystem),
        Arc::new(FakeTasks),
    )
}

/// Sends an authenticated GET and returns `(status, body-bytes)`.
async fn get(app: AppState, uri: &str) -> (StatusCode, Vec<u8>) {
    send(app, "GET", uri, Body::empty(), None).await
}

async fn send(
    app: AppState,
    method: &str,
    uri: &str,
    body: Body,
    content_type: Option<&str>,
) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("X-Emby-Token", "tok");
    if let Some(ct) = content_type {
        builder = builder.header("content-type", ct);
    }
    let response = create_router(app)
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("valid JSON body")
}

#[tokio::test]
async fn display_preferences_get_folds_scalars_into_custom_prefs() {
    let app = state_with_display_prefs(Arc::new(StubDisplayPreferences::default()));
    let (status, body) = get(app, "/DisplayPreferences/home?client=web").await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    // Scalar row fields surface under CustomPrefs.
    assert_eq!(v["CustomPrefs"]["chromecastVersion"], "unstable");
    assert_eq!(v["CustomPrefs"]["skipForwardLength"], "30000");
    assert_eq!(v["CustomPrefs"]["dashboardTheme"], "dark");
    // Stored custom prefs are carried through.
    assert_eq!(v["CustomPrefs"]["customKey"], "customVal");
    // C# returns `ItemId.ToString()` — the lowercase hyphenated Guid — while the
    // column stores `guid_to_db`'s uppercase form.
    assert_eq!(v["Id"], "aaaaaaaa-1111-1111-1111-111111111111");
    // IndexBy discriminant (1) → ProductionYear; scroll direction (1) → Vertical.
    assert_eq!(v["IndexBy"], "ProductionYear");
    assert_eq!(v["ScrollDirection"], "Vertical");
    assert_eq!(v["SortOrder"], "Descending");
}

#[tokio::test]
async fn display_preferences_post_parses_scalars_back() {
    let prefs = Arc::new(StubDisplayPreferences::default());
    let app = state_with_display_prefs(prefs.clone());
    let body = r#"{
        "CustomPrefs": {
            "chromecastVersion": "stable",
            "skipForwardLength": "5000",
            "skipBackLength": "5000",
            "enableNextVideoInfoOverlay": "False",
            "dashboardTheme": "light",
            "tvhome": "grid",
            "homesection0": "resume",
            "keepMe": "yes"
        },
        "SortBy": "DateCreated",
        "SortOrder": "Ascending",
        "ScrollDirection": "Horizontal",
        "ShowBackdrop": false,
        "ShowSidebar": true,
        "RememberIndexing": false,
        "RememberSorting": true,
        "PrimaryImageHeight": 250,
        "PrimaryImageWidth": 250
    }"#;
    let (status, _) = send(
        app,
        "POST",
        "/DisplayPreferences/home?client=web",
        Body::from(body),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let saved = prefs.row.lock().unwrap().clone().unwrap();
    assert_eq!(saved.chromecast_version, 0); // stable
    assert_eq!(saved.skip_forward_length, 5000);
    assert!(!saved.enable_next_video_info_overlay);
    assert_eq!(saved.dashboard_theme.as_deref(), Some("light"));
    assert_eq!(saved.tv_home.as_deref(), Some("grid"));
    assert!(!saved.show_backdrop);
    assert!(saved.show_sidebar);

    let item = prefs.item.lock().unwrap().clone().unwrap();
    assert_eq!(item.sort_by, "DateCreated");
    assert_eq!(item.sort_order, 0); // Ascending

    // The scalar + homesection keys are stripped; only "keepMe" remains custom.
    let custom = prefs.custom.lock().unwrap().clone().unwrap();
    assert!(custom.contains_key("keepMe"));
    assert!(!custom.contains_key("chromecastVersion"));
    assert!(!custom.contains_key("homesection0"));
    // ...and the homesection key became a real HomeSection row (Resume = 4).
    let sections = prefs.sections.lock().unwrap().clone().unwrap();
    assert_eq!(sections, vec![(0, 4)]);
}

/// With no `skipBackLength`/`skipForwardLength` in `CustomPrefs`, the C#
/// controller stores 10000/30000 — NOT one shared constant. (Upstream v12.0-rc3
/// unified both to 15000; the pinned oracle is 10.11.8.)
#[tokio::test]
async fn display_preferences_post_skip_length_fallbacks_are_10000_and_30000() {
    let prefs = Arc::new(StubDisplayPreferences::default());
    let app = state_with_display_prefs(prefs.clone());
    let body = r#"{
        "CustomPrefs": { "foo": "bar" },
        "ScrollDirection": "Horizontal",
        "SortOrder": "Ascending",
        "ShowBackdrop": false,
        "ShowSidebar": true,
        "RememberIndexing": false,
        "RememberSorting": true,
        "PrimaryImageHeight": 250,
        "PrimaryImageWidth": 250
    }"#;
    let (status, _) = send(
        app,
        "POST",
        "/DisplayPreferences/home?client=web",
        Body::from(body),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let saved = prefs.row.lock().unwrap().clone().unwrap();
    assert_eq!(saved.skip_backward_length, 10_000);
    assert_eq!(saved.skip_forward_length, 30_000);
}

/// A `landing-*` key whose value does not name a `ViewType` is dropped rather
/// than persisted; a valid one survives.
#[tokio::test]
async fn display_preferences_post_strips_invalid_landing_keys() {
    let prefs = Arc::new(StubDisplayPreferences::default());
    let app = state_with_display_prefs(prefs.clone());
    let body = r#"{
        "CustomPrefs": {
            "landing-abc": "movies",
            "landing-bad": "notaviewtype",
            "keepMe": "yes"
        },
        "ScrollDirection": "Horizontal",
        "SortOrder": "Ascending",
        "ShowBackdrop": false,
        "ShowSidebar": true,
        "RememberIndexing": false,
        "RememberSorting": true,
        "PrimaryImageHeight": 250,
        "PrimaryImageWidth": 250
    }"#;
    let (status, _) = send(
        app,
        "POST",
        "/DisplayPreferences/home?client=web",
        Body::from(body),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let custom = prefs.custom.lock().unwrap().clone().unwrap();
    assert!(
        custom.contains_key("landing-abc"),
        "valid ViewType survives"
    );
    assert!(
        !custom.contains_key("landing-bad"),
        "invalid ViewType stripped"
    );
    assert!(custom.contains_key("keepMe"), "unrelated keys untouched");
}

/// Home sections round-trip through `CustomPrefs`, and an unparseable type
/// falls back to `defaults[order]` (order 3 -> ResumeBook), while an order at or
/// beyond that 8-entry table falls back to `None`.
#[tokio::test]
async fn display_preferences_home_sections_round_trip_with_default_substitution() {
    let (v, _saved) = round_trip(
        r#"{
            "CustomPrefs": {
                "homesection0": "smalllibrarytiles",
                "homesection1": "resume",
                "homesection3": "bogusvalue",
                "homesection9": "alsobogus"
            },
            "ScrollDirection": "Horizontal",
            "SortOrder": "Ascending",
            "ShowBackdrop": false,
            "ShowSidebar": true,
            "RememberIndexing": false,
            "RememberSorting": true,
            "PrimaryImageHeight": 250,
            "PrimaryImageWidth": 250
        }"#,
    )
    .await;

    assert_eq!(v["CustomPrefs"]["homesection0"], "smalllibrarytiles");
    assert_eq!(v["CustomPrefs"]["homesection1"], "resume");
    // defaults[3] == HomeSectionType.ResumeBook
    assert_eq!(v["CustomPrefs"]["homesection3"], "resumebook");
    // order >= 8 has no default entry -> None
    assert_eq!(v["CustomPrefs"]["homesection9"], "none");
}

/// A display-preferences manager that actually round-trips: `get` returns the
/// last row `update` saved (falling back to the canned row), so a POST→GET test
/// observes the write path rather than a canned answer.
#[derive(Default)]
struct RoundTripDisplayPreferences {
    row: Mutex<Option<DisplayPreferencesEntity>>,
    sections: Mutex<Vec<(i32, i32)>>,
}

#[async_trait]
impl ferrofin_traits::configuration::DisplayPreferencesManager for RoundTripDisplayPreferences {
    async fn get_display_preferences(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _client: &str,
    ) -> Result<DisplayPreferencesEntity, ServiceError> {
        Ok(self
            .row
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(canned_prefs))
    }
    async fn get_item_display_preferences(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _client: &str,
    ) -> Result<ItemDisplayPreferencesEntity, ServiceError> {
        Ok(canned_item_prefs())
    }
    async fn list_item_display_preferences(
        &self,
        _user_id: Uuid,
        _client: &str,
    ) -> Result<Vec<ItemDisplayPreferencesEntity>, ServiceError> {
        Ok(vec![canned_item_prefs()])
    }
    async fn list_custom_item_display_preferences(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _client: &str,
    ) -> Result<std::collections::HashMap<String, Option<String>>, ServiceError> {
        Ok(std::collections::HashMap::new())
    }
    async fn set_custom_item_display_preferences(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _client: &str,
        _custom_preferences: &std::collections::HashMap<String, Option<String>>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn update_display_preferences(
        &self,
        display_preferences: &DisplayPreferencesEntity,
    ) -> Result<(), ServiceError> {
        *self.row.lock().unwrap() = Some(display_preferences.clone());
        Ok(())
    }
    async fn update_item_display_preferences(
        &self,
        _item_display_preferences: &ItemDisplayPreferencesEntity,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn list_home_sections(
        &self,
        _display_preferences_id: i64,
    ) -> Result<Vec<HomeSectionEntity>, ServiceError> {
        Ok(self
            .sections
            .lock()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, (order, type_))| HomeSectionEntity {
                id: i64::try_from(i).unwrap_or(0) + 1,
                display_preferences_id: 1,
                order: *order,
                type_: *type_,
            })
            .collect())
    }
    async fn set_home_sections(
        &self,
        _display_preferences_id: i64,
        sections: &[(i32, i32)],
    ) -> Result<(), ServiceError> {
        let mut stored = sections.to_vec();
        stored.sort_unstable();
        *self.sections.lock().unwrap() = stored;
        Ok(())
    }
}

/// POSTs `body` then GETs, against one round-tripping manager; returns the GET
/// body as JSON plus the saved row.
async fn round_trip(body: &'static str) -> (serde_json::Value, DisplayPreferencesEntity) {
    let prefs = Arc::new(RoundTripDisplayPreferences::default());
    let (status, _) = send(
        state_with_display_prefs(prefs.clone()),
        "POST",
        "/DisplayPreferences/home?client=web",
        Body::from(body),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, get_body) = get(
        state_with_display_prefs(prefs.clone()),
        "/DisplayPreferences/home?client=web",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let saved = prefs.row.lock().unwrap().clone().expect("row was saved");
    (json(&get_body), saved)
}

/// An explicit JSON `null` for `dashboardTheme`/`tvhome` must survive the POST →
/// GET round trip as `null`, not be flattened into `""`. Upstream stores the
/// dictionary's null verbatim (`TryGetValue(…, out var theme) ? theme : …` over
/// a `Dictionary<string, string?>`, `DisplayPreferences.DashboardTheme` being
/// `string?`), and the GET path hands the stored value back unchanged.
#[tokio::test]
async fn display_preferences_round_trip_preserves_explicit_null() {
    let (v, saved) = round_trip(
        r#"{
            "CustomPrefs": { "dashboardTheme": null, "tvhome": null },
            "ScrollDirection": "Horizontal",
            "SortOrder": "Ascending",
            "ShowBackdrop": false,
            "ShowSidebar": true,
            "RememberIndexing": false,
            "RememberSorting": true,
            "PrimaryImageHeight": 250,
            "PrimaryImageWidth": 250
        }"#,
    )
    .await;

    // The stored column stays NULL...
    assert_eq!(saved.dashboard_theme, None);
    assert_eq!(saved.tv_home, None);

    // ...and — the client-visible half — serializes as JSON `null`. Index-only
    // assertions would also pass for a *missing* key, so require the key first.
    let custom = v["CustomPrefs"].as_object().expect("CustomPrefs object");
    assert!(custom.contains_key("dashboardTheme"));
    assert!(custom.contains_key("tvhome"));
    assert_eq!(custom["dashboardTheme"], serde_json::Value::Null);
    assert_eq!(custom["tvhome"], serde_json::Value::Null);
    assert_ne!(custom["dashboardTheme"], serde_json::json!(""));
    assert_ne!(custom["tvhome"], serde_json::json!(""));
}

/// The *absent* key is the opposite case and must NOT become null: upstream's
/// `TryGetValue(…) ? theme : string.Empty` stores the empty string when the
/// client omits the key, so the round trip yields `""`. Pinned so the null fix
/// above is not over-applied to absence.
#[tokio::test]
async fn display_preferences_round_trip_absent_key_is_empty_string() {
    let (v, saved) = round_trip(
        r#"{
            "CustomPrefs": { "keepMe": "yes" },
            "ScrollDirection": "Horizontal",
            "SortOrder": "Ascending",
            "ShowBackdrop": false,
            "ShowSidebar": true,
            "RememberIndexing": false,
            "RememberSorting": true,
            "PrimaryImageHeight": 250,
            "PrimaryImageWidth": 250
        }"#,
    )
    .await;

    assert_eq!(saved.dashboard_theme.as_deref(), Some(""));
    assert_eq!(saved.tv_home.as_deref(), Some(""));
    assert_eq!(v["CustomPrefs"]["dashboardTheme"], serde_json::json!(""));
    assert_eq!(v["CustomPrefs"]["tvhome"], serde_json::json!(""));
}

/// The GET honours C# `RequestHelpers.GetUserId`: naming another user's id as a
/// non-administrator is refused, not served.
///
/// Measured live before the fix (lane-3 pair, `bench` reading `probe`'s row):
/// Ferrofin `200` with the other user's preferences, Jellyfin `403`.
#[tokio::test]
async fn display_preferences_get_cross_user_as_non_admin_is_forbidden() {
    let app = state_with_display_prefs(Arc::new(StubDisplayPreferences::default()));
    let (status, _) = get(
        app,
        &format!("/DisplayPreferences/home?client=web&userId={OTHER_USER_ID}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// The POST is the exploitable half — an ungated `userId` let any authenticated
/// account **overwrite** another account's row. Refused for a non-administrator.
#[tokio::test]
async fn display_preferences_post_cross_user_as_non_admin_is_forbidden() {
    let prefs = Arc::new(StubDisplayPreferences::default());
    let app = state_with_display_prefs(prefs.clone());
    let (status, _) = send(
        app,
        "POST",
        &format!("/DisplayPreferences/home?client=web&userId={OTHER_USER_ID}"),
        Body::from(r#"{"CustomPrefs":{},"ScrollDirection":"Horizontal","SortOrder":"Ascending","ShowBackdrop":false,"ShowSidebar":false,"RememberIndexing":false,"RememberSorting":false,"PrimaryImageHeight":250,"PrimaryImageWidth":250}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Nothing was written: the refusal happens before the row is touched.
    assert!(prefs.row.lock().unwrap().is_none());
}

/// The other half of the rule: an **administrator** naming another user's id is
/// still served. A gate that refuses everyone is not a port of `GetUserId`.
#[tokio::test]
async fn display_preferences_cross_user_as_admin_is_allowed() {
    let get_app = state_with_display_prefs_as(
        Arc::new(StubDisplayPreferences::default()),
        Arc::new(FakeAdminUsers),
    );
    let (status, _) = get(
        get_app,
        &format!("/DisplayPreferences/home?client=web&userId={OTHER_USER_ID}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let prefs = Arc::new(StubDisplayPreferences::default());
    let post_app = state_with_display_prefs_as(prefs.clone(), Arc::new(FakeAdminUsers));
    let (status, _) = send(
        post_app,
        "POST",
        &format!("/DisplayPreferences/home?client=web&userId={OTHER_USER_ID}"),
        Body::from(r#"{"CustomPrefs":{},"ScrollDirection":"Horizontal","SortOrder":"Ascending","ShowBackdrop":false,"ShowSidebar":false,"RememberIndexing":false,"RememberSorting":false,"PrimaryImageHeight":250,"PrimaryImageWidth":250}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(prefs.row.lock().unwrap().is_some());
}

/// ...and a non-administrator naming **their own** id is served, exactly as
/// omitting the parameter is.
#[tokio::test]
async fn display_preferences_self_as_non_admin_is_allowed() {
    let app = state_with_display_prefs(Arc::new(StubDisplayPreferences::default()));
    let (status, _) = get(
        app,
        &format!("/DisplayPreferences/home?client=web&userId={USER_ID}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let prefs = Arc::new(StubDisplayPreferences::default());
    let app = state_with_display_prefs(prefs.clone());
    let (status, _) = send(
        app,
        "POST",
        &format!("/DisplayPreferences/home?client=web&userId={USER_ID}"),
        Body::from(r#"{"CustomPrefs":{},"ScrollDirection":"Horizontal","SortOrder":"Ascending","ShowBackdrop":false,"ShowSidebar":false,"RememberIndexing":false,"RememberSorting":false,"PrimaryImageHeight":250,"PrimaryImageWidth":250}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(prefs.row.lock().unwrap().is_some());
}
