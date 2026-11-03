//! Ferrofin's login backoff: an owner decision beyond Jellyfin, which has no
//! login rate limit.
//!
//! Failed logins pace further attempts per account and per client address,
//! and the backoff never disables an account: a finite lockout would let
//! anyone who knows a username disable it, the only administrator included.
//!
//! Each key (the account, the client address) counts its failures. Once a
//! key's failures and attempts in flight reach `attempts`, its next attempt
//! waits [`FIRST_DELAY`], and each one past that doubles the wait, up to
//! `max_delay`. Counting the attempts in flight makes concurrent attempts
//! queue one growing wait apart, as sequential failures would.
//!
//! - An address whose next attempt would wait longer than `max_delay` is
//!   refused without the password being checked.
//! - An account only waits, at most `max_delay`, and is never refused, so a
//!   caller hammering one account cannot keep its owner out (owner decision,
//!   2026-11-02). The guessing rate on one account then grows with the number
//!   of addresses guessing it.
//! - A success clears the account's failures and leaves the address's as
//!   they are, so one valid account cannot reset an address that is guessing
//!   others.
//! - A key is forgotten `window` after its last failure, once its wait is
//!   over. At most `max_tracked` keys are remembered: past it, the keys not
//!   yet paced are forgotten first.
//!
//! Keys are stored as hashes under a per-process seed, so a key's size never
//! depends on what a client sent.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::ops::RangeInclusive;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The wait that starts a key's backoff; each attempt past it doubles it, up
/// to the maximum (owner decision, 2026-11-02).
const FIRST_DELAY: Duration = Duration::from_secs(1);

/// The login backoff settings (`login_backoff_attempts`,
/// `login_backoff_max_delay_secs`, `login_backoff_window_mins`,
/// `login_backoff_max_tracked`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginBackoffSettings {
    /// Failures a key may make before its attempts are paced; `0` turns the
    /// backoff off.
    pub attempts: u32,
    /// The longest wait an attempt is given. An address that would wait
    /// longer is refused; an account waits this long at most.
    pub max_delay: Duration,
    /// How long a key's failures are remembered after its last one.
    pub window: Duration,
    /// The most keys remembered at once.
    pub max_tracked: usize,
}

impl LoginBackoffSettings {
    /// The default `attempts` (owner decision on gl-1099).
    pub const DEFAULT_ATTEMPTS: u32 = 5;
    /// The default `max_delay`, in seconds (owner decision on gl-1099).
    pub const DEFAULT_MAX_DELAY_SECS: u32 = 30;
    /// The default `window`, in minutes (owner decision, 2026-11-02).
    pub const DEFAULT_WINDOW_MINS: u32 = 15;
    /// The default `max_tracked` (owner decision, 2026-11-02).
    pub const DEFAULT_MAX_TRACKED: u32 = 100_000;
    /// The defaults: paced after 5 failures, at most 30 s apart, remembered
    /// for 15 minutes, 100,000 keys at most.
    pub const DEFAULT: Self = Self::new(
        Self::DEFAULT_ATTEMPTS,
        Self::DEFAULT_MAX_DELAY_SECS,
        Self::DEFAULT_WINDOW_MINS,
        Self::DEFAULT_MAX_TRACKED,
    );
    /// The values `attempts` takes besides `0` (off).
    pub const ATTEMPTS_RANGE: RangeInclusive<u32> = 3..=20;
    /// The values `max_delay` takes, in seconds.
    pub const MAX_DELAY_RANGE_SECS: RangeInclusive<u32> = 5..=300;
    /// The values `window` takes, in minutes.
    pub const WINDOW_RANGE_MINS: RangeInclusive<u32> = 1..=1440;
    /// The values `max_tracked` takes.
    pub const MAX_TRACKED_RANGE: RangeInclusive<u32> = 1_000..=10_000_000;

    /// The settings from the units they are configured in.
    #[must_use]
    pub const fn new(
        attempts: u32,
        max_delay_secs: u32,
        window_mins: u32,
        max_tracked: u32,
    ) -> Self {
        Self {
            attempts,
            max_delay: Duration::from_secs(max_delay_secs as u64),
            window: Duration::from_secs(window_mins as u64 * 60),
            max_tracked: max_tracked as usize,
        }
    }

    /// The wait after a key's `count`th failure or attempt in flight.
    fn delay(self, count: u32) -> Duration {
        let doublings = count.saturating_sub(self.attempts);
        FIRST_DELAY
            .saturating_mul(1_u32.checked_shl(doublings).unwrap_or(u32::MAX))
            .min(self.max_delay)
    }
}

impl Default for LoginBackoffSettings {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One key's failures.
#[derive(Debug, Clone, Copy)]
struct Streak {
    failures: u32,
    /// Admitted attempts not yet settled.
    pending: u32,
    /// When the key's next attempt may run, once it is paced.
    next_attempt: Instant,
    last_failure: Option<Instant>,
}

impl Streak {
    fn paced(&self, settings: LoginBackoffSettings) -> bool {
        self.failures.saturating_add(self.pending) >= settings.attempts
    }

    fn expired(&self, now: Instant, window: Duration) -> bool {
        self.pending == 0
            && now >= self.next_attempt
            && self
                .last_failure
                .is_none_or(|at| now.saturating_duration_since(at) > window)
    }
}

#[derive(Debug, Default)]
struct Streaks {
    by_key: HashMap<u64, Streak>,
    /// The size at which the map is swept: twice what the last sweep kept, at
    /// most `max_tracked`, and a sweep at the cap keeps at most half of it. A
    /// sweep keeping k keys is followed by the next at least k/2 attempts
    /// later, so sweeping costs amortized O(1) per attempt.
    sweep_at: usize,
}

impl Streaks {
    /// Forgets expired streaks. At `max_tracked` it also brings the map down
    /// to half of it: the streaks not yet paced go first, then any.
    fn sweep(&mut self, now: Instant, settings: LoginBackoffSettings) {
        let at_cap = self.by_key.len() >= settings.max_tracked;
        self.by_key
            .retain(|_, streak| !streak.expired(now, settings.window));
        let half = settings.max_tracked / 2;
        if at_cap && self.by_key.len() > half {
            self.by_key.retain(|_, streak| streak.paced(settings));
        }
        if at_cap && self.by_key.len() > half {
            let mut excess = self.by_key.len() - half;
            self.by_key.retain(|_, _| {
                let keep = excess == 0;
                excess = excess.saturating_sub(1);
                keep
            });
        }
        self.sweep_at = (self.by_key.len() * 2).min(settings.max_tracked);
        self.by_key.shrink_to(self.sweep_at);
    }
}

/// The backoff's state, shared by every clone of the user manager.
#[derive(Debug)]
pub(super) struct LoginBackoff {
    settings: LoginBackoffSettings,
    seed: RandomState,
    streaks: Mutex<Streaks>,
}

/// The key an account is known by: `user:{id}` for a known user, else
/// `name:{name}` folded as the user lookup folds it, so the spellings that
/// would find one user share one key, known or not.
pub(super) fn account_key(user_id: Option<&str>, username: &str) -> String {
    user_id.map_or_else(
        || {
            format!(
                "name:{}",
                ferrofin_util::string_extensions::upper_invariant(username)
            )
        },
        |id| format!("user:{id}"),
    )
}

/// Who an attempt is for: the key the account is known by, and the names a
/// log line gives it.
pub(super) struct Who<'a> {
    /// The account's [`account_key`].
    pub(super) account: &'a str,
    /// The user's id, when the name is known.
    pub(super) user_id: Option<&'a str>,
    /// The name as entered.
    pub(super) username: &'a str,
    /// The client address, when the request has one.
    pub(super) address: Option<&'a str>,
}

/// An admitted attempt. Dropping it settles the attempt as a failure, so an
/// error path counts as one; [`Attempt::succeeded`] settles it as a success.
#[must_use]
pub(super) struct Attempt<'a> {
    backoff: &'a LoginBackoff,
    /// The account's and the address's keys; `None` when the backoff is off.
    keys: Option<(u64, Option<u64>)>,
    /// What the log line names: the user id or the name, and the address.
    user_id: Option<String>,
    username: String,
    address: Option<String>,
    /// When the attempt was let run: the time its failure counts from.
    at: Instant,
    succeeded: bool,
}

impl Attempt<'_> {
    /// Settles the attempt as a successful login.
    pub(super) fn succeeded(mut self) {
        self.succeeded = true;
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        let Some((account, address)) = self.keys else {
            return;
        };
        let (account_paced, address_paced) =
            self.backoff
                .settle(account, address, self.succeeded, self.at);
        let attempts = self.backoff.settings.attempts;
        if account_paced {
            tracing::warn!(
                user_id = self.user_id.as_deref(),
                username = ?self.username,
                attempts,
                "repeated failed logins for one account; its next attempts wait"
            );
        }
        if address_paced {
            tracing::warn!(
                remote_ip = self.address.as_deref(),
                attempts,
                "repeated failed logins from one address; its next attempts wait"
            );
        }
    }
}

impl LoginBackoff {
    pub(super) fn new(settings: LoginBackoffSettings) -> Self {
        Self {
            settings,
            seed: RandomState::new(),
            streaks: Mutex::new(Streaks::default()),
        }
    }

    /// Admits an attempt by `who` at `now`: how long it waits before its
    /// password is checked, or `None` when its address is refused.
    pub(super) fn admit(&self, who: &Who<'_>, now: Instant) -> Option<(Duration, Attempt<'_>)> {
        let settings = self.settings;
        let mut attempt = Attempt {
            backoff: self,
            keys: None,
            user_id: who.user_id.map(str::to_owned),
            username: who.username.to_owned(),
            address: who.address.map(str::to_owned),
            at: now,
            succeeded: false,
        };
        if settings.attempts == 0 {
            return Some((Duration::ZERO, attempt));
        }
        let account = self.seed.hash_one(("account", who.account));
        let address = who
            .address
            .map(|address| self.seed.hash_one(("address", address)));
        let mut streaks = self.streaks.lock().unwrap_or_else(PoisonError::into_inner);
        if streaks.by_key.len() >= streaks.sweep_at {
            streaks.sweep(now, settings);
        }
        for key in std::iter::once(account).chain(address) {
            if streaks
                .by_key
                .get(&key)
                .is_some_and(|streak| streak.expired(now, settings.window))
            {
                streaks.by_key.remove(&key);
            }
        }
        let paced_until = |key: u64| {
            streaks
                .by_key
                .get(&key)
                .filter(|streak| streak.paced(settings))
                .map_or(now, |streak| streak.next_attempt.max(now))
        };
        let address_wait = address.map_or(Duration::ZERO, |key| paced_until(key) - now);
        if address_wait > settings.max_delay {
            return None;
        }
        let account_wait = (paced_until(account) - now).min(settings.max_delay);
        let at = now + address_wait.max(account_wait);
        for key in std::iter::once(account).chain(address) {
            let streak = streaks.by_key.entry(key).or_insert(Streak {
                failures: 0,
                pending: 0,
                next_attempt: at,
                last_failure: None,
            });
            streak.pending = streak.pending.saturating_add(1);
            if streak.paced(settings) {
                streak.next_attempt =
                    at + settings.delay(streak.failures.saturating_add(streak.pending));
            }
        }
        drop(streaks);
        attempt.keys = Some((account, address));
        attempt.at = at;
        Some((at - now, attempt))
    }

    /// Settles an admitted attempt: a failure counts against both keys, a
    /// success clears the account's failures. Returns whether the failure
    /// started the account's and the address's pacing.
    fn settle(
        &self,
        account: u64,
        address: Option<u64>,
        succeeded: bool,
        at: Instant,
    ) -> (bool, bool) {
        let attempts = self.settings.attempts;
        let mut streaks = self.streaks.lock().unwrap_or_else(PoisonError::into_inner);
        let mut settle = |key: u64, is_account: bool| {
            let Some(streak) = streaks.by_key.get_mut(&key) else {
                return false;
            };
            streak.pending = streak.pending.saturating_sub(1);
            if succeeded {
                if is_account {
                    streak.failures = 0;
                }
            } else {
                streak.failures = streak.failures.saturating_add(1);
                streak.last_failure = Some(at);
            }
            let started = !succeeded && streak.failures == attempts;
            if streak.failures == 0 && streak.pending == 0 {
                streaks.by_key.remove(&key);
            }
            started
        };
        let account_paced = settle(account, true);
        let address_paced = address.is_some_and(|key| settle(key, false));
        (account_paced, address_paced)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTINGS: LoginBackoffSettings = LoginBackoffSettings::DEFAULT;
    const ACCOUNT: &str = "user:a";
    const ADDRESS: &str = "198.51.100.1";

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn who<'a>(account: &'a str, address: Option<&'a str>) -> Who<'a> {
        Who {
            account,
            user_id: None,
            username: account,
            address,
        }
    }

    /// One attempt on `account` from `address` at `now` that fails: its wait.
    fn fail(
        backoff: &LoginBackoff,
        account: &str,
        address: Option<&str>,
        now: Instant,
    ) -> Option<Duration> {
        backoff
            .admit(&who(account, address), now)
            .map(|(wait, _attempt)| wait)
    }

    fn tracked(backoff: &LoginBackoff) -> usize {
        backoff.streaks.lock().unwrap().by_key.len()
    }

    #[test]
    fn the_wait_starts_after_the_allowed_failures_and_doubles_to_the_maximum() {
        let backoff = LoginBackoff::new(SETTINGS);
        let t0 = Instant::now();
        for _ in 0..5 {
            assert_eq!(
                fail(&backoff, ACCOUNT, Some(ADDRESS), t0),
                Some(Duration::ZERO)
            );
        }
        // Each later attempt arrives at once and waits the paced delay.
        let mut now = t0;
        for expected in [1, 2, 4, 8, 16, 30, 30] {
            let wait = fail(&backoff, ACCOUNT, Some(ADDRESS), now).expect("admitted");
            assert_eq!(wait, secs(expected));
            now += wait;
        }
    }

    #[test]
    fn concurrent_attempts_from_one_address_queue_and_past_the_maximum_are_refused() {
        let backoff = LoginBackoff::new(SETTINGS);
        let t0 = Instant::now();
        // Eleven at once, none settled: the first five go straight in, the
        // fifth starting the pacing, and each later one waits behind the last
        // (1, 1 + 2, 1 + 2 + 4, ... = 15 s); the next would wait 31 s.
        let admitted: Vec<_> = (0..11)
            .map(|_| backoff.admit(&who(ACCOUNT, Some(ADDRESS)), t0))
            .collect();
        let waits: Vec<_> = admitted
            .iter()
            .map(|a| a.as_ref().map(|(wait, _)| wait.as_secs()))
            .collect();
        assert_eq!(
            waits,
            [
                Some(0),
                Some(0),
                Some(0),
                Some(0),
                Some(0),
                Some(1),
                Some(3),
                Some(7),
                Some(15),
                None,
                None
            ]
        );
    }

    #[test]
    fn an_account_only_waits_and_is_never_refused() {
        let backoff = LoginBackoff::new(SETTINGS);
        let t0 = Instant::now();
        // Many addresses at once on one account: none is refused, and none
        // waits longer than the maximum.
        let admitted: Vec<_> = (0..40)
            .map(|n| {
                let address = format!("198.51.100.{n}");
                backoff
                    .admit(&who(ACCOUNT, Some(&address)), t0)
                    .expect("never refused")
            })
            .collect();
        assert!(admitted.iter().all(|(wait, _)| *wait <= SETTINGS.max_delay));
        assert_eq!(admitted.last().unwrap().0, SETTINGS.max_delay);
        drop(admitted);
        // Its owner, from a fresh address, still gets in after the maximum.
        assert_eq!(
            fail(&backoff, ACCOUNT, Some("203.0.113.9"), t0),
            Some(SETTINGS.max_delay)
        );
    }

    #[test]
    fn an_attempt_with_no_address_is_paced_by_its_account() {
        let backoff = LoginBackoff::new(SETTINGS);
        let t0 = Instant::now();
        for _ in 0..5 {
            assert_eq!(fail(&backoff, ACCOUNT, None, t0), Some(Duration::ZERO));
        }
        assert_eq!(fail(&backoff, ACCOUNT, None, t0), Some(secs(1)));
    }

    #[test]
    fn the_address_paces_every_account_it_tries() {
        let backoff = LoginBackoff::new(SETTINGS);
        let t0 = Instant::now();
        for n in 0..5 {
            let account = format!("name:{n}");
            assert_eq!(
                fail(&backoff, &account, Some(ADDRESS), t0),
                Some(Duration::ZERO)
            );
        }
        assert_eq!(fail(&backoff, "name:new", Some(ADDRESS), t0), Some(secs(1)));
        assert_eq!(
            fail(&backoff, "name:other", Some("198.51.100.2"), t0),
            Some(Duration::ZERO),
            "another address and account are not paced"
        );
    }

    #[test]
    fn a_refused_attempt_changes_nothing() {
        let backoff = LoginBackoff::new(SETTINGS);
        let t0 = Instant::now();
        let _held: Vec<_> = (0..9)
            .map(|_| {
                backoff
                    .admit(&who(ACCOUNT, Some(ADDRESS)), t0)
                    .expect("admitted")
            })
            .collect();
        let before = backoff.streaks.lock().unwrap().by_key.clone();
        assert!(backoff.admit(&who(ACCOUNT, Some(ADDRESS)), t0).is_none());
        let after = backoff.streaks.lock().unwrap().by_key.clone();
        assert_eq!(before.len(), after.len());
        for (key, streak) in &before {
            let other = after[key];
            assert_eq!(
                (streak.failures, streak.pending, streak.next_attempt),
                (other.failures, other.pending, other.next_attempt)
            );
        }
    }

    #[test]
    fn a_success_clears_the_account_but_not_the_address() {
        let backoff = LoginBackoff::new(SETTINGS);
        let t0 = Instant::now();
        for _ in 0..4 {
            fail(&backoff, ACCOUNT, Some(ADDRESS), t0);
        }
        let (_, attempt) = backoff
            .admit(&who(ACCOUNT, Some(ADDRESS)), t0)
            .expect("admitted");
        attempt.succeeded();
        // The account starts over; the address keeps its four failures, so
        // one more from it starts the wait, and a success did not add one.
        for _ in 0..4 {
            assert_eq!(
                fail(&backoff, ACCOUNT, Some("198.51.100.2"), t0),
                Some(Duration::ZERO)
            );
        }
        assert_eq!(
            fail(&backoff, "user:b", Some(ADDRESS), t0),
            Some(Duration::ZERO)
        );
        assert_eq!(fail(&backoff, "user:c", Some(ADDRESS), t0), Some(secs(1)));
    }

    #[test]
    fn a_streak_is_forgotten_after_the_window() {
        let backoff = LoginBackoff::new(SETTINGS);
        let t0 = Instant::now();
        for _ in 0..6 {
            fail(&backoff, ACCOUNT, Some(ADDRESS), t0);
        }
        let later = t0 + SETTINGS.window + secs(31);
        assert_eq!(
            fail(&backoff, ACCOUNT, Some(ADDRESS), later),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn a_streak_outlives_a_window_shorter_than_its_wait() {
        let settings = LoginBackoffSettings::new(5, 300, 1, 100_000);
        let backoff = LoginBackoff::new(settings);
        let t0 = Instant::now();
        let mut now = t0;
        for _ in 0..14 {
            now += fail(&backoff, ACCOUNT, Some(ADDRESS), now).expect("admitted");
        }
        // The last wait was 256 s; 61 s on, the window has passed but the
        // wait has not, so the streak is still there.
        let wait = fail(&backoff, ACCOUNT, Some(ADDRESS), now + secs(61)).expect("admitted");
        assert!(wait > Duration::ZERO, "{wait:?}");
    }

    #[test]
    fn expired_streaks_are_swept() {
        let backoff = LoginBackoff::new(SETTINGS);
        let t0 = Instant::now();
        for n in 0..100 {
            fail(&backoff, &format!("name:{n}"), Some(&format!("a{n}")), t0);
        }
        let later = t0 + SETTINGS.window + secs(1);
        for n in 0..300 {
            fail(
                &backoff,
                &format!("name:x{n}"),
                Some(&format!("ax{n}")),
                later,
            );
        }
        // Some sweep after the window has forgotten every old streak.
        let fresh = backoff.streaks.lock().unwrap().by_key.len();
        assert!(fresh <= 600, "{fresh}");
        for n in 0..100 {
            let old = backoff
                .seed
                .hash_one(("account", format!("name:{n}").as_str()));
            assert!(!backoff.streaks.lock().unwrap().by_key.contains_key(&old));
        }
    }

    #[test]
    fn past_the_cap_the_keys_not_yet_paced_go_first() {
        let settings = LoginBackoffSettings::new(5, 30, 15, 1_000);
        let backoff = LoginBackoff::new(settings);
        let t0 = Instant::now();
        for _ in 0..5 {
            fail(&backoff, ACCOUNT, Some(ADDRESS), t0);
        }
        for n in 0..5_000 {
            fail(&backoff, &format!("name:{n}"), Some(&format!("a{n}")), t0);
        }
        assert!(tracked(&backoff) <= 1_002, "{}", tracked(&backoff));
        // The paced account and address are still paced.
        assert_eq!(fail(&backoff, "user:z", Some(ADDRESS), t0), Some(secs(1)));
        assert!(fail(&backoff, ACCOUNT, Some("x"), t0).expect("admitted") > Duration::ZERO);
    }

    #[test]
    fn past_the_cap_with_every_key_in_use_half_are_forgotten() {
        let settings = LoginBackoffSettings::new(3, 30, 15, 1_000);
        let backoff = LoginBackoff::new(settings);
        let t0 = Instant::now();
        // Three attempts in flight from each address keep its key paced, so
        // at the cap a sweep has little unpaced to drop (only the address
        // still being started) and forgets keys of attempts still in flight.
        let mut held = Vec::new();
        let (mut largest, mut smallest_after_cap) = (0, usize::MAX);
        for n in 0..1_500 {
            let address = format!("a{n}");
            for _ in 0..3 {
                held.push(
                    backoff
                        .admit(&who(ACCOUNT, Some(&address)), t0)
                        .expect("admitted"),
                );
                let now = tracked(&backoff);
                if largest >= 1_000 {
                    smallest_after_cap = smallest_after_cap.min(now);
                }
                largest = largest.max(now);
            }
        }
        assert!(largest <= 1_001, "{largest}");
        // A sweep at the cap brought the map down to half of it.
        assert!(smallest_after_cap <= 502, "{smallest_after_cap}");
        // The attempts whose keys were forgotten settle without a trace.
        drop(held);
        let streaks = backoff.streaks.lock().unwrap();
        assert!(streaks.by_key.values().all(|streak| streak.pending == 0));
        assert!(streaks.by_key.len() <= 1_001);
    }

    #[test]
    fn spellings_the_lookup_folds_together_share_an_account_key() {
        assert_eq!(account_key(None, "alice"), account_key(None, "ALICE"));
        assert_eq!(account_key(None, "münchen"), account_key(None, "MÜNCHEN"));
        assert_ne!(account_key(None, "alice"), account_key(None, "bob"));
        // A known user is keyed by id, whatever the name was typed as.
        assert_eq!(
            account_key(Some("id-1"), "Alice"),
            account_key(Some("id-1"), "alice")
        );
        assert_ne!(
            account_key(Some("id-1"), "alice"),
            account_key(None, "alice")
        );
    }

    #[test]
    fn successes_leave_nothing_behind() {
        let backoff = LoginBackoff::new(SETTINGS);
        for _ in 0..50 {
            let (wait, attempt) = backoff
                .admit(&who(ACCOUNT, Some(ADDRESS)), Instant::now())
                .expect("admitted");
            assert_eq!(wait, Duration::ZERO);
            attempt.succeeded();
        }
        assert_eq!(tracked(&backoff), 0);
    }

    #[test]
    fn zero_attempts_turns_it_off() {
        let backoff = LoginBackoff::new(LoginBackoffSettings {
            attempts: 0,
            ..SETTINGS
        });
        for _ in 0..50 {
            assert_eq!(
                fail(&backoff, ACCOUNT, Some(ADDRESS), Instant::now()),
                Some(Duration::ZERO)
            );
        }
        assert_eq!(tracked(&backoff), 0);
    }
}
