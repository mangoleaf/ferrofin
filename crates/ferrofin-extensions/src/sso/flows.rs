//! The pending OIDC flows: each sign-in started and not yet called back, in
//! memory, keyed by its `state` and bounded (D19). A restart forgets them, as
//! does a pod a callback reaches that did not start it: its refusal page's
//! "Try again" covers both.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use openidconnect::{Nonce, PkceCodeVerifier};

/// How long a sign-in may take, from its start to its callback: 10 minutes.
/// Candidate setting (D19): a constant until the owner asks for it.
pub const FLOW_LIFETIME: Duration = Duration::from_mins(10);

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
    /// Signing in to Ferrofin; the only purpose until linking and Quick
    /// Connect approval add theirs.
    SignIn,
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
