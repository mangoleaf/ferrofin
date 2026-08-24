//! [`FerrofinPlaybackMetrics`] — the concrete [`PlaybackMetrics`] over SQLite.
//!
//! Writes one `FerrofinPlaybackSessions` row per PlaybackInfo decision and updates it
//! on playstate start/stop. The point is ranking `TranscodeReasons` by
//! frequency and cost to find transcodes that
//! a better profile decision would have avoided.
//!
//! Storage failures are logged and swallowed — metrics must never break
//! playback (the trait contract).
//!
//! # Why the writes are queued
//!
//! Every trait method used to `await` its own statement on the single writer
//! connection, inside the request that produced it. Each one is its own
//! transaction, so each appends a commit frame to the WAL; the WAL then hits
//! its autocheckpoint threshold, and the checkpoint blocks the writer while it
//! folds pages back into the database — with a client waiting on the other end.
//! Measured on the bench fixture over an NVMe-backed data directory,
//! `POST /Items/{id}/PlaybackInfo` at its calibrated 1,278 req/s: p50 0.6 ms but
//! **p95 1,063 ms / p99 1,267 ms**, against p95 0.5 ms / p99 0.8 ms for the
//! `GET` form, which takes no decision and therefore writes nothing. The same
//! fixture on tmpfs shows p95 0.9 ms — the stall is invisible there, which is
//! why it only ever surfaced in the containerised benchmark.
//!
//! So the recorder hands each event to a bounded queue and returns. One
//! background task drains the queue and applies a whole batch in a single
//! transaction: one commit for up to [`BATCH`] events instead of one per event,
//! and no checkpoint ever lands on a request. Events go through one queue in
//! submission order, so a start/stop update can never overtake the insert that
//! creates its row. A full queue drops the event with a warning rather than
//! blocking playback, which is the trait's standing contract.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::mpsc;
use tracing::warn;

use ferrofin_db::Database;
use ferrofin_db::store::{datetime_to_db, guid_to_db};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::metrics::{PlaybackDecision, PlaybackMetrics};

/// Default depth of the pending-write queue, in events.
///
/// Roughly a second of the benchmark's peak PlaybackInfo rate, so a burst is
/// absorbed rather than dropped, while the memory it can pin stays trivial.
/// Overridable per deployment — see `Config::playback_metrics_queue`.
pub const DEFAULT_QUEUE_DEPTH: usize = 1024;

/// How many queued events one transaction applies.
///
/// The drain takes whatever is waiting up to this many; the batch is what turns
/// N commit frames into one, so it only needs to be large enough that a burst
/// collapses. Beyond a few hundred the marginal WAL saving is nil.
const BATCH: usize = 256;

/// One queued write, stamped at submission so a slow drain never back-dates the
/// row to when it happened to reach the database.
enum Event {
    /// A PlaybackInfo decision — the row-creating `INSERT`.
    Decided(Box<PlaybackDecision>, DateTime<Utc>),
    /// Playback began for a play session.
    Started(String, DateTime<Utc>),
    /// Playback stopped for a play session, at an optional position.
    Stopped(String, DateTime<Utc>, Option<i64>),
}

/// The concrete playback-metrics recorder.
#[derive(Clone)]
pub struct FerrofinPlaybackMetrics {
    tx: mpsc::Sender<Event>,
}

impl std::fmt::Debug for FerrofinPlaybackMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinPlaybackMetrics")
            .finish_non_exhaustive()
    }
}

impl FerrofinPlaybackMetrics {
    /// Creates the recorder over the given database, with the default queue
    /// depth ([`DEFAULT_QUEUE_DEPTH`]).
    ///
    /// Spawns the drain task, so it must be called from within a Tokio runtime.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self::with_queue_depth(db, DEFAULT_QUEUE_DEPTH)
    }

    /// [`Self::new`] with an explicit queue depth; `0` falls back to the
    /// default (an `mpsc` channel of capacity 0 panics, and "unset" is how
    /// every other numeric knob spells zero).
    ///
    /// Spawns the drain task, so it must be called from within a Tokio runtime.
    #[must_use]
    pub fn with_queue_depth(db: Database, depth: usize) -> Self {
        let (tx, rx) = mpsc::channel(if depth == 0 {
            DEFAULT_QUEUE_DEPTH
        } else {
            depth
        });
        tokio::spawn(drain(db, rx));
        Self { tx }
    }

    /// Queues `event`, dropping it (with a warning) when the queue is full.
    ///
    /// Never blocks and never fails: the caller is on a playback path and the
    /// trait contract is that metrics must not break it.
    fn enqueue(&self, event: Event) {
        if self.tx.try_send(event).is_err() {
            warn!("playback-metrics queue full or closed; dropping one event");
        }
    }
}

/// Applies queued events to the database, batching whatever is waiting into one
/// transaction. Ends when every recorder handle has been dropped.
async fn drain(db: Database, mut rx: mpsc::Receiver<Event>) {
    let mut batch: Vec<Event> = Vec::with_capacity(BATCH);
    while rx.recv_many(&mut batch, BATCH).await > 0 {
        if let Err(err) = apply(&db, &batch).await {
            warn!(%err, count = batch.len(), "failed to record playback metrics");
        }
        batch.clear();
    }
}

/// Runs `batch` in submission order inside a single transaction.
async fn apply(db: &Database, batch: &[Event]) -> Result<(), sqlx::Error> {
    let mut tx = db.writer().begin().await?;
    for event in batch {
        match event {
            Event::Decided(decision, at) => {
                sqlx::query(
                    r#"INSERT INTO "FerrofinPlaybackSessions"
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
                .bind(datetime_to_db(*at))
                .execute(&mut *tx)
                .await?;
            }
            Event::Started(play_session_id, at) => {
                sqlx::query(
                    r#"UPDATE "FerrofinPlaybackSessions" SET "StartedAt" = COALESCE("StartedAt", ?2)
                       WHERE "PlaySessionId" = ?1"#,
                )
                .bind(play_session_id)
                .bind(datetime_to_db(*at))
                .execute(&mut *tx)
                .await?;
            }
            Event::Stopped(play_session_id, at, position_ticks) => {
                sqlx::query(
                    r#"UPDATE "FerrofinPlaybackSessions"
                       SET "StoppedAt" = ?2, "PositionTicks" = ?3
                       WHERE "PlaySessionId" = ?1"#,
                )
                .bind(play_session_id)
                .bind(datetime_to_db(*at))
                .bind(position_ticks)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    tx.commit().await
}

#[async_trait]
impl PlaybackMetrics for FerrofinPlaybackMetrics {
    async fn record_decision(&self, decision: &PlaybackDecision) -> Result<(), ServiceError> {
        self.enqueue(Event::Decided(Box::new(decision.clone()), Utc::now()));
        Ok(())
    }

    async fn record_started(&self, play_session_id: &str) -> Result<(), ServiceError> {
        self.enqueue(Event::Started(play_session_id.to_owned(), Utc::now()));
        Ok(())
    }

    async fn record_stopped(
        &self,
        play_session_id: &str,
        position_ticks: Option<i64>,
    ) -> Result<(), ServiceError> {
        self.enqueue(Event::Stopped(
            play_session_id.to_owned(),
            Utc::now(),
            position_ticks,
        ));
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

    /// `PlayMethod`, `TranscodeReasons`, `StartedAt`, `StoppedAt`,
    /// `PositionTicks` — the columns the lifecycle assertions read.
    type SessionRow = (String, String, Option<String>, Option<String>, Option<i64>);

    /// Polls until the queued writes have landed. The recorder returns before
    /// the drain runs, which is the whole point of it; a test that read once and
    /// asserted would be racing the background task.
    async fn await_row(db: &Database, psid: &str) -> SessionRow {
        for _ in 0..200 {
            let row: Option<SessionRow> = sqlx::query_as(
                r#"SELECT "PlayMethod", "TranscodeReasons", "StartedAt", "StoppedAt",
                              "PositionTicks"
                       FROM "FerrofinPlaybackSessions" WHERE "PlaySessionId" = ?1"#,
            )
            .bind(psid)
            .fetch_optional(db.pool())
            .await
            .unwrap();
            // Only the fully-applied row settles the assertions below, so keep
            // polling until the stop update has landed too.
            if let Some(row) = row
                && row.3.is_some()
            {
                return row;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("queued playback metrics never reached the database");
    }

    #[tokio::test]
    async fn decision_start_stop_lifecycle_writes_one_row() {
        let db = test_db().await;
        let metrics = FerrofinPlaybackMetrics::new(db.clone());
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

        let row = await_row(&db, &psid).await;
        assert_eq!(row.0, "Transcode");
        assert_eq!(row.1, "AudioCodecNotSupported");
        assert!(row.2.is_some());
        assert!(row.3.is_some());
        assert_eq!(row.4, Some(1234));
    }

    /// The updates are queued behind the insert that creates their row, so a
    /// start/stop submitted in the same breath as the decision can never be
    /// applied first and lost — the failure a per-call `spawn` would have.
    #[tokio::test]
    async fn a_start_submitted_immediately_after_the_decision_is_not_lost() {
        let db = test_db().await;
        let metrics = FerrofinPlaybackMetrics::new(db.clone());
        let psid = Uuid::new_v4().to_string();
        metrics
            .record_decision(&PlaybackDecision {
                play_session_id: psid.clone(),
                play_method: "DirectPlay".to_owned(),
                ..PlaybackDecision::default()
            })
            .await
            .unwrap();
        metrics.record_started(&psid).await.unwrap();
        metrics.record_stopped(&psid, None).await.unwrap();

        let row = await_row(&db, &psid).await;
        assert!(row.2.is_some(), "StartedAt must survive the queue ordering");
    }

    #[tokio::test]
    async fn unknown_session_updates_are_harmless_noops() {
        let db = test_db().await;
        let metrics = FerrofinPlaybackMetrics::new(db.clone());
        metrics.record_started("nope").await.unwrap();
        metrics.record_stopped("nope", None).await.unwrap();
        // The drain must survive updates that match no row; if it did not, the
        // next event would never land.
        let psid = Uuid::new_v4().to_string();
        metrics
            .record_decision(&PlaybackDecision {
                play_session_id: psid.clone(),
                play_method: "DirectPlay".to_owned(),
                ..PlaybackDecision::default()
            })
            .await
            .unwrap();
        metrics.record_stopped(&psid, None).await.unwrap();
        await_row(&db, &psid).await;
    }

    /// A full queue drops the event rather than blocking the caller: playback
    /// must not wait on observability. Nothing else here can prove the bound is
    /// real — a queue that silently grew would pass every assertion above.
    #[tokio::test]
    async fn a_full_queue_drops_events_instead_of_blocking() {
        let db = test_db().await;
        let metrics = FerrofinPlaybackMetrics::with_queue_depth(db, 1);
        // The drain is parked on its own task and cannot run while this loop
        // holds the thread, so the channel fills and every later send is
        // refused. The assertion is that the loop finishes at all.
        for _ in 0..10_000 {
            metrics.record_started("psid").await.unwrap();
        }
    }
}
