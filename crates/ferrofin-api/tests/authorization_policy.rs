//! Contract-wide guard: every operation answers each kind of caller as its
//! upstream authorization policy does.
//!
//! **The expected policy** is the operation's `security` scopes in the vendored
//! contracts. Jellyfin writes them itself (`SecurityRequirementsOperationFilter`):
//! the policy of every `[Authorize]` on the action and on its controller, plus
//! `DefaultAuthorization` wherever a named policy's requirement derives from
//! `DefaultAuthorizationRequirement`. Two things the filter does not see are
//! corrected here, each re-checkable against the C# at `UPSTREAM_TAG`:
//! - `[AllowAnonymous]` (it reads `[Authorize]` only): [`ALLOW_ANONYMOUS`],
//!   from `git grep -n 'AllowAnonymous\]' v12.4 -- Jellyfin.Api/Controllers`.
//! - policy changes since 10.11.8: none.
//!   `git diff v10.11.8 v12.4 -- Jellyfin.Api/Controllers | grep -E '^[+-].*\[(Authorize|AllowAnonymous)'`
//!   lists only actions 12.x removed or added. The 12.1.0 contract agrees with
//!   10.11.8 on every operation the two share, which this test asserts.
//!
//! The 10.11.8 contract also holds operations of plugins installed when it was
//! captured (OpenSubtitles, Intro Skipper); for those the oracle is the
//! plugin's own controller, which the filter recorded the same way.
//!
//! **The callers**, on a server whose startup wizard is complete, each from a
//! remote address (so `LocalAccessOrRequiresElevation` is plain elevation):
//! see [`CALLERS`]. Each policy-named permission and each SyncPlay state is a
//! caller of its own, so a route gated by the wrong one of them shows. Each
//! operation is probed twice, with a JSON object and with no body (see
//! [`probe`]). A response of `401` or `403` is a rejection; any other status,
//! a fake manager's panic (`500`) included, means the request got past the
//! gate.
//!
//! What each policy predicts, from `ApiServiceCollectionExtensions.cs` and
//! the handlers behind it (`DefaultAuthorizationHandler`,
//! `UserPermissionHandler`, `FirstTimeSetupHandler`, `SyncPlayAccessHandler`):
//! see [`expected`].

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::test_support::{
    FakeAuthContext, FakeAuthService, FixedAuth, PolicyUsers, required_query, sample_user,
    setup_complete_state_as,
};
use ferrofin_model::sync_play::GroupInfoDto;
use ferrofin_model::users::{AccessSchedule, DynamicDayOfWeek, UserPolicy};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::net::{AuthService, AuthorizationContext};
use ferrofin_traits::options::AuthorizationInfo;
use ferrofin_traits::stubs::{
    PlaybackRequest, SyncPlayManager, SyncPlayNowPlaying, SyncPlaySession,
};
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

/// The address every probe comes from: outside every local network.
const REMOTE: &str = "203.0.113.9:40000";

/// Operations upstream marks `[AllowAnonymous]`, which overrides every
/// `[Authorize]` on them and which the contract's `security` does not show.
const ALLOW_ANONYMOUS: &[(&str, &str, &str)] = &[(
    "GET",
    "/Plugins/{pluginId}/{version}/Image",
    "PluginsController.GetPluginImage",
)];

/// Operations as `(METHOD, path)`, the callers (by [`Caller::name`]) whose
/// answer differs from the policy's on each of them, the answer they get
/// instead, and why.
type Exception = (
    &'static [(&'static str, &'static str)],
    &'static [&'static str],
    Outcome,
    &'static str,
);

/// The anonymous stream routes that hold a named user to its account rules.
const USER_HELD_STREAMS: &[(&str, &str)] = &[
    ("GET", "/Audio/{itemId}/stream"),
    ("GET", "/Audio/{itemId}/stream.{container}"),
    ("HEAD", "/Audio/{itemId}/stream"),
    ("HEAD", "/Audio/{itemId}/stream.{container}"),
    ("GET", "/Videos/{itemId}/stream"),
    ("GET", "/Videos/{itemId}/stream.{container}"),
    ("HEAD", "/Videos/{itemId}/stream"),
    ("HEAD", "/Videos/{itemId}/stream.{container}"),
    (
        "GET",
        "/Videos/{routeItemId}/{routeMediaSourceId}/Subtitles/{routeIndex}/Stream.{routeFormat}",
    ),
    (
        "GET",
        "/Videos/{routeItemId}/{routeMediaSourceId}/Subtitles/{routeIndex}/{routeStartPositionTicks}/Stream.{routeFormat}",
    ),
    (
        "GET",
        "/Videos/{videoId}/{mediaSourceId}/Attachments/{index}",
    ),
];

/// Answers that differ from what the policy predicts. Every pair must still
/// occur with that answer: a stale entry fails the test.
const EXCEPTIONS: &[Exception] = &[
    (
        &[
            ("GET", "/Audio/{itemId}/stream"),
            ("GET", "/Audio/{itemId}/stream.{container}"),
            ("GET", "/Videos/{itemId}/stream"),
            ("GET", "/Videos/{itemId}/stream.{container}"),
        ],
        &["anonymous"],
        Outcome::Unauthorized,
        "Stricter by owner decision (gl-1089): a GET that would start a transcode \
         needs a caller; upstream's stream actions are anonymous",
    ),
    (
        USER_HELD_STREAMS,
        &["out of schedule", "remote admin"],
        Outcome::Forbidden,
        "Stricter by owner decision (gl-1090): a request naming a user is held to \
         that user's account rules on the anonymous stream routes",
    ),
    (
        &[
            ("GET", "/Plugins"),
            ("DELETE", "/Plugins/{pluginId}"),
            ("GET", "/Plugins/{pluginId}/Configuration"),
            ("POST", "/Plugins/{pluginId}/Configuration"),
            ("POST", "/Plugins/{pluginId}/Manifest"),
            ("DELETE", "/Plugins/{pluginId}/{version}"),
            ("POST", "/Plugins/{pluginId}/{version}/Disable"),
            ("POST", "/Plugins/{pluginId}/{version}/Enable"),
        ],
        &["remote admin"],
        Outcome::Forbidden,
        "Known divergence, gl-1699: these run the default policy's remote-access \
         check, which PluginsController's bare RequiresElevation does not",
    ),
];

/// A kind of caller each operation is sent. Every signed-in caller's other
/// permissions (playback, deletion, device control, …) are on, so a
/// handler's own permission checks do not answer first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // one field per fact a policy reads
struct Caller {
    /// The name failures and [`EXCEPTIONS`] use.
    name: &'static str,
    /// Whether the request carries a token at all.
    signed_in: bool,
    /// `IsAdministrator`.
    admin: bool,
    /// `EnableRemoteAccess`.
    remote_access: bool,
    /// Whether now is inside the account's access schedule.
    in_schedule: bool,
    /// The [`USER_PERMISSIONS`] policies the account's permissions satisfy.
    permissions: &'static [&'static str],
    /// The user row's `SyncPlayAccess`: 0 creates and joins groups, 1 joins,
    /// 2 has no access.
    sync_play_access: i32,
    /// Whether the user is in a SyncPlay group.
    in_group: bool,
}

/// An ordinary signed-in user with none of the policy-named permissions and
/// no SyncPlay access.
const USER: Caller = Caller {
    name: "user",
    signed_in: true,
    admin: false,
    remote_access: true,
    in_schedule: true,
    permissions: &[],
    sync_play_access: 2,
    in_group: false,
};

/// [`USER`] holding only the permission that `policy` names.
const fn holding(policy: &'static [&'static str]) -> Caller {
    Caller {
        name: policy[0],
        permissions: policy,
        ..USER
    }
}

/// A user with every permission, in a SyncPlay group.
const PERMITTED: Caller = Caller {
    name: "permitted",
    permissions: USER_PERMISSIONS,
    sync_play_access: 0,
    in_group: true,
    ..USER
};

/// An administrator with none of the permissions a policy names, no SyncPlay
/// access, in no group and outside its schedule: every one of those policies
/// must let it through on its administrator arm.
const ADMIN: Caller = Caller {
    name: "admin",
    admin: true,
    in_schedule: false,
    ..USER
};

const CALLERS: [Caller; 14] = [
    Caller {
        name: "anonymous",
        signed_in: false,
        ..USER
    },
    USER,
    holding(&["LiveTvAccess"]),
    holding(&["LiveTvManagement"]),
    holding(&["SubtitleManagement"]),
    holding(&["LyricManagement"]),
    holding(&["CollectionManagement"]),
    holding(&["Download"]),
    Caller {
        name: "SyncPlay joiner",
        sync_play_access: 1,
        ..USER
    },
    Caller {
        name: "SyncPlay member without access",
        in_group: true,
        ..USER
    },
    PERMITTED,
    Caller {
        name: "out of schedule",
        in_schedule: false,
        ..PERMITTED
    },
    ADMIN,
    // The default policy refuses it remotely before its administrator arm.
    Caller {
        name: "remote admin",
        remote_access: false,
        ..ADMIN
    },
];

/// Policies that admit only an administrator (or an API key) once the
/// startup wizard is complete.
const ELEVATED: &[&str] = &[
    "RequiresElevation",
    "FirstTimeSetupOrElevated",
    "LocalAccessOrRequiresElevation",
];

/// `UserPermissionRequirement` policies: a user holding the permission each
/// names.
const USER_PERMISSIONS: &[&str] = &[
    "CollectionManagement",
    "Download",
    "LiveTvAccess",
    "LiveTvManagement",
    "LyricManagement",
    "SubtitleManagement",
];

/// Every policy [`expected`] models.
const KNOWN_POLICIES: &[&[&str]] = &[
    &["DefaultAuthorization"],
    ELEVATED,
    USER_PERMISSIONS,
    SYNC_PLAY,
    SCHEDULE_EXEMPT,
];

/// `SyncPlayAccessRequirement` policies, which [`meets`] decides.
const SYNC_PLAY: &[&str] = &[
    "SyncPlayHasAccess",
    "SyncPlayCreateGroup",
    "SyncPlayJoinGroup",
    "SyncPlayIsInGroup",
];

/// Policies whose requirement passes `validateParentalSchedule: false`
/// (`FirstTimeSetupRequirement`'s default, `IgnoreParentalControl`). None of
/// the operations naming one also carries a plain `[Authorize]`, whose
/// default requirement would check the schedule after all (v12.4:
/// `LocalizationController`, `LibraryController.GetLibraryOptionsInfo`,
/// `SystemController.GetSystemInfo`, `UserController.GetUserById`).
const SCHEDULE_EXEMPT: &[&str] = &[
    "FirstTimeSetupOrDefault",
    "FirstTimeSetupOrElevated",
    "FirstTimeSetupOrIgnoreParentalControl",
    "IgnoreParentalControl",
];

/// How a caller's request ends at the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Not rejected by authorization.
    Pass,
    /// `401`.
    Unauthorized,
    /// `403`.
    Forbidden,
}

/// Whether a non-administrator `caller` meets `policy`'s own requirement
/// (`UserPermissionHandler`, `SyncPlayAccessHandler`); any other policy asks
/// nothing more of a user.
fn meets(caller: Caller, policy: &str) -> bool {
    match policy {
        // `SyncPlayAccess is CreateAndJoinGroups or JoinGroups ||
        // IsUserActive(userId)`.
        "SyncPlayHasAccess" => caller.sync_play_access <= 1 || caller.in_group,
        "SyncPlayCreateGroup" => caller.sync_play_access == 0,
        "SyncPlayJoinGroup" => caller.sync_play_access <= 1,
        "SyncPlayIsInGroup" => caller.in_group,
        _ if USER_PERMISSIONS.contains(&policy) => caller.permissions.contains(&policy),
        _ => true,
    }
}

/// What `scopes` (an operation's policies) answer `caller`, in
/// `DefaultAuthorizationHandler`'s order.
fn expected(caller: Caller, scopes: &[String]) -> Outcome {
    let has = |set: &[&str]| scopes.iter().any(|s| set.contains(&s.as_str()));
    let refused = if scopes.is_empty() {
        false
    } else if !caller.signed_in {
        return Outcome::Unauthorized;
    } else if !caller.remote_access && has(&["DefaultAuthorization"]) {
        // Before the administrator arm; bare RequiresElevation is a role
        // claim only, with no DefaultAuthorizationRequirement.
        true
    } else if caller.admin {
        false
    } else {
        has(ELEVATED)
            || (!caller.in_schedule && !has(SCHEDULE_EXEMPT))
            || !scopes.iter().all(|policy| meets(caller, policy))
    };
    if refused {
        Outcome::Forbidden
    } else {
        Outcome::Pass
    }
}

/// The user policy behind `caller`.
fn policy(caller: Caller) -> UserPolicy {
    let holds = |policy| caller.permissions.contains(&policy);
    UserPolicy {
        is_administrator: caller.admin,
        enable_remote_access: caller.remote_access,
        enable_collection_management: holds("CollectionManagement"),
        enable_content_downloading: holds("Download"),
        enable_live_tv_access: holds("LiveTvAccess"),
        enable_live_tv_management: holds("LiveTvManagement"),
        enable_lyric_management: holds("LyricManagement"),
        enable_subtitle_management: holds("SubtitleManagement"),
        // No hour of any day is past 24.
        access_schedules: if caller.in_schedule {
            Vec::new()
        } else {
            vec![AccessSchedule {
                day_of_week: DynamicDayOfWeek::Everyday,
                start_hour: 25.0,
                end_hour: 26.0,
                ..AccessSchedule::default()
            }]
        },
        enable_user_preference_access: true,
        enable_remote_control_of_other_users: true,
        enable_shared_device_control: true,
        enable_media_playback: true,
        enable_audio_playback_transcoding: true,
        enable_video_playback_transcoding: true,
        enable_playback_remuxing: true,
        enable_content_deletion: true,
        enable_sync_transcoding: true,
        enable_media_conversion: true,
        enable_all_devices: true,
        enable_all_channels: true,
        enable_all_folders: true,
        enable_public_sharing: true,
        ..UserPolicy::default()
    }
}

/// A SyncPlay manager in which the caller is, or is not, in a group; anything
/// past the gate is a fake's panic.
struct SyncPlay {
    in_group: bool,
}

#[async_trait]
impl SyncPlayManager for SyncPlay {
    async fn new_group(
        &self,
        _session: &SyncPlaySession,
        _group_name: &str,
        _now_playing: Option<&SyncPlayNowPlaying>,
    ) -> Result<GroupInfoDto, ServiceError> {
        unimplemented!("fake")
    }
    async fn join_group(&self, _s: &SyncPlaySession, _g: Uuid) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn leave_group(&self, _s: &SyncPlaySession) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn list_groups(&self, _s: &SyncPlaySession) -> Result<Vec<GroupInfoDto>, ServiceError> {
        unimplemented!("fake")
    }
    async fn get_group(
        &self,
        _s: &SyncPlaySession,
        _g: Uuid,
    ) -> Result<GroupInfoDto, ServiceError> {
        unimplemented!("fake")
    }
    async fn handle_request(
        &self,
        _s: &SyncPlaySession,
        _r: PlaybackRequest,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn is_user_active(&self, _user_id: Uuid) -> Result<bool, ServiceError> {
        Ok(self.in_group)
    }
}

/// The router `caller`'s requests go through.
fn router(caller: Caller) -> Router {
    let policy = policy(caller);
    let (context, auth): (Arc<dyn AuthorizationContext>, Arc<dyn AuthService>) = if caller.signed_in
    {
        let user = ferrofin_db::entities::users::UserEntity {
            sync_play_access: caller.sync_play_access,
            // As the policy says (`AssertCanUpdateUser` reads the row).
            enable_user_preference_access: policy.enable_user_preference_access,
            ..sample_user()
        };
        let identity = Arc::new(FixedAuth(AuthorizationInfo {
            is_authenticated: true,
            token: Some("policy-sweep-token".into()),
            user: Some(Arc::new(user)),
            user_policy: Some(Arc::new(policy.clone())),
            ..AuthorizationInfo::default()
        }));
        (identity.clone(), identity)
    } else {
        (Arc::new(FakeAuthContext), Arc::new(FakeAuthService))
    };
    // Handlers that re-read the caller's policy see the same one.
    let users = Arc::new(PolicyUsers(policy));
    create_router(
        setup_complete_state_as(context, auth, users).with_sync_play(Arc::new(SyncPlay {
            in_group: caller.in_group,
        })),
    )
}

fn resolve<'a>(spec: &'a Value, schema: &'a Value) -> &'a Value {
    if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
        let name = r.trim_start_matches("#/components/schemas/");
        return resolve(spec, &spec["components"]["schemas"][name]);
    }
    for wrapper in ["allOf", "oneOf", "anyOf"] {
        if let Some([only]) = schema
            .get(wrapper)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
        {
            return resolve(spec, only);
        }
    }
    schema
}

/// The operation's path with every `{param}` filled from its declared type.
/// A uuid is the caller's own user id, so a route naming a user names them.
fn concrete_path(path: &str, operation: &Value, spec: &Value) -> String {
    let mut out = path.to_owned();
    for param in operation["parameters"].as_array().into_iter().flatten() {
        if param["in"].as_str() != Some("path") {
            continue;
        }
        let name = param["name"].as_str().unwrap_or_default();
        let schema = resolve(spec, &param["schema"]);
        let first_enum = schema["enum"]
            .as_array()
            .and_then(|values| values.first())
            .and_then(Value::as_str);
        let value = match (
            schema["type"].as_str(),
            schema["format"].as_str(),
            first_enum,
        ) {
            (_, _, Some(variant)) => variant,
            (_, Some("uuid"), _) => "11111111111111111111111111111111",
            (Some("integer" | "number"), _, _) => "1",
            _ => "x",
        };
        let placeholder = format!("{{{name}}}");
        if let Some(at) = out
            .to_ascii_lowercase()
            .find(&placeholder.to_ascii_lowercase())
        {
            out.replace_range(at..at + placeholder.len(), value);
        }
    }
    out
}

/// An operation to probe: its policies and the request that reaches it.
struct Operation {
    scopes: Vec<String>,
    uri: String,
}

/// Every operation of both vendored contracts, keyed by `(METHOD, path)`.
fn operations() -> BTreeMap<(String, String), Operation> {
    let mut out: BTreeMap<(String, String), Operation> = BTreeMap::new();
    for text in [
        include_str!("../../../contracts/jellyfin-openapi-10.11.8.json"),
        include_str!("../../../contracts/jellyfin-openapi-12.1.0.json"),
    ] {
        let spec: Value = serde_json::from_str(text).expect("spec");
        for (path, item) in spec["paths"].as_object().expect("paths") {
            for (method, operation) in item.as_object().expect("path item") {
                let method = method.to_ascii_uppercase();
                let mut scopes: Vec<String> = operation["security"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_object)
                    .flat_map(|requirement| requirement.values())
                    .filter_map(Value::as_array)
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                // A policy `expected` does not model would read as one that
                // asks nothing more of a user.
                for scope in &scopes {
                    assert!(
                        KNOWN_POLICIES
                            .iter()
                            .any(|set| set.contains(&scope.as_str())),
                        "{method} {path}: policy {scope} is not modelled"
                    );
                }
                if ALLOW_ANONYMOUS
                    .iter()
                    .any(|(m, p, _)| *m == method && *p == path)
                {
                    scopes.clear();
                }
                let key = (method.clone(), path.clone());
                if let Some(known) = out.get(&key) {
                    assert_eq!(
                        known.scopes, scopes,
                        "{method} {path}: the two contracts disagree on its policy"
                    );
                    continue;
                }
                let query = required_query(&method, path, operation, "");
                let path = concrete_path(path, operation, &spec);
                let uri = if query.is_empty() {
                    path
                } else {
                    format!("{path}?{query}")
                };
                out.insert(key, Operation { scopes, uri });
            }
        }
    }
    out
}

/// How the gate answered `caller`'s request, and the status it saw.
///
/// `json` sends a JSON object, as clients do; without it the request has no
/// body and no content type, which the binder refuses (415). Upstream's
/// `UseAuthorization` runs before the body is bound, so the gate must answer
/// first either way (gl-696).
async fn probe(router: &Router, method: &str, uri: &str, json: bool) -> (Outcome, StatusCode) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .extension(ConnectInfo(REMOTE.parse::<SocketAddr>().expect("address")));
    if json {
        request = request.header("Content-Type", "application/json");
    }
    let request = request
        .body(Body::from(if json { "{}" } else { "" }))
        .expect("request");
    let status = router
        .clone()
        .oneshot(request)
        .await
        .expect("response")
        .status();
    let outcome = match status {
        StatusCode::UNAUTHORIZED => Outcome::Unauthorized,
        StatusCode::FORBIDDEN => Outcome::Forbidden,
        _ => Outcome::Pass,
    };
    (outcome, status)
}

#[tokio::test]
async fn every_contract_operation_answers_each_caller_as_its_policy_does() {
    // A request past the gate reaches a fake that panics
    // (`unimplemented!("fake")`); the router answers 500, and its noise is
    // not the failure being reported.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !info
            .payload_as_str()
            .is_some_and(|m| m.contains("not implemented: fake"))
        {
            default_hook(info);
        }
    }));
    // Handlers past the gate may write under the fake paths, which are
    // relative (`POST /Startup/RemoteAccess` saves `named/network.json`): keep
    // that out of the source tree. This binary holds this one test only, so
    // changing the process's directory touches no other test.
    let scratch = tempfile::tempdir().expect("scratch directory");
    std::env::set_current_dir(scratch.path()).expect("enter scratch directory");
    let operations = operations();
    let mut failures = Vec::new();
    let mut excused = Vec::new();
    for caller in CALLERS {
        let router = router(caller);
        for (((method, path), operation), json) in
            operations.iter().flat_map(|op| [(op, true), (op, false)])
        {
            let want = expected(caller, &operation.scopes);
            let (got, status) = probe(&router, method, &operation.uri, json).await;
            if got == want {
                continue;
            }
            let excepted = EXCEPTIONS.iter().any(|(operations, callers, answer, _)| {
                *answer == got
                    && callers.contains(&caller.name)
                    && operations.iter().any(|(m, p)| m == method && p == path)
            });
            if excepted {
                excused.push((method.as_str(), path.as_str(), caller.name));
                continue;
            }
            failures.push(format!(
                "{method} {path} as {}{}: {status}, policy {:?} expects {want:?}",
                caller.name,
                if json { "" } else { " without a body" },
                operation.scopes
            ));
        }
    }
    let stale: Vec<_> = EXCEPTIONS
        .iter()
        .flat_map(|(operations, callers, _, _)| {
            operations
                .iter()
                .flat_map(move |(m, p)| callers.iter().map(move |c| (*m, *p, *c)))
        })
        .filter(|exception| !excused.contains(exception))
        .collect();
    println!(
        "authorization policy: {} operations x {} callers x 2 bodies",
        operations.len(),
        CALLERS.len()
    );
    assert!(
        failures.is_empty(),
        "{} failures: {failures:#?}",
        failures.len()
    );
    assert!(
        stale.is_empty(),
        "exceptions that no longer occur: {stale:#?}"
    );
}
