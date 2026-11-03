//! The admin filter at sign-in: the admin block of upstream's `Authenticate`
//! (jellyfin-plugin-ldapauth v24, `d11c86a`), over the fake directory and the
//! real user manager, which applies the flag the provider reports and never
//! demotes the last administrator (D5).

use super::*;

/// An admin filter that lists the administrators' own entries (DN-match mode).
const ADMIN_FILTER: &str = "(employeeType=jellyfin-admin)";
const GROUPS_DN: &str = "ou=groups,dc=example,dc=org";
const ADMINS_DN: &str = "cn=jellyfin-admins,ou=groups,dc=example,dc=org";
const LOCAL_PROVIDER: &str = "Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider";

/// [`config`] with the admin filter `filter` on, in DN-match mode.
fn with_admin_filter(filter: &str) -> LdapConfig {
    LdapConfig {
        ldap_admin_filter: filter.to_owned(),
        ..config()
    }
}

/// A group entry, which only member-uid mode counts.
fn admins_group() -> LdapEntry {
    entry(ADMINS_DN, &[("cn", "jellyfin-admins")])
}

impl Harness {
    /// A search with `filter` finds `entries` from now on.
    fn finds(&self, filter: &str, entries: Vec<LdapEntry>) {
        self.directory.state().by_filter.insert(
            filter.to_owned(),
            SearchOutcome {
                entries,
                references: Vec::new(),
            },
        );
    }

    async fn is_administrator(&self, name: &str) -> bool {
        let user = self.user(name).await.expect("user");
        self.policy(&user).await.is_administrator
    }

    /// Sets `user`'s administrator flag as an administrator would.
    async fn set_administrator(&self, user: &UserEntity, administrator: bool) {
        let mut policy = self.policy(user).await;
        policy.is_administrator = administrator;
        self.users
            .update_policy(id_of(user), &policy)
            .await
            .expect("policy");
    }

    /// A local administrator, so a directory user is not the last one.
    async fn local_administrator(&self) -> UserEntity {
        let root = self
            .existing_user("root", LOCAL_PROVIDER, "root-secret")
            .await;
        self.set_administrator(&root, true).await;
        root
    }
}

#[tokio::test]
async fn a_new_user_the_admin_filter_finds_is_created_an_administrator() {
    let harness = Harness::new(&with_admin_filter(ADMIN_FILTER)).await;
    harness.finds(ADMIN_FILTER, vec![ada()]);

    harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");

    assert!(harness.is_administrator("ada").await);
    // The bind DN searches again on a connection of its own, after the
    // password was checked, asking for no attributes.
    let state = harness.directory.state();
    assert_eq!(
        state.binds,
        [
            (READER_DN.to_owned(), READER_PASSWORD.to_owned()),
            (ADA_DN.to_owned(), ADA_PASSWORD.to_owned()),
            (READER_DN.to_owned(), READER_PASSWORD.to_owned()),
        ]
    );
    assert_eq!(
        state.searches[1],
        (
            BASE_DN.to_owned(),
            SearchScope::Subtree,
            ADMIN_FILTER.to_owned(),
            vec!["1.1".to_owned()],
        ),
        "the base DN while LdapAdminBaseDn is empty"
    );
    assert_eq!((state.connects, state.unbinds), (3, 3));
}

#[tokio::test]
async fn a_refused_password_never_reaches_the_admin_search() {
    let harness = Harness::new(&with_admin_filter(ADMIN_FILTER)).await;
    harness.finds(ADMIN_FILTER, vec![ada()]);
    assert!(matches!(
        harness.sign_in("ada", "wrong").await,
        Err(ServiceError::Unauthorized(_))
    ));
    let state = harness.directory.state();
    assert_eq!(state.searches.len(), 1, "the user search alone");
    assert_eq!(state.binds.len(), 2, "the bind DN, then the refused bind");
}

#[tokio::test]
async fn with_creation_off_the_admin_search_runs_before_the_refusal() {
    // Upstream's order: the admin block comes before `CreateUsersFromLdap`.
    let harness = Harness::new(&LdapConfig {
        create_users_from_ldap: false,
        ..with_admin_filter(ADMIN_FILTER)
    })
    .await;
    harness.finds(ADMIN_FILTER, vec![ada()]);
    assert!(matches!(
        harness.sign_in("ada", ADA_PASSWORD).await,
        Err(ServiceError::Unauthorized(_))
    ));
    assert_eq!(harness.user("ada").await, None);
    assert_eq!(harness.directory.state().searches[1].2, ADMIN_FILTER);

    // So a failing admin search is what the user then sees.
    harness.directory.state().failing_filters.insert(
        ADMIN_FILTER.to_owned(),
        LdapError::Result {
            code: 32,
            message: String::new(),
        },
    );
    assert!(matches!(
        harness.sign_in("ada", ADA_PASSWORD).await,
        Err(ServiceError::Backend(_))
    ));
}

#[tokio::test]
async fn a_new_user_whose_entry_it_does_not_find_is_created_without_the_flag() {
    let harness = Harness::new(&with_admin_filter(ADMIN_FILTER)).await;
    // DN-match mode: another entry, or a group, does not count, nor does the
    // user's own DN spelled in another case (an ordinal compare).
    harness.finds(
        ADMIN_FILTER,
        vec![
            bob(),
            admins_group(),
            entry("UID=ada,ou=people,dc=example,dc=org", &[]),
        ],
    );
    harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    assert!(!harness.is_administrator("ada").await);
}

#[tokio::test]
async fn the_flag_follows_the_directory_at_each_sign_in_while_another_administrator_exists() {
    let harness = Harness::new(&with_admin_filter(ADMIN_FILTER)).await;
    harness.local_administrator().await;
    harness.finds(ADMIN_FILTER, Vec::new());
    harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    assert!(!harness.is_administrator("ada").await);

    harness.finds(ADMIN_FILTER, vec![ada()]);
    harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("signed in");
    assert!(harness.is_administrator("ada").await, "added to the group");

    harness.finds(ADMIN_FILTER, Vec::new());
    harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("signed in");
    assert!(
        !harness.is_administrator("ada").await,
        "removed from the group"
    );
    assert!(harness.is_administrator("root").await);
}

#[tokio::test]
async fn the_last_administrator_removed_from_the_group_stays_one_and_signs_in() {
    let harness = Harness::new(&with_admin_filter(ADMIN_FILTER)).await;
    harness.finds(ADMIN_FILTER, vec![ada()]);
    harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    assert!(harness.is_administrator("ada").await);

    harness.finds(ADMIN_FILTER, Vec::new());
    let user = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("signed in");
    assert_eq!(user.username, "ada");
    assert!(
        harness.is_administrator("ada").await,
        "the only administrator is never demoted (D5)"
    );
}

#[tokio::test]
async fn in_member_uid_mode_any_entry_found_makes_an_administrator() {
    let filter = "(&(objectClass=posixGroup)(memberUid={username}))";
    let harness = Harness::new(&LdapConfig {
        enable_ldap_admin_filter_member_uid: true,
        ldap_admin_base_dn: GROUPS_DN.to_owned(),
        ..with_admin_filter(filter)
    })
    .await;
    harness.local_administrator().await;
    let searched = "(&(objectClass=posixGroup)(memberUid=ada))";
    harness.finds(searched, vec![admins_group()]);
    harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    assert!(harness.is_administrator("ada").await);
    assert_eq!(
        harness.directory.state().searches[1],
        (
            GROUPS_DN.to_owned(),
            SearchScope::Subtree,
            searched.to_owned(),
            vec!["1.1".to_owned()],
        ),
        "LdapAdminBaseDn, with the entry's username in the filter"
    );

    harness.finds(searched, Vec::new());
    harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("signed in");
    assert!(!harness.is_administrator("ada").await);
}

#[rstest]
#[case::empty("")]
#[case::disabled("_disabled_")]
#[tokio::test]
async fn with_the_filter_off_there_is_no_search_and_the_flag_stays(#[case] filter: &str) {
    let harness = Harness::new(&with_admin_filter(filter)).await;
    let ada = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    assert!(!harness.is_administrator("ada").await);
    harness.set_administrator(&ada, true).await;
    harness.directory.clear_calls();

    harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("signed in");

    assert!(
        harness.is_administrator("ada").await,
        "an administrator made by hand stays one"
    );
    let state = harness.directory.state();
    assert_eq!(state.searches.len(), 1, "the user search alone");
    assert_eq!(state.connects, 2);
}

#[rstest]
#[case::no_placeholder("(cn=admins)", "ada", "(cn=admins)")]
#[case::placeholder_any_case_every_one(
    "(|(memberUid={username})(member=uid={USERNAME},ou=people,dc=example,dc=org))",
    "ada",
    "(|(memberUid=ada)(member=uid=ada,ou=people,dc=example,dc=org))"
)]
#[case::escaped("(memberUid={username})", "a*(b)\\", "(memberUid=a\\2a\\28b\\29\\5c)")]
fn the_admin_filter_is_built_as_upstream_builds_it(
    #[case] filter: &str,
    #[case] ldap_username: &str,
    #[case] expected: &str,
) {
    assert_eq!(
        admin_search_filter(&with_admin_filter(filter), ldap_username),
        expected
    );
}

#[tokio::test]
async fn a_failed_admin_search_is_a_server_error_and_no_failed_attempt() {
    let harness = Harness::new(&with_admin_filter(ADMIN_FILTER)).await;
    harness.directory.state().failing_filters.insert(
        ADMIN_FILTER.to_owned(),
        LdapError::Result {
            code: 32,
            message: "no such object".to_owned(),
        },
    );
    assert!(matches!(
        harness.sign_in("ada", ADA_PASSWORD).await,
        Err(ServiceError::Backend(_))
    ));
    assert_eq!(harness.user("ada").await, None, "no account was created");

    harness.directory.state().failing_filters.clear();
    harness.finds(ADMIN_FILTER, Vec::new());
    harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    harness
        .directory
        .state()
        .failing_filters
        .insert(ADMIN_FILTER.to_owned(), LdapError::Timeout);
    assert!(matches!(
        harness.sign_in("ada", ADA_PASSWORD).await,
        Err(ServiceError::Backend(_))
    ));
    assert_eq!(
        harness
            .user("ada")
            .await
            .expect("ada")
            .invalid_login_attempt_count,
        0,
        "the password was right (D4)"
    );
}

#[tokio::test]
async fn an_entry_without_the_username_attribute_is_a_bad_request_while_the_filter_is_on() {
    let harness = Harness::new(&LdapConfig {
        ldap_username_attribute: "cn".to_owned(),
        ..with_admin_filter("(memberUid={username})")
    })
    .await;
    harness.answer(vec![entry(ADA_DN, &[("uid", "ada")])]);
    assert!(matches!(
        harness.sign_in("ada", ADA_PASSWORD).await,
        Err(ServiceError::InvalidInput(_))
    ));
    assert_eq!(
        harness.directory.state().searches.len(),
        1,
        "no admin search"
    );
}

/// An administrator whose entry only a referred server holds is found there:
/// the admin search follows the reference as the bind DN, as upstream's
/// `LdapAuthHandler` binds.
#[tokio::test]
async fn the_admin_search_follows_references_with_the_bind_credentials() {
    let harness = Harness::new(&with_admin_filter(ADMIN_FILTER)).await;
    {
        let mut state = harness.directory.state();
        state.by_filter.insert(
            ADMIN_FILTER.to_owned(),
            SearchOutcome {
                entries: Vec::new(),
                references: vec![vec![
                    "ldap://admins.example/ou=people,dc=example,dc=org??sub".to_owned(),
                ]],
            },
        );
        state.by_server.insert(
            "admins.example".to_owned(),
            SearchOutcome {
                entries: vec![ada()],
                references: Vec::new(),
            },
        );
    }
    harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    assert!(harness.is_administrator("ada").await);
    let state = harness.directory.state();
    assert_eq!(
        state.binds_at.last(),
        Some(&("admins.example".to_owned(), READER_DN.to_owned())),
        "bound at the referred server as the bind DN"
    );
    assert_eq!(
        state.searches.last(),
        Some(&(
            "ou=people,dc=example,dc=org".to_owned(),
            SearchScope::Subtree,
            ADMIN_FILTER.to_owned(),
            vec!["1.1".to_owned()],
        )),
        "the URL's base, the admin filter's own filter and attributes"
    );
}
