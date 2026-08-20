//! A connection-local replacement for SQLite's built-in `RANDOM()`.
//!
//! `ORDER BY RANDOM()` evaluates the function **once per scanned row**, and
//! SQLite's `random()` draws from one process-wide PRNG guarded by the static
//! `SQLITE_MUTEX_STATIC_PRNG` mutex. Every reader connection runs on its own OS
//! thread (sqlx SQLite is synchronous C on a dedicated thread), so a single
//! random-ordered query over a 9.8k-row table takes and releases that one global
//! mutex 9.8k times — and N concurrent such queries fight over it.
//!
//! Measured on the 9,862-item benchmark fixture, `GET /Items/Suggestions`
//! (`ORDER BY RANDOM() DESC` over `BaseItems`): at 400 req/s the server held
//! 4.2 ms p50, and at 500 req/s it collapsed to 5.4 s p50 while burning **19
//! cores of kernel time** against 2.9 cores of user time. Sampling every thread
//! during the collapse put 92 of ~110 non-idle stacks inside
//! `sqlite3_randomness` — `__lll_lock_wait` under `randomFunc` — i.e. the whole
//! machine was queueing on that one mutex.
//!
//! [`ferrofin_random`](RANDOM_SQL_EXPR) is the same draw from a **thread-local**
//! xorshift64\* generator: a connection touches only its own state, so the lock
//! disappears. It is registered as a non-deterministic scalar function, exactly
//! like `random()`, so SQLite re-evaluates it per row instead of folding it to a
//! constant.
//!
//! Distribution is unchanged in the way that matters to a caller: both draw a
//! value uniformly from the full `i64` range, independently per row, so
//! `ORDER BY ferrofin_random()` is the same uniformly random permutation
//! `ORDER BY RANDOM()` is.

use std::cell::Cell;
use std::ffi::{CStr, c_char, c_int, c_void};

use libsqlite3_sys as ffi;

/// The SQL expression to sort by wherever Jellyfin's C# emits `RANDOM()`.
///
/// Registered on every connection this crate opens (see
/// [`register_random_function`]), so query builders can reference it
/// unconditionally.
pub const RANDOM_SQL_EXPR: &str = "ferrofin_random()";

/// The registered SQL function name behind [`RANDOM_SQL_EXPR`].
const RANDOM_FN_NAME: &CStr = c"ferrofin_random";

thread_local! {
    /// This thread's xorshift64\* state; `0` means "not seeded yet" (and is
    /// also the one state xorshift can never reach, so the check is free).
    static PRNG_STATE: Cell<u64> = const { Cell::new(0) };
}

/// Seeds a thread's generator from the OS-randomized `RandomState` plus a
/// process-wide counter, so two connection threads never share a stream.
fn seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    match hasher.finish() {
        // xorshift64 is stuck at zero; the golden-ratio constant stands in.
        0 => 0x9E37_79B9_7F4A_7C15,
        seed => seed,
    }
}

/// Draws the next value from this thread's xorshift64\* stream.
fn next_random() -> i64 {
    let state = PRNG_STATE.with(|cell| {
        let mut x = cell.get();
        if x == 0 {
            x = seed();
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        cell.set(x);
        x
    });
    // The `*` of xorshift64*: the raw state is a full-period sequence, the
    // multiply is what makes the low bits as well-distributed as the high ones.
    let scrambled = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
    i64::from_ne_bytes(scrambled.to_ne_bytes())
}

/// SQLite scalar-function body for `ferrofin_random()`.
unsafe extern "C" fn random_fn(
    ctx: *mut ffi::sqlite3_context,
    _argc: c_int,
    _argv: *mut *mut ffi::sqlite3_value,
) {
    // SAFETY: SQLite passes a live context for the duration of the call, and
    // `sqlite3_result_int64` is the documented way to return an integer.
    unsafe { ffi::sqlite3_result_int64(ctx, next_random()) }
}

/// Auto-extension entry point: registers `ferrofin_random()` on a connection as
/// it opens.
///
/// Deliberately **not** `SQLITE_DETERMINISTIC` — the flag would let SQLite
/// evaluate the call once and reuse the value for every row, collapsing a
/// random sort into an arbitrary but fixed one.
unsafe extern "C" fn register_on_connection(
    db: *mut ffi::sqlite3,
    _err_msg: *mut *mut c_char,
    _api: *const ffi::sqlite3_api_routines,
) -> c_int {
    // SAFETY: called by SQLite with a freshly opened connection; the name
    // pointer is a `'static` C string and the callback is a plain `extern "C"`
    // fn with the signature SQLite expects.
    unsafe {
        ffi::sqlite3_create_function_v2(
            db,
            RANDOM_FN_NAME.as_ptr(),
            0,
            ffi::SQLITE_UTF8,
            std::ptr::null_mut::<c_void>(),
            Some(random_fn),
            None,
            None,
            None,
        )
    }
}

/// Registers [`RANDOM_SQL_EXPR`]'s backing function for every SQLite connection
/// opened from here on, once per process.
///
/// Uses `sqlite3_auto_extension` rather than a per-pool hook so that *every*
/// connection carries it — the reader pool, the writer pool, the throwaway
/// migration connection, and the single-connection in-memory pools tests use.
/// A query that sorts randomly would otherwise fail with "no such function" on
/// whichever connection was missed.
///
/// Must be called after `sqlite3_config` (registering an auto-extension
/// initializes the library, and `sqlite3_config` fails once that has happened).
pub(crate) fn register_random_function() {
    // SAFETY: `sqlite3_auto_extension` is thread-safe and only requires that
    // the entry point outlive every connection — it is a `fn` item, so it does.
    let rc = unsafe { ffi::sqlite3_auto_extension(Some(register_on_connection)) };
    if rc != ffi::SQLITE_OK {
        tracing::warn!(
            rc,
            "sqlite3_auto_extension(ferrofin_random) failed — random-ordered \
             queries will not run"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every draw must be independent: a generator that returned a constant (or
    /// a function registered as `SQLITE_DETERMINISTIC`) would still "work" for
    /// every caller that only checks the row count, so pin the property that
    /// makes it a random sort at all.
    #[test]
    fn successive_draws_differ_and_span_the_i64_range() {
        let draws: Vec<i64> = (0..1000).map(|_| next_random()).collect();
        let distinct: std::collections::HashSet<i64> = draws.iter().copied().collect();
        assert_eq!(distinct.len(), draws.len(), "draws repeated");
        assert!(
            draws.iter().any(|v| *v < 0) && draws.iter().any(|v| *v > 0),
            "draws must cover the whole i64 range like SQLite's random()"
        );
        // Top-bit balance: 1000 fair draws land far from 0 or 1000 negatives.
        let negatives = draws.iter().filter(|v| **v < 0).count();
        assert!(
            (350..=650).contains(&negatives),
            "sign should be ~balanced, got {negatives} negatives of 1000"
        );
    }

    /// A seed of zero is xorshift's one absorbing state — the generator must
    /// never be able to enter it.
    #[test]
    fn seeding_never_yields_the_zero_state() {
        for _ in 0..100 {
            assert_ne!(seed(), 0);
        }
    }

    /// Registration has to reach every connection this crate opens, and the
    /// function has to be re-evaluated per row.
    ///
    /// A pool the auto-extension missed fails the query outright ("no such
    /// function: ferrofin_random"), which is what an `ORDER BY` on it would do
    /// in production. A `SQLITE_DETERMINISTIC` registration would instead let
    /// SQLite hoist the call out of the loop and hand every row the same value
    /// — a "random" sort that never shuffles.
    async fn assert_random_per_row(db: &crate::Database) {
        let values: Vec<i64> = sqlx::query_scalar(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 500)
             SELECT ferrofin_random() FROM n",
        )
        .fetch_all(db.pool())
        .await
        .expect("ferrofin_random() must be registered on every connection");

        let distinct: std::collections::HashSet<i64> = values.iter().copied().collect();
        assert_eq!(distinct.len(), values.len(), "one draw per row");
        assert!(
            values.iter().any(|v| *v < 0) && values.iter().any(|v| *v > 0),
            "draws span the i64 range like SQLite's own random()"
        );
    }

    #[tokio::test]
    async fn random_function_is_registered_on_in_memory_and_file_pools() {
        let memory = crate::Database::connect_in_memory()
            .await
            .expect("in-memory pool");
        assert_random_per_row(&memory).await;

        let dir = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite://{}", dir.path().join("rand.db").display());
        let file = crate::Database::connect(&url).await.expect("file pool");
        assert_random_per_row(&file).await;
    }
}
