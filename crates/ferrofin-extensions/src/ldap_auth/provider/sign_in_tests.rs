//! Sign-in through LDAP-Auth (`LdapAuthenticationProviderPlugin.Authenticate`)
//! over a fake directory, entered as clients enter it: the real user manager's
//! `authenticate_user` on an in-memory database, with the real plugin manager
//! and external-identity store. Upstream ships no tests for the plugin.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ferrofin_core::FerrofinPluginManager;
use ferrofin_core::FerrofinUserManager;
use ferrofin_core::event_manager::FerrofinEventManager;
use ferrofin_core::external_identity_repository::FerrofinExternalIdentityStore;
use ferrofin_model::secret::Secret;
use ferrofin_traits::external_identities::ExternalIdentity;
use rstest::rstest;

use super::super::{LdapAttribute, LdapAuthExtension, LdapConnection, SearchOutcome, SearchScope};
use super::*;
use crate::{Extension, registered_plugins};

const BASE_DN: &str = "dc=example,dc=org";
const READER_DN: &str = "cn=reader,dc=example,dc=org";
const READER_PASSWORD: &str = "reader-secret";
const ADA_DN: &str = "uid=ada,ou=people,dc=example,dc=org";
const ADA_PASSWORD: &str = "ada-secret";
const BOB_DN: &str = "uid=bob,ou=people,dc=example,dc=org";
const BOB_PASSWORD: &str = "bob-secret";

/// The directory's side of the fake: who may bind, what a search answers,
/// what fails, and every call made.
#[derive(Default)]
struct State {
    /// The passwords the directory accepts, by DN.
    accounts: HashMap<String, String>,
    /// What every search answers.
    entries: Vec<LdapEntry>,
    references: Vec<String>,
    /// What a search with one of these filters answers instead.
    by_filter: HashMap<String, SearchOutcome>,
    /// Failures to answer with instead.
    connect: Option<LdapError>,
    search: Option<LdapError>,
    bind: HashMap<String, LdapError>,
    /// Failures for a search with one of these filters.
    failing_filters: HashMap<String, LdapError>,
    /// The root DSE's `supportedExtension` values, or the failure reading it
    /// answers with.
    extensions: Vec<String>,
    root_dse: Option<LdapError>,
    /// Connections past this many are refused.
    refuse_connections_after: Option<usize>,
    /// The failure a password write answers with instead.
    write: Option<LdapError>,
    /// Every password written, as `(dn, how, value)`: `how` is
    /// `passwordModify` or the attribute replaced.
    writes: Vec<(String, String, String)>,
    // The calls.
    connects: usize,
    binds: Vec<(String, String)>,
    searches: Vec<(String, SearchScope, String, Vec<String>)>,
    unbinds: usize,
}

/// An [`LdapDirectory`] (and its connections) over a shared [`State`].
#[derive(Clone, Default)]
struct FakeDirectory(Arc<Mutex<State>>);

impl FakeDirectory {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().expect("state")
    }

    /// Forgets the calls made so far.
    fn clear_calls(&self) {
        let mut state = self.state();
        state.connects = 0;
        state.binds.clear();
        state.searches.clear();
        state.unbinds = 0;
        state.writes.clear();
    }

    /// Whether anything reached the directory since the last
    /// [`clear_calls`](Self::clear_calls).
    fn untouched(&self) -> bool {
        let state = self.state();
        state.connects == 0
            && state.binds.is_empty()
            && state.searches.is_empty()
            && state.writes.is_empty()
    }

    /// Records a password write and, unless it is scripted to fail, makes the
    /// directory take the new password for `dn`.
    fn write(&self, dn: &str, how: &str, value: &str) -> Result<(), LdapError> {
        let mut state = self.state();
        state
            .writes
            .push((dn.to_owned(), how.to_owned(), value.to_owned()));
        if let Some(error) = &state.write {
            return Err(error.clone());
        }
        state.accounts.insert(dn.to_owned(), value.to_owned());
        Ok(())
    }
}

#[async_trait]
impl LdapDirectory for FakeDirectory {
    async fn connect(
        &self,
        _options: &ConnectionOptions,
    ) -> Result<Box<dyn LdapConnection>, LdapError> {
        let mut state = self.state();
        state.connects += 1;
        if let Some(error) = &state.connect {
            return Err(error.clone());
        }
        if state
            .refuse_connections_after
            .is_some_and(|allowed| state.connects > allowed)
        {
            return Err(LdapError::Connect("connection refused".to_owned()));
        }
        Ok(Box::new(self.clone()))
    }
}

#[async_trait]
impl LdapConnection for FakeDirectory {
    async fn simple_bind(&mut self, dn: &str, password: &str) -> Result<(), LdapError> {
        let mut state = self.state();
        state.binds.push((dn.to_owned(), password.to_owned()));
        if let Some(error) = state.bind.get(dn) {
            return Err(error.clone());
        }
        if (dn.is_empty() && password.is_empty())
            || state
                .accounts
                .get(dn)
                .is_some_and(|known| known == password)
        {
            Ok(())
        } else {
            Err(invalid_credentials())
        }
    }

    async fn search(
        &mut self,
        base: &str,
        scope: SearchScope,
        filter: &str,
        attributes: &[&str],
    ) -> Result<SearchOutcome, LdapError> {
        let mut state = self.state();
        state.searches.push((
            base.to_owned(),
            scope,
            filter.to_owned(),
            attributes.iter().map(|a| (*a).to_owned()).collect(),
        ));
        if let Some(error) = state
            .search
            .as_ref()
            .or_else(|| state.failing_filters.get(filter))
        {
            return Err(error.clone());
        }
        if let Some(outcome) = state.by_filter.get(filter) {
            return Ok(outcome.clone());
        }
        Ok(SearchOutcome {
            entries: state.entries.clone(),
            references: state.references.clone(),
        })
    }

    async fn modify_replace(
        &mut self,
        dn: &str,
        attribute: &str,
        values: &[&str],
    ) -> Result<(), LdapError> {
        assert_eq!(values.len(), 1, "a password write sets one value");
        self.write(dn, attribute, values[0])
    }

    async fn password_modify(
        &mut self,
        user_dn: &str,
        new_password: &str,
    ) -> Result<(), LdapError> {
        self.write(user_dn, "passwordModify", new_password)
    }

    async fn supported_extensions(&mut self) -> Result<Vec<String>, LdapError> {
        let state = self.state();
        match &state.root_dse {
            Some(error) => Err(error.clone()),
            None => Ok(state.extensions.clone()),
        }
    }

    async fn unbind(&mut self) {
        self.state().unbinds += 1;
    }
}

fn id_of(user: &UserEntity) -> Uuid {
    Uuid::parse_str(&user.id).expect("id")
}

fn invalid_credentials() -> LdapError {
    LdapError::Result {
        code: 49,
        message: String::new(),
    }
}

/// A directory entry with `attributes`, each one value.
fn entry(dn: &str, attributes: &[(&str, &str)]) -> LdapEntry {
    LdapEntry {
        dn: dn.to_owned(),
        attributes: attributes
            .iter()
            .map(|(name, value)| LdapAttribute {
                name: (*name).to_owned(),
                values: vec![value.as_bytes().to_vec()],
            })
            .collect(),
    }
}

fn ada() -> LdapEntry {
    entry(
        ADA_DN,
        &[
            ("uid", "ada"),
            ("cn", "Ada Lovelace"),
            ("mail", "ada@example.org"),
        ],
    )
}

fn bob() -> LdapEntry {
    entry(
        BOB_DN,
        &[("uid", "bob"), ("cn", "Bob"), ("mail", "bob@example.org")],
    )
}

/// A configured plain-LDAP directory whose users are named by their uid, with
/// the admin filter (on in upstream's defaults) off.
fn config() -> LdapConfig {
    LdapConfig {
        ldap_admin_filter: String::new(),
        ldap_server: "ldap.example".to_owned(),
        ldap_port: 389,
        use_ssl: false,
        ldap_bind_user: READER_DN.to_owned(),
        ldap_bind_password: Secret::new(READER_PASSWORD),
        ldap_base_dn: BASE_DN.to_owned(),
        ldap_search_filter: "(objectClass=inetOrgPerson)".to_owned(),
        ldap_search_attributes: "uid, mail".to_owned(),
        ldap_uid_attribute: "uid".to_owned(),
        ldap_username_attribute: "uid".to_owned(),
        enable_all_folders: true,
        ..LdapConfig::default()
    }
}

/// The user manager with LDAP-Auth registered, over a fake directory.
struct Harness {
    users: Arc<FerrofinUserManager>,
    /// The bus `users` announces renames and policy saves on (no consumer
    /// until a test subscribes one).
    events: FerrofinEventManager,
    identities: Arc<FerrofinExternalIdentityStore>,
    plugins: Arc<FerrofinPluginManager>,
    directory: FakeDirectory,
    plugins_dir: tempfile::TempDir,
}

impl Harness {
    /// LDAP-Auth enabled with `config`, the directory holding Ada and Bob.
    async fn new(config: &LdapConfig) -> Self {
        let harness = Self::disabled().await;
        harness
            .plugins
            .enable_plugin(EXTENSION_ID)
            .await
            .expect("enable");
        harness.configure(config).await;
        harness
    }

    /// LDAP-Auth registered but disabled, with its seed configuration.
    async fn disabled() -> Self {
        let db = ferrofin_db::Database::connect_in_memory()
            .await
            .expect("connect");
        db.run_migrations().await.expect("migrations");
        let events = FerrofinEventManager::new();
        let users =
            Arc::new(FerrofinUserManager::new(db.clone()).with_events(Arc::new(events.clone())));
        let identities = Arc::new(FerrofinExternalIdentityStore::new(db));
        let plugins_dir = tempfile::tempdir().expect("tempdir");
        let extensions: Vec<Arc<dyn Extension>> = vec![Arc::new(LdapAuthExtension)];
        let plugins = Arc::new(FerrofinPluginManager::new(
            registered_plugins(&extensions),
            plugins_dir.path().to_path_buf(),
        ));
        let directory = FakeDirectory::default();
        {
            let mut state = directory.state();
            state.accounts = HashMap::from([
                (READER_DN.to_owned(), READER_PASSWORD.to_owned()),
                (ADA_DN.to_owned(), ADA_PASSWORD.to_owned()),
                (BOB_DN.to_owned(), BOB_PASSWORD.to_owned()),
            ]);
            state.entries = vec![ada()];
        }
        users.set_extension_providers(vec![Arc::new(LdapAuthProvider::new(
            Arc::clone(&plugins) as Arc<dyn PluginManager>,
            Arc::clone(&users) as Arc<dyn UserManager>,
            Arc::clone(&identities) as Arc<dyn ExternalIdentityStore>,
            Arc::new(directory.clone()),
        ))]);
        Self {
            users,
            events,
            identities,
            plugins,
            directory,
            plugins_dir,
        }
    }

    async fn configure(&self, config: &LdapConfig) {
        self.plugins
            .set_plugin_configuration(EXTENSION_ID, serde_json::to_vec(config).expect("json"))
            .await
            .expect("configure");
    }

    /// The search answers `entries` from now on.
    fn answer(&self, entries: Vec<LdapEntry>) {
        self.directory.state().entries = entries;
    }

    async fn sign_in(
        &self,
        name: &str,
        password: &str,
    ) -> Result<Option<UserEntity>, ServiceError> {
        self.users
            .authenticate_user(name, password, "127.0.0.1", true)
            .await
    }

    async fn user(&self, name: &str) -> Option<UserEntity> {
        self.users.get_user_by_name(name).await.expect("lookup")
    }

    async fn policy(&self, user: &UserEntity) -> UserPolicy {
        self.users
            .get_user_dto(user, None)
            .await
            .expect("dto")
            .policy
            .expect("policy")
    }

    async fn link(&self, uid: &str) -> Option<ExternalIdentity> {
        self.identities
            .find(LdapAuthProvider::identity(uid))
            .await
            .expect("find")
    }

    async fn links(&self) -> Vec<ExternalIdentity> {
        self.identities.list(PROVIDER_ID).await.expect("list")
    }

    /// A user `name` whose saved provider is `provider_id`, as a user of an
    /// adopted Jellyfin, with `password` stored locally.
    async fn existing_user(&self, name: &str, provider_id: &str, password: &str) -> UserEntity {
        let user = self.users.create_user(name).await.expect("create");
        let id = Uuid::parse_str(&user.id).expect("id");
        if !password.is_empty() {
            self.users
                .change_password(id, password)
                .await
                .expect("password");
        }
        let mut policy = self.policy(&user).await;
        provider_id.clone_into(&mut policy.authentication_provider_id);
        self.users.update_policy(id, &policy).await.expect("policy");
        self.users
            .get_user_by_id(id)
            .await
            .expect("lookup")
            .expect("user")
    }
}

#[tokio::test]
async fn a_first_sign_in_creates_the_user_with_both_provider_ids_and_the_folders() {
    let folder = Uuid::from_u128(0xf01d_e125);
    let harness = Harness::new(&LdapConfig {
        enable_all_folders: false,
        enabled_folders: vec![folder.to_string()],
        ..config()
    })
    .await;

    let user = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("signed in");

    assert_eq!(user.username, "ada");
    assert_eq!(user.authentication_provider_id, PROVIDER_ID);
    assert_eq!(user.password_reset_provider_id, PROVIDER_ID);
    assert_eq!(user.password, None, "the directory keeps the password");
    let policy = harness.policy(&user).await;
    assert!(!policy.enable_all_folders);
    assert_eq!(policy.enabled_folders, [folder]);
    assert!(!policy.is_administrator);
    assert_eq!(
        harness.link("ada").await.map(|link| link.user_id),
        Some(id_of(&user)),
        "linked by uid"
    );

    // The bind DN searches, then the entry binds on a connection of its own.
    let state = harness.directory.state();
    assert_eq!(
        state.binds,
        [
            (READER_DN.to_owned(), READER_PASSWORD.to_owned()),
            (ADA_DN.to_owned(), ADA_PASSWORD.to_owned()),
        ]
    );
    assert_eq!(
        state.searches,
        [(
            BASE_DN.to_owned(),
            SearchScope::Subtree,
            "(&(objectClass=inetOrgPerson)(|(uid=ada)(mail=ada)))".to_owned(),
            vec!["uid".to_owned(), "uid".to_owned()],
        )]
    );
    assert_eq!((state.connects, state.unbinds), (2, 2));
}

#[tokio::test]
async fn with_all_folders_the_new_user_keeps_the_empty_folder_list() {
    let harness = Harness::new(&LdapConfig {
        enable_all_folders: true,
        enabled_folders: vec![Uuid::from_u128(7).to_string()],
        ..config()
    })
    .await;
    let user = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("signed in");
    let policy = harness.policy(&user).await;
    assert!(policy.enable_all_folders);
    assert_eq!(policy.enabled_folders, Vec::<Uuid>::new());
}

#[tokio::test]
async fn a_wrong_password_is_refused_and_counts_as_a_failed_attempt() {
    let harness = Harness::new(&config()).await;
    harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");

    assert_eq!(
        harness.sign_in("ada", "wrong").await.expect("sign in"),
        None
    );
    let user = harness.user("ada").await.expect("user");
    assert_eq!(user.invalid_login_attempt_count, 1);

    // The right password signs in and clears the count.
    let user = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("signed in");
    assert_eq!(user.invalid_login_attempt_count, 0);
}

#[tokio::test]
async fn an_empty_password_never_reaches_the_directory() {
    let harness = Harness::new(&config()).await;

    // An unknown name: refused, nothing sent.
    assert!(matches!(
        harness.sign_in("ada", "").await,
        Err(ServiceError::Unauthorized(_))
    ));
    assert!(harness.directory.untouched());
    assert_eq!(harness.user("ada").await, None);

    // A directory user: a failed attempt, still nothing sent.
    harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    harness.directory.clear_calls();
    assert_eq!(harness.sign_in("ada", "").await.expect("sign in"), None);
    assert!(harness.directory.untouched(), "no bind was sent");
    let user = harness.user("ada").await.expect("user");
    assert_eq!(user.invalid_login_attempt_count, 1);
}

/// Upstream's `Bound` check, behind the guard above: an empty password binds
/// anonymously, which the directory accepts and which is no sign-in.
#[tokio::test]
async fn the_users_bind_refuses_an_empty_password_itself() {
    let harness = Harness::new(&config()).await;
    let provider = LdapAuthProvider::new(
        Arc::clone(&harness.plugins) as Arc<dyn PluginManager>,
        Arc::clone(&harness.users) as Arc<dyn UserManager>,
        Arc::clone(&harness.identities) as Arc<dyn ExternalIdentityStore>,
        Arc::new(harness.directory.clone()),
    );
    assert!(matches!(
        provider.bind_as_user(&config(), ADA_DN, "").await,
        Err(ServiceError::Unauthorized(_))
    ));
    assert!(harness.directory.untouched());
}

/// A name that is a directory user's account and finds no entry (removed
/// from the directory, or no longer matching the filter): a counted failure.
/// An unknown name that finds none has no account to count against.
#[tokio::test]
async fn no_matching_entry_counts_for_an_account_and_not_for_an_unknown_name() {
    let harness = Harness::new(&config()).await;
    harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    harness.answer(Vec::new());
    assert_eq!(
        harness.sign_in("ada", ADA_PASSWORD).await.expect("sign in"),
        None
    );
    assert_eq!(
        harness
            .user("ada")
            .await
            .expect("ada")
            .invalid_login_attempt_count,
        1
    );
    assert!(matches!(
        harness.sign_in("grace", "grace-secret").await,
        Err(ServiceError::Unauthorized(_))
    ));
}

#[tokio::test]
async fn with_creation_off_a_new_directory_user_gets_no_account() {
    const CREATION_OFF: &str = "automatic user creation is off";
    let logs = CapturedLogs::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .with_env_filter("ferrofin_extensions=debug")
            .finish(),
    );
    let harness = Harness::new(&LdapConfig {
        create_users_from_ldap: false,
        ..config()
    })
    .await;

    // A wrong password: refused at the bind, before creation is considered.
    assert!(matches!(
        harness.sign_in("ada", "wrong").await,
        Err(ServiceError::Unauthorized(_))
    ));
    assert_eq!(logs.count("WARN", CREATION_OFF), 0);

    // The right password: refused for want of an account, which is warned.
    assert!(matches!(
        harness.sign_in("ada", ADA_PASSWORD).await,
        Err(ServiceError::Unauthorized(_))
    ));
    assert_eq!(logs.count("WARN", CREATION_OFF), 1);
    assert_eq!(harness.user("ada").await, None);
    assert_eq!(harness.links().await, Vec::<ExternalIdentity>::new());
}

#[tokio::test]
async fn a_returning_user_is_found_by_uid_and_follows_a_directory_rename() {
    let harness = Harness::new(&LdapConfig {
        ldap_username_attribute: "cn".to_owned(),
        ..config()
    })
    .await;
    let first = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    assert_eq!(first.username, "Ada Lovelace");

    // Upstream's `RenameUser` publishes `UserUpdatedEventArgs` from inside the
    // plugin's `Authenticate`: the rename is announced once. Signed in by her
    // stored name, the sign-in holds her own login lock meanwhile, and a
    // consumer that reads her back through the manager, as the push and the
    // device check do, lets it finish.
    let announced = Arc::new(Mutex::new(Vec::new()));
    let (reader, sink) = (Arc::clone(&harness.users), Arc::clone(&announced));
    harness.events.subscribe(
        ferrofin_core::user_updated::USER_UPDATED_EVENT,
        Arc::new(move |payload: &str| {
            let id: Uuid = serde_json::from_str(payload).expect("a user id");
            let (reader, sink) = (Arc::clone(&reader), Arc::clone(&sink));
            Box::pin(async move {
                let user = reader.get_user_by_id(id).await?.expect("the user");
                reader.get_user_dto(&user, None).await?;
                sink.lock().unwrap().push(user.username);
                Ok(())
            })
        }),
    );

    // The directory renames her; her uid stays.
    harness.answer(vec![entry(
        ADA_DN,
        &[
            ("uid", "ada"),
            ("cn", "Ada King"),
            ("mail", "ada@example.org"),
        ],
    )]);
    let renamed = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        harness.sign_in("Ada Lovelace", ADA_PASSWORD),
    )
    .await
    .expect("the sign-in does not wait on its own announcement")
    .expect("sign in")
    .expect("signed in");
    assert_eq!(renamed.id, first.id, "the same account");
    assert_eq!(renamed.username, "Ada King");
    assert_eq!(harness.user("Ada Lovelace").await, None);
    assert_eq!(harness.links().await.len(), 1);
    assert_eq!(*announced.lock().unwrap(), ["Ada King"]);
}

#[tokio::test]
async fn a_user_saved_with_the_upstream_provider_id_is_relinked_by_name() {
    let harness = Harness::new(&config()).await;
    // An adopted Jellyfin's LDAP user, the id in another case, and no link.
    let adopted = harness
        .existing_user(
            "ada",
            "jellyfin.plugin.ldap_auth.ldapauthenticationproviderplugin",
            "",
        )
        .await;
    assert_eq!(harness.link("ada").await, None);

    let user = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("signed in");
    assert_eq!(user.id, adopted.id);
    assert_eq!(
        harness.link("ada").await.map(|link| link.user_id),
        Some(id_of(&adopted)),
        "linked now"
    );
}

#[tokio::test]
async fn a_local_account_with_the_same_name_is_never_taken_over() {
    let harness = Harness::new(&config()).await;
    let local = harness
        .existing_user(
            "ada",
            "Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider",
            "local-secret",
        )
        .await;

    // Her name selects the local provider alone: the directory is not asked.
    assert_eq!(
        harness.sign_in("ada", ADA_PASSWORD).await.expect("sign in"),
        None
    );
    assert!(harness.directory.untouched());

    // Another name for the same directory entry finds the local account by
    // name, does not take it, and cannot create a second "ada".
    assert!(matches!(
        harness.sign_in("ada@example.org", ADA_PASSWORD).await,
        Err(ServiceError::InvalidInput(_))
    ));
    let after = harness.user("ada").await.expect("user");
    assert_eq!(after.id, local.id);
    assert_eq!(
        after.authentication_provider_id,
        "Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider"
    );
    assert_eq!(harness.links().await, Vec::<ExternalIdentity>::new());
    assert!(
        harness
            .sign_in("ada", "local-secret")
            .await
            .expect("sign in")
            .is_some(),
        "the local password still works"
    );
}

#[tokio::test]
async fn a_name_that_finds_another_accounts_entry_is_refused() {
    let harness = Harness::new(&config()).await;
    let ada_user = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("ada");
    harness.answer(vec![bob()]);
    let bob_user = harness
        .sign_in("bob", BOB_PASSWORD)
        .await
        .expect("sign in")
        .expect("bob");

    // Bob's entry answers the search for "ada" (say he set his mail to it).
    harness.directory.clear_calls();
    assert_eq!(
        harness.sign_in("ada", BOB_PASSWORD).await.expect("sign in"),
        None
    );
    assert!(
        !harness
            .directory
            .state()
            .binds
            .iter()
            .any(|(dn, _)| dn == BOB_DN),
        "refused before Bob's password is tried"
    );
    let ada_after = harness.user("ada").await.expect("ada");
    assert_eq!(ada_after.id, ada_user.id);
    assert_eq!(ada_after.invalid_login_attempt_count, 1);
    assert_eq!(harness.user("bob").await.map(|u| u.id), Some(bob_user.id));
}

#[tokio::test]
async fn an_entry_without_the_uid_attribute_is_not_linked_and_found_by_name() {
    let harness = Harness::new(&LdapConfig {
        ldap_username_attribute: "cn".to_owned(),
        ..config()
    })
    .await;
    harness.answer(vec![entry(ADA_DN, &[("cn", "Ada")])]);
    let first = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    assert_eq!(
        harness.links().await,
        Vec::<ExternalIdentity>::new(),
        "nothing a lookup could find"
    );

    let again = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("signed in");
    assert_eq!(again.id, first.id, "found by name");
}

#[tokio::test]
async fn a_linked_user_whose_entry_loses_its_uid_keeps_the_link() {
    let harness = Harness::new(&LdapConfig {
        ldap_username_attribute: "cn".to_owned(),
        ..config()
    })
    .await;
    let user = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");

    // For one sign-in the directory sends no uid: she is found by name.
    harness.answer(vec![entry(ADA_DN, &[("cn", "Ada Lovelace")])]);
    let again = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("signed in");
    assert_eq!(again.id, user.id);
    assert_eq!(
        harness.link("ada").await.map(|link| link.user_id),
        Some(id_of(&user)),
        "her uid link is untouched"
    );
    assert_eq!(harness.links().await.len(), 1);
}

#[tokio::test]
async fn an_entry_without_the_username_attribute_is_a_bad_request() {
    // A new user: there is no name to create them under.
    let harness = Harness::new(&LdapConfig {
        ldap_username_attribute: "cn".to_owned(),
        ..config()
    })
    .await;
    harness.answer(vec![entry(ADA_DN, &[("uid", "ada")])]);
    assert!(matches!(
        harness.sign_in("ada", ADA_PASSWORD).await,
        Err(ServiceError::InvalidInput(_))
    ));
    assert_eq!(harness.users.get_users().await.expect("users").len(), 0);

    // A linked user: there is no name to follow.
    harness.answer(vec![ada()]);
    let user = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    harness.answer(vec![entry(ADA_DN, &[("uid", "ada")])]);
    assert!(matches!(
        harness.sign_in("ada", ADA_PASSWORD).await,
        Err(ServiceError::InvalidInput(_))
    ));
    assert_eq!(
        harness.user("Ada Lovelace").await.map(|u| u.id),
        Some(user.id),
        "not renamed"
    );
}

/// A directory that refuses the user's bind with any of the codes that
/// refuse credentials: a counted failure.
#[rstest]
#[case::constraint_violation(19)]
#[case::inappropriate_authentication(48)]
#[case::invalid_credentials(49)]
#[case::insufficient_access(50)]
#[case::unwilling_to_perform(53)]
#[tokio::test]
async fn a_refused_bind_is_a_failed_attempt(#[case] code: u32) {
    let harness = Harness::new(&config()).await;
    harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    harness.directory.state().bind.insert(
        ADA_DN.to_owned(),
        LdapError::Result {
            code,
            message: String::new(),
        },
    );
    assert_eq!(
        harness.sign_in("ada", ADA_PASSWORD).await.expect("sign in"),
        None
    );
    assert_eq!(
        harness
            .user("ada")
            .await
            .expect("ada")
            .invalid_login_attempt_count,
        1
    );
}

#[rstest]
#[case::cannot_connect(
    |state: &mut State| state.connect = Some(LdapError::Connect("connection refused".to_owned()))
)]
#[case::search_times_out(|state: &mut State| state.search = Some(LdapError::Timeout))]
#[case::search_fails(|state: &mut State| state.search = Some(LdapError::Result {
    code: 32,
    message: String::new(),
}))]
#[case::bind_dn_refused(
    |state: &mut State| { state.bind.insert(READER_DN.to_owned(), invalid_credentials()); }
)]
#[case::busy_at_the_users_bind(|state: &mut State| {
    state.bind.insert(ADA_DN.to_owned(), LdapError::Result { code: 51, message: String::new() });
})]
#[case::unavailable_at_the_users_bind(|state: &mut State| {
    state.bind.insert(ADA_DN.to_owned(), LdapError::Result { code: 52, message: String::new() });
})]
#[case::operations_error_at_the_users_bind(|state: &mut State| {
    state.bind.insert(ADA_DN.to_owned(), LdapError::Result { code: 1, message: String::new() });
})]
#[case::other_at_the_users_bind(|state: &mut State| {
    state.bind.insert(ADA_DN.to_owned(), LdapError::Result { code: 80, message: String::new() });
})]
#[case::connection_lost_at_the_users_bind(|state: &mut State| {
    state.bind.insert(ADA_DN.to_owned(), LdapError::Connection("reset".to_owned()));
})]
#[case::only_referrals(|state: &mut State| {
    state.entries.clear();
    state.references = vec!["ldap://elsewhere.example/dc=example,dc=org".to_owned()];
})]
#[tokio::test]
async fn a_directory_that_cannot_answer_is_a_server_error_and_no_failed_attempt(
    #[case] outage: fn(&mut State),
) {
    let harness = Harness::new(&config()).await;
    harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    let local = harness
        .existing_user(
            "carol",
            "Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider",
            "carol-secret",
        )
        .await;

    outage(&mut harness.directory.state());
    assert!(
        matches!(
            harness.sign_in("ada", ADA_PASSWORD).await,
            Err(ServiceError::Backend(_))
        ),
        "a server error"
    );
    let ada = harness.user("ada").await.expect("ada");
    assert_eq!(ada.invalid_login_attempt_count, 0, "not counted (D4)");

    // A local account signs in without the directory.
    harness.directory.clear_calls();
    let signed_in = harness
        .sign_in("carol", "carol-secret")
        .await
        .expect("sign in")
        .expect("carol");
    assert_eq!(signed_in.id, local.id);
    assert!(harness.directory.untouched());
}

#[tokio::test]
async fn the_first_of_several_matches_is_used() {
    let harness = Harness::new(&config()).await;
    harness.answer(vec![ada(), bob()]);
    let user = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("signed in");
    assert_eq!(user.username, "ada");
}

#[tokio::test]
async fn the_provider_is_listed_only_while_enabled_with_a_server() {
    let harness = Harness::disabled().await;
    let names = || async {
        harness
            .users
            .get_authentication_providers()
            .await
            .expect("list")
            .into_iter()
            .map(|pair| (pair.name.unwrap_or_default(), pair.id.unwrap_or_default()))
            .collect::<Vec<_>>()
    };
    let default = (
        "Default".to_owned(),
        "Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider".to_owned(),
    );
    assert_eq!(
        names().await,
        std::slice::from_ref(&default),
        "disabled by default"
    );

    // Enabled, but its seed configuration names no server (D3).
    harness
        .plugins
        .enable_plugin(EXTENSION_ID)
        .await
        .expect("enable");
    assert_eq!(
        names().await,
        std::slice::from_ref(&default),
        "no server yet"
    );
    assert!(matches!(
        harness.sign_in("ada", ADA_PASSWORD).await,
        Err(ServiceError::Unauthorized(_))
    ));
    assert!(harness.directory.untouched(), "no network call");

    harness.configure(&config()).await;
    assert_eq!(
        names().await,
        [
            default.clone(),
            (PROVIDER_NAME.to_owned(), PROVIDER_ID.to_owned())
        ]
    );

    harness
        .plugins
        .disable_plugin(EXTENSION_ID)
        .await
        .expect("disable");
    assert_eq!(names().await, [default]);
}

#[tokio::test]
async fn a_configuration_that_cannot_be_read_turns_sign_in_off() {
    let harness = Harness::new(&config()).await;
    // Only a file written outside the API can hold one: a save is checked
    // against the schema.
    std::fs::write(
        harness
            .plugins_dir
            .path()
            .join(EXTENSION_ID.to_string())
            .join("config.json"),
        br#"{"LdapServer": 5}"#,
    )
    .expect("write");
    assert!(matches!(
        harness.sign_in("ada", ADA_PASSWORD).await,
        Err(ServiceError::Unauthorized(_))
    ));
    assert!(harness.directory.untouched());
    let providers = harness
        .users
        .get_authentication_providers()
        .await
        .expect("list");
    assert_eq!(providers.len(), 1, "{providers:?}");
}

/// An in-memory sink for a scoped `fmt` subscriber.
#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
    /// How many lines at `level` (`"WARN"`, `"DEBUG"`) contain `text`.
    /// Whether any line, at any level, contains `text`.
    fn contains(&self, text: &str) -> bool {
        String::from_utf8_lossy(&self.0.lock().expect("logs")).contains(text)
    }

    fn count(&self, level: &str, text: &str) -> usize {
        String::from_utf8_lossy(&self.0.lock().expect("logs"))
            .lines()
            .filter(|line| line.split_whitespace().nth(1) == Some(level) && line.contains(text))
            .count()
    }
}

impl std::io::Write for CapturedLogs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("logs").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// A configuration that cannot be read is checked at every unknown-name
/// sign-in and providers listing: it warns once, then logs at `debug`
/// until it parses again; breaking again warns again.
#[tokio::test]
async fn an_unreadable_configuration_warns_once_until_it_is_fixed() {
    const UNREADABLE: &str = "LDAP-Auth configuration cannot be read";
    let logs = CapturedLogs::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .with_env_filter("ferrofin_extensions=debug")
            .finish(),
    );
    let harness = Harness::new(&config()).await;
    let config_file = harness
        .plugins_dir
        .path()
        .join(EXTENSION_ID.to_string())
        .join("config.json");
    let list = || async {
        harness
            .users
            .get_authentication_providers()
            .await
            .expect("list")
            .len()
    };
    // A bind password stored as a number: serde's message would quote it.
    std::fs::write(&config_file, br#"{"LdapBindPassword": 9876543210}"#).expect("write");
    for _ in 0..3 {
        assert_eq!(list().await, 1);
    }
    assert_eq!(logs.count("WARN", UNREADABLE), 1);
    assert_eq!(logs.count("DEBUG", UNREADABLE), 2);

    harness.configure(&config()).await;
    assert_eq!(list().await, 2, "fixed");
    std::fs::write(&config_file, br#"{"LdapServer": 5}"#).expect("write");
    assert_eq!(list().await, 1);
    assert_eq!(logs.count("WARN", UNREADABLE), 2, "broken again");
    assert!(
        !logs.contains("9876543210"),
        "the stored value is not logged"
    );
}

#[tokio::test]
async fn a_password_change_is_refused_while_allow_pass_change_is_off() {
    let harness = Harness::new(&config()).await;
    let user = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    let id = Uuid::parse_str(&user.id).expect("id");
    assert!(matches!(
        harness.users.change_password(id, "new-secret").await,
        Err(ServiceError::Backend(_))
    ));
    assert_eq!(harness.user("ada").await.expect("user").password, None);
}

#[rstest]
#[case::backslash("a\\b", "a\\5cb")]
#[case::asterisk("a*b", "a\\2ab")]
#[case::open_parenthesis("a(b", "a\\28b")]
#[case::close_parenthesis("a)b", "a\\29b")]
#[case::nul("a\0b", "a\\00b")]
#[case::plain_text("Ada Lovelace-é.ü@example.org", "Ada Lovelace-é.ü@example.org")]
#[case::every_one(
    "*)(uid=*))(|(uid=*\\",
    "\\2a\\29\\28uid=\\2a\\29\\29\\28|\\28uid=\\2a\\5c"
)]
fn sanitize_filter_escapes_as_upstream(#[case] input: &str, #[case] expected: &str) {
    assert_eq!(sanitize_filter(input), expected);
}

#[rstest]
#[case::attributes(
    "(memberOf=CN=JellyfinUsers,DC=contoso,DC=com)",
    "uid, cn, mail, displayName",
    "ada",
    "(&(memberOf=CN=JellyfinUsers,DC=contoso,DC=com)(|(uid=ada)(cn=ada)(mail=ada)(displayName=ada)))"
)]
#[case::escaped_name(
    "(objectClass=*)",
    "uid",
    "a*)",
    "(&(objectClass=*)(|(uid=a\\2a\\29)))"
)]
#[case::empty_attribute_kept("(o=x)", "uid,", "ada", "(&(o=x)(|(uid=ada)(=ada)))")]
#[case::placeholder(
    "(&(objectClass=person)(sAMAccountName={username}))",
    "uid",
    "ada",
    "(&(objectClass=person)(sAMAccountName=ada))"
)]
#[case::placeholder_any_case_every_one(
    "(|(uid={USERNAME})(mail={UserName}@example.org))",
    "uid",
    "a(b",
    "(|(uid=a\\28b)(mail=a\\28b@example.org))"
)]
fn the_user_search_filter_is_built_as_upstream_builds_it(
    #[case] filter: &str,
    #[case] attributes: &str,
    #[case] username: &str,
    #[case] expected: &str,
) {
    let config = LdapConfig {
        ldap_search_filter: filter.to_owned(),
        ldap_search_attributes: attributes.to_owned(),
        ..LdapConfig::default()
    };
    assert_eq!(user_search_filter(&config, username), expected);
}

mod admin_filter_tests;
mod password_tests;
