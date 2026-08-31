//! The cross-user authorization rule, as one table.
//!
//! C# `RequestHelpers.GetUserId` (v10.11.8 `Jellyfin.Api/Helpers/RequestHelpers.cs`
//! lines 67-85) has two halves: an absent/all-zero `userId` falls back to the
//! authenticated caller, and a `userId` naming **another** user is honoured only
//! when the caller is an administrator — otherwise `SecurityException("Forbidden")`,
//! which the exception middleware maps to `403`.
//!
//! Ferrofin had ported only the first half across **thirteen** ungated handler
//! sites, so any authenticated account could read and write another account's
//! data by passing its guid (proven live: a non-admin overwrote the
//! administrator's display-preferences row). A fourteenth site, `GET /Devices`,
//! kept its hand-rolled fallback behind the elevation policy — which neutralises
//! only the administrator half — and so still mis-handled the all-zero guid
//! (`404` here against `200` upstream) until it was routed through the same
//! helper. Every one of the fourteen now goes through the one
//! `handlers::items::effective_user_id`, and this file is the table that keeps
//! them there: for each route, a non-administrator naming another user must be
//! refused, while the self path and the administrator path must still work.
//!
//! The count in this sentence is load-bearing — it is the drift alarm for the
//! sweep. Adding a handler that takes a `userId` without routing it through
//! `effective_user_id` is the defect this file exists to catch.
//!
//! Routes whose *post-gate* body needs a live manager are covered in their own
//! domain files (`display_preferences.rs`, `quick_connect.rs`, `playlists.rs`,
//! `session_playstate.rs`, `channels.rs`, `audio.rs`); this table covers the
//! ones whose bodies are inert here, and stands as the drift alarm for the rule
//! itself.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeActivity, FakeApiKeys, FakeAppHost, FakeClientEventLogger, FakeCollections, FakeConfig,
    FakeDevices, FakeDisplayPreferences, FakeDto, FakeFileSystem, FakeLibrary, FakeLocalization,
    FakeLyrics, FakeMediaSegments, FakeMediaSources, FakeMusic, FakePlaylists, FakeProviders,
    FakeQuickConnect, FakeSearch, FakeSessions, FakeSimilarItems, FakeSubtitles, FakeSystem,
    FakeTasks, FakeTrickplay, FakeTvSeries, FakeUserData, FakeUserViews, PolicyUsers,
    fake_user_entity,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::UserManager;
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::AuthorizationInfo;
use tower::ServiceExt;
use uuid::Uuid;

/// The authenticated caller.
const CALLER_ID: Uuid = Uuid::from_u128(0x00CA_0000);
/// Another account, never the caller — the target of every cross-user probe.
const OTHER_ID: Uuid = Uuid::from_u128(0x00CA_0001);

/// An auth pair authenticating as a concrete user, so the gate has both a
/// caller identity and a role to resolve.
struct UserAuth(Uuid);

impl UserAuth {
    fn info(&self) -> AuthorizationInfo {
        AuthorizationInfo {
            token: Some("tok".into()),
            user: Some(fake_user_entity(self.0, "caller")),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        }
    }
}

#[async_trait]
impl AuthService for UserAuth {
    async fn authenticate(&self, _r: &RequestContext) -> Result<AuthorizationInfo, ServiceError> {
        Ok(self.info())
    }
}

#[async_trait]
impl AuthorizationContext for UserAuth {
    async fn get_authorization_info(
        &self,
        _r: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(self.info())
    }
}

/// An [`AppState`] whose caller is [`CALLER_ID`] and whose policy comes from
/// `users` — see [`caller_policy`], where `is_administrator` is the one field
/// the cases below vary.
fn state(users: Arc<dyn UserManager>) -> AppState {
    let auth = Arc::new(UserAuth(CALLER_ID));
    AppState::new(
        Arc::new(FakeLibrary),
        users,
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        auth.clone(),
        auth,
        Arc::new(FakeQuickConnect),
        Arc::new(FakePlaylists),
        Arc::new(FakeCollections),
        Arc::new(FakeTvSeries),
        Arc::new(FakeSubtitles),
        Arc::new(FakeLyrics),
        Arc::new(FakeMediaSegments),
        Arc::new(FakeTrickplay),
        Arc::new(FakeDevices),
        Arc::new(FakeClientEventLogger),
        Arc::new(FakeApiKeys),
        Arc::new(FakeLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(FakeActivity),
        Arc::new(FakeFileSystem),
        Arc::new(FakeTasks),
    )
}

/// Every gated route this table owns, as a `{user}` template.
///
/// The Live TV pair is served with no `LiveTvManager` wired, so the *body* is a
/// `501` — which is exactly what makes them usable here: the gate must fire
/// before the seam is consulted, so the refused leg is `403` and the permitted
/// legs are anything but.
///
/// Those two also sit behind a SECOND, independent gate that this table is not
/// about: `LiveTvController` is `[Authorize(Policy = Policies.LiveTvAccess)]`
/// (v10.11.8 `ApiServiceCollectionExtensions.cs:80`), so a caller without
/// `EnableLiveTvAccess` is refused before `RequestHelpers.GetUserId` is
/// reached. `caller_policy` below grants it — the permission's own allow/deny
/// pair is owned by `live_tv_permissions.rs`, and asserting it here again would
/// make this table's `403`s ambiguous about which rule produced them.
const GATED: &[&str] = &[
    "/Channels?userId={user}",
    "/Channels/00000000-0000-0000-0000-0000000000c1/Items?userId={user}",
    "/Channels/Items/Latest?userId={user}",
    "/LiveTv/Recordings/Folders?userId={user}",
    "/LiveTv/Recordings/00000000-0000-0000-0000-0000000000d1?userId={user}",
];

/// The caller's policy: a stock account's Live TV and remote defaults
/// (`UserEntityExtensions.cs:187`, `UserPolicy::default()`), with
/// administrator set per case. Only the cross-user half of the rule varies
/// between the tests below.
fn caller_policy(admin: bool) -> ferrofin_model::users::UserPolicy {
    ferrofin_model::users::UserPolicy {
        is_administrator: admin,
        enable_live_tv_access: true,
        enable_remote_access: true,
        ..ferrofin_model::users::UserPolicy::default()
    }
}

/// A users manager answering [`caller_policy`].
fn users(admin: bool) -> Arc<dyn UserManager> {
    Arc::new(PolicyUsers(caller_policy(admin)))
}

async fn get(app: AppState, uri: &str) -> StatusCode {
    create_router(app)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("X-Emby-Token", "tok")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response")
        .status()
}

#[tokio::test]
async fn a_non_administrator_naming_another_user_is_refused() {
    for template in GATED {
        let uri = template.replace("{user}", &OTHER_ID.to_string());
        assert_eq!(
            get(state(users(false)), &uri).await,
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn an_administrator_naming_another_user_is_served() {
    for template in GATED {
        let uri = template.replace("{user}", &OTHER_ID.to_string());
        assert_ne!(
            get(state(users(true)), &uri).await,
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn a_non_administrator_naming_their_own_id_is_served() {
    for template in GATED {
        let uri = template.replace("{user}", &CALLER_ID.to_string());
        assert_ne!(
            get(state(users(false)), &uri).await,
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
}

/// The all-zero guid is "not provided" upstream (`userId.IsNullOrEmpty()`), so
/// it must fall back to the caller rather than trip the gate — the first half of
/// the rule, which a naive `requested != caller` check would break.
#[tokio::test]
async fn an_all_zero_user_id_falls_back_to_the_caller() {
    for template in GATED {
        let uri = template.replace("{user}", &Uuid::nil().to_string());
        assert_ne!(
            get(state(users(false)), &uri).await,
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
}

/// Drift alarm: five routes are covered by this table. Gating another one
/// (or dropping a gate) must update it.
#[test]
fn the_gated_table_covers_five_routes() {
    assert_eq!(GATED.len(), 5, "gated-route table drifted");
}
