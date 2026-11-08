//! WAL checkpoints taken off the writer connection.
//!
//! SQLite checkpoints a WAL database automatically: once a commit leaves the
//! log at its `wal_autocheckpoint` threshold (1,000 frames), that commit folds
//! the log back into the database file before it returns, and syncs both
//! files. The commit runs on [`Database::writer`](crate::Database::writer), the
//! one connection every write queues on, so each write behind it waits out the
//! copy and the syncs. In a memory-limited container, where the kernel throttles
//! a burst of dirty pages, that took up to 2 s, and in the v1.5.0 ramp gate
//! `POST /Sessions/Playing` stalled with it (gl-1245). Under `synchronous=NORMAL`
//! a WAL commit never syncs by itself: the checkpoint is the only thing a write
//! waits on the disk for.
//!
//! So the writer keeps SQLite's threshold but gives the work away. Its WAL hook
//! ([`WalCheckpoints::take_over`]) only wakes [`run`], which checkpoints on a
//! connection of its own. A `PASSIVE` checkpoint takes the checkpointer lock
//! and never the writer's, so commits go on appending to the log while it
//! copies pages back and syncs. The sync points and the threshold are SQLite's
//! own. The one difference is when a commit returns: the commit that crosses
//! the threshold no longer waits for the checkpoint's syncs, which
//! `synchronous=NORMAL` never promised it would.

use std::ffi::{c_char, c_int, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use libsqlite3_sys as ffi;
use sqlx::SqlitePool;
use tokio::sync::{Mutex, Notify};

/// What the writer's WAL hook, the checkpoint task and
/// [`Database::truncate_wal`](crate::Database::truncate_wal) share, one per
/// [`Database`](crate::Database).
#[derive(Debug, Default)]
pub(crate) struct WalCheckpoints {
    /// The writer connection's auto-checkpoint threshold, in log frames, read
    /// before its hook replaces SQLite's; `0` (never) until then.
    threshold: AtomicI32,
    /// Notified by a commit that leaves the log at or past `threshold`. One
    /// that comes while a checkpoint runs is kept, so the commits made meanwhile
    /// get one more pass.
    due: Notify,
    /// One checkpoint at a time: a pass of [`run`] or a truncation. SQLite
    /// answers a second checkpoint busy instead of waiting for the first, and a
    /// pool's acquire gives up after its timeout, which a checkpoint of a large
    /// database outlasts. This waits for as long as the checkpoint takes.
    pub(crate) turn: Mutex<()>,
}

impl WalCheckpoints {
    /// Moves `conn`'s automatic checkpoints to [`run`]: reads its
    /// `wal_autocheckpoint` threshold, then installs the WAL hook that, instead
    /// of checkpointing inside the commit, notifies the task. Called on every
    /// writer connection the pool opens.
    ///
    /// `self` must outlive the connection: the writer pool's connect callback
    /// owns it, and the pool outlives every connection it opens.
    pub(crate) async fn take_over(
        self: &Arc<Self>,
        conn: &mut sqlx::SqliteConnection,
    ) -> Result<(), sqlx::Error> {
        // Read first: once the hook is ours this pragma answers 0.
        let (threshold,): (i32,) = sqlx::query_as("PRAGMA wal_autocheckpoint")
            .fetch_one(&mut *conn)
            .await?;
        self.threshold.store(threshold, Ordering::Relaxed);
        let mut handle = conn.lock_handle().await?;
        // SAFETY: the handle is the open connection, locked to this thread for
        // the call. The context pointer is `self`, which the writer pool's
        // connect callback keeps alive for as long as the pool, and so this
        // connection, exists; SQLite only reads it from `committed`.
        unsafe {
            ffi::sqlite3_wal_hook(
                handle.as_raw_handle().as_ptr(),
                Some(committed),
                Arc::as_ptr(self).cast_mut().cast::<c_void>(),
            );
        }
        Ok(())
    }
}

/// The writer's WAL hook (`sqlite3_wal_hook`): SQLite calls it on the
/// connection's thread after each commit, with the frames now in the log. It
/// only notifies the checkpoint task where SQLite's own hook would have
/// checkpointed on the spot.
unsafe extern "C" fn committed(
    ctx: *mut c_void,
    _db: *mut ffi::sqlite3,
    _schema: *const c_char,
    frames: c_int,
) -> c_int {
    // SAFETY: `ctx` is the `WalCheckpoints` `take_over` registered, alive while
    // the connection is (see there); it is only read, through an atomic and
    // `Notify`, which are `Sync`.
    let checkpoints = unsafe { &*ctx.cast::<WalCheckpoints>() };
    let threshold = checkpoints.threshold.load(Ordering::Relaxed);
    if threshold > 0 && frames >= threshold {
        checkpoints.due.notify_one();
    }
    ffi::SQLITE_OK
}

/// Runs the checkpoints the writer's commits ask for, one at a time on `pool`'s
/// connection, until `pool` closes.
///
/// A failure is retried by the next commit past the threshold. It is logged
/// once when checkpoints start failing, not once per commit.
pub(crate) async fn run(pool: SqlitePool, checkpoints: Arc<WalCheckpoints>) {
    let mut closed = pool.close_event();
    let mut failing = false;
    loop {
        tokio::select! {
            () = &mut closed => return,
            () = checkpoints.due.notified() => {}
        }
        let _turn = checkpoints.turn.lock().await;
        match sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
            .execute(&pool)
            .await
        {
            Ok(_) => failing = false,
            Err(_) if pool.is_closed() => return,
            Err(error) if failing => {
                tracing::debug!(%error, "WAL checkpoint still failing");
            }
            Err(error) => {
                failing = true;
                tracing::warn!(
                    %error,
                    "WAL checkpoint failed; the next commit past the threshold retries it"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::Database;

    /// The checkpoint sequence number in the WAL file's header (offset 12,
    /// big-endian, SQLite's documented WAL format). It goes up each time the
    /// log restarts, which a commit only does once a checkpoint has folded the
    /// whole log back, so it shows whether anyone checkpointed, without
    /// running a checkpoint to look.
    fn log_restarts(dir: &std::path::Path) -> u32 {
        let mut header = [0_u8; 16];
        std::fs::File::open(dir.join("live.db-wal"))
            .and_then(|mut wal| wal.read_exact(&mut header))
            .expect("read the WAL header");
        u32::from_be_bytes([header[12], header[13], header[14], header[15]])
    }

    async fn file_db(dir: &std::path::Path) -> Database {
        let url = format!("sqlite://{}", dir.join("live.db").display());
        let db = Database::connect_sized(&url, Some(1))
            .await
            .expect("file connect");
        sqlx::query("CREATE TABLE filler (data BLOB)")
            .execute(db.writer())
            .await
            .expect("create");
        db
    }

    /// Commits about `frames` log frames through the writer, in 1 MiB rows.
    async fn write_frames(db: &Database, frames: usize) {
        for _ in 0..frames.div_ceil(256) {
            sqlx::query("INSERT INTO filler VALUES (randomblob(1048576))")
                .execute(db.writer())
                .await
                .expect("insert");
        }
    }

    async fn writer_threshold(db: &Database) -> i32 {
        let (threshold,): (i32,) = sqlx::query_as("PRAGMA wal_autocheckpoint")
            .fetch_one(db.writer())
            .await
            .expect("pragma");
        threshold
    }

    /// A commit past the threshold leaves the checkpoint to the task: with the
    /// task's connection taken, nobody checkpoints, however long the log
    /// grows. SQLite's own hook would have checkpointed inside the commit and
    /// restarted the log.
    #[tokio::test]
    async fn the_writer_never_checkpoints_inside_a_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = file_db(dir.path()).await;
        let held = db
            .checkpointer
            .acquire()
            .await
            .expect("take the task's connection");
        let before = log_restarts(dir.path());
        write_frames(&db, 3_000).await;
        assert_eq!(
            log_restarts(dir.path()),
            before,
            "the writer checkpointed inside a commit"
        );
        drop(held);
        assert_eq!(
            writer_threshold(&db).await,
            0,
            "the writer runs no automatic checkpoint"
        );
    }

    /// The pool replaces a writer connection that closes (its lifetime or idle
    /// timeout ran out), and the new one hands its checkpoints over too.
    #[tokio::test]
    async fn a_reopened_writer_connection_hands_its_checkpoints_over_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = file_db(dir.path()).await;
        let mut first = db.writer().acquire().await.expect("the writer");
        sqlx::query("CREATE TEMP TABLE first_connection (x)")
            .execute(&mut *first)
            .await
            .expect("mark the connection");
        first.close().await.expect("close it");

        let mut second = db.writer().acquire().await.expect("a new writer");
        let (marks,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM temp.sqlite_master WHERE name = 'first_connection'",
        )
        .fetch_one(&mut *second)
        .await
        .expect("look for the mark");
        assert_eq!(marks, 0, "a new connection, not the first one");
        drop(second);
        assert_eq!(writer_threshold(&db).await, 0);
    }

    /// Given its connection, the task checkpoints the log the commits woke it
    /// for, and the next commit restarts it.
    #[tokio::test]
    async fn the_task_checkpoints_once_the_log_passes_the_threshold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = file_db(dir.path()).await;
        let before = log_restarts(dir.path());
        write_frames(&db, 1_500).await;
        let restarted = tokio::time::timeout(Duration::from_secs(10), async {
            while log_restarts(dir.path()) == before {
                tokio::time::sleep(Duration::from_millis(20)).await;
                // Between checkpoints, so the commit can find the whole log
                // folded back, which is when it restarts the log.
                let _between = db.checkpoints.turn.lock().await;
                sqlx::query("INSERT INTO filler VALUES (1)")
                    .execute(db.writer())
                    .await
                    .expect("insert");
            }
        })
        .await;
        assert!(restarted.is_ok(), "no checkpoint ran in the background");
    }

    /// The optimization task's truncating checkpoint takes the checkpoint turn,
    /// so it waits for a running checkpoint, however long, instead of failing
    /// busy against it, then empties the log.
    #[tokio::test]
    async fn truncating_waits_for_a_running_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = file_db(dir.path()).await;
        write_frames(&db, 300).await;

        let running = db.checkpoints.turn.lock().await;
        let mut truncate = tokio::spawn({
            let db = db.clone();
            async move { db.truncate_wal().await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut truncate)
                .await
                .is_err(),
            "the truncation waits for the running checkpoint"
        );
        drop(running);
        truncate.await.expect("join").expect("truncate");
        let log = std::fs::metadata(dir.path().join("live.db-wal")).expect("the WAL");
        assert_eq!(log.len(), 0, "the log is empty");
    }

    /// The truncation runs on the writer, so writes queue behind it in the pool
    /// instead of failing busy inside SQLite.
    #[tokio::test]
    async fn truncating_runs_on_the_writer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = file_db(dir.path()).await;
        write_frames(&db, 300).await;

        let write = db.writer().acquire().await.expect("a write in progress");
        let mut truncate = tokio::spawn({
            let db = db.clone();
            async move { db.truncate_wal().await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut truncate)
                .await
                .is_err(),
            "the truncation queues behind the write"
        );
        drop(write);
        truncate.await.expect("join").expect("truncate");
        let log = std::fs::metadata(dir.path().join("live.db-wal")).expect("the WAL");
        assert_eq!(log.len(), 0, "the log is empty");
    }

    /// Closing the handle closes the task's connection and ends the task, so no
    /// checkpoint of the previous handle can land on a database restored after
    /// it.
    #[tokio::test]
    async fn close_ends_the_checkpoint_task() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = file_db(dir.path()).await;
        write_frames(&db, 1_500).await;
        let holders = Arc::strong_count(&db.checkpoints);
        db.close().await;
        assert!(db.checkpointer.is_closed());
        // The task's own reference is the one that goes.
        let ended = tokio::time::timeout(Duration::from_secs(5), async {
            while Arc::strong_count(&db.checkpoints) == holders {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(ended.is_ok(), "the checkpoint task outlived close");
    }
}
