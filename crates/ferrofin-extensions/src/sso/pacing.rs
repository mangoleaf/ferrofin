//! The pace of the Single Sign-On routes (D23): per client, at most
//! [`MAX_STARTS_PER_WINDOW`] requests that create a pending flow and
//! [`MAX_CALLBACKS_PER_WINDOW`] that take a one-time value in any
//! [`PACE_WINDOW`]. Past either, the route answers a `429` page that reflects
//! nothing from the request, and does nothing else.
//!
//! A client is its [`address_key`]: an IPv4 address as itself, an IPv6
//! address by its /64, which an ordinary host holds whole, and an IPv4-mapped
//! IPv6 address as its IPv4 address. The pending flows are capped per the
//! same key ([`flows`](super::flows)). Loopback and the server's own address
//! are clients like any other.
//!
//! Each key keeps the times of its last admitted requests, as many as its
//! limit: a request is admitted while the oldest of them is a window old, so
//! no window, however it falls, holds more than the limit. A start that
//! another client sharing the key admitted is then never followed by
//! [`MAX_PENDING_FLOWS_PER_ADDRESS`](super::flows::MAX_PENDING_FLOWS_PER_ADDRESS)
//! more within a window, which is what would push its flow out.
//!
//! The state is bounded as the login backoff's is: a key whose last admitted
//! request is a window old is forgotten at the next sweep, and each pace
//! remembers at most [`MAX_PACED_ADDRESSES`] keys; past that, the keys under
//! their limit are forgotten first.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use ferrofin_traits::sso::{OidcPacedRoute, SsoReply};

use super::sign_in::page_linking_home;

/// The window each limit counts its requests in: a minute (D23). The `429`
/// page says "a minute".
/// Candidate setting (D19): a constant until the owner asks for it.
pub const PACE_WINDOW: Duration = Duration::from_mins(1);

/// The requests that create a pending flow (the sign-in, Quick Connect and
/// link starts) one client may make in any [`PACE_WINDOW`]: 10 (D23).
/// Candidate setting (D19): a constant until the owner asks for it.
pub const MAX_STARTS_PER_WINDOW: usize = 10;

/// The requests that take a one-time value (the callback, the exchange and
/// the Quick Connect confirmation) one client may make in any
/// [`PACE_WINDOW`]: 30 (D23).
/// Candidate setting (D19): a constant until the owner asks for it.
pub const MAX_CALLBACKS_PER_WINDOW: usize = 30;

/// The clients each pace remembers at once: ten times the server's cap of
/// pending flows. A client at its limit is forgotten only once half this many
/// others are at theirs in the same window, and a hundred clients holding
/// their cap of flows already fill the server's, so a larger bound would
/// protect no flow. A client costs a few hundred bytes at most.
/// Candidate setting (D19): a constant until the owner asks for it.
pub const MAX_PACED_ADDRESSES: usize = 10_000;

/// The key `client` is paced and its pending flows capped by: an IPv4
/// address as itself, an IPv4-mapped IPv6 address as its IPv4 address, and
/// any other IPv6 address by its /64.
#[must_use]
pub fn address_key(client: IpAddr) -> IpAddr {
    match client {
        IpAddr::V4(_) => client,
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or_else(
            || IpAddr::V6(Ipv6Addr::from_bits(v6.to_bits() & !u128::from(u64::MAX))),
            IpAddr::V4,
        ),
    }
}

/// The pacing's limits: [`PaceLimits::DEFAULT`] on a server, others for a
/// test that makes more requests from one address than a person would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaceLimits {
    /// The starts a client may make in any window.
    pub starts: usize,
    /// The one-time values a client may bring in any window.
    pub callbacks: usize,
    /// The window.
    pub window: Duration,
    /// The clients each pace remembers at once.
    pub max_tracked: usize,
}

impl PaceLimits {
    /// [`MAX_STARTS_PER_WINDOW`] and [`MAX_CALLBACKS_PER_WINDOW`] in any
    /// [`PACE_WINDOW`], for at most [`MAX_PACED_ADDRESSES`] clients.
    pub const DEFAULT: Self = Self {
        starts: MAX_STARTS_PER_WINDOW,
        callbacks: MAX_CALLBACKS_PER_WINDOW,
        window: PACE_WINDOW,
        max_tracked: MAX_PACED_ADDRESSES,
    };
}

/// A key's admitted requests.
#[derive(Debug, Default)]
struct Admitted {
    /// Their times, at most the limit; once full, a ring whose oldest is at
    /// `oldest`.
    times: Vec<Instant>,
    oldest: usize,
    /// When a refusal was last logged.
    warned: Option<Instant>,
}

impl Admitted {
    /// The latest admitted time.
    fn latest(&self) -> Option<Instant> {
        let len = self.times.len();
        self.times
            .get((self.oldest + len).checked_sub(1)? % len)
            .copied()
    }

    /// Whether no admitted request is younger than `window`.
    fn idle(&self, now: Instant, window: Duration) -> bool {
        self.latest()
            .is_none_or(|at| now.saturating_duration_since(at) >= window)
    }

    /// Whether `limit` requests were admitted within `window`.
    fn at_limit(&self, now: Instant, limit: usize, window: Duration) -> bool {
        self.times.len() >= limit
            && self
                .times
                .get(self.oldest)
                .is_none_or(|at| now.saturating_duration_since(*at) < window)
    }
}

#[derive(Debug, Default)]
struct Keys {
    by_key: HashMap<IpAddr, Admitted>,
    /// The size at which the map is swept: twice what the last sweep kept,
    /// at most `max_tracked`, as the login backoff's `sweep_at`, so sweeping
    /// costs amortized O(1) per request.
    sweep_at: usize,
}

/// One limit, per key, in any window.
#[derive(Debug)]
struct Pace {
    /// What its log line calls it.
    name: &'static str,
    limit: usize,
    window: Duration,
    max_tracked: usize,
    keys: Mutex<Keys>,
}

impl Pace {
    fn new(name: &'static str, limit: usize, limits: PaceLimits) -> Self {
        Self {
            name,
            limit,
            window: limits.window,
            max_tracked: limits.max_tracked,
            keys: Mutex::new(Keys::default()),
        }
    }

    /// Counts a request from `client` at `now`: admitted, and kept, while
    /// fewer than the limit of its key's requests were admitted within the
    /// window before it.
    fn admit(&self, client: IpAddr, now: Instant) -> bool {
        let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        if keys.by_key.len() >= keys.sweep_at {
            self.sweep(&mut keys, now);
        }
        let admitted = keys.by_key.entry(address_key(client)).or_default();
        if !admitted.at_limit(now, self.limit, self.window) {
            if admitted.times.len() < self.limit {
                admitted.times.push(now);
            } else if let Some(oldest) = admitted.times.get_mut(admitted.oldest) {
                *oldest = now;
                admitted.oldest = (admitted.oldest + 1) % self.limit;
            }
            return true;
        }
        // Once per key and window, so the log grows with the paced
        // addresses, never with their requests.
        let warn = admitted
            .warned
            .is_none_or(|at| now.saturating_duration_since(at) >= self.window);
        if warn {
            admitted.warned = Some(now);
        }
        drop(keys);
        if warn {
            tracing::warn!(
                remote_ip = %client,
                pace = self.name,
                limit = self.limit,
                "repeated Single Sign-On requests from one address; its next ones are refused \
                 for up to a minute"
            );
        }
        false
    }

    /// Forgets the idle keys. At `max_tracked` it also brings the map down
    /// to half of it: the keys under their limit go first, then any.
    fn sweep(&self, keys: &mut Keys, now: Instant) {
        let at_cap = keys.by_key.len() >= self.max_tracked;
        keys.by_key
            .retain(|_, admitted| !admitted.idle(now, self.window));
        let half = self.max_tracked / 2;
        if at_cap && keys.by_key.len() > half {
            keys.by_key
                .retain(|_, admitted| admitted.at_limit(now, self.limit, self.window));
        }
        if at_cap && keys.by_key.len() > half {
            let mut excess = keys.by_key.len() - half;
            keys.by_key.retain(|_, _| {
                let keep = excess == 0;
                excess = excess.saturating_sub(1);
                keep
            });
        }
        keys.sweep_at = (keys.by_key.len() * 2).min(self.max_tracked);
        keys.by_key.shrink_to(keys.sweep_at);
    }
}

/// The pacing of the Single Sign-On routes: the starts' pace, and the pace
/// of the one-time values.
#[derive(Debug)]
pub(super) struct Pacing {
    starts: Pace,
    callbacks: Pace,
}

impl Pacing {
    /// The pacing under `limits`.
    pub(super) fn new(limits: PaceLimits) -> Self {
        Self {
            starts: Pace::new("start", limits.starts, limits),
            callbacks: Pace::new("callback", limits.callbacks, limits),
        }
    }

    /// Counts a request to `route` from `client` at `now`: whether it is
    /// within its pace.
    pub(super) fn admit(&self, route: OidcPacedRoute, client: IpAddr, now: Instant) -> bool {
        match route {
            OidcPacedRoute::Start => &self.starts,
            OidcPacedRoute::Callback
            | OidcPacedRoute::Exchange
            | OidcPacedRoute::QuickConnectConfirmation => &self.callbacks,
        }
        .admit(client, now)
    }
}

/// The `429` page of `route`, linking back to jellyfin-web from its path: the
/// same words whatever the request held.
pub(super) fn paced_page(route: OidcPacedRoute) -> SsoReply {
    let home = match route {
        // `/sso/oidc/{provider}/…`
        OidcPacedRoute::Start | OidcPacedRoute::Callback => "../../../web/",
        // `/sso/oidc/exchange`
        OidcPacedRoute::Exchange => "../../web/",
        // `/sso/quickconnect`
        OidcPacedRoute::QuickConnectConfirmation => "../web/",
    };
    page_linking_home(
        429,
        "Too many sign-in attempts",
        "<p>Too many sign-in attempts came from your network. Wait a minute, then try \
         again.</p>",
        home,
    )
}

#[cfg(test)]
mod tests;
