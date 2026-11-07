use super::*;
use crate::password_reset::DefaultPasswordResetProvider;
use ferrofin_model::users::{ForgotPasswordAction, ForgotPasswordResult, PinRedeemResult};
use ferrofin_traits::security::{PasswordResetManager, ProviderAuthenticationResult};

const TESTS_RESET_ID: &str = "Tests.Reset.Provider";

/// A reset provider like LDAP-Auth's: it answers with a reset page instead of
/// a PIN file, and redeems only the PIN "EXT-PIN".
struct DirectoryResetProvider {
    enabled: bool,
    /// Each request it was handed: (the account's name, the entered name, in network).
    starts: Mutex<Vec<(Option<String>, String, bool)>>,
}

impl DirectoryResetProvider {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            starts: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl PasswordResetManager for DirectoryResetProvider {
    fn name(&self) -> &'static str {
        "Tests Reset"
    }
    fn id(&self) -> &'static str {
        TESTS_RESET_ID
    }
    async fn is_enabled(&self) -> Result<bool, ServiceError> {
        Ok(self.enabled)
    }
    async fn start_forgot_password_process(
        &self,
        user: Option<&UserEntity>,
        entered_username: &str,
        is_in_network: bool,
    ) -> Result<ForgotPasswordResult, ServiceError> {
        self.starts.lock().unwrap().push((
            user.map(|u| u.username.clone()),
            entered_username.to_owned(),
            is_in_network,
        ));
        Ok(ForgotPasswordResult {
            action: ForgotPasswordAction::ContactAdmin,
            pin_file: Some("https://directory.example/reset".to_owned()),
            pin_expiration_date: None,
        })
    }
    async fn redeem_password_reset_pin(&self, pin: &str) -> Result<PinRedeemResult, ServiceError> {
        Ok(if pin == "EXT-PIN" {
            PinRedeemResult {
                success: true,
                users_reset: vec!["directory-user".to_owned()],
            }
        } else {
            PinRedeemResult::default()
        })
    }
}

/// An authentication provider whose password changes fail, to show that a
/// failed reset keeps its PIN file.
struct FailingPasswords;

#[async_trait]
impl AuthenticationManager for FailingPasswords {
    fn name(&self) -> &'static str {
        "Tests.FailingPasswords"
    }
    async fn is_enabled(&self) -> Result<bool, ServiceError> {
        Ok(true)
    }
    async fn authenticate(
        &self,
        _: &str,
        _: &str,
        _: Option<&UserEntity>,
    ) -> Result<ProviderAuthenticationResult, ServiceError> {
        Err(ServiceError::unauthorized("never signs anyone in"))
    }
    async fn change_password(&self, _: &UserEntity, _: &str) -> Result<(), ServiceError> {
        Err(ServiceError::backend("password store unavailable"))
    }
    fn new_user_policy(&self) -> Option<UserPolicy> {
        None
    }
}

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
        unimplemented!("not read here")
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

fn pin_file(result: &ForgotPasswordResult) -> std::path::PathBuf {
    std::path::PathBuf::from(result.pin_file.as_deref().expect("a PIN file path"))
}

fn rewrite(file: &std::path::Path, record: &serde_json::Value) {
    std::fs::write(file, serde_json::to_vec(record).unwrap()).unwrap();
}

async fn save_reset_provider(mgr: &FerrofinUserManager, name: &str, provider_id: &str) -> Uuid {
    let uid = Uuid::parse_str(&mgr.create_user(name).await.unwrap().id).unwrap();
    mgr.update_policy(
        uid,
        &UserPolicy {
            authentication_provider_id: DEFAULT_AUTH_PROVIDER_ID.to_owned(),
            password_reset_provider_id: provider_id.to_owned(),
            ..UserPolicy::default()
        },
    )
    .await
    .unwrap();
    uid
}

#[tokio::test]
async fn an_unknown_user_gets_the_uniform_pin_response_and_no_file() {
    // An unknown username must not disclose account existence, and writes no
    // file.
    let dir = tempfile::tempdir().unwrap();
    let mgr = FerrofinUserManager::new(crate::test_support::test_db().await)
        .with_password_reset_dir(dir.path());
    for entered in ["nobody", "", "   "] {
        let result = mgr
            .start_forgot_password_process(entered, true)
            .await
            .unwrap();
        assert_eq!(result.action, ForgotPasswordAction::PinCode);
        assert!(result.pin_expiration_date.is_some());
        assert!(!pin_file(&result).exists(), "{entered:?}");
    }
}

#[tokio::test]
async fn a_local_pin_resets_the_password_once_and_survives_failed_updates() {
    let dir = tempfile::tempdir().unwrap();
    let activity = Arc::new(RecordingActivity::default());
    let mgr = FerrofinUserManager::with_providers(
        crate::test_support::test_db().await,
        vec![Arc::new(FailingPasswords)],
    )
    .with_password_reset_dir(dir.path())
    .with_activity(activity.clone());
    let bob = mgr.create_user("bob").await.unwrap();
    let bob_id = Uuid::parse_str(&bob.id).unwrap();

    // From outside the local network: the same answer, and no file.
    let remote = mgr
        .start_forgot_password_process("bob", false)
        .await
        .unwrap();
    assert_eq!(remote.action, ForgotPasswordAction::PinCode);
    assert!(!pin_file(&remote).exists());
    let local = mgr
        .start_forgot_password_process("bob", true)
        .await
        .unwrap();
    assert_eq!(local.action, ForgotPasswordAction::PinCode);
    assert_eq!(local.pin_file, remote.pin_file);
    let file = pin_file(&local);
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    record["Pin"] = serde_json::json!("A1-B2-C3-D4");
    rewrite(&file, &record);

    // Case-sensitive: no match is a 404 and keeps the file.
    assert!(matches!(
        mgr.redeem_password_reset_pin("a1-b2-c3-d4").await,
        Err(ServiceError::NotFound(_))
    ));
    assert!(file.exists());

    // A password that cannot be saved fails the redemption and keeps the file.
    let mut policy = UserPolicy {
        authentication_provider_id: "Tests.FailingPasswords".to_owned(),
        ..UserPolicy::default()
    };
    mgr.update_policy(bob_id, &policy).await.unwrap();
    assert!(matches!(
        mgr.redeem_password_reset_pin("A1B2C3D4").await,
        Err(ServiceError::Backend(_))
    ));
    assert!(file.exists(), "a failed password update preserves recovery");
    policy.authentication_provider_id = DEFAULT_AUTH_PROVIDER_ID.to_owned();
    mgr.update_policy(bob_id, &policy).await.unwrap();

    // Two concurrent redemptions reset the password exactly once.
    let (first, second) = tokio::join!(
        mgr.redeem_password_reset_pin("A1B2C3D4"),
        mgr.redeem_password_reset_pin("A1B2C3D4"),
    );
    let (redeemed, refused) = match (first, second) {
        (Ok(redeemed), refused @ Err(_)) | (refused @ Err(_), Ok(redeemed)) => (redeemed, refused),
        other => panic!("expected one reset and one refusal, got {other:?}"),
    };
    assert!(matches!(refused, Err(ServiceError::NotFound(_))));
    assert_eq!(
        redeemed,
        PinRedeemResult {
            success: true,
            users_reset: vec!["bob".to_owned()],
        }
    );
    assert!(
        mgr.authenticate_user("bob", "A1B2C3D4", "127.0.0.1", true)
            .await
            .unwrap()
            .is_some(),
        "the exact entered form becomes the password"
    );
    assert!(!file.exists());
    let entries = activity.entries.lock().unwrap().clone();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].name, "Password has been changed for user bob");
    assert_eq!(entries[0].type_, "UserPasswordChanged");
    assert_eq!(entries[0].user_id, Some(bob_id));

    // A matched file for a deleted user also remains recoverable after failure.
    record["UserName"] = serde_json::json!("deleted");
    rewrite(&file, &record);
    assert!(matches!(
        mgr.redeem_password_reset_pin("A1B2C3D4").await,
        Err(ServiceError::NotFound(_))
    ));
    assert!(file.exists());

    // An expired file never matches and is removed.
    record["ExpirationDate"] = serde_json::json!("2000-01-01T00:00:00Z");
    rewrite(&file, &record);
    assert!(matches!(
        mgr.redeem_password_reset_pin("A1B2C3D4").await,
        Err(ServiceError::NotFound(_))
    ));
    assert!(!file.exists());
}

/// `saved`: the user's `PasswordResetProviderId`; `enabled`: whether the
/// extension provider is; `extension`: whether it answers the request.
#[rstest::rstest]
#[case::the_saved_id_case_insensitively("tests.reset.provider", true, true)]
#[case::an_unknown_id_falls_back_to_the_default("Tests.Missing", true, false)]
#[case::a_disabled_provider_falls_back_to_the_default(TESTS_RESET_ID, false, false)]
#[case::no_saved_id_takes_the_first_enabled_the_default("", true, false)]
#[case::the_default_id(DEFAULT_PASSWORD_RESET_PROVIDER_ID, true, false)]
#[tokio::test]
async fn a_users_saved_reset_provider_answers_their_request(
    #[case] saved: &str,
    #[case] enabled: bool,
    #[case] extension: bool,
) {
    let dir = tempfile::tempdir().unwrap();
    let mgr = FerrofinUserManager::new(crate::test_support::test_db().await)
        .with_password_reset_dir(dir.path());
    let provider = Arc::new(DirectoryResetProvider::new(enabled));
    // Filled on one clone, seen by the other.
    mgr.clone()
        .set_extension_reset_providers(vec![provider.clone()]);
    save_reset_provider(&mgr, "dana", saved).await;

    let result = mgr
        .start_forgot_password_process("dana", true)
        .await
        .unwrap();
    if extension {
        assert_eq!(result.action, ForgotPasswordAction::ContactAdmin);
        assert_eq!(
            *provider.starts.lock().unwrap(),
            vec![(Some("dana".to_owned()), "dana".to_owned(), true)]
        );
    } else {
        assert_eq!(result.action, ForgotPasswordAction::PinCode);
        assert!(pin_file(&result).exists());
        assert!(provider.starts.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn an_unknown_user_always_goes_to_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = FerrofinUserManager::new(crate::test_support::test_db().await)
        .with_password_reset_dir(dir.path());
    let provider = Arc::new(DirectoryResetProvider::new(true));
    mgr.set_extension_reset_providers(vec![provider.clone()]);
    let result = mgr
        .start_forgot_password_process("nobody", true)
        .await
        .unwrap();
    assert_eq!(result.action, ForgotPasswordAction::PinCode);
    assert!(provider.starts.lock().unwrap().is_empty());
}

#[rstest::rstest]
#[case::enabled(true)]
#[case::disabled(false)]
#[tokio::test]
async fn the_enabled_reset_providers_are_listed_default_first(#[case] enabled: bool) {
    let mgr = FerrofinUserManager::new(crate::test_support::test_db().await);
    mgr.set_extension_reset_providers(vec![Arc::new(DirectoryResetProvider::new(enabled))]);
    let mut expected = vec![NameIdPair {
        name: Some("Default Password Reset Provider".to_owned()),
        id: Some("Jellyfin.Server.Implementations.Users.DefaultPasswordResetProvider".to_owned()),
    }];
    if enabled {
        expected.push(NameIdPair {
            name: Some("Tests Reset".to_owned()),
            id: Some(TESTS_RESET_ID.to_owned()),
        });
    }
    assert_eq!(mgr.get_password_reset_providers().await.unwrap(), expected);
}

#[tokio::test]
async fn a_pin_reaches_the_default_after_a_provider_with_no_match() {
    // Redemption offers the PIN to every provider in turn: one that knows
    // nothing of it, ahead of the default, must not stop the default's.
    let dir = tempfile::tempdir().unwrap();
    let mgr = FerrofinUserManager::new(crate::test_support::test_db().await)
        .with_password_reset_dir(dir.path());
    mgr.create_user("erin").await.unwrap();
    let issued = mgr
        .start_forgot_password_process("erin", true)
        .await
        .unwrap();
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(pin_file(&issued)).unwrap()).unwrap();
    record["Pin"] = serde_json::json!("11-22-33-44");
    rewrite(&pin_file(&issued), &record);

    let first = DirectoryResetProvider::new(true);
    let default = DefaultPasswordResetProvider::new(&mgr);
    let result = FerrofinUserManager::redeem_with(&[&first, &default], "11223344")
        .await
        .unwrap();
    assert_eq!(result.users_reset, vec!["erin".to_owned()]);
}

#[tokio::test]
async fn an_extension_provider_redeems_a_pin_the_default_does_not_know() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = FerrofinUserManager::new(crate::test_support::test_db().await)
        .with_password_reset_dir(dir.path());
    // Disabled providers are offered PINs too, as upstream's loop does.
    mgr.set_extension_reset_providers(vec![Arc::new(DirectoryResetProvider::new(false))]);
    assert_eq!(
        mgr.redeem_password_reset_pin("EXT-PIN").await.unwrap(),
        PinRedeemResult {
            success: true,
            users_reset: vec!["directory-user".to_owned()],
        }
    );
    assert!(matches!(
        mgr.redeem_password_reset_pin("nobody's").await,
        Err(ServiceError::NotFound(_))
    ));
}

/// A provider with a given name and id, which redeems nothing.
struct NamedReset {
    name: &'static str,
    id: &'static str,
}

#[async_trait]
impl PasswordResetManager for NamedReset {
    fn name(&self) -> &str {
        self.name
    }
    fn id(&self) -> &str {
        self.id
    }
    async fn is_enabled(&self) -> Result<bool, ServiceError> {
        Ok(true)
    }
    async fn start_forgot_password_process(
        &self,
        _: Option<&UserEntity>,
        _: &str,
        _: bool,
    ) -> Result<ForgotPasswordResult, ServiceError> {
        Ok(ForgotPasswordResult::default())
    }
    async fn redeem_password_reset_pin(&self, _: &str) -> Result<PinRedeemResult, ServiceError> {
        Ok(PinRedeemResult::default())
    }
}

#[tokio::test]
async fn reset_providers_after_the_default_are_listed_by_name_in_the_current_culture() {
    // C# `ThenBy(i => i.Name)` compares in the current culture, where "beta"
    // sorts before "Zeta"; ordinally it would not.
    let mgr = FerrofinUserManager::new(crate::test_support::test_db().await);
    mgr.set_extension_reset_providers(vec![
        Arc::new(NamedReset {
            name: "Zeta",
            id: "Tests.Zeta",
        }),
        Arc::new(NamedReset {
            name: "beta",
            id: "Tests.Beta",
        }),
    ]);
    let names: Vec<Option<String>> = ferrofin_util::current_culture::scope(
        "en-US".to_owned(),
        mgr.get_password_reset_providers(),
    )
    .await
    .unwrap()
    .into_iter()
    .map(|pair| pair.name)
    .collect();
    assert_eq!(
        names,
        [
            Some("Default Password Reset Provider".to_owned()),
            Some("beta".to_owned()),
            Some("Zeta".to_owned()),
        ]
    );
}

#[tokio::test]
async fn a_providers_error_stops_the_redemption() {
    // Upstream's loop lets an exception out: no later provider is offered the
    // PIN, and the caller gets the error rather than a 404.
    struct Broken;

    #[async_trait]
    impl PasswordResetManager for Broken {
        fn name(&self) -> &'static str {
            "Tests Broken"
        }
        async fn is_enabled(&self) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn start_forgot_password_process(
            &self,
            _: Option<&UserEntity>,
            _: &str,
            _: bool,
        ) -> Result<ForgotPasswordResult, ServiceError> {
            Ok(ForgotPasswordResult::default())
        }
        async fn redeem_password_reset_pin(
            &self,
            _: &str,
        ) -> Result<PinRedeemResult, ServiceError> {
            Err(ServiceError::backend("directory unreachable"))
        }
    }

    let later = DirectoryResetProvider::new(true);
    assert!(matches!(
        FerrofinUserManager::redeem_with(&[&Broken, &later], "EXT-PIN").await,
        Err(ServiceError::Backend(_))
    ));
}

#[tokio::test]
async fn issuing_a_pin_needs_the_reset_directory() {
    // The composition root always sets it (upstream's ProgramDataPath); a
    // manager without one fails the request rather than writing anywhere.
    let mgr = FerrofinUserManager::new(crate::test_support::test_db().await);
    mgr.create_user("frank").await.unwrap();
    assert!(matches!(
        mgr.start_forgot_password_process("frank", true).await,
        Err(ServiceError::Backend(_))
    ));
}
