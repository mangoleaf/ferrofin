//! [`FerrofinSyncPlayManager`] — synchronized group playback.
//!
//! Port of `Emby.Server.Implementations.SyncPlay.SyncPlayManager` + `Group` +
//! the `GroupStates/` state machine. A server-authoritative group holds a play
//! queue, a playback state ([`GroupStateType`]), and a set of member sessions;
//! playback requests mutate that state and push `SyncPlayCommand` /
//! `SyncPlayGroupUpdate` envelopes to member sockets via the
//! [`SessionMessageBus`]. The server never plays media — clients synchronize off
//! the server clock using each command's future `When` timestamp (see the
//! time-sync note on [`Group::unpause_when`]).
//!
//! The C# per-state strategy objects (`IdleGroupState`, `WaitingGroupState`, …)
//! and their request-object double-dispatch are collapsed into a `match` over
//! ([`GroupStateType`], [`PlaybackRequest`]) inside [`Group::handle`], which is
//! how Rust expresses the same table without method-overload dispatch.
//!
//! ponytail: the intricate `WaitingGroupState` per-client drift reconciliation
//! (corrective per-session seeks when a client's reported position exceeds
//! `MAX_PLAYBACK_OFFSET`) is modeled at the group grain — a re-buffer puts the
//! whole group into `Waiting` and a client `Ready` resolves it — rather than
//! per-session. Clients still converge via the scheduled `When`; upgrade to the
//! full per-session algorithm if frame-exact catch-up on a laggy member matters.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use uuid::Uuid;

use ferrofin_model::sync_play::{
    GroupDoesNotExistUpdate, GroupInfoDto, GroupJoinedUpdate, GroupLeftUpdate, GroupQueueMode,
    GroupRepeatMode, GroupShuffleMode, GroupStateType, GroupStateUpdate, GroupUpdate,
    LibraryAccessDeniedUpdate, NotInGroupUpdate, PlayQueueGroupUpdate, PlayQueueUpdate,
    PlayQueueUpdateReason, PlaybackRequestType, SendCommand, SendCommandType, StateUpdate,
    SyncPlayQueueItem, UserJoinedUpdate, UserLeftUpdate,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, UserManager};
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::session_bus::SessionMessageBus;
use ferrofin_traits::stubs::{PlaybackRequest, SyncPlayManager, SyncPlaySession};

/// .NET tick resolution: 100-nanosecond units, so 10_000 ticks per millisecond.
/// Clients expect `PositionTicks`/`When` on the wire in these units.
const TICKS_PER_MS: i64 = 10_000;
/// Default latency cushion added when scheduling a command's `When`, in ms.
/// ponytail: Jellyfin's `Group.DefaultPing`; surface as a setting if tuned.
const DEFAULT_PING_MS: i64 = 500;

/// The two server→client WebSocket message types SyncPlay uses.
const MSG_COMMAND: &str = "SyncPlayCommand";
const MSG_GROUP_UPDATE: &str = "SyncPlayGroupUpdate";

/// Who a produced message is addressed to (the [`SyncPlayBroadcastType`] subset
/// this group-grain model uses; `AllExceptCurrentSession` belongs to the
/// per-session reconciliation noted in the module docs and is added with it).
///
/// [`SyncPlayBroadcastType`]: ferrofin_model::sync_play::SyncPlayBroadcastType
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// Every member.
    AllGroup,
    /// Only the requesting session (a corrective resync of one lagging client).
    CurrentSession,
    /// Only members that are not buffering (tell the ready ones to wait).
    AllReady,
}

/// One serialized envelope plus its addressing.
struct Outbound {
    target: Target,
    body: String,
}

impl Outbound {
    fn all(body: String) -> Self {
        Self {
            target: Target::AllGroup,
            body,
        }
    }

    fn to(target: Target, body: String) -> Self {
        Self { target, body }
    }
}

/// A member session of a group (port of `GroupMember`).
#[derive(Debug, Clone)]
struct GroupMember {
    session_id: String,
    /// The signed-in user behind the session — the grain the library-access
    /// checks and the `active_users` counter work at.
    user_id: Uuid,
    user_name: String,
    ping_ms: i64,
    is_buffering: bool,
    ignore_wait: bool,
}

/// The group's play queue (port of `PlayQueueManager`, essentials).
#[derive(Debug, Default)]
struct PlayQueue {
    items: Vec<SyncPlayQueueItem>,
    playing_index: i32,
    shuffle: GroupShuffleMode,
    repeat: GroupRepeatMode,
}

impl PlayQueue {
    /// Replaces the queue with `item_ids`, assigning each a fresh server-side
    /// `PlaylistItemId`, and sets the playing item to `position`.
    fn set(&mut self, item_ids: &[Uuid], position: i32) {
        self.items = item_ids
            .iter()
            .map(|&item_id| SyncPlayQueueItem {
                item_id,
                playlist_item_id: Uuid::new_v4(),
            })
            .collect();
        self.playing_index = position.clamp(0, self.max_index());
    }

    /// The queue's item ids, for the library-access checks.
    fn item_ids(&self) -> Vec<Uuid> {
        self.items.iter().map(|i| i.item_id).collect()
    }

    fn max_index(&self) -> i32 {
        i32::try_from(self.items.len().saturating_sub(1)).unwrap_or(0)
    }

    /// The `PlaylistItemId` of the current item (nil if the queue is empty).
    fn playing_item_id(&self) -> Uuid {
        self.current()
            .map(|i| i.playlist_item_id)
            .unwrap_or_default()
    }

    fn current(&self) -> Option<&SyncPlayQueueItem> {
        usize::try_from(self.playing_index)
            .ok()
            .and_then(|i| self.items.get(i))
    }

    /// Points the queue at `playlist_item_id`; returns whether it was found.
    fn set_playing(&mut self, playlist_item_id: Uuid) -> bool {
        if let Some(idx) = self
            .items
            .iter()
            .position(|i| i.playlist_item_id == playlist_item_id)
        {
            self.playing_index = i32::try_from(idx).unwrap_or(0);
            true
        } else {
            false
        }
    }

    /// Advances to the next item; returns whether a next item exists.
    fn next(&mut self) -> bool {
        if self.playing_index < self.max_index() {
            self.playing_index += 1;
            true
        } else {
            false
        }
    }

    /// Steps back to the previous item; returns whether one exists.
    fn previous(&mut self) -> bool {
        if self.playing_index > 0 {
            self.playing_index -= 1;
            true
        } else {
            false
        }
    }

    /// Appends items, either at the end (`Queue`) or right after the current
    /// item (`QueueNext`).
    fn enqueue(&mut self, item_ids: &[Uuid], mode: GroupQueueMode) {
        let new_items = item_ids.iter().map(|&item_id| SyncPlayQueueItem {
            item_id,
            playlist_item_id: Uuid::new_v4(),
        });
        match mode {
            GroupQueueMode::Queue => self.items.extend(new_items),
            GroupQueueMode::QueueNext => {
                let at = usize::try_from(self.playing_index + 1)
                    .unwrap_or(self.items.len())
                    .min(self.items.len());
                for (offset, item) in new_items.enumerate() {
                    self.items.insert(at + offset, item);
                }
            }
        }
    }

    /// Removes the given playlist items, keeping the playing item where possible.
    fn remove(&mut self, ids: &[Uuid]) {
        let current = self.playing_item_id();
        self.items.retain(|i| !ids.contains(&i.playlist_item_id));
        self.reanchor(current);
    }

    /// Moves `playlist_item_id` to `new_index`.
    fn move_item(&mut self, playlist_item_id: Uuid, new_index: i32) {
        let Some(from) = self
            .items
            .iter()
            .position(|i| i.playlist_item_id == playlist_item_id)
        else {
            return;
        };
        let current = self.playing_item_id();
        let to = usize::try_from(new_index)
            .unwrap_or(0)
            .min(self.items.len().saturating_sub(1));
        let item = self.items.remove(from);
        self.items.insert(to, item);
        self.reanchor(current);
    }

    /// Re-points `playing_index` at the item that had id `current` after a
    /// mutation, clamped into range.
    fn reanchor(&mut self, current: Uuid) {
        self.playing_index = self
            .items
            .iter()
            .position(|i| i.playlist_item_id == current)
            .and_then(|i| i32::try_from(i).ok())
            .unwrap_or(0)
            .min(self.max_index());
    }
}

/// A live SyncPlay group (port of `Group` + `IGroupStateContext`).
struct Group {
    id: Uuid,
    name: String,
    state: GroupStateType,
    members: Vec<GroupMember>,
    queue: PlayQueue,
    position_ticks: i64,
    last_activity: DateTime<Utc>,
    last_updated: DateTime<Utc>,
}

impl Group {
    fn new(id: Uuid, name: String, now: DateTime<Utc>) -> Self {
        Self {
            id,
            name,
            state: GroupStateType::Idle,
            members: Vec::new(),
            queue: PlayQueue::default(),
            position_ticks: 0,
            last_activity: now,
            last_updated: now,
        }
    }

    fn info(&self) -> GroupInfoDto {
        let mut participants: Vec<String> =
            self.members.iter().map(|m| m.user_name.clone()).collect();
        participants.sort();
        participants.dedup();
        GroupInfoDto {
            group_id: self.id,
            group_name: self.name.clone(),
            state: self.state,
            participants,
            last_updated_at: self.last_updated,
        }
    }

    fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    fn highest_ping_ms(&self) -> i64 {
        self.members.iter().map(|m| m.ping_ms).max().unwrap_or(0)
    }

    /// The distinct user ids behind the group's member sessions.
    fn member_user_ids(&self) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = self.members.iter().map(|m| m.user_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    fn member_mut(&mut self, session_id: &str) -> Option<&mut GroupMember> {
        self.members.iter_mut().find(|m| m.session_id == session_id)
    }

    fn add_member(&mut self, session: &SyncPlaySession) {
        if self.member_mut(&session.session_id).is_none() {
            self.members.push(GroupMember {
                session_id: session.session_id.clone(),
                user_id: session.user_id,
                user_name: session.user_name.clone(),
                ping_ms: DEFAULT_PING_MS,
                is_buffering: false,
                ignore_wait: false,
            });
        }
    }

    fn remove_member(&mut self, session_id: &str) {
        self.members.retain(|m| m.session_id != session_id);
    }

    fn set_all_buffering(&mut self, buffering: bool) {
        for m in &mut self.members {
            m.is_buffering = buffering;
        }
    }

    /// The command's scheduled execution instant: `now` + a latency cushion
    /// sized to the slowest member so every client can hit it. Port of
    /// `Group.LastActivity = now + max(2*highestPing, DefaultPing)`.
    fn unpause_when(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let cushion = (2 * self.highest_ping_ms()).max(DEFAULT_PING_MS);
        now + Duration::milliseconds(cushion)
    }

    /// Advances `position_ticks` by the real time elapsed since `last_activity`
    /// when playing (port of `PositionTicks += now - LastActivity`).
    fn advance_position(&mut self, now: DateTime<Utc>) {
        if self.state == GroupStateType::Playing {
            let elapsed = (now - self.last_activity).num_milliseconds().max(0) * TICKS_PER_MS;
            self.position_ticks += elapsed;
        }
    }

    fn command_env(
        &self,
        command: SendCommandType,
        when: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> String {
        let cmd = SendCommand {
            group_id: self.id,
            playlist_item_id: self.queue.playing_item_id(),
            when,
            position_ticks: Some(self.position_ticks),
            command,
            emitted_at: now,
        };
        envelope(MSG_COMMAND, serde_json::to_value(cmd).unwrap_or(json!({})))
    }

    fn state_update_env(&self, reason: PlaybackRequestType) -> String {
        render_update(GroupUpdate::StateUpdate(StateUpdate {
            group_id: self.id,
            data: GroupStateUpdate {
                state: self.state,
                reason,
            },
        }))
    }

    fn play_queue_env(&self, reason: PlayQueueUpdateReason, now: DateTime<Utc>) -> String {
        let update = PlayQueueUpdate {
            reason,
            last_update: now,
            playlist: self.queue.items.clone(),
            playing_item_index: self.queue.playing_index,
            start_position_ticks: self.position_ticks,
            is_playing: self.state == GroupStateType::Playing,
            shuffle_mode: self.queue.shuffle,
            repeat_mode: self.queue.repeat,
        };
        render_update(GroupUpdate::PlayQueue(PlayQueueGroupUpdate {
            group_id: self.id,
            data: update,
        }))
    }

    /// Applies a playback request, mutating state and returning the messages to
    /// broadcast. `now` is threaded in for determinism/testability.
    #[allow(clippy::too_many_lines)] // one match arm per PlaybackRequestType — a table, not logic
    fn handle(&mut self, request: &PlaybackRequest, now: DateTime<Utc>) -> Vec<Outbound> {
        self.last_updated = now;
        match request {
            PlaybackRequest::Play {
                playing_queue,
                playing_item_position,
                start_position_ticks,
            } => {
                self.queue.set(playing_queue, *playing_item_position);
                self.position_ticks = (*start_position_ticks).max(0);
                self.state = GroupStateType::Playing;
                self.last_activity = self.unpause_when(now);
                self.set_all_buffering(false);
                vec![
                    Outbound::all(self.play_queue_env(PlayQueueUpdateReason::NewPlaylist, now)),
                    Outbound::all(self.command_env(
                        SendCommandType::Unpause,
                        self.last_activity,
                        now,
                    )),
                ]
            }
            PlaybackRequest::SetPlaylistItem { playlist_item_id } => {
                if self.queue.set_playing(*playlist_item_id) {
                    self.position_ticks = 0;
                    self.state = GroupStateType::Playing;
                    self.last_activity = self.unpause_when(now);
                    vec![
                        Outbound::all(
                            self.play_queue_env(PlayQueueUpdateReason::SetCurrentItem, now),
                        ),
                        Outbound::all(self.command_env(
                            SendCommandType::Unpause,
                            self.last_activity,
                            now,
                        )),
                    ]
                } else {
                    Vec::new()
                }
            }
            PlaybackRequest::RemoveFromPlaylist {
                playlist_item_ids,
                clear_playlist,
                clear_playing_item,
            } => {
                if *clear_playlist {
                    self.queue = PlayQueue::default();
                    self.state = GroupStateType::Idle;
                    self.position_ticks = 0;
                    let _ = clear_playing_item;
                    vec![
                        Outbound::all(self.play_queue_env(PlayQueueUpdateReason::RemoveItems, now)),
                        Outbound::all(self.command_env(SendCommandType::Stop, now, now)),
                    ]
                } else {
                    self.queue.remove(playlist_item_ids);
                    vec![Outbound::all(
                        self.play_queue_env(PlayQueueUpdateReason::RemoveItems, now),
                    )]
                }
            }
            PlaybackRequest::MovePlaylistItem {
                playlist_item_id,
                new_index,
            } => {
                self.queue.move_item(*playlist_item_id, *new_index);
                vec![Outbound::all(
                    self.play_queue_env(PlayQueueUpdateReason::MoveItem, now),
                )]
            }
            PlaybackRequest::Queue { item_ids, mode } => {
                self.queue.enqueue(item_ids, *mode);
                let reason = match mode {
                    GroupQueueMode::Queue => PlayQueueUpdateReason::Queue,
                    GroupQueueMode::QueueNext => PlayQueueUpdateReason::QueueNext,
                };
                vec![Outbound::all(self.play_queue_env(reason, now))]
            }
            PlaybackRequest::Unpause => {
                if self.state == GroupStateType::Idle {
                    return Vec::new();
                }
                self.state = GroupStateType::Playing;
                self.last_activity = self.unpause_when(now);
                vec![
                    Outbound::all(self.command_env(
                        SendCommandType::Unpause,
                        self.last_activity,
                        now,
                    )),
                    Outbound::all(self.state_update_env(PlaybackRequestType::Unpause)),
                ]
            }
            PlaybackRequest::Pause => {
                if self.state == GroupStateType::Idle {
                    return Vec::new();
                }
                self.advance_position(now);
                self.state = GroupStateType::Paused;
                self.last_activity = now;
                vec![
                    Outbound::all(self.command_env(SendCommandType::Pause, now, now)),
                    Outbound::all(self.state_update_env(PlaybackRequestType::Pause)),
                ]
            }
            PlaybackRequest::Stop => {
                self.state = GroupStateType::Idle;
                vec![Outbound::all(self.command_env(
                    SendCommandType::Stop,
                    now,
                    now,
                ))]
            }
            PlaybackRequest::Seek { position_ticks } => {
                self.position_ticks = (*position_ticks).max(0);
                self.state = GroupStateType::Waiting;
                self.last_activity = now;
                self.set_all_buffering(true);
                vec![
                    Outbound::all(self.command_env(SendCommandType::Seek, now, now)),
                    Outbound::all(self.state_update_env(PlaybackRequestType::Seek)),
                ]
            }
            PlaybackRequest::Buffer { .. } => {
                self.state = GroupStateType::Waiting;
                // Tell the members that are ready to hold while this one buffers.
                vec![
                    Outbound::to(
                        Target::AllReady,
                        self.command_env(SendCommandType::Pause, now, now),
                    ),
                    Outbound::all(self.state_update_env(PlaybackRequestType::Buffer)),
                ]
            }
            PlaybackRequest::Ready { .. } => self.handle_ready(now),
            PlaybackRequest::NextItem { .. } => {
                if self.queue.next() {
                    self.position_ticks = 0;
                    self.last_activity = self.unpause_when(now);
                    vec![Outbound::all(
                        self.play_queue_env(PlayQueueUpdateReason::NextItem, now),
                    )]
                } else {
                    Vec::new()
                }
            }
            PlaybackRequest::PreviousItem { .. } => {
                if self.queue.previous() {
                    self.position_ticks = 0;
                    self.last_activity = self.unpause_when(now);
                    vec![Outbound::all(
                        self.play_queue_env(PlayQueueUpdateReason::PreviousItem, now),
                    )]
                } else {
                    Vec::new()
                }
            }
            PlaybackRequest::SetRepeatMode { mode } => {
                self.queue.repeat = *mode;
                vec![Outbound::all(
                    self.play_queue_env(PlayQueueUpdateReason::RepeatMode, now),
                )]
            }
            PlaybackRequest::SetShuffleMode { mode } => {
                self.queue.shuffle = *mode;
                vec![Outbound::all(
                    self.play_queue_env(PlayQueueUpdateReason::ShuffleMode, now),
                )]
            }
            // Member-scoped bookkeeping is applied by the manager before this;
            // neither produces a group broadcast.
            PlaybackRequest::Ping { .. } | PlaybackRequest::IgnoreWait { .. } => Vec::new(),
        }
    }

    /// A client reported Ready. From `Waiting`, the group resumes with a
    /// scheduled Unpause to everyone. If the group is already `Playing` (this
    /// client fell behind and re-buffered alone), only that client is resynced.
    fn handle_ready(&mut self, now: DateTime<Utc>) -> Vec<Outbound> {
        self.set_all_buffering(false);
        if self.state == GroupStateType::Waiting {
            self.state = GroupStateType::Playing;
            self.last_activity = self.unpause_when(now);
            vec![
                Outbound::all(self.command_env(SendCommandType::Unpause, self.last_activity, now)),
                Outbound::all(self.state_update_env(PlaybackRequestType::Ready)),
            ]
        } else if self.state == GroupStateType::Playing {
            vec![Outbound::to(
                Target::CurrentSession,
                self.command_env(SendCommandType::Unpause, self.unpause_when(now), now),
            )]
        } else {
            Vec::new()
        }
    }
}

/// Wraps a payload in the Jellyfin WebSocket message envelope, consuming `data`.
///
/// Every outbound message carries a `MessageId` (C# `OutboundWebSocketMessage`
/// sets `Guid.NewGuid()`); it is `format: uuid` and required by strict clients
/// (the Jellyfin Kotlin SDK rejects a message missing it), so emit a fresh,
/// canonically-hyphenated UUID here too.
fn envelope(message_type: &str, data: serde_json::Value) -> String {
    let mut map = serde_json::Map::new();
    map.insert(
        "MessageType".to_owned(),
        serde_json::Value::String(message_type.to_owned()),
    );
    map.insert(
        "MessageId".to_owned(),
        serde_json::Value::String(Uuid::new_v4().hyphenated().to_string()),
    );
    map.insert("Data".to_owned(), data);
    serde_json::Value::Object(map).to_string()
}

/// Serializes a [`GroupUpdate`] into a `SyncPlayGroupUpdate` envelope.
fn render_update(update: GroupUpdate) -> String {
    envelope(
        MSG_GROUP_UPDATE,
        serde_json::to_value(update).unwrap_or(json!({})),
    )
}

/// The shared, lock-guarded registry of all groups and session membership.
#[derive(Default)]
struct Registry {
    /// group id → group.
    groups: HashMap<Uuid, Group>,
    /// session id → the group it belongs to.
    session_to_group: HashMap<String, Uuid>,
    /// user id → count of that user's active member sessions.
    active_users: HashMap<Uuid, u32>,
}

/// The user + library seams a group needs to answer "may this user see the
/// items in that play queue?" (the C# `Group`'s `_userManager`/`_libraryManager`).
struct LibraryAccess {
    users: Arc<dyn UserManager>,
    library: Arc<dyn LibraryManager>,
}

/// The real SyncPlay manager: a group registry plus a socket message bus.
pub struct FerrofinSyncPlayManager {
    registry: Mutex<Registry>,
    bus: Arc<dyn SessionMessageBus>,
    /// When wired, group visibility and queue changes are gated on every
    /// affected member's access to the queued items. Absent (unit tests), every
    /// queue is treated as accessible.
    access: Option<LibraryAccess>,
}

impl std::fmt::Debug for FerrofinSyncPlayManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinSyncPlayManager")
            .finish_non_exhaustive()
    }
}

impl FerrofinSyncPlayManager {
    /// Creates a SyncPlay manager delivering commands over `bus`.
    #[must_use]
    pub fn new(bus: Arc<dyn SessionMessageBus>) -> Self {
        Self {
            registry: Mutex::new(Registry::default()),
            bus,
            access: None,
        }
    }

    /// Wires the user + library seams (composition root only), enabling the
    /// library-access rules C# `Group` applies: a group whose queue a user
    /// cannot see is hidden from `List`/`{id}` and refuses their `Join`, and a
    /// queue change is rejected unless every member can see the new items.
    #[must_use]
    pub fn with_library_access(
        mut self,
        users: Arc<dyn UserManager>,
        library: Arc<dyn LibraryManager>,
    ) -> Self {
        self.access = Some(LibraryAccess { users, library });
        self
    }

    /// Whether `user_id` may see every item in `items` — port of C#
    /// `Group.HasAccessToQueue`, which rejects an item that does not resolve or
    /// is not `IsVisibleStandalone` for the user. An empty queue is accessible.
    ///
    /// ponytail: routed through the repository's user-scoped query rather than a
    /// second, SyncPlay-only visibility rule, so it enforces exactly what the
    /// rest of the API enforces. Today that means "the item exists and the query
    /// returns it for this user"; Ferrofin's query pipeline does not yet apply
    /// parental-rating or enabled-folder predicates, so those parts of
    /// `IsVisibleStandalone` are not checked here either — this tightens by
    /// itself when they land in `translate_query`.
    async fn has_access_to_queue(&self, user_id: Uuid, items: &[Uuid]) -> bool {
        let Some(access) = self.access.as_ref() else {
            return true;
        };
        if items.is_empty() {
            return true;
        }
        let Ok(Some(user)) = access.users.get_user_by_id(user_id).await else {
            return false;
        };
        let query = InternalItemsQuery {
            item_ids: items.to_vec(),
            user: Some(user),
            ..InternalItemsQuery::default()
        };
        let Ok(visible) = access.library.get_item_ids(&query).await else {
            return false;
        };
        items.iter().all(|id| visible.contains(id))
    }

    /// Whether every member of the group may see `items` (C#
    /// `Group.AllUsersHaveAccessToQueue`), used to gate queue changes.
    async fn all_members_have_access(&self, members: &[Uuid], items: &[Uuid]) -> bool {
        for user_id in members {
            if !self.has_access_to_queue(*user_id, items).await {
                return false;
            }
        }
        true
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry.lock().expect("sync-play registry poisoned")
    }

    /// Delivers `outbound` to the resolved target sessions. Called after the
    /// registry lock is released (delivery is a non-blocking sink invocation).
    fn deliver(&self, members: &[(String, bool)], from_session: &str, outbound: &[Outbound]) {
        for msg in outbound {
            for (session_id, is_buffering) in members {
                let deliver = match msg.target {
                    Target::AllGroup => true,
                    Target::CurrentSession => session_id == from_session,
                    Target::AllReady => !is_buffering,
                };
                if deliver {
                    self.bus.send(session_id, msg.body.clone());
                }
            }
        }
    }
}

/// A snapshot of member `(session_id, is_buffering)` taken under the lock so
/// delivery can happen after unlocking.
fn member_targets(group: &Group) -> Vec<(String, bool)> {
    group
        .members
        .iter()
        .map(|m| (m.session_id.clone(), m.is_buffering))
        .collect()
}

#[async_trait]
impl SyncPlayManager for FerrofinSyncPlayManager {
    async fn new_group(
        &self,
        session: &SyncPlaySession,
        group_name: &str,
    ) -> Result<GroupInfoDto, ServiceError> {
        let now = Utc::now();
        let (info, joined_env) = {
            let mut reg = self.lock();
            // A session may be in at most one group — leave the old one first.
            leave_locked(&mut reg, session);
            let group_id = Uuid::new_v4();
            let mut group = Group::new(group_id, group_name.trim().to_owned(), now);
            group.add_member(session);
            let info = group.info();
            let joined = render_update(GroupUpdate::GroupJoined(GroupJoinedUpdate {
                group_id,
                data: info.clone(),
            }));
            reg.groups.insert(group_id, group);
            reg.session_to_group
                .insert(session.session_id.clone(), group_id);
            *reg.active_users.entry(session.user_id).or_insert(0) += 1;
            (info, joined)
        };
        self.bus.send(&session.session_id, joined_env);
        Ok(info)
    }

    async fn join_group(
        &self,
        session: &SyncPlaySession,
        group_id: Uuid,
    ) -> Result<(), ServiceError> {
        let now = Utc::now();
        let joined_env;
        let user_joined_env;
        let others: Vec<String>;
        // The joiner must be able to see what the group is already playing (C#
        // `JoinGroup` -> `HasAccessToPlayQueue`). Read the queue under the lock,
        // then check without holding it.
        let queue = self
            .lock()
            .groups
            .get(&group_id)
            .map(|g| g.queue.item_ids());
        if let Some(queue) = queue
            && !self.has_access_to_queue(session.user_id, &queue).await
        {
            let env = render_update(GroupUpdate::LibraryAccessDenied(
                LibraryAccessDeniedUpdate {
                    group_id,
                    data: String::new(),
                },
            ));
            self.bus.send(&session.session_id, env);
            return Ok(());
        }
        {
            let mut reg = self.lock();
            if !reg.groups.contains_key(&group_id) {
                let env = envelope(
                    MSG_GROUP_UPDATE,
                    serde_json::to_value(GroupUpdate::GroupDoesNotExist(GroupDoesNotExistUpdate {
                        group_id,
                        data: group_id.to_string(),
                    }))
                    .unwrap_or(json!({})),
                );
                drop(reg);
                self.bus.send(&session.session_id, env);
                return Ok(());
            }
            leave_locked(&mut reg, session);
            let user_name = session.user_name.clone();
            let group = reg.groups.get_mut(&group_id).expect("group present");
            group.add_member(session);
            group.last_updated = now;
            let info = group.info();
            joined_env = render_update(GroupUpdate::GroupJoined(GroupJoinedUpdate {
                group_id,
                data: info,
            }));
            user_joined_env = render_update(GroupUpdate::UserJoined(UserJoinedUpdate {
                group_id,
                data: user_name,
            }));
            others = group
                .members
                .iter()
                .filter(|m| m.session_id != session.session_id)
                .map(|m| m.session_id.clone())
                .collect();
            reg.session_to_group
                .insert(session.session_id.clone(), group_id);
            *reg.active_users.entry(session.user_id).or_insert(0) += 1;
        }
        self.bus.send(&session.session_id, joined_env);
        for sid in others {
            self.bus.send(&sid, user_joined_env.clone());
        }
        Ok(())
    }

    async fn leave_group(&self, session: &SyncPlaySession) -> Result<(), ServiceError> {
        let now = Utc::now();
        let notices = {
            let mut reg = self.lock();
            leave_locked_with_notices(&mut reg, session, now)
        };
        if let Some((left_env, user_left_env, others)) = notices {
            self.bus.send(&session.session_id, left_env);
            for sid in others {
                self.bus.send(&sid, user_left_env.clone());
            }
        } else {
            // Not in a group: tell the caller so its client can reset.
            let env = envelope(
                MSG_GROUP_UPDATE,
                serde_json::to_value(GroupUpdate::NotInGroup(NotInGroupUpdate {
                    group_id: Uuid::nil(),
                    data: String::new(),
                }))
                .unwrap_or(json!({})),
            );
            self.bus.send(&session.session_id, env);
        }
        Ok(())
    }

    async fn list_groups(
        &self,
        session: &SyncPlaySession,
    ) -> Result<Vec<GroupInfoDto>, ServiceError> {
        // A group is only listed when the caller could actually join it — C#
        // `ListGroups` filters on `HasAccessToPlayQueue`.
        let mut candidates: Vec<(GroupInfoDto, Vec<Uuid>)> = {
            let reg = self.lock();
            reg.groups
                .values()
                .map(|g| (g.info(), g.queue.item_ids()))
                .collect()
        };
        candidates.sort_by_key(|(info, _)| info.group_id);
        let mut groups = Vec::with_capacity(candidates.len());
        for (info, queue) in candidates {
            if self.has_access_to_queue(session.user_id, &queue).await {
                groups.push(info);
            }
        }
        Ok(groups)
    }

    async fn get_group(
        &self,
        session: &SyncPlaySession,
        group_id: Uuid,
    ) -> Result<GroupInfoDto, ServiceError> {
        let found = {
            let reg = self.lock();
            reg.groups
                .get(&group_id)
                .map(|g| (g.info(), g.queue.item_ids()))
        };
        let not_found = || ServiceError::not_found(format!("sync-play group {group_id}"));
        let (info, queue) = found.ok_or_else(not_found)?;
        // An inaccessible group is indistinguishable from a missing one, as it is
        // upstream (the id simply does not match any group the user may see).
        if self.has_access_to_queue(session.user_id, &queue).await {
            Ok(info)
        } else {
            Err(not_found())
        }
    }

    async fn handle_request(
        &self,
        session: &SyncPlaySession,
        request: PlaybackRequest,
    ) -> Result<(), ServiceError> {
        let now = Utc::now();
        // A request that puts new items in the queue is refused unless *every*
        // member can see them (C# `SetPlayQueue`/`Queue` ->
        // `AllUsersHaveAccessToQueue`, which return false and broadcast nothing).
        let incoming: &[Uuid] = match &request {
            PlaybackRequest::Play { playing_queue, .. } => playing_queue,
            PlaybackRequest::Queue { item_ids, .. } => item_ids,
            _ => &[],
        };
        if !incoming.is_empty() {
            let members = {
                let reg = self.lock();
                reg.session_to_group
                    .get(&session.session_id)
                    .and_then(|id| reg.groups.get(id))
                    .map(Group::member_user_ids)
                    .unwrap_or_default()
            };
            if !members.is_empty() && !self.all_members_have_access(&members, incoming).await {
                return Ok(());
            }
        }
        let plan = {
            let mut reg = self.lock();
            let Some(&group_id) = reg.session_to_group.get(&session.session_id) else {
                drop(reg);
                let env = envelope(
                    MSG_GROUP_UPDATE,
                    serde_json::to_value(GroupUpdate::NotInGroup(NotInGroupUpdate {
                        group_id: Uuid::nil(),
                        data: String::new(),
                    }))
                    .unwrap_or(json!({})),
                );
                self.bus.send(&session.session_id, env);
                return Ok(());
            };
            let group = reg.groups.get_mut(&group_id).expect("mapped group present");
            // Member-scoped requests mutate the requesting member, not the group.
            match &request {
                PlaybackRequest::Ping { ping } => {
                    if let Some(m) = group.member_mut(&session.session_id) {
                        m.ping_ms = (*ping).max(0);
                    }
                }
                PlaybackRequest::IgnoreWait { ignore_wait } => {
                    if let Some(m) = group.member_mut(&session.session_id) {
                        m.ignore_wait = *ignore_wait;
                    }
                }
                PlaybackRequest::Buffer { .. } => {
                    if let Some(m) = group.member_mut(&session.session_id) {
                        m.is_buffering = true;
                    }
                }
                PlaybackRequest::Ready { .. } => {
                    if let Some(m) = group.member_mut(&session.session_id) {
                        m.is_buffering = false;
                    }
                }
                _ => {}
            }
            let outbound = group.handle(&request, now);
            (member_targets(group), outbound)
        };
        let (targets, outbound) = plan;
        self.deliver(&targets, &session.session_id, &outbound);
        Ok(())
    }

    async fn is_user_active(&self, user_id: Uuid) -> Result<bool, ServiceError> {
        Ok(self.lock().active_users.get(&user_id).copied().unwrap_or(0) > 0)
    }
}

/// Removes a session from any group it is in (registry pre-locked), decrementing
/// the user counter and dropping the group if it becomes empty. No broadcasts.
fn leave_locked(reg: &mut Registry, session: &SyncPlaySession) {
    if let Some(group_id) = reg.session_to_group.remove(&session.session_id) {
        if let Some(group) = reg.groups.get_mut(&group_id) {
            group.remove_member(&session.session_id);
            if group.is_empty() {
                reg.groups.remove(&group_id);
            }
        }
        decrement_user(reg, session.user_id);
    }
}

/// Like [`leave_locked`] but returns the `(GroupLeft, UserLeft, other-sessions)`
/// broadcast plan, or `None` if the session was not in a group.
#[allow(clippy::type_complexity)]
fn leave_locked_with_notices(
    reg: &mut Registry,
    session: &SyncPlaySession,
    now: DateTime<Utc>,
) -> Option<(String, String, Vec<String>)> {
    let group_id = reg.session_to_group.remove(&session.session_id)?;
    let group = reg.groups.get_mut(&group_id)?;
    group.remove_member(&session.session_id);
    group.last_updated = now;
    let left_env = render_update(GroupUpdate::GroupLeft(GroupLeftUpdate {
        group_id,
        data: group_id.to_string(),
    }));
    let user_left_env = render_update(GroupUpdate::UserLeft(UserLeftUpdate {
        group_id,
        data: session.user_name.clone(),
    }));
    let others: Vec<String> = group.members.iter().map(|m| m.session_id.clone()).collect();
    if group.is_empty() {
        reg.groups.remove(&group_id);
    }
    decrement_user(reg, session.user_id);
    Some((left_env, user_left_env, others))
}

/// Decrements a user's active-session counter, removing it at zero.
fn decrement_user(reg: &mut Registry, user_id: Uuid) {
    if let Some(count) = reg.active_users.get_mut(&user_id) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            reg.active_users.remove(&user_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;

    /// A bus that records every delivered `(session_id, body)`.
    #[derive(Default)]
    struct RecordingBus {
        sent: StdMutex<Vec<(String, String)>>,
    }

    impl RecordingBus {
        fn count_for(&self, session_id: &str) -> usize {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .filter(|(s, _)| s == session_id)
                .count()
        }
        fn bodies(&self) -> Vec<String> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .map(|(_, b)| b.clone())
                .collect()
        }
    }

    impl SessionMessageBus for RecordingBus {
        fn register(
            &self,
            _session_id: String,
            _sink: ferrofin_traits::session_bus::MessageSink,
        ) -> ferrofin_traits::session_bus::SinkToken {
            0
        }
        fn unregister(
            &self,
            _session_id: &str,
            _token: ferrofin_traits::session_bus::SinkToken,
        ) -> bool {
            false
        }
        fn send(&self, session_id: &str, message: String) -> bool {
            self.sent.lock().unwrap().push((session_id.into(), message));
            true
        }
        fn is_connected(&self, _session_id: &str) -> bool {
            true
        }
    }

    fn session(id: &str, name: &str) -> SyncPlaySession {
        SyncPlaySession {
            session_id: id.into(),
            user_id: Uuid::new_v4(),
            user_name: name.into(),
        }
    }

    fn mgr() -> (FerrofinSyncPlayManager, Arc<RecordingBus>) {
        let bus = Arc::new(RecordingBus::default());
        (FerrofinSyncPlayManager::new(bus.clone()), bus)
    }

    fn play(items: usize) -> PlaybackRequest {
        PlaybackRequest::Play {
            playing_queue: (0..items).map(|_| Uuid::new_v4()).collect(),
            playing_item_position: 0,
            start_position_ticks: 0,
        }
    }

    #[tokio::test]
    async fn new_group_creates_and_lists() {
        let (m, bus) = mgr();
        let s = session("s1", "alice");
        let info = m.new_group(&s, "movie night").await.unwrap();
        assert_eq!(info.group_name, "movie night");
        assert_eq!(info.participants, vec!["alice".to_string()]);
        assert_eq!(info.state, GroupStateType::Idle);
        assert_eq!(bus.count_for("s1"), 1); // GroupJoined push

        assert_eq!(m.list_groups(&s).await.unwrap().len(), 1);
        assert!(m.is_user_active(s.user_id).await.unwrap());
        assert_eq!(
            m.get_group(&s, info.group_id).await.unwrap().group_id,
            info.group_id
        );
    }

    #[tokio::test]
    async fn join_notifies_existing_members() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        let b = session("s2", "bob");
        m.join_group(&b, info.group_id).await.unwrap();

        let g = m.get_group(&a, info.group_id).await.unwrap();
        assert_eq!(g.participants, vec!["alice".to_string(), "bob".to_string()]);
        assert_eq!(bus.count_for("s1"), 2); // GroupJoined(create) + UserJoined(bob)
        assert_eq!(bus.count_for("s2"), 1); // GroupJoined
    }

    #[tokio::test]
    async fn join_nonexistent_group_notifies_caller() {
        let (m, bus) = mgr();
        let s = session("s1", "alice");
        m.join_group(&s, Uuid::new_v4()).await.unwrap();
        assert!(bus.bodies()[0].contains("GroupDoesNotExist"));
        assert!(m.list_groups(&s).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn leave_removes_and_drops_empty_group() {
        let (m, _bus) = mgr();
        let s = session("s1", "alice");
        let info = m.new_group(&s, "g").await.unwrap();
        m.leave_group(&s).await.unwrap();
        assert!(m.list_groups(&s).await.unwrap().is_empty());
        assert!(!m.is_user_active(s.user_id).await.unwrap());
        assert!(m.get_group(&s, info.group_id).await.is_err());
    }

    #[tokio::test]
    async fn leave_when_not_in_group_notifies_not_in_group() {
        let (m, bus) = mgr();
        let s = session("s1", "alice");
        m.leave_group(&s).await.unwrap();
        assert!(bus.bodies()[0].contains("NotInGroup"));
    }

    #[tokio::test]
    async fn set_new_queue_broadcasts_playqueue_and_unpause() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        let b = session("s2", "bob");
        m.join_group(&b, info.group_id).await.unwrap();

        m.handle_request(&a, play(2)).await.unwrap();

        let g = m.get_group(&a, info.group_id).await.unwrap();
        assert_eq!(g.state, GroupStateType::Playing);
        let bodies = bus.bodies();
        assert!(bodies.iter().any(|b| b.contains("PlayQueue")));
        assert!(
            bodies
                .iter()
                .any(|b| b.contains("SyncPlayCommand") && b.contains("Unpause"))
        );
        assert!(bus.count_for("s2") >= 3); // GroupJoined + PlayQueue + Unpause
    }

    #[tokio::test]
    async fn pause_unpause_stop_transitions() {
        let (m, _bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        m.handle_request(&a, play(1)).await.unwrap();
        let gid = info.group_id;

        m.handle_request(&a, PlaybackRequest::Pause).await.unwrap();
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Paused
        );
        m.handle_request(&a, PlaybackRequest::Unpause)
            .await
            .unwrap();
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Playing
        );
        m.handle_request(&a, PlaybackRequest::Stop).await.unwrap();
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Idle
        );
        // Idle pause/unpause are no-ops.
        m.handle_request(&a, PlaybackRequest::Pause).await.unwrap();
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Idle
        );
    }

    #[tokio::test]
    async fn handle_request_when_not_in_group_notifies() {
        let (m, bus) = mgr();
        let s = session("s1", "alice");
        m.handle_request(&s, PlaybackRequest::Pause).await.unwrap();
        assert!(bus.bodies()[0].contains("NotInGroup"));
    }

    #[tokio::test]
    async fn queue_move_remove_repeat_shuffle_mutate() {
        let (m, _bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        m.handle_request(&a, play(1)).await.unwrap();
        m.handle_request(
            &a,
            PlaybackRequest::Queue {
                item_ids: vec![Uuid::new_v4(), Uuid::new_v4()],
                mode: GroupQueueMode::QueueNext,
            },
        )
        .await
        .unwrap();
        m.handle_request(
            &a,
            PlaybackRequest::NextItem {
                playlist_item_id: Uuid::nil(),
            },
        )
        .await
        .unwrap();
        m.handle_request(
            &a,
            PlaybackRequest::PreviousItem {
                playlist_item_id: Uuid::nil(),
            },
        )
        .await
        .unwrap();
        m.handle_request(
            &a,
            PlaybackRequest::SetRepeatMode {
                mode: GroupRepeatMode::RepeatAll,
            },
        )
        .await
        .unwrap();
        m.handle_request(
            &a,
            PlaybackRequest::SetShuffleMode {
                mode: GroupShuffleMode::Shuffle,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            m.get_group(&a, info.group_id).await.unwrap().state,
            GroupStateType::Playing
        );
    }

    #[tokio::test]
    async fn ping_seek_buffer_ready_flow() {
        let (m, _bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        let gid = info.group_id;
        m.handle_request(&a, PlaybackRequest::Ping { ping: 40 })
            .await
            .unwrap();
        m.handle_request(&a, play(1)).await.unwrap();
        m.handle_request(&a, PlaybackRequest::IgnoreWait { ignore_wait: true })
            .await
            .unwrap();
        m.handle_request(
            &a,
            PlaybackRequest::Seek {
                position_ticks: 5_000_000,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Waiting
        );
        m.handle_request(
            &a,
            PlaybackRequest::Buffer {
                when: Utc::now(),
                position_ticks: 5_000_000,
                is_playing: false,
                playlist_item_id: Uuid::nil(),
            },
        )
        .await
        .unwrap();
        m.handle_request(
            &a,
            PlaybackRequest::Ready {
                when: Utc::now(),
                position_ticks: 5_000_000,
                is_playing: true,
                playlist_item_id: Uuid::nil(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Playing
        );
    }

    #[tokio::test]
    async fn clear_playlist_stops_group() {
        let (m, _bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        m.handle_request(&a, play(3)).await.unwrap();
        m.handle_request(
            &a,
            PlaybackRequest::RemoveFromPlaylist {
                playlist_item_ids: vec![],
                clear_playlist: true,
                clear_playing_item: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            m.get_group(&a, info.group_id).await.unwrap().state,
            GroupStateType::Idle
        );
    }

    #[tokio::test]
    async fn creating_a_second_group_leaves_the_first() {
        let (m, _bus) = mgr();
        let a = session("s1", "alice");
        let first = m.new_group(&a, "one").await.unwrap();
        let _second = m.new_group(&a, "two").await.unwrap();
        // The first group is now empty and dropped.
        assert!(m.get_group(&a, first.group_id).await.is_err());
        assert_eq!(m.list_groups(&a).await.unwrap().len(), 1);
    }

    // ── library access (C# `Group.HasAccessToPlayQueue`) ───────────────────

    /// A manager wired to a real DB-backed library + user manager, so the
    /// access checks run against the same query path the rest of the API uses.
    async fn access_mgr() -> (
        FerrofinSyncPlayManager,
        Arc<RecordingBus>,
        ferrofin_db::Database,
        Arc<dyn LibraryManager>,
    ) {
        let db = crate::test_support::test_db().await;
        let bus = Arc::new(RecordingBus::default());
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(crate::item_type_lookup::ItemTypeLookup::new());
        let library: Arc<dyn LibraryManager> =
            Arc::new(crate::library_manager::FerrofinLibraryManager::new(
                Arc::new(crate::item_repository::FerrofinItemRepository::new(
                    db.clone(),
                    lookup,
                )),
                Arc::new(crate::item_count_service::FerrofinItemCountService::new(
                    db.clone(),
                )),
                Arc::new(
                    crate::item_persistence_service::FerrofinItemPersistenceService::new(
                        db.clone(),
                    ),
                ),
                Arc::new(crate::people_repository::FerrofinPeopleRepository::new(
                    db.clone(),
                )),
            ));
        let users: Arc<dyn UserManager> =
            Arc::new(crate::user_manager::FerrofinUserManager::new(db.clone()));
        let mgr = FerrofinSyncPlayManager::new(Arc::clone(&bus) as Arc<dyn SessionMessageBus>)
            .with_library_access(users, Arc::clone(&library));
        (mgr, bus, db, library)
    }

    /// A session for a user that exists in the DB.
    async fn db_session(db: &ferrofin_db::Database, id: &str, name: &str) -> SyncPlaySession {
        let user_id = Uuid::new_v4();
        crate::test_support::seed_named_user(db, user_id, name).await;
        SyncPlaySession {
            session_id: id.into(),
            user_id,
            user_name: name.into(),
        }
    }

    /// The `Type` of every group update pushed to `session_id`.
    fn update_types(bus: &RecordingBus, session_id: &str) -> Vec<String> {
        bus.sent
            .lock()
            .unwrap()
            .iter()
            .filter(|(sid, _)| sid == session_id)
            .filter_map(|(_, body)| serde_json::from_str::<serde_json::Value>(body).ok())
            .filter_map(|v| v["Data"]["Type"].as_str().map(str::to_owned))
            .collect()
    }

    /// Seeds a movie and returns its id.
    async fn seed_movie(db: &ferrofin_db::Database, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        crate::test_support::seed_named_item(
            db,
            id,
            ferrofin_model::data::BaseItemKind::Movie,
            name,
        )
        .await;
        id
    }

    /// Removes an item from the library out from under a live group — the
    /// reachable way a queued item stops resolving (the queue gate keeps an
    /// unresolvable item from being enqueued in the first place).
    async fn delete_item(library: &dyn LibraryManager, id: Uuid) {
        library
            .delete_item(
                id,
                &ferrofin_traits::options::DeleteOptions {
                    delete_file_location: false,
                    ..Default::default()
                },
            )
            .await
            .expect("delete item");
    }

    /// Puts `items` on the group's queue via the owner.
    async fn set_queue(m: &FerrofinSyncPlayManager, owner: &SyncPlaySession, items: Vec<Uuid>) {
        m.handle_request(
            owner,
            PlaybackRequest::Play {
                playing_queue: items,
                playing_item_position: 0,
                start_position_ticks: 0,
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_queue_item_that_stops_resolving_hides_the_group() {
        let (m, _bus, db, library) = access_mgr().await;
        let owner = db_session(&db, "s1", "alice").await;
        let other = db_session(&db, "s2", "bob").await;
        let movie = seed_movie(&db, "Movie").await;

        let info = m.new_group(&owner, "movie night").await.unwrap();
        // An empty queue is accessible to everyone, so the group is visible.
        assert_eq!(m.list_groups(&other).await.unwrap().len(), 1);
        set_queue(&m, &owner, vec![movie]).await;
        assert_eq!(m.list_groups(&other).await.unwrap().len(), 1);

        delete_item(library.as_ref(), movie).await;
        assert!(
            m.list_groups(&other).await.unwrap().is_empty(),
            "a group playing what the user cannot see is not listed"
        );
        assert!(
            m.get_group(&other, info.group_id).await.is_err(),
            "and is indistinguishable from a missing group"
        );
    }

    #[tokio::test]
    async fn joining_a_group_whose_queue_is_inaccessible_is_denied() {
        let (m, bus, db, library) = access_mgr().await;
        let owner = db_session(&db, "s1", "alice").await;
        let other = db_session(&db, "s2", "bob").await;
        let movie = seed_movie(&db, "Movie").await;

        let info = m.new_group(&owner, "movie night").await.unwrap();
        set_queue(&m, &owner, vec![movie]).await;
        delete_item(library.as_ref(), movie).await;

        m.join_group(&other, info.group_id).await.unwrap();
        assert_eq!(
            update_types(&bus, "s2"),
            vec!["LibraryAccessDenied".to_owned()],
            "the joiner is told why"
        );
        assert!(
            !m.is_user_active(other.user_id).await.unwrap(),
            "and is not added to the group"
        );
        assert!(
            m.is_user_active(owner.user_id).await.unwrap(),
            "while the existing member stays in it"
        );
        let _ = info;
    }

    #[tokio::test]
    async fn a_visible_queue_can_be_listed_and_joined() {
        let (m, bus, db, _library) = access_mgr().await;
        let owner = db_session(&db, "s1", "alice").await;
        let other = db_session(&db, "s2", "bob").await;

        let movie = seed_movie(&db, "Movie").await;

        let info = m.new_group(&owner, "movie night").await.unwrap();
        set_queue(&m, &owner, vec![movie]).await;

        assert_eq!(m.list_groups(&other).await.unwrap().len(), 1);
        m.join_group(&other, info.group_id).await.unwrap();
        assert!(
            !update_types(&bus, "s2").contains(&"LibraryAccessDenied".to_owned()),
            "a resolvable queue is not denied"
        );
        assert_eq!(
            m.get_group(&owner, info.group_id)
                .await
                .unwrap()
                .participants
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn a_queue_change_a_member_cannot_see_is_refused() {
        let (m, bus, db, _library) = access_mgr().await;
        let owner = db_session(&db, "s1", "alice").await;
        let other = db_session(&db, "s2", "bob").await;

        let info = m.new_group(&owner, "movie night").await.unwrap();
        m.join_group(&other, info.group_id).await.unwrap();
        bus.sent.lock().unwrap().clear();

        // A queue of items nobody can resolve is rejected outright — no state
        // change and, as upstream, no broadcast at all.
        m.handle_request(&owner, play(2)).await.unwrap();
        assert!(
            bus.sent.lock().unwrap().is_empty(),
            "a refused queue change broadcasts nothing"
        );
        assert_eq!(
            m.get_group(&owner, info.group_id).await.unwrap().state,
            GroupStateType::Idle,
            "and leaves the group in its previous state"
        );
    }

    #[tokio::test]
    async fn without_the_library_seam_every_queue_is_accessible() {
        // The unit-test wiring (no library) must stay permissive, so the rest of
        // the suite exercises group mechanics without a database.
        let (m, _bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        m.handle_request(&a, play(2)).await.unwrap();
        assert_eq!(m.list_groups(&a).await.unwrap().len(), 1);
        assert!(m.get_group(&a, info.group_id).await.is_ok());
    }
}
