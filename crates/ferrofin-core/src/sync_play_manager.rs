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

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration as StdDuration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use tracing::Instrument;
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
use ferrofin_util::shuffle_extensions::shuffle;

/// .NET tick resolution: 100-nanosecond units, so 10_000 ticks per millisecond.
/// Clients expect `PositionTicks`/`When` on the wire in these units.
const TICKS_PER_MS: i64 = 10_000;
/// Default latency cushion added when scheduling a command's `When`, in ms.
/// ponytail: Jellyfin's `Group.DefaultPing`; surface as a setting if tuned.
const DEFAULT_PING_MS: i64 = 500;
/// Upper bound on a client-reported ping, in ms. Pings are scaled into resume
/// delays, so an unbounded value lets one session push the whole group's
/// playback arbitrarily far out. Upstream `Group.MaxPing` (Group.cs).
const MAX_PING_MS: i64 = 10_000;
/// How stale a client's self-reported `When` may be before its elapsed time is
/// discarded, in ms. ponytail: Jellyfin's `Group.TimeSyncOffset` (Group.cs:97).
const TIME_SYNC_OFFSET_MS: i64 = 2000;
/// How far a client's reported position may sit from the group's before it is
/// corrected with a seek, in ms. Jellyfin's `Group.MaxPlaybackOffset`
/// (Group.cs:103).
const MAX_PLAYBACK_OFFSET_MS: i64 = 500;
/// How far a member that reports Ready while PLAYING may lag the group before
/// it is corrected with a seek, in ms: it may still be catching up after
/// buffering. Jellyfin 12.1's `IGroupStateContext.MaxCatchUpOffset`
/// (IGroupStateContext.cs:40), a hardcoded interface default. A jump of the
/// group position during the wait takes the tolerance away
/// ([`Group::position_jumped`]).
const MAX_CATCH_UP_OFFSET_MS: i64 = 60_000;

/// `Group.DefaultGroupWaitTimeout` (12.1): how long a group waits for its
/// members to report ready before it carries on without the ones that have
/// not.
const DEFAULT_GROUP_WAIT_TIMEOUT: StdDuration = StdDuration::from_secs(30);

/// `SyncPlayManager.GroupWaitSweepInterval` (12.1): how often the manager
/// looks for a group whose wait has run out, while any group exists.
const GROUP_WAIT_SWEEP_INTERVAL: StdDuration = StdDuration::from_secs(1);

/// The `task` field of the group-wait sweep's span and events.
const SWEEP_TASK: &str = "sync_play_group_wait_sweep";

/// A .NET tick count as a [`Duration`]. Ticks are 100 ns, so ten to the
/// microsecond; going through microseconds keeps the full resolution that
/// milliseconds would round away.
fn ticks(count: i64) -> Duration {
    Duration::microseconds(count / 10)
}

/// A [`Duration`] as .NET ticks (100 ns units) — the inverse of [`ticks`].
///
/// `TimeSpan.Ticks` in the C# is the FULL 100 ns count, so every position this
/// server derives from a clock difference has to be too. Rounding through
/// `num_milliseconds()` quantises the result to a multiple of 10_000 ticks,
/// which is visible on the wire next to a Jellyfin that emits the exact value
/// (every Ferrofin `PositionTicks` ended in four zeroes; no Jellyfin one did).
///
/// `chrono` only fails to answer in nanoseconds beyond ~292 years, and in
/// microseconds beyond ~292_000; both fallbacks are unreachable for a playback
/// delta but are written out rather than saturating silently.
fn duration_ticks(delta: Duration) -> i64 {
    if let Some(ns) = delta.num_nanoseconds() {
        ns / 100
    } else if let Some(us) = delta.num_microseconds() {
        us.saturating_mul(10)
    } else {
        delta.num_milliseconds().saturating_mul(TICKS_PER_MS)
    }
}

/// `PlayQueueManager.NoPlayingItemIndex` — the sentinel `PlayingItemIndex` a
/// queue carries when nothing is playing. It goes out on the wire as `-1`, and
/// `AbstractGroupState.HandleRequest(RemoveFromPlaylistGroupRequest)` keys the
/// group's drop to `Idle` on it (`!context.PlayQueue.IsItemPlaying()`).
const NO_PLAYING_ITEM_INDEX: i32 = -1;

/// The two server→client WebSocket message types SyncPlay uses.
const MSG_COMMAND: &str = "SyncPlayCommand";
const MSG_GROUP_UPDATE: &str = "SyncPlayGroupUpdate";

/// Who a produced message is addressed to — port of [`SyncPlayBroadcastType`]
/// and of the `Group.FilterSessions` switch that resolves it (Group.cs:171-189).
///
/// [`SyncPlayBroadcastType`]: ferrofin_model::sync_play::SyncPlayBroadcastType
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// Every member.
    AllGroup,
    /// Only the requesting session (a corrective resync of one lagging client).
    CurrentSession,
    /// Every member EXCEPT the requesting session — used when the client that
    /// was buffering is already playing and only the others need to resume.
    AllExceptCurrentSession,
    /// Only members that are not buffering (tell the ready ones to wait).
    AllReady,
}

/// The four fields a `ReadyGroupRequest` carries, grouped so the reconciliation
/// arm keeps a readable signature.
#[derive(Debug, Clone, Copy)]
struct ReadyReport {
    /// The client's UTC clock when it sent the report.
    when: DateTime<Utc>,
    /// The client's playback position, in ticks.
    position_ticks: i64,
    /// Whether the client is unpaused.
    is_playing: bool,
    /// The playlist item the client believes it is on.
    playlist_item_id: Uuid,
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

/// The group's play queue (port of `PlayQueueManager`).
///
/// Like upstream it keeps the queue twice: `sorted` in the order it was built,
/// and `shuffled`, which holds the same items in shuffled order only while
/// `shuffle` is [`GroupShuffleMode::Shuffle`] (PlayQueueManager.cs:22-32).
/// `playing_index` points into whichever of the two is active
/// ([`PlayQueue::playlist`]), and that list is the one every `PlayQueue`
/// update sends. Moves change only the active list; additions and removals
/// change both.
#[derive(Debug)]
struct PlayQueue {
    sorted: Vec<SyncPlayQueueItem>,
    shuffled: Vec<SyncPlayQueueItem>,
    playing_index: i32,
    shuffle: GroupShuffleMode,
    repeat: GroupRepeatMode,
}

/// `PlayQueueManager()` (PlayQueueManager.cs:37-40) calls `Reset()`, which sets
/// `PlayingItemIndex = NoPlayingItemIndex` (-1, :20 and :100) — a fresh queue
/// has NOTHING playing.
///
/// This cannot be `#[derive(Default)]`: `i32::default()` is 0, which is a valid
/// INDEX, so a queued-but-never-played group reported `PlayingItemIndex: 0` on
/// the wire against Jellyfin's `-1`, `is_item_playing()` claimed an unplayed
/// queue was playing, and `clear(false)` kept a phantom item Jellyfin does not
/// keep. Measured live on the parity pair before this was fixed.
impl Default for PlayQueue {
    fn default() -> Self {
        Self {
            sorted: Vec::new(),
            shuffled: Vec::new(),
            playing_index: NO_PLAYING_ITEM_INDEX,
            shuffle: GroupShuffleMode::default(),
            repeat: GroupRepeatMode::default(),
        }
    }
}

/// `CreateQueueItemsFromArray` (PlayQueueManager.cs:527-537): each item gets a
/// fresh server-side `PlaylistItemId`.
fn new_queue_items(item_ids: &[Uuid]) -> Vec<SyncPlayQueueItem> {
    item_ids
        .iter()
        .map(|&item_id| SyncPlayQueueItem {
            item_id,
            playlist_item_id: Uuid::new_v4(),
        })
        .collect()
}

/// The position of `playlist_item_id` in `list`, as a queue index (`-1` when
/// absent) — C#'s `FindIndex`/`IndexOf` on a playlist.
fn index_of(list: &[SyncPlayQueueItem], playlist_item_id: Uuid) -> i32 {
    list.iter()
        .position(|i| i.playlist_item_id == playlist_item_id)
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(NO_PLAYING_ITEM_INDEX)
}

/// The insertion point right after queue index `index` — `index + 1`, so `-1`
/// (nothing playing) inserts at the front — kept inside `list`.
fn after(list: &[SyncPlayQueueItem], index: i32) -> usize {
    usize::try_from(index + 1)
        .unwrap_or_default()
        .min(list.len())
}

impl PlayQueue {
    /// Replaces the queue with `item_ids`, assigning each a fresh server-side
    /// `PlaylistItemId`, and sets the playing item to `position`.
    ///
    /// `Group.SetPlayQueue` (Group.cs:503) calls `PlayQueue.Reset()` FIRST, and
    /// `Reset` (PlayQueueManager.cs:367-375) clears shuffle and repeat back to
    /// their defaults as well as the items — a new queue arrives unshuffled and
    /// not repeating, which is not what "replace the items" alone would do.
    fn set(&mut self, item_ids: &[Uuid], position: i32) {
        self.shuffle = GroupShuffleMode::default();
        self.repeat = GroupRepeatMode::default();
        self.shuffled.clear();
        self.sorted = new_queue_items(item_ids);
        let max_index = i32::try_from(self.sorted.len().saturating_sub(1)).unwrap_or(0);
        self.playing_index = position.clamp(0, max_index);
    }

    /// `GetPlaylistInternal` (PlayQueueManager.cs:543-551): the shuffled list
    /// in Shuffle mode, the sorted one otherwise. `playing_index` indexes it.
    fn playlist(&self) -> &[SyncPlayQueueItem] {
        if self.shuffle == GroupShuffleMode::Shuffle {
            &self.shuffled
        } else {
            &self.sorted
        }
    }

    /// The active list's length, as a queue index bound.
    fn playlist_len(&self) -> i32 {
        i32::try_from(self.playlist().len()).unwrap_or(i32::MAX)
    }

    /// The queue's item ids, for the library-access checks.
    ///
    /// Read from `sorted`, which holds the same items as the active list but
    /// keeps its order through a shuffle: `join_group` compares this list
    /// before and after its off-lock access check, and a reshuffle in between
    /// changes no item a member can or cannot see.
    fn item_ids(&self) -> Vec<Uuid> {
        self.sorted.iter().map(|i| i.item_id).collect()
    }

    /// The `PlaylistItemId` of the current item (nil if the queue is empty).
    fn playing_item_id(&self) -> Uuid {
        self.current()
            .map(|i| i.playlist_item_id)
            .unwrap_or_default()
    }

    /// `GetPlayingItem` (PlayQueueManager.cs:557-570).
    fn current(&self) -> Option<&SyncPlayQueueItem> {
        usize::try_from(self.playing_index)
            .ok()
            .and_then(|i| self.playlist().get(i))
    }

    /// Port of `SetPlayingItemByPlaylistId` (PlayQueueManager.cs:262-269):
    /// points the queue at `playlist_item_id` and returns whether it was found.
    /// An id that is not queued leaves NOTHING playing, as upstream does — it
    /// does not keep the item that was playing before.
    fn set_playing(&mut self, playlist_item_id: Uuid) -> bool {
        self.playing_index = index_of(self.playlist(), playlist_item_id);
        self.is_item_playing()
    }

    /// Port of `Next` (PlayQueueManager.cs:457-486): returns whether the
    /// playing item changed.
    ///
    /// An empty queue has no next item in any repeat mode. `RepeatOne` stays
    /// on the item (the group restarts it); past the last item `RepeatAll`
    /// wraps to the first, and anything else stays on the last and fails.
    fn next(&mut self) -> bool {
        if self.playlist().is_empty() {
            return false;
        }
        if self.repeat == GroupRepeatMode::RepeatOne {
            return true;
        }
        self.playing_index += 1;
        if self.playing_index >= self.playlist_len() {
            if self.repeat == GroupRepeatMode::RepeatAll {
                self.playing_index = 0;
            } else {
                self.playing_index = self.playlist_len() - 1;
                return false;
            }
        }
        true
    }

    /// Port of `Previous` (PlayQueueManager.cs:492-521), the mirror of
    /// [`PlayQueue::next`]: before the first item `RepeatAll` wraps to the
    /// last, and anything else stays on the first and fails.
    fn previous(&mut self) -> bool {
        if self.playlist().is_empty() {
            return false;
        }
        if self.repeat == GroupRepeatMode::RepeatOne {
            return true;
        }
        self.playing_index -= 1;
        if self.playing_index < 0 {
            if self.repeat == GroupRepeatMode::RepeatAll {
                self.playing_index = self.playlist_len() - 1;
            } else {
                self.playing_index = 0;
                return false;
            }
        }
        true
    }

    /// Appends items, either at the end (`Queue`, PlayQueueManager.cs:108-119)
    /// or right after the current item (`QueueNext`, :206-224), in the given
    /// order.
    ///
    /// In Shuffle mode both lists take the items: at the end of each, or after
    /// the playing item in each — its own position in `sorted`, and the
    /// playing index in `shuffled`.
    fn enqueue(&mut self, item_ids: &[Uuid], mode: GroupQueueMode) {
        let new_items = new_queue_items(item_ids);
        let shuffled = self.shuffle == GroupShuffleMode::Shuffle;
        match mode {
            GroupQueueMode::Queue | GroupQueueMode::Unrecognized(_) => {
                if shuffled {
                    self.shuffled.extend_from_slice(&new_items);
                }
                self.sorted.extend(new_items);
            }
            GroupQueueMode::QueueNext => {
                if shuffled {
                    let sorted_playing = index_of(&self.sorted, self.playing_item_id());
                    let at = after(&self.shuffled, self.playing_index);
                    self.shuffled.splice(at..at, new_items.iter().copied());
                    let at = after(&self.sorted, sorted_playing);
                    self.sorted.splice(at..at, new_items);
                } else {
                    let at = after(&self.sorted, self.playing_index);
                    self.sorted.splice(at..at, new_items);
                }
            }
        }
    }

    /// `PlayQueueManager.IsItemPlaying` — whether the queue points at an item.
    fn is_item_playing(&self) -> bool {
        self.playing_index != NO_PLAYING_ITEM_INDEX
    }

    /// Port of `PlayQueueManager.ClearPlaylist` (PlayQueueManager.cs:179-200).
    ///
    /// The playing item is KEPT unless `clear_playing_item` says otherwise, and
    /// shuffle/repeat survive either way — `PlayQueue::default()` threw all three
    /// away, so a `ClearPlaylist=true, ClearPlayingItem=false` request emptied a
    /// Ferrofin queue that Jellyfin leaves one item long.
    fn clear(&mut self, clear_playing_item: bool) {
        let playing = self.current().copied();
        self.sorted.clear();
        self.shuffled.clear();
        match (clear_playing_item, playing) {
            (false, Some(item)) => {
                self.sorted.push(item);
                if self.shuffle == GroupShuffleMode::Shuffle {
                    self.shuffled.push(item);
                }
                self.playing_index = 0;
            }
            _ => self.playing_index = NO_PLAYING_ITEM_INDEX,
        }
    }

    /// Port of `PlayQueueManager.RemoveFromPlaylist` (PlayQueueManager.cs:295-335):
    /// removes the given playlist items from both lists and returns whether the
    /// PLAYING item was one of them — which is what `AbstractGroupState` needs
    /// to decide whether the group falls to `Idle`.
    ///
    /// When the playing item goes, upstream steps BACK to the item before it
    /// ("picking previous item") and only falls forward to 0 if it was the
    /// first; an empty remainder leaves no playing item at all. Removed items
    /// in front of the playing item shift its index as well
    /// (`removedBeforePlayingItem`, PlayQueueManager.cs:299-306, :318), counted
    /// in the active list.
    fn remove(&mut self, ids: &[Uuid]) -> bool {
        let playing = self.current().copied();
        let playing_at = usize::try_from(self.playing_index).unwrap_or_default();
        let removed_before_playing = match playing {
            Some(_) => self.playlist()[..playing_at]
                .iter()
                .filter(|i| ids.contains(&i.playlist_item_id))
                .count(),
            None => 0,
        };
        self.sorted.retain(|i| !ids.contains(&i.playlist_item_id));
        self.shuffled.retain(|i| !ids.contains(&i.playlist_item_id));
        let Some(playing) = playing else {
            return false;
        };
        if !ids.contains(&playing.playlist_item_id) {
            self.reanchor(playing.playlist_item_id);
            return false;
        }
        // At most `playing_at` items were counted, and that came from an i32.
        self.playing_index -=
            i32::try_from(removed_before_playing).unwrap_or(self.playing_index) + 1;
        if self.playing_index < 0 {
            self.playing_index = if self.playlist().is_empty() {
                NO_PLAYING_ITEM_INDEX
            } else {
                0
            };
        }
        true
    }

    /// Port of `MovePlaylistItem` (PlayQueueManager.cs:343-362): moves
    /// `playlist_item_id` to `new_index` in the ACTIVE list only — moving an
    /// item while shuffled reorders the shuffle, not the sorted queue a later
    /// Sorted request restores. Returns false, changing nothing, when the item
    /// is not in the queue.
    fn move_item(&mut self, playlist_item_id: Uuid, new_index: i32) -> bool {
        let Ok(from) = usize::try_from(index_of(self.playlist(), playlist_item_id)) else {
            return false;
        };
        let current = self.playing_item_id();
        let list = if self.shuffle == GroupShuffleMode::Shuffle {
            &mut self.shuffled
        } else {
            &mut self.sorted
        };
        let item = list.remove(from);
        // `Math.Clamp(newIndex, 0, playlist.Count)` on the list with the item
        // already lifted out — the upper bound is `len`, not `len - 1`, so the
        // last position is reachable.
        let to = usize::try_from(new_index).unwrap_or(0).min(list.len());
        list.insert(to, item);
        self.reanchor(current);
        true
    }

    /// Port of `SetShuffleMode` (PlayQueueManager.cs:391-401): `Shuffle`
    /// (re)shuffles, any other mode restores the sorted queue.
    fn set_shuffle_mode(&mut self, mode: GroupShuffleMode) {
        if mode == GroupShuffleMode::Shuffle {
            self.shuffle_playlist();
        } else {
            self.restore_sorted_playlist();
        }
    }

    /// Port of `ShufflePlaylist` (PlayQueueManager.cs:124-153).
    ///
    /// With nothing playing the whole sorted queue is shuffled. Otherwise the
    /// rest of the active list is shuffled behind the playing item, which
    /// becomes the first: shuffling the first time starts from the sorted
    /// queue, shuffling again re-shuffles the shuffled one.
    fn shuffle_playlist(&mut self) {
        match self.current().copied() {
            None => {
                self.shuffled.clone_from(&self.sorted);
                shuffle(&mut self.shuffled);
            }
            Some(playing) => {
                let mut rest = self.playlist().to_vec();
                rest.remove(usize::try_from(self.playing_index).unwrap_or_default());
                shuffle(&mut rest);
                rest.insert(0, playing);
                self.shuffled = rest;
                self.playing_index = 0;
            }
        }
        self.shuffle = GroupShuffleMode::Shuffle;
    }

    /// Port of `RestoreSortedPlaylist` (PlayQueueManager.cs:158-173): the
    /// playing item keeps playing at its place in the sorted queue, and the
    /// shuffled list is dropped. Asking for the sorted queue while already
    /// sorted changes nothing (`shuffled` is empty then).
    fn restore_sorted_playlist(&mut self) {
        if self.is_item_playing() && !self.shuffled.is_empty() {
            let playing = usize::try_from(self.playing_index)
                .ok()
                .and_then(|i| self.shuffled.get(i))
                .map(|i| i.playlist_item_id)
                .unwrap_or_default();
            self.playing_index = index_of(&self.sorted, playing);
        }
        self.shuffled.clear();
        self.shuffle = GroupShuffleMode::Sorted;
    }

    /// Re-points `playing_index` at the item that had id `current` after a
    /// mutation — `PlayingItemIndex = playlist.IndexOf(playingItem)`, which
    /// answers `-1` (nothing playing) when the item is gone. Clamping to `0`
    /// instead silently started playing a different item.
    fn reanchor(&mut self, current: Uuid) {
        self.playing_index = index_of(self.playlist(), current);
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
    /// Whether playback should resume once every member reports Ready — the
    /// `WaitingGroupState.ResumePlaying` flag, set when the group entered
    /// `Waiting` from `Playing`.
    resume_playing: bool,
    /// `WaitingGroupState.InitialState` — the state the group was in when it
    /// started waiting, which is what a Ready that settles the group replays.
    /// `None` whenever the group is not `Waiting`: upstream builds a fresh
    /// `WaitingGroupState` per entry, so the memory dies with the state object.
    initial_state: Option<GroupStateType>,
    /// `WaitingGroupState.PositionJumped` (12.1): whether the group position
    /// moved during this wait — a Seek, a new queue, SetPlaylistItem,
    /// Next/PreviousItem, or the Unpause restart from `Idle`. A member that
    /// reports Ready while playing behind such a jump has not applied it, so it
    /// gets no catch-up tolerance. Like `initial_state`, it dies with the wait.
    position_jumped: bool,
    /// `PlayingGroupState.IgnoreBuffering` — armed by an Unpause that forced
    /// playback out of `Waiting`, and cleared by the next entry into `Playing`.
    ignore_buffering: bool,
    /// `Group.RunTimeTicks`, resolved once per item as it enters the queue.
    /// Keyed by item id, so re-pointing the queue needs no further lookups.
    item_runtimes: HashMap<Uuid, i64>,
    /// `Group.GroupWaitTimeout` (12.1).
    wait_timeout: StdDuration,
    /// `Group.GroupWaitDeadline` (12.1): when the group gives up waiting for
    /// its members, or `None` when it is not waiting for anyone.
    wait_deadline: Option<Instant>,
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
            resume_playing: false,
            initial_state: None,
            position_jumped: false,
            ignore_buffering: false,
            item_runtimes: HashMap::new(),
            wait_timeout: DEFAULT_GROUP_WAIT_TIMEOUT,
            wait_deadline: None,
        }
    }

    /// Port of `Group.GetInfo()`.
    ///
    /// `LastUpdatedAt` is the instant the DTO is BUILT — upstream passes
    /// `DateTime.UtcNow` straight into the constructor, so the value advances on
    /// every read rather than freezing at the last mutation. `Participants` is
    /// LINQ `Distinct()` over the participant dictionary: first-occurrence order,
    /// not sorted.
    fn info(&self, now: DateTime<Utc>) -> GroupInfoDto {
        let mut seen = std::collections::HashSet::new();
        let participants: Vec<String> = self
            .members
            .iter()
            .filter(|m| seen.insert(m.user_name.clone()))
            .map(|m| m.user_name.clone())
            .collect();
        GroupInfoDto {
            group_id: self.id,
            group_name: self.name.clone(),
            state: self.state,
            participants,
            last_updated_at: now,
        }
    }

    fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    fn highest_ping_ms(&self) -> i64 {
        self.members
            .iter()
            .map(|m| m.ping_ms)
            .max()
            .unwrap_or(DEFAULT_PING_MS)
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
                // `Group.AddSession` seeds the member with `DefaultPing`
                // (Group.cs:145-153), NOT with the `GroupMember.Ping` property's
                // own zero default — so `GetHighestPing` is 500 before any
                // client has reported one, and every scheduled `When` is
                // `now + max(2*500, 500) = now + 1s`. Measured: both servers
                // stamp an Unpause exactly 1.000 s ahead of `EmittedAt`.
                ping_ms: DEFAULT_PING_MS,
                is_buffering: false,
                ignore_wait: false,
            });
        }
    }

    fn remove_member(&mut self, session_id: &str) {
        self.members.retain(|m| m.session_id != session_id);
        self.update_group_wait_deadline(false);
    }

    /// Port of `IGroupStateContext.SetBuffering(session, buffering)`.
    fn set_buffering(&mut self, session_id: &str, buffering: bool) {
        if let Some(m) = self.member_mut(session_id) {
            m.is_buffering = buffering;
        }
        self.update_group_wait_deadline(false);
    }

    /// Whether the group is still waiting on anybody — port of
    /// `Group.IsBuffering` (Group.cs:475-486), which is `IsBuffering &&
    /// !IgnoreGroupWait`: a member that asked not to be waited for
    /// (`POST /SyncPlay/SetIgnoreWait`) does not hold the group.
    fn is_buffering(&self) -> bool {
        self.members
            .iter()
            .any(|m| m.is_buffering && !m.ignore_wait)
    }

    /// The `WaitingGroupState.ResumePlaying` update shared by every entry into
    /// `Waiting` — `SessionJoined` and `HandleRequest(Seek/Buffer)`.
    ///
    /// Upstream writes it as THREE arms, not two
    /// (`MediaBrowser.Controller/SyncPlay/GroupStates/WaitingGroupState.cs`,
    /// v10.11.8 — Seek at :297-303, Buffer at :345-388): `prevState == Playing`
    /// arms the flag, `prevState == Paused` clears it, and ANY OTHER previous
    /// state — in practice `Waiting`, i.e. the group is already waiting on
    /// somebody — LEAVES IT UNTOUCHED, because that state object already holds
    /// the answer from whichever transition created it.
    ///
    /// Collapsing that to `resume_playing = prev == Playing` silently clears the
    /// flag on the third arm: a group that dropped from `Playing` to `Waiting`
    /// (a join, a seek) and then saw a second Buffer would settle into `Paused`
    /// where Jellyfin resumes playing.
    fn resume_playing_from(&mut self, prev_state: GroupStateType) {
        match prev_state {
            GroupStateType::Playing => self.resume_playing = true,
            GroupStateType::Paused => self.resume_playing = false,
            GroupStateType::Waiting | GroupStateType::Idle => {}
        }
    }

    /// `Group.RestartCurrentItem` (Group.cs:601-605) — rewind to the head of the
    /// item and restamp the group's clock.
    fn restart_current_item(&mut self, now: DateTime<Utc>) {
        self.position_ticks = 0;
        self.last_activity = now;
    }

    fn set_all_buffering(&mut self, buffering: bool) {
        for m in &mut self.members {
            m.is_buffering = buffering;
        }
        // Resetting the status of every member starts a new waiting period.
        self.update_group_wait_deadline(buffering);
    }

    /// Port of `Group.UpdateGroupWaitDeadline` (12.1): the group has a
    /// deadline only while it is `Waiting` on a member that does not ignore
    /// the wait. A running deadline covers the waiting period as a whole, so
    /// members that keep reporting buffering while they load do not push it
    /// back; only a new waiting period restarts it.
    fn update_group_wait_deadline(&mut self, start_new_waiting_period: bool) {
        if self.state != GroupStateType::Waiting || !self.is_buffering() {
            self.wait_deadline = None;
            return;
        }
        if self.wait_deadline.is_none() || start_new_waiting_period {
            self.wait_deadline = Some(Instant::now() + self.wait_timeout);
        }
    }

    /// Port of `Group.HandleGroupWaitTimeout` (12.1, `Group.cs:738-795`):
    /// once the deadline has passed, the group stops waiting for the members
    /// that have not reported ready. A group that was going to resume starts
    /// playing without them (the waiting state's forced Unpause); any other
    /// group settles into `Paused` where the wait started. Returns the session
    /// acting for the group (any blocking one, as the recovery is broadcast to
    /// the whole group) and the messages to deliver.
    fn handle_group_wait_timeout(
        &mut self,
        now: Instant,
        now_utc: DateTime<Utc>,
    ) -> Option<(String, Vec<Outbound>)> {
        let deadline = self.wait_deadline?;
        if deadline > now {
            return None;
        }
        self.wait_deadline = None;
        if self.state != GroupStateType::Waiting {
            return None;
        }
        let blocking: Vec<String> = self
            .members
            .iter()
            .filter(|m| m.is_buffering && !m.ignore_wait)
            .map(|m| m.session_id.clone())
            .collect();
        let session = blocking.first()?.clone();
        let waited = self.wait_timeout + now.saturating_duration_since(deadline);
        tracing::warn!(
            group_id = %self.id,
            waited_ms = waited.as_millis(),
            session_ids = %blocking.join(", "),
            "sync-play group gave up waiting for sessions to report ready"
        );
        if self.resume_playing {
            // An Unpause in the waiting state means "start now, ignoring the
            // sessions that are not ready".
            return Some((
                session,
                self.handle_unpause(GroupStateType::Waiting, now_utc),
            ));
        }
        // The members have been paused for the whole waiting period, so the
        // playback position stays where the wait started.
        self.set_all_buffering(false);
        self.set_state(GroupStateType::Paused);
        let outbound = vec![
            Outbound::all(self.command_env(SendCommandType::Pause, self.last_activity, now_utc)),
            Outbound::all(self.state_update_env(PlaybackRequestType::Pause)),
        ];
        Some((session, outbound))
    }

    /// The command's scheduled execution instant: `now` + a latency cushion
    /// sized to the slowest member so every client can hit it. Port of
    /// `Group.LastActivity = now + max(2*highestPing, DefaultPing)`.
    fn unpause_when(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let cushion = (2 * self.highest_ping_ms()).max(DEFAULT_PING_MS);
        now + Duration::milliseconds(cushion)
    }

    /// Banks the real time elapsed since `last_activity` into `position_ticks`
    /// — `PositionTicks += Math.Max(elapsedTime.Ticks, 0)`.
    ///
    /// The floor matters: `LastActivity` is in the FUTURE while a scheduled
    /// unpause has not fired yet, so a negative elapsed means playback never
    /// started and nothing should be banked. Upstream guards this at the CALL
    /// SITE (only the `prevState` arms that came from `Playing` run it), never
    /// on the group's current state, so this helper does not second-guess it.
    fn advance_position(&mut self, now: DateTime<Utc>) {
        let elapsed = duration_ticks(now - self.last_activity).max(0);
        self.position_ticks += elapsed;
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
        // `Group.GetPlayQueueUpdate` (Group.cs:650-677) reports where playback
        // WILL be, not where the last mutation left it: while the group is
        // playing it adds the time elapsed since `LastActivity` (floored at 0,
        // because a scheduled unpause may not have fired yet).
        let is_playing = self.state == GroupStateType::Playing;
        let mut start_position_ticks = self.position_ticks;
        if is_playing {
            start_position_ticks += duration_ticks(now - self.last_activity).max(0);
        }
        let update = PlayQueueUpdate {
            reason,
            last_update: now,
            playlist: self.queue.playlist().to_vec(),
            playing_item_index: self.queue.playing_index,
            start_position_ticks,
            is_playing,
            shuffle_mode: self.queue.shuffle,
            repeat_mode: self.queue.repeat,
        };
        render_update(GroupUpdate::PlayQueue(PlayQueueGroupUpdate {
            group_id: self.id,
            data: update,
        }))
    }

    /// Enters `Waiting` from `prev`, recording `WaitingGroupState.InitialState`.
    ///
    /// Upstream constructs a FRESH `WaitingGroupState` on every entry, so
    /// `InitialStateSet` is false and the first request handled there records the
    /// state the group came from (`WaitingGroupState.cs:129-134` and its twin at
    /// the head of every other arm). While the group STAYS `Waiting` the same
    /// object — and the same `InitialState` — is reused; leaving `Waiting` throws
    /// it away, which [`Group::set_state`] models by clearing the field.
    fn enter_waiting(&mut self, prev: GroupStateType) {
        if self.state != GroupStateType::Waiting {
            self.position_jumped = false;
        }
        if self.state != GroupStateType::Waiting || self.initial_state.is_none() {
            self.initial_state = Some(prev);
        }
        self.state = GroupStateType::Waiting;
    }

    /// The state a `WaitingGroupState` arm falls back to when the queue change
    /// it was entered for cannot be applied.
    ///
    /// Upstream does NOT restore `prevState`. Every failed arm writes the same
    /// switch (WaitingGroupState.cs:144-148 Play, :189-194 SetPlaylistItem,
    /// :595-600 NextItem, :641-646 PreviousItem):
    ///
    /// ```text
    /// prevState switch { Playing => Playing, Paused => Paused, _ => Idle }
    /// ```
    ///
    /// so a group that was ALREADY `Waiting` when the request arrived falls
    /// through to the default arm and drops to `Idle` — it does not go back to
    /// waiting for a queue that no longer exists. Restoring `prev` verbatim left
    /// Ferrofin in `Waiting` where Jellyfin was `Idle`, measured live on the
    /// parity pair with `SetNewQueue {"PlayingQueue": []}` issued from `Waiting`.
    fn restore_after_failed_queue_change(&mut self, prev: GroupStateType) {
        self.set_state(match prev {
            GroupStateType::Playing => GroupStateType::Playing,
            GroupStateType::Paused => GroupStateType::Paused,
            GroupStateType::Waiting | GroupStateType::Idle => GroupStateType::Idle,
        });
    }

    /// Port of `IGroupStateContext.SetState` (Group.cs:382-386).
    ///
    /// Every C# state object is constructed fresh at the transition, so the two
    /// pieces of per-state memory die with it: leaving `Waiting` drops
    /// `InitialState`, and entering `Playing` drops `IgnoreBuffering` — only the
    /// forced-Unpause path (`WaitingGroupState.cs:236-239`) re-arms it.
    fn set_state(&mut self, new_state: GroupStateType) {
        if new_state != GroupStateType::Waiting {
            self.initial_state = None;
            self.position_jumped = false;
            self.wait_deadline = None;
        }
        if new_state == GroupStateType::Playing {
            self.ignore_buffering = false;
        }
        self.state = new_state;
    }

    /// `Group.RunTimeTicks` — the run time of the CURRENT playing item.
    ///
    /// Upstream re-reads it from `_libraryManager.GetItemById(...)` at every
    /// point the playing item changes (Group.cs:506-507/520-526/550-558/613-614/
    /// 628-629). Ferrofin resolves the run times ONCE, as the items enter the
    /// queue — the manager is already reading those rows for the access check —
    /// and looks the current one up here: a run time is a property of the item,
    /// so the two are equivalent and this one costs no query per transition.
    ///
    /// `None` means "not known here": the library seam is unwired (unit tests),
    /// which is the only way an item that passed the access check can be absent.
    fn run_time_ticks(&self) -> Option<i64> {
        self.queue
            .current()
            .and_then(|i| self.item_runtimes.get(&i.item_id).copied())
    }

    /// Port of `Group.SanitizePositionTicks` — `Math.Clamp(ticks, 0, RunTimeTicks)`
    /// (Group.cs:429-432). With no library seam wired only the floor applies.
    fn sanitize_position_ticks(&self, ticks: i64) -> i64 {
        match self.run_time_ticks() {
            Some(run_time) => ticks.clamp(0, run_time.max(0)),
            None => ticks.max(0),
        }
    }

    /// `IdleGroupState.SendStopCommand` (IdleGroupState.cs:113-125).
    ///
    /// Every Stop upstream routes through `IdleGroupState.HandleRequest`, which
    /// branches on where the group came from: `prevState != Idle` broadcasts to
    /// `AllGroup`, `prevState == Idle` answers the REQUESTING SESSION only
    /// ("client got lost, sending current state"). `NewSyncPlayCommand` stamps
    /// `When = LastActivity` (Group.cs:417-426) and nothing on this path advances
    /// it, so `When` is the group's last real activity, NOT `now`.
    fn send_stop_command(&mut self, prev: GroupStateType, now: DateTime<Utc>) -> Vec<Outbound> {
        self.set_state(GroupStateType::Idle);
        let cmd = self.command_env(SendCommandType::Stop, self.last_activity, now);
        if prev == GroupStateType::Idle {
            vec![Outbound::to(Target::CurrentSession, cmd)]
        } else {
            vec![Outbound::all(cmd)]
        }
    }

    /// Applies a playback request, mutating state and returning the messages to
    /// broadcast. `now` is threaded in for determinism/testability.
    ///
    /// `session_id` is the REQUESTING session. Upstream's per-state handlers take
    /// a `SessionInfo` and use it both for `SetBuffering` and for the
    /// `CurrentSession` / `AllExceptCurrentSession` broadcast filters, so the
    /// identity has to reach the state machine rather than stopping at the
    /// manager.
    ///
    /// `IGroupPlaybackRequest.Apply` is always
    /// `state.HandleRequest(this, context, state.Type, session, ct)`, so `prev`
    /// below is the state the group was in when the request arrived — which is
    /// exactly what every `prevState.Equals(Type)` branch keys on.
    fn handle(
        &mut self,
        session_id: &str,
        request: &PlaybackRequest,
        now: DateTime<Utc>,
    ) -> Vec<Outbound> {
        let prev = self.state;
        match request {
            PlaybackRequest::Play {
                playing_queue,
                playing_item_position,
                start_position_ticks,
            } => self.handle_play(
                prev,
                playing_queue,
                *playing_item_position,
                *start_position_ticks,
                now,
            ),
            PlaybackRequest::SetPlaylistItem { playlist_item_id } => {
                self.handle_set_playlist_item(prev, *playlist_item_id, now)
            }
            PlaybackRequest::RemoveFromPlaylist {
                playlist_item_ids,
                clear_playlist,
                clear_playing_item,
            } => self.handle_remove_from_playlist(
                prev,
                playlist_item_ids,
                *clear_playlist,
                *clear_playing_item,
                now,
            ),
            PlaybackRequest::MovePlaylistItem {
                playlist_item_id,
                new_index,
            } => self.handle_move_playlist_item(*playlist_item_id, *new_index, now),
            PlaybackRequest::Queue { item_ids, mode } => {
                // `Group.AddToPlayQueue` (Group.cs:571-598) returns false on an
                // empty list, and `AbstractGroupState.cs:114-132` broadcasts
                // NOTHING when it does. (The other false arm — a member without
                // access — is refused before the state machine runs.)
                if item_ids.is_empty() {
                    return Vec::new();
                }
                self.queue.enqueue(item_ids, *mode);
                let reason = match mode {
                    GroupQueueMode::Queue | GroupQueueMode::Unrecognized(_) => {
                        PlayQueueUpdateReason::Queue
                    }
                    GroupQueueMode::QueueNext => PlayQueueUpdateReason::QueueNext,
                };
                vec![Outbound::all(self.play_queue_env(reason, now))]
            }
            PlaybackRequest::Unpause => self.handle_unpause(prev, now),
            PlaybackRequest::Pause => self.handle_pause(prev, now),
            PlaybackRequest::Stop => self.send_stop_command(prev, now),
            PlaybackRequest::Seek { position_ticks } => {
                self.handle_seek(prev, *position_ticks, now)
            }
            PlaybackRequest::Buffer {
                playlist_item_id, ..
            } => self.handle_buffer(prev, session_id, *playlist_item_id, now),
            PlaybackRequest::Ready {
                when,
                position_ticks,
                is_playing,
                playlist_item_id,
            } => self.handle_ready(
                prev,
                session_id,
                ReadyReport {
                    when: *when,
                    position_ticks: *position_ticks,
                    is_playing: *is_playing,
                    playlist_item_id: *playlist_item_id,
                },
                now,
            ),
            PlaybackRequest::NextItem { playlist_item_id } => {
                self.handle_queue_step(prev, *playlist_item_id, true, now)
            }
            PlaybackRequest::PreviousItem { playlist_item_id } => {
                self.handle_queue_step(prev, *playlist_item_id, false, now)
            }
            PlaybackRequest::SetRepeatMode { mode } => {
                self.queue.repeat = *mode;
                vec![Outbound::all(
                    self.play_queue_env(PlayQueueUpdateReason::RepeatMode, now),
                )]
            }
            PlaybackRequest::SetShuffleMode { mode } => {
                self.queue.set_shuffle_mode(*mode);
                vec![Outbound::all(
                    self.play_queue_env(PlayQueueUpdateReason::ShuffleMode, now),
                )]
            }
            PlaybackRequest::IgnoreWait { .. } => self.handle_ignore_wait(prev, now),
            // `AbstractGroupState.HandleRequest(PingGroupRequest)` only records
            // the ping, which the manager already applied to the member.
            PlaybackRequest::Ping { .. } => Vec::new(),
        }
    }

    /// `POST /SyncPlay/MovePlaylistItem` — `AbstractGroupState.cs:98-111`: an
    /// item that is not in the queue moves nothing and broadcasts nothing.
    fn handle_move_playlist_item(
        &mut self,
        playlist_item_id: Uuid,
        new_index: i32,
        now: DateTime<Utc>,
    ) -> Vec<Outbound> {
        if !self.queue.move_item(playlist_item_id, new_index) {
            tracing::debug!(
                group_id = %self.id,
                %playlist_item_id,
                "sync-play: MovePlaylistItem names an item not in the queue; ignored"
            );
            return Vec::new();
        }
        vec![Outbound::all(
            self.play_queue_env(PlayQueueUpdateReason::MoveItem, now),
        )]
    }

    /// `POST /SyncPlay/RemoveFromPlaylist` — `AbstractGroupState.cs:69-95`.
    ///
    /// Two things it does that a blanket "wipe the queue and Stop" does not:
    /// `ClearPlayingItem` decides whether the playing item survives the clear,
    /// and the group only drops to `Idle` when the playing item was removed AND
    /// nothing is playing afterwards. So `ClearPlaylist=true,
    /// ClearPlayingItem=false` leaves a one-item queue still playing, and a
    /// removal that misses the playing item never stops anybody.
    fn handle_remove_from_playlist(
        &mut self,
        prev: GroupStateType,
        playlist_item_ids: &[Uuid],
        clear_playlist: bool,
        clear_playing_item: bool,
        now: DateTime<Utc>,
    ) -> Vec<Outbound> {
        let playing_item_removed = if clear_playlist {
            self.queue.clear(clear_playing_item);
            // `Group.ClearPlayQueue` (Group.cs:535-542) restarts the current
            // item only when it was the one cleared.
            if clear_playing_item {
                self.restart_current_item(now);
            }
            clear_playing_item
        } else {
            let removed = self.queue.remove(playlist_item_ids);
            // `Group.RemoveFromPlayQueue` (Group.cs:545-565).
            if removed {
                self.restart_current_item(now);
            }
            removed
        };
        let mut out = vec![Outbound::all(
            self.play_queue_env(PlayQueueUpdateReason::RemoveItems, now),
        )];
        if playing_item_removed && !self.queue.is_item_playing() {
            // The Stop goes through `IdleGroupState.HandleRequest` with
            // `prevState = Type`, so it obeys the same scope + `When` rules as
            // every other Stop.
            out.extend(self.send_stop_command(prev, now));
        }
        out
    }

    /// `POST /SyncPlay/SetNewQueue`. Every state delegates a `PlayGroupRequest`
    /// to `WaitingGroupState.HandleRequest` (WaitingGroupState.cs:127-162): the
    /// group WAITS for every member's Ready before playing. It does not start
    /// playing and it sends no command — only the queue update.
    fn handle_play(
        &mut self,
        prev: GroupStateType,
        queue: &[Uuid],
        position: i32,
        start_position_ticks: i64,
        now: DateTime<Utc>,
    ) -> Vec<Outbound> {
        self.enter_waiting(prev);
        self.resume_playing = true;
        // `Group.SetPlayQueue` (Group.cs:489-512) refuses an empty queue or an
        // out-of-range position; the state machine then broadcasts nothing and
        // settles per `restore_after_failed_queue_change` — which is NOT `prev`
        // when `prev` was `Waiting` (WaitingGroupState.cs:139-152).
        let out_of_range =
            usize::try_from(position).map_or(true, |p| queue.is_empty() || p >= queue.len());
        self.position_jumped = !out_of_range;
        if out_of_range {
            self.restore_after_failed_queue_change(prev);
            return Vec::new();
        }
        self.queue.set(queue, position);
        self.position_ticks = start_position_ticks;
        self.last_activity = now;
        let out = vec![Outbound::all(
            self.play_queue_env(PlayQueueUpdateReason::NewPlaylist, now),
        )];
        // Sent BEFORE the members are reset to buffering, as upstream does — the
        // update is `AllGroup`, so the order is only visible to `AllReady` peers.
        self.set_all_buffering(true);
        out
    }

    /// `POST /SyncPlay/SetPlaylistItem` — `AbstractGroupState.cs:61-66` delegates
    /// to `WaitingGroupState.cs:165-200`. `Group.SetPlayingItem` restarts the
    /// item whether or not it was found (Group.cs:515-532).
    fn handle_set_playlist_item(
        &mut self,
        prev: GroupStateType,
        playlist_item_id: Uuid,
        now: DateTime<Utc>,
    ) -> Vec<Outbound> {
        self.enter_waiting(prev);
        self.resume_playing = true;
        let found = self.queue.set_playing(playlist_item_id);
        self.position_jumped = found;
        self.position_ticks = 0;
        self.last_activity = now;
        if !found {
            self.restore_after_failed_queue_change(prev);
            return Vec::new();
        }
        let out = vec![Outbound::all(
            self.play_queue_env(PlayQueueUpdateReason::SetCurrentItem, now),
        )];
        self.set_all_buffering(true);
        out
    }

    /// `POST /SyncPlay/NextItem` / `PreviousItem` — every state delegates to
    /// `WaitingGroupState.cs:563-652`: the client's `PlaylistItemId` must be the
    /// one actually playing (a stale duplicate request is dropped, leaving the
    /// group in the `Waiting` the delegation put it in), the group waits for
    /// fresh Ready events, and a queue that cannot move settles per
    /// [`Group::restore_after_failed_queue_change`].
    fn handle_queue_step(
        &mut self,
        prev: GroupStateType,
        playlist_item_id: Uuid,
        forward: bool,
        now: DateTime<Utc>,
    ) -> Vec<Outbound> {
        self.enter_waiting(prev);
        self.resume_playing = true;
        if playlist_item_id != self.queue.playing_item_id() {
            return Vec::new();
        }
        let moved = if forward {
            self.queue.next()
        } else {
            self.queue.previous()
        };
        self.position_jumped = moved;
        if !moved {
            self.restore_after_failed_queue_change(prev);
            return Vec::new();
        }
        self.position_ticks = 0;
        self.last_activity = now;
        let reason = if forward {
            PlayQueueUpdateReason::NextItem
        } else {
            PlayQueueUpdateReason::PreviousItem
        };
        let out = vec![Outbound::all(self.play_queue_env(reason, now))];
        self.set_all_buffering(true);
        out
    }

    /// `POST /SyncPlay/Unpause`, one arm per state.
    fn handle_unpause(&mut self, prev: GroupStateType, now: DateTime<Utc>) -> Vec<Outbound> {
        match prev {
            // IdleGroupState.cs:57-63 -> WaitingGroupState.cs:212-225: an idle
            // group RESTARTS the current item and waits — it does not stop.
            GroupStateType::Idle => {
                self.enter_waiting(prev);
                self.resume_playing = true;
                // `Group.RestartCurrentItem` (Group.cs:601-605).
                self.position_ticks = 0;
                self.last_activity = now;
                self.position_jumped = true;
                let out = vec![Outbound::all(
                    self.play_queue_env(PlayQueueUpdateReason::NewPlaylist, now),
                )];
                self.set_all_buffering(true);
                out
            }
            GroupStateType::Waiting => {
                if self.resume_playing {
                    // WaitingGroupState.cs:228-242: force the playback to start,
                    // ignoring members that are not ready — group-wait stays off
                    // until the next state change.
                    self.set_all_buffering(false);
                    self.set_state(GroupStateType::Playing);
                    self.ignore_buffering = true;
                    self.last_activity = self.unpause_when(now);
                    vec![
                        Outbound::all(self.command_env(
                            SendCommandType::Unpause,
                            self.last_activity,
                            now,
                        )),
                        Outbound::all(self.state_update_env(PlaybackRequestType::Unpause)),
                    ]
                } else {
                    // WaitingGroupState.cs:243-250: the group would have settled
                    // into Paused; now it will play once ready. No command.
                    self.resume_playing = true;
                    vec![Outbound::all(
                        self.state_update_env(PlaybackRequestType::Unpause),
                    )]
                }
            }
            // PlayingGroupState.cs:81-86, `prevState == Type`: "client got lost".
            // The command carries the EXISTING `LastActivity` — resyncing one
            // client must not move everybody else's playback clock.
            GroupStateType::Playing => vec![Outbound::to(
                Target::CurrentSession,
                self.command_env(SendCommandType::Unpause, self.last_activity, now),
            )],
            // PausedGroupState.cs:57-63 -> PlayingGroupState.cs:64-80.
            GroupStateType::Paused => {
                self.set_state(GroupStateType::Playing);
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
        }
    }

    /// `POST /SyncPlay/Pause`, one arm per state.
    fn handle_pause(&mut self, prev: GroupStateType, now: DateTime<Utc>) -> Vec<Outbound> {
        match prev {
            // IdleGroupState.cs:67-70 — a Pause on an idle group answers the
            // caller with the group's current state, which is Stop.
            GroupStateType::Idle => self.send_stop_command(prev, now),
            // PlayingGroupState.cs:90-96 -> PausedGroupState.cs:66-87.
            GroupStateType::Playing => {
                self.advance_position(now);
                self.last_activity = now;
                self.set_state(GroupStateType::Paused);
                vec![
                    Outbound::all(self.command_env(
                        SendCommandType::Pause,
                        self.last_activity,
                        now,
                    )),
                    Outbound::all(self.state_update_env(PlaybackRequestType::Pause)),
                ]
            }
            // PausedGroupState.cs:88-93, `prevState == Type`: "client got lost",
            // so the caller alone is re-told the state. No position math, no
            // `LastActivity` write and NO state update.
            GroupStateType::Paused => vec![Outbound::to(
                Target::CurrentSession,
                self.command_env(SendCommandType::Pause, self.last_activity, now),
            )],
            // WaitingGroupState.cs:255-269: stay Waiting, disarm the resume, and
            // send the state update only — no command.
            GroupStateType::Waiting => {
                self.resume_playing = false;
                vec![Outbound::all(
                    self.state_update_env(PlaybackRequestType::Pause),
                )]
            }
        }
    }

    /// `POST /SyncPlay/Seek`.
    fn handle_seek(
        &mut self,
        prev: GroupStateType,
        position_ticks: i64,
        now: DateTime<Utc>,
    ) -> Vec<Outbound> {
        // IdleGroupState.cs:79-83 — Stop to the caller; the group stays Idle and
        // nothing else is touched.
        if prev == GroupStateType::Idle {
            return self.send_stop_command(prev, now);
        }
        // WaitingGroupState.cs:288-321.
        self.enter_waiting(prev);
        self.resume_playing_from(prev);
        self.position_ticks = self.sanitize_position_ticks(position_ticks);
        self.last_activity = now;
        self.position_jumped = true;
        let seek = Outbound::all(self.command_env(SendCommandType::Seek, self.last_activity, now));
        self.set_all_buffering(true);
        vec![
            seek,
            Outbound::all(self.state_update_env(PlaybackRequestType::Seek)),
        ]
    }

    /// `POST /SyncPlay/Buffering`.
    fn handle_buffer(
        &mut self,
        prev: GroupStateType,
        session_id: &str,
        playlist_item_id: Uuid,
        now: DateTime<Utc>,
    ) -> Vec<Outbound> {
        match prev {
            // IdleGroupState.cs:85-89 — Stop to the caller. Note there is no
            // `SetBuffering` on this path, which is why the manager cannot mark
            // the member before the state machine runs.
            GroupStateType::Idle => return self.send_stop_command(prev, now),
            // PlayingGroupState.cs:117-123 — a forced Unpause turned group-wait
            // off, so this buffer report is dropped entirely.
            GroupStateType::Playing if self.ignore_buffering => return Vec::new(),
            _ => {}
        }
        // Playing/Paused `SetState(Waiting)` BEFORE delegating, so even the
        // wrong-item early return below leaves the group in `Waiting`.
        self.enter_waiting(prev);
        // WaitingGroupState.cs:333-344: the client is on the wrong item, so it is
        // sent the queue and nothing else happens.
        if playlist_item_id != self.queue.playing_item_id() {
            self.set_buffering(session_id, true);
            return vec![Outbound::to(
                Target::CurrentSession,
                self.play_queue_env(PlayQueueUpdateReason::SetCurrentItem, now),
            )];
        }
        let mut out = Vec::new();
        match prev {
            // WaitingGroupState.cs:346-368.
            GroupStateType::Playing => {
                self.resume_playing = true;
                self.set_buffering(session_id, true);
                self.advance_position(now);
                self.last_activity = now;
                out.push(Outbound::to(
                    Target::AllReady,
                    self.command_env(SendCommandType::Pause, self.last_activity, now),
                ));
            }
            // WaitingGroupState.cs:369-379.
            GroupStateType::Paused => {
                self.resume_playing = false;
                self.set_buffering(session_id, true);
                out.push(Outbound::to(
                    Target::CurrentSession,
                    self.command_env(SendCommandType::Pause, self.last_activity, now),
                ));
            }
            // WaitingGroupState.cs:380-391 — the flag is LEFT ALONE here; only a
            // group that will not resume force-updates the new buffering client.
            GroupStateType::Waiting => {
                self.set_buffering(session_id, true);
                if !self.resume_playing {
                    out.push(Outbound::to(
                        Target::CurrentSession,
                        self.command_env(SendCommandType::Pause, self.last_activity, now),
                    ));
                }
            }
            GroupStateType::Idle => {}
        }
        out.push(Outbound::all(
            self.state_update_env(PlaybackRequestType::Buffer),
        ));
        out
    }

    /// `POST /SyncPlay/Ready`.
    ///
    /// Three of the four states just re-tell the caller what the group is doing
    /// (`prevState == Type`, "client got lost"); `Waiting` runs the real
    /// reconciliation, `WaitingGroupState.cs:398-560`.
    #[allow(clippy::too_many_lines)] // one branch per upstream arm — a table, not logic
    fn handle_ready(
        &mut self,
        prev: GroupStateType,
        session_id: &str,
        report: ReadyReport,
        now: DateTime<Utc>,
    ) -> Vec<Outbound> {
        match prev {
            // IdleGroupState.cs:91-95.
            GroupStateType::Idle => return self.send_stop_command(prev, now),
            // PlayingGroupState.cs:133-138.
            GroupStateType::Playing => {
                return vec![Outbound::to(
                    Target::CurrentSession,
                    self.command_env(SendCommandType::Unpause, self.last_activity, now),
                )];
            }
            // PausedGroupState.cs:126-131.
            GroupStateType::Paused => {
                return vec![Outbound::to(
                    Target::CurrentSession,
                    self.command_env(SendCommandType::Pause, self.last_activity, now),
                )];
            }
            GroupStateType::Waiting => {}
        }
        // WaitingGroupState.cs:407-418 — wrong item: resync the caller's queue.
        if report.playlist_item_id != self.queue.playing_item_id() {
            self.set_buffering(session_id, true);
            return vec![Outbound::to(
                Target::CurrentSession,
                self.play_queue_env(PlayQueueUpdateReason::SetCurrentItem, now),
            )];
        }
        // WaitingGroupState.cs:420-446: the client's clock is trusted only inside
        // `TimeSyncOffset`, and only while it says it is actually playing.
        let mut elapsed_ticks = duration_ticks(now - report.when);
        if elapsed_ticks.abs() > TIME_SYNC_OFFSET_MS * TICKS_PER_MS || !report.is_playing {
            elapsed_ticks = 0;
        }
        let request_ticks = self.sanitize_position_ticks(report.position_ticks);
        let delay_ticks = self.position_ticks - (request_ticks + elapsed_ticks);
        let max_playback_offset_ticks = MAX_PLAYBACK_OFFSET_MS * TICKS_PER_MS;
        if self.resume_playing {
            // :460-481 — it says ready, but it is nowhere near the group's
            // position: it has no idea where it is. Correct it. A member that
            // is playing may lag while it catches up after buffering, unless
            // the position jumped during this wait and it has not applied the
            // jump (PR 17797, 12.1).
            let max_offset_ticks = if report.is_playing && !self.position_jumped {
                MAX_CATCH_UP_OFFSET_MS * TICKS_PER_MS
            } else {
                max_playback_offset_ticks
            };
            if delay_ticks.abs() > max_offset_ticks {
                self.set_buffering(session_id, true);
                return vec![
                    Outbound::to(
                        Target::CurrentSession,
                        self.command_env(SendCommandType::Seek, self.last_activity, now),
                    ),
                    Outbound::all(self.state_update_env(PlaybackRequestType::Ready)),
                ];
            }
            self.set_buffering(session_id, false);
            // :471-479 — others are still buffering: this one is told to pause
            // when it reaches the group's position.
            if self.is_buffering() {
                let when = now + ticks(delay_ticks);
                return vec![Outbound::to(
                    Target::CurrentSession,
                    self.command_env(SendCommandType::Pause, when, now),
                )];
            }
            // :484-511 — everybody is ready, so playback starts.
            let target = if delay_ticks > 2 * self.highest_ping_ms() * TICKS_PER_MS {
                // The client that was buffering is still catching up; the others
                // resume when it gets there. It is already playing, so it is not
                // told again — unless it reported paused.
                self.last_activity = now + ticks(delay_ticks);
                if report.is_playing {
                    Target::AllExceptCurrentSession
                } else {
                    Target::AllGroup
                }
            } else {
                let recovered =
                    (2 * self.highest_ping_ms() * TICKS_PER_MS).max(DEFAULT_PING_MS * TICKS_PER_MS);
                self.last_activity = now + ticks(recovered);
                Target::AllGroup
            };
            let unpause = self.command_env(SendCommandType::Unpause, self.last_activity, now);
            // :513-516 -> PlayingGroupState.cs:139-143, `prevState == Waiting`:
            // the state update follows the command, so it reads `Playing`.
            self.set_state(GroupStateType::Playing);
            vec![
                Outbound::to(target, unpause),
                Outbound::all(self.state_update_env(PlaybackRequestType::Ready)),
            ]
        } else {
            // :522-535 — the group is settling into Paused, so the only question
            // is whether this client seeked to the right place.
            if (self.position_ticks - request_ticks).abs() > max_playback_offset_ticks {
                self.set_buffering(session_id, true);
                return vec![
                    Outbound::to(
                        Target::CurrentSession,
                        self.command_env(SendCommandType::Seek, self.last_activity, now),
                    ),
                    Outbound::all(self.state_update_env(PlaybackRequestType::Ready)),
                ];
            }
            self.set_buffering(session_id, false);
            if self.is_buffering() {
                return Vec::new();
            }
            // :540-558 — the group is ready; it returns to the state it was in
            // before it started waiting, replaying that state's request.
            let initial = self.initial_state;
            self.set_state(GroupStateType::Paused);
            match initial {
                // PausedGroupState.cs:66-87 with `prevState == Waiting`.
                Some(GroupStateType::Playing) => {
                    self.advance_position(now);
                    self.last_activity = now;
                    vec![
                        Outbound::all(self.command_env(
                            SendCommandType::Pause,
                            self.last_activity,
                            now,
                        )),
                        Outbound::all(self.state_update_env(PlaybackRequestType::Pause)),
                    ]
                }
                // PausedGroupState.cs:132-140 with `prevState == Waiting`.
                Some(GroupStateType::Paused) => vec![
                    Outbound::all(self.command_env(
                        SendCommandType::Pause,
                        self.last_activity,
                        now,
                    )),
                    Outbound::all(self.state_update_env(PlaybackRequestType::Ready)),
                ],
                _ => Vec::new(),
            }
        }
    }

    /// `POST /SyncPlay/SetIgnoreWait`.
    ///
    /// `AbstractGroupState.cs:207-211` only records the flag (the manager has
    /// already applied it to the member). `WaitingGroupState.cs:655-678`
    /// additionally re-evaluates the group wait: a member the group no longer
    /// waits for can be what releases it.
    fn handle_ignore_wait(&mut self, prev: GroupStateType, now: DateTime<Utc>) -> Vec<Outbound> {
        if prev != GroupStateType::Waiting || self.is_buffering() {
            return Vec::new();
        }
        if self.resume_playing {
            // `PlayingGroupState.HandleRequest(UnpauseGroupRequest)` with
            // `prevState == Waiting`, so the state update's reason is Unpause —
            // it reads `reason.Action` off the request the Waiting arm builds.
            self.set_state(GroupStateType::Playing);
            self.last_activity = self.unpause_when(now);
            vec![
                Outbound::all(self.command_env(SendCommandType::Unpause, self.last_activity, now)),
                Outbound::all(self.state_update_env(PlaybackRequestType::Unpause)),
            ]
        } else {
            // "Group is ready, returning to previous state" — a SILENT transition.
            self.set_state(GroupStateType::Paused);
            Vec::new()
        }
    }

    /// The group-state `SessionJoined` hook.
    ///
    /// `Group.CreateGroup` and `Group.SessionJoin` both END with
    /// `_state.SessionJoined(this, _state.Type, session, ct)` — after the member
    /// has been added and after the `GroupJoined`/`UserJoined` envelopes have gone
    /// out. Omitting it is why a client that joined a Ferrofin group was never
    /// told what to do with whatever it was already playing.
    ///
    /// * `Idle` — `IdleGroupState.SessionJoined` → `SendStopCommand`; because
    ///   `prevState == Type` it is addressed to the joining session ONLY.
    /// * `Playing` / `Paused` — both set `Waiting` and delegate to
    ///   `WaitingGroupState.SessionJoined`, which is also the `Waiting` arm.
    fn session_joined(&mut self, session_id: &str, now: DateTime<Utc>) -> Vec<Outbound> {
        let prev = self.state;
        if prev == GroupStateType::Idle {
            // `IdleGroupState.SessionJoined` passes its own `Type` as `prevState`,
            // so the Stop is addressed to the joining session alone.
            return self.send_stop_command(prev, now);
        }
        if prev == GroupStateType::Playing {
            // Pause the group and bank the time that has actually elapsed.
            self.advance_position(now);
            self.last_activity = now;
        }
        // Playing arms the flag, Paused clears it, an already-`Waiting` group
        // keeps whatever it holds — the same three arms as Seek/Buffer.
        self.resume_playing_from(prev);
        self.enter_waiting(prev);
        // Built after the state change, as upstream does — `IsPlaying` in the
        // queue update must read `Waiting`, not the state the group just left.
        let queue = self.play_queue_env(PlayQueueUpdateReason::NewPlaylist, now);
        // Marked buffering BEFORE the AllReady command is addressed, so the
        // joiner is not one of the sessions told to pause.
        self.set_buffering(session_id, true);
        vec![
            Outbound::to(Target::CurrentSession, queue),
            Outbound::to(
                Target::AllReady,
                self.command_env(SendCommandType::Pause, now, now),
            ),
        ]
    }

    /// The group-state `SessionLeaving` hook, run BEFORE the member is removed
    /// (`Group.SessionLeave` calls `_state.SessionLeaving(...)` first).
    ///
    /// Only `WaitingGroupState` does anything: it clears the leaver's buffering
    /// flag and, if the group was waiting on nobody else, either resumes
    /// (`ResumePlaying`) or settles into `Paused`. `Idle`/`Playing`/`Paused`
    /// leave silently.
    fn session_leaving(&mut self, session_id: &str, now: DateTime<Utc>) -> Vec<Outbound> {
        if self.state != GroupStateType::Waiting {
            return Vec::new();
        }
        self.set_buffering(session_id, false);
        if self.is_buffering() {
            return Vec::new();
        }
        if self.resume_playing {
            // `PlayingGroupState.HandleRequest(UnpauseGroupRequest)` with
            // `prevState == Waiting`: a scheduled Unpause to the whole group.
            self.set_state(GroupStateType::Playing);
            self.last_activity = self.unpause_when(now);
            vec![
                Outbound::all(self.command_env(SendCommandType::Unpause, self.last_activity, now)),
                Outbound::all(self.state_update_env(PlaybackRequestType::Unpause)),
            ]
        } else {
            self.set_state(GroupStateType::Paused);
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
        serde_json::Value::String(Uuid::new_v4().simple().to_string()),
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
    /// Whether the group-wait sweep task is running. It runs only while a
    /// group exists, as upstream arms its `_groupWaitTimer`.
    sweeping: bool,
}

/// The user + library seams a group needs to answer "may this user see the
/// items in that play queue?" (the C# `Group`'s `_userManager`/`_libraryManager`).
struct LibraryAccess {
    users: Arc<dyn UserManager>,
    library: Arc<dyn LibraryManager>,
}

/// The real SyncPlay manager: a group registry plus a socket message bus.
pub struct FerrofinSyncPlayManager {
    registry: Arc<Mutex<Registry>>,
    bus: Arc<dyn SessionMessageBus>,
    /// `SyncPlayManager.GroupWaitTimeout` (12.1), given to every new group.
    group_wait_timeout: StdDuration,
    /// How often the group-wait sweep runs; `None` runs no sweep task (tests
    /// drive it by hand).
    group_wait_sweep: Option<StdDuration>,
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
            registry: Arc::new(Mutex::new(Registry::default())),
            bus,
            group_wait_timeout: DEFAULT_GROUP_WAIT_TIMEOUT,
            group_wait_sweep: Some(GROUP_WAIT_SWEEP_INTERVAL),
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
    /// Uses the same standalone policy as by-ID endpoints. Explicit-ID browse
    /// queries intentionally have different semantics and cannot authorize queues.
    async fn has_access_to_queue(&self, user_id: Uuid, items: &[Uuid]) -> bool {
        match self.visible_subset(user_id, items).await {
            None => true,
            Some(visible) => items.iter().all(|id| visible.contains(id)),
        }
    }

    /// Which of `ids` the user may see, using one batched policy evaluation.
    ///
    /// `None` means no access seam is wired, i.e. everything is visible. An
    /// unresolvable user or a failed query yields an empty set, so a non-empty
    /// request denies (fail closed) while an empty one still passes.
    ///
    /// Batching matters: `list_groups` checks every group, and a per-group
    /// user-fetch-plus-query made that route cost `2N+1` statements — measured
    /// at 41 queries and p50 6ms for 50 groups before this was hoisted.
    async fn visible_subset(
        &self,
        user_id: Uuid,
        ids: &[Uuid],
    ) -> Option<std::collections::HashSet<Uuid>> {
        let access = self.access.as_ref()?;
        if ids.is_empty() {
            return Some(std::collections::HashSet::new());
        }
        // Failing closed is right, but failing closed *silently* is not: a
        // transient fault here hides every group from `List` and turns every
        // join into `LibraryAccessDenied`, which is indistinguishable from a
        // genuinely inaccessible queue. Say which it was.
        let user = match access.users.get_user_by_id(user_id).await {
            Ok(Some(user)) => user,
            Ok(None) => {
                tracing::warn!(%user_id, "sync-play access check: no such user — denying");
                return Some(std::collections::HashSet::new());
            }
            Err(err) => {
                tracing::warn!(%user_id, %err, "sync-play access check: user lookup failed — denying");
                return Some(std::collections::HashSet::new());
            }
        };
        match access.library.get_visible_item_ids(ids, &user).await {
            Ok(visible) => Some(visible.into_iter().collect()),
            Err(err) => {
                tracing::warn!(
                    %user_id,
                    items = ids.len(),
                    %err,
                    "sync-play access check: item lookup failed — denying"
                );
                Some(std::collections::HashSet::new())
            }
        }
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

    /// The run time of each of `ids`, in ONE query — the values behind
    /// `Group.RunTimeTicks`. An id the query does not return keeps whatever the
    /// group already knew; with no library seam wired (unit tests) the map is
    /// empty and [`Group::sanitize_position_ticks`] applies only its floor.
    async fn item_run_times(&self, ids: &[Uuid]) -> HashMap<Uuid, i64> {
        let mut out = HashMap::new();
        let Some(access) = self.access.as_ref() else {
            return out;
        };
        if ids.is_empty() {
            return out;
        }
        let query = InternalItemsQuery {
            item_ids: ids.to_vec(),
            ..InternalItemsQuery::default()
        };
        match access.library.get_item_list(&query).await {
            Ok(items) => {
                for item in items {
                    if let Ok(id) = Uuid::parse_str(&item.id) {
                        out.insert(id, item.run_time_ticks.unwrap_or(0).max(0));
                    }
                }
            }
            Err(err) => {
                // Not fatal: without a run time the clamp loses its ceiling, so
                // say so rather than silently seeking past the end of a file.
                tracing::warn!(%err, "sync-play: run-time lookup failed — Seek will not clamp");
            }
        }
        out
    }

    /// The registry guard; see [`lock_registry`] for poison recovery.
    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        lock_registry(&self.registry)
    }

    /// Delivers the plan from [`leave_locked_with_notices`] (after unlocking):
    /// the state hook's broadcast, `GroupLeft` to the leaver, `UserLeft` to the
    /// rest — the order `Group.SessionLeave` sends them in.
    fn deliver_leave(&self, session_id: &str, notices: LeaveNotices) {
        // The state hook runs (and broadcasts) BEFORE the member is removed,
        // so the leaver still receives the group-wide resume it triggered.
        self.deliver(&notices.targets, session_id, &notices.outbound);
        self.bus.send(session_id, notices.left_env);
        for sid in notices.others {
            self.bus.send(&sid, notices.user_left_env.clone());
        }
    }

    /// Delivers `outbound` to the resolved target sessions. Called after the
    /// registry lock is released (delivery is a non-blocking sink invocation).
    fn deliver(&self, members: &[(String, bool)], from_session: &str, outbound: &[Outbound]) {
        deliver_to(self.bus.as_ref(), members, from_session, outbound);
    }

    /// Starts the group-wait sweep unless it is running or disabled. Called
    /// under the registry lock when a group is created; the task stops on its
    /// own once no group is left. Outside a Tokio runtime nothing is armed.
    fn arm_group_wait_sweep(&self, reg: &mut Registry) {
        let Some(every) = self.group_wait_sweep else {
            return;
        };
        if reg.sweeping {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(
            run_group_wait_sweep(Arc::downgrade(&self.registry), self.bus.clone(), every)
                .instrument(tracing::info_span!(
                    parent: None,
                    "sync_play_group_wait_sweep",
                    task = SWEEP_TASK,
                    trigger = "schedule"
                )),
        );
        reg.sweeping = true;
    }
}

/// The registry guard. A panic while it was held poisons the mutex, and
/// panicking on every later call would turn one bad request into a
/// server-wide SyncPlay outage until restart. So recover instead: this trades
/// a possibly half-applied mutation for keeping SyncPlay up, and the poison is
/// cleared so the error is logged once per panic, not per call.
fn lock_registry(registry: &Mutex<Registry>) -> std::sync::MutexGuard<'_, Registry> {
    registry.lock().unwrap_or_else(|poisoned| {
        tracing::error!("sync-play registry lock was poisoned by a panic; recovering");
        registry.clear_poison();
        poisoned.into_inner()
    })
}

/// Delivers `outbound` to the resolved target sessions. Called after the
/// registry lock is released (delivery is a non-blocking sink invocation).
fn deliver_to(
    bus: &dyn SessionMessageBus,
    members: &[(String, bool)],
    from_session: &str,
    outbound: &[Outbound],
) {
    for msg in outbound {
        for (session_id, is_buffering) in members {
            let deliver = match msg.target {
                Target::AllGroup => true,
                Target::CurrentSession => session_id == from_session,
                Target::AllExceptCurrentSession => session_id != from_session,
                Target::AllReady => !is_buffering,
            };
            if deliver {
                bus.send(session_id, msg.body.clone());
            }
        }
    }
}

/// What one group-wait sweep has to deliver: each recovered group's member
/// targets, the session acting for it, and its messages.
type GroupWaitDeliveries = Vec<(Vec<(String, bool)>, String, Vec<Outbound>)>;

/// One pass of `SyncPlayManager.OnGroupWaitTimerTick` (12.1): every group
/// whose wait has run out carries on without the members it was waiting for.
/// `None`, with the sweep marked stopped, once no group is left.
fn sweep_group_waits(
    registry: &Mutex<Registry>,
    now: Instant,
    now_utc: DateTime<Utc>,
) -> Option<GroupWaitDeliveries> {
    let mut reg = lock_registry(registry);
    if reg.groups.is_empty() {
        reg.sweeping = false;
        return None;
    }
    Some(
        reg.groups
            .values_mut()
            .filter_map(|group| {
                let (from, outbound) = group.handle_group_wait_timeout(now, now_utc)?;
                Some((member_targets(group), from, outbound))
            })
            .collect(),
    )
}

/// Marks the sweep stopped if its task unwinds, so the next new group starts
/// it again, and makes the panic visible (LOGGING.md: panics must be visible).
struct SweepPanicGuard(Weak<Mutex<Registry>>);

impl Drop for SweepPanicGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            tracing::error!(
                task = SWEEP_TASK,
                "sync-play group-wait sweep panicked; the next new group restarts it"
            );
            if let Some(registry) = self.0.upgrade() {
                lock_registry(&registry).sweeping = false;
            }
        }
    }
}

/// The group-wait sweep task (`SyncPlayManager._groupWaitTimer`, 12.1). It
/// holds the registry weakly, so it ends with the manager, and it delivers
/// after releasing the lock, as the request paths do.
async fn run_group_wait_sweep(
    registry: Weak<Mutex<Registry>>,
    bus: Arc<dyn SessionMessageBus>,
    every: StdDuration,
) {
    let _panic_guard = SweepPanicGuard(registry.clone());
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick completes at once; the first sweep is one interval in.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let Some(registry) = registry.upgrade() else {
            return;
        };
        let Some(deliveries) = sweep_group_waits(&registry, Instant::now(), Utc::now()) else {
            return;
        };
        drop(registry);
        for (targets, from, outbound) in deliveries {
            deliver_to(bus.as_ref(), &targets, &from, &outbound);
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
        let (info, joined_env, targets, outbound, left) = {
            let mut reg = self.lock();
            // A session may be in at most one group — leave the old one first,
            // with the full `LeaveGroup` (state hook + GroupLeft/UserLeft).
            let left = leave_locked_with_notices(&mut reg, session, now);
            let group_id = Uuid::new_v4();
            let mut group = Group::new(group_id, group_name.trim().to_owned(), now);
            group.wait_timeout = self.group_wait_timeout;
            group.add_member(session);
            let joined = render_update(GroupUpdate::GroupJoined(GroupJoinedUpdate {
                group_id,
                data: group.info(now),
            }));
            // `Group.CreateGroup` ends with the state hook — on a brand-new
            // (Idle) group that is a Stop command to the creating session.
            let outbound = group.session_joined(&session.session_id, now);
            // The HTTP response is a SECOND, LATER snapshot: `SyncPlayManager.NewGroup`
            // returns `group.GetInfo()` AFTER `CreateGroup` has run the state hook
            // (v10.11.8:Emby.Server.Implementations/SyncPlay/SyncPlayManager.cs:134-135),
            // where the envelope above is built before it (Group.cs:277-279). The two
            // agree today only because the Idle hook changes no state; taking the
            // snapshot in upstream's order means they keep agreeing when a
            // state-changing hook lands on create.
            let info = group.info(now);
            let targets = member_targets(&group);
            reg.groups.insert(group_id, group);
            reg.session_to_group
                .insert(session.session_id.clone(), group_id);
            *reg.active_users.entry(session.user_id).or_insert(0) += 1;
            self.arm_group_wait_sweep(&mut reg);
            (info, joined, targets, outbound, left)
        };
        if let Some(n) = left {
            self.deliver_leave(&session.session_id, n);
        }
        self.bus.send(&session.session_id, joined_env);
        self.deliver(&targets, &session.session_id, &outbound);
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
        let targets: Vec<(String, bool)>;
        let outbound: Vec<Outbound>;
        let left: Option<LeaveNotices>;
        // The joiner must be able to see what the group is already playing (C#
        // `JoinGroup` -> `HasAccessToPlayQueue`). Read the queue under the lock,
        // then check without holding it.
        let checked_queue = self
            .lock()
            .groups
            .get(&group_id)
            .map(|g| g.queue.item_ids());
        if let Some(queue) = &checked_queue
            && !self.has_access_to_queue(session.user_id, queue).await
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
                let env = render_update(GroupUpdate::GroupDoesNotExist(GroupDoesNotExistUpdate {
                    group_id,
                    data: group_id.to_string(),
                }));
                drop(reg);
                self.bus.send(&session.session_id, env);
                return Ok(());
            }
            // The access check above ran off-lock; if the queue moved on since,
            // it was never checked against what the group is playing now.
            // Refusing here (rather than joining) keeps the gate closed.
            if reg
                .groups
                .get(&group_id)
                .is_some_and(|g| Some(g.queue.item_ids()) != checked_queue)
            {
                drop(reg);
                tracing::warn!(
                    session_id = %session.session_id,
                    %group_id,
                    "sync-play join refused: the queue changed during the access check"
                );
                let env = render_update(GroupUpdate::LibraryAccessDenied(
                    LibraryAccessDeniedUpdate {
                        group_id,
                        data: String::new(),
                    },
                ));
                self.bus.send(&session.session_id, env);
                return Ok(());
            }
            // `JoinGroup`'s "Restore session" branch: a join to the group the
            // session is already in re-runs `SessionJoin` without leaving it
            // (leaving would drop a sole member's group mid-join) and without
            // counting the session a second time.
            let restore = reg.session_to_group.get(&session.session_id) == Some(&group_id);
            left = if restore {
                None
            } else {
                leave_locked_with_notices(&mut reg, session, now)
            };
            let user_name = session.user_name.clone();
            let Some(group) = reg.groups.get_mut(&group_id) else {
                // Unreachable: leaving another group cannot remove this one.
                return Err(ServiceError::backend(
                    "sync-play group vanished during join",
                ));
            };
            group.add_member(session);
            let info = group.info(now);
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
            // `Group.SessionJoin` ends with the state hook, after both envelopes.
            outbound = group.session_joined(&session.session_id, now);
            targets = member_targets(group);
            if !restore {
                reg.session_to_group
                    .insert(session.session_id.clone(), group_id);
                *reg.active_users.entry(session.user_id).or_insert(0) += 1;
            }
        }
        if let Some(n) = left {
            self.deliver_leave(&session.session_id, n);
        }
        self.bus.send(&session.session_id, joined_env);
        for sid in others {
            self.bus.send(&sid, user_joined_env.clone());
        }
        self.deliver(&targets, &session.session_id, &outbound);
        Ok(())
    }

    async fn leave_group(&self, session: &SyncPlaySession) -> Result<(), ServiceError> {
        let now = Utc::now();
        let notices = {
            let mut reg = self.lock();
            leave_locked_with_notices(&mut reg, session, now)
        };
        if let Some(n) = notices {
            self.deliver_leave(&session.session_id, n);
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
        let now = Utc::now();
        let mut candidates: Vec<(GroupInfoDto, Vec<Uuid>)> = {
            let reg = self.lock();
            reg.groups
                .values()
                .map(|g| (g.info(now), g.queue.item_ids()))
                .collect()
        };
        candidates.sort_by_key(|(info, _)| info.group_id);
        // One batched evaluation of the union of every group's queue —
        // this route is polled by SyncPlay clients, so a per-group round trip
        // is a latency cliff as the server fills with groups.
        let union: Vec<Uuid> = {
            let mut seen = std::collections::HashSet::new();
            candidates
                .iter()
                .flat_map(|(_, queue)| queue.iter().copied())
                .filter(|id| seen.insert(*id))
                .collect()
        };
        let visible = self.visible_subset(session.user_id, &union).await;
        Ok(candidates
            .into_iter()
            .filter(|(_, queue)| {
                visible
                    .as_ref()
                    .is_none_or(|set| queue.iter().all(|id| set.contains(id)))
            })
            .map(|(info, _)| info)
            .collect())
    }

    async fn get_group(
        &self,
        session: &SyncPlaySession,
        group_id: Uuid,
    ) -> Result<GroupInfoDto, ServiceError> {
        let now = Utc::now();
        let found = {
            let reg = self.lock();
            reg.groups
                .get(&group_id)
                .map(|g| (g.info(now), g.queue.item_ids()))
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
        // The set of members the access check below was made against. The check
        // itself hits the database, so it cannot run under the registry lock;
        // the mutation re-verifies this snapshot while holding that lock, which
        // is what stops a member who joined in between from being skipped.
        let mut checked_members: Option<Vec<Uuid>> = None;
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
                // Upstream logs and returns to the previous state without
                // broadcasting (`WaitingGroupState.HandleRequest` on a failed
                // `SetPlayQueue`), so the refusal is only visible in the log.
                tracing::warn!(
                    session_id = %session.session_id,
                    items = incoming.len(),
                    "sync-play queue change refused: a member cannot access the items"
                );
                return Ok(());
            }
            checked_members = Some(members);
        }
        // `Group.RunTimeTicks` is what `SanitizePositionTicks` clamps a Seek or a
        // Ready position against, and upstream refreshes it from the library
        // every time the playing item changes. Resolved here, off the registry
        // lock, for the items this request is about to put in the queue.
        let runtimes = self.item_run_times(incoming).await;
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
            let Some(group) = reg.groups.get_mut(&group_id) else {
                // Unreachable: a group is only dropped once it has no members.
                return Err(ServiceError::backend("sync-play mapped group missing"));
            };
            // Closes the window between the off-lock access check and this
            // mutation: if the membership changed, the items were not checked
            // against whoever is in the group *now*, so refuse rather than
            // apply a queue nobody vouched for.
            if let Some(checked) = &checked_members
                && group.member_user_ids() != *checked
            {
                tracing::warn!(
                    session_id = %session.session_id,
                    "sync-play queue change refused: membership changed during the access check"
                );
                return Ok(());
            }
            // Member-scoped requests mutate the requesting member, not the group.
            match &request {
                PlaybackRequest::Ping { ping } => {
                    if let Some(m) = group.member_mut(&session.session_id) {
                        m.ping_ms = (*ping).clamp(0, MAX_PING_MS);
                    }
                }
                PlaybackRequest::IgnoreWait { ignore_wait } => {
                    if let Some(m) = group.member_mut(&session.session_id) {
                        m.ignore_wait = *ignore_wait;
                    }
                    // `Group.SetIgnoreGroupWait` (12.1).
                    group.update_group_wait_deadline(false);
                }
                _ => {}
            }
            // Buffer/Ready do NOT set the member's flag here: upstream sets it
            // inside the per-state arms, and the states that answer "you are
            // lost, here is where the group is" (every Idle arm, and the
            // wrong-playlist-item early returns) deliberately do not touch it.
            group
                .item_runtimes
                .extend(runtimes.iter().map(|(k, v)| (*k, *v)));
            let outbound = group.handle(&session.session_id, &request, now);
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

/// The broadcast plan a leave produces: whatever the group-state `SessionLeaving`
/// hook emitted (addressed against the membership as it was BEFORE the removal),
/// then `GroupLeft` to the leaver and `UserLeft` to everyone else.
struct LeaveNotices {
    outbound: Vec<Outbound>,
    targets: Vec<(String, bool)>,
    left_env: String,
    user_left_env: String,
    others: Vec<String>,
}

/// Removes a session from any group it is in (registry pre-locked): unmaps it
/// and decrements the user counter, then runs the state hook and drops the
/// group if it becomes empty. Returns the broadcast plan to deliver after
/// unlocking, or `None` if the session was not mapped to a live group.
fn leave_locked_with_notices(
    reg: &mut Registry,
    session: &SyncPlaySession,
    now: DateTime<Utc>,
) -> Option<LeaveNotices> {
    let group_id = reg.session_to_group.remove(&session.session_id)?;
    // Uncounted with the unmapping, before the group is touched — upstream's
    // `TryRemove` then `UpdateSessionsCounter(-1)` order — so even a stale
    // mapping cannot leave the user counted as active.
    decrement_user(reg, session.user_id);
    let group = reg.groups.get_mut(&group_id)?;
    // `Group.SessionLeave` runs the state hook first, while the leaver is still
    // a participant — so the targets it broadcasts to include it.
    let outbound = group.session_leaving(&session.session_id, now);
    let targets = member_targets(group);
    group.remove_member(&session.session_id);
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
    Some(LeaveNotices {
        outbound,
        targets,
        left_env,
        user_left_env,
        others,
    })
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
        fn bodies_for(&self, session_id: &str) -> Vec<String> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .filter(|(s, _)| s == session_id)
                .map(|(_, b)| b.clone())
                .collect()
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

    /// The `PlaylistItemId` the group minted for its current item, read off the
    /// last `PlayQueue` update — exactly where a real client gets it, and the
    /// value every Buffer/Ready/NextItem request has to echo back.
    fn playing_item_id(bus: &RecordingBus) -> Uuid {
        let body = bus
            .bodies()
            .into_iter()
            .rev()
            .find(|b| b.contains("\"Type\":\"PlayQueue\""))
            .expect("a PlayQueue update was pushed");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let data = &v["Data"]["Data"];
        let idx = usize::try_from(data["PlayingItemIndex"].as_i64().unwrap()).unwrap();
        Uuid::parse_str(data["Playlist"][idx]["PlaylistItemId"].as_str().unwrap()).unwrap()
    }

    /// Every `PlaylistItemId` in the last `PlayQueue` update, in queue order.
    fn last_playlist(bus: &RecordingBus) -> Vec<Uuid> {
        let body = bus
            .bodies()
            .into_iter()
            .rev()
            .find(|b| b.contains("\"Type\":\"PlayQueue\""))
            .expect("a PlayQueue update was pushed");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        v["Data"]["Data"]["Playlist"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| Uuid::parse_str(i["PlaylistItemId"].as_str().unwrap()).unwrap())
            .collect()
    }

    fn ready_at(pli: Uuid, position_ticks: i64, is_playing: bool) -> PlaybackRequest {
        PlaybackRequest::Ready {
            when: Utc::now(),
            position_ticks,
            is_playing,
            playlist_item_id: pli,
        }
    }

    fn buffer_at(pli: Uuid, position_ticks: i64) -> PlaybackRequest {
        PlaybackRequest::Buffer {
            when: Utc::now(),
            position_ticks,
            is_playing: false,
            playlist_item_id: pli,
        }
    }

    /// Drives a group all the way to `Playing` the way a client does: a new
    /// queue puts it in `Waiting` and only a Ready from EVERY member starts it.
    /// Returns the current `PlaylistItemId`.
    async fn start_playing(
        m: &FerrofinSyncPlayManager,
        bus: &RecordingBus,
        members: &[&SyncPlaySession],
        items: usize,
    ) -> Uuid {
        m.handle_request(members[0], play(items)).await.unwrap();
        let pli = playing_item_id(bus);
        for s in members {
            m.handle_request(s, ready_at(pli, 0, true)).await.unwrap();
        }
        pli
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
        // `Group.CreateGroup`: GroupJoined, then the Idle state's Stop command.
        assert_eq!(bus.count_for("s1"), 2);

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
        // alice: GroupJoined + her own Stop, then UserJoined(bob).
        assert_eq!(bus.count_for("s1"), 3);
        // bob: GroupJoined + the Idle state's Stop.
        assert_eq!(bus.count_for("s2"), 2);
    }

    /// `Group.CreateGroup` / `Group.SessionJoin` both end with
    /// `_state.SessionJoined(...)`, which on an Idle group is a Stop command
    /// addressed to the joining session only. Ferrofin used to push nothing.
    #[tokio::test]
    async fn joining_an_idle_group_pushes_group_joined_then_stop() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();

        let creator = bus.bodies_for("s1");
        assert_eq!(creator.len(), 2, "{creator:?}");
        assert!(creator[0].contains("GroupJoined"));
        assert!(creator[1].contains("SyncPlayCommand") && creator[1].contains("\"Stop\""));
        // The all-zero playlist item id Jellyfin sends for an empty queue.
        assert!(creator[1].contains("\"PlaylistItemId\":\"00000000000000000000000000000000\""));

        let b = session("s2", "bob");
        m.join_group(&b, info.group_id).await.unwrap();
        let joiner = bus.bodies_for("s2");
        assert_eq!(joiner.len(), 2, "{joiner:?}");
        assert!(joiner[0].contains("GroupJoined"));
        assert!(joiner[1].contains("SyncPlayCommand") && joiner[1].contains("\"Stop\""));
        // The Stop is CurrentSession-scoped: alice only saw UserJoined.
        let alice_after = &bus.bodies_for("s1")[2..];
        assert_eq!(alice_after.len(), 1);
        assert!(alice_after[0].contains("UserJoined"));
    }

    /// `PlayingGroupState.SessionJoined` -> `WaitingGroupState.SessionJoined`:
    /// the joiner gets the play queue, the group drops to `Waiting`, and the
    /// members that are ready are told to pause.
    #[tokio::test]
    async fn joining_a_playing_group_pushes_the_queue_and_pauses_the_ready() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        start_playing(&m, &bus, &[&a], 2).await;
        let before = bus.count_for("s1");

        let b = session("s2", "bob");
        m.join_group(&b, info.group_id).await.unwrap();

        let joiner = bus.bodies_for("s2");
        assert_eq!(joiner.len(), 2, "{joiner:?}");
        assert!(joiner[0].contains("GroupJoined"));
        assert!(joiner[1].contains("PlayQueue") && joiner[1].contains("NewPlaylist"));
        // alice is ready, so she is the one told to pause (after UserJoined).
        let alice_after = &bus.bodies_for("s1")[before..];
        assert_eq!(alice_after.len(), 2, "{alice_after:?}");
        assert!(alice_after[0].contains("UserJoined"));
        assert!(alice_after[1].contains("SyncPlayCommand") && alice_after[1].contains("Pause"));
        assert_eq!(
            m.get_group(&a, info.group_id).await.unwrap().state,
            GroupStateType::Waiting
        );
    }

    /// `WaitingGroupState.SessionLeaving`: the buffering member the group was
    /// waiting on leaves, so playback resumes for everyone still in it.
    #[tokio::test]
    async fn a_buffering_member_leaving_resumes_the_group() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        start_playing(&m, &bus, &[&a], 2).await;
        let b = session("s2", "bob");
        m.join_group(&b, info.group_id).await.unwrap(); // -> Waiting, bob buffering
        let before = bus.count_for("s1");

        m.leave_group(&b).await.unwrap();

        let alice_after = &bus.bodies_for("s1")[before..];
        assert!(
            alice_after
                .iter()
                .any(|x| x.contains("SyncPlayCommand") && x.contains("Unpause")),
            "{alice_after:?}"
        );
        assert!(alice_after.iter().any(|x| x.contains("UserLeft")));
        assert_eq!(
            m.get_group(&a, info.group_id).await.unwrap().state,
            GroupStateType::Playing
        );
    }

    /// `WaitingGroupState.HandleRequest(BufferGroupRequest)` has THREE arms, and
    /// the third one is the trap: `prevState == Waiting` must LEAVE
    /// `ResumePlaying` alone (WaitingGroupState.cs:345-388 has no `else`).
    ///
    /// The group is Playing; bob joins, which drops it to Waiting with
    /// `ResumePlaying = true` and bob buffering. Bob then reports Buffering
    /// AGAIN — arriving from Waiting this time — and leaves. Nobody is buffering
    /// any more, so `SessionLeaving` resolves the group: with the flag intact it
    /// resumes Playing, and a two-arm `flag = prev == Playing` would have cleared
    /// it and settled the group into Paused instead.
    #[tokio::test]
    async fn buffering_from_waiting_keeps_resume_playing_armed() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        let pli = start_playing(&m, &bus, &[&a], 2).await;
        let b = session("s2", "bob");
        m.join_group(&b, info.group_id).await.unwrap(); // Playing -> Waiting, resume armed
        assert_eq!(
            m.get_group(&a, info.group_id).await.unwrap().state,
            GroupStateType::Waiting
        );

        // A second Buffer, this time with the group ALREADY Waiting.
        m.handle_request(&b, buffer_at(pli, 0)).await.unwrap();
        let before = bus.count_for("s1");

        m.leave_group(&b).await.unwrap();

        assert_eq!(
            m.get_group(&a, info.group_id).await.unwrap().state,
            GroupStateType::Playing,
            "a Buffer arriving from Waiting must not clear ResumePlaying"
        );
        let alice_after = &bus.bodies_for("s1")[before..];
        assert!(
            alice_after
                .iter()
                .any(|x| x.contains("SyncPlayCommand") && x.contains("Unpause")),
            "{alice_after:?}"
        );
    }

    /// `Group.GetInfo()` stamps `DateTime.UtcNow` at DTO construction, so the
    /// value advances on every read; Ferrofin used to freeze it at the last
    /// mutation. `Participants` is LINQ `Distinct()` — first-join order.
    #[tokio::test]
    async fn group_info_is_stamped_per_read_and_keeps_join_order() {
        let (m, _bus) = mgr();
        let z = session("s1", "zoe");
        let info = m.new_group(&z, "g").await.unwrap();
        let adam = session("s2", "adam");
        m.join_group(&adam, info.group_id).await.unwrap();

        let first = m.get_group(&z, info.group_id).await.unwrap();
        assert_eq!(
            first.participants,
            vec!["zoe".to_string(), "adam".to_string()]
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let second = m.get_group(&z, info.group_id).await.unwrap();
        assert!(
            second.last_updated_at > first.last_updated_at,
            "LastUpdatedAt must advance between reads: {} !> {}",
            second.last_updated_at,
            first.last_updated_at
        );
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

    /// `WaitingGroupState.HandleRequest(PlayGroupRequest)`
    /// (WaitingGroupState.cs:127-162): a new queue does NOT start playback. The
    /// group broadcasts the queue, resets every member to buffering and WAITS —
    /// only a Ready from each of them starts it. Ferrofin used to go straight to
    /// `Playing` and push an `Unpause` nobody upstream sends.
    #[tokio::test]
    async fn set_new_queue_waits_for_every_member_before_playing() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        let b = session("s2", "bob");
        m.join_group(&b, info.group_id).await.unwrap();
        let before = bus.count_for("s2");

        m.handle_request(&a, play(2)).await.unwrap();

        assert_eq!(
            m.get_group(&a, info.group_id).await.unwrap().state,
            GroupStateType::Waiting
        );
        let pushed = &bus.bodies_for("s2")[before..];
        assert_eq!(pushed.len(), 1, "{pushed:?}");
        assert!(pushed[0].contains("PlayQueue") && pushed[0].contains("NewPlaylist"));
        assert!(
            !pushed[0].contains("\"IsPlaying\":true"),
            "a waiting group is not playing: {pushed:?}"
        );

        // ...and the group starts only once BOTH members report ready.
        let pli = playing_item_id(&bus);
        m.handle_request(&a, ready_at(pli, 0, true)).await.unwrap();
        assert_eq!(
            m.get_group(&a, info.group_id).await.unwrap().state,
            GroupStateType::Waiting,
            "one of two members ready must not start the group"
        );
        m.handle_request(&b, ready_at(pli, 0, true)).await.unwrap();
        assert_eq!(
            m.get_group(&a, info.group_id).await.unwrap().state,
            GroupStateType::Playing
        );
        assert!(
            bus.bodies_for("s2")
                .iter()
                .any(|x| x.contains("SyncPlayCommand") && x.contains("Unpause"))
        );
    }

    /// A two-session group whose members can be told apart on the bus, which is
    /// the ONLY way `CurrentSession` and `AllGroup` are distinguishable: with a
    /// single member every broadcast target looks identical, and that is how the
    /// per-state arms below stayed wrong for so long.
    async fn pair() -> (
        FerrofinSyncPlayManager,
        Arc<RecordingBus>,
        SyncPlaySession,
        SyncPlaySession,
        Uuid,
    ) {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let b = session("s2", "bob");
        let info = m.new_group(&a, "g").await.unwrap();
        m.join_group(&b, info.group_id).await.unwrap();
        (m, bus, a, b, info.group_id)
    }

    /// What each session was pushed since `mark`, as `(caller, peer)`.
    fn since(bus: &RecordingBus, mark: (usize, usize)) -> (Vec<String>, Vec<String>) {
        (
            bus.bodies_for("s1")[mark.0..].to_vec(),
            bus.bodies_for("s2")[mark.1..].to_vec(),
        )
    }

    fn mark(bus: &RecordingBus) -> (usize, usize) {
        (bus.count_for("s1"), bus.count_for("s2"))
    }

    fn is_cmd(body: &str, command: &str) -> bool {
        body.contains("\"MessageType\":\"SyncPlayCommand\"")
            && body.contains(&format!("\"Command\":\"{command}\""))
    }

    fn is_state(body: &str, state: &str, reason: &str) -> bool {
        body.contains("\"Type\":\"StateUpdate\"")
            && body.contains(&format!("\"State\":\"{state}\""))
            && body.contains(&format!("\"Reason\":\"{reason}\""))
    }

    /// `POST /SyncPlay/Pause`, all four arms.
    ///
    /// * Idle — `IdleGroupState.cs:67-70` -> `SendStopCommand` with
    ///   `prevState == Type`: ONE Stop, to the CALLER only. Ferrofin used to send
    ///   nothing at all, and had a unit test asserting that as intended.
    /// * Playing — `PausedGroupState.cs:66-87`: AllGroup Pause + StateUpdate.
    /// * Paused — `PausedGroupState.cs:88-93`, "client got lost": caller only,
    ///   no state update.
    /// * Waiting — `WaitingGroupState.cs:255-269`: stays Waiting, disarms the
    ///   resume, StateUpdate only.
    #[tokio::test]
    async fn pause_has_one_arm_per_state() {
        let (m, bus, a, b, gid) = pair().await;

        // -- Idle
        let at = mark(&bus);
        m.handle_request(&a, PlaybackRequest::Pause).await.unwrap();
        let (caller, peer) = since(&bus, at);
        assert_eq!(caller.len(), 1, "{caller:?}");
        assert!(is_cmd(&caller[0], "Stop"));
        assert!(peer.is_empty(), "the idle Stop is caller-only: {peer:?}");
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Idle
        );

        // -- Playing
        start_playing(&m, &bus, &[&a, &b], 1).await;
        let at = mark(&bus);
        m.handle_request(&a, PlaybackRequest::Pause).await.unwrap();
        let (caller, peer) = since(&bus, at);
        for got in [&caller, &peer] {
            assert_eq!(got.len(), 2, "{got:?}");
            assert!(is_cmd(&got[0], "Pause"));
            assert!(is_state(&got[1], "Paused", "Pause"));
        }
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Paused
        );

        // -- Paused: the caller alone is re-told, with no state update.
        let at = mark(&bus);
        m.handle_request(&a, PlaybackRequest::Pause).await.unwrap();
        let (caller, peer) = since(&bus, at);
        assert_eq!(caller.len(), 1, "{caller:?}");
        assert!(is_cmd(&caller[0], "Pause"));
        assert!(peer.is_empty(), "{peer:?}");

        // -- Waiting: a Seek puts the group back into Waiting.
        m.handle_request(
            &a,
            PlaybackRequest::Seek {
                position_ticks: 1_000,
            },
        )
        .await
        .unwrap();
        let at = mark(&bus);
        m.handle_request(&a, PlaybackRequest::Pause).await.unwrap();
        let (caller, peer) = since(&bus, at);
        for got in [&caller, &peer] {
            assert_eq!(got.len(), 1, "{got:?}");
            assert!(is_state(&got[0], "Waiting", "Pause"), "{got:?}");
        }
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Waiting,
            "a Pause while waiting must not drop the group to Paused"
        );
    }

    /// `POST /SyncPlay/Unpause`, all four arms.
    ///
    /// * Idle — `IdleGroupState.cs:57-63` -> `WaitingGroupState.cs:212-225`: the
    ///   item restarts and the group WAITS; one AllGroup PlayQueue, no command.
    /// * Playing — `PlayingGroupState.cs:81-86`: caller-only resync, and the
    ///   group's `LastActivity` is NOT moved.
    /// * Paused — AllGroup Unpause + StateUpdate.
    /// * Waiting with the resume armed — forces playback and turns group-wait
    ///   off (`IgnoreBuffering`), so the next Buffer is dropped.
    #[tokio::test]
    async fn unpause_has_one_arm_per_state() {
        let (m, bus, a, b, gid) = pair().await;

        // -- Idle
        let at = mark(&bus);
        m.handle_request(&a, PlaybackRequest::Unpause)
            .await
            .unwrap();
        let (caller, peer) = since(&bus, at);
        for got in [&caller, &peer] {
            assert_eq!(got.len(), 1, "{got:?}");
            assert!(got[0].contains("PlayQueue") && got[0].contains("NewPlaylist"));
        }
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Waiting
        );

        // -- Playing, reached the way a client does.
        m.handle_request(&a, PlaybackRequest::Stop).await.unwrap();
        start_playing(&m, &bus, &[&a, &b], 1).await;
        let at = mark(&bus);
        m.handle_request(&a, PlaybackRequest::Unpause)
            .await
            .unwrap();
        let (caller, peer) = since(&bus, at);
        assert_eq!(caller.len(), 1, "{caller:?}");
        assert!(is_cmd(&caller[0], "Unpause"));
        assert!(
            peer.is_empty(),
            "resyncing one client must not move the group"
        );

        // -- Paused
        m.handle_request(&a, PlaybackRequest::Pause).await.unwrap();
        let at = mark(&bus);
        m.handle_request(&a, PlaybackRequest::Unpause)
            .await
            .unwrap();
        let (caller, peer) = since(&bus, at);
        for got in [&caller, &peer] {
            assert_eq!(got.len(), 2, "{got:?}");
            assert!(is_cmd(&got[0], "Unpause"));
            assert!(is_state(&got[1], "Playing", "Unpause"));
        }

        // -- Waiting, resume armed: bob joining drops the group to Waiting.
        let c = session("s3", "carol");
        m.join_group(&c, gid).await.unwrap();
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Waiting
        );
        let at = mark(&bus);
        m.handle_request(&a, PlaybackRequest::Unpause)
            .await
            .unwrap();
        let (caller, _) = since(&bus, at);
        assert_eq!(caller.len(), 2, "{caller:?}");
        assert!(is_cmd(&caller[0], "Unpause") && is_state(&caller[1], "Playing", "Unpause"));
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Playing
        );
        // ...and group-wait is now off: carol's Buffer is dropped.
        let pli = playing_item_id(&bus);
        let at = mark(&bus);
        m.handle_request(&c, buffer_at(pli, 0)).await.unwrap();
        let (caller, _) = since(&bus, at);
        assert!(
            caller.is_empty(),
            "IgnoreBuffering must drop it: {caller:?}"
        );
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Playing
        );
    }

    /// `POST /SyncPlay/Stop`. `IdleGroupState.SendStopCommand` broadcasts when
    /// the group was NOT already idle and answers the caller alone when it was;
    /// either way `When` is the group's `LastActivity` (Group.cs:417-426), not
    /// the instant the command was rendered.
    #[tokio::test]
    async fn stop_scope_depends_on_the_previous_state_and_when_is_last_activity() {
        let (m, bus, a, b, gid) = pair().await;
        start_playing(&m, &bus, &[&a, &b], 1).await;

        let at = mark(&bus);
        m.handle_request(&a, PlaybackRequest::Stop).await.unwrap();
        let (caller, peer) = since(&bus, at);
        assert_eq!(caller.len(), 1);
        assert_eq!(peer.len(), 1, "a Stop from Playing reaches the group");
        assert!(is_cmd(&caller[0], "Stop"));
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Idle
        );

        // The command was scheduled at the group's last activity — which is in
        // the PAST here, since the group has been running — not at `now`.
        let v: serde_json::Value = serde_json::from_str(&caller[0]).unwrap();
        let when = v["Data"]["When"].as_str().unwrap();
        let emitted = v["Data"]["EmittedAt"].as_str().unwrap();
        assert_ne!(
            when, emitted,
            "When must be LastActivity, not the emit time"
        );

        // A second Stop, this time from Idle: the caller alone.
        let at = mark(&bus);
        m.handle_request(&a, PlaybackRequest::Stop).await.unwrap();
        let (caller, peer) = since(&bus, at);
        assert_eq!(caller.len(), 1);
        assert!(is_cmd(&caller[0], "Stop"));
        assert!(peer.is_empty(), "an idle Stop is caller-only: {peer:?}");
    }

    /// `POST /SyncPlay/Seek` on an IDLE group: `IdleGroupState.cs:79-83` answers
    /// the caller with a Stop and changes NOTHING — no `Waiting`, no Seek
    /// command, no state update. Ferrofin used to run its Waiting arm here.
    #[tokio::test]
    async fn seek_on_an_idle_group_stops_the_caller_only() {
        let (m, bus, a, _b, gid) = pair().await;
        let at = mark(&bus);

        m.handle_request(
            &a,
            PlaybackRequest::Seek {
                position_ticks: 50_000_000,
            },
        )
        .await
        .unwrap();

        let (caller, peer) = since(&bus, at);
        assert_eq!(caller.len(), 1, "{caller:?}");
        assert!(is_cmd(&caller[0], "Stop"), "{caller:?}");
        assert!(peer.is_empty(), "{peer:?}");
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Idle
        );
    }

    /// `Group.SanitizePositionTicks` — `Math.Clamp(ticks, 0, RunTimeTicks)`
    /// (Group.cs:429-432). Ferrofin had the floor and no ceiling, so a client
    /// could seek the whole group past the end of the file.
    #[test]
    fn seek_clamps_to_the_playing_item_run_time() {
        let now = Utc::now();
        let mut g = Group::new(Uuid::new_v4(), "g".into(), now);
        let item = Uuid::new_v4();
        g.item_runtimes.insert(item, 10_230_000);
        g.queue.set(&[item], 0);
        g.state = GroupStateType::Playing;

        g.handle(
            "s1",
            &PlaybackRequest::Seek {
                position_ticks: 999_999_999,
            },
            now,
        );
        assert_eq!(g.position_ticks, 10_230_000, "clamped to the run time");

        g.handle(
            "s1",
            &PlaybackRequest::Seek {
                position_ticks: -5_000,
            },
            now,
        );
        assert_eq!(g.position_ticks, 0, "and floored at zero");
    }

    /// Ticks in `n` minutes.
    fn minutes(n: i64) -> i64 {
        n * 60_000 * TICKS_PER_MS
    }

    /// The `GroupHarness` of v12.2 `WaitingGroupStateTests`: two members
    /// playing a two-hour item at 10:00, with a second item queued after it.
    /// Returns the group and both items' `PlaylistItemId`s.
    fn playing_pair(now: DateTime<Utc>) -> (Group, Uuid, Uuid) {
        let mut g = Group::new(Uuid::new_v4(), "g".into(), now);
        for (id, name) in [("s1", "first"), ("s2", "second")] {
            g.add_member(&session(id, name));
        }
        let items = [Uuid::new_v4(), Uuid::new_v4()];
        for item in items {
            g.item_runtimes.insert(item, minutes(120));
        }
        g.queue.set(&items, 0);
        let first = g.queue.playing_item_id();
        g.queue.next();
        let second = g.queue.playing_item_id();
        g.queue.set_playing(first);
        g.state = GroupStateType::Playing;
        g.position_ticks = minutes(10);
        g.last_activity = now;
        (g, first, second)
    }

    fn ready_now(
        pli: Uuid,
        position_ticks: i64,
        is_playing: bool,
        now: DateTime<Utc>,
    ) -> PlaybackRequest {
        PlaybackRequest::Ready {
            when: now,
            position_ticks,
            is_playing,
            playlist_item_id: pli,
        }
    }

    fn sent(out: &[Outbound], command: &str) -> bool {
        out.iter().any(|o| is_cmd(&o.body, command))
    }

    fn bodies(out: &[Outbound]) -> Vec<&str> {
        out.iter().map(|o| o.body.as_str()).collect()
    }

    /// v12.2 `Ready_PlayingSessionReportsPositionFromBeforeSeek_IsCorrected`
    /// (PR 17797): after a Seek the group position JUMPED, so a member that
    /// still reports the old position is corrected even though it says it is
    /// playing — it is not accepted and handed a Pause half an hour out.
    #[test]
    fn a_playing_member_behind_a_seek_is_corrected() {
        let now = Utc::now();
        let (mut g, pli, _) = playing_pair(now);
        g.handle(
            "s2",
            &PlaybackRequest::Seek {
                position_ticks: minutes(40),
            },
            now,
        );

        let out = g.handle("s1", &ready_now(pli, minutes(10), true, now), now);

        assert!(sent(&out, "Seek"), "{:?}", bodies(&out));
        assert!(!sent(&out, "Pause"), "{:?}", bodies(&out));
        assert!(g.is_buffering(), "the member still counts as buffering");
    }

    /// v12.2 `Ready_PlayingSessionRecoveringFromALongStall_IsNotSeeked`: with
    /// no jump, a playing member 45 s behind is catching up after buffering
    /// and is waited for — up to `MaxCatchUpOffset` (60 s), past which it is
    /// seeked like any other member out of position.
    #[rstest::rstest]
    #[case::forty_five_seconds_behind(45, false)]
    #[case::past_the_catch_up_offset(61, true)]
    fn a_playing_member_recovering_from_a_stall_is_waited_for_up_to_a_minute(
        #[case] behind_secs: i64,
        #[case] seeked: bool,
    ) {
        let now = Utc::now();
        let (mut g, pli, _) = playing_pair(now);
        g.handle(
            "s1",
            &PlaybackRequest::Buffer {
                when: now,
                position_ticks: g.position_ticks,
                is_playing: true,
                playlist_item_id: pli,
            },
            now,
        );

        let behind = g.position_ticks - behind_secs * 1000 * TICKS_PER_MS;
        let out = g.handle("s1", &ready_now(pli, behind, true, now), now);

        assert_eq!(sent(&out, "Seek"), seeked, "{:?}", bodies(&out));
    }

    /// The jump belongs to the whole wait: upstream handles every request
    /// while `Waiting` on the same `WaitingGroupState`, so another member's
    /// Buffer after the Seek does not give the lagging member its catch-up
    /// tolerance back.
    #[test]
    fn a_jump_lasts_for_the_rest_of_the_wait() {
        let now = Utc::now();
        let (mut g, pli, _) = playing_pair(now);
        g.handle(
            "s2",
            &PlaybackRequest::Seek {
                position_ticks: minutes(40),
            },
            now,
        );
        g.handle(
            "s2",
            &PlaybackRequest::Buffer {
                when: now,
                position_ticks: minutes(40),
                is_playing: false,
                playlist_item_id: pli,
            },
            now,
        );
        assert_eq!(g.state, GroupStateType::Waiting);

        let off = minutes(40) + 30 * 1000 * TICKS_PER_MS;
        let out = g.handle("s1", &ready_now(pli, off, true, now), now);

        assert!(sent(&out, "Seek"), "{:?}", bodies(&out));
    }

    /// v12.2 `Ready_PlayingSessionSlightlyBehindGroup_IsStillTreatedAsCatchingUp`
    /// and `Ready_PausedSessionOutOfPosition_IsStillCorrected`: in a wait with
    /// no jump, a playing member 2 s behind gets the Pause while the other
    /// member buffers, and a paused member out of position is still seeked
    /// (`MaxPlaybackOffset`).
    #[rstest::rstest]
    #[case::playing_two_seconds_behind(minutes(30) - 2000 * TICKS_PER_MS, true, "Pause")]
    #[case::paused_out_of_position(0, false, "Seek")]
    fn a_wait_without_a_jump_tolerates_only_a_playing_member(
        #[case] position_ticks: i64,
        #[case] is_playing: bool,
        #[case] expected: &str,
    ) {
        let now = Utc::now();
        let (mut g, pli, _) = playing_pair(now);
        g.position_ticks = minutes(30);
        g.state = GroupStateType::Waiting;
        g.resume_playing = true;
        g.set_buffering("s1", true);
        g.set_buffering("s2", true);

        let out = g.handle("s1", &ready_now(pli, position_ticks, is_playing, now), now);

        assert!(sent(&out, expected), "{:?}", bodies(&out));
        assert_eq!(sent(&out, "Seek"), expected == "Seek", "{:?}", bodies(&out));
    }

    /// Every request that moves the group position marks the wait as jumped
    /// (`PositionJumped`, WaitingGroupState.cs:144, :184, :224, :321, :598,
    /// :645). A playing member half a minute off the new position has not
    /// applied the jump, so it is seeked; without the jump it would be within
    /// the catch-up tolerance. (Off by being AHEAD: a restart puts the group at
    /// 0, and a position before 0 is clamped to it.)
    #[rstest::rstest]
    #[case::seek("seek")]
    #[case::set_playlist_item("set_playlist_item")]
    #[case::next_item("next_item")]
    #[case::previous_item("previous_item")]
    #[case::play("play")]
    #[case::unpause_from_idle("unpause_from_idle")]
    fn every_position_jump_ends_the_catch_up_tolerance(#[case] jump: &str) {
        let now = Utc::now();
        let (mut g, first, second) = playing_pair(now);
        let request = match jump {
            "seek" => PlaybackRequest::Seek {
                position_ticks: minutes(40),
            },
            "set_playlist_item" => PlaybackRequest::SetPlaylistItem {
                playlist_item_id: second,
            },
            "next_item" => PlaybackRequest::NextItem {
                playlist_item_id: first,
            },
            "previous_item" => {
                g.queue.set_playing(second);
                PlaybackRequest::PreviousItem {
                    playlist_item_id: second,
                }
            }
            "play" => PlaybackRequest::Play {
                playing_queue: g.queue.item_ids(),
                playing_item_position: 0,
                start_position_ticks: minutes(40),
            },
            _ => {
                g.state = GroupStateType::Idle;
                PlaybackRequest::Unpause
            }
        };
        g.handle("s2", &request, now);
        let pli = g.queue.playing_item_id();

        let off = g.position_ticks + 30 * 1000 * TICKS_PER_MS;
        let out = g.handle("s1", &ready_now(pli, off, true, now), now);

        assert!(sent(&out, "Seek"), "{jump}: {:?}", bodies(&out));
    }

    /// Upstream builds a fresh `WaitingGroupState` on every entry into
    /// `Waiting`, so a jump in one wait does not take the catch-up tolerance
    /// away from the next wait, which only a member's buffering started.
    #[test]
    fn a_new_wait_starts_without_a_jump() {
        let now = Utc::now();
        let (mut g, pli, _) = playing_pair(now);
        g.handle(
            "s2",
            &PlaybackRequest::Seek {
                position_ticks: minutes(40),
            },
            now,
        );
        for member in ["s1", "s2"] {
            g.handle(member, &ready_now(pli, minutes(40), true, now), now);
        }
        assert_eq!(g.state, GroupStateType::Playing);

        g.handle(
            "s1",
            &PlaybackRequest::Buffer {
                when: now,
                position_ticks: g.position_ticks,
                is_playing: true,
                playlist_item_id: pli,
            },
            now,
        );
        let behind = g.position_ticks - 45 * 1000 * TICKS_PER_MS;
        let out = g.handle("s1", &ready_now(pli, behind, true, now), now);

        assert!(!sent(&out, "Seek"), "{:?}", bodies(&out));
    }

    /// `POST /SyncPlay/Buffering`, per state.
    ///
    /// * Idle — `IdleGroupState.cs:85-89`: a Stop to the caller, the group stays
    ///   Idle and the member is NOT marked buffering.
    /// * Playing — `WaitingGroupState.cs:346-368`: the Pause goes to `AllReady`,
    ///   which excludes the member that just said it is buffering.
    /// * Paused — `:369-379`: the Pause goes to the CALLER only.
    /// * a `PlaylistItemId` that is not the playing one — `:333-344`: the caller
    ///   is sent the queue and nothing else happens.
    #[tokio::test]
    async fn buffering_has_one_arm_per_state() {
        let (m, bus, a, b, gid) = pair().await;

        // -- Idle
        let at = mark(&bus);
        m.handle_request(&a, buffer_at(Uuid::nil(), 0))
            .await
            .unwrap();
        let (caller, peer) = since(&bus, at);
        assert_eq!(caller.len(), 1, "{caller:?}");
        assert!(is_cmd(&caller[0], "Stop"));
        assert!(peer.is_empty(), "{peer:?}");
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Idle
        );

        // -- Playing: the buffering member is the one NOT told to pause.
        let pli = start_playing(&m, &bus, &[&a, &b], 1).await;
        let at = mark(&bus);
        m.handle_request(&a, buffer_at(pli, 0)).await.unwrap();
        let (caller, peer) = since(&bus, at);
        assert_eq!(caller.len(), 1, "{caller:?}");
        assert!(is_state(&caller[0], "Waiting", "Buffer"));
        assert_eq!(peer.len(), 2, "{peer:?}");
        assert!(is_cmd(&peer[0], "Pause"), "{peer:?}");
        assert!(is_state(&peer[1], "Waiting", "Buffer"));

        // -- the wrong playlist item: the caller gets its queue back, alone.
        let at = mark(&bus);
        m.handle_request(&b, buffer_at(Uuid::new_v4(), 0))
            .await
            .unwrap();
        let (caller, peer) = since(&bus, at);
        assert_eq!(peer.len(), 1, "{peer:?}");
        assert!(peer[0].contains("PlayQueue") && peer[0].contains("SetCurrentItem"));
        assert!(caller.is_empty(), "{caller:?}");

        // -- Paused: the Pause goes to the caller only.
        m.handle_request(&a, PlaybackRequest::Stop).await.unwrap();
        let pli = start_playing(&m, &bus, &[&a, &b], 1).await;
        m.handle_request(&a, PlaybackRequest::Pause).await.unwrap();
        let at = mark(&bus);
        m.handle_request(&a, buffer_at(pli, 0)).await.unwrap();
        let (caller, peer) = since(&bus, at);
        assert_eq!(caller.len(), 2, "{caller:?}");
        assert!(is_cmd(&caller[0], "Pause"));
        assert!(is_state(&caller[1], "Waiting", "Buffer"));
        assert_eq!(peer.len(), 1, "the paused Pause is caller-only: {peer:?}");
        assert!(is_state(&peer[0], "Waiting", "Buffer"));
    }

    /// `POST /SyncPlay/Ready` outside `Waiting` is the "client got lost" answer:
    /// the group re-tells the CALLER what it is doing, and nobody else hears it
    /// (`IdleGroupState.cs:91-95`, `PlayingGroupState.cs:133-138`,
    /// `PausedGroupState.cs:126-131`). Ferrofin sent nothing at all from Idle and
    /// from Paused.
    #[tokio::test]
    async fn ready_outside_waiting_resyncs_the_caller_only() {
        let (m, bus, a, b, _gid) = pair().await;

        let at = mark(&bus);
        m.handle_request(&a, ready_at(Uuid::nil(), 0, false))
            .await
            .unwrap();
        let (caller, peer) = since(&bus, at);
        assert_eq!(caller.len(), 1, "idle: {caller:?}");
        assert!(is_cmd(&caller[0], "Stop"));
        assert!(peer.is_empty(), "{peer:?}");

        let pli = start_playing(&m, &bus, &[&a, &b], 1).await;
        let at = mark(&bus);
        m.handle_request(&a, ready_at(pli, 0, true)).await.unwrap();
        let (caller, peer) = since(&bus, at);
        assert_eq!(caller.len(), 1, "playing: {caller:?}");
        assert!(is_cmd(&caller[0], "Unpause"));
        assert!(peer.is_empty(), "{peer:?}");

        m.handle_request(&a, PlaybackRequest::Pause).await.unwrap();
        let at = mark(&bus);
        m.handle_request(&a, ready_at(pli, 0, false)).await.unwrap();
        let (caller, peer) = since(&bus, at);
        assert_eq!(caller.len(), 1, "paused: {caller:?}");
        assert!(is_cmd(&caller[0], "Pause"));
        assert!(peer.is_empty(), "{peer:?}");
    }

    /// A Ready whose `PlaylistItemId` is not the playing one is not a Ready at
    /// all: `WaitingGroupState.cs:407-418` sends the caller its queue, marks it
    /// buffering and RETURNS, so the group keeps waiting. Ferrofin ignored the
    /// field entirely and started playback on it.
    #[tokio::test]
    async fn ready_for_the_wrong_item_resyncs_the_queue_and_keeps_waiting() {
        let (m, bus, a, b, gid) = pair().await;
        m.handle_request(&a, play(1)).await.unwrap();
        let at = mark(&bus);

        m.handle_request(&b, ready_at(Uuid::new_v4(), 0, true))
            .await
            .unwrap();

        let (caller, peer) = since(&bus, at);
        assert_eq!(peer.len(), 1, "{peer:?}");
        assert!(peer[0].contains("PlayQueue") && peer[0].contains("SetCurrentItem"));
        assert!(caller.is_empty(), "{caller:?}");
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Waiting
        );
    }

    /// `WaitingGroupState.cs:471-479` — while ANOTHER member is still buffering,
    /// the member that just reported ready is told to pause when it gets there,
    /// and it is told alone. Only the last Ready starts the group.
    #[tokio::test]
    async fn ready_while_others_buffer_pauses_the_caller_only() {
        let (m, bus, a, b, gid) = pair().await;
        m.handle_request(&a, play(1)).await.unwrap();
        let pli = playing_item_id(&bus);
        let at = mark(&bus);

        m.handle_request(&a, ready_at(pli, 0, true)).await.unwrap();

        let (caller, peer) = since(&bus, at);
        assert_eq!(caller.len(), 1, "{caller:?}");
        assert!(is_cmd(&caller[0], "Pause"), "{caller:?}");
        assert!(peer.is_empty(), "{peer:?}");
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Waiting
        );

        let at = mark(&bus);
        m.handle_request(&b, ready_at(pli, 0, true)).await.unwrap();
        let (caller, peer) = since(&bus, at);
        assert_eq!(peer.len(), 2, "{peer:?}");
        assert!(is_cmd(&peer[0], "Unpause"));
        assert!(is_state(&peer[1], "Playing", "Ready"));
        assert_eq!(caller.len(), 2, "{caller:?}");
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Playing
        );
    }

    /// `WaitingGroupState.cs:540-558` — when the group is NOT going to resume, a
    /// Ready that completes the wait settles it into `Paused` by REPLAYING the
    /// state it came from: `InitialState == Playing` replays the Pause (reason
    /// `Pause`), `InitialState == Paused` replays the Ready (reason `Ready`).
    /// Ferrofin had neither the flag nor the field, so it always resumed.
    #[tokio::test]
    async fn a_ready_that_ends_a_wait_settles_into_paused_by_initial_state() {
        // InitialState == Playing: buffer out of Playing, then Pause while
        // waiting, then Ready.
        let (m, bus, a, b, gid) = pair().await;
        let pli = start_playing(&m, &bus, &[&a, &b], 1).await;
        m.handle_request(&a, buffer_at(pli, 0)).await.unwrap();
        m.handle_request(&a, PlaybackRequest::Pause).await.unwrap();
        let at = mark(&bus);

        m.handle_request(&a, ready_at(pli, 0, false)).await.unwrap();

        let (caller, peer) = since(&bus, at);
        for got in [&caller, &peer] {
            assert_eq!(got.len(), 2, "{got:?}");
            assert!(is_cmd(&got[0], "Pause"));
            assert!(is_state(&got[1], "Paused", "Pause"), "{got:?}");
        }
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Paused
        );

        // InitialState == Paused: buffer out of Paused, then Ready.
        m.handle_request(&a, buffer_at(pli, 0)).await.unwrap();
        let at = mark(&bus);
        m.handle_request(&a, ready_at(pli, 0, false)).await.unwrap();
        let (caller, peer) = since(&bus, at);
        for got in [&caller, &peer] {
            assert_eq!(got.len(), 2, "{got:?}");
            assert!(is_cmd(&got[0], "Pause"));
            assert!(is_state(&got[1], "Paused", "Ready"), "{got:?}");
        }
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Paused
        );
    }

    /// `POST /SyncPlay/SetIgnoreWait`. `Group.IsBuffering` skips a member that
    /// asked to be ignored (Group.cs:475-486), so the flag can be what RELEASES
    /// a waiting group — `WaitingGroupState.cs:655-678` re-evaluates the wait and
    /// either resumes or settles. Ferrofin wrote the flag and never read it, so
    /// the group waited forever.
    #[tokio::test]
    async fn ignore_wait_releases_a_waiting_group() {
        let (m, bus, a, b, gid) = pair().await;
        start_playing(&m, &bus, &[&a, &b], 1).await;
        let c = session("s3", "carol");
        m.join_group(&c, gid).await.unwrap(); // -> Waiting on carol
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Waiting
        );
        let at = mark(&bus);

        m.handle_request(&c, PlaybackRequest::IgnoreWait { ignore_wait: true })
            .await
            .unwrap();

        let (caller, peer) = since(&bus, at);
        for got in [&caller, &peer] {
            assert_eq!(got.len(), 2, "{got:?}");
            assert!(is_cmd(&got[0], "Unpause"));
            // The reason is `Unpause`, not `IgnoreWait`: the state update reads
            // `reason.Action` off the UnpauseGroupRequest the Waiting arm builds.
            assert!(is_state(&got[1], "Playing", "Unpause"), "{got:?}");
        }
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Playing
        );
    }

    /// The other half of the same arm: a group that was NOT going to resume
    /// settles into `Paused` SILENTLY — upstream sends nothing at all there.
    #[tokio::test]
    async fn ignore_wait_settles_a_paused_wait_silently() {
        let (m, bus, a, b, gid) = pair().await;
        start_playing(&m, &bus, &[&a, &b], 1).await;
        m.handle_request(&a, PlaybackRequest::Pause).await.unwrap();
        let c = session("s3", "carol");
        m.join_group(&c, gid).await.unwrap(); // Paused -> Waiting, resume clear
        let at = mark(&bus);

        m.handle_request(&c, PlaybackRequest::IgnoreWait { ignore_wait: true })
            .await
            .unwrap();

        let (caller, peer) = since(&bus, at);
        assert!(caller.is_empty() && peer.is_empty(), "{caller:?} {peer:?}");
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Paused
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
        let (m, bus) = mgr();
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
        // `WaitingGroupState.cs:575-579`: a queue step whose `PlaylistItemId` is
        // not the playing one is a duplicate request and is dropped, so the real
        // id has to be echoed back.
        m.handle_request(
            &a,
            PlaybackRequest::NextItem {
                playlist_item_id: playing_item_id(&bus),
            },
        )
        .await
        .unwrap();
        m.handle_request(
            &a,
            PlaybackRequest::PreviousItem {
                playlist_item_id: playing_item_id(&bus),
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
        // Every queue mutation routes through `WaitingGroupState`, so the group
        // is waiting for fresh Ready events, not playing.
        assert_eq!(
            m.get_group(&a, info.group_id).await.unwrap().state,
            GroupStateType::Waiting
        );
    }

    #[tokio::test]
    async fn ping_seek_buffer_ready_flow() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        let gid = info.group_id;
        m.handle_request(&a, PlaybackRequest::Ping { ping: 40 })
            .await
            .unwrap();
        let pli = start_playing(&m, &bus, &[&a], 1).await;
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
        m.handle_request(&a, buffer_at(pli, 5_000_000))
            .await
            .unwrap();
        m.handle_request(&a, ready_at(pli, 5_000_000, true))
            .await
            .unwrap();
        assert_eq!(
            m.get_group(&a, gid).await.unwrap().state,
            GroupStateType::Playing
        );
    }

    /// A queue that was created but never played reports NOTHING playing.
    ///
    /// `PlayQueueManager()` calls `Reset()` -> `PlayingItemIndex = -1`
    /// (PlayQueueManager.cs:37-40, :20, :100). `#[derive(Default)]` gave 0,
    /// which is a valid index, so an unplayed queue claimed to be playing its
    /// first item and put `PlayingItemIndex: 0` on the wire where Jellyfin puts
    /// `-1`. Found live on the parity pair by exercising RemoveFromPlaylist on a
    /// group that had queued but never played.
    #[test]
    fn a_fresh_play_queue_has_nothing_playing() {
        let q = PlayQueue::default();
        assert_eq!(q.playing_index, NO_PLAYING_ITEM_INDEX);
        assert!(!q.is_item_playing());
    }

    /// The same rule after `clear(false)`: "keep the playing item" cannot
    /// resurrect one that never existed (`ClearPlaylist`, PlayQueueManager.cs:176-197).
    #[test]
    fn clearing_an_unplayed_queue_keeps_no_phantom_item() {
        let mut q = PlayQueue::default();
        q.clear(false);
        assert!(
            q.playlist().is_empty(),
            "nothing was playing, so nothing is kept"
        );
        assert_eq!(q.playing_index, NO_PLAYING_ITEM_INDEX);
        assert!(!q.is_item_playing());
    }

    /// `len` queued items with the one at `playing` playing — `CreateQueue`
    /// plus `SetPlayingItemByIndex` in v12.2 `PlayQueueManagerTests`.
    fn queue_playing(len: usize, playing: i32) -> PlayQueue {
        let ids: Vec<Uuid> = (0..len).map(|_| Uuid::new_v4()).collect();
        let mut q = PlayQueue::default();
        q.set(&ids, playing);
        q
    }

    /// v12.2 `PlayQueueManagerTests` (PR 17234): removed items in front of the
    /// playing item shift its index too (`removedBeforePlayingItem`,
    /// PlayQueueManager.cs:299-306, :318), so removing the playing item picks
    /// the item that was before it in the ORIGINAL queue.
    #[rstest::rstest]
    // RemoveFromPlaylist_PlayingItemAndPrecedingItemRemoved_PicksPreviousItem
    #[case::preceding_item_removed(5, 3, &[0, 3], 2, 1)]
    // RemoveFromPlaylist_PlayingItemAndAllPrecedingItemsRemoved_PicksFirstRemainingItem
    #[case::all_preceding_items_removed(3, 2, &[0, 2], 1, 0)]
    fn removing_the_playing_item_picks_the_item_before_it(
        #[case] len: usize,
        #[case] playing: i32,
        #[case] removed: &[usize],
        #[case] expected_item: usize,
        #[case] expected_index: i32,
    ) {
        let mut q = queue_playing(len, playing);
        let before = q.playlist().to_vec();
        let ids: Vec<Uuid> = removed
            .iter()
            .map(|&i| before[i].playlist_item_id)
            .collect();

        assert!(q.remove(&ids));
        assert_eq!(q.playlist().len(), len - removed.len());
        assert_eq!(q.playing_index, expected_index);
        assert_eq!(
            q.current().map(|i| i.item_id),
            Some(before[expected_item].item_id)
        );
    }

    /// v12.2 `RemoveFromPlaylist_AllItemsRemoved_ResetsPlayingItem`.
    #[test]
    fn removing_every_item_leaves_nothing_playing() {
        let mut q = queue_playing(2, 1);
        let ids: Vec<Uuid> = q.playlist().iter().map(|i| i.playlist_item_id).collect();

        assert!(q.remove(&ids));
        assert!(q.playlist().is_empty());
        assert!(!q.is_item_playing());
        assert_eq!(q.playing_item_id(), Uuid::nil());
    }

    /// v12.2 `RemoveFromPlaylist_PlayingItemNotRemoved_RestoresPlayingItem`.
    #[test]
    fn removing_an_earlier_item_keeps_the_playing_item() {
        let mut q = queue_playing(3, 2);
        let before = q.playlist().to_vec();

        assert!(!q.remove(&[before[0].playlist_item_id]));
        assert_eq!(q.playing_index, 1);
        assert_eq!(q.current().map(|i| i.item_id), Some(before[2].item_id));
    }

    /// v12.2 `Next_EmptyPlaylist_ReturnsFalse` / `Previous_EmptyPlaylist_ReturnsFalse`:
    /// an empty queue has nowhere to go in any repeat mode
    /// (PlayQueueManager.cs:459-462, :494-497).
    #[rstest::rstest]
    fn stepping_an_empty_queue_goes_nowhere(
        #[values(
            GroupRepeatMode::RepeatNone,
            GroupRepeatMode::RepeatOne,
            GroupRepeatMode::RepeatAll
        )]
        repeat: GroupRepeatMode,
        #[values(true, false)] forward: bool,
    ) {
        let mut q = PlayQueue {
            repeat,
            ..PlayQueue::default()
        };

        let moved = if forward { q.next() } else { q.previous() };

        assert!(!moved);
        assert!(!q.is_item_playing());
        assert_eq!(q.playing_item_id(), Uuid::nil());
    }

    /// v12.2 `RemoveFromPlaylist_ShuffleMode_PicksPreviousItem`: while
    /// shuffled, removal counts and picks in the shuffled queue.
    #[test]
    fn removing_while_shuffled_picks_the_previous_shuffled_item() {
        let mut q = queue_playing(5, 0);
        q.set_shuffle_mode(GroupShuffleMode::Shuffle);
        q.playing_index = 3;
        let playlist = q.playlist().to_vec();

        assert!(q.remove(&[playlist[0].playlist_item_id, playlist[3].playlist_item_id]));
        assert_eq!(q.playlist().len(), 3);
        assert_eq!(q.sorted.len(), 3);
        assert_eq!(q.playing_index, 1);
        assert_eq!(q.current().map(|i| i.item_id), Some(playlist[2].item_id));
    }

    /// v12.2 `SetShuffleMode_SortedWhileAlreadySorted_KeepsPlayingItem` and
    /// `SetShuffleMode_SortedTwiceAfterShuffle_KeepsPlayingItem`: restoring the
    /// sorted queue, once or twice, keeps the playing item.
    #[rstest::rstest]
    #[case::already_sorted(3, 1, &[GroupShuffleMode::Sorted])]
    #[case::sorted_twice_after_shuffle(
        5,
        2,
        &[GroupShuffleMode::Shuffle, GroupShuffleMode::Sorted, GroupShuffleMode::Sorted]
    )]
    fn restoring_the_sorted_queue_keeps_the_playing_item(
        #[case] len: usize,
        #[case] playing: i32,
        #[case] modes: &[GroupShuffleMode],
    ) {
        let mut q = queue_playing(len, playing);
        let expected = q.current().copied();

        for &mode in modes {
            q.set_shuffle_mode(mode);
        }

        assert_eq!(q.shuffle, GroupShuffleMode::Sorted);
        assert_eq!(q.playlist().len(), len);
        assert_eq!(q.playing_index, playing);
        assert_eq!(q.current().copied(), expected);
    }

    /// `ShufflePlaylist` with nothing playing (PlayQueueManager.cs:126-130)
    /// shuffles the whole queue and leaves nothing playing; shuffling again
    /// while something plays keeps it first.
    #[test]
    fn shuffling_with_nothing_playing_shuffles_every_item() {
        let mut q = queue_playing(5, 0);
        q.playing_index = NO_PLAYING_ITEM_INDEX;
        let sorted = q.sorted.clone();

        q.set_shuffle_mode(GroupShuffleMode::Shuffle);
        assert!(!q.is_item_playing());
        let mut got = q.playlist().to_vec();
        got.sort_by_key(|i| i.playlist_item_id);
        let mut want = sorted.clone();
        want.sort_by_key(|i| i.playlist_item_id);
        assert_eq!(got, want);
        assert_eq!(q.sorted, sorted, "the sorted queue is kept as it was");

        q.playing_index = 3;
        let playing = q.current().copied();
        q.set_shuffle_mode(GroupShuffleMode::Shuffle);
        assert_eq!(q.playing_index, 0);
        assert_eq!(q.current().copied(), playing);
        assert_eq!(q.playlist().len(), 5);
    }

    /// A shuffle does reorder: the seven items behind the playing one come back
    /// in their sorted order with probability 1/5040, so ten shuffles in a row
    /// that all leave them sorted mean the queue is not being shuffled.
    #[test]
    fn shuffling_changes_the_order() {
        let mut q = queue_playing(8, 0);
        let sorted = q.sorted.clone();
        let reordered = (0..10).any(|_| {
            q.set_shuffle_mode(GroupShuffleMode::Shuffle);
            let changed = q.playlist() != sorted.as_slice();
            q.set_shuffle_mode(GroupShuffleMode::Sorted);
            changed
        });
        assert!(reordered);
    }

    /// The list the access checks compare does not move with a shuffle, so a
    /// reshuffle during `join_group`'s off-lock check cannot refuse the join.
    #[test]
    fn shuffling_keeps_the_access_check_list() {
        let mut q = queue_playing(6, 2);
        let before = q.item_ids();

        q.set_shuffle_mode(GroupShuffleMode::Shuffle);

        assert_eq!(q.item_ids(), before);
    }

    /// `Next`/`Previous` at the ends of a three-item queue in every repeat mode
    /// (PlayQueueManager.cs:457-521): `(repeat, forward, from, moved, to)`.
    #[rstest::rstest]
    #[case::none_next_middle(GroupRepeatMode::RepeatNone, true, 1, true, 2)]
    #[case::none_next_last(GroupRepeatMode::RepeatNone, true, 2, false, 2)]
    #[case::none_previous_first(GroupRepeatMode::RepeatNone, false, 0, false, 0)]
    #[case::none_previous_nothing_playing(GroupRepeatMode::RepeatNone, false, -1, false, 0)]
    #[case::one_next(GroupRepeatMode::RepeatOne, true, 2, true, 2)]
    #[case::one_previous(GroupRepeatMode::RepeatOne, false, 0, true, 0)]
    #[case::all_next_last(GroupRepeatMode::RepeatAll, true, 2, true, 0)]
    #[case::all_previous_first(GroupRepeatMode::RepeatAll, false, 0, true, 2)]
    fn stepping_follows_the_repeat_mode(
        #[case] repeat: GroupRepeatMode,
        #[case] forward: bool,
        #[case] from: i32,
        #[case] moved: bool,
        #[case] to: i32,
    ) {
        let mut q = queue_playing(3, 0);
        q.repeat = repeat;
        q.playing_index = from;

        assert_eq!(if forward { q.next() } else { q.previous() }, moved);
        assert_eq!(q.playing_index, to);
    }

    /// While shuffled, `Queue` appends to both lists and `ClearPlaylist`
    /// keeps the playing item in both (PlayQueueManager.cs:108-119, :179-200),
    /// so the sorted queue restored afterwards has the same items.
    #[test]
    fn queue_and_clear_while_shuffled_keep_both_lists_in_step() {
        let mut q = queue_playing(3, 1);
        q.set_shuffle_mode(GroupShuffleMode::Shuffle);
        let added = Uuid::new_v4();

        q.enqueue(&[added], GroupQueueMode::Queue);
        assert_eq!(q.playlist().last().map(|i| i.item_id), Some(added));
        assert_eq!(q.sorted.last().map(|i| i.item_id), Some(added));

        let playing = q.current().copied();
        q.clear(false);
        assert_eq!(q.playlist(), q.sorted.as_slice());
        q.set_shuffle_mode(GroupShuffleMode::Sorted);
        assert_eq!(q.playlist().len(), 1);
        assert_eq!(q.current().copied(), playing);
    }

    /// The issue's case end to end: [A, B, C] with C playing, and one request
    /// removes A and C. Upstream picks B at index 0; stepping back one slot
    /// left the index past the end of the one-item queue, in the update every
    /// member receives.
    #[tokio::test]
    async fn removing_the_playing_item_with_an_earlier_one_stays_in_the_queue() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        m.new_group(&a, "g").await.unwrap();
        m.handle_request(
            &a,
            PlaybackRequest::Play {
                playing_queue: (0..3).map(|_| Uuid::new_v4()).collect(),
                playing_item_position: 2,
                start_position_ticks: 0,
            },
        )
        .await
        .unwrap();
        let queue = last_playlist(&bus);

        m.handle_request(
            &a,
            PlaybackRequest::RemoveFromPlaylist {
                playlist_item_ids: vec![queue[0], queue[2]],
                clear_playlist: false,
                clear_playing_item: false,
            },
        )
        .await
        .unwrap();

        assert_eq!(last_playlist(&bus), [queue[1]]);
        assert_eq!(playing_item_id(&bus), queue[1]);
    }

    /// The issue's other case: after a clear that removed the playing item, a
    /// NextItem (jellyfin-web sends `PlaylistItemId: null` with nothing
    /// playing) must not "advance" into the empty queue. Upstream's `Next()`
    /// returns false, so `WaitingGroupState.cs:609-622` broadcasts nothing and
    /// returns the group to `Idle`.
    #[tokio::test]
    async fn next_item_on_a_cleared_queue_changes_nothing() {
        let (m, bus) = mgr();
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
        let at = mark(&bus);

        m.handle_request(
            &a,
            PlaybackRequest::NextItem {
                playlist_item_id: Uuid::nil(),
            },
        )
        .await
        .unwrap();

        let (caller, _) = since(&bus, at);
        assert!(caller.is_empty(), "{caller:?}");
        assert_eq!(
            m.get_group(&a, info.group_id).await.unwrap().state,
            GroupStateType::Idle
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

    /// `AbstractGroupState.cs:69-95` + `PlayQueueManager.ClearPlaylist`
    /// (:176-197): `ClearPlayingItem=false` KEEPS the playing item, so nothing
    /// was removed that was playing and the group never stops. Ferrofin used to
    /// throw the whole `PlayQueue` away — shuffle and repeat with it — and Stop.
    #[tokio::test]
    async fn clearing_the_playlist_can_keep_the_playing_item() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        m.handle_request(&a, play(3)).await.unwrap();
        m.handle_request(
            &a,
            PlaybackRequest::SetShuffleMode {
                mode: GroupShuffleMode::Shuffle,
            },
        )
        .await
        .unwrap();
        let at = mark(&bus);

        m.handle_request(
            &a,
            PlaybackRequest::RemoveFromPlaylist {
                playlist_item_ids: vec![],
                clear_playlist: true,
                clear_playing_item: false,
            },
        )
        .await
        .unwrap();

        let (caller, _) = since(&bus, at);
        assert_eq!(
            caller.len(),
            1,
            "one RemoveItems update, no Stop: {caller:?}"
        );
        assert!(
            caller[0].contains("\"Reason\":\"RemoveItems\""),
            "{caller:?}"
        );
        // The playing item survived, still playing, and shuffle survived with it.
        let v: serde_json::Value = serde_json::from_str(&caller[0]).unwrap();
        let data = &v["Data"]["Data"];
        assert_eq!(data["Playlist"].as_array().unwrap().len(), 1);
        assert_eq!(data["PlayingItemIndex"], 0);
        assert_eq!(data["ShuffleMode"], "Shuffle");
        assert_eq!(
            m.get_group(&a, info.group_id).await.unwrap().state,
            GroupStateType::Waiting
        );
    }

    /// `Group.SetPlayQueue` calls `PlayQueue.Reset()` before filling the queue,
    /// and `Reset` (PlayQueueManager.cs:352-361) clears shuffle and repeat too —
    /// a new queue arrives unshuffled and not repeating.
    #[tokio::test]
    async fn a_new_queue_resets_shuffle_and_repeat() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        m.new_group(&a, "g").await.unwrap();
        m.handle_request(&a, play(2)).await.unwrap();
        for r in [
            PlaybackRequest::SetRepeatMode {
                mode: GroupRepeatMode::RepeatAll,
            },
            PlaybackRequest::SetShuffleMode {
                mode: GroupShuffleMode::Shuffle,
            },
        ] {
            m.handle_request(&a, r).await.unwrap();
        }
        let at = mark(&bus);

        m.handle_request(&a, play(2)).await.unwrap();

        let (caller, _) = since(&bus, at);
        let v: serde_json::Value = serde_json::from_str(&caller[0]).unwrap();
        assert_eq!(v["Data"]["Data"]["ShuffleMode"], "Sorted", "{caller:?}");
        assert_eq!(v["Data"]["Data"]["RepeatMode"], "RepeatNone", "{caller:?}");
    }

    /// The `Data` of the last `PlayQueue` update, as a client receives it.
    fn last_play_queue(bus: &RecordingBus) -> serde_json::Value {
        let body = bus
            .bodies()
            .into_iter()
            .rev()
            .find(|b| b.contains("\"Type\":\"PlayQueue\""))
            .expect("a PlayQueue update was pushed");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        v["Data"]["Data"].clone()
    }

    /// Every `ItemId` in the last `PlayQueue` update, in queue order.
    fn last_item_ids(bus: &RecordingBus) -> Vec<Uuid> {
        last_play_queue(bus)["Playlist"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| Uuid::parse_str(i["ItemId"].as_str().unwrap()).unwrap())
            .collect()
    }

    /// A new group whose `len`-item queue plays the item at `position`; returns
    /// the queue's `PlaylistItemId`s in order.
    async fn group_playing(
        m: &FerrofinSyncPlayManager,
        bus: &RecordingBus,
        a: &SyncPlaySession,
        len: usize,
        position: i32,
    ) -> Vec<Uuid> {
        m.new_group(a, "g").await.unwrap();
        m.handle_request(
            a,
            PlaybackRequest::Play {
                playing_queue: (0..len).map(|_| Uuid::new_v4()).collect(),
                playing_item_position: position,
                start_position_ticks: 0,
            },
        )
        .await
        .unwrap();
        last_playlist(bus)
    }

    fn shuffle_mode(mode: GroupShuffleMode) -> PlaybackRequest {
        PlaybackRequest::SetShuffleMode { mode }
    }

    /// `ShufflePlaylist` (PlayQueueManager.cs:124-153): shuffling sends the
    /// shuffled queue with the playing item first, and `RestoreSortedPlaylist`
    /// (:158-173) brings the original order back with the playing item at its
    /// place in it. Ferrofin only flipped the flag, so every client showed
    /// shuffle on while the group kept playing the queue in order.
    #[tokio::test]
    async fn shuffle_reorders_the_queue_behind_the_playing_item() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let sorted = group_playing(&m, &bus, &a, 6, 2).await;

        m.handle_request(&a, shuffle_mode(GroupShuffleMode::Shuffle))
            .await
            .unwrap();
        let update = last_play_queue(&bus);
        assert_eq!(update["Reason"], "ShuffleMode");
        assert_eq!(update["ShuffleMode"], "Shuffle");
        assert_eq!(update["PlayingItemIndex"], 0);
        let shuffled = last_playlist(&bus);
        assert_eq!(shuffled[0], sorted[2]);
        let (mut got, mut want) = (shuffled.clone(), sorted.clone());
        got.sort();
        want.sort();
        assert_eq!(got, want, "a shuffle keeps every item exactly once");

        m.handle_request(&a, shuffle_mode(GroupShuffleMode::Sorted))
            .await
            .unwrap();
        let update = last_play_queue(&bus);
        assert_eq!(update["ShuffleMode"], "Sorted");
        assert_eq!(update["PlayingItemIndex"], 2);
        assert_eq!(last_playlist(&bus), sorted);
    }

    /// `MovePlaylistItem` (PlayQueueManager.cs:343-362) reorders the ACTIVE
    /// list: a move while shuffled changes the shuffle, and the sorted queue
    /// comes back untouched.
    #[tokio::test]
    async fn a_move_while_shuffled_leaves_the_sorted_queue_alone() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let sorted = group_playing(&m, &bus, &a, 4, 0).await;
        m.handle_request(&a, shuffle_mode(GroupShuffleMode::Shuffle))
            .await
            .unwrap();
        let last = last_playlist(&bus)[3];

        m.handle_request(
            &a,
            PlaybackRequest::MovePlaylistItem {
                playlist_item_id: last,
                new_index: 1,
            },
        )
        .await
        .unwrap();
        assert_eq!(last_playlist(&bus)[1], last);

        m.handle_request(&a, shuffle_mode(GroupShuffleMode::Sorted))
            .await
            .unwrap();
        assert_eq!(last_playlist(&bus), sorted);
    }

    /// `QueueNext` while shuffled (PlayQueueManager.cs:206-224) lands right
    /// after the playing item in the shuffled queue the clients see, and right
    /// after it in the sorted queue too.
    #[tokio::test]
    async fn queue_next_while_shuffled_plays_next_in_both_orders() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        group_playing(&m, &bus, &a, 4, 1).await;
        let sorted_items = last_item_ids(&bus);
        m.handle_request(&a, shuffle_mode(GroupShuffleMode::Shuffle))
            .await
            .unwrap();
        let next = Uuid::new_v4();

        m.handle_request(
            &a,
            PlaybackRequest::Queue {
                item_ids: vec![next],
                mode: GroupQueueMode::QueueNext,
            },
        )
        .await
        .unwrap();
        let update = last_play_queue(&bus);
        assert_eq!(update["PlayingItemIndex"], 0);
        assert_eq!(last_item_ids(&bus)[1], next);

        m.handle_request(&a, shuffle_mode(GroupShuffleMode::Sorted))
            .await
            .unwrap();
        let mut want = sorted_items;
        want.insert(2, next);
        assert_eq!(last_item_ids(&bus), want);
    }

    /// `Next`/`Previous` (PlayQueueManager.cs:457-521) wrap around the queue
    /// under `RepeatAll`; Ferrofin ignored the repeat mode and stopped at
    /// either end.
    #[tokio::test]
    async fn repeat_all_wraps_around_the_queue() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let queue = group_playing(&m, &bus, &a, 3, 2).await;
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
            PlaybackRequest::NextItem {
                playlist_item_id: queue[2],
            },
        )
        .await
        .unwrap();
        assert_eq!(last_play_queue(&bus)["Reason"], "NextItem");
        assert_eq!(playing_item_id(&bus), queue[0]);

        m.handle_request(
            &a,
            PlaybackRequest::PreviousItem {
                playlist_item_id: queue[0],
            },
        )
        .await
        .unwrap();
        assert_eq!(last_play_queue(&bus)["Reason"], "PreviousItem");
        assert_eq!(playing_item_id(&bus), queue[2]);
    }

    /// Under `RepeatOne` NextItem and PreviousItem stay on the playing item
    /// (PlayQueueManager.cs:464-468, :499-503), and `Group.NextItemInQueue` /
    /// `PreviousItemInQueue` restart it: every member gets the update for the
    /// same item and buffers it again.
    #[tokio::test]
    async fn repeat_one_restarts_the_playing_item() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let queue = group_playing(&m, &bus, &a, 3, 1).await;
        m.handle_request(
            &a,
            PlaybackRequest::SetRepeatMode {
                mode: GroupRepeatMode::RepeatOne,
            },
        )
        .await
        .unwrap();

        for (request, reason) in [
            (
                PlaybackRequest::NextItem {
                    playlist_item_id: queue[1],
                },
                "NextItem",
            ),
            (
                PlaybackRequest::PreviousItem {
                    playlist_item_id: queue[1],
                },
                "PreviousItem",
            ),
        ] {
            m.handle_request(&a, request).await.unwrap();
            let update = last_play_queue(&bus);
            assert_eq!(update["Reason"], reason);
            assert_eq!(update["PlayingItemIndex"], 1);
        }
    }

    /// `SetPlayingItemByPlaylistId` (PlayQueueManager.cs:262-269) answers an
    /// id that is not queued with `PlayingItemIndex = -1`: the next update
    /// every member receives shows nothing playing, not the old item.
    #[tokio::test]
    async fn setting_an_unqueued_item_leaves_nothing_playing() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        group_playing(&m, &bus, &a, 3, 1).await;

        m.handle_request(
            &a,
            PlaybackRequest::SetPlaylistItem {
                playlist_item_id: Uuid::new_v4(),
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

        let update = last_play_queue(&bus);
        assert_eq!(update["Reason"], "RepeatMode");
        assert_eq!(update["PlayingItemIndex"], NO_PLAYING_ITEM_INDEX);
    }

    /// `AbstractGroupState.cs:98-111`: `MovePlaylistItem` fails for an item
    /// that is not in the queue, and the group then broadcasts nothing.
    #[tokio::test]
    async fn moving_an_item_that_is_not_queued_sends_nothing() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        group_playing(&m, &bus, &a, 3, 0).await;
        let at = mark(&bus);

        m.handle_request(
            &a,
            PlaybackRequest::MovePlaylistItem {
                playlist_item_id: Uuid::new_v4(),
                new_index: 0,
            },
        )
        .await
        .unwrap();

        let (caller, _) = since(&bus, at);
        assert!(caller.is_empty(), "{caller:?}");
    }

    /// The other half of the same condition: `playingItemRemoved` is false when
    /// the removal misses the playing item, so no Stop is sent even though the
    /// queue shrank.
    #[tokio::test]
    async fn removing_a_non_playing_item_never_stops_the_group() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        m.handle_request(&a, play(3)).await.unwrap();
        let playing = playing_item_id(&bus);
        let other = last_playlist(&bus)
            .into_iter()
            .find(|id| *id != playing)
            .expect("three items were queued");
        let at = mark(&bus);

        m.handle_request(
            &a,
            PlaybackRequest::RemoveFromPlaylist {
                playlist_item_ids: vec![other],
                clear_playlist: false,
                clear_playing_item: false,
            },
        )
        .await
        .unwrap();

        let (caller, _) = since(&bus, at);
        assert_eq!(caller.len(), 1, "update only, no Stop: {caller:?}");
        assert_eq!(playing_item_id(&bus), playing, "playing item unchanged");
        assert_ne!(
            m.get_group(&a, info.group_id).await.unwrap().state,
            GroupStateType::Idle
        );
    }

    /// `PlayQueueManager.cs:303-312`: when the PLAYING item goes, upstream steps
    /// BACK one slot; only an empty remainder leaves nothing playing, and that is
    /// the single case that Stops the group.
    #[tokio::test]
    async fn removing_the_playing_item_steps_back_one_slot() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        m.handle_request(&a, play(3)).await.unwrap();
        let items = last_playlist(&bus);
        // Point the queue at the middle item, then remove it.
        m.handle_request(
            &a,
            PlaybackRequest::SetPlaylistItem {
                playlist_item_id: items[1],
            },
        )
        .await
        .unwrap();
        let at = mark(&bus);

        m.handle_request(
            &a,
            PlaybackRequest::RemoveFromPlaylist {
                playlist_item_ids: vec![items[1]],
                clear_playlist: false,
                clear_playing_item: false,
            },
        )
        .await
        .unwrap();

        let (caller, _) = since(&bus, at);
        assert_eq!(caller.len(), 1, "update only, no Stop: {caller:?}");
        assert_eq!(
            playing_item_id(&bus),
            items[0],
            "stepped back, not to 0-by-luck"
        );
        assert_ne!(
            m.get_group(&a, info.group_id).await.unwrap().state,
            GroupStateType::Idle
        );

        // Emptying the queue is what finally stops it.
        m.handle_request(
            &a,
            PlaybackRequest::RemoveFromPlaylist {
                playlist_item_ids: vec![items[0], items[2]],
                clear_playlist: false,
                clear_playing_item: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            m.get_group(&a, info.group_id).await.unwrap().state,
            GroupStateType::Idle
        );
    }

    /// `Group.AddToPlayQueue` returns false on an empty list and
    /// `AbstractGroupState.cs:114-132` broadcasts nothing when it does.
    #[tokio::test]
    async fn queueing_nothing_pushes_nothing() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        m.new_group(&a, "g").await.unwrap();
        m.handle_request(&a, play(1)).await.unwrap();
        let at = mark(&bus);

        m.handle_request(
            &a,
            PlaybackRequest::Queue {
                item_ids: vec![],
                mode: GroupQueueMode::Queue,
            },
        )
        .await
        .unwrap();

        let (caller, _) = since(&bus, at);
        assert!(caller.is_empty(), "{caller:?}");
    }

    /// The `prevState switch` every failed `WaitingGroupState` arm writes
    /// (:144-148, :189-194, :595-600, :641-646): `Waiting` is NOT restored, it
    /// falls through the default arm to `Idle`. Ferrofin restored `prev`
    /// verbatim and sat in `Waiting` where Jellyfin was `Idle` — measured live
    /// on the parity pair before this fix.
    #[tokio::test]
    async fn a_queue_change_that_fails_while_waiting_drops_to_idle() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        m.handle_request(&a, play(1)).await.unwrap(); // Idle -> Waiting
        assert_eq!(
            m.get_group(&a, info.group_id).await.unwrap().state,
            GroupStateType::Waiting
        );
        let at = mark(&bus);

        // An empty queue is refused by `Group.SetPlayQueue`.
        m.handle_request(
            &a,
            PlaybackRequest::Play {
                playing_queue: vec![],
                playing_item_position: 0,
                start_position_ticks: 0,
            },
        )
        .await
        .unwrap();

        let (caller, _) = since(&bus, at);
        assert!(
            caller.is_empty(),
            "the failed arm broadcasts nothing: {caller:?}"
        );
        assert_eq!(
            m.get_group(&a, info.group_id).await.unwrap().state,
            GroupStateType::Idle
        );
    }

    /// ...while the two arms the switch DOES name are restored as they were.
    #[tokio::test]
    async fn a_queue_change_that_fails_while_playing_or_paused_is_restored() {
        for pause_first in [false, true] {
            let (m, bus) = mgr();
            let a = session("s1", "alice");
            let info = m.new_group(&a, "g").await.unwrap();
            start_playing(&m, &bus, &[&a], 1).await;
            let want = if pause_first {
                m.handle_request(&a, PlaybackRequest::Pause).await.unwrap();
                GroupStateType::Paused
            } else {
                GroupStateType::Playing
            };
            assert_eq!(m.get_group(&a, info.group_id).await.unwrap().state, want);

            m.handle_request(
                &a,
                PlaybackRequest::SetPlaylistItem {
                    playlist_item_id: Uuid::new_v4(), // not in the queue
                },
            )
            .await
            .unwrap();

            assert_eq!(
                m.get_group(&a, info.group_id).await.unwrap().state,
                want,
                "pause_first={pause_first}"
            );
        }
    }

    /// `TimeSpan.Ticks` is the FULL 100 ns count. Rounding a clock delta through
    /// milliseconds quantised every position Ferrofin emitted to a multiple of
    /// 10_000 ticks, which is visible beside the oracle's exact values.
    #[test]
    fn a_clock_delta_keeps_full_tick_resolution() {
        assert_eq!(
            duration_ticks(Duration::nanoseconds(123_456_700)),
            1_234_567
        );
        assert_eq!(duration_ticks(Duration::microseconds(1)), 10);
        assert_eq!(duration_ticks(Duration::nanoseconds(99)), 0);
        assert_eq!(duration_ticks(Duration::nanoseconds(-123_400)), -1_234);
        // The two fallbacks are unreachable for a playback delta, but must not
        // return nonsense if they ever are reached.
        assert_eq!(
            duration_ticks(Duration::days(365 * 400)),
            365 * 400 * 864_000_000_000
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

    /// `JoinGroup`'s "Restore session" branch. Re-joining the group you are the
    /// only member of used to leave it first, which dropped the group and then
    /// panicked under the registry lock, poisoning SyncPlay server-wide.
    #[tokio::test]
    async fn rejoining_your_own_group_as_its_only_member_restores_the_session() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        let before = bus.count_for("s1");

        m.join_group(&a, info.group_id).await.unwrap();

        let group = m.get_group(&a, info.group_id).await.unwrap();
        assert_eq!(group.participants, vec!["alice".to_string()]);
        let after = &bus.bodies_for("s1")[before..];
        assert!(after.iter().any(|x| x.contains("GroupJoined")), "{after:?}");
        assert!(!after.iter().any(|x| x.contains("GroupLeft")), "{after:?}");
        // Counted once: a single leave makes the user inactive again.
        m.leave_group(&a).await.unwrap();
        assert!(!m.is_user_active(a.user_id).await.unwrap());
        assert!(m.get_group(&a, info.group_id).await.is_err());
    }

    /// A restore keeps the member where it was (`Group.AddSession` is a
    /// `TryAdd`); leaving and re-adding would move bob behind carol.
    #[tokio::test]
    async fn rejoining_a_shared_group_keeps_one_membership() {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        let b = session("s2", "bob");
        m.join_group(&b, info.group_id).await.unwrap();
        let c = session("s3", "carol");
        m.join_group(&c, info.group_id).await.unwrap();
        let before = bus.count_for("s1");

        m.join_group(&b, info.group_id).await.unwrap();

        let group = m.get_group(&a, info.group_id).await.unwrap();
        assert_eq!(
            group.participants,
            vec!["alice".to_string(), "bob".to_string(), "carol".to_string()]
        );
        let alice_after = &bus.bodies_for("s1")[before..];
        assert!(alice_after.iter().any(|x| x.contains("UserJoined")));
        assert!(!alice_after.iter().any(|x| x.contains("UserLeft")));
        m.leave_group(&b).await.unwrap();
        assert!(!m.is_user_active(b.user_id).await.unwrap());
    }

    /// Switching groups runs the full `LeaveGroup` on the old one: the group
    /// that was waiting on the leaver resumes, its members get `UserLeft`, and
    /// the leaver gets `GroupLeft` — for both `NewGroup` and `JoinGroup`.
    #[rstest::rstest]
    #[case::new_group(true)]
    #[case::join_group(false)]
    #[tokio::test]
    async fn switching_groups_while_waiting_resumes_the_old_group(#[case] via_new: bool) {
        let (m, bus) = mgr();
        let a = session("s1", "alice");
        let old = m.new_group(&a, "old").await.unwrap();
        start_playing(&m, &bus, &[&a], 2).await;
        let b = session("s2", "bob");
        m.join_group(&b, old.group_id).await.unwrap(); // -> Waiting on bob
        let alice_before = bus.count_for("s1");
        let bob_before = bus.count_for("s2");

        if via_new {
            m.new_group(&b, "new").await.unwrap();
        } else {
            let c = session("s3", "carol");
            let other = m.new_group(&c, "other").await.unwrap();
            m.join_group(&b, other.group_id).await.unwrap();
        }

        assert_eq!(
            m.get_group(&a, old.group_id).await.unwrap().state,
            GroupStateType::Playing
        );
        let alice_after = &bus.bodies_for("s1")[alice_before..];
        assert!(
            alice_after
                .iter()
                .any(|x| x.contains("SyncPlayCommand") && x.contains("Unpause")),
            "{alice_after:?}"
        );
        assert!(alice_after.iter().any(|x| x.contains("UserLeft")));
        let bob_after = &bus.bodies_for("s2")[bob_before..];
        let left = bob_after.iter().position(|x| x.contains("GroupLeft"));
        let joined = bob_after.iter().position(|x| x.contains("GroupJoined"));
        assert!(left.is_some() && left < joined, "{bob_after:?}");
        // One group, one count: leaving the new group makes bob inactive.
        m.leave_group(&b).await.unwrap();
        assert!(!m.is_user_active(b.user_id).await.unwrap());
    }

    #[tokio::test]
    async fn a_poisoned_registry_recovers_instead_of_panicking_every_call() {
        let (m, _bus) = mgr();
        let s = session("s1", "alice");
        m.new_group(&s, "g").await.unwrap();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = m.registry.lock().unwrap();
            panic!("poison the sync-play registry");
        }));
        assert!(poisoned.is_err());
        assert!(m.registry.is_poisoned());

        assert!(m.is_user_active(s.user_id).await.unwrap());
        assert!(!m.registry.is_poisoned());
    }

    // ── library access (C# `Group.HasAccessToPlayQueue`) ───────────────────

    /// A manager wired to a real DB-backed library + user manager, so the
    /// access checks run through the real standalone visibility evaluator.
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
        let items: Arc<dyn ferrofin_traits::persistence::ItemRepository> = Arc::new(
            crate::item_repository::FerrofinItemRepository::new(db.clone(), lookup),
        );
        let visibility = Arc::new(crate::item_visibility::ItemVisibility::new(
            db.clone(),
            items.clone(),
            Arc::new(crate::localization_manager::LocalizationManager::new("US")),
            "/server/data".to_owned(),
        ));
        let library: Arc<dyn LibraryManager> = Arc::new(
            crate::library_manager::FerrofinLibraryManager::new(
                items,
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
            )
            .with_visibility(visibility),
        );
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

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn hidden_library_and_policy_changes_control_queue_access() {
        use ferrofin_model::data::BaseItemKind;
        use ferrofin_model::users::UserPolicy;
        let (m, bus, db, library) = access_mgr().await;
        let owner = db_session(&db, "s1", "alice").await;
        let guest = db_session(&db, "s2", "bob").await;
        let folder_id = Uuid::new_v4();
        crate::test_support::seed_folder_item(
            &db,
            folder_id,
            BaseItemKind::CollectionFolder,
            "Hidden",
            None,
        )
        .await;
        let mut folder = library.get_item_by_id(folder_id).await.unwrap().unwrap();
        folder.path = Some("/views/hidden".into());
        let movie_id = seed_movie(&db, "Hidden movie").await;
        let mut movie = library.get_item_by_id(movie_id).await.unwrap().unwrap();
        movie.parent_id = Some(folder.id.clone());
        movie.top_parent_id = Some(folder.id.clone());
        movie.path = Some("/media/hidden.mkv".into());
        movie.tags = Some("Restricted".into());
        library.update_items(&[folder, movie], None).await.unwrap();
        let users = crate::user_manager::FerrofinUserManager::new(db.clone());
        users
            .update_policy(
                owner.user_id,
                &UserPolicy {
                    enable_all_folders: true,
                    ..UserPolicy::default()
                },
            )
            .await
            .unwrap();
        users
            .update_policy(
                guest.user_id,
                &UserPolicy {
                    enable_all_folders: false,
                    ..UserPolicy::default()
                },
            )
            .await
            .unwrap();
        let guest_user = users.get_user_by_id(guest.user_id).await.unwrap().unwrap();
        // Explicit-ID browse remains permissive, proving it cannot implement this gate.
        assert_eq!(
            library
                .get_item_ids(&InternalItemsQuery {
                    item_ids: vec![movie_id],
                    user: Some(guest_user.clone()),
                    ..InternalItemsQuery::default()
                })
                .await
                .unwrap(),
            vec![movie_id]
        );
        assert!(
            library
                .get_visible_item_ids(&vec![movie_id; 1200], &guest_user)
                .await
                .unwrap()
                .is_empty()
        );
        let group = m.new_group(&owner, "movie night").await.unwrap();
        set_queue(&m, &owner, vec![movie_id]).await;
        assert!(m.list_groups(&guest).await.unwrap().is_empty());
        m.join_group(&guest, group.group_id).await.unwrap();
        assert_eq!(update_types(&bus, "s2"), vec!["LibraryAccessDenied"]);
        users
            .update_policy(
                guest.user_id,
                &UserPolicy {
                    enable_all_folders: true,
                    ..UserPolicy::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(m.list_groups(&guest).await.unwrap().len(), 1);
        m.join_group(&guest, group.group_id).await.unwrap();
        assert_eq!(
            m.get_group(&owner, group.group_id)
                .await
                .unwrap()
                .participants
                .len(),
            2
        );
        users
            .update_policy(
                guest.user_id,
                &UserPolicy {
                    enable_all_folders: true,
                    blocked_tags: vec!["restricted".into()],
                    ..UserPolicy::default()
                },
            )
            .await
            .unwrap();
        assert!(m.list_groups(&guest).await.unwrap().is_empty());
        bus.sent.lock().unwrap().clear();
        set_queue(&m, &owner, vec![movie_id]).await;
        assert!(
            bus.sent.lock().unwrap().is_empty(),
            "a queue edit inaccessible to a member is refused"
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

    #[test]
    fn empty_group_returns_default_ping() {
        let group = Group::new(Uuid::new_v4(), String::new(), Utc::now());
        assert_eq!(group.highest_ping_ms(), DEFAULT_PING_MS);
    }

    #[tokio::test]
    async fn ping_is_clamped_to_max() {
        let (m, _bus) = mgr();
        let a = session("s1", "alice");
        m.new_group(&a, "g").await.unwrap();
        m.handle_request(&a, PlaybackRequest::Ping { ping: i64::MAX })
            .await
            .unwrap();
        let reg = m.lock();
        let group = reg.groups.values().next().unwrap();
        assert_eq!(group.highest_ping_ms(), MAX_PING_MS);
    }

    #[tokio::test]
    async fn negative_ping_is_clamped_to_zero() {
        let (m, _bus) = mgr();
        let a = session("s1", "alice");
        m.new_group(&a, "g").await.unwrap();
        m.handle_request(&a, PlaybackRequest::Ping { ping: -1 })
            .await
            .unwrap();
        let reg = m.lock();
        let group = reg.groups.values().next().unwrap();
        let member = group.members.first().unwrap();
        assert_eq!(member.ping_ms, 0);
    }

    // ----- group-wait timeout (12.1) ---------------------------------------
    //
    // Transliterated from upstream's `WaitingGroupStateTests` and
    // `SyncPlayManagerTests` (v12.2, jellyfin#17938). Upstream gives its
    // groups a 200 ms wait and pumps `Group.HandleGroupWaitTimeout` by hand;
    // here the sweep is pumped at an instant past the deadline instead of
    // sleeping until it.

    const TEST_GROUP_WAIT: StdDuration = StdDuration::from_millis(200);

    /// A manager whose groups wait `TEST_GROUP_WAIT`, with no sweep task.
    fn mgr_with_group_wait() -> (FerrofinSyncPlayManager, Arc<RecordingBus>) {
        let (m, bus) = mgr();
        (m.with_group_wait(TEST_GROUP_WAIT, None), bus)
    }

    /// Pumps the sweep once the group's wait has run out.
    fn after_the_wait(m: &FerrofinSyncPlayManager) {
        m.sweep_group_waits_at(Instant::now() + TEST_GROUP_WAIT + StdDuration::from_millis(1));
    }

    async fn state_of(
        m: &FerrofinSyncPlayManager,
        s: &SyncPlaySession,
        id: Uuid,
    ) -> GroupStateType {
        m.get_group(s, id).await.unwrap().state
    }

    impl FerrofinSyncPlayManager {
        /// Groups wait `timeout` for their members; the sweep runs every
        /// `sweep`, or never for `None`.
        fn with_group_wait(mut self, timeout: StdDuration, sweep: Option<StdDuration>) -> Self {
            self.group_wait_timeout = timeout;
            self.group_wait_sweep = sweep;
            self
        }

        /// One sweep as of `now`, delivered as the sweep task delivers it.
        fn sweep_group_waits_at(&self, now: Instant) {
            if let Some(deliveries) = sweep_group_waits(&self.registry, now, Utc::now()) {
                for (targets, from, outbound) in deliveries {
                    self.deliver(&targets, &from, &outbound);
                }
            }
        }
    }

    /// `SessionJoined_JoinerNeverReportsReady_GroupResumesWithoutIt`.
    #[tokio::test]
    async fn group_wait_resumes_without_a_joiner_that_never_reports_ready() {
        let (m, bus) = mgr_with_group_wait();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        let b = session("s2", "bob");
        m.join_group(&b, info.group_id).await.unwrap();
        let pli = start_playing(&m, &bus, &[&a, &b], 1).await;
        assert_eq!(
            state_of(&m, &a, info.group_id).await,
            GroupStateType::Playing
        );

        // A session joins while the group is playing: it pauses and waits.
        let joiner = session("s3", "joiner");
        let before_the_join = Instant::now();
        m.join_group(&joiner, info.group_id).await.unwrap();
        assert_eq!(
            state_of(&m, &a, info.group_id).await,
            GroupStateType::Waiting
        );

        // Nothing gives up before the deadline.
        m.sweep_group_waits_at(before_the_join);
        assert_eq!(
            state_of(&m, &a, info.group_id).await,
            GroupStateType::Waiting
        );

        // The joiner never reports ready; the group carries on without it.
        after_the_wait(&m);
        assert_eq!(
            state_of(&m, &a, info.group_id).await,
            GroupStateType::Playing
        );

        // A late Buffer from the session that missed the deadline does not
        // drag the group back into waiting.
        m.handle_request(&joiner, buffer_at(pli, 0)).await.unwrap();
        assert_eq!(
            state_of(&m, &a, info.group_id).await,
            GroupStateType::Playing
        );
    }

    /// `SessionJoined_GroupWasPaused_TimeoutLeavesTheGroupPaused`.
    #[tokio::test]
    async fn group_wait_leaves_a_paused_group_paused_where_it_was() {
        let (m, bus) = mgr_with_group_wait();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        let b = session("s2", "bob");
        m.join_group(&b, info.group_id).await.unwrap();
        start_playing(&m, &bus, &[&a, &b], 1).await;
        m.handle_request(&a, PlaybackRequest::Pause).await.unwrap();
        assert_eq!(
            state_of(&m, &a, info.group_id).await,
            GroupStateType::Paused
        );
        // The group has been sitting paused for a while before anyone joins.
        m.lock()
            .groups
            .get_mut(&info.group_id)
            .unwrap()
            .last_activity = Utc::now() - Duration::minutes(2);
        let position = m.lock().groups[&info.group_id].position_ticks;

        let joiner = session("s3", "joiner");
        m.join_group(&joiner, info.group_id).await.unwrap();
        assert_eq!(
            state_of(&m, &a, info.group_id).await,
            GroupStateType::Waiting
        );
        let before: Vec<usize> = ["s1", "s2", "s3"]
            .iter()
            .map(|s| bus.count_for(s))
            .collect();

        // A group that was paused does not start playing because a member
        // failed to report ready.
        after_the_wait(&m);
        assert_eq!(
            state_of(&m, &a, info.group_id).await,
            GroupStateType::Paused
        );
        // Nor does giving up on the joiner move its playback position.
        assert_eq!(m.lock().groups[&info.group_id].position_ticks, position);
        // Every member is told the group is no longer waiting.
        for (sid, from) in ["s1", "s2", "s3"].iter().zip(before) {
            let after = &bus.bodies_for(sid)[from..];
            assert!(after.iter().any(|x| is_cmd(x, "Pause")), "{sid}: {after:?}");
            assert!(
                after.iter().any(|x| is_state(x, "Paused", "Pause")),
                "{sid}: {after:?}"
            );
        }
    }

    /// `Ready_ReportedBeforeTheDeadline_GroupDoesNotGiveUpOnAnyone`.
    #[tokio::test]
    async fn group_wait_ends_when_everyone_reports_ready_in_time() {
        let (m, bus) = mgr_with_group_wait();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        let b = session("s2", "bob");
        m.join_group(&b, info.group_id).await.unwrap();
        let pli = start_playing(&m, &bus, &[&a, &b], 1).await;

        let joiner = session("s3", "joiner");
        m.join_group(&joiner, info.group_id).await.unwrap();
        assert_eq!(
            state_of(&m, &a, info.group_id).await,
            GroupStateType::Waiting
        );
        assert!(m.lock().groups[&info.group_id].wait_deadline.is_some());

        m.handle_request(&joiner, ready_at(pli, 0, true))
            .await
            .unwrap();

        // Everyone reported ready, so no deadline is left to trip.
        assert_eq!(
            state_of(&m, &a, info.group_id).await,
            GroupStateType::Playing
        );
        assert!(m.lock().groups[&info.group_id].wait_deadline.is_none());
        after_the_wait(&m);
        assert_eq!(
            state_of(&m, &a, info.group_id).await,
            GroupStateType::Playing
        );
    }

    /// `SetPlaylistItem_AfterATimeout_GroupWaitsForEveryoneAgain`.
    #[tokio::test]
    async fn group_wait_gives_up_only_until_the_group_changes_item() {
        let (m, bus) = mgr_with_group_wait();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        let b = session("s2", "bob");
        m.join_group(&b, info.group_id).await.unwrap();
        let pli = start_playing(&m, &bus, &[&a, &b], 1).await;

        let joiner = session("s3", "joiner");
        m.join_group(&joiner, info.group_id).await.unwrap();
        after_the_wait(&m);
        assert_eq!(
            state_of(&m, &a, info.group_id).await,
            GroupStateType::Playing
        );

        m.handle_request(
            &a,
            PlaybackRequest::SetPlaylistItem {
                playlist_item_id: pli,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            state_of(&m, &a, info.group_id).await,
            GroupStateType::Waiting
        );
        assert!(m.lock().groups[&info.group_id].wait_deadline.is_some());
    }

    /// Members that keep reporting buffering do not push the deadline back;
    /// a new waiting period (every member reset) restarts it.
    #[tokio::test]
    async fn group_wait_deadline_covers_the_whole_waiting_period() {
        let (m, bus) = mgr_with_group_wait();
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        let b = session("s2", "bob");
        m.join_group(&b, info.group_id).await.unwrap();
        let pli = start_playing(&m, &bus, &[&a, &b], 1).await;
        m.handle_request(&b, buffer_at(pli, 0)).await.unwrap();
        let deadline = m.lock().groups[&info.group_id].wait_deadline;
        assert!(deadline.is_some());

        m.handle_request(&b, buffer_at(pli, 0)).await.unwrap();
        assert_eq!(m.lock().groups[&info.group_id].wait_deadline, deadline);

        // A new item resets every member: a new waiting period, a new deadline.
        tokio::time::sleep(StdDuration::from_millis(2)).await;
        m.handle_request(
            &a,
            PlaybackRequest::SetPlaylistItem {
                playlist_item_id: pli,
            },
        )
        .await
        .unwrap();
        assert!(m.lock().groups[&info.group_id].wait_deadline > deadline);

        // `SetIgnoreWait` takes the blocking members out: no deadline once
        // nobody blocks.
        m.handle_request(&a, PlaybackRequest::IgnoreWait { ignore_wait: true })
            .await
            .unwrap();
        assert!(m.lock().groups[&info.group_id].wait_deadline.is_some());
        m.handle_request(&b, PlaybackRequest::IgnoreWait { ignore_wait: true })
            .await
            .unwrap();
        assert!(m.lock().groups[&info.group_id].wait_deadline.is_none());
    }

    /// The sweep task runs only while a group exists: it ends once the last
    /// one is gone, and the next new group starts it again.
    #[tokio::test]
    async fn group_wait_sweep_runs_only_while_a_group_exists() {
        let (m, _bus) = mgr();
        let m = m.with_group_wait(TEST_GROUP_WAIT, Some(StdDuration::from_millis(20)));
        let runtime = tokio::runtime::Handle::current();
        let a = session("s1", "alice");
        m.new_group(&a, "g").await.unwrap();
        assert!(m.lock().sweeping);
        assert_eq!(runtime.metrics().num_alive_tasks(), 1, "the sweep task");

        m.leave_group(&a).await.unwrap();
        assert!(m.lock().groups.is_empty());
        let until = Instant::now() + StdDuration::from_secs(10);
        while runtime.metrics().num_alive_tasks() > 0 && Instant::now() < until {
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }
        assert_eq!(runtime.metrics().num_alive_tasks(), 0, "the task ended");
        assert!(!m.lock().sweeping, "no group left, so the sweep stopped");

        m.new_group(&a, "g2").await.unwrap();
        assert!(m.lock().sweeping, "a new group starts it again");
        assert_eq!(runtime.metrics().num_alive_tasks(), 1);
    }

    /// `HandleRequest_GroupWaitsForAMemberThatNeverReportsReady_RecoversOnItsOwn`:
    /// the manager's own sweep, not a hand pump, brings the group out.
    #[tokio::test]
    async fn group_wait_sweep_recovers_a_group_on_its_own() {
        let (m, _bus) = mgr();
        let m = m.with_group_wait(TEST_GROUP_WAIT, Some(StdDuration::from_millis(20)));
        let a = session("s1", "alice");
        let info = m.new_group(&a, "g").await.unwrap();
        let b = session("s2", "bob");
        m.join_group(&b, info.group_id).await.unwrap();

        // Starting playback puts the group behind the ready barrier.
        m.handle_request(&a, play(1)).await.unwrap();
        assert_eq!(
            state_of(&m, &a, info.group_id).await,
            GroupStateType::Waiting
        );

        // Neither session reports ready; the group comes out by itself.
        let until = Instant::now() + StdDuration::from_secs(10);
        while state_of(&m, &a, info.group_id).await != GroupStateType::Playing
            && Instant::now() < until
        {
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }
        assert_eq!(
            state_of(&m, &a, info.group_id).await,
            GroupStateType::Playing
        );
    }
}
