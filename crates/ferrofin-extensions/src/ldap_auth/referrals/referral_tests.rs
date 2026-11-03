//! Referral following (upstream's `GetSearchConstraints` with its
//! `LdapAuthHandler`) over a fake network of directory servers, each
//! answering by search base, through the searches that follow referrals:
//! `search_as_bind_dn`, the filter test and the user search.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_model::ldap::{LdapFilterResponse, LdapTestErrorResponse, UserSearchResponse};
use ferrofin_model::secret::Secret;
use ferrofin_traits::ldap::LdapTestAnswer;
use rstest::rstest;

use super::super::{
    ADMIN_FILTER_DISABLED, BoundSearchError, CertificateVerification, ClientCertificate,
    LdapConnection, NO_ATTRIBUTES, ldap_user_search, search_as_bind_dn, test_ldap_filters,
};
use super::*;

const BASE_DN: &str = "dc=example,dc=org";
const PARTNERS_DN: &str = "ou=partners,dc=example,dc=org";
const READER_DN: &str = "cn=reader,dc=example,dc=org";
const READER_PASSWORD: &str = "reader-secret";
const FILTER: &str = "(objectClass=inetOrgPerson)";
const ADA_DN: &str = "uid=ada,ou=people,dc=example,dc=org";
const GRACE_DN: &str = "uid=grace,ou=partners,dc=example,dc=org";

/// One fake directory server: what a search answers by its base DN (any
/// other base finds nothing), and what fails.
#[derive(Clone, Default)]
struct Server {
    answers: HashMap<String, Result<SearchOutcome, LdapError>>,
    connect: Option<LdapError>,
    bind: Option<LdapError>,
}

impl Server {
    /// A server answering a search of `base` with `found`.
    fn answering(base: &str, found: SearchOutcome) -> Self {
        Self {
            answers: HashMap::from([(base.to_owned(), Ok(found))]),
            ..Self::default()
        }
    }

    /// A server failing a search of `base` with `error`.
    fn failing(base: &str, error: LdapError) -> Self {
        Self {
            answers: HashMap::from([(base.to_owned(), Err(error))]),
            ..Self::default()
        }
    }

    /// A server whose connections fail with `error`.
    fn unreachable(error: LdapError) -> Self {
        Self {
            connect: Some(error),
            ..Self::default()
        }
    }

    /// A server that refuses every bind with `error`.
    fn refusing(error: LdapError) -> Self {
        Self {
            bind: Some(error),
            ..Self::default()
        }
    }
}

/// Everything the network was asked to do.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Calls {
    connects: Vec<ConnectionOptions>,
    /// The server, the DN and the password of each bind.
    binds: Vec<(String, String, String)>,
    /// The server, the search and the attributes of each search.
    searches: Vec<(String, Search, Vec<String>)>,
    unbinds: usize,
}

impl Calls {
    /// The servers connected to, in order.
    fn servers(&self) -> Vec<String> {
        self.connects
            .iter()
            .map(|options| options.server.clone())
            .collect()
    }
}

/// An [`LdapDirectory`] reaching the [`Server`]s it holds by host name; any
/// other host does not resolve.
#[derive(Clone, Default)]
struct Network {
    servers: Arc<HashMap<String, Server>>,
    calls: Arc<Mutex<Calls>>,
}

impl Network {
    fn of<H: Into<String>>(servers: impl IntoIterator<Item = (H, Server)>) -> Self {
        Self {
            servers: Arc::new(
                servers
                    .into_iter()
                    .map(|(host, server)| (host.into(), server))
                    .collect(),
            ),
            calls: Arc::default(),
        }
    }

    fn calls(&self) -> Calls {
        self.calls.lock().expect("calls").clone()
    }
}

#[async_trait]
impl LdapDirectory for Network {
    async fn connect(
        &self,
        options: &ConnectionOptions,
    ) -> Result<Box<dyn LdapConnection>, LdapError> {
        self.calls
            .lock()
            .expect("calls")
            .connects
            .push(options.clone());
        let server = self
            .servers
            .get(&options.server)
            .ok_or_else(|| LdapError::Connect(format!("{} does not resolve", options.server)))?;
        if let Some(error) = &server.connect {
            return Err(error.clone());
        }
        Ok(Box::new(Connection {
            network: self.clone(),
            server: options.server.clone(),
        }))
    }
}

/// A connection to one of the network's servers.
struct Connection {
    network: Network,
    server: String,
}

impl Connection {
    fn script(&self) -> &Server {
        &self.network.servers[&self.server]
    }
}

#[async_trait]
impl LdapConnection for Connection {
    async fn simple_bind(&mut self, dn: &str, password: &str) -> Result<(), LdapError> {
        self.network.calls.lock().expect("calls").binds.push((
            self.server.clone(),
            dn.to_owned(),
            password.to_owned(),
        ));
        self.script().bind.clone().map_or(Ok(()), Err)
    }

    async fn search(
        &mut self,
        base: &str,
        scope: SearchScope,
        filter: &str,
        attributes: &[&str],
    ) -> Result<SearchOutcome, LdapError> {
        self.network.calls.lock().expect("calls").searches.push((
            self.server.clone(),
            Search {
                base: base.to_owned(),
                scope,
                filter: filter.to_owned(),
            },
            attributes.iter().map(|a| (*a).to_owned()).collect(),
        ));
        self.script()
            .answers
            .get(base)
            .cloned()
            .unwrap_or_else(|| Ok(SearchOutcome::default()))
    }

    async fn modify_replace(
        &mut self,
        _dn: &str,
        _attribute: &str,
        _values: &[&str],
    ) -> Result<(), LdapError> {
        Err(LdapError::Connection("not scripted".to_owned()))
    }

    async fn password_modify(
        &mut self,
        _user_dn: &str,
        _new_password: &str,
    ) -> Result<(), LdapError> {
        Err(LdapError::Connection("not scripted".to_owned()))
    }

    async fn supported_extensions(&mut self) -> Result<Vec<String>, LdapError> {
        Err(LdapError::Connection("not scripted".to_owned()))
    }

    async fn unbind(&mut self) {
        self.network.calls.lock().expect("calls").unbinds += 1;
    }
}

/// A plain-LDAP directory at `dc1.example`, the bind DN written with the
/// spaces Novell's bind trims, the admin filter off.
fn config() -> LdapConfig {
    LdapConfig {
        ldap_server: "dc1.example".to_owned(),
        ldap_port: 389,
        use_ssl: false,
        ldap_bind_user: format!("  {READER_DN} "),
        ldap_bind_password: Secret::new(READER_PASSWORD),
        ldap_base_dn: BASE_DN.to_owned(),
        ldap_search_filter: FILTER.to_owned(),
        ldap_admin_filter: ADMIN_FILTER_DISABLED.to_owned(),
        ..LdapConfig::default()
    }
}

/// A search that found the entries `dns` and the references `references`.
fn found(dns: &[&str], references: &[&[&str]]) -> SearchOutcome {
    SearchOutcome {
        entries: dns
            .iter()
            .map(|dn| LdapEntry {
                dn: (*dn).to_owned(),
                attributes: Vec::new(),
            })
            .collect(),
        references: references
            .iter()
            .map(|urls| urls.iter().map(|url| (*url).to_owned()).collect())
            .collect(),
    }
}

/// A `referral` result naming `urls`.
fn referred(urls: &[&str]) -> LdapError {
    LdapError::Referred {
        urls: urls.iter().map(|url| (*url).to_owned()).collect(),
        message: String::new(),
    }
}

/// The DNs `search_as_bind_dn` finds under the base DN with [`FILTER`],
/// asking for `uid`.
async fn search(network: &Network, config: &LdapConfig) -> Result<Vec<String>, BoundSearchError> {
    search_as_bind_dn(network, config, &config.ldap_base_dn, FILTER, &["uid"])
        .await
        .map(|entries| entries.into_iter().map(|entry| entry.dn).collect())
}

fn plain(server: &str, port: i32) -> ConnectionOptions {
    ConnectionOptions {
        server: server.to_owned(),
        port,
        use_ssl: false,
        use_start_tls: false,
        verification: CertificateVerification::System,
        client_certificate: None,
    }
}

/// `(server, dn, password)` of a bind as the configured bind DN at `server`.
fn reader_bind(server: &str) -> (String, String, String) {
    (
        server.to_owned(),
        READER_DN.to_owned(),
        READER_PASSWORD.to_owned(),
    )
}

#[tokio::test]
async fn a_search_reference_is_followed_as_the_bind_dn() {
    let network = Network::of([
        (
            "dc1.example",
            Server::answering(
                BASE_DN,
                found(
                    &[ADA_DN],
                    &[&["ldap://dc2.example/ou=partners,dc=example,dc=org??sub"]],
                ),
            ),
        ),
        (
            "dc2.example",
            Server::answering(PARTNERS_DN, found(&[GRACE_DN], &[])),
        ),
    ]);
    assert_eq!(
        search(&network, &config()).await,
        Ok(vec![ADA_DN.to_owned(), GRACE_DN.to_owned()])
    );
    let calls = network.calls();
    assert_eq!(
        calls.connects,
        [plain("dc1.example", 389), plain("dc2.example", 389)]
    );
    assert_eq!(
        calls.binds,
        [reader_bind("dc1.example"), reader_bind("dc2.example")]
    );
    assert_eq!(
        calls.searches[1],
        (
            "dc2.example".to_owned(),
            Search {
                base: PARTNERS_DN.to_owned(),
                scope: SearchScope::Subtree,
                filter: FILTER.to_owned(),
            },
            vec!["uid".to_owned()],
        ),
        "the URL's base, the search's own filter and attributes"
    );
    assert_eq!(calls.unbinds, 2, "every connection is closed");
}

#[tokio::test]
async fn referrals_are_followed_depth_first() {
    let network = Network::of([
        (
            "dc1.example",
            Server::answering(
                BASE_DN,
                found(
                    &["uid=a,dc=example,dc=org"],
                    &[
                        &["ldap://dc2.example/ou=one,dc=example,dc=org"],
                        &["ldap://dc3.example/ou=two,dc=example,dc=org"],
                    ],
                ),
            ),
        ),
        (
            "dc2.example",
            Server::answering(
                "ou=one,dc=example,dc=org",
                found(
                    &["uid=b,ou=one,dc=example,dc=org"],
                    &[&["ldap://dc4.example/ou=deep,ou=one,dc=example,dc=org"]],
                ),
            ),
        ),
        (
            "dc3.example",
            Server::answering(
                "ou=two,dc=example,dc=org",
                found(&["uid=c,ou=two,dc=example,dc=org"], &[]),
            ),
        ),
        (
            "dc4.example",
            Server::answering(
                "ou=deep,ou=one,dc=example,dc=org",
                found(&["uid=d,ou=deep,ou=one,dc=example,dc=org"], &[]),
            ),
        ),
    ]);
    assert_eq!(
        search(&network, &config()).await,
        Ok(vec![
            "uid=a,dc=example,dc=org".to_owned(),
            "uid=b,ou=one,dc=example,dc=org".to_owned(),
            "uid=d,ou=deep,ou=one,dc=example,dc=org".to_owned(),
            "uid=c,ou=two,dc=example,dc=org".to_owned(),
        ])
    );
    assert_eq!(
        network.calls().servers(),
        ["dc1.example", "dc2.example", "dc4.example", "dc3.example"]
    );
}

#[tokio::test]
async fn a_referral_result_redoes_the_search_where_it_points() {
    // No DN in the URL: the search keeps its base.
    let network = Network::of([
        (
            "dc1.example",
            Server::failing(BASE_DN, referred(&["ldap://dc2.example:3389"])),
        ),
        (
            "dc2.example",
            Server::answering(BASE_DN, found(&[ADA_DN], &[])),
        ),
    ]);
    assert_eq!(
        search(&network, &config()).await,
        Ok(vec![ADA_DN.to_owned()])
    );
    let calls = network.calls();
    assert_eq!(calls.connects[1], plain("dc2.example", 3389));
    assert_eq!(calls.searches[1].1.base, BASE_DN);
}

#[tokio::test]
async fn the_urls_of_a_referral_are_alternatives_tried_in_order() {
    let reference: &[&str] = &[
        "ldap://down.example/ou=partners,dc=example,dc=org",
        "ldap://refusing.example/ou=partners,dc=example,dc=org",
        "ldap://dc2.example/ou=partners,dc=example,dc=org",
        "ldap://never.example/ou=partners,dc=example,dc=org",
    ];
    let network = Network::of([
        (
            "dc1.example",
            Server::answering(BASE_DN, found(&[ADA_DN], &[reference])),
        ),
        (
            "down.example",
            Server::unreachable(LdapError::Connect("refused".to_owned())),
        ),
        (
            "refusing.example",
            Server::refusing(LdapError::Result {
                code: 49,
                message: String::new(),
            }),
        ),
        (
            "dc2.example",
            Server::answering(PARTNERS_DN, found(&[GRACE_DN], &[])),
        ),
    ]);
    assert_eq!(
        search(&network, &config()).await,
        Ok(vec![ADA_DN.to_owned(), GRACE_DN.to_owned()])
    );
    let calls = network.calls();
    assert_eq!(
        calls.servers(),
        [
            "dc1.example",
            "down.example",
            "refusing.example",
            "dc2.example"
        ]
    );
    assert_eq!(calls.unbinds, 3, "the refused connection is closed too");
}

#[rstest]
#[case::cannot_connect(
    Server::unreachable(LdapError::Connect("connection refused".to_owned())),
    "the referral to down.example:3890 failed: connection refused"
)]
#[case::refuses_the_bind_dn(
    Server::refusing(LdapError::Result { code: 49, message: String::new() }),
    "the referral to down.example:3890 failed: Invalid Credentials"
)]
#[case::fails_the_search(
    Server::failing(PARTNERS_DN, LdapError::Result {
        code: 32,
        message: "no such entry".to_owned(),
    }),
    "the referral to down.example:3890 failed: No Such Object: no such entry"
)]
#[tokio::test]
async fn a_referral_that_cannot_be_followed_fails_the_search_naming_the_server(
    #[case] referred: Server,
    #[case] message: &str,
) {
    let network = Network::of([
        (
            "dc1.example",
            Server::answering(
                BASE_DN,
                found(
                    &[ADA_DN],
                    &[&["ldap://down.example:3890/ou=partners,dc=example,dc=org"]],
                ),
            ),
        ),
        ("down.example", referred),
    ]);
    let error = search(&network, &config()).await.expect_err("fails");
    assert!(
        matches!(&error, BoundSearchError::Search(LdapError::ReferralFailed { server, .. })
            if server == "down.example:3890"),
        "{error:?}"
    );
    assert_eq!(error.to_string(), message);
}

#[tokio::test]
async fn a_referral_to_a_host_that_does_not_resolve_fails_the_search() {
    let network = Network::of([(
        "dc1.example",
        Server::failing(BASE_DN, referred(&["ldap://gone.example/"])),
    )]);
    assert_eq!(
        search(&network, &config()).await,
        Err(BoundSearchError::Search(LdapError::ReferralFailed {
            server: "gone.example:389".to_owned(),
            reason: Box::new(LdapError::Connect(
                "gone.example does not resolve".to_owned()
            )),
        }))
    );
}

/// How many lines the subscriber wrote at `level` that contain `text`.
#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
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

/// Two servers referring the search to each other: the hop limit stops it,
/// and the filter test fails with a warning that names the server.
#[tokio::test]
async fn the_hop_limit_stops_a_referral_loop_with_a_clear_log_line() {
    let logs = CapturedLogs::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .with_env_filter("ferrofin_extensions=debug")
            .finish(),
    );
    let network = Network::of([
        (
            "dc1.example",
            Server::answering(
                BASE_DN,
                found(&[], &[&["ldap://dc2.example/dc=example,dc=org"]]),
            ),
        ),
        (
            "dc2.example",
            Server::answering(
                BASE_DN,
                found(&[], &[&["ldap://dc1.example/dc=example,dc=org"]]),
            ),
        ),
    ]);
    let message = format!(
        "the search was referred more than {REFERRAL_HOP_LIMIT} times in a row, the last time \
         by dc1.example:389: the directory may refer in a loop"
    );
    assert_eq!(
        test_ldap_filters(&network, &config()).await,
        LdapTestAnswer::BadRequest(LdapTestErrorResponse::new(format!(
            "User Filter: {message}"
        )))
    );
    assert_eq!(
        network.calls().connects.len(),
        REFERRAL_HOP_LIMIT + 1,
        "the configured server, then one per referral followed"
    );
    assert_eq!(logs.count("WARN", "LDAP filter test failed"), 1);
    assert_eq!(logs.count("WARN", &message), 1, "the warning says why");
}

#[tokio::test]
async fn a_server_referring_to_itself_is_stopped_too() {
    let network = Network::of([(
        "dc1.example",
        Server::failing(BASE_DN, referred(&["ldap://dc1.example/dc=example,dc=org"])),
    )]);
    assert_eq!(
        search(&network, &config()).await,
        Err(BoundSearchError::Search(LdapError::ReferralHopLimit {
            server: "dc1.example:389".to_owned(),
        }))
    );
    assert_eq!(network.calls().connects.len(), REFERRAL_HOP_LIMIT + 1);
}

/// A chain of `referrals` referrals: `hop0.example` refers to `hop1`, and so
/// on; the last server finds Ada.
fn chain(referrals: usize) -> Network {
    Network::of((0..=referrals).map(|hop| {
        let answer = if hop == referrals {
            found(&[ADA_DN], &[])
        } else {
            let url = format!("ldap://hop{}.example/{BASE_DN}", hop + 1);
            found(&[], &[&[url.as_str()]])
        };
        (
            format!("hop{hop}.example"),
            Server::answering(BASE_DN, answer),
        )
    }))
}

#[tokio::test]
async fn exactly_the_hop_limit_of_referrals_is_followed() {
    let config = LdapConfig {
        ldap_server: "hop0.example".to_owned(),
        ..config()
    };
    assert_eq!(
        search(&chain(REFERRAL_HOP_LIMIT), &config).await,
        Ok(vec![ADA_DN.to_owned()])
    );
    let longer = chain(REFERRAL_HOP_LIMIT + 1);
    assert_eq!(
        search(&longer, &config).await,
        Err(BoundSearchError::Search(LdapError::ReferralHopLimit {
            server: format!("hop{REFERRAL_HOP_LIMIT}.example:389"),
        }))
    );
    assert_eq!(longer.calls().connects.len(), REFERRAL_HOP_LIMIT + 1);
}

/// The connection to a referred server keeps the configured TLS and its
/// certificate checks: never plain LDAP where the configuration asks for
/// LDAPS or StartTLS.
#[rstest]
#[case::ldaps_to_an_ldap_url(true, false, "ldap://dc2.example/ou=p", true, false, 636)]
#[case::ldaps_keeps_the_urls_port(true, false, "ldap://dc2.example:389/ou=p", true, false, 389)]
#[case::start_tls(false, true, "ldap://dc2.example/ou=p", false, true, 389)]
#[case::start_tls_to_an_ldaps_url(false, true, "ldaps://dc2.example/ou=p", true, false, 636)]
#[case::plain_to_an_ldaps_url(false, false, "ldaps://dc2.example:6360/ou=p", true, false, 6360)]
#[case::plain_stays_plain(false, false, "ldap://dc2.example/ou=p", false, false, 389)]
#[tokio::test]
async fn a_referral_never_downgrades_tls(
    #[case] use_ssl: bool,
    #[case] use_start_tls: bool,
    #[case] url: &str,
    #[case] referred_ssl: bool,
    #[case] referred_start_tls: bool,
    #[case] referred_port: i32,
) {
    let config = LdapConfig {
        use_ssl,
        use_start_tls,
        ldap_port: 1636,
        ldap_root_ca_path: "/ca.pem".to_owned(),
        ldap_client_cert_path: "/client.pem".to_owned(),
        ldap_client_key_path: "/client.key".to_owned(),
        ..config()
    };
    let network = Network::of([
        (
            "dc1.example",
            Server::answering(BASE_DN, found(&[], &[&[url]])),
        ),
        ("dc2.example", Server::default()),
    ]);
    search(&network, &config).await.expect("search");
    assert_eq!(
        network.calls().connects[1],
        ConnectionOptions {
            server: "dc2.example".to_owned(),
            port: referred_port,
            use_ssl: referred_ssl,
            use_start_tls: referred_start_tls,
            verification: CertificateVerification::CustomRoot(PathBuf::from("/ca.pem")),
            client_certificate: Some(ClientCertificate {
                certificate: PathBuf::from("/client.pem"),
                key: PathBuf::from("/client.key"),
            }),
        }
    );
}

#[tokio::test]
async fn a_referral_keeps_skipping_certificate_checks_only_when_configured() {
    let config = LdapConfig {
        use_ssl: true,
        skip_ssl_verify: true,
        ..config()
    };
    let network = Network::of([
        (
            "dc1.example",
            Server::answering(BASE_DN, found(&[], &[&["ldap://dc2.example/ou=p"]])),
        ),
        ("dc2.example", Server::default()),
    ]);
    search(&network, &config).await.expect("search");
    assert_eq!(
        network.calls().connects[1].verification,
        CertificateVerification::None
    );
}

#[tokio::test]
async fn a_url_that_cannot_be_followed_is_skipped_and_never_quoted() {
    const SECRET_URL: &str = "http://reader:hunter2@dc2.example/";
    let alone = Network::of([(
        "dc1.example",
        Server::answering(BASE_DN, found(&[], &[&[SECRET_URL]])),
    )]);
    let error = search(&alone, &config()).await.expect_err("fails");
    assert_eq!(
        error,
        BoundSearchError::Search(LdapError::ReferralUrl(
            "it is not an ldap:// or ldaps:// URL".to_owned()
        ))
    );
    assert!(!error.to_string().contains("hunter2"), "{error}");
    assert!(!format!("{error:?}").contains("hunter2"), "{error:?}");

    let with_another = Network::of([
        (
            "dc1.example",
            Server::answering(
                BASE_DN,
                found(
                    &[],
                    &[&[
                        SECRET_URL,
                        "ldap://dc2.example/ou=partners,dc=example,dc=org",
                    ]],
                ),
            ),
        ),
        (
            "dc2.example",
            Server::answering(PARTNERS_DN, found(&[GRACE_DN], &[])),
        ),
    ]);
    assert_eq!(
        search(&with_another, &config()).await,
        Ok(vec![GRACE_DN.to_owned()])
    );
}

#[tokio::test]
async fn a_reference_with_no_url_fails_the_search() {
    let network = Network::of([(
        "dc1.example",
        Server::answering(BASE_DN, found(&[], &[&[]])),
    )]);
    assert_eq!(
        search(&network, &config()).await,
        Err(BoundSearchError::Search(LdapError::ReferralUrl(
            "the referral names no URL".to_owned()
        )))
    );
}

/// `GetFilteredUsers` follows references: the filter test counts the users
/// a referred server holds.
#[tokio::test]
async fn the_filter_test_counts_the_entries_behind_a_reference() {
    let network = Network::of([
        (
            "dc1.example",
            Server::answering(
                BASE_DN,
                found(
                    &[ADA_DN],
                    &[&["ldap://dc2.example/ou=partners,dc=example,dc=org"]],
                ),
            ),
        ),
        (
            "dc2.example",
            Server::answering(PARTNERS_DN, found(&[GRACE_DN], &[])),
        ),
    ]);
    assert_eq!(
        test_ldap_filters(&network, &config()).await,
        LdapTestAnswer::Ok(LdapFilterResponse {
            users: 2,
            admins: 0,
            is_subset: true,
        })
    );
    assert_eq!(
        network.calls().searches[1].2,
        [NO_ATTRIBUTES],
        "the filter search's own attributes"
    );
}

/// `LocateLdapUser` follows references: the user search finds a user only a
/// referred server holds.
#[tokio::test]
async fn the_user_search_finds_a_user_behind_a_reference() {
    let network = Network::of([
        (
            "dc1.example",
            Server::answering(
                BASE_DN,
                found(
                    &[],
                    &[&["ldap://dc2.example/ou=partners,dc=example,dc=org"]],
                ),
            ),
        ),
        (
            "dc2.example",
            Server::answering(PARTNERS_DN, found(&[GRACE_DN], &[])),
        ),
    ]);
    assert_eq!(
        ldap_user_search(&network, &config(), "grace").await,
        LdapTestAnswer::Ok(UserSearchResponse {
            located_dn: Some(GRACE_DN.to_owned()),
        })
    );
}

#[rstest]
#[case::dn_scope_and_filter(
    "ldap://dc2.example/ou=Domain%20Users,dc=example,dc=org??sub?(uid=a%2ab)",
    false,
    "dc2.example",
    None,
    Some("ou=Domain Users,dc=example,dc=org"),
    Some(SearchScope::Subtree),
    Some("(uid=a*b)")
)]
#[case::scheme_ignores_case_and_a_port(
    "LDAPS://DC2.example:3269",
    true,
    "DC2.example",
    Some(3269),
    None,
    None,
    None
)]
#[case::ipv6(
    "ldap://[2001:db8::1]:389/dc=x",
    false,
    "2001:db8::1",
    Some(389),
    Some("dc=x"),
    None,
    None
)]
#[case::ipv6_without_a_port("ldaps://[::1]/dc=x", true, "::1", None, Some("dc=x"), None, None)]
#[case::enclosed(
    "<URL:ldap://dc2.example/dc=x>",
    false,
    "dc2.example",
    None,
    Some("dc=x"),
    None,
    None
)]
#[case::one_level(
    "ldap://dc2.example/dc=x?cn?ONE",
    false,
    "dc2.example",
    None,
    Some("dc=x"),
    Some(SearchScope::OneLevel),
    None
)]
#[case::base(
    "ldap://dc2.example/dc=x??base",
    false,
    "dc2.example",
    None,
    Some("dc=x"),
    Some(SearchScope::Base),
    None
)]
#[case::empty_parts("ldap://dc2.example/???", false, "dc2.example", None, None, None, None)]
#[case::extensions_ignored(
    "ldap://dc2.example/dc=x??sub??!bindname=cn=x",
    false,
    "dc2.example",
    None,
    Some("dc=x"),
    Some(SearchScope::Subtree),
    None
)]
#[case::novells_broken_format_is_not_decoded(
    "ldap://10.0.0.2??cn=100%25",
    false,
    "10.0.0.2",
    None,
    Some("cn=100%25"),
    None,
    None
)]
#[case::novells_broken_format(
    "ldap://10.0.0.2:389??cn=abc,o=company",
    false,
    "10.0.0.2",
    Some(389),
    Some("cn=abc,o=company"),
    None,
    None
)]
#[case::no_dn("ldap://dc2.example?uid", false, "dc2.example", None, None, None, None)]
fn an_ldap_url_is_read_as_rfc_4516_writes_it(
    #[case] url: &str,
    #[case] secure: bool,
    #[case] host: &str,
    #[case] port: Option<u16>,
    #[case] dn: Option<&str>,
    #[case] scope: Option<SearchScope>,
    #[case] filter: Option<&str>,
) {
    assert_eq!(
        ReferralUrl::parse(url),
        Ok(ReferralUrl {
            secure,
            host: host.to_owned(),
            port,
            dn: dn.map(str::to_owned),
            scope,
            filter: filter.map(str::to_owned),
        })
    );
}

#[rstest]
#[case::another_scheme("https://dc2.example/dc=x", "it is not an ldap:// or ldaps:// URL")]
#[case::no_server("ldap:///dc=x", "it names no server")]
#[case::no_server_but_a_port("ldap://:389", "it names no server")]
#[case::a_port_too_large("ldap://dc2.example:65536/", "its port is not a TCP port")]
#[case::an_unclosed_address("ldap://[::1/dc=x", "its IPv6 address has no closing bracket")]
#[case::garbage_after_an_address(
    "ldap://[::1]x/dc=x",
    "its IPv6 address is not followed by a port"
)]
#[case::an_unknown_scope(
    "ldap://dc2.example/dc=x??deep",
    "it names a scope other than base, one or sub"
)]
#[case::not_utf8(
    "ldap://dc2.example/dc=%FF",
    "its DN or filter is not UTF-8 once decoded"
)]
#[case::a_space_in_the_host(
    "ldap://dc2 example/dc=x",
    "its server is not a host name or an address"
)]
#[case::a_control_character_in_the_host(
    "ldap://dc2\u{1b}[31m.example/dc=x",
    "its server is not a host name or an address"
)]
fn an_ldap_url_that_cannot_be_followed_says_why(#[case] url: &str, #[case] reason: &str) {
    assert_eq!(ReferralUrl::parse(url), Err(reason.to_owned()));
}

#[rstest]
#[case::continuation_of_one_level(None, SearchScope::OneLevel, true, SearchScope::Base)]
#[case::referral_of_one_level(None, SearchScope::OneLevel, false, SearchScope::OneLevel)]
#[case::subtree(None, SearchScope::Subtree, true, SearchScope::Subtree)]
#[case::the_urls_scope_wins(Some(SearchScope::Base), SearchScope::Subtree, true, SearchScope::Base)]
fn a_continued_search_takes_the_urls_parts_else_the_searchs(
    #[case] url_scope: Option<SearchScope>,
    #[case] scope: SearchScope,
    #[case] continuation: bool,
    #[case] expected: SearchScope,
) {
    let url = ReferralUrl {
        secure: false,
        host: "dc2.example".to_owned(),
        port: None,
        dn: None,
        scope: url_scope,
        filter: Some("(uid=b)".to_owned()),
    };
    let search = Search {
        base: BASE_DN.to_owned(),
        scope,
        filter: FILTER.to_owned(),
    };
    assert_eq!(
        url.continue_search(&search, continuation),
        Search {
            base: BASE_DN.to_owned(),
            scope: expected,
            filter: "(uid=b)".to_owned(),
        }
    );
}

#[test]
fn a_server_is_named_with_its_port_an_address_in_brackets() {
    assert_eq!(server_label("dc1.example", 389), "dc1.example:389");
    assert_eq!(server_label("::1", 636), "[::1]:636");
    assert_eq!(server_label("[::1]", 636), "[::1]:636");
}
