//! The account a verified sign-in ends at (B6), and the provider of the
//! accounts sign-in creates (B11, D20), through the sign-in harness and the
//! fake identity provider.

use std::sync::atomic::{AtomicUsize, Ordering};

use ferrofin_core::external_identity_repository::FerrofinExternalIdentityStore;
use ferrofin_traits::external_identities::{
    ConditionalLink, ExternalIdentity, ExternalIdentityStore, LinkBy,
};
use ferrofin_traits::library::UserManager;
use ferrofin_traits::sso::SsoManager as _;
use serde_json::{Value, json};

use super::*;
use crate::sso::EXTENSION_ID;
use crate::sso::fake_idp::{SUBJECT, USERNAME};
use crate::sso::sign_in::tests::{Harness, PUBLIC_URL};

/// The identity `subject` at `issuer`, as sign-in links it.
fn identity<'a>(issuer: &'a str, subject: &'a str) -> ExternalIdentityKey<'a> {
    ExternalIdentityKey {
        provider_id: PROVIDER_ID,
        issuer,
        subject,
    }
}

async fn user(harness: &Harness, name: &str) -> Option<UserEntity> {
    harness.users.get_user_by_name(name).await.expect("lookup")
}

async fn user_count(harness: &Harness) -> usize {
    harness.users.get_users().await.expect("users").len()
}

fn id_of(user: &UserEntity) -> Uuid {
    Uuid::parse_str(&user.id).expect("a user id")
}

/// A local account named `name`, linked to the fake's `SUBJECT` at `issuer`.
async fn linked_local(harness: &Harness, name: &str, issuer: &str) -> UserEntity {
    let local = harness.users.create_user(name).await.expect("created");
    harness
        .identities
        .link(id_of(&local), identity(issuer, SUBJECT), None)
        .await
        .expect("linked");
    local
}

#[tokio::test]
async fn a_first_sign_in_creates_an_account_with_the_password_refusing_provider_and_links_it() {
    let harness = Harness::new().await;
    let reply = harness.finish().await;
    assert_eq!(reply.status, 200, "{reply:?}");

    let created = user(&harness, USERNAME).await.expect("created");
    assert_eq!(created.authentication_provider_id, PROVIDER_ID);
    assert_eq!(created.password_reset_provider_id, PROVIDER_ID);
    // D21: a password no one knows, in the default provider's format.
    let password = created.password.as_deref().expect("a password hash");
    assert!(
        password.starts_with("$PBKDF2-SHA512$iterations=210000$"),
        "{password}"
    );
    let link = harness
        .identities
        .find(identity(&harness.fake.issuer(), SUBJECT))
        .await
        .expect("find")
        .expect("linked by issuer and subject");
    assert_eq!(link.user_id, id_of(&created));

    // The next sign-in finds it by the link.
    assert_eq!(harness.finish().await.status, 200);
    assert_eq!(user_count(&harness).await, 1);
}

/// Never by name: a linked identity ends at its account whatever the
/// username claim says, and a local account linked to it keeps its provider
/// (D5).
#[tokio::test]
async fn a_linked_identity_signs_in_to_its_account_whatever_its_name() {
    let harness = Harness::new().await;
    let local = linked_local(&harness, "linked-local", &harness.fake.issuer()).await;
    let session = harness.session("linked-device").await.expect("signed in");
    assert_eq!(session.user.name.as_deref(), Some("linked-local"));
    assert_eq!(user(&harness, USERNAME).await, None, "no account was made");
    assert_eq!(
        user(&harness, "linked-local")
            .await
            .expect("kept")
            .authentication_provider_id,
        local.authentication_provider_id
    );
}

/// D3: the subject is the identity only with its issuer. The account linked
/// to the same subject at another issuer keeps its link.
#[tokio::test]
async fn the_same_subject_at_another_issuer_is_another_identity() {
    let harness = Harness::new().await;
    let elsewhere = linked_local(&harness, "elsewhere", "https://other.example.org").await;
    assert_eq!(harness.finish().await.status, 200);
    let created = user(&harness, USERNAME).await.expect("a new account");
    assert_ne!(created.id, elsewhere.id);
    let kept = harness
        .identities
        .find(identity("https://other.example.org", SUBJECT))
        .await
        .expect("find")
        .expect("kept");
    assert_eq!(kept.user_id, id_of(&elsewhere));
}

#[tokio::test]
async fn with_create_users_off_an_unlinked_identity_is_refused_and_no_account_is_made() {
    let harness = Harness::with(Some(PUBLIC_URL), |provider| provider.create_users = false).await;
    let reply = harness.finish().await;
    assert_eq!(reply.status, 403, "{reply:?}");
    assert!(
        reply
            .html
            .contains("this provider does not create accounts"),
        "{}",
        reply.html
    );
    assert!(reply.html.contains("Ask an administrator"));
    assert!(reply.html.contains(r#"<a href="../../../web/">"#));
    assert_eq!(
        reply.set_cookies,
        ["__Host-ferrofin_sso_flow=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax; Secure"]
    );
    assert_eq!(user_count(&harness).await, 0);

    // A linked identity still signs in.
    linked_local(&harness, "linked-local", &harness.fake.issuer()).await;
    assert_eq!(harness.finish().await.status, 200);
}

/// D4: a username claim that is missing, not a name Ferrofin takes, or a
/// name an account has (compared without case, as the user manager finds
/// names) is refused, and nothing is created or changed.
#[rstest::rstest]
#[case::missing(Value::Null, None, "did not send the name")]
#[case::invalid(json!("fake/user"), None, "cannot be a Ferrofin username")]
#[case::padded(json!(" fake-user"), None, "cannot be a Ferrofin username")]
#[case::taken(json!(USERNAME), Some(USERNAME), "already has your name")]
#[case::taken_in_another_case(json!(USERNAME), Some("FAKE-USER"), "already has your name")]
#[tokio::test]
async fn a_missing_invalid_or_taken_name_is_refused_and_nothing_changes(
    #[case] claim: Value,
    #[case] local: Option<&str>,
    #[case] says: &str,
) {
    let harness = Harness::new().await;
    let local = match local {
        Some(name) => Some(harness.users.create_user(name).await.expect("created")),
        None => None,
    };
    harness.fake.sign_in(|sign_in| {
        sign_in
            .claims
            .insert("preferred_username".to_owned(), claim.clone());
    });
    let reply = harness.finish().await;
    assert_eq!(reply.status, 403, "{claim}: {reply:?}");
    assert!(reply.html.contains(says), "{claim}: {}", reply.html);
    assert!(!reply.html.contains(USERNAME), "no claim on the page");
    assert_eq!(user_count(&harness).await, usize::from(local.is_some()));
    assert_eq!(
        harness.identities.list(PROVIDER_ID).await.expect("list"),
        Vec::new()
    );
    if let Some(local) = local {
        // Never signed in to or changed.
        let now = harness
            .users
            .get_user_by_id(id_of(&local))
            .await
            .expect("lookup")
            .expect("kept");
        assert_eq!(
            now.authentication_provider_id,
            local.authentication_provider_id
        );
        assert_eq!(now.password, local.password);
        assert_eq!(now.last_login_date, local.last_login_date);
    }
}

/// B11 and D20, and the community plugin's bug on Jellyfin 10.11, whose
/// accounts were saved with no password under the default provider and so
/// signed in with an empty one: an account sign-in made refuses the empty
/// password and every other, its counter never moves, even at a lockout of
/// one, and no password can be set while this provider is its own.
#[tokio::test]
async fn regression_jellyfin_10_11_a_created_account_never_signs_in_with_a_password() {
    let harness = Harness::new().await;
    assert_eq!(harness.finish().await.status, 200);
    let created = user(&harness, USERNAME).await.expect("created");
    let mut policy = harness
        .users
        .get_user_dto(&created, None)
        .await
        .expect("dto")
        .policy
        .expect("policy");
    policy.login_attempts_before_lockout = 1;
    harness
        .users
        .update_policy(id_of(&created), &policy)
        .await
        .expect("saved");

    for password in ["", "guess", USERNAME] {
        let answer = harness
            .users
            .authenticate_user(USERNAME, password, "127.0.0.1", true)
            .await;
        assert!(matches!(answer, Ok(None)), "{password:?}: {answer:?}");
    }
    let after = user(&harness, USERNAME).await.expect("kept");
    assert_eq!(after.invalid_login_attempt_count, 0);
    let policy = harness
        .users
        .get_user_dto(&after, None)
        .await
        .expect("dto")
        .policy
        .expect("policy");
    assert!(!policy.is_disabled, "never locked out");

    for set in [
        harness.users.change_password(id_of(&after), "chosen").await,
        harness.users.reset_password(id_of(&after)).await,
    ] {
        assert!(matches!(set, Err(ServiceError::Forbidden(_))), "{set:?}");
    }
    assert_eq!(
        user(&harness, USERNAME).await.expect("kept").password,
        created.password
    );

    // With the plugin off, its provider is absent: still refused, still
    // not counted.
    harness
        .sso
        .plugins
        .disable_plugin(EXTENSION_ID)
        .await
        .expect("disable");
    let answer = harness
        .users
        .authenticate_user(USERNAME, "", "127.0.0.1", true)
        .await;
    assert!(matches!(answer, Ok(None)), "{answer:?}");
    let after = user(&harness, USERNAME).await.expect("kept");
    assert_eq!(after.invalid_login_attempt_count, 0);
}

/// Two first sign-ins of one identity at once end at one account.
#[tokio::test]
async fn two_first_sign_ins_of_one_identity_at_once_end_at_one_account() {
    let harness = Harness::new().await;
    let (first, second) = (harness.start().await, harness.start().await);
    let (first, second) = (
        harness.callback(&first).await,
        harness.callback(&second).await,
    );
    let (first, second) = tokio::join!(
        harness.sso.finish_oidc_sign_in("fake", first),
        harness.sso.finish_oidc_sign_in("fake", second),
    );
    for reply in [first.expect("an answer"), second.expect("an answer")] {
        assert_eq!(reply.status, 200, "{reply:?}");
    }
    assert_eq!(user_count(&harness).await, 1);
}

/// The identity store with every link hidden from the first `hidden`
/// lookups (another server linking while this one was not looking), and
/// every new link refused with `refuse_links` (a store that fails).
struct LateLinks {
    inner: Arc<FerrofinExternalIdentityStore>,
    hidden: AtomicUsize,
    refuse_links: bool,
}

/// `harness` with its service over `store`.
fn over(harness: &mut Harness, store: LateLinks) {
    harness.sso = SsoService::new(
        Arc::clone(&harness.sso.plugins),
        Arc::clone(&harness.users) as Arc<dyn UserManager>,
        Arc::new(store),
        Arc::clone(&harness.sso.sessions),
        Arc::clone(&harness.sso.activity),
        Arc::clone(&harness.sso.localization),
        Some(PUBLIC_URL.to_owned()),
    );
}

#[async_trait]
impl ExternalIdentityStore for LateLinks {
    async fn find(
        &self,
        identity: ExternalIdentityKey<'_>,
    ) -> Result<Option<ExternalIdentity>, ServiceError> {
        if self
            .hidden
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Ok(None);
        }
        self.inner.find(identity).await
    }
    async fn list(&self, provider_id: &str) -> Result<Vec<ExternalIdentity>, ServiceError> {
        self.inner.list(provider_id).await
    }
    async fn list_for_user(
        &self,
        provider_id: &str,
        user_id: Uuid,
    ) -> Result<Vec<ExternalIdentity>, ServiceError> {
        self.inner.list_for_user(provider_id, user_id).await
    }
    async fn link(
        &self,
        user_id: Uuid,
        identity: ExternalIdentityKey<'_>,
        avatar_hash: Option<&str>,
    ) -> Result<ExternalIdentity, ServiceError> {
        if self.refuse_links {
            return Err(ServiceError::backend("the store is busy"));
        }
        self.inner.link(user_id, identity, avatar_hash).await
    }
    async fn link_unless_taken(
        &self,
        user_id: Uuid,
        identity: ExternalIdentityKey<'_>,
        by: LinkBy,
    ) -> Result<ConditionalLink, ServiceError> {
        self.inner.link_unless_taken(user_id, identity, by).await
    }
    async fn set_avatar_hash(
        &self,
        identity: ExternalIdentityKey<'_>,
        avatar_hash: Option<&str>,
    ) -> Result<bool, ServiceError> {
        self.inner.set_avatar_hash(identity, avatar_hash).await
    }
    async fn unlink(&self, identity: ExternalIdentityKey<'_>) -> Result<bool, ServiceError> {
        self.inner.unlink(identity).await
    }
}

/// Another server created and linked the identity's account after both of
/// this one's lookups: the create collides on its name, and the link,
/// looked up again, finds it rather than refusing the name as taken.
#[tokio::test]
async fn a_create_that_collides_with_another_servers_finds_its_link() {
    let mut harness = Harness::new().await;
    let elsewhere = linked_local(&harness, USERNAME, &harness.fake.issuer()).await;
    let store = LateLinks {
        inner: Arc::clone(&harness.identities),
        hidden: AtomicUsize::new(2),
        refuse_links: false,
    };
    over(&mut harness, store);
    let reply = harness.finish().await;
    assert_eq!(reply.status, 200, "{reply:?}");
    assert_eq!(user(&harness, USERNAME).await, Some(elsewhere));
    assert_eq!(user_count(&harness).await, 1);
}

/// A link that fails takes the account just created with it: never found by
/// name, it would only refuse the identity at every later sign-in.
#[tokio::test]
async fn an_account_whose_link_fails_is_removed() {
    let mut harness = Harness::new().await;
    // A server always holds another account: the last one cannot be deleted.
    harness.users.create_user("admin").await.expect("created");
    let store = LateLinks {
        inner: Arc::clone(&harness.identities),
        hidden: AtomicUsize::new(0),
        refuse_links: true,
    };
    over(&mut harness, store);
    let started = harness.start().await;
    let callback = harness.callback(&started).await;
    let failed = harness.sso.finish_oidc_sign_in("fake", callback).await;
    assert!(
        matches!(failed, Err(ServiceError::Backend(_))),
        "{failed:?}"
    );
    assert_eq!(user(&harness, USERNAME).await, None, "removed");
    assert_eq!(user_count(&harness).await, 1);
}

/// An identity provider that names no subject: its account could never be
/// found again, so none is made.
#[tokio::test]
async fn an_empty_subject_is_refused_and_no_account_is_made() {
    let harness = Harness::new().await;
    let identity = VerifiedIdentity {
        provider_key: "fake".to_owned(),
        issuer: harness.fake.issuer(),
        subject: String::new(),
        username: Some(USERNAME.to_owned()),
        claims: serde_json::Map::new(),
    };
    let answer = harness
        .sso
        .account(&OidcProvider::default(), &identity)
        .await
        .expect("no store error");
    assert_eq!(answer, Err(Refusal::bad("id_token_subject")));
    assert_eq!(user_count(&harness).await, 0);
}

#[tokio::test]
async fn the_provider_is_listed_only_while_the_plugin_is_enabled() {
    let harness = Harness::new().await;
    let listed = || async {
        harness
            .users
            .get_authentication_providers()
            .await
            .expect("list")
            .into_iter()
            .filter_map(|pair| pair.id.zip(pair.name))
            .collect::<Vec<_>>()
    };
    assert!(
        listed()
            .await
            .contains(&(PROVIDER_ID.to_owned(), PROVIDER_NAME.to_owned())),
        "{:?}",
        listed().await
    );
    harness
        .sso
        .plugins
        .disable_plugin(EXTENSION_ID)
        .await
        .expect("disable");
    assert!(
        !listed().await.iter().any(|(id, _)| id == PROVIDER_ID),
        "{:?}",
        listed().await
    );

    // Written into user rows for good: it never changes.
    assert_eq!(
        PROVIDER_ID,
        "Ferrofin.SingleSignOn.SsoAuthenticationProvider"
    );
    let provider = SsoAuthProvider::new(Arc::clone(&harness.sso.plugins));
    assert!(!provider.counts_failed_passwords());
    assert_eq!(provider.new_user_policy(), None);
    assert!(matches!(
        provider.authenticate(USERNAME, "", None).await,
        Err(ServiceError::Unauthorized(_))
    ));
}
