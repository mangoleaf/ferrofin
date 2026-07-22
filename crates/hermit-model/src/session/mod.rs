//! Port of `MediaBrowser.Model.Session`.
//!
//! Playback control commands, the WebSocket message taxonomy, the
//! `TranscodeReason` flags, and the session request/state DTOs. On the wire a
//! set of transcode reasons is a JSON array of PascalCase strings, so
//! [`TranscodeReason`] is the serde/schema type; [`TranscodeReasons`] is the
//! internal `bitflags` bitmask the `StreamBuilder` accumulates.

use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

mod authentication_result;
mod client_capabilities;
mod general_command;
mod message_command;
mod playback_info;
mod player_state_info;
mod requests;
mod transcoding_info;

pub use authentication_result::AuthenticationResult;
pub use client_capabilities::ClientCapabilities;
pub use general_command::GeneralCommand;
pub use message_command::MessageCommand;
pub use playback_info::{
    PlaybackProgressInfo, PlaybackStartInfo, PlaybackStopInfo, QueueItem, SessionUserInfo,
    UserDataChangeInfo,
};
pub use player_state_info::PlayerStateInfo;
pub use requests::{BrowseRequest, PlayRequest, PlaystateRequest};
pub use transcoding_info::TranscodingInfo;

/// The play method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum PlayMethod {
    /// The media is transcoded before it is sent to the client.
    #[default]
    Transcode = 0,
    /// The media is remuxed into a compatible container without re-encoding.
    DirectStream = 1,
    /// The media is sent to the client as-is.
    DirectPlay = 2,
}

/// Enum `PlayCommand`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum PlayCommand {
    /// The play now.
    #[default]
    PlayNow = 0,
    /// The play next.
    PlayNext = 1,
    /// The play last.
    PlayLast = 2,
    /// The play instant mix.
    PlayInstantMix = 3,
    /// The play shuffle.
    PlayShuffle = 4,
}

/// Enum `PlaybackOrder`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum PlaybackOrder {
    /// Sorted playlist.
    #[default]
    Default = 0,
    /// Shuffled playlist.
    Shuffle = 1,
}

/// The repeat mode of a play queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum RepeatMode {
    /// Nothing is repeated.
    #[default]
    RepeatNone = 0,
    /// The whole queue is repeated.
    RepeatAll = 1,
    /// The current item is repeated.
    RepeatOne = 2,
}

/// Enum `PlaystateCommand`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum PlaystateCommand {
    /// The stop.
    #[default]
    Stop,
    /// The pause.
    Pause,
    /// The unpause.
    Unpause,
    /// The next track.
    NextTrack,
    /// The previous track.
    PreviousTrack,
    /// The seek.
    Seek,
    /// The rewind.
    Rewind,
    /// The fast forward.
    FastForward,
    /// The play/pause toggle.
    PlayPause,
}

/// A set of known remote-control commands a client can issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum GeneralCommandType {
    /// Move the focus up.
    MoveUp = 0,
    /// Move the focus down.
    MoveDown = 1,
    /// Move the focus left.
    MoveLeft = 2,
    /// Move the focus right.
    MoveRight = 3,
    /// Page up.
    PageUp = 4,
    /// Page down.
    PageDown = 5,
    /// Jump to the previous letter.
    PreviousLetter = 6,
    /// Jump to the next letter.
    NextLetter = 7,
    /// Toggle the on-screen display.
    ToggleOsd = 8,
    /// Toggle the context menu.
    ToggleContextMenu = 9,
    /// Select the focused item.
    Select = 10,
    /// Go back.
    Back = 11,
    /// Take a screenshot.
    TakeScreenshot = 12,
    /// Send a key.
    SendKey = 13,
    /// Send a string.
    SendString = 14,
    /// Go to the home screen.
    GoHome = 15,
    /// Go to settings.
    GoToSettings = 16,
    /// Increase the volume.
    VolumeUp = 17,
    /// Decrease the volume.
    VolumeDown = 18,
    /// Mute.
    Mute = 19,
    /// Unmute.
    Unmute = 20,
    /// Toggle mute.
    ToggleMute = 21,
    /// Set the volume to a specific level.
    SetVolume = 22,
    /// Set the audio stream index.
    SetAudioStreamIndex = 23,
    /// Set the subtitle stream index.
    SetSubtitleStreamIndex = 24,
    /// Toggle fullscreen.
    ToggleFullscreen = 25,
    /// Display content.
    DisplayContent = 26,
    /// Go to search.
    GoToSearch = 27,
    /// Display a message.
    DisplayMessage = 28,
    /// Set the repeat mode.
    SetRepeatMode = 29,
    /// Channel up.
    ChannelUp = 30,
    /// Channel down.
    ChannelDown = 31,
    /// Show the guide.
    Guide = 32,
    /// Toggle stats.
    ToggleStats = 33,
    /// Play a specific media source.
    PlayMediaSource = 34,
    /// Play trailers.
    PlayTrailers = 35,
    /// Set the shuffle-queue mode.
    SetShuffleQueue = 36,
    /// Report play state.
    PlayState = 37,
    /// Play the next item.
    PlayNext = 38,
    /// Toggle the on-screen-display menu.
    ToggleOsdMenu = 39,
    /// Play.
    Play = 40,
    /// Set the maximum streaming bitrate.
    SetMaxStreamingBitrate = 41,
    /// Set the playback order.
    SetPlaybackOrder = 42,
}

/// The different kinds of messages used in the WebSocket API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum SessionMessageType {
    /// Server → client: force a keep-alive.
    ForceKeepAlive,
    /// Server → client: a general command.
    GeneralCommand,
    /// Server → client: user data changed.
    UserDataChanged,
    /// Server → client: sessions update.
    Sessions,
    /// Server → client: play.
    Play,
    /// Server → client: sync-play command.
    SyncPlayCommand,
    /// Server → client: sync-play group update.
    SyncPlayGroupUpdate,
    /// Server → client: play state.
    Playstate,
    /// Server → client: restart required.
    RestartRequired,
    /// Server → client: server shutting down.
    ServerShuttingDown,
    /// Server → client: server restarting.
    ServerRestarting,
    /// Server → client: library changed.
    LibraryChanged,
    /// Server → client: user deleted.
    UserDeleted,
    /// Server → client: user updated.
    UserUpdated,
    /// Server → client: series timer created.
    SeriesTimerCreated,
    /// Server → client: timer created.
    TimerCreated,
    /// Server → client: series timer cancelled.
    SeriesTimerCancelled,
    /// Server → client: timer cancelled.
    TimerCancelled,
    /// Server → client: refresh progress.
    RefreshProgress,
    /// Server → client: scheduled task ended.
    ScheduledTaskEnded,
    /// Server → client: package installation cancelled.
    PackageInstallationCancelled,
    /// Server → client: package installation failed.
    PackageInstallationFailed,
    /// Server → client: package installation completed.
    PackageInstallationCompleted,
    /// Server → client: package installing.
    PackageInstalling,
    /// Server → client: package uninstalled.
    PackageUninstalled,
    /// Server → client: activity log entry.
    ActivityLogEntry,
    /// Server → client: scheduled tasks info.
    ScheduledTasksInfo,
    /// Client → server: start activity-log entries.
    ActivityLogEntryStart,
    /// Client → server: stop activity-log entries.
    ActivityLogEntryStop,
    /// Client → server: start sessions.
    SessionsStart,
    /// Client → server: stop sessions.
    SessionsStop,
    /// Client → server: start scheduled-tasks info.
    ScheduledTasksInfoStart,
    /// Client → server: stop scheduled-tasks info.
    ScheduledTasksInfoStop,
    /// Shared: keep-alive.
    KeepAlive,
}

/// A single reason the server chose to transcode rather than direct-play.
///
/// This is the wire representation: a set of reasons serializes as a JSON
/// array of these PascalCase strings. For the internal accumulating bitmask
/// used by the stream builder see [`TranscodeReasons`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum TranscodeReason {
    /// The container is not supported.
    ContainerNotSupported,
    /// The video codec is not supported.
    VideoCodecNotSupported,
    /// The audio codec is not supported.
    AudioCodecNotSupported,
    /// The subtitle codec is not supported.
    SubtitleCodecNotSupported,
    /// The audio is external.
    AudioIsExternal,
    /// Secondary audio is not supported.
    SecondaryAudioNotSupported,
    /// The video profile is not supported.
    VideoProfileNotSupported,
    /// The video level is not supported.
    VideoLevelNotSupported,
    /// The video resolution is not supported.
    VideoResolutionNotSupported,
    /// The video bit depth is not supported.
    VideoBitDepthNotSupported,
    /// The video framerate is not supported.
    VideoFramerateNotSupported,
    /// The reference frame count is not supported.
    RefFramesNotSupported,
    /// Anamorphic video is not supported.
    AnamorphicVideoNotSupported,
    /// Interlaced video is not supported.
    InterlacedVideoNotSupported,
    /// The audio channel count is not supported.
    AudioChannelsNotSupported,
    /// The audio profile is not supported.
    AudioProfileNotSupported,
    /// The audio sample rate is not supported.
    AudioSampleRateNotSupported,
    /// The audio bit depth is not supported.
    AudioBitDepthNotSupported,
    /// The container bitrate exceeds the limit.
    ContainerBitrateExceedsLimit,
    /// The video bitrate is not supported.
    VideoBitrateNotSupported,
    /// The audio bitrate is not supported.
    AudioBitrateNotSupported,
    /// The video stream info is unknown.
    UnknownVideoStreamInfo,
    /// The audio stream info is unknown.
    UnknownAudioStreamInfo,
    /// A direct-play error occurred.
    DirectPlayError,
    /// The video range type is not supported.
    VideoRangeTypeNotSupported,
    /// The video codec tag is not supported.
    VideoCodecTagNotSupported,
    /// The stream count exceeds the limit.
    StreamCountExceedsLimit,
    /// The video rotation is not supported.
    VideoRotationNotSupported,
}

bitflags! {
    /// Bitmask of [`TranscodeReason`]s, mirroring the C# `[Flags] enum`.
    ///
    /// Bit positions are taken verbatim from `MediaBrowser.Model.Session.
    /// TranscodeReason` so accumulated masks match the upstream stream-builder
    /// logic exactly.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct TranscodeReasons: u32 {
        /// The container is not supported.
        const CONTAINER_NOT_SUPPORTED = 1 << 0;
        /// The video codec is not supported.
        const VIDEO_CODEC_NOT_SUPPORTED = 1 << 1;
        /// The audio codec is not supported.
        const AUDIO_CODEC_NOT_SUPPORTED = 1 << 2;
        /// The subtitle codec is not supported.
        const SUBTITLE_CODEC_NOT_SUPPORTED = 1 << 3;
        /// The audio is external.
        const AUDIO_IS_EXTERNAL = 1 << 4;
        /// Secondary audio is not supported.
        const SECONDARY_AUDIO_NOT_SUPPORTED = 1 << 5;
        /// The stream count exceeds the limit.
        const STREAM_COUNT_EXCEEDS_LIMIT = 1 << 26;
        /// The video profile is not supported.
        const VIDEO_PROFILE_NOT_SUPPORTED = 1 << 6;
        /// The video range type is not supported.
        const VIDEO_RANGE_TYPE_NOT_SUPPORTED = 1 << 24;
        /// The video codec tag is not supported.
        const VIDEO_CODEC_TAG_NOT_SUPPORTED = 1 << 25;
        /// The video level is not supported.
        const VIDEO_LEVEL_NOT_SUPPORTED = 1 << 7;
        /// The video resolution is not supported.
        const VIDEO_RESOLUTION_NOT_SUPPORTED = 1 << 8;
        /// The video bit depth is not supported.
        const VIDEO_BIT_DEPTH_NOT_SUPPORTED = 1 << 9;
        /// The video framerate is not supported.
        const VIDEO_FRAMERATE_NOT_SUPPORTED = 1 << 10;
        /// The video rotation is not supported.
        const VIDEO_ROTATION_NOT_SUPPORTED = 1 << 27;
        /// The reference frame count is not supported.
        const REF_FRAMES_NOT_SUPPORTED = 1 << 11;
        /// Anamorphic video is not supported.
        const ANAMORPHIC_VIDEO_NOT_SUPPORTED = 1 << 12;
        /// Interlaced video is not supported.
        const INTERLACED_VIDEO_NOT_SUPPORTED = 1 << 13;
        /// The audio channel count is not supported.
        const AUDIO_CHANNELS_NOT_SUPPORTED = 1 << 14;
        /// The audio profile is not supported.
        const AUDIO_PROFILE_NOT_SUPPORTED = 1 << 15;
        /// The audio sample rate is not supported.
        const AUDIO_SAMPLE_RATE_NOT_SUPPORTED = 1 << 16;
        /// The audio bit depth is not supported.
        const AUDIO_BIT_DEPTH_NOT_SUPPORTED = 1 << 17;
        /// The container bitrate exceeds the limit.
        const CONTAINER_BITRATE_EXCEEDS_LIMIT = 1 << 18;
        /// The video bitrate is not supported.
        const VIDEO_BITRATE_NOT_SUPPORTED = 1 << 19;
        /// The audio bitrate is not supported.
        const AUDIO_BITRATE_NOT_SUPPORTED = 1 << 20;
        /// The video stream info is unknown.
        const UNKNOWN_VIDEO_STREAM_INFO = 1 << 21;
        /// The audio stream info is unknown.
        const UNKNOWN_AUDIO_STREAM_INFO = 1 << 22;
        /// A direct-play error occurred.
        const DIRECT_PLAY_ERROR = 1 << 23;
    }
}

impl From<TranscodeReason> for TranscodeReasons {
    /// Lifts a single [`TranscodeReason`] into its one-bit mask.
    fn from(reason: TranscodeReason) -> Self {
        match reason {
            TranscodeReason::ContainerNotSupported => Self::CONTAINER_NOT_SUPPORTED,
            TranscodeReason::VideoCodecNotSupported => Self::VIDEO_CODEC_NOT_SUPPORTED,
            TranscodeReason::AudioCodecNotSupported => Self::AUDIO_CODEC_NOT_SUPPORTED,
            TranscodeReason::SubtitleCodecNotSupported => Self::SUBTITLE_CODEC_NOT_SUPPORTED,
            TranscodeReason::AudioIsExternal => Self::AUDIO_IS_EXTERNAL,
            TranscodeReason::SecondaryAudioNotSupported => Self::SECONDARY_AUDIO_NOT_SUPPORTED,
            TranscodeReason::VideoProfileNotSupported => Self::VIDEO_PROFILE_NOT_SUPPORTED,
            TranscodeReason::VideoLevelNotSupported => Self::VIDEO_LEVEL_NOT_SUPPORTED,
            TranscodeReason::VideoResolutionNotSupported => Self::VIDEO_RESOLUTION_NOT_SUPPORTED,
            TranscodeReason::VideoBitDepthNotSupported => Self::VIDEO_BIT_DEPTH_NOT_SUPPORTED,
            TranscodeReason::VideoFramerateNotSupported => Self::VIDEO_FRAMERATE_NOT_SUPPORTED,
            TranscodeReason::RefFramesNotSupported => Self::REF_FRAMES_NOT_SUPPORTED,
            TranscodeReason::AnamorphicVideoNotSupported => Self::ANAMORPHIC_VIDEO_NOT_SUPPORTED,
            TranscodeReason::InterlacedVideoNotSupported => Self::INTERLACED_VIDEO_NOT_SUPPORTED,
            TranscodeReason::AudioChannelsNotSupported => Self::AUDIO_CHANNELS_NOT_SUPPORTED,
            TranscodeReason::AudioProfileNotSupported => Self::AUDIO_PROFILE_NOT_SUPPORTED,
            TranscodeReason::AudioSampleRateNotSupported => Self::AUDIO_SAMPLE_RATE_NOT_SUPPORTED,
            TranscodeReason::AudioBitDepthNotSupported => Self::AUDIO_BIT_DEPTH_NOT_SUPPORTED,
            TranscodeReason::ContainerBitrateExceedsLimit => Self::CONTAINER_BITRATE_EXCEEDS_LIMIT,
            TranscodeReason::VideoBitrateNotSupported => Self::VIDEO_BITRATE_NOT_SUPPORTED,
            TranscodeReason::AudioBitrateNotSupported => Self::AUDIO_BITRATE_NOT_SUPPORTED,
            TranscodeReason::UnknownVideoStreamInfo => Self::UNKNOWN_VIDEO_STREAM_INFO,
            TranscodeReason::UnknownAudioStreamInfo => Self::UNKNOWN_AUDIO_STREAM_INFO,
            TranscodeReason::DirectPlayError => Self::DIRECT_PLAY_ERROR,
            TranscodeReason::VideoRangeTypeNotSupported => Self::VIDEO_RANGE_TYPE_NOT_SUPPORTED,
            TranscodeReason::VideoCodecTagNotSupported => Self::VIDEO_CODEC_TAG_NOT_SUPPORTED,
            TranscodeReason::StreamCountExceedsLimit => Self::STREAM_COUNT_EXCEEDS_LIMIT,
            TranscodeReason::VideoRotationNotSupported => Self::VIDEO_ROTATION_NOT_SUPPORTED,
        }
    }
}

/// Transcode-reason flags paired with their PascalCase names, ordered by
/// ascending numeric bit value (matching C# `Enum.GetValues`).
const TRANSCODE_REASON_ORDERED_NAMES: &[(TranscodeReasons, &str)] = &[
    (
        TranscodeReasons::CONTAINER_NOT_SUPPORTED,
        "ContainerNotSupported",
    ),
    (
        TranscodeReasons::VIDEO_CODEC_NOT_SUPPORTED,
        "VideoCodecNotSupported",
    ),
    (
        TranscodeReasons::AUDIO_CODEC_NOT_SUPPORTED,
        "AudioCodecNotSupported",
    ),
    (
        TranscodeReasons::SUBTITLE_CODEC_NOT_SUPPORTED,
        "SubtitleCodecNotSupported",
    ),
    (TranscodeReasons::AUDIO_IS_EXTERNAL, "AudioIsExternal"),
    (
        TranscodeReasons::SECONDARY_AUDIO_NOT_SUPPORTED,
        "SecondaryAudioNotSupported",
    ),
    (
        TranscodeReasons::VIDEO_PROFILE_NOT_SUPPORTED,
        "VideoProfileNotSupported",
    ),
    (
        TranscodeReasons::VIDEO_LEVEL_NOT_SUPPORTED,
        "VideoLevelNotSupported",
    ),
    (
        TranscodeReasons::VIDEO_RESOLUTION_NOT_SUPPORTED,
        "VideoResolutionNotSupported",
    ),
    (
        TranscodeReasons::VIDEO_BIT_DEPTH_NOT_SUPPORTED,
        "VideoBitDepthNotSupported",
    ),
    (
        TranscodeReasons::VIDEO_FRAMERATE_NOT_SUPPORTED,
        "VideoFramerateNotSupported",
    ),
    (
        TranscodeReasons::REF_FRAMES_NOT_SUPPORTED,
        "RefFramesNotSupported",
    ),
    (
        TranscodeReasons::ANAMORPHIC_VIDEO_NOT_SUPPORTED,
        "AnamorphicVideoNotSupported",
    ),
    (
        TranscodeReasons::INTERLACED_VIDEO_NOT_SUPPORTED,
        "InterlacedVideoNotSupported",
    ),
    (
        TranscodeReasons::AUDIO_CHANNELS_NOT_SUPPORTED,
        "AudioChannelsNotSupported",
    ),
    (
        TranscodeReasons::AUDIO_PROFILE_NOT_SUPPORTED,
        "AudioProfileNotSupported",
    ),
    (
        TranscodeReasons::AUDIO_SAMPLE_RATE_NOT_SUPPORTED,
        "AudioSampleRateNotSupported",
    ),
    (
        TranscodeReasons::AUDIO_BIT_DEPTH_NOT_SUPPORTED,
        "AudioBitDepthNotSupported",
    ),
    (
        TranscodeReasons::CONTAINER_BITRATE_EXCEEDS_LIMIT,
        "ContainerBitrateExceedsLimit",
    ),
    (
        TranscodeReasons::VIDEO_BITRATE_NOT_SUPPORTED,
        "VideoBitrateNotSupported",
    ),
    (
        TranscodeReasons::AUDIO_BITRATE_NOT_SUPPORTED,
        "AudioBitrateNotSupported",
    ),
    (
        TranscodeReasons::UNKNOWN_VIDEO_STREAM_INFO,
        "UnknownVideoStreamInfo",
    ),
    (
        TranscodeReasons::UNKNOWN_AUDIO_STREAM_INFO,
        "UnknownAudioStreamInfo",
    ),
    (TranscodeReasons::DIRECT_PLAY_ERROR, "DirectPlayError"),
    (
        TranscodeReasons::VIDEO_RANGE_TYPE_NOT_SUPPORTED,
        "VideoRangeTypeNotSupported",
    ),
    (
        TranscodeReasons::VIDEO_CODEC_TAG_NOT_SUPPORTED,
        "VideoCodecTagNotSupported",
    ),
    (
        TranscodeReasons::STREAM_COUNT_EXCEEDS_LIMIT,
        "StreamCountExceedsLimit",
    ),
    (
        TranscodeReasons::VIDEO_ROTATION_NOT_SUPPORTED,
        "VideoRotationNotSupported",
    ),
];

/// Returns the PascalCase names of the flags set in `reasons`, ordered by
/// ascending bit position.
///
/// Mirrors C# `TranscodeReasons.GetUniqueFlags()` followed by
/// `Enum.ToString()`, which the stream-builder joins with `,` when writing the
/// `TranscodeReasons` query parameter.
#[must_use]
pub fn transcode_reasons_unique_names(reasons: TranscodeReasons) -> Vec<&'static str> {
    TRANSCODE_REASON_ORDERED_NAMES
        .iter()
        .filter(|(flag, _)| reasons.contains(*flag))
        .map(|(_, name)| *name)
        .collect()
}
