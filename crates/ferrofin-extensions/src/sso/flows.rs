//! The pending OIDC flows: each sign-in started and not yet called back, in
//! memory, keyed by its `state` and bounded (D19), and each exchange code a
//! completion page holds and has not yet swapped for a session, or a Quick
//! Connect confirmation page has not yet posted. A restart
//! forgets both, as does a pod a callback or an exchange reaches that did not
//! start it: the refusal's "Try again" covers both.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use openidconnect::{Nonce, PkceCodeVerifier};
use uuid::Uuid;

use super::sign_in::VerifiedIdentity;

/// How long a sign-in may take, from its start to its callback: 10 minutes.
/// Candidate setting (D19): a constant until the owner asks for it.
pub const FLOW_LIFETIME: Duration = Duration::from_mins(10);

/// How long a completion page's exchange code can be swapped for a session:
/// 60 seconds, plenty for a script that posts it at once.
/// Candidate setting (D19): a constant until the owner asks for it.
pub const EXCHANGE_CODE_LIFETIME: Duration = Duration::from_secs(60);

/// The pending flows one address may hold; its next start drops its oldest.
/// Quick Connect's default per-address cap (`DEFAULT_PENDING_PER_IP`).
/// Candidate setting (D19): a constant until the owner asks for it.
pub const MAX_PENDING_FLOWS_PER_ADDRESS: usize = 10;

/// The pending flows the server holds; the next start drops the oldest.
/// Candidate setting (D19): a constant until the owner asks for it.
pub const MAX_PENDING_FLOWS: usize = 1000;

/// What a flow's verified identity is for once its callback passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowPurpose {
    /// Signing in to Ferrofin.
    SignIn,
    /// Linking the identity to an account ([`links`](super::links)): it
    /// opens no session.
    Link {
        /// The account signed in when the flow started.
        user_id: Uuid,
        /// The SHA-256 of the access token of the session that started it,
        /// never the token: the callback links only while that session is
        /// still one of the account's.
        session: [u8; 32],
    },
    /// Approving a native app's Quick Connect code
    /// ([`quick_connect`](super::quick_connect)): it ends on a confirmation
    /// page, where the code is typed (D23), and opens no browser session.
    QuickConnect,
}

/// One pending flow: what its callback is checked against.
pub struct PendingFlow {
    /// The provider the flow was started at.
    pub provider_key: String,
    /// The provider's issuer when the flow started.
    pub issuer: String,
    /// The provider's client id when the flow started.
    pub client_id: String,
    /// The redirect URI the authorization request named, which the code
    /// exchange must name again.
    pub redirect_uri: String,
    /// The PKCE verifier of the request's `S256` challenge.
    pub verifier: PkceCodeVerifier,
    /// The nonce the ID token must carry.
    pub nonce: Nonce,
    /// The SHA-256 of the browser-binding cookie's value: the value itself is
    /// only ever in that browser.
    pub binding_hash: [u8; 32],
    /// When the flow started.
    pub created: Instant,
    /// The address that started it, for [`MAX_PENDING_FLOWS_PER_ADDRESS`].
    pub client: IpAddr,
    /// What the verified identity is for.
    pub purpose: FlowPurpose,
}

impl PendingFlow {
    /// Whether the flow is older than [`FLOW_LIFETIME`].
    #[must_use]
    pub fn expired(&self) -> bool {
        self.created.elapsed() >= FLOW_LIFETIME
    }
}

/// The pending flows, by `state`.
#[derive(Default)]
pub struct PendingFlows {
    flows: Mutex<HashMap<String, PendingFlow>>,
}

impl PendingFlows {
    /// Keeps `flow` under `state`, after dropping every expired flow, then
    /// its address's oldest until it holds fewer than
    /// [`MAX_PENDING_FLOWS_PER_ADDRESS`], then the oldest of all until fewer
    /// than [`MAX_PENDING_FLOWS`] are left, as Quick Connect's `make_room`
    /// does.
    pub fn insert(&self, state: String, flow: PendingFlow) {
        let mut flows = self.flows.lock().unwrap_or_else(PoisonError::into_inner);
        flows.retain(|_, pending| !pending.expired());
        let client = flow.client;
        make_room(&mut flows, Some(client), MAX_PENDING_FLOWS_PER_ADDRESS);
        make_room(&mut flows, None, MAX_PENDING_FLOWS);
        flows.insert(state, flow);
    }

    /// Removes and returns the flow `state` names, expired or not: a flow is
    /// used once, whatever its callback's outcome.
    pub fn take(&self, state: &str) -> Option<PendingFlow> {
        self.flows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(state)
    }

    /// Drops every pending link flow of the account `user_id`, so none links
    /// once a removal has signed the account out ([`links`](super::links)).
    pub fn drop_link_flows_of(&self, user_id: Uuid) {
        self.flows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|_, flow| {
                !matches!(flow.purpose, FlowPurpose::Link { user_id: of, .. } if of == user_id)
            });
    }

    /// How many flows are pending.
    #[must_use]
    pub fn len(&self) -> usize {
        self.flows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether no flow is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A verified sign-in whose completion page holds its exchange code, or
/// whose Quick Connect confirmation page holds its confirmation: who signs
/// in, what for, and the browser the code is bound to. `Debug` leaves the
/// claims out.
#[derive(Debug)]
pub struct PendingExchange {
    /// The SHA-256 of the flow cookie's value the sign-in was verified with:
    /// the exchange must bring the same cookie.
    pub binding_hash: [u8; 32],
    /// The account the verified identity signs in to.
    pub user_id: Uuid,
    /// The verified identity, its provider and claims.
    pub identity: VerifiedIdentity,
    /// What the code is for: a browser session, or the confirmation of a
    /// Quick Connect approval, which the exchange refuses.
    pub purpose: FlowPurpose,
    /// When the code was issued.
    pub created: Instant,
}

impl PendingExchange {
    /// Whether the code is older than [`EXCHANGE_CODE_LIFETIME`].
    #[must_use]
    pub fn expired(&self) -> bool {
        self.created.elapsed() >= EXCHANGE_CODE_LIFETIME
    }
}

/// The exchange codes not yet swapped, by code, capped at
/// [`MAX_PENDING_FLOWS`] in all. Each costs a whole sign-in at the identity
/// provider and lives a minute, so no address has a cap of its own.
#[derive(Default)]
pub struct ExchangeCodes {
    codes: Mutex<HashMap<String, PendingExchange>>,
}

impl ExchangeCodes {
    /// Keeps `exchange` under `code`, after dropping every expired code, then
    /// the oldest while [`MAX_PENDING_FLOWS`] are held.
    pub fn insert(&self, code: String, exchange: PendingExchange) {
        let mut codes = self.codes.lock().unwrap_or_else(PoisonError::into_inner);
        codes.retain(|_, pending| !pending.expired());
        // One at a time, so one removal keeps the cap.
        // shortcut: an O(n) scan per code over at most MAX_PENDING_FLOWS; index by age if the cap grows.
        if codes.len() >= MAX_PENDING_FLOWS
            && let Some(oldest) = codes
                .iter()
                .min_by_key(|(_, pending)| pending.created)
                .map(|(code, _)| code.clone())
        {
            codes.remove(&oldest);
        }
        codes.insert(code, exchange);
    }

    /// Removes and returns the exchange `code` names, expired or not: a code
    /// is used once, whatever its exchange's outcome.
    pub fn take(&self, code: &str) -> Option<PendingExchange> {
        self.codes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(code)
    }

    /// How many codes are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.codes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether no code is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Drops the oldest flows of `client` (of every address with `None`) until
/// fewer than `limit` are left, so the flow about to be added fits.
// shortcut: an O(n) scan per start over at most MAX_PENDING_FLOWS; index by address if the cap grows.
fn make_room(flows: &mut HashMap<String, PendingFlow>, client: Option<IpAddr>, limit: usize) {
    let mut own: Vec<(Instant, String)> = flows
        .iter()
        .filter(|(_, flow)| client.is_none_or(|client| flow.client == client))
        .map(|(state, flow)| (flow.created, state.clone()))
        .collect();
    if own.len() < limit {
        return;
    }
    own.sort_unstable();
    for (_, state) in &own[..=own.len() - limit] {
        flows.remove(state);
    }
}

#[cfg(test)]
mod tests;
