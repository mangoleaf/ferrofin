//! Session WebSocket (`/socket`) tests that need a **real** socket.
//!
//! The handler unit tests in `handlers::websocket` cover frame parsing and the
//! bounded push queue in isolation; these drive an actual TCP connection through
//! `axum::serve`, which is the only way to exercise the inbound framing (the
//! part affected by the socket's read-buffer capacity), the authentication
//! that decides whether the upgrade happens at all, and the event-driven
//! `Sessions` stream.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrofin_api::state::AppState;
use ferrofin_db::entities::security::DeviceEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::SessionInfoDto;
use ferrofin_model::session::{
    ClientCapabilities, GeneralCommand, MessageCommand, PlayRequest, PlaybackProgressInfo,
    PlaybackStartInfo, PlaybackStopInfo, PlaystateRequest, SessionMessageType, SessionUserInfo,
    TranscodingInfo,
};
use ferrofin_model::users::UserPolicy;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::net::{AuthService, RequestContext};
use ferrofin_traits::options::AuthorizationInfo;
use ferrofin_traits::session::{
    AuthenticationRequest, AuthenticationResultData, SessionEvent, SessionManager,
};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use uuid::Uuid;

/// Serves the real router over `state` on an ephemeral loopback port and
/// returns its address.
async fn serve(state: AppState) -> SocketAddr {
    let app = ferrofin_api::create_router(state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// Reads frames until a text frame arrives (ping/pong traffic is skipped).
async fn next_text<S>(socket: &mut S) -> String
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => return text.to_string(),
            Some(Ok(_)) => {}
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

/// The socket's inbound buffer is deliberately much smaller than a frame a
/// client is *allowed* to send (see `READ_BUFFER_BYTES` — it is sized for the
/// tiny `KeepAlive`/subscription vocabulary because it is memset on every push,
/// not for the largest legal frame). Shrinking it must not change what the
/// socket accepts: a frame many times the buffer has to be reassembled across
/// reads and answered exactly as a small one is.
#[tokio::test]
async fn a_frame_many_times_the_read_buffer_is_still_answered() {
    let addr = serve(ferrofin_api::test_support::authed_fake_state()).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/socket"))
        .await
        .expect("upgrade");

    // Control: the ordinary, tiny keep-alive is acked.
    socket
        .send(Message::text(r#"{"MessageType":"KeepAlive"}"#))
        .await
        .expect("send small");
    let small: serde_json::Value =
        serde_json::from_str(&next_text(&mut socket).await).expect("json");
    assert_eq!(small["MessageType"], "KeepAlive");

    // The same message padded to 256 KiB — 64x the read buffer, so tungstenite
    // must reserve the frame length and fill it over many reads.
    let padding = "p".repeat(256 * 1024);
    socket
        .send(Message::text(format!(
            r#"{{"MessageType":"KeepAlive","Pad":"{padding}"}}"#
        )))
        .await
        .expect("send large");
    let large: serde_json::Value =
        serde_json::from_str(&next_text(&mut socket).await).expect("json");
    assert_eq!(
        large["MessageType"], "KeepAlive",
        "a frame far larger than the read buffer must be reassembled and answered"
    );
    // The SDK's `MessageId` is a required `format: uuid` field, so it has to be
    // present on every outbound message — in Jellyfin's dashless "N" spelling,
    // which every `Guid` takes through `JsonGuidConverter`.
    assert!(
        large["MessageId"]
            .as_str()
            .is_some_and(|id| id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())),
        "every outbound message carries a dashless MessageId: {large}"
    );
}

/// The socket authenticates through the same `AuthService` as every HTTP
/// request (C# `WebSocketManager.WebSocketRequestHandler`), so whether an
/// Emby-era credential counts is `EnableLegacyAuthorization`'s call, not the
/// socket's. With an auth service that accepts nothing, every credential form,
/// including the `api_key` and bare-token headers the socket once read itself,
/// is upstream's `403` "Token is required", and the connection is never upgraded.
#[tokio::test]
async fn a_socket_the_auth_service_rejects_is_refused_before_the_upgrade() {
    let addr = serve(ferrofin_api::test_support::fake_state()).await;
    for (label, query, header) in [
        ("no credentials", "", None),
        ("?api_key=", "?api_key=tok", None),
        ("?ApiKey=", "?ApiKey=tok", None),
        ("X-Emby-Token", "", Some(("X-Emby-Token", "tok"))),
        (
            "X-MediaBrowser-Token",
            "",
            Some(("X-MediaBrowser-Token", "tok")),
        ),
        (
            "Authorization: MediaBrowser",
            "",
            Some(("Authorization", r#"MediaBrowser Token="tok""#)),
        ),
    ] {
        let mut request = format!("ws://{addr}/socket{query}")
            .into_client_request()
            .expect("request");
        if let Some((name, value)) = header {
            request
                .headers_mut()
                .insert(name, value.parse().expect("header value"));
        }
        match tokio_tungstenite::connect_async(request).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                assert_eq!(response.status(), 403, "{label}");
            }
            other => panic!("{label}: expected a 403 refusal, got {other:?}"),
        }
    }
}

/// A lost socket closes only when it has a session to end
/// (`SessionWebSocketListener.KeepAliveSockets` disposes it since Jellyfin
/// 12.0-rc3; every socket has a session there). One without a session (this
/// fake's auth carries no token, so it resolves none) was never greeted, so it is only dropped from the
/// watch and stays open past `WebSocketLostTimeout`. The clock is paused once
/// the upgrade is done, so the timeout passes at once.
#[tokio::test]
async fn a_lost_sessionless_socket_stays_open() {
    let addr = serve(ferrofin_api::test_support::authed_fake_state()).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/socket"))
        .await
        .expect("upgrade");
    tokio::time::pause();

    // Read the server's prompts, never answering, well past the lost timeout.
    let closed = tokio::time::timeout(std::time::Duration::from_secs(300), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;

    assert!(closed.is_err(), "a sessionless socket was closed when lost");
}

// ---------------------------------------------------------------------------
// The `Sessions` stream: sent on session events, throttled by the period
// (`BasePeriodicWebSocketListener.HandleMessages`, `SessionInfoWebSocketListener`).
// ---------------------------------------------------------------------------

/// The signed-in socket user ([`ferrofin_api::test_support::sample_user`]).
const CALLER: Uuid = Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111);
/// Another user, whose sessions a non-administrator must not see.
const OTHER: Uuid = Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222);

/// Authenticates every socket with a token as [`CALLER`], an administrator or
/// not, so the socket resolves to a session and may subscribe.
struct TokenAuth {
    admin: bool,
}

#[async_trait::async_trait]
impl AuthService for TokenAuth {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            is_authenticated: true,
            token: Some("socket-token".into()),
            user: Some(ferrofin_api::test_support::sample_user()),
            user_policy: Some(Arc::new(UserPolicy {
                is_administrator: self.admin,
                enable_remote_access: true,
                ..UserPolicy::default()
            })),
            ..AuthorizationInfo::default()
        })
    }
}

/// A session manager for the `Sessions` stream: every socket resolves to one
/// session of [`CALLER`], the events a test sends on `events` reach the
/// subscribers, and each `get_sessions` snapshot the stream builds is counted.
struct StreamingSessions {
    events: broadcast::Sender<SessionEvent>,
    sessions: Vec<SessionInfoDto>,
    builds: AtomicUsize,
}

impl StreamingSessions {
    fn new(sessions: Vec<SessionInfoDto>) -> Arc<Self> {
        Arc::new(Self {
            events: broadcast::channel(64).0,
            sessions,
            builds: AtomicUsize::new(0),
        })
    }

    fn raise(&self, event: SessionEvent) {
        self.events.send(event).expect("a subscribed socket");
    }

    fn builds(&self) -> usize {
        self.builds.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl SessionManager for StreamingSessions {
    fn subscribe_session_events(&self) -> Option<broadcast::Receiver<SessionEvent>> {
        Some(self.events.subscribe())
    }
    async fn get_session_by_authentication_token(
        &self,
        _token: &str,
        _device_id: &str,
        _remote_endpoint: &str,
    ) -> Result<SessionInfoDto, ServiceError> {
        Ok(SessionInfoDto {
            id: Some("socket-session".to_owned()),
            user_id: CALLER,
            ..SessionInfoDto::default()
        })
    }
    async fn get_sessions(
        &self,
        _user_id: Uuid,
        _device_id: Option<&str>,
        _active_within_seconds: Option<i32>,
        _controllable_user_to_check: Option<Uuid>,
        _is_api_key: bool,
    ) -> Result<Vec<SessionInfoDto>, ServiceError> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        Ok(self.sessions.clone())
    }
    async fn close_if_needed(&self, _session_id: &str) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn log_session_activity(
        &self,
        _app_name: &str,
        _app_version: &str,
        _device_id: &str,
        _device_name: &str,
        _remote_endpoint: &str,
        _user: Option<&UserEntity>,
    ) -> Result<SessionInfoDto, ServiceError> {
        unimplemented!("fake")
    }
    async fn update_device_name(&self, _session_id: &str, _name: &str) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn on_playback_start(&self, _info: &PlaybackStartInfo) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn on_playback_progress(
        &self,
        _info: &PlaybackProgressInfo,
        _is_automated: bool,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn on_playback_stopped(&self, _info: &PlaybackStopInfo) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn report_session_ended(&self, _session_id: &str) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_general_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &GeneralCommand,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_message_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &MessageCommand,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_play_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &PlayRequest,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_playstate_command(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
        _command: &PlaystateRequest,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_message_to_admin_sessions(
        &self,
        _message_type: SessionMessageType,
        _data: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_message_to_user_sessions(
        &self,
        _user_ids: &[Uuid],
        _message_type: SessionMessageType,
        _data: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_message_to_user_device_sessions(
        &self,
        _device_id: &str,
        _message_type: SessionMessageType,
        _data: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn send_restart_required_notification(&self) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn add_additional_user(
        &self,
        _session_id: &str,
        _user: Uuid,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn remove_additional_user(
        &self,
        _session_id: &str,
        _user: Uuid,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn report_now_viewing_item(
        &self,
        _session_id: &str,
        _item_id: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn authenticate_new_session(
        &self,
        _request: &AuthenticationRequest,
    ) -> Result<AuthenticationResultData, ServiceError> {
        unimplemented!("fake")
    }
    async fn authenticate_direct(
        &self,
        _request: &AuthenticationRequest,
    ) -> Result<AuthenticationResultData, ServiceError> {
        unimplemented!("fake")
    }
    async fn report_capabilities(
        &self,
        _session_id: &str,
        _capabilities: &ClientCapabilities,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn report_transcoding_info(
        &self,
        _device_id: &str,
        _info: &TranscodingInfo,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn clear_transcoding_info(&self, _device_id: &str) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn logout(&self, _access_token: &str) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn logout_device(&self, _device: &DeviceEntity) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn revoke_user_tokens(
        &self,
        _user_id: Uuid,
        _current_access_token: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
    async fn close_live_stream_if_needed(
        &self,
        _live_stream_id: &str,
        _session_or_play_session_id: &str,
    ) -> Result<(), ServiceError> {
        unimplemented!("fake")
    }
}

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The next text frame, failing after 10 s instead of hanging.
async fn next_json(socket: &mut Client) -> serde_json::Value {
    let text = tokio::time::timeout(std::time::Duration::from_secs(10), next_text(socket))
        .await
        .expect("a frame in time");
    serde_json::from_str(&text).expect("json frame")
}

/// Sends a client `KeepAlive` and asserts the ack is the very next frame: no
/// stream frame was waiting before it. The socket handles frames in order, so
/// the ack also proves every message sent before it was processed.
async fn assert_next_is_ack(socket: &mut Client) {
    socket
        .send(Message::text(r#"{"MessageType":"KeepAlive"}"#))
        .await
        .expect("send keep-alive");
    let frame = next_json(socket).await;
    assert_eq!(
        frame["MessageType"], "KeepAlive",
        "unexpected frame {frame}"
    );
}

/// Opens a socket over `sessions` as [`CALLER`] and subscribes it to the
/// `Sessions` stream with `data` (`"initialDelayMs,periodMs"`).
async fn subscribe(sessions: &Arc<StreamingSessions>, admin: bool, data: &str) -> Client {
    let state = ferrofin_api::test_support::fake_state_with_sessions(
        Arc::new(TokenAuth { admin }),
        Arc::new(ferrofin_api::test_support::FakeConfig),
        Arc::clone(sessions) as Arc<dyn SessionManager>,
    );
    let addr = serve(state).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/socket"))
        .await
        .expect("upgrade");
    assert_eq!(
        next_json(&mut socket).await["MessageType"],
        "ForceKeepAlive"
    );
    assert_eq!(
        sessions.events.receiver_count(),
        0,
        "the socket subscribes at SessionsStart, not when it opens"
    );
    socket
        .send(Message::text(format!(
            r#"{{"MessageType":"SessionsStart","Data":"{data}"}}"#
        )))
        .await
        .expect("subscribe");
    assert_next_is_ack(&mut socket).await;
    assert_eq!(sessions.events.receiver_count(), 1, "subscribed");
    socket
}

/// A session with this id and user.
fn session(id: &str, user_id: Uuid) -> SessionInfoDto {
    SessionInfoDto {
        id: Some(id.to_owned()),
        user_id,
        ..SessionInfoDto::default()
    }
}

/// The session ids a `Sessions` frame lists.
fn ids(frame: &serde_json::Value) -> Vec<&str> {
    assert_eq!(frame["MessageType"], "Sessions", "{frame}");
    frame["Data"]
        .as_array()
        .expect("a session list")
        .iter()
        .map(|s| s["Id"].as_str().expect("an id"))
        .collect()
}

/// B1: a session event is sent at once, whatever the period: upstream's
/// forced `SendData(true)` passes the throttle. The period here is a minute,
/// so a timer-driven stream would send nothing before the read gives up.
#[tokio::test]
async fn a_session_event_sends_the_session_list_at_once() {
    let sessions = StreamingSessions::new(vec![session("a", CALLER)]);
    let mut socket = subscribe(&sessions, true, "0,60000").await;

    sessions.raise(SessionEvent::PlaybackStart);
    assert_eq!(ids(&next_json(&mut socket).await), ["a"]);
    assert_eq!(sessions.builds(), 1);
}

/// B3 and B7: subscribing sends nothing (upstream sends nothing on `Start`),
/// and while nothing happens the stream neither sends nor builds a session
/// list, however many periods pass. The clock is paused once the
/// subscription is in, so the seconds below pass at once.
#[tokio::test]
async fn an_idle_subscription_sends_and_builds_nothing() {
    let sessions = StreamingSessions::new(vec![session("a", CALLER)]);
    let mut socket = subscribe(&sessions, true, "0,1500").await;
    tokio::time::pause();

    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    assert_next_is_ack(&mut socket).await;
    assert_eq!(sessions.builds(), 0, "no session list was built");
}

/// B1 and B2: upstream's throttle per socket. Automated progress is
/// `SendData(false)`: the first passes (nothing was sent yet), the rest inside
/// the period are dropped with no trailing send. Every other event is forced
/// and sends each time.
#[tokio::test]
async fn throttled_events_inside_a_period_send_once_and_forced_events_always() {
    let sessions = StreamingSessions::new(vec![session("a", CALLER)]);
    let mut socket = subscribe(&sessions, true, "0,60000").await;

    for _ in 0..4 {
        sessions.raise(SessionEvent::PlaybackProgress { is_automated: true });
    }
    assert_eq!(ids(&next_json(&mut socket).await), ["a"]);
    assert_next_is_ack(&mut socket).await;

    for event in [
        SessionEvent::PlaybackProgress {
            is_automated: false,
        },
        SessionEvent::PlaybackStopped,
        SessionEvent::SessionEnded,
    ] {
        sessions.raise(event);
        assert_eq!(ids(&next_json(&mut socket).await), ["a"], "{event:?}");
    }
    assert_eq!(
        sessions.builds(),
        4,
        "one build for the automated burst, one per forced event"
    );
}

/// `SessionActivity` is `SendData(false)` and `CapabilitiesChanged`
/// `SendData(true)` (`SessionInfoWebSocketListener.cs:111-119`): activity
/// inside the period is dropped, and sends again once the period has passed
/// since the last frame, while a capabilities report sends at once. The
/// period is skipped by advancing the clock, paused only while no frame is in
/// flight: left paused, it would also run out `next_json`'s timeout. The
/// period stays under the 45 s keep-alive watchdog.
#[tokio::test]
async fn session_activity_is_throttled_and_capabilities_changed_is_forced() {
    let sessions = StreamingSessions::new(vec![session("a", CALLER)]);
    let mut socket = subscribe(&sessions, true, "0,30000").await;

    sessions.raise(SessionEvent::SessionActivity);
    assert_eq!(ids(&next_json(&mut socket).await), ["a"]);
    sessions.raise(SessionEvent::SessionActivity);
    assert_next_is_ack(&mut socket).await;
    sessions.raise(SessionEvent::CapabilitiesChanged);
    assert_eq!(ids(&next_json(&mut socket).await), ["a"]);

    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    tokio::time::resume();
    sessions.raise(SessionEvent::SessionActivity);
    assert_eq!(ids(&next_json(&mut socket).await), ["a"]);
    assert_eq!(sessions.builds(), 3, "the dropped activity built nothing");
}

/// B4: a non-administrator's frames list only their own sessions and those
/// they joined as an additional user (`SessionInfoWebSocketListener
/// .GetDataToSendForConnection`); a session with no user is not theirs. An
/// administrator's list every session.
#[tokio::test]
async fn a_non_administrator_sees_only_their_own_sessions() {
    let joined = SessionInfoDto {
        additional_users: Some(vec![SessionUserInfo {
            user_id: CALLER,
            user_name: None,
        }]),
        ..session("joined", OTHER)
    };
    let all = vec![
        session("own", CALLER),
        joined,
        session("other", OTHER),
        session("user-less", Uuid::nil()),
    ];

    let sessions = StreamingSessions::new(all.clone());
    let mut socket = subscribe(&sessions, false, "0,1500").await;
    sessions.raise(SessionEvent::SessionStarted);
    assert_eq!(ids(&next_json(&mut socket).await), ["own", "joined"]);

    let sessions = StreamingSessions::new(all);
    let mut socket = subscribe(&sessions, true, "0,1500").await;
    sessions.raise(SessionEvent::SessionStarted);
    assert_eq!(
        ids(&next_json(&mut socket).await),
        ["own", "joined", "other", "user-less"]
    );
}

/// B7: `SessionsStop` drops the subscription, so later events reach nobody
/// and build nothing.
#[tokio::test]
async fn sessions_stop_ends_the_stream() {
    let sessions = StreamingSessions::new(vec![session("a", CALLER)]);
    let mut socket = subscribe(&sessions, true, "0,1500").await;

    socket
        .send(Message::text(r#"{"MessageType":"SessionsStop"}"#))
        .await
        .expect("unsubscribe");
    assert_next_is_ack(&mut socket).await;
    assert_eq!(sessions.events.receiver_count(), 0, "unsubscribed");

    // No receiver is left, so the send itself reports nobody listening.
    assert!(sessions.events.send(SessionEvent::PlaybackStart).is_err());
    assert_next_is_ack(&mut socket).await;
    assert_eq!(sessions.builds(), 0);
}
