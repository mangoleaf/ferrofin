//! An install adopted from a Jellyfin that ran the community SSO-Auth plugin
//! (B14, D9), end to end through the composition root and the real router:
//! Single Sign-On starts off, nothing of the community plugin is imported
//! (neither its `SSO-Auth.xml` nor its links), its accounts keep the provider
//! id they had, `Jellyfin.Plugin.SSO_Auth.Api.SSOController`, which nothing
//! registers, so a password never signs in to them, and local accounts sign
//! in as before.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_api::state::AppState;
use ferrofin_core::external_identity_repository::FerrofinExternalIdentityStore;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_extensions::sso::accounts::PROVIDER_ID;
use ferrofin_extensions::sso::{EXTENSION_ID, SsoConfig};
use ferrofin_model::users::UserPolicy;
use ferrofin_server::config::Config;
use ferrofin_server::state::build_app_state;
use ferrofin_traits::external_identities::ExternalIdentityStore;
use ferrofin_traits::library::UserManager;
use tower::ServiceExt as _;
use uuid::Uuid;

/// The community plugin's authentication provider id, as its accounts store
/// it.
const COMMUNITY_PROVIDER_ID: &str = "Jellyfin.Plugin.SSO_Auth.Api.SSOController";

/// The community plugin's id (v4.0.0.4).
const COMMUNITY_PLUGIN_ID: &str = "505ce9d1-d916-42fa-86ca-673ef241d7df";

/// The client secret the community plugin's settings hold.
const COMMUNITY_SECRET: &str = "community-oidc-client-secret";

const PASSWORD: &str = "a-local-password";

/// A minimal `SSO-Auth.xml` as the community plugin v4.0.0.4 writes it: one
/// OIDC provider, with its secret, and its username-keyed link of `user_id`.
fn community_xml(user_id: Uuid) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<PluginConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <SamlConfigs />
  <OidConfigs>
    <item>
      <key><string>authentik</string></key>
      <value>
        <PluginConfiguration>
          <OidEndpoint>https://idp.example.org/application/o/jellyfin/</OidEndpoint>
          <OidClientId>jellyfin</OidClientId>
          <OidSecret>{COMMUNITY_SECRET}</OidSecret>
          <Enabled>true</Enabled>
          <EnableAuthorization>true</EnableAuthorization>
          <EnableAllFolders>true</EnableAllFolders>
          <AdminRoles><string>jellyfin-admins</string></AdminRoles>
          <CanonicalLinks>
            <item>
              <key><string>carol</string></key>
              <value><guid>{user_id}</guid></value>
            </item>
          </CanonicalLinks>
        </PluginConfiguration>
      </value>
    </item>
  </OidConfigs>
</PluginConfiguration>"#
    )
}

/// An adopted install: `carol`, created by the community plugin with the
/// password hash it sets, `dave`, created by it on Jellyfin 10.11, where its
/// accounts came out with none, and `erin`, a local account; the plugin's
/// folder and its `SSO-Auth.xml` where Jellyfin keeps them.
struct Adopted {
    _temp: tempfile::TempDir,
    config: Config,
    db: ferrofin_db::Database,
    xml: std::path::PathBuf,
    /// carol, dave and erin, as the database held them before the boot:
    /// each row with its policy (administrator flag, libraries, provider).
    before: Vec<(UserEntity, Option<UserPolicy>)>,
}

/// The account `id`: its row and its policy.
async fn account(users: &dyn UserManager, id: Uuid) -> (UserEntity, Option<UserPolicy>) {
    let user = users
        .get_user_by_id(id)
        .await
        .expect("lookup")
        .expect("the account");
    let policy = users.get_user_dto(&user, None).await.expect("dto").policy;
    (user, policy)
}

impl Adopted {
    async fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        for d in ["config", "data", "cache"] {
            std::fs::create_dir_all(temp.path().join(d)).expect("dir");
        }
        let config = Config::test_stub(temp.path());
        let db = ferrofin_db::Database::connect(&config.database_url())
            .await
            .expect("open db");
        db.run_migrations().await.expect("migrations");

        let users = ferrofin_core::FerrofinUserManager::new(db.clone());
        let mut before = Vec::new();
        for (name, community, password) in [
            ("carol", true, Some(PASSWORD)),
            ("dave", true, None),
            ("erin", false, Some(PASSWORD)),
        ] {
            let user = users.create_user(name).await.expect("create");
            let id = Uuid::parse_str(&user.id).expect("id");
            if let Some(password) = password {
                users.change_password(id, password).await.expect("password");
            }
            if community {
                let mut policy = users
                    .get_user_dto(&user, None)
                    .await
                    .expect("dto")
                    .policy
                    .expect("policy");
                COMMUNITY_PROVIDER_ID.clone_into(&mut policy.authentication_provider_id);
                users.update_policy(id, &policy).await.expect("policy");
            }
            before.push(account(&users, id).await);
        }
        assert_eq!(
            before[0].0.authentication_provider_id, COMMUNITY_PROVIDER_ID,
            "the fixture"
        );
        assert!(before[0].0.password.is_some(), "the fixture: a password");
        assert!(before[1].0.password.is_none(), "the fixture: no password");

        let plugins = config.data_dir.join("plugins");
        let folder = plugins.join("SSO Authentication_4.0.0.4");
        std::fs::create_dir_all(&folder).expect("dir");
        std::fs::write(
            folder.join("meta.json"),
            format!(
                r#"{{"guid":"{COMMUNITY_PLUGIN_ID}","name":"SSO Authentication","version":"4.0.0.4","targetAbi":"10.11.0.0","status":"Active","autoUpdate":true,"assemblies":[]}}"#
            ),
        )
        .expect("write");
        let configurations = plugins.join("configurations");
        std::fs::create_dir_all(&configurations).expect("dir");
        let xml = configurations.join("SSO-Auth.xml");
        std::fs::write(
            &xml,
            community_xml(Uuid::parse_str(&before[0].0.id).expect("id")),
        )
        .expect("write");
        Self {
            _temp: temp,
            config,
            db,
            xml,
            before,
        }
    }

    /// Boots the composition root on the adopted install: its state, and the
    /// shutdown channel's receiver, held while the state serves.
    async fn boot(&self) -> (AppState, tokio::sync::oneshot::Receiver<()>) {
        let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
            encoder_app_path_display: None,
            ffmpeg: std::path::PathBuf::from("ffmpeg"),
            ffprobe: std::path::PathBuf::from("ffprobe"),
            capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
            chromaprint_muxer: false,
        };
        let (shutdown_tx, shutdown) = tokio::sync::oneshot::channel();
        let wired = build_app_state(&self.db, &self.config, &ffmpeg, None, shutdown_tx)
            .await
            .expect("wire app state");
        (wired.state, shutdown)
    }
}

/// Every file under `dir` but `except` whose bytes hold `needle`. A file
/// gone between the listing and the read (a configuration written whole
/// through a renamed temporary file) holds nothing.
fn files_holding(
    dir: &std::path::Path,
    except: &std::path::Path,
    needle: &str,
) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    // A directory gone since its parent was listed holds nothing either.
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_holding(&path, except, needle));
        } else if path != except
            && std::fs::read(&path).is_ok_and(|bytes| {
                bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes())
            })
        {
            found.push(path);
        }
    }
    found
}

/// `POST /Users/AuthenticateByName` as `name` with `password`: its status.
async fn password_sign_in(router: &axum::Router, name: &str, password: &str) -> StatusCode {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(
                    header::AUTHORIZATION,
                    format!(
                        r#"MediaBrowser Client="Jellyfin Web", Device="Firefox", DeviceId="adopted-{name}", Version="12.4.0""#
                    ),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "Username": name, "Pw": password }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response")
        .status()
}

#[tokio::test]
async fn an_install_adopted_with_the_community_plugin_starts_with_sso_off_and_nothing_imported() {
    let adopted = Adopted::new().await;
    let (state, _shutdown) = adopted.boot().await;

    // Single Sign-On is off, with its own empty configuration and no
    // private data; the community plugin is not a plugin here.
    let plugins = &state.plugins;
    let sso = plugins
        .get_plugin(EXTENSION_ID)
        .await
        .expect("get")
        .expect("registered");
    assert!(!sso.enabled, "Single Sign-On starts off");
    let stored: serde_json::Value = serde_json::from_slice(
        &plugins
            .get_plugin_configuration(EXTENSION_ID)
            .await
            .expect("config"),
    )
    .expect("json");
    assert_eq!(
        stored,
        serde_json::to_value(SsoConfig::default()).expect("json"),
        "nothing imported into the configuration"
    );
    assert_eq!(
        plugins
            .get_plugin_private_data(EXTENSION_ID)
            .await
            .expect("private data"),
        None,
        "no secret imported"
    );
    let community: Uuid = COMMUNITY_PLUGIN_ID.parse().expect("a GUID");
    assert!(
        plugins.get_plugin(community).await.expect("get").is_none(),
        "the community plugin is not loaded"
    );
    // Not in a configuration, the database or a cache: only in the
    // plugin's own file.
    let copies: Vec<_> = [
        &adopted.config.config_dir,
        &adopted.config.data_dir,
        &adopted.config.cache_dir,
    ]
    .into_iter()
    .flat_map(|dir| files_holding(dir, &adopted.xml, COMMUNITY_SECRET))
    .collect();
    assert_eq!(
        copies,
        Vec::<std::path::PathBuf>::new(),
        "the community secret is copied nowhere"
    );
    assert_eq!(
        files_holding(
            &adopted.config.data_dir,
            std::path::Path::new(""),
            COMMUNITY_SECRET
        ),
        std::slice::from_ref(&adopted.xml),
        "the scan finds the plugin's own file"
    );

    // No link: neither under Single Sign-On's provider id nor the
    // community plugin's.
    let identities = FerrofinExternalIdentityStore::new(adopted.db.clone());
    for provider_id in [PROVIDER_ID, COMMUNITY_PROVIDER_ID] {
        assert_eq!(
            identities.list(provider_id).await.expect("list"),
            Vec::new(),
            "{provider_id}"
        );
    }

    // Every account is as it was, the community plugin's provider id kept,
    // and the plugin's file is untouched.
    for before in &adopted.before {
        let id = Uuid::parse_str(&before.0.id).expect("id");
        assert_eq!(
            &account(state.users.as_ref(), id).await,
            before,
            "{} unchanged, its policy included",
            before.0.username
        );
    }
    assert_eq!(
        std::fs::read_to_string(&adopted.xml).expect("the file"),
        community_xml(Uuid::parse_str(&adopted.before[0].0.id).expect("id")),
        "SSO-Auth.xml untouched"
    );
}

#[tokio::test]
async fn the_community_plugins_accounts_fail_closed_and_local_ones_sign_in() {
    let adopted = Adopted::new().await;
    let (state, _shutdown) = adopted.boot().await;
    let (users, plugins) = (state.users.clone(), state.plugins.clone());
    let router = ferrofin_api::create_router(state);

    // Whatever the password, and the account the plugin left without one
    // does not take an empty one.
    for (name, password) in [
        ("carol", PASSWORD),
        ("carol", ""),
        ("dave", ""),
        ("dave", PASSWORD),
    ] {
        assert_eq!(
            password_sign_in(&router, name, password).await,
            StatusCode::UNAUTHORIZED,
            "{name} with {password:?}"
        );
    }
    assert_eq!(
        password_sign_in(&router, "erin", PASSWORD).await,
        StatusCode::OK,
        "the local account"
    );
    // The refusals moved the accounts to no other provider and set no
    // password.
    for before in &adopted.before[..2] {
        let id = Uuid::parse_str(&before.0.id).expect("id");
        let (after, _) = account(users.as_ref(), id).await;
        assert_eq!(after.authentication_provider_id, COMMUNITY_PROVIDER_ID);
        assert_eq!(after.password, before.0.password, "{}", after.username);
    }

    // And the plugin is off: its anonymous exchange is not there; enabled,
    // the same request is refused as an unknown code.
    let exchange = || {
        Request::builder()
            .method("POST")
            .uri("/sso/oidc/exchange")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"Code":"not-a-code"}"#))
            .expect("request")
    };
    let off = router.clone().oneshot(exchange()).await.expect("response");
    assert_eq!(off.status(), StatusCode::NOT_FOUND);
    plugins.enable_plugin(EXTENSION_ID).await.expect("enable");
    let on = router.oneshot(exchange()).await.expect("response");
    assert_eq!(on.status(), StatusCode::BAD_REQUEST);
}
