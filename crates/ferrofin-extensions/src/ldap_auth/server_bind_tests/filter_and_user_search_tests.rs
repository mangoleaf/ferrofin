//! The settings page's filter test (`LdapController.TestLdapFilters` with
//! `GetFilteredUsers`) and user search (`LdapController.LdapUserSearch` with
//! `LocateLdapUser`) over the fake directory, and the service that saves their
//! bodies first, over the real plugin manager.

use super::*;

const USER_FILTER: &str = "(objectClass=inetOrgPerson)";
const ADMIN_FILTER: &str = "(employeeType=jellyfin-admin)";

/// [`plain_config`] with both filters set.
fn filters_config() -> LdapConfig {
    LdapConfig {
        ldap_search_filter: USER_FILTER.to_owned(),
        ldap_admin_filter: ADMIN_FILTER.to_owned(),
        ..plain_config()
    }
}

fn entry(dn: &str) -> LdapEntry {
    LdapEntry {
        dn: dn.to_owned(),
        attributes: Vec::new(),
    }
}

fn found(dns: &[&str]) -> SearchOutcome {
    SearchOutcome {
        entries: dns.iter().map(|dn| entry(dn)).collect(),
        references: Vec::new(),
    }
}

/// A directory whose user filter finds `users` and admin filter `admins`.
fn directory(users: &[&str], admins: &[&str]) -> FakeDirectory {
    FakeDirectory::answering(Script {
        by_filter: HashMap::from([
            (USER_FILTER.to_owned(), Ok(found(users))),
            (ADMIN_FILTER.to_owned(), Ok(found(admins))),
        ]),
        ..Script::default()
    })
}

fn error(message: &str) -> LdapTestErrorResponse {
    LdapTestErrorResponse::new(message)
}

#[tokio::test]
async fn the_filter_test_counts_each_filters_entries_once_and_compares_them() {
    let directory = directory(
        &[
            "uid=ada,dc=example,dc=org",
            "uid=bob,dc=example,dc=org",
            "uid=ada,dc=example,dc=org",
        ],
        &["uid=ada,dc=example,dc=org"],
    );
    let answer = test_ldap_filters(&directory, &filters_config()).await;
    assert_eq!(
        answer,
        LdapTestAnswer::Ok(LdapFilterResponse {
            users: 2,
            admins: 1,
            is_subset: true,
        }),
        "the same DN twice counts once, as upstream's ToHashSet"
    );

    // Each filter on a connection of its own, bound as the bind DN, asking
    // for no attributes.
    let calls = directory.calls();
    assert_eq!(calls.connects.len(), 2);
    assert_eq!(calls.unbinds, 2);
    assert_eq!(
        calls.binds,
        vec![
            (
                "cn=reader,dc=example,dc=org".to_owned(),
                "reader-secret".to_owned()
            ),
            (
                "cn=reader,dc=example,dc=org".to_owned(),
                "reader-secret".to_owned()
            ),
        ]
    );
    assert_eq!(
        calls.searches,
        vec![
            (
                "dc=example,dc=org".to_owned(),
                SearchScope::Subtree,
                USER_FILTER.to_owned(),
                vec!["1.1".to_owned()],
            ),
            (
                "dc=example,dc=org".to_owned(),
                SearchScope::Subtree,
                ADMIN_FILTER.to_owned(),
                vec!["1.1".to_owned()],
            ),
        ]
    );
}

#[tokio::test]
async fn admins_the_user_filter_does_not_find_are_not_a_subset() {
    let directory = directory(
        &["uid=bob,dc=example,dc=org"],
        &["uid=ada,dc=example,dc=org"],
    );
    assert_eq!(
        test_ldap_filters(&directory, &filters_config()).await,
        LdapTestAnswer::Ok(LdapFilterResponse {
            users: 1,
            admins: 1,
            is_subset: false,
        })
    );
}

#[tokio::test]
async fn the_admin_filter_is_tested_as_written_under_the_base_dn() {
    // As upstream: no `{username}` replaced, and not LdapAdminBaseDn.
    let config = LdapConfig {
        ldap_admin_filter: "(memberUid={username})".to_owned(),
        ldap_admin_base_dn: "ou=groups,dc=example,dc=org".to_owned(),
        ..filters_config()
    };
    let directory = FakeDirectory::default();
    test_ldap_filters(&directory, &config).await;
    let searches = directory.calls().searches;
    assert_eq!(searches[1].0, "dc=example,dc=org");
    assert_eq!(searches[1].2, "(memberUid={username})");
}

#[rstest]
#[case::empty("")]
#[case::disabled("_disabled_")]
#[tokio::test]
async fn with_the_admin_filter_off_only_users_are_counted(#[case] admin_filter: &str) {
    let directory = directory(
        &["uid=ada,dc=example,dc=org"],
        &["uid=ada,dc=example,dc=org"],
    );
    let config = LdapConfig {
        ldap_admin_filter: admin_filter.to_owned(),
        ..filters_config()
    };
    assert_eq!(
        test_ldap_filters(&directory, &config).await,
        LdapTestAnswer::Ok(LdapFilterResponse {
            users: 1,
            admins: 0,
            is_subset: true,
        })
    );
    assert_eq!(directory.calls().searches.len(), 1);
}

#[tokio::test]
async fn an_empty_user_filter_searches_for_every_entry_as_novell_does() {
    let config = LdapConfig {
        ldap_search_filter: String::new(),
        ..filters_config()
    };
    let directory = FakeDirectory::default();
    test_ldap_filters(&directory, &config).await;
    assert_eq!(directory.calls().searches[0].2, "(objectclass=*)");
}

#[rstest]
#[case::cannot_connect(Script {
    connect: Err(LdapError::Connect("connection refused".to_owned())),
    ..Script::default()
})]
#[case::start_tls_refused(Script {
    connect: Err(LdapError::StartTls("unwilling".to_owned())),
    ..Script::default()
})]
#[case::bind_dn_refused(Script {
    bind: Err(LdapError::Result { code: 49, message: String::new() }),
    ..Script::default()
})]
#[tokio::test]
async fn a_directory_that_cannot_be_reached_or_bound_is_unauthorized(#[case] script: Script) {
    let directory = FakeDirectory::answering(script);
    assert_eq!(
        test_ldap_filters(&directory, &filters_config()).await,
        LdapTestAnswer::Unauthorized(error("Failed to Connect or Bind to server."))
    );
}

#[tokio::test]
async fn a_failed_search_is_a_bad_request_naming_the_filter() {
    let unparsable = FakeDirectory::answering(Script {
        by_filter: HashMap::from([(
            USER_FILTER.to_owned(),
            Err(LdapError::Result {
                code: FILTER_ERROR,
                message: String::new(),
            }),
        )]),
        ..Script::default()
    });
    assert_eq!(
        test_ldap_filters(&unparsable, &filters_config()).await,
        LdapTestAnswer::BadRequest(error("User Filter: Filter Error"))
    );
    assert_eq!(
        unparsable.calls().searches.len(),
        1,
        "the admin filter waits"
    );

    let refused = FakeDirectory::answering(Script {
        by_filter: HashMap::from([
            (USER_FILTER.to_owned(), Ok(found(&[]))),
            (
                ADMIN_FILTER.to_owned(),
                Err(LdapError::Result {
                    code: 50,
                    message: "no read access".to_owned(),
                }),
            ),
        ]),
        ..Script::default()
    });
    assert_eq!(
        test_ldap_filters(&refused, &filters_config()).await,
        LdapTestAnswer::BadRequest(error(
            "Admin Filter: Insufficient Access Rights: no read access"
        ))
    );
}

#[tokio::test]
async fn the_user_search_finds_the_entry_a_name_signs_in_as() {
    let config = LdapConfig {
        ldap_search_attributes: "uid, mail".to_owned(),
        ..filters_config()
    };
    let directory = FakeDirectory::answering(Script {
        search: Ok(found(&[
            "uid=ada,dc=example,dc=org",
            "uid=ada2,dc=example,dc=org",
        ])),
        ..Script::default()
    });
    assert_eq!(
        ldap_user_search(&directory, &config, "ada").await,
        LdapTestAnswer::Ok(UserSearchResponse {
            located_dn: Some("uid=ada,dc=example,dc=org".to_owned()),
        }),
        "the first entry, as sign-in uses"
    );
    assert_eq!(
        directory.calls().searches,
        vec![(
            "dc=example,dc=org".to_owned(),
            SearchScope::Subtree,
            "(&(objectClass=inetOrgPerson)(|(uid=ada)(mail=ada)))".to_owned(),
            vec!["cn".to_owned(), "uid".to_owned()],
        )]
    );
}

#[rstest]
#[case::no_match(Script::default(), "Found no LDAP users matching provided username.")]
#[case::cannot_connect(
    Script { connect: Err(LdapError::Connect("refused".to_owned())), ..Script::default() },
    "Failed to Connect or Bind to server."
)]
#[case::bind_dn_refused(
    Script { bind: Err(LdapError::Result { code: 49, message: String::new() }), ..Script::default() },
    "Failed to Connect or Bind to server."
)]
#[case::search_fails(
    Script { search: Err(LdapError::Timeout), ..Script::default() },
    "Error completing LDAP login while applying user filter."
)]
#[case::a_referral_loop(
    Script {
        search: Ok(SearchOutcome {
            entries: Vec::new(),
            references: vec![vec!["ldap://ldap.example/dc=example,dc=org".to_owned()]],
        }),
        ..Script::default()
    },
    "Error completing LDAP login while applying user filter."
)]
#[tokio::test]
async fn a_user_search_that_finds_no_one_is_unauthorized(
    #[case] script: Script,
    #[case] message: &str,
) {
    let directory = FakeDirectory::answering(script);
    assert_eq!(
        ldap_user_search(&directory, &filters_config(), "ada").await,
        LdapTestAnswer::Unauthorized(error(message))
    );
}

#[test]
fn applying_the_filters_keeps_member_uid_mode_unless_it_is_sent() {
    let mut config = LdapConfig {
        enable_ldap_admin_filter_member_uid: true,
        ..filters_config()
    };
    config.apply_filters(UserFilterInfo {
        ldap_search_filter: Some("(uid=*)".to_owned()),
        ldap_admin_filter: None,
        enable_ldap_admin_filter_member_uid: None,
    });
    assert_eq!(config.ldap_search_filter, "(uid=*)");
    assert_eq!(
        config.ldap_admin_filter, "",
        "a missing filter is saved empty"
    );
    assert!(config.enable_ldap_admin_filter_member_uid);
    assert!(!config.admin_filter_enabled());

    config.apply_filters(UserFilterInfo {
        enable_ldap_admin_filter_member_uid: Some(false),
        ..UserFilterInfo::default()
    });
    assert!(!config.enable_ldap_admin_filter_member_uid);
}

#[rstest]
#[case::set("(cn=admins)", true)]
#[case::empty("", false)]
#[case::disabled("_disabled_", false)]
#[case::disabled_is_case_sensitive("_DISABLED_", true)]
fn the_admin_filter_is_on_unless_empty_or_disabled(#[case] filter: &str, #[case] on: bool) {
    let config = LdapConfig {
        ldap_admin_filter: filter.to_owned(),
        ..LdapConfig::default()
    };
    assert_eq!(config.admin_filter_enabled(), on);
}

#[test]
fn the_admin_base_dn_falls_back_to_the_base_dn() {
    let mut config = plain_config();
    assert_eq!(config.admin_base_dn(), "dc=example,dc=org");
    config.ldap_admin_base_dn = "ou=groups,dc=example,dc=org".to_owned();
    assert_eq!(config.admin_base_dn(), "ou=groups,dc=example,dc=org");
}

#[tokio::test]
async fn while_disabled_neither_test_saves_or_connects() {
    let dir = tempfile::tempdir().expect("tempdir");
    let directory = FakeDirectory::default();
    let service = LdapAuthService::new(plugin_manager(dir.path()), Arc::new(directory.clone()));
    assert!(matches!(
        service.test_ldap_filters(UserFilterInfo::default()).await,
        Err(ServiceError::NotFound(_))
    ));
    assert!(matches!(
        service
            .ldap_user_search(UserSearchAttributes {
                ldap_search_attributes: Some("mail".to_owned()),
                test_search_username: Some("ada".to_owned()),
            })
            .await,
        Err(ServiceError::NotFound(_))
    ));
    assert_eq!(directory.calls(), Calls::default(), "no network call");
    assert_eq!(
        service.config().await.expect("config"),
        LdapConfig::default()
    );
}

#[tokio::test]
async fn the_filter_test_saves_the_filters_then_tests_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plugins = plugin_manager(dir.path());
    plugins.enable_plugin(EXTENSION_ID).await.expect("enable");
    let directory = directory(&["uid=ada,dc=example,dc=org"], &[]);
    let service = LdapAuthService::new(plugins.clone(), Arc::new(directory.clone()));
    service
        .test_server_bind(connection_info())
        .await
        .expect("connection saved");
    service
        .update_config(|config| config.enable_ldap_admin_filter_member_uid = true)
        .await
        .expect("member uid on");

    // What upstream's settings page sends.
    let answer = service
        .test_ldap_filters(UserFilterInfo {
            ldap_search_filter: Some(USER_FILTER.to_owned()),
            ldap_admin_filter: Some(ADMIN_FILTER.to_owned()),
            enable_ldap_admin_filter_member_uid: None,
        })
        .await
        .expect("tested");
    assert_eq!(
        answer,
        LdapTestAnswer::Ok(LdapFilterResponse {
            users: 1,
            admins: 0,
            is_subset: true,
        })
    );
    let saved = service.config().await.expect("config");
    assert_eq!(saved.ldap_search_filter, USER_FILTER);
    assert_eq!(saved.ldap_admin_filter, ADMIN_FILTER);
    assert!(
        saved.enable_ldap_admin_filter_member_uid,
        "not turned off by a body that leaves it out"
    );
    assert_eq!(saved.ldap_server, "ldap.example", "the rest is kept");
}

#[tokio::test]
async fn the_user_search_saves_the_attributes_and_looks_up_only_a_given_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plugins = plugin_manager(dir.path());
    plugins.enable_plugin(EXTENSION_ID).await.expect("enable");
    let directory = FakeDirectory::answering(Script {
        search: Ok(found(&["uid=ada,dc=example,dc=org"])),
        ..Script::default()
    });
    let service = LdapAuthService::new(plugins.clone(), Arc::new(directory.clone()));

    for name in [None, Some(String::new())] {
        let answer = service
            .ldap_user_search(UserSearchAttributes {
                ldap_search_attributes: Some("uid, mail".to_owned()),
                test_search_username: name,
            })
            .await
            .expect("saved");
        assert_eq!(answer, LdapTestAnswer::Ok(UserSearchResponse::default()));
    }
    assert_eq!(directory.calls(), Calls::default(), "no name, no search");
    assert_eq!(
        service
            .config()
            .await
            .expect("config")
            .ldap_search_attributes,
        "uid, mail"
    );

    let answer = service
        .ldap_user_search(UserSearchAttributes {
            ldap_search_attributes: Some("mail".to_owned()),
            test_search_username: Some("ada@example.org".to_owned()),
        })
        .await
        .expect("searched");
    assert_eq!(
        answer,
        LdapTestAnswer::Ok(UserSearchResponse {
            located_dn: Some("uid=ada,dc=example,dc=org".to_owned()),
        })
    );
    let filter = &directory.calls().searches[0].2;
    assert!(
        filter.ends_with("(|(mail=ada@example.org)))"),
        "searched with the attributes just saved: {filter}"
    );
}
