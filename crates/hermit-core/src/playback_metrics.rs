//! [`HermitPlaybackMetrics`] — the concrete [`PlaybackMetrics`] over SQLite.
//!
//! Writes one `PlaybackSessions` row per PlaybackInfo decision and updates it
//! on playstate start/stop. See `brain/PLAN_PERFORMANCE.md` Track A: the point
//! is ranking `TranscodeReasons` by frequency and cost to find transcodes that
//! a better profile decision would have avoided.
//!
//! Storage failures are logged and swallowed — metrics must never break
//! playback (the trait contract).

use async_trait::async_trait;
use chrono::Utc;
use tracing::warn;

use hermit_db::Database;
use hermit_db::store::{datetime_to_db, guid_to_db};
use hermit_traits::error::ServiceError;
use hermit_traits::metrics::{PlaybackDecision, PlaybackMetrics};

/// The concrete playback-metrics recorder.
#[derive(Clone)]
pub struct HermitPlaybackMetrics {
    db: Database,
}

impl std::fmt::Debug for HermitPlaybackMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitPlaybackMetrics")
            .finish_non_exhaustive()
    }
}

impl HermitPlaybackMetrics {
    /// Creates the recorder over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl PlaybackMetrics for HermitPlaybackMetrics {
    async fn record_decision(&self, decision: &PlaybackDecision) -> Result<(), ServiceError> {
        let result = sqlx::query(
            r#"INSERT INTO "PlaybackSessions"
               ("PlaySessionId", "ItemId", "UserId", "Client", "DeviceId",
                "PlayMethod", "TranscodeReasons", "Container", "VideoCodec",
                "AudioCodec", "TargetContainer", "TargetVideoCodec",
                "TargetAudioCodec", "DecidedAt")
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
               ON CONFLICT("PlaySessionId") DO NOTHING"#,
        )
        .bind(&decision.play_session_id)
        .bind(guid_to_db(decision.item_id))
        .bind(guid_to_db(decision.user_id))
        .bind(&decision.client)
        .bind(&decision.device_id)
        .bind(&decision.play_method)
        .bind(&decision.transcode_reasons)
        .bind(&decision.container)
        .bind(&decision.video_codec)
        .bind(&decision.audio_codec)
        .bind(&decision.target_container)
        .bind(&decision.target_video_codec)
        .bind(&decision.target_audio_codec)
        .bind(datetime_to_db(Utc::now()))
        .execute(self.db.writer())
        .await;
        if let Err(err) = result {
            warn!(%err, "failed to record playback decision");
        }
        Ok(())
    }

    async fn record_started(&self, play_session_id: &str) -> Result<(), ServiceError> {
        let result = sqlx::query(
            r#"UPDATE "PlaybackSessions" SET "StartedAt" = COALESCE("StartedAt", ?2)
               WHERE "PlaySessionId" = ?1"#,
        )
        .bind(play_session_id)
        .bind(datetime_to_db(Utc::now()))
        .execute(self.db.writer())
        .await;
        if let Err(err) = result {
            warn!(%err, "failed to record playback start");
        }
        Ok(())
    }

    async fn record_stopped(
        &self,
        play_session_id: &str,
        position_ticks: Option<i64>,
    ) -> Result<(), ServiceError> {
        let result = sqlx::query(
            r#"UPDATE "PlaybackSessions"
               SET "StoppedAt" = ?2, "PositionTicks" = ?3
               WHERE "PlaySessionId" = ?1"#,
        )
        .bind(play_session_id)
        .bind(datetime_to_db(Utc::now()))
        .bind(position_ticks)
        .execute(self.db.writer())
        .await;
        if let Err(err) = result {
            warn!(%err, "failed to record playback stop");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    async fn test_db() -> Database {
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        db
    }

    #[tokio::test]
    async fn decision_start_stop_lifecycle_writes_one_row() {
        let db = test_db().await;
        let metrics = HermitPlaybackMetrics::new(db.clone());
        let psid = Uuid::new_v4().to_string();

        metrics
            .record_decision(&PlaybackDecision {
                play_session_id: psid.clone(),
                item_id: Uuid::new_v4(),
                user_id: Uuid::new_v4(),
                client: Some("Jellyfin Web".to_owned()),
                play_method: "Transcode".to_owned(),
                transcode_reasons: "AudioCodecNotSupported".to_owned(),
                container: Some("mkv".to_owned()),
                video_codec: Some("hevc".to_owned()),
                audio_codec: Some("eac3".to_owned()),
                target_container: Some("mp4".to_owned()),
                target_audio_codec: Some("aac".to_owned()),
                ..PlaybackDecision::default()
            })
            .await
            .unwrap();
        metrics.record_started(&psid).await.unwrap();
        metrics.record_stopped(&psid, Some(1234)).await.unwrap();

        let row: (String, String, Option<String>, Option<String>, Option<i64>) = sqlx::query_as(
            r#"SELECT "PlayMethod", "TranscodeReasons", "StartedAt", "StoppedAt",
                          "PositionTicks"
                   FROM "PlaybackSessions" WHERE "PlaySessionId" = ?1"#,
        )
        .bind(&psid)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(row.0, "Transcode");
        assert_eq!(row.1, "AudioCodecNotSupported");
        assert!(row.2.is_some());
        assert!(row.3.is_some());
        assert_eq!(row.4, Some(1234));
    }

    #[tokio::test]
    async fn unknown_session_updates_are_harmless_noops() {
        let db = test_db().await;
        let metrics = HermitPlaybackMetrics::new(db);
        metrics.record_started("nope").await.unwrap();
        metrics.record_stopped("nope", None).await.unwrap();
    }
}
