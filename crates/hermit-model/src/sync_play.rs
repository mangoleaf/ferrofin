//! Port of `MediaBrowser.Model.SyncPlay`.
//!
//! The polymorphic C# `GroupUpdate<T>` hierarchy (nine `SyncPlay*Update`
//! subclasses each overriding `Type`) is modeled as a single serde
//! internally-tagged enum [`GroupUpdate`] keyed on the `Type` discriminator,
//! matching the OpenAPI `oneOf`/`discriminator` contract. Each variant carries
//! its `GroupId` and typed `Data` payload.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// The mode for inserting items into a group queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum GroupQueueMode {
    /// Insert items at the end of the queue.
    #[default]
    Queue = 0,
    /// Insert items after the currently playing item.
    QueueNext = 1,
}

/// The repeat mode of a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum GroupRepeatMode {
    /// Repeat one item only.
    RepeatOne = 0,
    /// Cycle the playlist.
    RepeatAll = 1,
    /// Do not repeat.
    #[default]
    RepeatNone = 2,
}

/// The shuffle mode of a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum GroupShuffleMode {
    /// Sorted playlist.
    #[default]
    Sorted = 0,
    /// Shuffled playlist.
    Shuffle = 1,
}

/// The state of a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum GroupStateType {
    /// The group is idle. No media is playing.
    #[default]
    Idle = 0,
    /// The group is waiting. Playback is paused; will start when users are
    /// ready.
    Waiting = 1,
    /// The group is paused. Will resume on play command.
    Paused = 2,
    /// The group is playing. Playback is advancing.
    Playing = 3,
}

/// The type of a group update (used as the [`GroupUpdate`] discriminator).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum GroupUpdateType {
    /// Tells members of a group about a new user.
    #[default]
    UserJoined,
    /// Tells members of a group that a user left.
    UserLeft,
    /// Tells a user that the group has been joined.
    GroupJoined,
    /// Tells a user that the group has been left.
    GroupLeft,
    /// Tells members of the group that the state changed.
    StateUpdate,
    /// Tells a user the playing queue of the group.
    PlayQueue,
    /// Tells a user that they don't belong to a group.
    NotInGroup,
    /// Sent when trying to join a non-existing group.
    GroupDoesNotExist,
    /// Sent when a user tries to join a group without required library access.
    LibraryAccessDenied,
}

/// The type of a `SyncPlay` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum RequestType {
    /// A user is requesting to create a new group.
    #[default]
    NewGroup = 0,
    /// A user is requesting to join a group.
    JoinGroup = 1,
    /// A user is requesting to leave a group.
    LeaveGroup = 2,
    /// A user is requesting the list of available groups.
    ListGroups = 3,
    /// A user is sending a playback command to a group.
    Playback = 4,
}

/// The type of a command sent to group members.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum SendCommandType {
    /// Instructs users to unpause playback.
    #[default]
    Unpause = 0,
    /// Instructs users to pause playback.
    Pause = 1,
    /// Instructs users to stop playback.
    Stop = 2,
    /// Instructs users to seek to a specified time.
    Seek = 3,
}

/// Used to filter the sessions of a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum SyncPlayBroadcastType {
    /// All sessions will receive the message.
    #[default]
    AllGroup = 0,
    /// Only the specified session will receive the message.
    CurrentSession = 1,
    /// All sessions except the current one will receive the message.
    AllExceptCurrentSession = 2,
    /// Only sessions that are not buffering will receive the message.
    AllReady = 3,
}

/// The type of a playback request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum PlaybackRequestType {
    /// A user is setting a new playlist.
    #[default]
    Play = 0,
    /// A user is changing the playlist item.
    SetPlaylistItem = 1,
    /// A user is removing items from the playlist.
    RemoveFromPlaylist = 2,
    /// A user is moving an item in the playlist.
    MovePlaylistItem = 3,
    /// A user is adding items to the playlist.
    Queue = 4,
    /// A user is requesting an unpause command for the group.
    Unpause = 5,
    /// A user is requesting a pause command for the group.
    Pause = 6,
    /// A user is requesting a stop command for the group.
    Stop = 7,
    /// A user is requesting a seek command for the group.
    Seek = 8,
    /// A user is signaling that playback is buffering.
    Buffer = 9,
    /// A user is signaling that playback resumed.
    Ready = 10,
    /// A user is requesting the next item in playlist.
    NextItem = 11,
    /// A user is requesting the previous item in playlist.
    PreviousItem = 12,
    /// A user is setting the repeat mode.
    SetRepeatMode = 13,
    /// A user is setting the shuffle mode.
    SetShuffleMode = 14,
    /// A user is reporting their ping.
    Ping = 15,
    /// A user is requesting to be ignored on group wait.
    IgnoreWait = 16,
}

/// The reason a play queue update was emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum PlayQueueUpdateReason {
    /// A user is requesting to play a new playlist.
    #[default]
    NewPlaylist = 0,
    /// A user is changing the playing item.
    SetCurrentItem = 1,
    /// A user is removing items from the playlist.
    RemoveItems = 2,
    /// A user is moving an item in the playlist.
    MoveItem = 3,
    /// A user is adding items to the queue.
    Queue = 4,
    /// A user is adding items to the queue, after the currently playing item.
    QueueNext = 5,
    /// A user is requesting the next item in queue.
    NextItem = 6,
    /// A user is requesting the previous item in queue.
    PreviousItem = 7,
    /// A user is changing repeat mode.
    RepeatMode = 8,
    /// A user is changing shuffle mode.
    ShuffleMode = 9,
}

/// A state-change update for a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct GroupStateUpdate {
    /// Gets the state of the group.
    pub state: GroupStateType,

    /// Gets the reason of the state change.
    pub reason: PlaybackRequestType,
}

/// A command to be sent to group members.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct SendCommand {
    /// Gets the group identifier.
    #[schema(value_type = String, format = "uuid")]
    pub group_id: Uuid,

    /// Gets the playlist identifier of the playing item.
    #[schema(value_type = String, format = "uuid")]
    pub playlist_item_id: Uuid,

    /// Gets or sets the UTC time when to execute the command.
    #[schema(value_type = String, format = "date-time")]
    pub when: DateTime<Utc>,

    /// Gets the position ticks, for commands that require it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_ticks: Option<i64>,

    /// Gets the command.
    pub command: SendCommandType,

    /// Gets the UTC time when this command has been emitted.
    #[schema(value_type = String, format = "date-time")]
    pub emitted_at: DateTime<Utc>,
}

/// A single item in a `SyncPlay` queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct SyncPlayQueueItem {
    /// Gets the item identifier.
    #[schema(value_type = String, format = "uuid")]
    pub item_id: Uuid,

    /// Gets the playlist identifier of the item.
    #[schema(value_type = String, format = "uuid")]
    pub playlist_item_id: Uuid,
}

/// An update to the play queue of a group.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PlayQueueUpdate {
    /// Gets the request type that originated this update.
    pub reason: PlayQueueUpdateReason,

    /// Gets the UTC time of the last change to the playing queue.
    #[schema(value_type = String, format = "date-time")]
    pub last_update: DateTime<Utc>,

    /// Gets the playlist.
    pub playlist: Vec<SyncPlayQueueItem>,

    /// Gets the playing item index in the playlist.
    pub playing_item_index: i32,

    /// Gets the start position ticks.
    pub start_position_ticks: i64,

    /// Gets a value indicating whether the current item is playing.
    pub is_playing: bool,

    /// Gets the shuffle mode.
    pub shuffle_mode: GroupShuffleMode,

    /// Gets the repeat mode.
    pub repeat_mode: GroupRepeatMode,
}

/// Information about a `SyncPlay` group.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct GroupInfoDto {
    /// Gets the group identifier.
    #[schema(value_type = String, format = "uuid")]
    pub group_id: Uuid,

    /// Gets the group name.
    pub group_name: String,

    /// Gets the group state.
    pub state: GroupStateType,

    /// Gets the participants.
    pub participants: Vec<String>,

    /// Gets the date when this DTO has been created.
    #[schema(value_type = String, format = "date-time")]
    pub last_updated_at: DateTime<Utc>,
}

/// A response carrying the server's UTC time (for clock synchronization).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct UtcTimeResponse {
    /// Gets the UTC time when the request has been received.
    #[schema(value_type = String, format = "date-time")]
    pub request_reception_time: DateTime<Utc>,

    /// Gets the UTC time when the response has been sent.
    #[schema(value_type = String, format = "date-time")]
    pub response_transmission_time: DateTime<Utc>,
}

/// Request body for `POST /SyncPlay/New` — create a group.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct NewGroupRequestDto {
    /// The name for the new group.
    pub group_name: String,
}

/// Request body for `POST /SyncPlay/Join` — join an existing group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct JoinGroupRequestDto {
    /// The identifier of the group to join.
    #[schema(value_type = String, format = "uuid")]
    pub group_id: Uuid,
}

/// Request body for `POST /SyncPlay/SetNewQueue` — set the group play queue.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct PlayRequestDto {
    /// The ordered item ids that make up the queue.
    #[schema(value_type = Vec<String>, format = "uuid")]
    pub playing_queue: Vec<Uuid>,

    /// The index (in `playing_queue`) of the item to play first.
    pub playing_item_position: i32,

    /// The start position for the first item, in ticks.
    pub start_position_ticks: i64,
}

/// Request body for `POST /SyncPlay/SetPlaylistItem` — change the playing item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct SetPlaylistItemRequestDto {
    /// The playlist identifier of the item to make current.
    #[schema(value_type = String, format = "uuid")]
    pub playlist_item_id: Uuid,
}

/// Request body for `POST /SyncPlay/RemoveFromPlaylist` — remove queue entries.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct RemoveFromPlaylistRequestDto {
    /// The playlist identifiers to remove (ignored when clearing the playlist).
    #[schema(value_type = Vec<String>, format = "uuid")]
    pub playlist_item_ids: Vec<Uuid>,

    /// Whether the entire playlist should be cleared.
    pub clear_playlist: bool,

    /// Whether the playing item should also be removed (only when clearing).
    pub clear_playing_item: bool,
}

/// Request body for `POST /SyncPlay/MovePlaylistItem` — reorder a queue entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct MovePlaylistItemRequestDto {
    /// The playlist identifier of the item to move.
    #[schema(value_type = String, format = "uuid")]
    pub playlist_item_id: Uuid,

    /// The new position for the item.
    pub new_index: i32,
}

/// Request body for `POST /SyncPlay/Queue` — enqueue items.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct QueueRequestDto {
    /// The items to enqueue.
    #[schema(value_type = Vec<String>, format = "uuid")]
    pub item_ids: Vec<Uuid>,

    /// Where to insert the items.
    pub mode: GroupQueueMode,
}

/// Request body for `POST /SyncPlay/Seek` — seek the group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct SeekRequestDto {
    /// The position to seek to, in ticks.
    pub position_ticks: i64,
}

/// Request body for `POST /SyncPlay/Buffering` — signal a client is buffering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct BufferRequestDto {
    /// When the client made the request (client UTC).
    #[schema(value_type = String, format = "date-time")]
    pub when: DateTime<Utc>,

    /// The client's playback position, in ticks.
    pub position_ticks: i64,

    /// Whether the client's playback is unpaused.
    pub is_playing: bool,

    /// The playlist item the client is playing.
    #[schema(value_type = String, format = "uuid")]
    pub playlist_item_id: Uuid,
}

/// Request body for `POST /SyncPlay/Ready` — signal a client is ready.
///
/// Same shape as [`BufferRequestDto`]; kept distinct to match the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct ReadyRequestDto {
    /// When the client made the request (client UTC).
    #[schema(value_type = String, format = "date-time")]
    pub when: DateTime<Utc>,

    /// The client's playback position, in ticks.
    pub position_ticks: i64,

    /// Whether the client's playback is unpaused.
    pub is_playing: bool,

    /// The playlist item the client is playing.
    #[schema(value_type = String, format = "uuid")]
    pub playlist_item_id: Uuid,
}

/// Request body for `POST /SyncPlay/SetIgnoreWait` — toggle wait-ignoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct IgnoreWaitRequestDto {
    /// Whether this client should be ignored when the group waits.
    pub ignore_wait: bool,
}

/// Request body for `POST /SyncPlay/NextItem` — advance the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct NextItemRequestDto {
    /// The playlist item the client is currently playing.
    #[schema(value_type = String, format = "uuid")]
    pub playlist_item_id: Uuid,
}

/// Request body for `POST /SyncPlay/PreviousItem` — go back in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct PreviousItemRequestDto {
    /// The playlist item the client is currently playing.
    #[schema(value_type = String, format = "uuid")]
    pub playlist_item_id: Uuid,
}

/// Request body for `POST /SyncPlay/SetRepeatMode` — set repeat mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct SetRepeatModeRequestDto {
    /// The repeat mode to apply.
    pub mode: GroupRepeatMode,
}

/// Request body for `POST /SyncPlay/SetShuffleMode` — set shuffle mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct SetShuffleModeRequestDto {
    /// The shuffle mode to apply.
    pub mode: GroupShuffleMode,
}

/// Request body for `POST /SyncPlay/Ping` — report a client's measured ping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct PingRequestDto {
    /// The measured ping time, in milliseconds.
    pub ping: i64,
}

/// A polymorphic group update, internally tagged by the `Type` discriminator.
///
/// This unifies the C# `GroupUpdate<T>` subclasses; each variant carries the
/// `GroupId` and a typed `Data` payload matching that subclass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "Type")]
pub enum GroupUpdate {
    /// A new user joined the group.
    UserJoined(UserJoinedUpdate),
    /// A user left the group.
    UserLeft(UserLeftUpdate),
    /// The user joined a group.
    GroupJoined(GroupJoinedUpdate),
    /// The user left a group.
    GroupLeft(GroupLeftUpdate),
    /// The group state changed.
    StateUpdate(StateUpdate),
    /// The play queue of the group changed.
    PlayQueue(PlayQueueGroupUpdate),
    /// The user does not belong to a group.
    NotInGroup(NotInGroupUpdate),
    /// The requested group does not exist.
    GroupDoesNotExist(GroupDoesNotExistUpdate),
    /// The user lacks the required library access to join.
    LibraryAccessDenied(LibraryAccessDeniedUpdate),
}

/// The `UserJoined` variant payload (`SyncPlayUserJoinedUpdate`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct UserJoinedUpdate {
    /// Gets the group identifier.
    #[schema(value_type = String, format = "uuid")]
    pub group_id: Uuid,

    /// Gets the update data (the joining user's name).
    pub data: String,
}

/// The `UserLeft` variant payload (`SyncPlayUserLeftUpdate`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct UserLeftUpdate {
    /// Gets the group identifier.
    #[schema(value_type = String, format = "uuid")]
    pub group_id: Uuid,

    /// Gets the update data (the leaving user's name).
    pub data: String,
}

/// The `GroupJoined` variant payload (`SyncPlayGroupJoinedUpdate`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct GroupJoinedUpdate {
    /// Gets the group identifier.
    #[schema(value_type = String, format = "uuid")]
    pub group_id: Uuid,

    /// Gets the update data (the joined group's info).
    pub data: GroupInfoDto,
}

/// The `GroupLeft` variant payload (`SyncPlayGroupLeftUpdate`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct GroupLeftUpdate {
    /// Gets the group identifier.
    #[schema(value_type = String, format = "uuid")]
    pub group_id: Uuid,

    /// Gets the update data (the left group's id, stringified).
    pub data: String,
}

/// The `StateUpdate` variant payload (`SyncPlayStateUpdate`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct StateUpdate {
    /// Gets the group identifier.
    #[schema(value_type = String, format = "uuid")]
    pub group_id: Uuid,

    /// Gets the update data (the new group state).
    pub data: GroupStateUpdate,
}

/// The `PlayQueue` variant payload (`SyncPlayPlayQueueUpdate`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PlayQueueGroupUpdate {
    /// Gets the group identifier.
    #[schema(value_type = String, format = "uuid")]
    pub group_id: Uuid,

    /// Gets the update data (the play queue update).
    pub data: PlayQueueUpdate,
}

/// The `NotInGroup` variant payload (`SyncPlayNotInGroupUpdate`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct NotInGroupUpdate {
    /// Gets the group identifier.
    #[schema(value_type = String, format = "uuid")]
    pub group_id: Uuid,

    /// Gets the update data.
    pub data: String,
}

/// The `GroupDoesNotExist` variant payload (`SyncPlayGroupDoesNotExistUpdate`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct GroupDoesNotExistUpdate {
    /// Gets the group identifier.
    #[schema(value_type = String, format = "uuid")]
    pub group_id: Uuid,

    /// Gets the update data.
    pub data: String,
}

/// The `LibraryAccessDenied` variant payload
/// (`SyncPlayLibraryAccessDeniedUpdate`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LibraryAccessDeniedUpdate {
    /// Gets the group identifier.
    #[schema(value_type = String, format = "uuid")]
    pub group_id: Uuid,

    /// Gets the update data.
    pub data: String,
}
