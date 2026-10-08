use super::*;
use ferrofin_traits::security::ProviderAuthenticationResult;
use std::sync::atomic::{AtomicUsize, Ordering};

struct ExternalProvider {
    db: Database,
    enabled: bool,
    provision: bool,
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
    let db = crate::test_support::test_db().await;
    let provider = Arc::new(ExternalProvider {
        db: db.clone(),
        enabled,
        provision,
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
