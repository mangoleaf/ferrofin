//! A warning when the writer connection stays out of its pool too long.
//!
//! Every write queues on the writer pool's one connection. While a holder
//! keeps it past the pool's acquire timeout, every write waiting behind it
//! fails with `pool timed out while waiting for an open connection`, and the
//! log names the writes that failed but never the holder. So each acquire
//! records the span it was made in, and [`watch`] warns once per hold, inside
//! that span, when the connection is still out of the pool after that
//! timeout. The timeout is the threshold because it is when waiting writes
//! start to fail: a shorter hold only delays them.
//!
//! The pool's `after_release` hook drops the recorded span, so a returned
//! connection keeps no span open. A connection sqlx closes instead of
//! returning it (past its lifetime) runs no hook, and its span waits for
//! [`watch`], at most the timeout. sqlx pings a returned connection before it
//! takes it back (`return_to_pool`, with no timeout), so a connection can
//! also be handed back and still not reach the pool: that gets its own
//! warning. The ping sqlx sends when it hands out an idle connection runs
//! before the acquire hook, but it is no hold: the acquire's timeout ends it
//! and drops that connection.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::time::Instant;

/// What the writer pool's hooks and [`watch`] share, one per
/// [`Database`](crate::Database).
#[derive(Debug, Default)]
pub(crate) struct WriterHolds {
    state: Mutex<State>,
    /// Bumped at each acquire, so [`watch`] sleeps only while a hold runs.
    bumped: tokio::sync::watch::Sender<()>,
}

#[derive(Debug, Default)]
struct State {
    /// How many times the connection has been acquired, so [`watch`] can tell
    /// a later hold from the one it waited on.
    acquires: u64,
    /// The latest acquire, until [`watch`] has looked at it.
    hold: Option<Hold>,
}

#[derive(Debug)]
struct Hold {
    acquire: u64,
    since: Instant,
    /// The acquirer's span, until its holder hands the connection back.
    span: Option<tracing::Span>,
    /// That span's name, kept for the warning after the hand-back.
    span_name: &'static str,
}

impl WriterHolds {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The writer connection left the pool for the task this runs in (the
    /// pool's `before_acquire` and `after_connect` hooks).
    pub(crate) fn acquired(&self) {
        let span = tracing::Span::current();
        let span_name = span.metadata().map_or("none", tracing::Metadata::name);
        let mut state = self.state();
        state.acquires += 1;
        let acquire = state.acquires;
        let previous = state.hold.replace(Hold {
            acquire,
            since: Instant::now(),
            span: Some(span),
            span_name,
        });
        drop(state);
        drop(previous);
        self.bumped.send_replace(());
    }

    /// Its holder handed the connection back (the pool's `after_release`
    /// hook); the pool takes it back once a ping answers.
    pub(crate) fn released(&self) {
        let span = self.state().hold.as_mut().and_then(|hold| hold.span.take());
        drop(span);
    }

    /// The latest acquire's number and time, while [`watch`] has not looked
    /// at it.
    fn latest(&self) -> Option<(u64, Instant)> {
        self.state()
            .hold
            .as_ref()
            .map(|hold| (hold.acquire, hold.since))
    }

    /// Takes the hold of acquire `acquire`, unless a later one replaced it.
    fn take(&self, acquire: u64) -> Option<Hold> {
        self.state().hold.take_if(|hold| hold.acquire == acquire)
    }
}

/// Warns, once per hold, when `pool`'s connection is still out of the pool
/// `limit` after it was acquired, until `pool` closes.
pub(crate) async fn watch(pool: SqlitePool, holds: Arc<WriterHolds>, limit: Duration) {
    let mut closed = pool.close_event();
    let mut bumped = holds.bumped.subscribe();
    loop {
        // Each pass looks at the latest acquire: while one holds the
        // connection, no later one can happen.
        while let Some((acquire, since)) = holds.latest() {
            tokio::select! {
                () = &mut closed => return,
                () = tokio::time::sleep_until(since + limit) => {}
            }
            // Not out: back in the pool, or closed and not reopened.
            let out = pool.num_idle() == 0 && pool.size() > 0;
            if let Some(hold) = holds.take(acquire).filter(|_| out) {
                report(&hold);
            }
        }
        tokio::select! {
            () = &mut closed => return,
            changed = bumped.changed() => if changed.is_err() { return },
        }
    }
}

fn report(hold: &Hold) {
    let held_ms = hold.since.elapsed().as_millis();
    if let Some(span) = &hold.span {
        // In the holder's span, so the line names it, its fields and the
        // spans around it.
        span.in_scope(|| {
            tracing::warn!(
                held_ms,
                holder_span = hold.span_name,
                "the database writer connection is still held past its acquire timeout; \
                 every write waiting for it fails until its holder returns it"
            );
        });
    } else {
        tracing::warn!(
            held_ms,
            holder_span = hold.span_name,
            "the database writer connection was returned but is still not back in its pool \
             past its acquire timeout; every write waiting for it fails until it is"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use sqlx::SqlitePool;
    use sqlx::sqlite::SqliteConnectOptions;
    use tracing::instrument::WithSubscriber as _;

    use super::WriterHolds;

    const LIMIT: Duration = Duration::from_millis(500);

    #[derive(Clone, Default)]
    struct Logs(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A writer pool built as the server's is, over a file in `dir`.
    async fn writer(dir: &std::path::Path) -> (SqlitePool, Arc<WriterHolds>) {
        let holds = Arc::new(WriterHolds::default());
        let pool = crate::database::writer_pool_options(&Arc::default(), &holds)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(dir.join("live.db"))
                    .create_if_missing(true),
            )
            .await
            .expect("the writer");
        (pool, holds)
    }

    /// Runs `work` beside a watch of `pool` with a short limit, and answers
    /// the watch's warnings.
    async fn warnings(
        pool: &SqlitePool,
        holds: &Arc<WriterHolds>,
        work: impl std::future::Future<Output = ()>,
    ) -> Vec<String> {
        let logs = Logs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer({
                let logs = logs.clone();
                move || logs.clone()
            })
            .finish();
        let watch = super::watch(pool.clone(), Arc::clone(holds), LIMIT);
        async {
            tokio::select! {
                () = watch => panic!("the watch ended while the pool was open"),
                () = work => {}
            }
        }
        .with_subscriber(subscriber)
        .await;
        String::from_utf8(logs.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter(|line| line.contains("WARN"))
            .map(str::to_owned)
            .collect()
    }

    /// A hold past the limit is reported once, in the holder's span; a short
    /// one, and a connection closed instead of returned, are not.
    #[tokio::test]
    async fn a_hold_past_the_limit_is_reported_once_in_its_holders_span() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (pool, holds) = writer(dir.path()).await;
        let warned = warnings(&pool, &holds, async {
            let short = pool.acquire().await.expect("the writer");
            drop(short);
            tokio::time::sleep(LIMIT * 3).await;
            let held = tracing::Instrument::instrument(
                pool.acquire(),
                tracing::info_span!("post_scan_pass", pass = "aggregates"),
            )
            .await
            .expect("the writer");
            tokio::time::sleep(LIMIT * 4).await;
            drop(held);
            let closed = pool.acquire().await.expect("the writer");
            closed.close().await.expect("close it");
            tokio::time::sleep(LIMIT * 3).await;
        })
        .await;
        assert_eq!(
            warned.len(),
            1,
            "one warning, for the long hold: {warned:#?}"
        );
        assert!(
            warned[0].contains("post_scan_pass{pass=\"aggregates\"}")
                && warned[0].contains("still held"),
            "names the holder's span: {}",
            warned[0]
        );
    }

    /// A connection handed back that does not reach the pool (sqlx's release
    /// ping has no timeout) is reported too, by the name of the span its
    /// holder has closed.
    #[tokio::test]
    async fn a_connection_returned_but_not_back_in_the_pool_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (pool, holds) = writer(dir.path()).await;
        let warned = warnings(&pool, &holds, async {
            let held =
                tracing::Instrument::instrument(pool.acquire(), tracing::info_span!("holder"))
                    .await
                    .expect("the writer");
            // What `after_release` does, with the connection still out.
            holds.released();
            tokio::time::sleep(LIMIT * 3).await;
            drop(held);
        })
        .await;
        assert_eq!(warned.len(), 1, "{warned:#?}");
        assert!(
            warned[0].contains("not back in its pool")
                && warned[0].contains("holder_span=\"holder\""),
            "{}",
            warned[0]
        );
    }
}
