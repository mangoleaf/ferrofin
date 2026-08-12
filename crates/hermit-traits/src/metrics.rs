//! [`PlaybackMetrics`] — records playback decisions for the metrics track.
//!
//! Not a port of a Jellyfin interface: upstream throws the per-request
//! `StreamInfo` decision away, which is exactly why avoidable transcodes are
//! invisible there. Hermit records every PlaybackInfo decision (play method +
//! `TranscodeReasons`) into the `HermitPlaybackSessions` table so transcode
//! causes can be ranked by cost.
//!
//! Recording must never break playback: implementations swallow and log their
//! own storage errors; the methods only fail on programmer error.

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::ServiceError;

/// One playback decision, as computed while answering `PlaybackInfo`.
#[derive(Debug, Clone, Default)]
pub struct PlaybackDecision {
    /// The `PlaySessionId` minted for the PlaybackInfo response (the client
    /// threads it through every playstate report).
    pub play_session_id: String,
    /// The item the decision is for.
    pub item_id: Uuid,
    /// The requesting user.
    pub user_id: Uuid,
    /// The client/app name from the authorization header.
    pub client: Option<String>,
    /// The device id from the authorization header.
    pub device_id: Option<String>,
    /// The final decision sent to the client: `DirectPlay` | `DirectStream` |
    /// `Transcode`.
    pub play_method: String,
    /// Comma-separated `TranscodeReason` names; empty for direct play.
    pub transcode_reasons: String,
    /// The source container.
    pub container: Option<String>,
    /// The source video codec.
    pub video_codec: Option<String>,
    /// The source audio codec.
    pub audio_codec: Option<String>,
    /// The negotiated target container (transcode only).
    pub target_container: Option<String>,
    /// The negotiated target video codec (transcode only).
    pub target_video_codec: Option<String>,
    /// The negotiated target audio codec (transcode only).
    pub target_audio_codec: Option<String>,
}

/// Records playback decisions and their lifecycle into the metrics store.
#[async_trait]
pub trait PlaybackMetrics: Send + Sync {
    /// Records a fresh PlaybackInfo decision (one row per play session).
    async fn record_decision(&self, decision: &PlaybackDecision) -> Result<(), ServiceError>;

    /// Marks the session as actually started (first playstate start report).
    async fn record_started(&self, play_session_id: &str) -> Result<(), ServiceError>;

    /// Marks the session stopped, with the final playback position.
    async fn record_stopped(
        &self,
        play_session_id: &str,
        position_ticks: Option<i64>,
    ) -> Result<(), ServiceError>;
}

fn _assert_object_safe_playback_metrics(_: &dyn PlaybackMetrics) {}
