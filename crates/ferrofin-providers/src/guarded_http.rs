//! GETs of a URL a third party chose, under a [`DestinationPolicy`].
//!
//! An image a content source names and the subtitle link OpenSubtitles
//! answers with are both fetched here, so that neither can make the server
//! reach an address its policy refuses: each request and each redirect hop is
//! vetted before it is sent, and redirects are followed here, not by the
//! client, so that none goes unvetted. Each kind of fetch names its own
//! policy and its own allow-list ([`FetchKind`]): image downloads
//! (`image_private_http_allow`, gl-1096) and subtitle links
//! (`subtitle_private_http_allow`, gl-1160).
//!
//! Upstream follows redirects anywhere with a default `HttpClient`, both for
//! images and in the OpenSubtitles plugin; the guard is an accepted
//! divergence (owner decisions 2026-10-31 and 2026-11-03).

use crate::destination::{self, DestinationPolicy, DestinationRefused, PublicOnlyResolver};
use crate::rate_limit::{LimitedRequest as _, RateLimiter, RequestError, request_limits};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Most redirects one fetch follows: reqwest's default policy
/// (`redirect::Policy::default()`), which downloads went by until they followed
/// redirects themselves, to vet each hop.
pub(crate) const MAX_REDIRECTS: usize = 10;

/// Image/CDN quotas are independent of metadata API quotas. Share a gate by
/// origin across download callers, never one gate across unrelated hosts.
///
/// `label` is only the provider the caller's downloads are counted under
/// (`ferrofin_metadata_provider_requests_total{provider}`): `image` for
/// artwork, `opensubtitles` for a subtitle file. It never picks the gate:
/// every caller of one origin shares its pacing and cooldowns whatever it is
/// counted as (METRICS.md rule 6).
pub(crate) fn limiter_for(url: &str, label: &'static str) -> RateLimiter {
    static LIMITERS: OnceLock<Mutex<HashMap<String, RateLimiter>>> = OnceLock::new();
    let origin = reqwest::Url::parse(url)
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_default();
    LIMITERS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(origin)
        .or_insert_with_key(|origin| RateLimiter::new(origin.clone()))
        .counted_as(label)
}

/// What a guarded fetch is, for its limiter and its log lines.
pub(crate) struct FetchKind {
    /// The `provider` label its requests are counted under
    /// ([`limiter_for`]).
    label: &'static str,
    /// What one fetch gets, in log lines: `remote image`, `subtitle`.
    noun: &'static str,
    /// The fetches as a whole, in log lines: `image downloads`.
    plural: &'static str,
    /// What lets a refused destination through, for the first refusal's
    /// warning: the setting, and anything else that exempts a host.
    allowed_by: &'static str,
    /// The `User-Agent` each request sends, if any.
    user_agent: Option<&'static str>,
    /// Whether a refusal was logged at `warn` yet.
    refusal_warned: AtomicBool,
    /// Whether a name unresolvable behind a proxy was logged at `warn` yet.
    proxy_warned: AtomicBool,
}

impl FetchKind {
    /// A kind of fetch; see the fields.
    pub(crate) const fn new(
        label: &'static str,
        noun: &'static str,
        plural: &'static str,
        allowed_by: &'static str,
        user_agent: Option<&'static str>,
    ) -> Self {
        Self {
            label,
            noun,
            plural,
            allowed_by,
            user_agent,
            refusal_warned: AtomicBool::new(false),
            proxy_warned: AtomicBool::new(false),
        }
    }

    /// Logs a refused destination: the first of the process at `warn`, naming
    /// what allows it, the rest at `debug`, since a playlist or a library can
    /// hold any number of them.
    fn refused(&self, host: &str, address: IpAddr) {
        if self.refusal_warned.swap(true, Ordering::Relaxed) {
            tracing::debug!(%host, %address, "{} refused: private address", self.noun);
        } else {
            tracing::warn!(
                %host,
                %address,
                "{} refused: the host is at a private address, which {} may not reach unless {}; \
                 further refusals are logged at debug",
                self.noun,
                self.plural,
                self.allowed_by,
            );
        }
    }

    /// Logs, once per process at `warn`, that behind a proxy a host the
    /// server cannot resolve is not sent: it could not be checked.
    fn unresolvable_behind_a_proxy(&self, host: &str) {
        if !self.proxy_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                %host,
                "{} not downloaded: behind an HTTP proxy, {} go only to hosts the server can \
                 resolve itself, to check their address; further ones are logged at debug",
                self.noun,
                self.plural,
            );
        }
    }

    /// A request that could not be sent, or a redirect that could not be
    /// followed: a failure of the provider, counted as the limiter counts one
    /// (reqwest's own redirect errors used to come back through it), and
    /// logged at `debug`, since a library can hold any number.
    fn unsent(&self, reason: &'static str) -> SendError {
        crate::rate_limit::note_request_failure();
        tracing::debug!(reason, "{} not downloaded", self.noun);
        SendError::Failed(reason)
    }
}

/// Why [`GuardedHttp::send`] has no response.
#[derive(Debug)]
pub(crate) enum SendError {
    /// A destination the policy refuses: a private address nothing exempts.
    /// The refusal's log line names the host and the address.
    Refused,
    /// The request through the origin's limiter failed: a transport failure
    /// or a cooldown.
    Request(RequestError),
    /// No request could be made: a URL that does not parse, a redirect to
    /// another scheme or past [`MAX_REDIRECTS`], a name a proxy would resolve
    /// that the server cannot, or a client that could not be built.
    Failed(&'static str),
}

/// A response, and whether a redirect led to it.
pub(crate) struct Fetched {
    /// The answer: never a redirect still to follow.
    pub(crate) response: reqwest::Response,
    /// Whether a redirect led to it.
    pub(crate) redirected: bool,
}

/// The clients a kind of fetch goes through, and the destinations it refuses
/// ([`DestinationPolicy`]).
pub(crate) struct GuardedHttp {
    /// Direct connections, with no proxy: every name it resolves goes through
    /// [`PublicOnlyResolver`], so the address it connects to is one the policy
    /// was asked about, even if the name's answer changed since the check (DNS
    /// rebinding). `None` when it could not be built: every fetch then
    /// fails, rather than going out unguarded.
    vetted: Option<reqwest::Client>,
    /// Set when the environment configures a proxy
    /// ([`destination::environment_proxy`]): the client that goes through it.
    /// The proxy resolves names itself, out of the resolver's sight, so a name
    /// is checked on the server's own answer before each request and pinned
    /// to it for the hosts the proxy settings exclude.
    proxied: Option<reqwest::Client>,
    policy: Arc<dyn DestinationPolicy>,
    kind: &'static FetchKind,
}

impl GuardedHttp {
    /// Clients refusing what `policy` refuses, through the environment's
    /// proxy when `proxy`.
    pub(crate) fn new(
        policy: Arc<dyn DestinationPolicy>,
        proxy: bool,
        kind: &'static FetchKind,
    ) -> Self {
        let vetted = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::new(PublicOnlyResolver::new(Arc::clone(&policy))))
            .build()
            .ok();
        let proxied = proxy.then(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .ok()
        });
        Self {
            vetted,
            proxied: proxied.flatten(),
            policy,
            kind,
        }
    }

    /// Clients refusing what `policy` refuses, through the process
    /// environment's proxy if it configures one.
    pub(crate) fn with_environment_proxy(
        policy: Arc<dyn DestinationPolicy>,
        kind: &'static FetchKind,
    ) -> Self {
        Self::new(
            policy,
            destination::environment_proxy(|name| std::env::var_os(name)),
            kind,
        )
    }

    /// GETs `url` through its origin's limiter, following redirects up to
    /// [`MAX_REDIRECTS`] and vetting the destination of each.
    pub(crate) async fn send(&self, url: &str) -> Result<Fetched, SendError> {
        let limiter = limiter_for(url, self.kind.label);
        let mut current =
            reqwest::Url::parse(url).map_err(|_| SendError::Failed("a URL that does not parse"))?;
        let mut redirects = 0;
        loop {
            let client = self.client_for(&current).await?;
            let mut request = client.get(current.clone());
            if let Some(agent) = self.kind.user_agent {
                request = request.header(reqwest::header::USER_AGENT, agent);
            }
            let response = request
                .send_limited(&limiter)
                .await
                .map_err(SendError::Request)?;
            let Some(next) = redirect_target(&response, &current) else {
                if crate::rate_limit::is_redirect_to_follow(&response) {
                    // The limiter took it for a hop to follow.
                    return Err(self
                        .kind
                        .unsent("a redirect whose Location cannot be followed"));
                }
                return Ok(Fetched {
                    response,
                    redirected: redirects > 0,
                });
            };
            if redirects == MAX_REDIRECTS {
                return Err(self.kind.unsent("too many redirects"));
            }
            redirects += 1;
            current = next;
        }
    }

    /// The client to send a request for `url` with, once its destination
    /// passed the policy.
    async fn client_for(&self, url: &reqwest::Url) -> Result<reqwest::Client, SendError> {
        let kind = self.kind;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(kind.unsent("a URL with a scheme other than http or https"));
        }
        let host = url
            .host_str()
            .ok_or_else(|| kind.unsent("a URL with no host"))?;
        // An address literal reaches no resolver, so it is vetted here.
        if let Ok(address) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
            if self.policy.refuses(host, address).await {
                kind.refused(host, address);
                return Err(SendError::Refused);
            }
            return self
                .proxied
                .as_ref()
                .or(self.vetted.as_ref())
                .cloned()
                .ok_or_else(|| kind.unsent("the HTTP client could not be built"));
        }
        // A name is vetted here too, so that a refusal is told apart from a
        // connection failure; the vetted client's resolver vets it again when
        // it connects. One that does not resolve, or not within the request
        // timeout, goes on to fail the way it always did, as a connection
        // failure, unless a proxy would resolve it.
        let port = url.port_or_known_default().unwrap_or_default();
        let lookup = destination::resolve_vetted(self.policy.as_ref(), host, port);
        let vetted = match tokio::time::timeout(request_limits().timeout, lookup).await {
            Ok(Ok(addresses)) => Some(addresses),
            Ok(Err(DestinationRefused::Private { host, address })) => {
                kind.refused(&host, address);
                return Err(SendError::Refused);
            }
            Ok(Err(_)) | Err(_) => None,
        };
        if self.proxied.is_none() {
            return self
                .vetted
                .clone()
                .ok_or_else(|| kind.unsent("the HTTP client could not be built"));
        }
        // Through the proxy, or straight to the vetted addresses for a host
        // the proxy settings exclude.
        let Some(addresses) = vetted else {
            kind.unresolvable_behind_a_proxy(host);
            return Err(kind.unsent("a host the server cannot resolve, behind a proxy"));
        };
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(host, &addresses)
            .build()
            .map_err(|_| kind.unsent("the HTTP client could not be built"))
    }
}

#[cfg(test)]
impl GuardedHttp {
    /// Clients that refuse nothing, for a test serving on loopback.
    pub(crate) fn open(kind: &'static FetchKind) -> Self {
        let nothing: fn(IpAddr) -> bool = |_| false;
        Self::new(Arc::new(nothing), false, kind)
    }
}

/// Where a redirect response sends the request next: the statuses reqwest's
/// own policy follows (301, 302, 303, 307, 308), with a `Location` that
/// resolves against `current`. `None` for any other response, which is the
/// answer.
fn redirect_target(response: &reqwest::Response, current: &reqwest::Url) -> Option<reqwest::Url> {
    if !crate::rate_limit::REDIRECT_STATUSES.contains(&response.status()) {
        return None;
    }
    let location = response.headers().get(reqwest::header::LOCATION)?;
    // UTF-8, not just visible ASCII, as reqwest's own policy reads it.
    current
        .join(std::str::from_utf8(location.as_bytes()).ok()?)
        .ok()
}

#[cfg(test)]
mod tests {
    /// The label never picks the gate: an artwork download and a subtitle
    /// download from one origin share its pacing and cooldowns, each counted
    /// under its own provider; another origin has a gate of its own.
    #[test]
    fn two_labels_on_one_origin_share_one_gate() {
        let artwork = super::limiter_for("https://cdn.example/poster.jpg", crate::metrics::IMAGE);
        let subtitle = super::limiter_for("https://cdn.example/file.srt", "opensubtitles");
        let other = super::limiter_for("https://elsewhere.example/poster.jpg", "image");
        assert!(artwork.shares_gate_with(&subtitle));
        assert!(!artwork.shares_gate_with(&other));
        assert_eq!(artwork.metric_label(), "image");
        assert_eq!(subtitle.metric_label(), "opensubtitles");
    }
}
