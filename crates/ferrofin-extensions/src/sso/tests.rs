//! The Single Sign-On extension and its provider settings, over the real
//! plugin manager in a temporary directory.

use ferrofin_core::FerrofinPluginManager;

use super::fake_idp::{FakeIdentityProvider, FakeResponse};
use super::*;

const SECRET: &str = "pocket-client-secret-7f3a";

/// The service over a plugin manager holding only this extension, enabled
/// unless `enabled` is false, and the manager and its directory.
pub(super) async fn service(
    enabled: bool,
    published_url: Option<&str>,
) -> (SsoService, Arc<FerrofinPluginManager>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let extensions: Vec<Arc<dyn Extension>> = vec![Arc::new(SsoExtension)];
    let plugins = Arc::new(FerrofinPluginManager::new(
        crate::registered_plugins(&extensions),
        dir.path().to_path_buf(),
    ));
    if enabled {
        plugins.enable_plugin(EXTENSION_ID).await.expect("enable");
    }
    let db = ferrofin_db::Database::connect_in_memory()
        .await
        .expect("connect");
    db.run_migrations().await.expect("migrations");
    let users = Arc::new(ferrofin_core::FerrofinUserManager::new(db.clone()));
    let sessions = sign_in::tests::sessions(
        &db,
        &users,
        &Arc::new(ferrofin_core::FerrofinEventManager::new()),
    );
    let sso = SsoService::new(
        Arc::clone(&plugins) as Arc<dyn PluginManager>,
        users,
        Arc::new(
            ferrofin_core::external_identity_repository::FerrofinExternalIdentityStore::new(
                db.clone(),
            ),
        ),
        sessions,
        Arc::new(ferrofin_core::FerrofinActivityManager::new(db)),
        published_url.map(str::to_owned),
    );
    (sso, plugins, dir)
}

/// A provider the routes accept, with `secret` as its client secret.
fn pocket(secret: Option<&str>) -> OidcProvider {
    OidcProvider {
        name: "Pocket ID".to_owned(),
        issuer: "https://id.example.org".to_owned(),
        client_id: "ferrofin".to_owned(),
        client_secret: secret.map(Secret::new),
        ..OidcProvider::default()
    }
}

/// The private data file and what it holds.
fn private_file(dir: &tempfile::TempDir) -> (std::path::PathBuf, String) {
    let path = dir
        .path()
        .join(EXTENSION_ID.to_string())
        .join("private.json");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    (path, text)
}

/// The stored configuration, as `GET /Plugins/{id}/Configuration` returns it.
async fn configuration(plugins: &FerrofinPluginManager) -> String {
    String::from_utf8(
        plugins
            .get_plugin_configuration(EXTENSION_ID)
            .await
            .expect("configuration"),
    )
    .expect("UTF-8")
}

#[test]
fn the_plugin_is_single_sign_on_under_a_ferrofin_guid_and_off_by_default() {
    let descriptor = SsoExtension.descriptor();
    assert_eq!(descriptor.id, EXTENSION_ID);
    assert_eq!(SsoExtension.id(), EXTENSION_ID);
    assert_eq!(
        EXTENSION_ID,
        Uuid::parse_str("67a29a1d-e02b-4aaa-bba0-4b2275e765d0").expect("guid")
    );
    assert_eq!(descriptor.name, "Single Sign-On");
    assert!(!descriptor.enabled);
    assert!(!descriptor.can_uninstall);
    assert_eq!(descriptor.configuration_file_name, None);
    assert_eq!(
        descriptor.version,
        crate::ferrofin_settings::plugin_version(ferrofin_health::build_version())
    );
    let config: serde_json::Value =
        serde_json::from_slice(&SsoExtension.default_config()).expect("JSON");
    assert_eq!(
        config,
        serde_json::json!({"PublicUrl": "", "Providers": [], "ShowLoginButton": true})
    );
    let cx_free = SsoExtension.config_pages();
    assert_eq!(cx_free.len(), 1);
    assert_eq!(cx_free[0].name, SETTINGS_PAGE_NAME);
    assert!(!cx_free[0].enable_in_main_menu);
}

/// The settings page's HTML, as `GET /web/ConfigurationPage` serves it.
fn settings_html() -> String {
    let mut pages = SsoExtension.config_pages();
    String::from_utf8(pages.remove(0).bytes).expect("the page is UTF-8")
}

#[test]
fn the_settings_page_reads_the_admin_routes_and_holds_no_setting() {
    let html = settings_html();
    // The plugin-page shape jellyfin-web loads: it keeps only the
    // `data-role="page"` element and runs the scripts inside it.
    assert!(html.contains(r#"<div data-role="page""#), "{html}");
    assert!(html.contains("pluginConfigurationPage"), "{html}");
    for call in [
        "ApiClient.getUrl('sso/admin/oidc/providers')",
        "'sso/admin/oidc/providers/' + encodeURIComponent(",
        "type: 'PUT'",
        "type: 'DELETE'",
        "ApiClient.getPluginConfiguration(PLUGIN_ID)",
        "ApiClient.updatePluginConfiguration(PLUGIN_ID, config)",
    ] {
        assert!(html.contains(call), "{call}");
    }
    assert!(
        html.contains(&format!("var PLUGIN_ID = '{EXTENSION_ID}';")),
        "{html}"
    );
    // A stored secret is never shown: the field starts empty, which keeps it.
    assert!(html.contains("HasClientSecret"), "{html}");
    assert!(html.contains("autocomplete=\"new-password\""), "{html}");
    // The http-issuer switch carries its warning, and the callback URL is
    // shown, or the setting to fill in.
    assert!(html.contains("ssoHttpWarning"), "{html}");
    assert!(html.contains("CallbackUrl"), "{html}");
    assert!(html.contains("Published server URL"), "{html}");
    assert!(html.contains("PublicUrlError"), "{html}");
    assert!(html.contains("already exists: edit it instead"), "{html}");
    // A save's discovery test is shown in the provider's row.
    for shown in [
        "saved.Discovery",
        "discovery.Error",
        "discovery.AuthorizationEndpoint",
        "discovery.TokenEndpoint",
        "discovery.UserinfoEndpoint",
        "discovery.JwksUri",
        "discovery.SigningKeys",
        "discovery.Warnings",
    ] {
        assert!(html.contains(shown), "{shown}");
    }
    // jellyfin-web renders a confirm message as HTML, its title as text: no
    // response string goes into the message.
    assert!(
        html.contains("Dashboard.confirm(DELETE_MESSAGE, 'Delete '"),
        "{html}"
    );
    assert!(
        html.contains(
            "Dashboard.confirm(NO_ACCESS_MESSAGE, 'Save with no library or Live TV role?'"
        ),
        "{html}"
    );
    for (at, call) in html.match_indices("Dashboard.confirm(") {
        let rest = &html[at + call.len()..];
        let message = &rest[..rest.find(',').unwrap_or(rest.len())];
        assert!(
            [
                "DELETE_MESSAGE",
                "NO_ACCESS_MESSAGE",
                "last ? LAST_LINK_MESSAGE : REMOVE_MESSAGE"
            ]
            .contains(&message),
            "a confirm message that is not a constant: {message}"
        );
    }
    assert!(!html.contains("Dashboard.alert("), "{html}");
    // Every string from a response is set as text, never parsed as HTML.
    for sink in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "createContextualFragment",
        ".html(",
    ] {
        assert!(!html.contains(sink), "the page uses `{sink}`");
    }
}

#[tokio::test]
async fn a_disabled_plugin_answers_not_found_and_writes_nothing() {
    let (sso, plugins, dir) = service(false, None).await;
    assert!(matches!(
        sso.ensure_enabled().await,
        Err(ServiceError::NotFound(_))
    ));
    assert!(matches!(
        sso.oidc_providers().await,
        Err(ServiceError::NotFound(_))
    ));
    assert!(matches!(
        sso.save_oidc_provider("pocket", pocket(Some(SECRET))).await,
        Err(ServiceError::NotFound(_))
    ));
    assert!(matches!(
        sso.delete_oidc_provider("pocket").await,
        Err(ServiceError::NotFound(_))
    ));
    assert_eq!(private_file(&dir).1, "");
    assert!(!configuration(&plugins).await.contains("pocket"));
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one provider's life: add, list, edit, delete
async fn a_provider_is_added_listed_edited_and_deleted_and_its_secret_never_read_back() {
    let (sso, plugins, dir) = service(true, Some("https://media.example.org/jf/")).await;
    let logs = crate::tests::CapturedLogs::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish(),
    );

    let saved = sso
        .save_oidc_provider("pocket", pocket(Some(SECRET)))
        .await
        .expect("add");
    assert_eq!(saved.provider.key, "pocket");
    assert!(saved.has_client_secret);
    assert_eq!(saved.validation_error, None);
    assert_eq!(
        saved.callback_url.as_deref(),
        Some("https://media.example.org/jf/sso/oidc/pocket/callback"),
        "the published URL, base path kept, when the plugin sets none"
    );
    assert!(!format!("{saved:?}").contains(SECRET));

    // Listed, with no secret in the answer or the configuration.
    let listed = sso.oidc_providers().await.expect("list");
    assert_eq!(listed.providers, vec![saved.clone()]);
    let answer = serde_json::to_string(&listed).expect("JSON");
    assert!(!answer.contains(SECRET), "{answer}");
    assert!(!answer.contains("\"ClientSecret\""), "{answer}");
    let stored = configuration(&plugins).await;
    assert!(stored.contains("Pocket ID"), "{stored}");
    assert!(!stored.contains(SECRET), "{stored}");
    assert!(!stored.contains("\"ClientSecret\""), "{stored}");
    // The private file holds it, readable by the server's user only.
    let (path, private) = private_file(&dir);
    assert!(private.contains(SECRET), "{private}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).expect("mode").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    // An edit without a secret, or with an empty one, keeps it.
    for kept in [None, Some("")] {
        let mut edit = pocket(kept);
        edit.name = "Pocket".to_owned();
        let edited = sso.save_oidc_provider("pocket", edit).await.expect("edit");
        assert!(edited.has_client_secret);
        assert_eq!(edited.provider.name, "Pocket");
        assert!(private_file(&dir).1.contains(SECRET));
    }
    // A new one replaces it.
    sso.save_oidc_provider("pocket", pocket(Some("rotated-secret-99")))
        .await
        .expect("rotate");
    let private = private_file(&dir).1;
    assert!(private.contains("rotated-secret-99") && !private.contains(SECRET));

    // A second provider goes after the first; the list keeps their order.
    sso.save_oidc_provider(
        "keycloak",
        OidcProvider {
            name: "Keycloak".to_owned(),
            issuer: "https://sso.example.org/realms/home".to_owned(),
            client_id: "media".to_owned(),
            client_secret: Some(Secret::new("keycloak-secret")),
            ..OidcProvider::default()
        },
    )
    .await
    .expect("second");
    let keys = |list: &OidcProviderList| -> Vec<String> {
        list.providers
            .iter()
            .map(|info| info.provider.key.clone())
            .collect()
    };
    let listed = sso.oidc_providers().await.expect("list");
    assert_eq!(keys(&listed), ["pocket", "keycloak"]);
    sso.save_oidc_provider("pocket", pocket(None))
        .await
        .expect("edit in place");
    assert_eq!(
        keys(&sso.oidc_providers().await.expect("list")),
        ["pocket", "keycloak"]
    );

    // A delete removes the provider and its secret, and only those.
    sso.delete_oidc_provider("pocket").await.expect("delete");
    let listed = sso.oidc_providers().await.expect("list");
    assert_eq!(keys(&listed), ["keycloak"]);
    let private = private_file(&dir).1;
    assert!(!private.contains("rotated-secret-99"), "{private}");
    assert!(private.contains("keycloak-secret"), "{private}");
    assert!(matches!(
        sso.delete_oidc_provider("pocket").await,
        Err(ServiceError::NotFound(_))
    ));

    // No log line holds a secret.
    let logged = logs.text();
    assert!(logged.contains("OIDC provider saved"), "{logged}");
    for secret in [SECRET, "rotated-secret-99", "keycloak-secret"] {
        assert!(!logged.contains(secret), "{logged}");
    }
}

#[test]
fn the_private_data_debug_output_holds_no_secret() {
    let mut secrets = SsoSecrets::default();
    secrets
        .oidc_client_secrets
        .insert("pocket".to_owned(), Secret::new(SECRET));
    let debug = format!("{secrets:?}");
    assert!(
        debug.contains("pocket") && !debug.contains(SECRET),
        "{debug}"
    );
}

#[rstest::rstest]
#[case::uppercase_key("Pocket", pocket(Some(SECRET)), "the provider key must be")]
#[case::underscore_key("pocket_id", pocket(Some(SECRET)), "the provider key must be")]
#[case::key_too_long(
    "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk",
    pocket(Some(SECRET)),
    "the provider key must be 1 to 32"
)]
#[case::body_names_another_key("pocket", OidcProvider { key: "other".to_owned(), ..pocket(Some(SECRET)) }, "a provider's key cannot be changed")]
#[case::empty_name("pocket", OidcProvider { name: "  ".to_owned(), ..pocket(Some(SECRET)) }, "the provider name is required")]
#[case::empty_client_id("pocket", OidcProvider { client_id: String::new(), ..pocket(Some(SECRET)) }, "the client id is required")]
#[case::http_issuer("pocket", OidcProvider { issuer: "http://id.lan".to_owned(), ..pocket(Some(SECRET)) }, "the issuer must use https")]
#[case::not_a_url("pocket", OidcProvider { issuer: "id.example.org".to_owned(), ..pocket(Some(SECRET)) }, "the issuer is not a URL")]
#[case::other_scheme("pocket", OidcProvider { issuer: "ftp://id.example.org".to_owned(), allow_http_issuer: true, ..pocket(Some(SECRET)) }, "the issuer must be an https URL")]
#[case::issuer_query("pocket", OidcProvider { issuer: "https://id.example.org/?realm=a".to_owned(), ..pocket(Some(SECRET)) }, "no query or fragment")]
#[case::issuer_credentials("pocket", OidcProvider { issuer: "https://u:p@id.example.org".to_owned(), ..pocket(Some(SECRET)) }, "no user name or password")]
#[case::no_openid_scope("pocket", OidcProvider { scopes: "profile email".to_owned(), ..pocket(Some(SECRET)) }, "the scopes must include openid")]
#[case::empty_username_claim("pocket", OidcProvider { username_claim: String::new(), ..pocket(Some(SECRET)) }, "the username claim is required")]
#[case::issuer_upper_case_scheme("pocket", OidcProvider { issuer: "HTTPS://id.example.org".to_owned(), ..pocket(Some(SECRET)) }, "the issuer must be written as")]
#[case::issuer_without_slashes("pocket", OidcProvider { issuer: "https:id.example.org".to_owned(), ..pocket(Some(SECRET)) }, "the issuer must be written as")]
#[case::issuer_with_a_backslash("pocket", OidcProvider { issuer: "https://id.example.org\\realm".to_owned(), ..pocket(Some(SECRET)) }, "the issuer must be written as")]
#[case::issuer_with_dot_segments("pocket", OidcProvider { issuer: "https://id.example.org/a/../b".to_owned(), ..pocket(Some(SECRET)) }, "such as https://id.example.org/b")]
#[case::issuer_with_the_default_port("pocket", OidcProvider { issuer: "https://id.example.org:443/x".to_owned(), ..pocket(Some(SECRET)) }, "the issuer must be written as")]
#[case::issuer_path_keeps_its_slash("pocket", OidcProvider { issuer: "HTTPS://auth.example.org/application/o/ferrofin/".to_owned(), ..pocket(Some(SECRET)) }, "such as https://auth.example.org/application/o/ferrofin/")]
#[case::secret_with_a_trailing_space("pocket", pocket(Some("abc ")), "start or end with a space")]
#[case::secret_with_a_newline(
    "pocket",
    pocket(Some("abc\n")),
    "the client secret must not hold control characters"
)]
#[case::blank_secret_on_a_new_provider(
    "pocket",
    pocket(Some("   ")),
    "the provider needs a client secret"
)]
#[case::issuer_with_a_tab("pocket", OidcProvider { issuer: "https://id.exa\tmple.org".to_owned(), ..pocket(Some(SECRET)) }, "control characters")]
#[case::name_with_a_newline("pocket", OidcProvider { name: "Pocket\nID".to_owned(), ..pocket(Some(SECRET)) }, "control characters")]
#[case::new_provider_without_secret("pocket", pocket(None), "the provider needs a client secret")]
#[case::new_provider_with_empty_secret(
    "pocket",
    pocket(Some("")),
    "the provider needs a client secret"
)]
#[tokio::test]
async fn a_refused_provider_is_a_clear_invalid_input_and_nothing_is_saved(
    #[case] key: &str,
    #[case] provider: OidcProvider,
    #[case] message: &str,
) {
    let (sso, plugins, dir) = service(true, None).await;
    match sso.save_oidc_provider(key, provider).await {
        Err(ServiceError::InvalidInput(why)) => assert!(why.contains(message), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        sso.oidc_providers().await.expect("list").providers,
        Vec::new()
    );
    assert!(!configuration(&plugins).await.contains("Pocket"));
    assert_eq!(private_file(&dir).1, "");
}

#[tokio::test]
async fn an_http_issuer_is_accepted_with_its_switch_on() {
    let (sso, _plugins, _dir) = service(true, None).await;
    let saved = sso
        .save_oidc_provider(
            "lan",
            OidcProvider {
                issuer: "http://id.lan:9000/application/o/ferrofin/".to_owned(),
                allow_http_issuer: true,
                ..pocket(Some(SECRET))
            },
        )
        .await
        .expect("an http issuer with the switch on");
    assert!(saved.provider.allow_http_issuer);
    assert_eq!(saved.callback_url, None, "no public URL is known");
}

/// What the generic configuration route or a hand edit can store: each
/// provider the routes would refuse is listed with why, and kept as it is
/// when another provider is saved.
#[tokio::test]
async fn a_stored_provider_the_routes_would_refuse_is_listed_as_refused_and_kept() {
    let (sso, plugins, _dir) = service(true, None).await;
    let stored = serde_json::json!({
        "PublicUrl": "https://media.example.org",
        "Providers": [
            {"Key": "lan", "Name": "LAN", "Issuer": "http://id.lan", "ClientId": "a"},
            {"Key": "twice", "Name": "First", "Issuer": "https://a.example", "ClientId": "a"},
            {"Key": "twice", "Name": "Second", "Issuer": "https://b.example", "ClientId": "b"},
            {"Key": "broken", "Name": null, "Future": 1},
            "not a provider",
            {"Key": "Bad/Key", "Name": "Bad", "Issuer": "https://c.example", "ClientId": "c"}
        ]
    });
    plugins
        .set_plugin_configuration(EXTENSION_ID, stored.to_string().into_bytes())
        .await
        .expect("the generic route stores it");
    let listed = sso.oidc_providers().await.expect("list");
    let refused: Vec<(&str, Option<&str>, Option<&str>)> = listed
        .providers
        .iter()
        .map(|info: &OidcProviderInfo| {
            (
                info.provider.key.as_str(),
                info.validation_error.as_deref(),
                info.callback_url.as_deref(),
            )
        })
        .collect();
    assert_eq!(refused.len(), 6, "{refused:?}");
    assert!(refused[0].1.is_some_and(|why| why.contains("https")));
    assert_eq!(
        refused[0].2,
        Some("https://media.example.org/sso/oidc/lan/callback")
    );
    assert_eq!(refused[1].1, None);
    assert_eq!(refused[2].1, Some("an earlier provider has the same key"));
    assert!(
        refused[3]
            .1
            .is_some_and(|why| why.starts_with("the stored provider does not read"))
    );
    assert!(!listed.providers[3].provider.enabled);
    assert_eq!(refused[4].0, "");
    assert!(refused[4].1.is_some());
    assert!(refused[5].1.is_some_and(|why| why.contains("provider key")));
    assert_eq!(refused[5].2, None, "a refused key builds no URL");

    // Saving another provider keeps every one of them as stored.
    sso.save_oidc_provider("pocket", pocket(Some(SECRET)))
        .await
        .expect("save another");
    let config: serde_json::Value =
        serde_json::from_str(&configuration(&plugins).await).expect("JSON");
    let providers = config["Providers"].as_array().expect("providers");
    assert_eq!(providers.len(), 7);
    assert_eq!(
        providers[..6],
        stored["Providers"].as_array().expect("stored")[..]
    );
    assert_eq!(providers[6]["Key"], "pocket");
    // Saving one with a repeated key leaves one, in the first one's place.
    sso.save_oidc_provider("twice", pocket(Some(SECRET)))
        .await
        .expect("save the repeated key");
    let keys: Vec<String> = sso
        .oidc_providers()
        .await
        .expect("list")
        .providers
        .into_iter()
        .map(|info| info.provider.key)
        .collect();
    assert_eq!(keys, ["lan", "twice", "broken", "", "Bad/Key", "pocket"]);
}

#[rstest::rstest]
#[case::plugin_setting_wins(
    "https://sso.example.org/media/",
    Some("https://published.example"),
    Some("https://sso.example.org/media/sso/oidc/pocket/callback"),
    false
)]
#[case::published_url(
    "",
    Some("https://published.example/"),
    Some("https://published.example/sso/oidc/pocket/callback"),
    false
)]
#[case::unusable_setting_is_refused_not_passed_over(
    "media.example.org",
    Some("http://10.0.0.5:8096"),
    None,
    true
)]
#[case::setting_with_a_query("https://media.example.org/?a=b", None, None, true)]
#[case::neither("", None, None, false)]
#[case::unusable_published_url("", Some("not a url"), None, false)]
#[tokio::test]
async fn the_callback_url_is_built_on_the_public_url_never_the_request(
    #[case] public_url: &str,
    #[case] published_url: Option<&str>,
    #[case] callback: Option<&str>,
    #[case] refused: bool,
) {
    let (sso, plugins, _dir) = service(true, published_url).await;
    plugins
        .set_plugin_configuration(
            EXTENSION_ID,
            serde_json::json!({"PublicUrl": public_url})
                .to_string()
                .into_bytes(),
        )
        .await
        .expect("public URL");
    let saved = sso
        .save_oidc_provider("pocket", pocket(Some(SECRET)))
        .await
        .expect("save");
    assert_eq!(saved.callback_url.as_deref(), callback);
    let listed = sso.oidc_providers().await.expect("list");
    assert_eq!(listed.public_url, public_url);
    assert_eq!(listed.providers[0].callback_url.as_deref(), callback);
    assert_eq!(
        listed.public_url_error.is_some(),
        refused,
        "{:?}",
        listed.public_url_error
    );
}

#[tokio::test]
async fn two_saves_at_once_both_land() {
    let (sso, _plugins, dir) = service(true, None).await;
    let first = OidcProvider {
        client_secret: Some(Secret::new("first-secret")),
        ..pocket(None)
    };
    let second = OidcProvider {
        client_secret: Some(Secret::new("second-secret")),
        ..pocket(None)
    };
    let (a, b) = tokio::join!(
        sso.save_oidc_provider("first", first),
        sso.save_oidc_provider("second", second)
    );
    a.expect("first");
    b.expect("second");
    let listed = sso.oidc_providers().await.expect("list");
    assert_eq!(listed.providers.len(), 2);
    assert!(listed.providers.iter().all(|info| info.has_client_secret));
    let private = private_file(&dir).1;
    assert!(private.contains("first-secret") && private.contains("second-secret"));
}

/// A save trims each text setting, so the issuer is stored exactly as a
/// token's `iss` must match it; a stored one that is not trimmed is refused.
#[tokio::test]
async fn text_settings_are_trimmed_on_save_and_must_be_trimmed_when_stored() {
    let (sso, plugins, _dir) = service(true, None).await;
    let saved = sso
        .save_oidc_provider(
            "pocket",
            OidcProvider {
                name: "  Pocket ID ".to_owned(),
                issuer: " https://id.example.org ".to_owned(),
                client_id: " ferrofin ".to_owned(),
                scopes: " openid profile ".to_owned(),
                username_claim: " preferred_username ".to_owned(),
                ..pocket(Some(SECRET))
            },
        )
        .await
        .expect("save");
    assert_eq!(saved.provider.name, "Pocket ID");
    assert_eq!(saved.provider.issuer, "https://id.example.org");
    assert_eq!(saved.provider.client_id, "ferrofin");
    assert_eq!(saved.provider.scopes, "openid profile");
    assert_eq!(saved.provider.username_claim, "preferred_username");
    assert!(
        configuration(&plugins)
            .await
            .contains(r#""Issuer":"https://id.example.org""#)
    );

    plugins
        .set_plugin_configuration(
            EXTENSION_ID,
            serde_json::json!({"Providers": [{
                "Key": "pocket", "Name": "Pocket", "Issuer": "https://id.example.org ",
                "ClientId": "a"
            }]})
            .to_string()
            .into_bytes(),
        )
        .await
        .expect("the generic route stores it");
    let listed = sso.oidc_providers().await.expect("list");
    assert!(
        listed.providers[0]
            .validation_error
            .as_deref()
            .is_some_and(|why| why.contains("the issuer must not")),
        "{listed:?}"
    );
}

/// A secret is only kept beside its provider: one the configuration route
/// left behind is dropped at the next save, and a provider created again
/// under that key needs a secret of its own.
#[tokio::test]
async fn a_secret_left_without_its_provider_is_not_inherited() {
    let (sso, plugins, dir) = service(true, None).await;
    sso.save_oidc_provider("pocket", pocket(Some(SECRET)))
        .await
        .expect("save");
    // The generic route removes the provider, not its secret.
    plugins
        .set_plugin_configuration(EXTENSION_ID, br#"{"Providers": []}"#.to_vec())
        .await
        .expect("the generic route");
    match sso.save_oidc_provider("pocket", pocket(None)).await {
        Err(ServiceError::InvalidInput(why)) => assert!(why.contains("client secret"), "{why}"),
        other => panic!("{other:?}"),
    }
    sso.save_oidc_provider("other", pocket(Some("other-secret")))
        .await
        .expect("save another");
    let private = private_file(&dir).1;
    assert!(!private.contains(SECRET), "{private}");
    assert!(private.contains("other-secret"), "{private}");
}

/// A client secret the configuration route stored in a provider document is
/// never read, and is gone from the configuration at the next save.
#[tokio::test]
async fn a_secret_stored_in_the_configuration_is_ignored_and_scrubbed() {
    let (sso, plugins, _dir) = service(true, None).await;
    plugins
        .set_plugin_configuration(
            EXTENSION_ID,
            serde_json::json!({"Providers": [{
                "Key": "lan", "Name": "LAN", "Issuer": "https://id.lan", "ClientId": "a",
                "clientSecret": "pasted-secret", "ClientSecret": "pasted-secret"
            }]})
            .to_string()
            .into_bytes(),
        )
        .await
        .expect("the generic route stores it");
    let listed = sso.oidc_providers().await.expect("list");
    assert!(!listed.providers[0].has_client_secret);
    assert!(!format!("{listed:?}").contains("pasted-secret"));
    sso.save_oidc_provider("pocket", pocket(Some(SECRET)))
        .await
        .expect("save");
    let stored = configuration(&plugins).await;
    assert!(!stored.contains("pasted-secret"), "{stored}");
    assert!(stored.contains(r#""Key":"lan""#), "{stored}");
}

/// Private data that does not read fails the routes rather than being
/// overwritten, which would lose every other provider's secret, and the
/// error quotes none of it.
#[tokio::test]
async fn private_data_that_does_not_read_is_an_error_and_left_alone() {
    let (sso, plugins, dir) = service(true, None).await;
    plugins
        .set_plugin_private_data(
            EXTENSION_ID,
            br#"{"OidcClientSecrets": {"a": 12345}}"#.to_vec(),
        )
        .await
        .expect("write");
    for result in [
        sso.oidc_providers().await.map(|_| ()),
        sso.save_oidc_provider("pocket", pocket(Some(SECRET)))
            .await
            .map(|_| ()),
    ] {
        match result {
            Err(ServiceError::Backend(why)) => {
                assert!(why.contains("private.json"), "{why}");
                assert!(!why.contains("12345"), "{why}");
            }
            other => panic!("{other:?}"),
        }
    }
    // A delete fails before it writes anything.
    plugins
        .set_plugin_configuration(
            EXTENSION_ID,
            serde_json::json!({"Providers": [{"Key": "pocket"}]})
                .to_string()
                .into_bytes(),
        )
        .await
        .expect("a stored provider");
    assert!(matches!(
        sso.delete_oidc_provider("pocket").await,
        Err(ServiceError::Backend(_))
    ));
    assert!(configuration(&plugins).await.contains("pocket"));
    assert_eq!(
        private_file(&dir).1,
        r#"{"OidcClientSecrets": {"a": 12345}}"#
    );
}

/// A provider for the fake identity provider's issuer, with its http switch
/// on, and `secret` as its client secret.
fn fake_provider(fake: &FakeIdentityProvider, secret: Option<&str>) -> OidcProvider {
    OidcProvider {
        issuer: fake.issuer(),
        allow_http_issuer: true,
        ..pocket(secret)
    }
}

/// How many times the fake's discovery document was asked for.
fn discoveries(fake: &FakeIdentityProvider) -> usize {
    let path = fake.path("/.well-known/openid-configuration");
    fake.requests().iter().filter(|r| r.path == path).count()
}

#[tokio::test]
async fn a_saved_provider_is_tested_and_its_metadata_kept_until_it_changes() {
    let fake = FakeIdentityProvider::start().await;
    let (sso, _plugins, _dir) = service(true, None).await;
    let saved = sso
        .save_oidc_provider("fake", fake_provider(&fake, Some(SECRET)))
        .await
        .expect("saved");
    assert!(sso.metadata.get(&saved.provider).is_none());
    let found = sso
        .test_oidc_provider(&saved.provider)
        .await
        .expect("tested");
    let issuer = fake.issuer();
    assert_eq!(found.error, None, "{found:?}");
    assert_eq!(
        found.authorization_endpoint,
        Some(format!("{issuer}/protocol/openid-connect/auth"))
    );
    assert_eq!(found.signing_keys, Some(1));
    assert!(found.warnings.is_empty(), "{found:?}");
    assert!(
        sso.metadata.get(&saved.provider).is_some(),
        "kept for sign-in"
    );

    // Saved again, its entry is dropped, and a test fetches it fresh.
    let saved = sso
        .save_oidc_provider("fake", fake_provider(&fake, None))
        .await
        .expect("saved again");
    assert!(sso.metadata.get(&saved.provider).is_none());
    sso.test_oidc_provider(&saved.provider)
        .await
        .expect("tested again");
    assert_eq!(discoveries(&fake), 2);
    assert!(sso.metadata.get(&saved.provider).is_some());

    // Deleted, its entry goes too.
    sso.delete_oidc_provider("fake").await.expect("deleted");
    assert!(sso.metadata.get(&saved.provider).is_none());
}

#[tokio::test]
async fn a_failed_test_is_a_result_that_says_why() {
    let fake = FakeIdentityProvider::start().await;
    let (sso, _plugins, _dir) = service(true, None).await;
    let mut document = fake.discovery_document();
    document["issuer"] = serde_json::json!(fake.url("/realms/other"));
    fake.set(
        &fake.path("/.well-known/openid-configuration"),
        FakeResponse::json(&document),
    );
    let saved = sso
        .save_oidc_provider("fake", fake_provider(&fake, Some(SECRET)))
        .await
        .expect("saved");
    let found = sso
        .test_oidc_provider(&saved.provider)
        .await
        .expect("a result");
    assert!(
        found
            .error
            .as_deref()
            .is_some_and(|why| why.contains("another issuer")),
        "{found:?}"
    );
    assert_eq!(found.authorization_endpoint, None);
    assert!(sso.metadata.get(&saved.provider).is_none());
    // Still saved.
    assert_eq!(sso.oidc_providers().await.expect("list").providers.len(), 1);
}

#[tokio::test]
async fn a_provider_the_routes_would_refuse_is_not_fetched() {
    let fake = FakeIdentityProvider::start().await;
    let (sso, _plugins, _dir) = service(true, None).await;
    let found = sso
        .test_oidc_provider(&OidcProvider {
            key: "fake".to_owned(),
            allow_http_issuer: false,
            ..fake_provider(&fake, None)
        })
        .await
        .expect("a result");
    assert!(
        found
            .error
            .as_deref()
            .is_some_and(|why| why.contains("the issuer must use https")),
        "{found:?}"
    );
    assert!(fake.requests().is_empty(), "nothing was fetched");
}

#[tokio::test]
async fn a_disabled_plugin_tests_nothing() {
    let fake = FakeIdentityProvider::start().await;
    let (sso, _plugins, _dir) = service(false, None).await;
    let provider = OidcProvider {
        key: "fake".to_owned(),
        ..fake_provider(&fake, None)
    };
    assert!(matches!(
        sso.test_oidc_provider(&provider).await,
        Err(ServiceError::NotFound(_))
    ));
    assert_eq!(fake.requests().len(), 0, "nothing was fetched");
}
