use super::*;
use ferrofin_traits::security::ProviderAuthenticationResult;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct ExternalProvider {
    db: Database,
    enabled: bool,
    provision: bool,
    /// The administrator flag every successful sign-in reports.
    administrator: Option<bool>,
    calls: AtomicUsize,
    passwords: Mutex<Vec<String>>,
}

#[async_trait]
impl AuthenticationManager for ExternalProvider {
    fn name(&self) -> &'static str {
        "Tests.External"
    }
    async fn is_enabled(&self) -> Result<bool, ServiceError> {
        Ok(self.enabled)
    }
    async fn authenticate(
        &self,
        username: &str,
        password: &str,
        resolved_user: Option<&UserEntity>,
    ) -> Result<ProviderAuthenticationResult, ServiceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if password != "external" {
            return Err(ServiceError::unauthorized("wrong password"));
        }
        let username = if self.provision && resolved_user.is_none() {
            FerrofinUserManager::new(self.db.clone())
                .create_user("canonical")
                .await?;
            "canonical"
        } else {
            username
        };
        Ok(ProviderAuthenticationResult {
            username: username.to_owned(),
            display_name: None,
            administrator: self.administrator,
        })
    }
    async fn change_password(
        &self,
        _user: &UserEntity,
        new_password: &str,
    ) -> Result<(), ServiceError> {
        self.passwords.lock().unwrap().push(new_password.to_owned());
        Ok(())
    }
    fn new_user_policy(&self) -> Option<UserPolicy> {
        Some(UserPolicy {
            is_hidden: true,
            max_active_sessions: 3,
            ..UserPolicy::default()
        })
    }
}

async fn manager(enabled: bool, provision: bool) -> (FerrofinUserManager, Arc<ExternalProvider>) {
    flagging_manager(enabled, provision, None).await
}

async fn flagging_manager(
    enabled: bool,
    provision: bool,
    administrator: Option<bool>,
) -> (FerrofinUserManager, Arc<ExternalProvider>) {
    let db = crate::test_support::test_db().await;
    let provider = Arc::new(ExternalProvider {
        db: db.clone(),
        enabled,
        provision,
        administrator,
        calls: AtomicUsize::new(0),
        passwords: Mutex::new(Vec::new()),
    });
    (
        FerrofinUserManager::with_providers(db, vec![provider.clone()]),
        provider,
    )
}

#[rstest::rstest]
#[case("tests.external", true, true, 1)]
#[case("Tests.External", false, false, 0)]
#[case("missing", true, false, 0)]
#[case("", true, true, 1)]
#[case(DEFAULT_AUTH_PROVIDER_ID, true, false, 0)]
#[tokio::test]
async fn saved_provider_selects_only_enabled_matches(
    #[case] selected: &str,
    #[case] enabled: bool,
    #[case] allowed: bool,
    #[case] calls: usize,
) {
    let (mgr, provider) = manager(enabled, false).await;
    let user = mgr.create_user("selected").await.unwrap();
    let uid = Uuid::parse_str(&user.id).unwrap();
    mgr.update_policy(
        uid,
        &UserPolicy {
            authentication_provider_id: selected.to_owned(),
            ..UserPolicy::default()
        },
    )
    .await
    .unwrap();
    let result = mgr
        .authenticate_user("selected", "external", "127.0.0.1", true)
        .await
        .unwrap();
    assert_eq!(result.is_some(), allowed);
    assert_eq!(provider.calls.load(Ordering::SeqCst), calls);
    if allowed {
        assert!(
            mgr.get_user_by_id(uid)
                .await
                .unwrap()
                .unwrap()
                .authentication_provider_id
                .eq_ignore_ascii_case("Tests.External")
        );
    }
    let choices = mgr.get_authentication_providers().await.unwrap();
    assert_eq!(choices[0].id.as_deref(), Some(DEFAULT_AUTH_PROVIDER_ID));
    assert_eq!(choices.len(), if enabled { 2 } else { 1 });
}

#[tokio::test]
async fn selected_provider_owns_password_changes_and_reset() {
    let (mgr, provider) = manager(true, false).await;
    let user = mgr.create_user("password-owner").await.unwrap();
    let uid = Uuid::parse_str(&user.id).unwrap();
    let mut policy = UserPolicy {
        authentication_provider_id: "Tests.External".into(),
        ..UserPolicy::default()
    };
    mgr.update_policy(uid, &policy).await.unwrap();
    mgr.change_password(uid, "provider-password").await.unwrap();
    mgr.reset_password(uid).await.unwrap();
    assert_eq!(
        *provider.passwords.lock().unwrap(),
        vec!["provider-password", ""]
    );
    assert!(
        mgr.get_user_by_id(uid)
            .await
            .unwrap()
            .unwrap()
            .password
            .is_none()
    );
    // Jellyfin's InvalidAuthProvider rejects authentication and leaves password
    // changes alone. An unavailable provider must not rewrite the local hash.
    policy.authentication_provider_id = "missing".into();
    mgr.update_policy(uid, &policy).await.unwrap();
    mgr.change_password(uid, "unavailable").await.unwrap();
    assert!(
        mgr.get_user_by_id(uid)
            .await
            .unwrap()
            .unwrap()
            .password
            .is_none()
    );
    assert_eq!(provider.passwords.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn provider_provisioning_uses_canonical_name_and_new_user_policy() {
    let (mgr, _) = manager(true, true).await;
    let user = mgr
        .authenticate_user("external-alias", "external", "127.0.0.1", true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(user.username, "canonical");
    assert_eq!(user.authentication_provider_id, "Tests.External");
    let policy = mgr.get_user_dto(&user, None).await.unwrap().policy.unwrap();
    assert!(policy.is_hidden);
    assert_eq!(policy.max_active_sessions, 3);
}

/// gl-1179: a provider's first sign-in saves the new user's policy with
/// `update_policy`, which announces `UserUpdated`, as upstream's
/// `AuthenticateUser` calls `UpdatePolicyAsync` (`UserManager.cs:593`): the
/// new user is announced once, by the name the provider gave it.
/// (`user_updated.rs` pins that the save releases its own lock first.)
#[tokio::test]
async fn a_provisioning_sign_in_announces_the_new_user() {
    let (mgr, _) = manager(true, true).await;
    let bus = crate::event_manager::FerrofinEventManager::new();
    let mgr = mgr.with_events(Arc::new(bus.clone()));
    let announced = Arc::new(Mutex::new(Vec::new()));
    let (reader, sink) = (mgr.clone(), Arc::clone(&announced));
    bus.subscribe(
        crate::user_updated::USER_UPDATED_EVENT,
        Arc::new(move |payload: &str| {
            let id: Uuid = serde_json::from_str(payload).expect("a user id");
            let (reader, sink) = (reader.clone(), Arc::clone(&sink));
            Box::pin(async move {
                let user = reader.get_user_by_id(id).await?.expect("the new user");
                sink.lock().unwrap().push(user.username);
                Ok(())
            })
        }),
    );
    let user = mgr
        .authenticate_user("external-alias", "external", "127.0.0.1", true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*announced.lock().unwrap(), [user.username]);
}

#[tokio::test]
async fn password_login_uses_live_network_classification_for_remote_policy() {
    let db = crate::test_support::test_db().await;
    let mut config = ferrofin_networking::NetworkConfiguration::default();
    let network = Arc::new(RwLock::new(
        ferrofin_networking::NetworkManager::with_defaults(config.clone(), "127.0.0.1,1,lo"),
    ));
    let mgr = FerrofinUserManager::new(db).with_network(network.clone());
    let user = mgr.create_user("local-only").await.unwrap();
    let uid = Uuid::parse_str(&user.id).unwrap();
    let mut policy = UserPolicy {
        enable_remote_access: false,
        ..Default::default()
    };
    for admin in [false, true] {
        policy.is_administrator = admin;
        mgr.update_policy(uid, &policy).await.unwrap();
        assert!(
            mgr.authenticate_user("local-only", "", "127.0.0.1", true)
                .await
                .unwrap()
                .is_some()
        );
        assert!(matches!(
            mgr.authenticate_user("local-only", "", "203.0.113.8", true)
                .await,
            Err(ServiceError::Forbidden(_))
        ));
    }
    config.local_network_subnets = vec!["203.0.113.0/24".into()];
    network.write().unwrap().update_settings(&config);
    assert!(
        mgr.authenticate_user("local-only", "", "203.0.113.8", true)
            .await
            .unwrap()
            .is_some()
    );
}

/// A provider whose display name and stored id differ, as LDAP-Auth's
/// (`LDAP-Authentication`, `Jellyfin.Plugin.LDAP_Auth.LdapAuthenticationProviderPlugin`) do.
struct RenamedProvider {
    name: &'static str,
    id: &'static str,
}

#[async_trait]
impl AuthenticationManager for RenamedProvider {
    fn name(&self) -> &str {
        self.name
    }
    fn id(&self) -> &str {
        self.id
    }
    async fn is_enabled(&self) -> Result<bool, ServiceError> {
        Ok(true)
    }
    async fn authenticate(
        &self,
        username: &str,
        password: &str,
        _resolved_user: Option<&UserEntity>,
    ) -> Result<ProviderAuthenticationResult, ServiceError> {
        if password != "directory" {
            return Err(ServiceError::unauthorized("wrong password"));
        }
        Ok(ProviderAuthenticationResult {
            username: username.to_owned(),
            ..ProviderAuthenticationResult::default()
        })
    }
    async fn change_password(
        &self,
        _user: &UserEntity,
        _new_password: &str,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    fn new_user_policy(&self) -> Option<UserPolicy> {
        None
    }
}

const DIRECTORY: RenamedProvider = RenamedProvider {
    name: "Tests Directory",
    id: "Tests.Directory.Provider",
};

fn pair(name: &str, id: &str) -> NameIdPair {
    NameIdPair {
        name: Some(name.to_owned()),
        id: Some(id.to_owned()),
    }
}

async fn saved_with_provider(mgr: &FerrofinUserManager, name: &str, provider_id: &str) -> Uuid {
    let user = mgr.create_user(name).await.unwrap();
    let uid = Uuid::parse_str(&user.id).unwrap();
    mgr.update_policy(
        uid,
        &UserPolicy {
            authentication_provider_id: provider_id.to_owned(),
            ..UserPolicy::default()
        },
    )
    .await
    .unwrap();
    uid
}

async fn is_administrator(mgr: &FerrofinUserManager, uid: Uuid) -> bool {
    has_permission(
        mgr.db.pool(),
        &guid_to_db(uid),
        PermissionKind::IsAdministrator,
    )
    .await
    .unwrap()
}

async fn set_administrator(mgr: &FerrofinUserManager, uid: Uuid, provider_id: &str) {
    mgr.update_policy(
        uid,
        &UserPolicy {
            is_administrator: true,
            authentication_provider_id: provider_id.to_owned(),
            ..UserPolicy::default()
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn built_in_provider_names_and_ids_are_unchanged() {
    // Jellyfin's `Name` and `GetType().FullName`: a migrated database stores
    // these ids, and jellyfin-web shows these names.
    let default = DefaultAuthenticationProvider::new();
    assert_eq!(default.name(), "Default");
    assert_eq!(
        default.id(),
        "Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider"
    );
    let invalid = InvalidAuthProvider::new();
    assert_eq!(invalid.name(), "InvalidOrMissingAuthenticationProvider");
    assert_eq!(
        invalid.id(),
        "Jellyfin.Server.Implementations.Users.InvalidAuthProvider"
    );

    let mgr = FerrofinUserManager::new(crate::test_support::test_db().await);
    assert_eq!(
        mgr.get_authentication_providers().await.unwrap(),
        vec![pair(
            "Default",
            "Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider"
        )]
    );
    let user = mgr.create_user("local").await.unwrap();
    assert_eq!(
        user.authentication_provider_id,
        "Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider"
    );
}

#[tokio::test]
async fn a_provider_is_listed_by_its_name_and_selected_by_its_id() {
    let db = crate::test_support::test_db().await;
    let mgr = FerrofinUserManager::with_providers(db, vec![Arc::new(DIRECTORY)]);
    assert_eq!(
        mgr.get_authentication_providers().await.unwrap(),
        vec![
            pair("Default", DEFAULT_AUTH_PROVIDER_ID),
            pair("Tests Directory", "Tests.Directory.Provider"),
        ]
    );

    let uid = saved_with_provider(&mgr, "by-id", "tests.directory.provider").await;
    let user = mgr
        .authenticate_user("by-id", "directory", "127.0.0.1", true)
        .await
        .unwrap()
        .expect("the saved id selects the provider, case-insensitively");
    assert_eq!(user.id, guid_to_db(uid));

    // The display name is not an id: such a user fails closed, as upstream's
    // `GetAuthenticationProviders(User)` falls to `InvalidAuthProvider`.
    saved_with_provider(&mgr, "by-name", "Tests Directory").await;
    assert!(
        mgr.authenticate_user("by-name", "directory", "127.0.0.1", true)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_first_sign_in_saves_the_providers_id_not_its_name() {
    struct Provisioning(Database);

    #[async_trait]
    impl AuthenticationManager for Provisioning {
        fn name(&self) -> &'static str {
            DIRECTORY.name
        }
        fn id(&self) -> &'static str {
            DIRECTORY.id
        }
        async fn is_enabled(&self) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn authenticate(
            &self,
            username: &str,
            password: &str,
            resolved_user: Option<&UserEntity>,
        ) -> Result<ProviderAuthenticationResult, ServiceError> {
            if resolved_user.is_none() {
                FerrofinUserManager::new(self.0.clone())
                    .create_user(username)
                    .await?;
            }
            DIRECTORY
                .authenticate(username, password, resolved_user)
                .await
        }
        async fn change_password(&self, _: &UserEntity, _: &str) -> Result<(), ServiceError> {
            Ok(())
        }
        fn new_user_policy(&self) -> Option<UserPolicy> {
            None
        }
    }

    let db = crate::test_support::test_db().await;
    let mgr = FerrofinUserManager::with_providers(db.clone(), vec![Arc::new(Provisioning(db))]);
    let user = mgr
        .authenticate_user("newcomer", "directory", "127.0.0.1", true)
        .await
        .unwrap()
        .expect("provisioned");
    assert_eq!(user.authentication_provider_id, "Tests.Directory.Provider");
    assert!(
        mgr.authenticate_user("newcomer", "directory", "127.0.0.1", true)
            .await
            .unwrap()
            .is_some(),
        "the saved id selects the provider on the next sign-in"
    );
}

#[tokio::test]
async fn providers_are_listed_default_first_then_by_name_in_the_current_culture() {
    // C# `OrderBy(Default ? 0 : 1).ThenBy(i => i.Name)` compares names in the
    // current culture, where "beta" sorts before "Zeta"; ordinally it would not.
    let db = crate::test_support::test_db().await;
    let mgr = FerrofinUserManager::with_providers(
        db,
        vec![
            Arc::new(RenamedProvider {
                name: "Zeta",
                id: "Tests.Zeta",
            }),
            Arc::new(RenamedProvider {
                name: "beta",
                id: "Tests.Beta",
            }),
        ],
    );
    let listed = ferrofin_util::current_culture::scope(
        "en-US".to_owned(),
        mgr.get_authentication_providers(),
    )
    .await
    .unwrap();
    assert_eq!(
        listed,
        vec![
            pair("Default", DEFAULT_AUTH_PROVIDER_ID),
            pair("beta", "Tests.Beta"),
            pair("Zeta", "Tests.Zeta"),
        ]
    );
}

/// `was_administrator`: the signing-in user's flag before; `other_admins`: how
/// many other administrators exist; `flag`: what the provider reports.
#[rstest::rstest]
#[case::promotes(false, 1, Some(true), true, false)]
#[case::demotes_while_another_administrator_exists(true, 1, Some(false), false, false)]
#[case::keeps_the_last_administrator(true, 0, Some(false), true, true)]
#[case::no_flag_keeps_an_administrator(true, 0, None, true, false)]
#[case::no_flag_keeps_a_user(false, 1, None, false, false)]
#[case::an_unchanged_flag_keeps_an_administrator(true, 0, Some(true), true, false)]
#[tokio::test]
async fn a_providers_administrator_flag_is_applied_at_sign_in(
    #[case] was_administrator: bool,
    #[case] other_admins: usize,
    #[case] flag: Option<bool>,
    #[case] administrator_after: bool,
    #[case] warned: bool,
) {
    let (mgr, provider) = flagging_manager(true, false, flag).await;
    let uid = saved_with_provider(&mgr, "directory-user", "Tests.External").await;
    if was_administrator {
        set_administrator(&mgr, uid, "Tests.External").await;
    }
    for n in 0..other_admins {
        let other = mgr.create_user(&format!("admin-{n}")).await.unwrap();
        set_administrator(
            &mgr,
            Uuid::parse_str(&other.id).unwrap(),
            DEFAULT_AUTH_PROVIDER_ID,
        )
        .await;
    }

    let logs = crate::test_support::CapturedLogs::default();
    let signed_in = {
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(logs.clone())
                .with_ansi(false)
                .finish(),
        );
        // The user exists, so the sign-in holds their login lock across the
        // provider: applying the flag must not wait on that lock.
        tokio::time::timeout(
            Duration::from_secs(10),
            mgr.authenticate_user("directory-user", "external", "127.0.0.1", true),
        )
        .await
        .expect("the sign-in finished: applying the flag took no second login lock")
        .unwrap()
    };
    assert!(signed_in.is_some(), "the sign-in succeeds either way");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(is_administrator(&mgr, uid).await, administrator_after);
    assert_eq!(
        logs.text().contains("last administrator"),
        warned,
        "{}",
        logs.text()
    );
}

#[tokio::test]
async fn a_flag_on_a_wrong_password_changes_nothing() {
    let (mgr, _) = flagging_manager(true, false, Some(true)).await;
    let uid = saved_with_provider(&mgr, "directory-user", "Tests.External").await;
    assert!(
        mgr.authenticate_user("directory-user", "wrong", "127.0.0.1", true)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!is_administrator(&mgr, uid).await);
}

#[tokio::test]
async fn a_provisioned_user_takes_the_flag_after_the_new_user_policy() {
    let (mgr, _) = flagging_manager(true, true, Some(true)).await;
    let user = mgr
        .authenticate_user("external-alias", "external", "127.0.0.1", true)
        .await
        .unwrap()
        .expect("provisioned");
    assert_eq!(user.username, "canonical");
    let policy = mgr.get_user_dto(&user, None).await.unwrap().policy.unwrap();
    // The new-user policy still applied, and the flag came after it.
    assert!(policy.is_hidden);
    assert!(policy.is_administrator);
}

#[tokio::test]
async fn concurrent_demotions_leave_one_administrator() {
    // The administrator count is read inside the transaction that writes the
    // flag, so two administrators demoted at once cannot both pass it.
    let (mgr, _) = flagging_manager(true, false, Some(false)).await;
    let first = saved_with_provider(&mgr, "first", "Tests.External").await;
    let second = saved_with_provider(&mgr, "second", "Tests.External").await;
    set_administrator(&mgr, first, "Tests.External").await;
    set_administrator(&mgr, second, "Tests.External").await;

    let (a, b) = tokio::join!(
        mgr.authenticate_user("first", "external", "127.0.0.1", true),
        mgr.authenticate_user("second", "external", "127.0.0.1", true),
    );
    assert!(a.unwrap().is_some() && b.unwrap().is_some());
    assert_eq!(
        u8::from(is_administrator(&mgr, first).await)
            + u8::from(is_administrator(&mgr, second).await),
        1
    );
}

#[tokio::test]
async fn extension_providers_fill_one_slot_every_clone_shares() {
    let mgr = FerrofinUserManager::new(crate::test_support::test_db().await);
    let earlier = mgr.clone();
    mgr.set_extension_providers(vec![Arc::new(DIRECTORY)]);
    let later = mgr.clone();

    let logs = crate::test_support::CapturedLogs::default();
    {
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(logs.clone())
                .with_ansi(false)
                .finish(),
        );
        later.set_extension_providers(vec![Arc::new(RenamedProvider {
            name: "Second",
            id: "Tests.Second",
        })]);
    }
    assert!(
        logs.text().contains("already registered"),
        "{}",
        logs.text()
    );

    for clone in [&mgr, &earlier, &later] {
        assert_eq!(
            clone.get_authentication_providers().await.unwrap(),
            vec![
                pair("Default", DEFAULT_AUTH_PROVIDER_ID),
                pair("Tests Directory", "Tests.Directory.Provider"),
            ],
            "the first fill, seen by every clone"
        );
    }
    saved_with_provider(&earlier, "slot-user", "Tests.Directory.Provider").await;
    assert!(
        later
            .authenticate_user("slot-user", "directory", "127.0.0.1", true)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        format!("{mgr:?}"),
        "FerrofinUserManager { provider_count: 2, .. }"
    );
}

/// The same rule called directly, as a sign-in path that does not go through
/// `authenticate_user` (OIDC) does: `was_administrator`, `other_admins` and
/// `flag` as in [`a_providers_administrator_flag_is_applied_at_sign_in`].
#[rstest::rstest]
#[case::promotes(false, 1, true, AdminFlagOutcome::Applied, true)]
#[case::demotes_while_another_administrator_exists(
    true,
    1,
    false,
    AdminFlagOutcome::Applied,
    false
)]
#[case::keeps_the_last_administrator(true, 0, false, AdminFlagOutcome::KeptLastAdministrator, true)]
#[case::an_administrator_flag_on_an_administrator_is_unchanged(
    true,
    0,
    true,
    AdminFlagOutcome::Unchanged,
    true
)]
#[case::a_user_flag_on_a_user_is_unchanged(false, 1, false, AdminFlagOutcome::Unchanged, false)]
#[tokio::test]
async fn the_administrator_flag_applies_when_called_directly(
    #[case] was_administrator: bool,
    #[case] other_admins: usize,
    #[case] flag: bool,
    #[case] outcome: AdminFlagOutcome,
    #[case] administrator_after: bool,
) {
    let mgr = FerrofinUserManager::new(crate::test_support::test_db().await);
    let uid = Uuid::parse_str(&mgr.create_user("directory-user").await.unwrap().id).unwrap();
    if was_administrator {
        set_administrator(&mgr, uid, DEFAULT_AUTH_PROVIDER_ID).await;
    }
    for n in 0..other_admins {
        let other = mgr.create_user(&format!("admin-{n}")).await.unwrap();
        set_administrator(
            &mgr,
            Uuid::parse_str(&other.id).unwrap(),
            DEFAULT_AUTH_PROVIDER_ID,
        )
        .await;
    }

    let logs = crate::test_support::CapturedLogs::default();
    let applied = {
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(logs.clone())
                .with_ansi(false)
                .finish(),
        );
        mgr.apply_external_administrator(uid, flag).await.unwrap()
    };
    assert_eq!(applied, outcome);
    assert_eq!(is_administrator(&mgr, uid).await, administrator_after);
    assert_eq!(
        logs.text().contains("last administrator"),
        outcome == AdminFlagOutcome::KeptLastAdministrator,
        "{}",
        logs.text()
    );
}

#[tokio::test]
async fn the_administrator_flag_reaches_a_user_with_no_permission_rows() {
    // An adopted or hand-seeded user may have no `IsAdministrator` row: it
    // reads as false, and the flag inserts it.
    let db = crate::test_support::test_db().await;
    let uid = Uuid::from_u128(0x1141);
    crate::test_support::seed_user(&db, uid).await;
    let mgr = FerrofinUserManager::new(db);
    assert_eq!(
        mgr.apply_external_administrator(uid, false).await.unwrap(),
        AdminFlagOutcome::Unchanged
    );
    assert_eq!(
        mgr.apply_external_administrator(uid, true).await.unwrap(),
        AdminFlagOutcome::Applied
    );
    assert!(is_administrator(&mgr, uid).await);
}

#[tokio::test]
async fn the_administrator_flag_for_an_unknown_user_is_not_found() {
    let mgr = FerrofinUserManager::new(crate::test_support::test_db().await);
    assert!(matches!(
        mgr.apply_external_administrator(Uuid::from_u128(0xdead), true)
            .await,
        Err(ServiceError::NotFound(_))
    ));
}

#[tokio::test]
async fn a_failing_providers_is_enabled_never_reaches_a_local_sign_in() {
    struct Broken;

    #[async_trait]
    impl AuthenticationManager for Broken {
        fn name(&self) -> &'static str {
            "Tests Broken"
        }
        async fn is_enabled(&self) -> Result<bool, ServiceError> {
            Err(ServiceError::backend("configuration unreadable"))
        }
        async fn authenticate(
            &self,
            _: &str,
            _: &str,
            _: Option<&UserEntity>,
        ) -> Result<ProviderAuthenticationResult, ServiceError> {
            unreachable!("never selected")
        }
        async fn change_password(&self, _: &UserEntity, _: &str) -> Result<(), ServiceError> {
            Ok(())
        }
        fn new_user_policy(&self) -> Option<UserPolicy> {
            None
        }
    }

    let mgr = FerrofinUserManager::new(crate::test_support::test_db().await);
    mgr.set_extension_providers(vec![Arc::new(Broken)]);
    mgr.create_user("local").await.unwrap();
    // The saved id selects Default alone, so the broken provider is never asked.
    assert!(
        mgr.authenticate_user("local", "", "127.0.0.1", true)
            .await
            .unwrap()
            .is_some()
    );
}

/// What `f` logs under a scoped `fmt` subscriber.
async fn logged<F: std::future::Future>(f: F) -> (F::Output, String) {
    let logs = crate::test_support::CapturedLogs::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .finish(),
    );
    let output = f.await;
    (output, logs.text())
}

/// Upstream's `GetAuthenticationProviders(User)` warns, naming the user and
/// the saved provider id, before it falls back to `InvalidAuthProvider`. A
/// sign-in and a password change both go through it, so an administrator can
/// tell a missing or disabled provider from a wrong password.
#[rstest::rstest]
#[case::not_registered("Jellyfin.Plugin.LDAP_Auth.LdapAuthenticationProviderPlugin", true)]
#[case::disabled("Tests.External", false)]
#[tokio::test]
async fn a_saved_provider_that_is_missing_or_disabled_is_named_in_a_warning(
    #[case] saved: &str,
    #[case] enabled: bool,
) {
    let (mgr, provider) = manager(enabled, false).await;
    let uid = saved_with_provider(&mgr, "directory-user", saved).await;
    // The fields are checked on the warning's own line: the sign-in's
    // `login failed` line names the user too.
    let named = |text: &str| {
        text.lines().any(|line| {
            line.contains("WARN")
                && line.contains("authentication provider is missing or disabled")
                && line.contains("username=directory-user")
                && line.contains(&format!("user_id={}", guid_to_db(uid)))
                && line.contains(&format!("authentication_provider_id={saved:?}"))
        })
    };

    let (signed_in, text) =
        logged(mgr.authenticate_user("directory-user", "external", "127.0.0.1", true)).await;
    assert!(signed_in.unwrap().is_none(), "the sign-in fails closed");
    assert!(named(&text), "{text}");

    let (changed, text) = logged(mgr.change_password(uid, "new-password")).await;
    changed.unwrap();
    assert!(named(&text), "{text}");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(provider.passwords.lock().unwrap().is_empty());
}

/// A provider the saved id selects, and the built-in one, log no such warning.
#[rstest::rstest]
#[case::selected("Tests.External")]
#[case::built_in(DEFAULT_AUTH_PROVIDER_ID)]
#[case::unsaved("")]
#[tokio::test]
async fn a_saved_provider_that_is_enabled_logs_no_warning(#[case] saved: &str) {
    let (mgr, _) = manager(true, false).await;
    saved_with_provider(&mgr, "directory-user", saved).await;
    let (_, text) =
        logged(mgr.authenticate_user("directory-user", "external", "127.0.0.1", true)).await;
    assert!(
        !text.contains("authentication provider is missing or disabled"),
        "{text}"
    );
}

/// Records the activity entries a manager writes.
#[derive(Default)]
struct RecordingActivity {
    entries: Mutex<Vec<ferrofin_traits::activity::ActivityLogCreate>>,
}

#[async_trait]
impl ferrofin_traits::activity::ActivityManager for RecordingActivity {
    async fn get_paged_result(
        &self,
        _query: &ferrofin_traits::activity::ActivityLogQuery,
    ) -> Result<
        ferrofin_model::querying::QueryResult<ferrofin_model::activity::ActivityLogEntry>,
        ServiceError,
    > {
        Ok(ferrofin_model::querying::QueryResult::default())
    }
    async fn create_entry(
        &self,
        entry: ferrofin_traits::activity::ActivityLogCreate,
    ) -> Result<(), ServiceError> {
        self.entries.lock().unwrap().push(entry);
        Ok(())
    }
    async fn clean(&self, _before: chrono::DateTime<chrono::Utc>) -> Result<u64, ServiceError> {
        Ok(0)
    }
}

/// A user an outside provider creates is its user from the first write, with
/// the folder access it asked for, and is recorded as created.
#[tokio::test]
async fn an_external_user_is_created_already_owned_by_its_provider() {
    let (mgr, provider) = manager(true, false).await;
    let activity = Arc::new(RecordingActivity::default());
    let mgr = mgr.with_activity(activity.clone());
    let folder = Uuid::from_u128(0xf01d_e125);
    let setup = ExternalUserSetup {
        authentication_provider_id: "Tests.External".to_owned(),
        password_reset_provider_id: "Tests.External.Reset".to_owned(),
        enable_all_folders: false,
        enabled_folders: Some(vec![folder.to_string()]),
    };

    let user = mgr
        .create_external_user("dana", &setup)
        .await
        .expect("create");

    assert_eq!(user.authentication_provider_id, "Tests.External");
    assert_eq!(user.password_reset_provider_id, "Tests.External.Reset");
    assert_eq!(user.password, None);
    let policy = mgr
        .get_user_dto(&user, None)
        .await
        .expect("dto")
        .policy
        .expect("policy");
    assert!(!policy.enable_all_folders);
    assert_eq!(policy.enabled_folders, [folder]);
    assert!(!policy.is_administrator);
    assert!(policy.enable_remote_access, "the other defaults are seeded");
    {
        let entries = activity.entries.lock().unwrap();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].type_, "UserCreated");
        assert_eq!(entries[0].name, "User dana has been created");
        assert_eq!(
            entries[0].user_id,
            Some(Uuid::parse_str(&user.id).expect("id"))
        );
    }

    // An empty password goes to its provider, never the built-in one, which
    // would sign a user without a password in.
    assert!(
        mgr.authenticate_user("dana", "", "127.0.0.1", true)
            .await
            .expect("sign in")
            .is_none()
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let plain = mgr.create_user("erin").await.expect("create");
    assert_eq!(plain.authentication_provider_id, DEFAULT_AUTH_PROVIDER_ID);
    assert!(
        mgr.authenticate_user("erin", "", "127.0.0.1", true)
            .await
            .expect("sign in")
            .is_some(),
        "a plain new user is that passwordless local account"
    );
}

/// Without folders, the new user keeps the seeded empty list; an invalid or
/// taken name creates nothing.
#[tokio::test]
async fn an_external_user_without_folders_or_with_a_bad_name() {
    let (mgr, _) = manager(true, false).await;
    let setup = ExternalUserSetup {
        authentication_provider_id: "Tests.External".to_owned(),
        password_reset_provider_id: "Tests.External".to_owned(),
        enable_all_folders: true,
        enabled_folders: None,
    };
    let user = mgr
        .create_external_user("dana", &setup)
        .await
        .expect("create");
    let policy = mgr
        .get_user_dto(&user, None)
        .await
        .expect("dto")
        .policy
        .expect("policy");
    assert!(policy.enable_all_folders);
    assert_eq!(policy.enabled_folders, Vec::<Uuid>::new());

    for name in ["DANA", " dana", ""] {
        assert!(
            matches!(
                mgr.create_external_user(name, &setup).await,
                Err(ServiceError::InvalidInput(_))
            ),
            "{name:?}"
        );
    }
    assert_eq!(mgr.get_users().await.expect("users").len(), 1);
}

/// Delegates everything but `create_external_user` to the real manager.
struct Delegating(FerrofinUserManager);
#[async_trait]
impl UserManager for Delegating {
    async fn get_users(&self) -> Result<Vec<UserEntity>, ServiceError> {
        self.0.get_users().await
    }
    async fn initialize(&self) -> Result<(), ServiceError> {
        self.0.initialize().await
    }
    async fn get_user_by_id(&self, id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
        self.0.get_user_by_id(id).await
    }
    async fn get_first_user(&self) -> Result<Option<UserEntity>, ServiceError> {
        self.0.get_first_user().await
    }
    async fn get_user_by_name(&self, name: &str) -> Result<Option<UserEntity>, ServiceError> {
        self.0.get_user_by_name(name).await
    }
    async fn rename_user(&self, id: Uuid, old: &str, new: &str) -> Result<(), ServiceError> {
        self.0.rename_user(id, old, new).await
    }
    async fn update_user(&self, user: &UserEntity) -> Result<(), ServiceError> {
        self.0.update_user(user).await
    }
    async fn create_user(&self, name: &str) -> Result<UserEntity, ServiceError> {
        self.0.create_user(name).await
    }
    async fn delete_user(&self, user_id: Uuid) -> Result<(), ServiceError> {
        self.0.delete_user(user_id).await
    }
    async fn reset_password(&self, user_id: Uuid) -> Result<(), ServiceError> {
        self.0.reset_password(user_id).await
    }
    async fn change_password(&self, user_id: Uuid, pw: &str) -> Result<(), ServiceError> {
        self.0.change_password(user_id, pw).await
    }
    async fn authenticate_user(
        &self,
        username: &str,
        password: &str,
        remote_endpoint: &str,
        is_user_session: bool,
    ) -> Result<Option<UserEntity>, ServiceError> {
        self.0
            .authenticate_user(username, password, remote_endpoint, is_user_session)
            .await
    }
    async fn get_authentication_providers(&self) -> Result<Vec<NameIdPair>, ServiceError> {
        self.0.get_authentication_providers().await
    }
    async fn get_password_reset_providers(&self) -> Result<Vec<NameIdPair>, ServiceError> {
        self.0.get_password_reset_providers().await
    }
    async fn get_user_dto(
        &self,
        user: &UserEntity,
        server_id: Option<String>,
    ) -> Result<UserDto, ServiceError> {
        self.0.get_user_dto(user, server_id).await
    }
    async fn update_configuration(
        &self,
        user_id: Uuid,
        config: &UserConfiguration,
    ) -> Result<(), ServiceError> {
        self.0.update_configuration(user_id, config).await
    }
    async fn update_policy(&self, user_id: Uuid, policy: &UserPolicy) -> Result<(), ServiceError> {
        self.0.update_policy(user_id, policy).await
    }
    async fn clear_profile_image(&self, user: &UserEntity) -> Result<(), ServiceError> {
        self.0.clear_profile_image(user).await
    }
}

/// The trait's default, for managers without a store of their own, sets the
/// user up with a policy save after creating them.
#[tokio::test]
async fn the_default_external_user_creation_saves_the_setup() {
    let (mgr, _) = manager(true, false).await;
    let users = Delegating(mgr);
    let folder = Uuid::from_u128(0xf01d_e125);
    let user = users
        .create_external_user(
            "dana",
            &ExternalUserSetup {
                authentication_provider_id: "Tests.External".to_owned(),
                password_reset_provider_id: "Tests.External".to_owned(),
                enable_all_folders: false,
                enabled_folders: Some(vec![folder.to_string(), "not a guid".to_owned()]),
            },
        )
        .await
        .expect("create");
    assert_eq!(user.authentication_provider_id, "Tests.External");
    assert_eq!(user.password_reset_provider_id, "Tests.External");
    let policy = users
        .get_user_dto(&user, None)
        .await
        .expect("dto")
        .policy
        .expect("policy");
    assert!(!policy.enable_all_folders);
    assert_eq!(policy.enabled_folders, [folder]);
}
