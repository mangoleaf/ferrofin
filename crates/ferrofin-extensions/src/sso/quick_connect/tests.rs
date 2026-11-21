//! Quick Connect approval through a sign-in (B13): the code-entry page, the
//! start's code check, the callback's confirmation page and the confirmation
//! itself, against the fake identity provider and the server's own Quick
//! Connect manager over the harness's sessions.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ferrofin_core::FerrofinQuickConnect;
use ferrofin_model::configuration::ServerConfiguration;
use ferrofin_model::users::UserPolicy;
use ferrofin_traits::activity::{ActivityLogQuery, ActivityManager as _};
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::library::UserManager as _;
use ferrofin_traits::options::AuthorizationInfo;
use ferrofin_traits::session::SessionManager as _;
use ferrofin_traits::sso::OidcExchangeRefusal;
use serde_json::json;
use uuid::Uuid;

use super::*;
use crate::sso::fake_idp::USERNAME;
use crate::sso::sign_in::tests::{Harness, PUBLIC_URL, device, exchange_code};

/// The approving browser's address: a remote one.
const CLIENT: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 9));

/// The role a test requires or maps to the administrator flag.
const USERS: &str = "ferrofin-users";
const ADMINS: &str = "ferrofin-admins";

/// A change to an account's policy that makes a sign-in rule refuse it.
type Rule = fn(&mut UserPolicy);

/// The flow cookie, cleared.
const CLEARED: &str = "ferrofin_sso_flow=; Path=/jf/sso; Max-Age=0; HttpOnly; SameSite=Lax; Secure";

/// The server configuration the Quick Connect manager reads: the defaults,
/// with Quick Connect on while the flag is.
struct Switch(AtomicBool);

#[async_trait]
impl ServerConfigurationManager for Switch {
    fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
        unreachable!("Quick Connect reads no application path")
    }
    async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
        let mut configuration = ferrofin_core::default_server_configuration();
        configuration.quick_connect_available = self.0.load(Ordering::SeqCst);
        Ok(Arc::new(configuration))
    }
    async fn update_configuration(&self, _: &ServerConfiguration) -> Result<(), ServiceError> {
        unreachable!("Quick Connect saves no configuration")
    }
    async fn get_branding(
        &self,
    ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
        unreachable!("Quick Connect reads no branding")
    }
    async fn update_branding(
        &self,
        _: &ferrofin_model::branding::BrandingOptions,
    ) -> Result<(), ServiceError> {
        unreachable!("Quick Connect saves no branding")
    }
}

/// The sign-in harness, its service approving through the server's Quick
/// Connect manager, and the manager's switch.
struct Approval {
    harness: Harness,
    quick_connect: Arc<FerrofinQuickConnect>,
    switch: Arc<Switch>,
}

impl Approval {
    async fn new() -> Self {
        Self::with(|_| {}).await
    }

    /// With the provider `fake` `edit`ed.
    async fn with(edit: impl FnOnce(&mut OidcProvider)) -> Self {
        let mut harness = Harness::with(Some(PUBLIC_URL), edit).await;
        let switch = Arc::new(Switch(AtomicBool::new(true)));
        let quick_connect = Arc::new(FerrofinQuickConnect::new(
            Arc::clone(&switch) as Arc<dyn ServerConfigurationManager>,
            Arc::clone(&harness.sessions) as Arc<dyn ferrofin_traits::session::SessionManager>,
        ));
        harness.sso.quick_connect = Some(Arc::clone(&quick_connect) as Arc<dyn QuickConnect>);
        Self {
            harness,
            quick_connect,
            switch,
        }
    }

    fn quick_connect(&self, on: bool) {
        self.switch.0.store(on, Ordering::SeqCst);
    }

    /// The app's `POST /QuickConnect/Initiate`: the code it shows, and its
    /// secret.
    async fn initiate(&self) -> (String, String) {
        let request = self
            .quick_connect
            .try_connect(&AuthorizationInfo {
                device_id: Some("qc-app".to_owned()),
                device: Some("Living Room".to_owned()),
                client: Some("Jellyfin Android TV".to_owned()),
                version: Some("0.19.10".to_owned()),
                ..AuthorizationInfo::default()
            })
            .await
            .expect("initiated");
        (request.code, request.secret.expose().to_owned())
    }

    /// Whether the app's request is authorized.
    async fn authorized(&self, secret: &str) -> bool {
        self.quick_connect
            .check_request_status(secret)
            .await
            .expect("pending")
            .authenticated
    }

    /// A sign-in at `fake` started to approve `code` and called back: the
    /// callback's page, and the browser's flow cookie.
    async fn called_back(&self, code: &str) -> (SsoReply, String) {
        let started = Harness::started(
            self.harness
                .sso
                .start_oidc_quick_connect("fake", CLIENT, code)
                .await
                .expect("starts"),
        );
        let callback = self.harness.callback(&started).await;
        let page = self
            .harness
            .sso
            .finish_oidc_sign_in("fake", callback)
            .await
            .expect("an answer");
        (page, started.cookie)
    }

    /// The confirmation page's form posted, from [`CLIENT`].
    async fn confirm(&self, confirmation: Option<&str>, cookie: Option<&str>) -> SsoReply {
        self.confirm_from(CLIENT, confirmation, cookie).await
    }

    /// The confirmation page's form posted, from `client`.
    async fn confirm_from(
        &self,
        client: IpAddr,
        confirmation: Option<&str>,
        cookie: Option<&str>,
    ) -> SsoReply {
        self.harness
            .sso
            .confirm_oidc_quick_connect(
                confirmation.map(Secret::new),
                cookie.map(Secret::new),
                client,
            )
            .await
            .expect("an answer")
    }

    /// A code approved from start to confirmation: the app's secret, and
    /// the confirmation's page.
    async fn approved(&self) -> (String, SsoReply) {
        let (code, secret) = self.initiate().await;
        let (page, cookie) = self.called_back(&code).await;
        assert_eq!(page.status, 200, "{}", page.html);
        let reply = self
            .confirm(Some(&confirmation_of(&page.html)), Some(&cookie))
            .await;
        (secret, reply)
    }

    /// The fake's next sign-ins carry the role claim `groups`.
    fn groups(&self, groups: serde_json::Value) {
        self.harness.fake.sign_in(|sign_in| {
            sign_in.claims.insert("groups".to_owned(), groups);
        });
    }

    /// Whether the fake's account is an administrator now.
    async fn is_administrator(&self) -> bool {
        let user = self.account().await;
        self.harness
            .users
            .get_user_dto(&user, None)
            .await
            .expect("a DTO")
            .policy
            .expect("a policy")
            .is_administrator
    }

    /// The account the fake's identity signs in to.
    async fn account(&self) -> UserEntity {
        self.harness
            .users
            .get_user_by_name(USERNAME)
            .await
            .expect("lookup")
            .expect("the account")
    }

    /// Saves the account's policy, `edit`ed, and answers it as it was.
    async fn edit_policy(&self, edit: impl FnOnce(&mut UserPolicy)) -> UserPolicy {
        let user = self.account().await;
        let mut policy = self
            .harness
            .users
            .get_user_dto(&user, None)
            .await
            .expect("a DTO")
            .policy
            .expect("a policy");
        let before = policy.clone();
        edit(&mut policy);
        self.harness
            .users
            .update_policy(Uuid::parse_str(&user.id).expect("an id"), &policy)
            .await
            .expect("saved");
        before
    }
}

/// The confirmation a confirmation page's form posts.
fn confirmation_of(html: &str) -> String {
    html.split_once("name=\"confirmation\" value=\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map_or_else(
            || panic!("no confirmation in {html}"),
            |(value, _)| value.to_owned(),
        )
}

#[test]
fn a_code_is_six_digits_and_its_debug_hides_them() {
    for (text, digits) in [
        ("123456", "123456"),
        (" 654321\n", "654321"),
        ("000000", "000000"),
    ] {
        assert_eq!(
            QuickConnectCode::parse(text)
                .as_ref()
                .map(QuickConnectCode::as_str),
            Some(digits),
            "{text:?}"
        );
    }
    for text in [
        "",
        "12345",
        "1234567",
        "12a456",
        "-12345",
        "\u{ff11}\u{ff12}\u{ff13}\u{ff14}\u{ff15}\u{ff16}",
        "<b>12",
    ] {
        assert!(QuickConnectCode::parse(text).is_none(), "{text:?}");
    }
    assert_eq!(
        format!("{:?}", QuickConnectCode::parse("123456")),
        "Some(QuickConnectCode(..))"
    );
}

#[tokio::test]
async fn the_code_entry_page_has_a_button_for_each_provider_that_signs_users_in() {
    let approval = Approval::new().await;
    let page = approval
        .harness
        .sso
        .quick_connect_page()
        .await
        .expect("a page");
    assert_eq!(page.status, 200, "{}", page.html);
    for part in [
        "<form method=\"get\">",
        "<input id=\"code\" name=\"quickConnect\" required inputmode=\"numeric\" \
         pattern=\"[0-9]{6}\" maxlength=\"6\" autocomplete=\"off\" autofocus>",
        // The name is the admin's text, escaped.
        "<button type=\"submit\" formaction=\"oidc/fake/start\">Sign in with Fake \
         &lt;IdP&gt;</button>",
        "<a href=\"../web/\">Back to Ferrofin</a>",
    ] {
        assert!(page.html.contains(part), "{part}: {}", page.html);
    }
    // The start the form goes to sends the browser on to the provider, whose
    // issuer here is `http`.
    assert_eq!(
        page.content_security_policy.as_deref(),
        Some(
            "default-src 'none'; base-uri 'none'; form-action 'self' https: http:; \
             frame-ancestors 'none'"
        )
    );
    assert_eq!(page.set_cookies, Vec::<String>::new());

    approval.quick_connect(false);
    let off = approval
        .harness
        .sso
        .quick_connect_page()
        .await
        .expect("a page");
    assert_eq!(off.status, 403);
    assert!(
        off.html
            .contains("Quick Connect is turned off on this server."),
        "{}",
        off.html
    );

    // B1: with no provider that signs users in, there is no such page.
    let none = Approval::with(|provider| provider.enabled = false).await;
    let refused = none.harness.sso.quick_connect_page().await;
    assert!(
        matches!(refused, Err(ServiceError::NotFound(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn start_refuses_what_is_not_a_code_and_quick_connect_turned_off_before_the_provider() {
    let approval = Approval::new().await;
    let sso = &approval.harness.sso;
    for code in ["12345", "<b>123456</b>", ""] {
        let reply = sso
            .start_oidc_quick_connect("fake", CLIENT, code)
            .await
            .expect("an answer");
        assert_eq!(reply.status, 400, "{code:?}");
        assert_eq!(reply.location, None, "{code:?}");
        assert!(reply.set_cookies.is_empty(), "{code:?}");
        assert!(reply.html.contains("six digits"), "{}", reply.html);
        assert!(!reply.html.contains("<b>"), "{}", reply.html);
    }
    approval.quick_connect(false);
    let off = sso
        .start_oidc_quick_connect("fake", CLIENT, "123456")
        .await
        .expect("an answer");
    assert_eq!(off.status, 403);
    assert!(
        off.html.contains("<a href=\"../../../web/\">"),
        "{}",
        off.html
    );
    assert!(sso.flows.is_empty(), "no flow started");

    // An unknown provider is no such route, whatever the code.
    let unknown = sso.start_oidc_quick_connect("nope", CLIENT, "x").await;
    assert!(
        matches!(unknown, Err(ServiceError::NotFound(_))),
        "{unknown:?}"
    );

    // A code starts a flow for it.
    approval.quick_connect(true);
    let started = Harness::started(
        sso.start_oidc_quick_connect("fake", CLIENT, " 123456 ")
            .await
            .expect("starts"),
    );
    let state = url::Url::parse(&started.location)
        .expect("a URL")
        .query_pairs()
        .find(|(name, _)| name == "state")
        .map(|(_, state)| state.into_owned())
        .expect("a state");
    let flow = sso.flows.take(&state).expect("a flow");
    assert_eq!(
        flow.purpose,
        FlowPurpose::QuickConnect(QuickConnectCode::parse("123456").expect("a code"))
    );
}

#[tokio::test]
async fn only_the_confirmation_with_its_cookie_approves_the_code_and_only_once() {
    let approval = Approval::new().await;
    let (code, secret) = approval.initiate().await;

    let (page, cookie) = approval.called_back(&code).await;
    assert_eq!(page.status, 200, "{}", page.html);
    assert!(
        page.html.contains(&format!(
            "Sign in the device showing code <strong>{code}</strong> as \
             <strong>{USERNAME}</strong>?"
        )),
        "{}",
        page.html
    );
    assert!(
        page.html
            .contains("<form method=\"post\" action=\"../../quickconnect\">"),
        "{}",
        page.html
    );
    assert_eq!(
        page.set_cookies,
        [format!(
            "ferrofin_sso_flow={cookie}; Path=/jf/sso; Max-Age=60; HttpOnly; SameSite=Lax; Secure"
        )]
    );
    assert_eq!(
        page.content_security_policy.as_deref(),
        Some(CONFIRMATION_POLICY)
    );
    let confirmation = confirmation_of(&page.html);
    assert!(!approval.authorized(&secret).await, "not before it");

    // Another browser's cookie is refused, and that uses the confirmation up.
    let elsewhere = approval
        .confirm(Some(&confirmation), Some("another-browser"))
        .await;
    assert_eq!(elsewhere.status, 400, "{}", elsewhere.html);
    assert_eq!(elsewhere.set_cookies, [CLEARED]);
    let after = approval.confirm(Some(&confirmation), Some(&cookie)).await;
    assert_eq!(after.status, 400);
    assert!(!approval.authorized(&secret).await);

    // Signed in again and confirmed with its cookie, the code is approved
    // as the account, on the app's device.
    let (page, cookie) = approval.called_back(&code).await;
    let confirmation = confirmation_of(&page.html);
    let approved = approval.confirm(Some(&confirmation), Some(&cookie)).await;
    assert_eq!(approved.status, 200, "{}", approved.html);
    assert!(
        approved
            .html
            .contains("Your app is signed in. You can close this page."),
    );
    assert_eq!(approved.set_cookies, [CLEARED]);
    assert!(approval.authorized(&secret).await);
    let session = approval
        .quick_connect
        .get_authorized_request(&secret)
        .await
        .expect("a session");
    assert_eq!(session.user.name.as_deref(), Some(USERNAME));
    assert_eq!(session.session.device_id.as_deref(), Some("qc-app"));

    // A replay does nothing, and a form without a confirmation neither.
    let replayed = approval.confirm(Some(&confirmation), Some(&cookie)).await;
    assert_eq!(replayed.status, 400);
    assert_eq!(approval.confirm(None, Some(&cookie)).await.status, 400);
    let sessions = approval
        .harness
        .sessions
        .get_session_summaries()
        .await
        .expect("sessions");
    assert_eq!(sessions.len(), 1, "the app's only");
}

#[tokio::test]
async fn a_confirmation_opens_no_browser_session_and_an_exchange_code_approves_nothing() {
    let approval = Approval::new().await;
    let (code, secret) = approval.initiate().await;
    let (page, cookie) = approval.called_back(&code).await;
    let confirmation = confirmation_of(&page.html);
    let exchanged = approval
        .harness
        .exchange(&confirmation, Some(&cookie), device("browser"))
        .await;
    assert_eq!(
        exchanged.outcome,
        Err(OidcExchangeRefusal {
            status: 400,
            message: "Sign-in failed. Try again.".to_owned(),
        })
    );
    assert!(approval.harness.sso.exchanges.is_empty(), "used up");
    assert!(!approval.authorized(&secret).await);

    // A browser sign-in's exchange code is no confirmation either.
    let (page, cookie) = approval.harness.completed().await;
    let refused = approval
        .confirm(Some(&exchange_code(&page.html)), Some(&cookie))
        .await;
    assert_eq!(refused.status, 400, "{}", refused.html);
    assert!(!approval.authorized(&secret).await);
    let sessions = approval
        .harness
        .sessions
        .get_session_summaries()
        .await
        .expect("sessions");
    assert!(sessions.is_empty(), "{sessions:?}");
}

/// B7: the account rules of a password sign-in, for the approving browser,
/// then device access as `authenticate_direct` applies it to the app's
/// device: each refuses with its message and writes `AuthenticationFailed`.
#[tokio::test]
async fn each_account_rule_refuses_the_approval_and_is_logged() {
    let approval = Approval::new().await;
    let (code, secret) = approval.initiate().await;
    let rules: [(Rule, &str); 3] = [
        (
            |policy| policy.is_disabled = true,
            "The fake-user account is currently disabled. Please consult with your administrator.",
        ),
        // Judged on the approving browser's address, a remote one.
        (
            |policy| policy.enable_remote_access = false,
            "remote access is not permitted for this user",
        ),
        (
            |policy| {
                policy.enable_all_devices = false;
                policy.enabled_devices = Vec::new();
            },
            "user is not allowed access from this device",
        ),
    ];
    for (edit, message) in rules {
        let (page, cookie) = approval.called_back(&code).await;
        let confirmation = confirmation_of(&page.html);
        let allowed = approval.edit_policy(edit).await;
        let refused = approval.confirm(Some(&confirmation), Some(&cookie)).await;
        assert_eq!(refused.status, 403, "{}", refused.html);
        assert!(
            refused.html.contains(&escape_html(message)),
            "{message}: {}",
            refused.html
        );
        assert_eq!(refused.set_cookies, [CLEARED]);
        let log = approval
            .harness
            .activity
            .get_paged_result(&ActivityLogQuery::default())
            .await
            .expect("the log")
            .items;
        assert_eq!(log[0].type_, "AuthenticationFailed", "{message}");
        assert_eq!(log[0].name, format!("Failed login attempt from {USERNAME}"));
        assert!(!approval.authorized(&secret).await, "{message}");
        approval.edit_policy(move |policy| *policy = allowed).await;
    }
}

#[tokio::test]
async fn an_unknown_or_approved_code_and_quick_connect_turned_off_are_refused_at_the_confirmation()
{
    let approval = Approval::new().await;

    // No app waits with this code.
    let (page, cookie) = approval.called_back("111111").await;
    let unknown = approval
        .confirm(Some(&confirmation_of(&page.html)), Some(&cookie))
        .await;
    assert_eq!(unknown.status, 400);
    assert!(
        unknown.html.contains("No app is waiting with this code"),
        "{}",
        unknown.html
    );

    // Two sign-ins for one code: the second finds it approved.
    let (code, _) = approval.initiate().await;
    let (first, first_cookie) = approval.called_back(&code).await;
    let (second, second_cookie) = approval.called_back(&code).await;
    let approved = approval
        .confirm(Some(&confirmation_of(&first.html)), Some(&first_cookie))
        .await;
    assert_eq!(approved.status, 200, "{}", approved.html);
    let again = approval
        .confirm(Some(&confirmation_of(&second.html)), Some(&second_cookie))
        .await;
    assert_eq!(again.status, 400);
    assert!(
        again.html.contains("This code is approved already."),
        "{}",
        again.html
    );

    // Quick Connect turned off since the callback.
    let (code, secret) = approval.initiate().await;
    let (page, cookie) = approval.called_back(&code).await;
    approval.quick_connect(false);
    let off = approval
        .confirm(Some(&confirmation_of(&page.html)), Some(&cookie))
        .await;
    assert_eq!(off.status, 403);
    assert!(off.html.contains("<a href=\"../web/\">"), "{}", off.html);
    approval.quick_connect(true);
    assert!(!approval.authorized(&secret).await);
}

#[tokio::test]
async fn an_identity_without_an_account_is_refused_before_any_confirmation() {
    let approval = Approval::with(|provider| provider.create_users = false).await;
    let (code, _) = approval.initiate().await;
    let (page, _) = approval.called_back(&code).await;
    assert_eq!(page.status, 403, "{}", page.html);
    assert!(
        page.html.contains("does not create accounts"),
        "{}",
        page.html
    );
    assert_eq!(page.set_cookies, [CLEARED]);
    assert!(approval.harness.sso.exchanges.is_empty());
}

#[tokio::test]
async fn without_a_quick_connect_manager_there_is_no_approval() {
    let harness = Harness::new().await;
    let sso = &harness.sso;
    let page = sso.quick_connect_page().await;
    let start = sso.start_oidc_quick_connect("fake", CLIENT, "123456").await;
    let confirm = sso
        .confirm_oidc_quick_connect(Some(Secret::new("c")), None, CLIENT)
        .await;
    for answer in [page, start, confirm] {
        assert!(
            matches!(answer, Err(ServiceError::NotFound(_))),
            "{answer:?}"
        );
    }
}

#[tokio::test]
async fn an_account_without_remote_access_approves_from_the_local_network() {
    let approval = Approval::new().await;
    let (code, secret) = approval.initiate().await;
    let (page, cookie) = approval.called_back(&code).await;
    approval
        .edit_policy(|policy| policy.enable_remote_access = false)
        .await;
    let local = approval
        .confirm_from(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            Some(&confirmation_of(&page.html)),
            Some(&cookie),
        )
        .await;
    assert_eq!(local.status, 200, "{}", local.html);
    assert!(approval.authorized(&secret).await);
}

/// What changed between the callback and the confirmation: a confirmation
/// past its minute, the provider disabled, the account deleted. Each is
/// refused, and the code stays unapproved.
#[tokio::test]
async fn what_changed_since_the_callback_refuses_the_approval() {
    let approval = Approval::new().await;
    let sso = &approval.harness.sso;
    let (code, secret) = approval.initiate().await;

    let (page, cookie) = approval.called_back(&code).await;
    let confirmation = confirmation_of(&page.html);
    let mut pending = sso.exchanges.take(&confirmation).expect("pending");
    pending.created = Instant::now()
        .checked_sub(EXCHANGE_CODE_LIFETIME + Duration::from_secs(1))
        .expect("the clock reaches back");
    sso.exchanges.insert(confirmation.clone(), pending);
    let late = approval.confirm(Some(&confirmation), Some(&cookie)).await;
    assert_eq!(late.status, 400, "{}", late.html);

    let (page, cookie) = approval.called_back(&code).await;
    let mut provider = approval.harness.provider("fake").await;
    provider.enabled = false;
    sso.save_oidc_provider("fake", provider.clone())
        .await
        .expect("saved");
    let disabled = approval
        .confirm(Some(&confirmation_of(&page.html)), Some(&cookie))
        .await;
    assert_eq!(disabled.status, 400, "{}", disabled.html);
    provider.enabled = true;
    sso.save_oidc_provider("fake", provider)
        .await
        .expect("saved");

    let (page, cookie) = approval.called_back(&code).await;
    let user = approval.account().await;
    // The last account cannot be deleted.
    approval
        .harness
        .users
        .create_user("keeper")
        .await
        .expect("created");
    approval
        .harness
        .users
        .delete_user(Uuid::parse_str(&user.id).expect("an id"))
        .await
        .expect("deleted");
    let deleted = approval
        .confirm(Some(&confirmation_of(&page.html)), Some(&cookie))
        .await;
    assert_eq!(deleted.status, 400, "{}", deleted.html);
    assert!(!approval.authorized(&secret).await);
}

/// B8: an identity without the provider's required role approves nothing,
/// and no account is made for it.
#[tokio::test]
async fn without_the_required_role_no_code_is_approved_and_no_account_made() {
    let approval = Approval::with(|provider| USERS.clone_into(&mut provider.required_role)).await;
    let (code, secret) = approval.initiate().await;
    approval.groups(json!(["someone-else"]));
    let (page, cookie) = approval.called_back(&code).await;
    assert_eq!(page.status, 403, "{}", page.html);
    assert!(
        page.html
            .contains("does not have the role this server requires"),
        "{}",
        page.html
    );
    assert_eq!(page.set_cookies, [CLEARED]);
    assert!(approval.harness.sso.exchanges.is_empty(), "no confirmation");
    let account = approval
        .harness
        .users
        .get_user_by_name(USERNAME)
        .await
        .expect("lookup");
    assert!(account.is_none(), "no account");
    // Nothing was offered to confirm.
    assert_eq!(approval.confirm(None, Some(&cookie)).await.status, 400);
    assert!(!approval.authorized(&secret).await);

    // With the role, the same code is asked about.
    approval.groups(json!([USERS]));
    let (page, _) = approval.called_back(&code).await;
    assert_eq!(page.status, 200, "{}", page.html);
}

/// B9: approving a code is a sign-in on the app's device, so role sync
/// keeps the administrator flag in step with the role claim, both ways.
#[tokio::test]
async fn role_sync_applies_when_a_code_is_approved() {
    let approval = Approval::with(|provider| {
        provider.role_sync = true;
        ADMINS.clone_into(&mut provider.admin_role);
    })
    .await;
    // A local administrator, so the fake's account is never the last one.
    let keeper = approval
        .harness
        .users
        .create_user("keeper")
        .await
        .expect("created");
    let mut policy = approval
        .harness
        .users
        .get_user_dto(&keeper, None)
        .await
        .expect("a DTO")
        .policy
        .expect("a policy");
    policy.is_administrator = true;
    approval
        .harness
        .users
        .update_policy(Uuid::parse_str(&keeper.id).expect("an id"), &policy)
        .await
        .expect("saved");

    approval.groups(json!([ADMINS]));
    let (_, reply) = approval.approved().await;
    assert_eq!(reply.status, 200, "{}", reply.html);
    assert!(approval.is_administrator().await, "promoted");

    approval.groups(json!([]));
    let (_, reply) = approval.approved().await;
    assert_eq!(reply.status, 200, "{}", reply.html);
    assert!(!approval.is_administrator().await, "demoted");
}

/// B12: a link removed since the callback approves nothing, as an exchange
/// code issued before a removal opens no session.
#[tokio::test]
async fn a_link_removed_since_the_callback_refuses_the_approval() {
    let approval = Approval::new().await;
    let (code, secret) = approval.initiate().await;
    let (page, cookie) = approval.called_back(&code).await;
    assert_eq!(page.status, 200, "{}", page.html);
    let user = approval.account().await;
    approval
        .harness
        .sso
        .remove_oidc_link(
            Uuid::parse_str(&user.id).expect("an id"),
            ferrofin_traits::sso::OidcLinkAt::Provider("fake"),
        )
        .await
        .expect("removed");

    let reply = approval
        .confirm(Some(&confirmation_of(&page.html)), Some(&cookie))
        .await;
    assert_eq!(reply.status, 400, "{}", reply.html);
    assert!(!approval.authorized(&secret).await);
}

/// B12: a removal that starts while an approval is minting the app's session
/// waits for it, then signs that session out: the approval holds the lock the
/// removal unlinks under from its lookup of the link to its session.
#[tokio::test]
async fn a_removal_during_an_approval_signs_the_app_out() {
    let approval = Approval::new().await;
    let (code, _secret) = approval.initiate().await;
    let (page, cookie) = approval.called_back(&code).await;
    assert_eq!(page.status, 200, "{}", page.html);
    let user = Uuid::parse_str(&approval.account().await.id).expect("an id");
    // The session's success event stalls the approval after the app's
    // session is minted, until released.
    let minting = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (entered, waiting) = (Arc::clone(&minting), Arc::clone(&release));
    approval.harness.events.subscribe(
        "AuthenticationSucceeded",
        Arc::new(move |_: &str| -> ferrofin_traits::events::EventFuture {
            let (entered, waiting) = (Arc::clone(&entered), Arc::clone(&waiting));
            Box::pin(async move {
                entered.notify_one();
                waiting.notified().await;
                Ok(())
            })
        }),
    );

    let confirmation = confirmation_of(&page.html);
    let removal = async {
        tokio::time::timeout(Duration::from_secs(30), minting.notified())
            .await
            .expect("the approval reaches its success event");
        let removing = approval
            .harness
            .sso
            .remove_oidc_link(user, ferrofin_traits::sso::OidcLinkAt::Provider("fake"));
        tokio::pin!(removing);
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut removing)
                .await
                .is_err(),
            "the removal waits for the approval"
        );
        release.notify_one();
        removing.await
    };
    let (reply, removed) = tokio::join!(
        approval.confirm(Some(&confirmation), Some(&cookie)),
        removal
    );
    removed.expect("removed");
    assert_eq!(
        reply.status, 200,
        "approved before the removal: {}",
        reply.html
    );
    let sessions = approval
        .harness
        .sessions
        .get_session_summaries()
        .await
        .expect("sessions");
    assert!(sessions.is_empty(), "and signed out by it: {sessions:?}");
}
