//! Curated, compiled-in **extensions** for Ferrofin — the Rust answer to Jellyfin
//! plugins (Rust cannot load .NET assemblies at runtime; see
//! `docs/EXTENSIONS.md`).
//!
//! An [`Extension`] is a repo-curated capability that **surfaces as a plugin** on
//! the frozen `/Plugins` API (enable/disable toggle, config page) via the
//! existing [`PluginManager`](ferrofin_traits::plugins::PluginManager), and can
//! contribute background [`ScheduledTask`]s. The set is static —
//! [`builtin_extensions`] — but each is enable/disable-able at runtime; a task
//! self-gates on its plugin's enabled flag, so toggling needs no restart.
//!
//! The composition root drives this: it picks the extensions to register
//! ([`registered_extensions`]), builds [`RegisteredPlugin`]s from their
//! descriptors ([`registered_plugins`]), registers each extension's tasks
//! ([`register_tasks`]) and hands their authentication providers to the user
//! manager ([`register_auth_providers`]).

use std::sync::Arc;

use ferrofin_core::{
    FerrofinTaskManager, FerrofinUserManager, PluginConfigPage, RegisteredPlugin, ScheduledTask,
};
use ferrofin_traits::library::{LibraryManager, UserManager};
use ferrofin_traits::media_segments::{MediaSegmentManager, MediaSegmentProvider};
use ferrofin_traits::merge_versions::MergeVersionsManager;
use ferrofin_traits::plugins::{PluginDescriptor, PluginManager};
use ferrofin_traits::security::{AuthenticationManager, PasswordResetManager};
use uuid::Uuid;

use crate::fingerprint::Fingerprinter;

pub mod ferrofin_settings;
pub mod ffmpeg;
pub mod file_transformation;
pub mod fingerprint;
pub mod intro_skipper;
pub mod ldap_auth;
mod library_pages;
pub mod merge_versions;

/// The collaborators an extension's tasks are allowed to touch (trait objects
/// only, so extensions stay decoupled from the concrete managers).
#[derive(Clone)]
pub struct ExtensionContext {
    /// Enumerate library items (episodes to analyze).
    pub library: Arc<dyn LibraryManager>,
    /// Persist detected intro/credit segments.
    pub media_segments: Arc<dyn MediaSegmentManager>,
    /// Read the extension's enabled flag + JSON configuration.
    pub plugins: Arc<dyn PluginManager>,
    /// The audio fingerprinter, or `None` when no Chromaprint backend exists
    /// (ffmpeg's `chromaprint` muxer, else `fpcalc`) — the intro skipper then
    /// reports unavailable.
    pub fingerprinter: Option<Arc<dyn Fingerprinter>>,
    /// ffmpeg's other detections (silence, keyframes) and the audio-duration
    /// probe.
    pub ffmpeg: Arc<dyn ffmpeg::FfmpegService>,
    /// Root for per-extension caches (fingerprints): `{cache}/extensions`.
    pub cache_dir: ferrofin_util::directory_path::DirectoryPath,
    /// Bulk merge/split of duplicate versions — the Merge Versions extension's
    /// service, shared by its scheduled tasks and the `/MergeVersions/*` routes.
    pub merge_versions: Arc<dyn MergeVersionsManager>,
    /// The Intro Skipper's own store (segments, season state, disabled
    /// episodes), which detection writes and honours.
    pub intro_skipper: Arc<dyn ferrofin_traits::intro_skipper::IntroSkipperStore>,
    /// The Intro Skipper's run state (its one-pass latch, `AnalyzeAgain`, the
    /// automatic analysis), shared by its scheduled tasks and the routes'
    /// analysis handle.
    pub intro_skipper_runtime: Arc<intro_skipper::Runtime>,
    /// The libraries and their options (which libraries an extension may
    /// analyse).
    pub virtual_folders: Arc<dyn ferrofin_traits::library::VirtualFolderManager>,
    /// Items' chapters (analysis snaps segment bounds to them).
    pub chapters: Arc<dyn ferrofin_traits::chapters::ChapterManager>,
    /// The user accounts: an authentication provider looks up and provisions
    /// the users it signs in through them. Such a provider is registered with
    /// this same manager ([`register_auth_providers`]), so each holds the
    /// other. Both live for the whole process, so the cycle is never freed,
    /// which is harmless.
    pub users: Arc<dyn UserManager>,
    /// Links from outside identities (an LDAP uid, later an OIDC subject) to
    /// users: an authentication provider finds and records through it who
    /// signs in with which identity.
    pub external_identities: Arc<dyn ferrofin_traits::external_identities::ExternalIdentityStore>,
}

/// A curated, compiled-in capability that surfaces as a Jellyfin plugin.
pub trait Extension: Send + Sync {
    /// Stable id — also the plugin id in `/Plugins`.
    fn id(&self) -> Uuid;
    /// The `/Plugins` presentation (name, version, description). `enabled` is the
    /// plugin's default: the plugin manager reports it until an administrator
    /// enables or disables the plugin, and the persisted choice after that.
    fn descriptor(&self) -> PluginDescriptor;
    /// The seed configuration written on first run (the plugin config schema).
    fn default_config(&self) -> Vec<u8>;
    /// The dashboard pages/resources this extension ships; the first entry is
    /// the main settings page (jellyfin-web links the first page whose
    /// `PluginId` matches). Providing any makes a Settings link appear on the
    /// plugin in the dashboard (served via `GET /web/ConfigurationPage`).
    /// Defaults to none.
    fn config_pages(&self) -> Vec<PluginConfigPage> {
        Vec::new()
    }
    /// Providers backed by this actual loaded extension.
    fn media_segment_providers(
        &self,
        _cx: &ExtensionContext,
    ) -> Vec<Arc<dyn MediaSegmentProvider>> {
        Vec::new()
    }
    /// The authentication providers this extension contributes. Sign-in
    /// consults them after the built-in default, and `GET /Auth/Providers`
    /// lists each one its `is_enabled` admits, so a provider gates itself on
    /// its plugin's enabled flag. Defaults to none.
    fn auth_providers(&self, _cx: &ExtensionContext) -> Vec<Arc<dyn AuthenticationManager>> {
        Vec::new()
    }
    /// The password-reset providers this extension contributes. A
    /// forgotten-password request goes to the one a user's
    /// `PasswordResetProviderId` names, when it is enabled, and every provider
    /// is offered a PIN after the built-in one. Defaults to none.
    fn password_reset_providers(
        &self,
        _cx: &ExtensionContext,
    ) -> Vec<Arc<dyn PasswordResetManager>> {
        Vec::new()
    }
    /// The background tasks this extension contributes.
    fn tasks(&self, cx: &ExtensionContext) -> Vec<Arc<dyn ScheduledTask>>;
}

/// The curated set of extensions — the only place they are listed.
#[must_use]
pub fn builtin_extensions() -> Vec<Arc<dyn Extension>> {
    vec![
        Arc::new(intro_skipper::IntroSkipperExtension::new()),
        Arc::new(file_transformation::FileTransformationExtension),
        Arc::new(merge_versions::MergeVersionsExtension),
        Arc::new(ferrofin_settings::FerrofinSettingsExtension),
        Arc::new(ldap_auth::LdapAuthExtension),
    ]
}

/// The extensions a server registers: every one of [`builtin_extensions`],
/// or, with `disable_extensions` set (the benchmark's switch, so Ferrofin is
/// measured against a Jellyfin with no plugins), only the Ferrofin settings
/// extension. That one ports no plugin and has no tasks or hooks to fire
/// inside a measurement: it is the server's own settings page.
#[must_use]
pub fn registered_extensions(disable_extensions: bool) -> Vec<Arc<dyn Extension>> {
    builtin_extensions()
        .into_iter()
        .filter(|extension| {
            !disable_extensions || extension.id() == ferrofin_settings::EXTENSION_ID
        })
        .collect()
}

/// Builds the [`RegisteredPlugin`]s the plugin manager needs, so every extension
/// appears in `/Plugins` with its seed config.
#[must_use]
pub fn registered_plugins(extensions: &[Arc<dyn Extension>]) -> Vec<RegisteredPlugin> {
    extensions
        .iter()
        .map(|ext| {
            let mut plugin = RegisteredPlugin::new(ext.descriptor(), None)
                .with_default_config(ext.default_config());
            for page in ext.config_pages() {
                plugin = plugin.with_config_page(page);
            }
            plugin
        })
        .collect()
}

/// Registers every extension's tasks with the task manager.
pub fn register_tasks(
    extensions: &[Arc<dyn Extension>],
    cx: &ExtensionContext,
    task_manager: &FerrofinTaskManager,
) {
    for ext in extensions {
        for task in ext.tasks(cx) {
            task_manager.register(task);
        }
    }
}

/// Hands every extension's authentication and password-reset providers to the
/// user manager, in extension order. The manager is built long before the
/// extensions, so this fills its late slots
/// ([`FerrofinUserManager::set_extension_providers`],
/// [`FerrofinUserManager::set_extension_reset_providers`]); call it once.
pub fn register_auth_providers(
    extensions: &[Arc<dyn Extension>],
    cx: &ExtensionContext,
    users: &FerrofinUserManager,
) {
    users.set_extension_providers(
        extensions
            .iter()
            .flat_map(|ext| ext.auth_providers(cx))
            .collect(),
    );
    users.set_extension_reset_providers(
        extensions
            .iter()
            .flat_map(|ext| ext.password_reset_providers(cx))
            .collect(),
    );
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::merge_versions::{MergeProgress, MergeVersionsManager};
    use ferrofin_traits::plugins::DisabledPluginManager;
    use ferrofin_traits::tasks::TaskManager;

    use super::*;

    struct NoMerges;

    #[async_trait]
    impl MergeVersionsManager for NoMerges {
        async fn merge_movies(&self, _p: Option<MergeProgress<'_>>) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn split_movies(&self, _p: Option<MergeProgress<'_>>) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn merge_episodes(&self, _p: Option<MergeProgress<'_>>) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn split_episodes(&self, _p: Option<MergeProgress<'_>>) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    async fn context() -> ExtensionContext {
        context_with_users().await.0
    }

    /// The context, and its user manager as the concrete type the composition
    /// root registers providers with.
    async fn context_with_users() -> (ExtensionContext, Arc<FerrofinUserManager>) {
        let db = ferrofin_db::Database::connect_in_memory()
            .await
            .expect("connect");
        db.run_migrations().await.expect("migrations");
        let users = Arc::new(FerrofinUserManager::new(db.clone()));
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ferrofin_core::item_type_lookup::ItemTypeLookup::new());
        let library: Arc<dyn LibraryManager> =
            Arc::new(ferrofin_core::FerrofinLibraryManager::new(
                Arc::new(ferrofin_core::FerrofinItemRepository::new(
                    db.clone(),
                    lookup,
                )),
                Arc::new(ferrofin_core::FerrofinItemCountService::new(db.clone())),
                Arc::new(ferrofin_core::FerrofinItemPersistenceService::new(
                    db.clone(),
                )),
                Arc::new(ferrofin_core::FerrofinPeopleRepository::new(db.clone())),
            ));
        let cx = ExtensionContext {
            media_segments: Arc::new(ferrofin_core::FerrofinMediaSegmentManager::new(
                db.clone(),
                Arc::clone(&library),
            )),
            library,
            plugins: Arc::new(DisabledPluginManager),
            fingerprinter: None,
            ffmpeg: Arc::new(crate::ffmpeg::FakeFfmpeg::default()),
            cache_dir: ferrofin_util::directory_path::DirectoryPath::from(
                "/tmp/ferrofin-extensions-test-cache",
            ),
            merge_versions: Arc::new(NoMerges),
            intro_skipper: Arc::new(
                ferrofin_traits::intro_skipper::InMemoryIntroSkipperStore::default(),
            ),
            intro_skipper_runtime: Arc::default(),
            virtual_folders: Arc::new(
                ferrofin_traits::stubs::virtual_folders::DisabledVirtualFolderManager,
            ),
            chapters: Arc::new(ferrofin_traits::stubs::chapters::NoChapters),
            users: Arc::clone(&users) as Arc<dyn UserManager>,
            external_identities: Arc::new(
                ferrofin_core::external_identity_repository::FerrofinExternalIdentityStore::new(
                    db.clone(),
                ),
            ),
        };
        (cx, users)
    }

    /// A test-only extension contributing one directory-style provider, whose
    /// display name and stored id differ, as LDAP-Auth's do.
    struct DirectoryExtension;

    /// Signs anyone in with the password "directory", provisioning a
    /// first-time user through the context's user manager.
    struct DirectoryProvider {
        users: Arc<dyn UserManager>,
    }

    #[async_trait]
    impl AuthenticationManager for DirectoryProvider {
        fn name(&self) -> &'static str {
            "Tests Directory"
        }
        fn id(&self) -> &'static str {
            "Tests.Plugin.DirectoryProvider"
        }
        async fn is_enabled(&self) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn authenticate(
            &self,
            username: &str,
            password: &str,
            resolved_user: Option<&ferrofin_db::entities::users::UserEntity>,
        ) -> Result<ferrofin_traits::security::ProviderAuthenticationResult, ServiceError> {
            if password != "directory" {
                return Err(ServiceError::unauthorized("wrong password"));
            }
            if resolved_user.is_none() {
                self.users.create_user(username).await?;
            }
            Ok(ferrofin_traits::security::ProviderAuthenticationResult {
                username: username.to_owned(),
                ..Default::default()
            })
        }
        async fn change_password(
            &self,
            _user: &ferrofin_db::entities::users::UserEntity,
            _new_password: &str,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        fn new_user_policy(&self) -> Option<ferrofin_model::users::UserPolicy> {
            None
        }
    }

    impl Extension for DirectoryExtension {
        fn id(&self) -> Uuid {
            Uuid::from_u128(0x5eed_d1ec_7012)
        }
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                id: self.id(),
                name: "Tests Directory".to_owned(),
                ..PluginDescriptor::default()
            }
        }
        fn default_config(&self) -> Vec<u8> {
            b"{}".to_vec()
        }
        fn auth_providers(&self, cx: &ExtensionContext) -> Vec<Arc<dyn AuthenticationManager>> {
            vec![Arc::new(DirectoryProvider {
                users: Arc::clone(&cx.users),
            })]
        }
        fn password_reset_providers(
            &self,
            _cx: &ExtensionContext,
        ) -> Vec<Arc<dyn PasswordResetManager>> {
            vec![Arc::new(DirectoryResetPage)]
        }
        fn tasks(&self, _cx: &ExtensionContext) -> Vec<Arc<dyn ScheduledTask>> {
            Vec::new()
        }
    }

    /// Sends a forgotten password to the directory's own reset page, as
    /// LDAP-Auth's reset provider does.
    struct DirectoryResetPage;

    #[async_trait]
    impl PasswordResetManager for DirectoryResetPage {
        fn name(&self) -> &'static str {
            "Tests Directory"
        }
        fn id(&self) -> &'static str {
            "Tests.Plugin.DirectoryProvider"
        }
        async fn is_enabled(&self) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn start_forgot_password_process(
            &self,
            _user: Option<&ferrofin_db::entities::users::UserEntity>,
            _entered_username: &str,
            _is_in_network: bool,
        ) -> Result<ferrofin_model::users::ForgotPasswordResult, ServiceError> {
            Ok(ferrofin_model::users::ForgotPasswordResult {
                action: ferrofin_model::users::ForgotPasswordAction::ContactAdmin,
                pin_file: Some("https://directory.example/reset".to_owned()),
                pin_expiration_date: None,
            })
        }
        async fn redeem_password_reset_pin(
            &self,
            _pin: &str,
        ) -> Result<ferrofin_model::users::PinRedeemResult, ServiceError> {
            Ok(ferrofin_model::users::PinRedeemResult::default())
        }
    }

    #[tokio::test]
    async fn an_extensions_reset_provider_is_listed_and_answers_its_users() {
        let (cx, users) = context_with_users().await;
        let extensions: Vec<Arc<dyn Extension>> = vec![Arc::new(DirectoryExtension)];
        register_auth_providers(&extensions, &cx, &users);

        let ids: Vec<Option<String>> = cx
            .users
            .get_password_reset_providers()
            .await
            .expect("list")
            .into_iter()
            .map(|pair| pair.id)
            .collect();
        assert_eq!(
            ids,
            [
                Some(
                    "Jellyfin.Server.Implementations.Users.DefaultPasswordResetProvider".to_owned()
                ),
                Some("Tests.Plugin.DirectoryProvider".to_owned()),
            ]
        );
        let user = cx.users.create_user("dana").await.expect("create");
        let uid = Uuid::parse_str(&user.id).expect("id");
        cx.users
            .update_policy(
                uid,
                &ferrofin_model::users::UserPolicy {
                    authentication_provider_id:
                        "Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider"
                            .to_owned(),
                    password_reset_provider_id: "Tests.Plugin.DirectoryProvider".to_owned(),
                    ..Default::default()
                },
            )
            .await
            .expect("policy");
        let result = cx
            .users
            .start_forgot_password_process("dana", true)
            .await
            .expect("forgot password");
        assert_eq!(
            result.pin_file.as_deref(),
            Some("https://directory.example/reset"),
            "the saved reset provider answers"
        );
    }

    #[tokio::test]
    async fn an_extensions_auth_provider_is_listed_and_signs_its_users_in() {
        let (cx, users) = context_with_users().await;
        let extensions: Vec<Arc<dyn Extension>> = vec![Arc::new(DirectoryExtension)];
        register_auth_providers(&extensions, &cx, &users);

        let pair = |name: &str, id: &str| ferrofin_model::dto::NameIdPair {
            name: Some(name.to_owned()),
            id: Some(id.to_owned()),
        };
        assert_eq!(
            cx.users.get_authentication_providers().await.expect("list"),
            vec![
                pair(
                    "Default",
                    "Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider"
                ),
                pair("Tests Directory", "Tests.Plugin.DirectoryProvider"),
            ],
            "after Default, under a name that differs from its id"
        );

        // A first sign-in provisions the user and saves the provider's id.
        let user = cx
            .users
            .authenticate_user("dana", "directory", "127.0.0.1", true)
            .await
            .expect("sign in")
            .expect("provisioned");
        assert_eq!(
            user.authentication_provider_id,
            "Tests.Plugin.DirectoryProvider"
        );
        // That saved id selects the provider from then on.
        assert!(
            cx.users
                .authenticate_user("dana", "wrong", "127.0.0.1", true)
                .await
                .expect("sign in")
                .is_none()
        );
        assert!(
            cx.users
                .authenticate_user("dana", "directory", "127.0.0.1", true)
                .await
                .expect("sign in")
                .is_some()
        );
    }

    #[test]
    fn registered_plugins_carry_each_extensions_seed_config_and_pages() {
        let extensions = builtin_extensions();
        assert_eq!(extensions.len(), 5);
        let plugins = registered_plugins(&extensions);
        assert_eq!(plugins.len(), extensions.len());
        for (plugin, ext) in plugins.iter().zip(&extensions) {
            assert_eq!(plugin.descriptor.id, ext.id());
            assert!(
                serde_json::from_slice::<serde_json::Value>(&plugin.default_config)
                    .expect("seed config is JSON")
                    .is_object(),
                "{} must seed an object config",
                plugin.descriptor.name
            );
            assert_eq!(plugin.config_pages.len(), ext.config_pages().len());
        }
    }

    #[test]
    fn extension_ids_are_unique() {
        let ids: std::collections::HashSet<Uuid> =
            builtin_extensions().iter().map(|e| e.id()).collect();
        assert_eq!(ids.len(), 5, "plugin ids double as /Plugins keys");
    }

    #[tokio::test]
    async fn ldap_auth_is_listed_under_the_upstream_guid_and_off_by_default() {
        let id = Uuid::parse_str("958aad66-3784-4d2a-b89a-a7b6fab6e25c").expect("guid");
        let ldap = builtin_extensions()
            .into_iter()
            .find(|e| e.id() == id)
            .expect("LDAP-Auth is a built-in extension");
        let descriptor = ldap.descriptor();
        assert_eq!(descriptor.name, "LDAP-Auth");
        assert!(
            !descriptor.enabled,
            "D3: disabled until an admin enables it"
        );
        // It contributes the sign-in provider under upstream's name and id,
        // which stays out of `GET /Auth/Providers` while the plugin is off.
        let (cx, users) = context_with_users().await;
        let providers = ldap.auth_providers(&cx);
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].name(), "LDAP-Authentication");
        assert_eq!(
            providers[0].id(),
            "Jellyfin.Plugin.LDAP_Auth.LdapAuthenticationProviderPlugin"
        );
        assert!(!providers[0].is_enabled().await.expect("enabled"));
        register_auth_providers(std::slice::from_ref(&ldap), &cx, &users);
        let listed: Vec<Option<String>> = cx
            .users
            .get_authentication_providers()
            .await
            .expect("list")
            .into_iter()
            .map(|pair| pair.name)
            .collect();
        assert_eq!(listed, [Some("Default".to_owned())]);
        assert!(ldap.password_reset_providers(&cx).is_empty());
        assert!(ldap.tasks(&cx).is_empty());
    }

    #[tokio::test]
    async fn disable_extensions_keeps_only_the_ferrofin_settings() {
        let ids = |extensions: Vec<Arc<dyn Extension>>| -> Vec<Uuid> {
            extensions.iter().map(|e| e.id()).collect()
        };
        assert_eq!(
            ids(registered_extensions(false)),
            ids(builtin_extensions()),
            "unset, every extension is registered"
        );
        let kept = registered_extensions(true);
        assert_eq!(ids(kept.clone()), vec![ferrofin_settings::EXTENSION_ID]);
        // Nothing of it can fire inside a measurement window.
        let cx = context().await;
        assert!(kept[0].tasks(&cx).is_empty());
        assert!(kept[0].media_segment_providers(&cx).is_empty());
        assert!(kept[0].auth_providers(&cx).is_empty());
        assert!(kept[0].password_reset_providers(&cx).is_empty());
    }

    #[tokio::test]
    async fn register_tasks_registers_every_extensions_tasks() {
        let cx = context().await;
        let extensions = builtin_extensions();
        let expected: Vec<String> = extensions
            .iter()
            .flat_map(|e| e.tasks(&cx))
            .map(|t| t.key().to_owned())
            .collect();
        assert!(!expected.is_empty(), "{expected:?}");

        let tasks = FerrofinTaskManager::new();
        register_tasks(&extensions, &cx, &tasks);
        let registered: std::collections::HashSet<String> = tasks
            .get_tasks()
            .await
            .expect("tasks")
            .into_iter()
            .filter_map(|t| t.key)
            .collect();
        for key in expected {
            assert!(registered.contains(&key), "{key} was not registered");
        }
    }
}
