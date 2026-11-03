//! Password changes (`ChangePassword`) and forgotten passwords
//! (`StartForgotPasswordProcess`, `RedeemPasswordResetPin`) for LDAP users
//! (jellyfin-plugin-ldapauth v24, `d11c86a`), over the fake directory and the
//! real user manager, which sends a change to the user's authentication
//! provider and a forgotten-password request to their reset provider.

use super::*;

const NEW_PASSWORD: &str = "ada-new-secret";

/// [`config`] with password changes allowed.
fn changes_allowed() -> LdapConfig {
    LdapConfig {
        allow_pass_change: true,
        ..config()
    }
}

impl Harness {
    /// Signs Ada in once, so her account exists and LDAP-Auth owns it, then
    /// forgets the calls that made.
    async fn ada(&self) -> Uuid {
        let user = self
            .sign_in("ada", ADA_PASSWORD)
            .await
            .expect("sign in")
            .expect("created");
        self.directory.clear_calls();
        id_of(&user)
    }

    /// A provider over this harness, as the extension builds one.
    fn provider(&self) -> LdapAuthProvider {
        LdapAuthProvider::new(
            Arc::clone(&self.plugins) as Arc<dyn PluginManager>,
            Arc::clone(&self.users) as Arc<dyn UserManager>,
            Arc::clone(&self.identities) as Arc<dyn ExternalIdentityStore>,
            Arc::new(self.directory.clone()),
        )
    }

    /// Registers LDAP-Auth's password-reset provider, as boot does.
    fn with_reset_provider(&self) {
        self.users
            .set_extension_reset_providers(vec![Arc::new(self.provider())]);
    }

    /// Whether `password` signs Ada in.
    async fn ada_signs_in_with(&self, password: &str) -> bool {
        self.sign_in("ada", password)
            .await
            .expect("sign in")
            .is_some()
    }
}

#[tokio::test]
async fn a_server_with_the_password_modify_operation_takes_the_new_password_through_it() {
    let harness = Harness::new(&changes_allowed()).await;
    harness.directory.state().extensions = vec![PASSWORD_MODIFY_OID.to_owned()];
    let ada = harness.ada().await;

    harness
        .users
        .change_password(ada, NEW_PASSWORD)
        .await
        .expect("changed");

    {
        let state = harness.directory.state();
        assert_eq!(
            state.writes,
            [(
                ADA_DN.to_owned(),
                "passwordModify".to_owned(),
                NEW_PASSWORD.to_owned()
            )]
        );
        // The entry is found as sign-in finds it, then the bind DN writes on a
        // connection of its own.
        assert_eq!(
            state.binds,
            [
                (READER_DN.to_owned(), READER_PASSWORD.to_owned()),
                (READER_DN.to_owned(), READER_PASSWORD.to_owned()),
            ]
        );
        assert_eq!(
            state.searches[0].2,
            "(&(objectClass=inetOrgPerson)(|(uid=ada)(mail=ada)))"
        );
        assert_eq!((state.connects, state.unbinds), (2, 2));
    }
    assert!(harness.ada_signs_in_with(NEW_PASSWORD).await);
    assert!(!harness.ada_signs_in_with(ADA_PASSWORD).await);
}

#[tokio::test]
async fn a_server_without_it_gets_the_password_attribute_replaced() {
    let harness = Harness::new(&changes_allowed()).await;
    let ada = harness.ada().await;

    harness
        .users
        .change_password(ada, NEW_PASSWORD)
        .await
        .expect("changed");

    assert_eq!(
        harness.directory.state().writes,
        [(
            ADA_DN.to_owned(),
            "userPassword".to_owned(),
            NEW_PASSWORD.to_owned()
        )],
        "upstream's default LdapPasswordAttribute"
    );
    assert!(harness.ada_signs_in_with(NEW_PASSWORD).await);
}

#[tokio::test]
async fn with_no_password_attribute_either_the_change_fails_and_nothing_is_written() {
    let harness = Harness::new(&LdapConfig {
        ldap_password_attribute: String::new(),
        ..changes_allowed()
    })
    .await;
    let ada = harness.ada().await;

    match harness.users.change_password(ada, NEW_PASSWORD).await {
        Err(ServiceError::Backend(message)) => {
            assert_eq!(message, "Password attribute is not set");
        }
        other => panic!("expected upstream's InvalidOperationException, got {other:?}"),
    }
    assert_eq!(
        harness.directory.state().writes,
        Vec::<(String, String, String)>::new()
    );
    assert!(harness.ada_signs_in_with(ADA_PASSWORD).await);
}

#[tokio::test]
async fn with_changes_turned_off_nothing_reaches_the_directory() {
    let harness = Harness::new(&config()).await;
    let ada = harness.ada().await;
    assert!(matches!(
        harness.users.change_password(ada, NEW_PASSWORD).await,
        Err(ServiceError::Backend(_))
    ));
    assert!(harness.directory.untouched());
}

#[tokio::test]
async fn a_blank_password_or_a_reset_is_refused_before_the_directory() {
    // D11: upstream sends it, and the directory then sets a random password.
    let harness = Harness::new(&changes_allowed()).await;
    harness.directory.state().extensions = vec![PASSWORD_MODIFY_OID.to_owned()];
    let ada = harness.ada().await;

    for blank in ["", "   "] {
        assert!(
            matches!(
                harness.users.change_password(ada, blank).await,
                Err(ServiceError::InvalidInput(_))
            ),
            "{blank:?}"
        );
    }
    assert!(matches!(
        harness.users.reset_password(ada).await,
        Err(ServiceError::InvalidInput(_))
    ));
    assert!(harness.directory.untouched(), "no connection, no write");
    assert!(harness.ada_signs_in_with(ADA_PASSWORD).await);
}

#[tokio::test]
async fn a_user_the_directory_does_not_find_is_unauthorized() {
    let harness = Harness::new(&changes_allowed()).await;
    let ada = harness.ada().await;
    harness.answer(Vec::new());
    assert!(matches!(
        harness.users.change_password(ada, NEW_PASSWORD).await,
        Err(ServiceError::Unauthorized(_))
    ));
    assert_eq!(
        harness.directory.state().writes,
        Vec::<(String, String, String)>::new()
    );
}

#[rstest]
#[case::cannot_connect(|state: &mut State| {
    state.connect = Some(LdapError::Connect("connection refused".to_owned()));
})]
#[case::bind_dn_refused(|state: &mut State| {
    state.bind.insert(READER_DN.to_owned(), invalid_credentials());
})]
#[case::write_refused(|state: &mut State| {
    state.write = Some(LdapError::Result {
        code: 50,
        message: "no write access".to_owned(),
    });
})]
#[tokio::test]
async fn a_directory_that_fails_the_change_is_a_server_error(#[case] failure: fn(&mut State)) {
    let harness = Harness::new(&changes_allowed()).await;
    let ada = harness.ada().await;
    failure(&mut harness.directory.state());
    assert!(matches!(
        harness.users.change_password(ada, NEW_PASSWORD).await,
        Err(ServiceError::Backend(_))
    ));
    harness.directory.state().connect = None;
    harness.directory.state().bind.clear();
    assert!(harness.ada_signs_in_with(ADA_PASSWORD).await, "unchanged");
}

#[tokio::test]
async fn a_forgotten_password_is_sent_to_the_reset_page_with_both_placeholders_filled() {
    let harness = Harness::new(&LdapConfig {
        password_reset_url: Some(
            "https://id.example/reset?user=$UserName&id=$userid&again=$USERNAME".to_owned(),
        ),
        ..config()
    })
    .await;
    let ada = harness.ada().await;
    harness.with_reset_provider();

    let result = harness
        .users
        .start_forgot_password_process("ADA", false)
        .await
        .expect("started");

    assert_eq!(
        result,
        ForgotPasswordResult {
            action: ForgotPasswordAction::PinCode,
            pin_file: Some(format!(
                "https://id.example/reset?user=ada&id={ada}&again=ada"
            )),
            pin_expiration_date: None,
        },
        "the entered name lowercased, the id in .NET's lowercase form, from outside the network too"
    );
    assert!(harness.directory.untouched());
}

#[tokio::test]
async fn without_a_reset_page_a_forgotten_password_fails_as_upstream() {
    let harness = Harness::new(&config()).await;
    harness.ada().await;
    harness.with_reset_provider();
    assert!(matches!(
        harness
            .users
            .start_forgot_password_process("ada", true)
            .await,
        Err(ServiceError::Backend(_))
    ));
}

#[tokio::test]
async fn a_name_with_no_account_is_identified_by_the_md5_of_the_name() {
    let harness = Harness::new(&LdapConfig {
        password_reset_url: Some("https://id.example/$userId/$userName".to_owned()),
        ..config()
    })
    .await;
    let result = harness
        .provider()
        .start_forgot_password_process(None, "Bob", true)
        .await
        .expect("started");
    assert_eq!(
        result.pin_file,
        Some(format!("https://id.example/{}/bob", get_md5("Bob")))
    );
}

#[tokio::test]
async fn the_provider_redeems_no_pin() {
    let harness = Harness::new(&config()).await;
    assert_eq!(
        harness
            .provider()
            .redeem_password_reset_pin("1234")
            .await
            .expect("no error"),
        PinRedeemResult::default()
    );
}

#[tokio::test]
async fn the_reset_provider_is_listed_only_while_the_plugin_is_usable() {
    let listed = |pairs: Vec<ferrofin_model::dto::NameIdPair>| {
        pairs
            .into_iter()
            .any(|pair| pair.id.as_deref() == Some(PROVIDER_ID))
    };
    let enabled = Harness::new(&config()).await;
    enabled.with_reset_provider();
    assert!(listed(
        enabled
            .users
            .get_password_reset_providers()
            .await
            .expect("providers")
    ));

    let disabled = Harness::disabled().await;
    disabled.with_reset_provider();
    assert!(!listed(
        disabled
            .users
            .get_password_reset_providers()
            .await
            .expect("providers")
    ));
}

#[rstest]
#[case::both("https://x/?u=$userName&i=$userId", "Ada", "https://x/?u=ada&i={id}")]
#[case::any_case_every_one("$USERID/$UserId/$username", "Ada", "{id}/{id}/ada")]
#[case::unencoded("https://x/?u=$userName", "A&B #1", "https://x/?u=a&b #1")]
#[case::invariant_lowercase("https://x/$userName", "ÄDA", "https://x/äda")]
#[case::no_placeholder("https://x/reset", "Ada", "https://x/reset")]
fn the_reset_page_is_filled_in_as_upstream_fills_it(
    #[case] template: &str,
    #[case] name: &str,
    #[case] expected: &str,
) {
    let id = Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
    assert_eq!(
        password_reset_url(template, id, name),
        expected.replace("{id}", "01234567-89ab-cdef-0123-456789abcdef")
    );
}

#[tokio::test]
async fn a_password_change_writes_only_to_the_users_own_entry() {
    let harness = Harness::new(&changes_allowed()).await;
    let ada = harness.ada().await;
    // Bob has an account linked to his entry, and Ada's name now finds it.
    harness.answer(vec![bob()]);
    harness
        .sign_in("bob", BOB_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    harness.directory.clear_calls();

    assert!(matches!(
        harness.users.change_password(ada, NEW_PASSWORD).await,
        Err(ServiceError::Unauthorized(_))
    ));
    assert_eq!(
        harness.directory.state().writes,
        Vec::<(String, String, String)>::new()
    );
}

#[tokio::test]
async fn a_write_connection_that_cannot_be_opened_is_a_server_error() {
    let harness = Harness::new(&changes_allowed()).await;
    let ada = harness.ada().await;
    // The search's connection opens; the write's does not.
    harness.directory.state().refuse_connections_after = Some(1);
    match harness.users.change_password(ada, NEW_PASSWORD).await {
        Err(ServiceError::Backend(message)) => assert!(
            message.starts_with("LDAP password change could not connect"),
            "{message}"
        ),
        other => panic!("expected a server error, got {other:?}"),
    }
    assert_eq!(harness.directory.state().connects, 2);
}

#[rstest]
#[case::bind_dn_refused(
    |state: &mut State| { state.bind.insert(READER_DN.to_owned(), invalid_credentials()); },
    "bind as the configured bind DN"
)]
#[case::root_dse_unreadable(
    |state: &mut State| state.root_dse = Some(LdapError::Timeout),
    "read the root DSE"
)]
#[case::password_modify_refused(
    |state: &mut State| {
        state.extensions = vec![PASSWORD_MODIFY_OID.to_owned()];
        state.write = Some(LdapError::Result { code: 50, message: String::new() });
    },
    "set the password"
)]
#[tokio::test]
async fn each_write_step_fails_with_its_own_server_error(
    #[case] failure: fn(&mut State),
    #[case] step: &str,
) {
    let directory = FakeDirectory::default();
    directory.state().accounts =
        HashMap::from([(READER_DN.to_owned(), READER_PASSWORD.to_owned())]);
    failure(&mut directory.state());
    let mut connection = directory.clone();

    match set_password(&mut connection, &changes_allowed(), ADA_DN, NEW_PASSWORD).await {
        Err(ServiceError::Backend(message)) => assert!(
            message.starts_with(&format!("LDAP password change could not {step}")),
            "{message}"
        ),
        other => panic!("expected a server error, got {other:?}"),
    }
    // A refused password modify operation does not fall back to the
    // attribute, as upstream's exception does not.
    assert!(
        directory
            .state()
            .writes
            .iter()
            .all(|(_, how, _)| how == "passwordModify")
    );
}

#[tokio::test]
async fn a_linked_user_whose_entry_names_them_otherwise_still_changes_their_password() {
    // Named by cn: Ada's account is "Ada Lovelace", linked by her uid.
    let harness = Harness::new(&LdapConfig {
        ldap_username_attribute: "cn".to_owned(),
        ..changes_allowed()
    })
    .await;
    let user = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    assert_eq!(user.username, "Ada Lovelace");
    // A directory rename not yet followed: the uid link still finds her.
    harness.answer(vec![entry(
        ADA_DN,
        &[
            ("uid", "ada"),
            ("cn", "Ada King"),
            ("mail", "ada@example.org"),
        ],
    )]);
    harness
        .users
        .change_password(id_of(&user), NEW_PASSWORD)
        .await
        .expect("changed");
    assert_eq!(harness.directory.state().writes.len(), 1);
}

#[tokio::test]
async fn a_never_linked_user_ldap_auth_owns_changes_their_password_and_gains_no_link() {
    let harness = Harness::new(&changes_allowed()).await;
    let ada = harness.existing_user("ada", PROVIDER_ID, "").await;
    harness
        .users
        .change_password(id_of(&ada), NEW_PASSWORD)
        .await
        .expect("changed");
    assert_eq!(harness.directory.state().writes.len(), 1);
    assert_eq!(
        harness.link("ada").await,
        None,
        "a password change links nothing"
    );
}

/// A user whose entry only a referred server lists is found there, as sign-in
/// finds them, and the bind DN then writes the password at the configured
/// server, as upstream's `ChangePassword` does (`ConnectToLdap()` follows no
/// referral).
#[tokio::test]
async fn a_user_found_behind_a_referral_has_the_password_written_at_the_configured_server() {
    const GRACE_DN: &str = "uid=grace,ou=partners,dc=example,dc=org";
    let harness = Harness::new(&changes_allowed()).await;
    {
        let mut state = harness.directory.state();
        state.extensions = vec![PASSWORD_MODIFY_OID.to_owned()];
        state.entries.clear();
        state.references = vec![vec![
            "ldap://partners.example/ou=partners,dc=example,dc=org??sub".to_owned(),
        ]];
        state.by_server.insert(
            "partners.example".to_owned(),
            SearchOutcome {
                entries: vec![entry(
                    GRACE_DN,
                    &[("uid", "grace"), ("mail", "grace@example.org")],
                )],
                references: Vec::new(),
            },
        );
        state
            .accounts
            .insert(GRACE_DN.to_owned(), "grace-secret".to_owned());
    }
    let grace = harness
        .sign_in("grace", "grace-secret")
        .await
        .expect("sign in")
        .expect("created");
    harness.directory.clear_calls();

    harness
        .users
        .change_password(id_of(&grace), "grace-new-secret")
        .await
        .expect("changed");

    let state = harness.directory.state();
    assert_eq!(
        state.writes,
        [(
            GRACE_DN.to_owned(),
            "passwordModify".to_owned(),
            "grace-new-secret".to_owned()
        )]
    );
    assert_eq!(
        state.binds_at,
        [
            ("ldap.example".to_owned(), READER_DN.to_owned()),
            ("partners.example".to_owned(), READER_DN.to_owned()),
            ("ldap.example".to_owned(), READER_DN.to_owned()),
        ],
        "searched through the referral, written at the configured server"
    );
}
