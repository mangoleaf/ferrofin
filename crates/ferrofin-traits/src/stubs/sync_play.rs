//! SyncPlay manager trait — synchronized group playback.
//!
//! Port of `MediaBrowser.Controller.SyncPlay.ISyncPlayManager` plus the
//! `IGroupPlaybackRequest` hierarchy, collapsed to Rust idiom: the C# per-request
//! strategy classes (`PlayGroupRequest`, `SeekGroupRequest`, …) become the
//! [`PlaybackRequest`] enum, and `SessionInfo` becomes the lightweight
//! [`SyncPlaySession`] identity the controller already has in hand.
//!
//! The concrete impl (`ferrofin-core`) owns the group registry, the per-group state
//! machine, and broadcasts commands to members via
//! [`SessionMessageBus`](crate::session_bus::SessionMessageBus).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_model::sync_play::{GroupInfoDto, GroupQueueMode, GroupRepeatMode, GroupShuffleMode};
use uuid::Uuid;

use crate::error::ServiceError;

/// The session identity a SyncPlay request carries.
///
/// Stands in for C# `SessionInfo`: a group tracks membership by `session_id` and
/// participants by `user_name`, and access checks key on `user_id`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncPlaySession {
    /// The session's id (group membership key).
    pub session_id: String,
    /// The authenticated user's id.
    pub user_id: Uuid,
    /// The authenticated user's display name (shown in `Participants`).
    pub user_name: String,
}

/// A playback request against a group — the `IGroupPlaybackRequest` hierarchy as
/// one enum. Ports the 16 `PlaybackRequestType` commands funneled through
/// `ISyncPlayManager.HandleRequest`.
#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackRequest {
    /// Set a brand-new play queue (`SetNewQueue`).
    Play {
        /// The ordered item ids forming the queue.
        playing_queue: Vec<Uuid>,
        /// Index of the item to play first.
        playing_item_position: i32,
        /// Start position of the first item, in ticks.
        start_position_ticks: i64,
    },
    /// Make an existing queue entry the current item.
    SetPlaylistItem {
        /// The playlist item to make current.
        playlist_item_id: Uuid,
    },
    /// Remove entries from the queue (or clear it).
    RemoveFromPlaylist {
        /// The playlist items to remove (ignored when clearing).
        playlist_item_ids: Vec<Uuid>,
        /// Clear the whole playlist.
        clear_playlist: bool,
        /// Also drop the playing item (only when clearing).
        clear_playing_item: bool,
    },
    /// Move a queue entry to a new index.
    MovePlaylistItem {
        /// The playlist item to move.
        playlist_item_id: Uuid,
        /// The new index.
        new_index: i32,
    },
    /// Enqueue items.
    Queue {
        /// The items to enqueue.
        item_ids: Vec<Uuid>,
        /// Where to insert them.
        mode: GroupQueueMode,
    },
    /// Request the group unpause.
    Unpause,
    /// Request the group pause.
    Pause,
    /// Request the group stop.
    Stop,
    /// Request the group seek.
    Seek {
        /// Target position, in ticks.
        position_ticks: i64,
    },
    /// Signal this client is buffering.
    Buffer {
        /// Client UTC when the request was made.
        when: DateTime<Utc>,
        /// Client playback position, in ticks.
        position_ticks: i64,
        /// Whether the client is unpaused.
        is_playing: bool,
        /// The playlist item the client is on.
        playlist_item_id: Uuid,
    },
    /// Signal this client is ready (buffered and in position).
    Ready {
        /// Client UTC when the request was made.
        when: DateTime<Utc>,
        /// Client playback position, in ticks.
        position_ticks: i64,
        /// Whether the client is unpaused.
        is_playing: bool,
        /// The playlist item the client is on.
        playlist_item_id: Uuid,
    },
    /// Advance to the next queue item.
    NextItem {
        /// The client's current playlist item (staleness guard).
        playlist_item_id: Uuid,
    },
    /// Go back to the previous queue item.
    PreviousItem {
        /// The client's current playlist item (staleness guard).
        playlist_item_id: Uuid,
    },
    /// Set the repeat mode.
    SetRepeatMode {
        /// The repeat mode.
        mode: GroupRepeatMode,
    },
    /// Set the shuffle mode.
    SetShuffleMode {
        /// The shuffle mode.
        mode: GroupShuffleMode,
    },
    /// Report this client's measured ping.
    Ping {
        /// The ping, in milliseconds.
        ping: i64,
    },
    /// Toggle whether this client is ignored while the group waits.
    IgnoreWait {
        /// Whether to ignore this client.
        ignore_wait: bool,
    },
}

/// The SyncPlay manager: coordinates synchronized group playback.
///
/// Port of `ISyncPlayManager`. Group creation/join/leave mutate a live group
/// registry; [`handle_request`](SyncPlayManager::handle_request) drives the
/// per-group state machine and pushes `SyncPlayCommand`/`SyncPlayGroupUpdate`
/// messages to member sockets.
#[async_trait]
pub trait SyncPlayManager: Send + Sync {
    /// Creates a new group owned by the session, returning its info.
    async fn new_group(
        &self,
        session: &SyncPlaySession,
        group_name: &str,
    ) -> Result<GroupInfoDto, ServiceError>;

    /// Adds the session to an existing group.
    async fn join_group(
        &self,
        session: &SyncPlaySession,
        group_id: Uuid,
    ) -> Result<(), ServiceError>;

    /// Removes the session from its current group (idempotent).
    async fn leave_group(&self, session: &SyncPlaySession) -> Result<(), ServiceError>;

    /// Lists the groups visible to the session.
    async fn list_groups(
        &self,
        session: &SyncPlaySession,
    ) -> Result<Vec<GroupInfoDto>, ServiceError>;

    /// Returns info for a single group, or `NotFound`.
    async fn get_group(
        &self,
        session: &SyncPlaySession,
        group_id: Uuid,
    ) -> Result<GroupInfoDto, ServiceError>;

    /// Applies a playback request to the session's group, broadcasting any
    /// resulting commands/updates to members.
    async fn handle_request(
        &self,
        session: &SyncPlaySession,
        request: PlaybackRequest,
    ) -> Result<(), ServiceError>;

    /// Whether the user currently participates in any group.
    async fn is_user_active(&self, user_id: Uuid) -> Result<bool, ServiceError>;
}

fn _assert_object_safe_sync_play_manager(_: &dyn SyncPlayManager) {}
