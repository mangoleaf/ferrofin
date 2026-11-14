//! `GET /UserViews`' `includeExternalContent` and `presetViews`, through the
//! wired server: `UserViewsController.GetUserViews` → `UserViewQuery` →
//! `UserViewManager.GetUserViews`.
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use ferrofin_db::Database;
use ferrofin_server::{config::Config, state::build_app_state};
use serde_json::{Value, json};
use tower::ServiceExt as _;

async fn call(
    router: &axum::Router,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let client = r#"MediaBrowser Client="user-views-test", Device="fixture", DeviceId="user-views-test", Version="1""#;
    let auth = token.map_or_else(
        || client.to_owned(),
        |token| format!(r#"{client}, Token="{token}""#),
    );
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, auth);
    let request = match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

async fn get(router: &axum::Router, token: &str, path: &str) -> Value {
    let (status, value) = call(router, Some(token), "GET", path, None).await;
    assert_eq!(status, StatusCode::OK, "{path}: {value}");
    value
}

async fn post(router: &axum::Router, token: &str, path: &str, value: Value) {
    let (status, body) = call(router, Some(token), "POST", path, Some(value)).await;
    assert!(status.is_success(), "{path}: {status} {body}");
}

/// The views' `(Name, Type, CollectionType)`.
fn views(page: &Value) -> Vec<(String, String, String)> {
    page["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            let text = |key: &str| item[key].as_str().unwrap_or_default().to_owned();
            (text("Name"), text("Type"), text("CollectionType"))
        })
        .collect()
}

fn view<'a>(page: &'a Value, name: &str) -> &'a Value {
    page["Items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["Name"] == name)
        .unwrap_or_else(|| panic!("no view {name}: {page}"))
}

struct Fixture {
    temp: tempfile::TempDir,
    db: Database,
    router: axum::Router,
    token: String,
    user: String,
    _wired: ferrofin_server::state::WiredApp,
}

async fn setup() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let config = Config {
        admin_user: "admin".to_owned(),
        admin_password: "user-views-pw".to_owned(),
        ..Config::test_stub(temp.path())
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    let db = Database::connect(&config.database_url()).await.unwrap();
    db.run_migrations().await.unwrap();
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        encoder_app_path_display: None,
        ffmpeg: "ffmpeg".into(),
        ffprobe: "ffprobe".into(),
        capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
        chromaprint_muxer: false,
    };
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, &config, &ffmpeg, None, shutdown_tx)
        .await
        .unwrap();
    ferrofin_server::seed::seed_default_admin(wired.state.users.as_ref(), &config)
        .await
        .unwrap();
    let router = ferrofin_api::create_router(wired.state.clone());
    let (status, auth) = call(
        &router,
        None,
        "POST",
        "/Users/AuthenticateByName",
        Some(json!({"Username":"admin","Pw":"user-views-pw"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    Fixture {
        db,
        router,
        token: auth["AccessToken"].as_str().unwrap().to_owned(),
        user: auth["User"]["Id"].as_str().unwrap().to_owned(),
        temp,
        _wired: wired,
    }
}

impl Fixture {
    async fn library(&self, name: &str, collection_type: &str) {
        let media = self.temp.path().join("media").join(name);
        std::fs::create_dir_all(&media).unwrap();
        let options = json!({"LibraryOptions":{"PathInfos":[{"Path":media.to_str().unwrap()}],
            "EnableRealtimeMonitor":false,"EnableChapterImageExtraction":false,
            "EnableTrickplayImageExtraction":false,"TypeOptions":[]}});
        post(
            &self.router,
            &self.token,
            &format!(
                "/Library/VirtualFolders?name={name}&collectionType={collection_type}&refreshLibrary=false"
            ),
            options,
        )
        .await;
    }

    async fn views(&self, query: &str) -> Value {
        get(
            &self.router,
            &self.token,
            &format!("/UserViews?userId={}{query}", self.user),
        )
        .await
    }
}

/// `UserViewQuery.IncludeExternalContent` defaults to true; `false` leaves out
/// the channels and the Live TV view (`UserViewManager.GetUserViews`).
#[tokio::test]
async fn include_external_content_false_leaves_out_the_live_tv_view() {
    let f = setup().await;
    f.library("Films", "movies").await;
    f.db.upsert_live_tv_tuner_host("t1", "http://tuner", "m3u", "{}")
        .await
        .unwrap();
    let live_tv = |page: &Value| {
        views(page)
            .into_iter()
            .filter(|(_, _, collection_type)| collection_type == "livetv")
            .count()
    };

    assert_eq!(live_tv(&f.views("").await), 1, "included by default");
    assert_eq!(live_tv(&f.views("&includeExternalContent=true").await), 1);
    let without = f.views("&includeExternalContent=false").await;
    assert_eq!(live_tv(&without), 0, "{without}");
    assert_eq!(
        views(&without),
        [(
            "Films".to_owned(),
            "CollectionFolder".to_owned(),
            "movies".to_owned()
        )]
    );
}

/// `UserViewQuery.PresetViews`: a library whose type is named is listed as
/// its shadow view (`LibraryManager.GetShadowView`): a `UserView` of the
/// library's name and type, standing for the library, under its own id. The
/// other libraries, and a request without the parameter, list the library.
#[tokio::test]
async fn preset_views_list_a_named_library_as_its_shadow_view() {
    let f = setup().await;
    f.library("Films", "movies").await;
    f.library("Series", "tvshows").await;
    let plain = f.views("").await;
    let library = view(&plain, "Films")["Id"].as_str().unwrap().to_owned();

    let preset = f.views("&presetViews=Movies,bogus").await;
    assert_eq!(
        views(&preset),
        [
            (
                "Films".to_owned(),
                "UserView".to_owned(),
                "movies".to_owned()
            ),
            (
                "Series".to_owned(),
                "CollectionFolder".to_owned(),
                "tvshows".to_owned()
            ),
        ],
        "{preset}"
    );
    let shadow = view(&preset, "Films");
    let shadow_id = shadow["Id"].as_str().unwrap().to_owned();
    assert_ne!(shadow_id, library, "the shadow view has an id of its own");
    // `dto.ParentId = item.DisplayParentId` reads `BaseItem`'s, which is the
    // (empty) `ParentId`: `UserView.DisplayParentId` only hides it.
    assert!(shadow["ParentId"].is_null(), "{shadow}");

    // The same view on the next request; the plain list keeps the library.
    let again = f.views("&presetViews=movies").await;
    assert_eq!(
        view(&again, "Films")["Id"].as_str(),
        Some(shadow_id.as_str())
    );
    assert_eq!(views(&f.views("").await), views(&plain));

    // The shadow view is browsable.
    get(
        &f.router,
        &f.token,
        &format!("/Items?userId={}&parentId={shadow_id}", f.user),
    )
    .await;
}

/// The grouping arm (`GetUserView(parents, …)`): a grouped library that is
/// the only one of its type is listed as itself, or as its shadow view when
/// the request names its type.
#[tokio::test]
async fn preset_views_apply_to_a_lone_grouped_library() {
    let f = setup().await;
    f.library("Films", "movies").await;
    let plain = f.views("").await;
    let library = view(&plain, "Films")["Id"].as_str().unwrap().to_owned();
    let user = get(&f.router, &f.token, &format!("/Users/{}", f.user)).await;
    let mut configuration = user["Configuration"].clone();
    configuration["GroupedFolders"] = json!([library]);
    post(
        &f.router,
        &f.token,
        &format!("/Users/Configuration?userId={}", f.user),
        configuration,
    )
    .await;

    assert_eq!(
        views(&f.views("").await),
        [(
            "Films".to_owned(),
            "CollectionFolder".to_owned(),
            "movies".to_owned()
        )]
    );
    assert_eq!(
        views(&f.views("&presetViews=movies").await),
        [(
            "Films".to_owned(),
            "UserView".to_owned(),
            "movies".to_owned()
        )]
    );
}
