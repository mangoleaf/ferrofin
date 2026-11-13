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
//! divergence (owner decisions 2026-10-31 and 2026-11-03). Like that client,
//! a fetch never follows a redirect from https to http, and its clients have
//! the provider defaults ([`crate::http_client`]).

use crate::destination::{self, DestinationPolicy, DestinationRefused, PublicOnlyResolver};
use crate::rate_limit::{LimitedRequest as _, RateLimiter, RequestError, request_limits};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// Most redirects one fetch follows: reqwest's default policy
/// (`redirect::Policy::default()`), which downloads went by until they followed
/// redirects themselves, to vet each hop, and which the provider client
/// ([`crate::http_client`]) keeps.
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
        .unwrap_or_else(PoisonError::into_inner)
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
    /// another scheme, from https to http or past [`MAX_REDIRECTS`], a name a
    /// proxy would resolve that the server cannot, or a client that could not
    /// be built.
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
    /// to it ([`VettedAddresses`]) for the hosts the proxy settings exclude.
    /// One client serves every request, so its connections are reused
    /// (gl-1270).
    proxied: Option<reqwest::Client>,
    /// The proxied client's resolver.
    pinned: Arc<VettedAddresses>,
    policy: Arc<dyn DestinationPolicy>,
    kind: &'static FetchKind,
}

/// The proxied client's resolver: a name resolves to the addresses
/// [`GuardedHttp::client_for`] last vetted for it, which is where a host the
/// proxy settings exclude is reached. A name never vetted, which is the
/// proxy's own, gets the system's answer unchecked, since a proxy may well be
/// private. A client per request with `resolve_to_addrs` did the same, but
/// loaded the root certificates and connected afresh each time.
///
/// An entry is replaced by the next request to its host and never removed:
/// a connection the pool finishes after its request has ended must still
/// find it. So the map holds one entry per host fetched through the proxy.
#[derive(Default)]
struct VettedAddresses(Mutex<HashMap<String, Vec<SocketAddr>>>);

impl VettedAddresses {
    /// Sends `host` to `addresses`, just vetted for it, from now on. The port
    /// is dropped: each connection takes its own URL's port. (Hosts compare
    /// in lower case on both sides, so that a miss never falls through to the
    /// unchecked lookup.)
    fn pin(&self, host: &str, addresses: &[SocketAddr]) {
        let addresses = addresses
            .iter()
            .map(|address| {
                // `set_port`, not a new address: an IPv6 zone stays.
                let mut address = *address;
                address.set_port(0);
                address
            })
            .collect();
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(host.to_ascii_lowercase(), addresses);
    }
}

impl reqwest::dns::Resolve for VettedAddresses {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let pinned = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&name.as_str().to_ascii_lowercase())
            .cloned();
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addresses = match pinned {
                Some(addresses) => addresses,
                None => destination::lookup(&host, 0).await.map_err(|failed| {
                    Box::new(failed) as Box<dyn std::error::Error + Send + Sync>
                })?,
            };
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

impl GuardedHttp {
    /// Clients refusing what `policy` refuses, through the environment's
    /// proxy when `proxy`.
    pub(crate) fn new(
        policy: Arc<dyn DestinationPolicy>,
        proxy: bool,
        kind: &'static FetchKind,
    ) -> Self {
        let vetted = crate::http_client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::new(PublicOnlyResolver::new(Arc::clone(&policy))))
            .build()
            .ok();
        let pinned = Arc::new(VettedAddresses::default());
        let proxied = proxy.then(|| {
            crate::http_client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .dns_resolver(Arc::clone(&pinned))
                .build()
                .ok()
        });
        Self {
            vetted,
            proxied: proxied.flatten(),
            pinned,
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
        // Every URL asked for so far, as reqwest's redirect policy sees them.
        let mut previous = vec![current.clone()];
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
                    redirected: previous.len() > 1,
                });
            };
            // The provider client's rule: past MAX_REDIRECTS, or https to http.
            if let Some(reason) = crate::http_client::redirect_refusal(&previous, &next) {
                return Err(self.kind.unsent(reason));
            }
            previous.push(next.clone());
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
        let Some(proxied) = &self.proxied else {
            return self
                .vetted
                .clone()
                .ok_or_else(|| kind.unsent("the HTTP client could not be built"));
        };
        // Through the proxy, or straight to the vetted addresses for a host
        // the proxy settings exclude.
        let Some(addresses) = vetted else {
            kind.unresolvable_behind_a_proxy(host);
            return Err(kind.unsent("a host the server cannot resolve, behind a proxy"));
        };
        self.pinned.pin(host, &addresses);
        Ok(proxied.clone())
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
    static KIND: super::FetchKind =
        super::FetchKind::new(crate::metrics::IMAGE, "image", "images", "nothing", None);

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

    /// gl-1270: with a proxy configured, the requests to a named host go
    /// through one client, so the second reuses the first's connection
    /// instead of building a client and connecting again. (No proxy is set
    /// in the test's environment, so the client reaches the host directly, at
    /// the addresses vetted for it.)
    #[tokio::test]
    async fn behind_a_proxy_requests_to_a_named_host_share_a_connection() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        // Held on `::1` too, which `localhost` tries first (gl-1387).
        let (listener, _ipv6) = crate::mock_http::loopback_listener().await;
        let port = listener.local_addr().expect("address").port();
        let connections = std::sync::Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn({
            let connections = std::sync::Arc::clone(&connections);
            async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    connections.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(async move {
                        let (mut buffer, mut request) = ([0; 4096], Vec::new());
                        while let Ok(read @ 1..) = socket.read(&mut buffer).await {
                            request.extend_from_slice(&buffer[..read]);
                            // A GET has no body: its head ends the request.
                            if !request.ends_with(b"\r\n\r\n") {
                                continue;
                            }
                            request.clear();
                            let kept_alive = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
                            if socket.write_all(kept_alive).await.is_err() {
                                break;
                            }
                        }
                    });
                }
            }
        });
        let nothing: fn(std::net::IpAddr) -> bool = |_| false;
        let http = super::GuardedHttp::new(std::sync::Arc::new(nothing), true, &KIND);
        for _ in 0..2 {
            let fetched = http
                .send(&format!("http://localhost:{port}/a.png"))
                .await
                .expect("sent");
            assert_eq!(fetched.response.text().await.expect("body"), "ok");
        }
        server.abort();
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        // It went to the addresses vetted for the name, pinned without a port.
        let pins = http.pinned.0.lock().expect("pins");
        let vetted = pins.get("localhost").expect("a pin for the name");
        assert!(
            vetted
                .iter()
                .all(|address| address.ip().is_loopback() && address.port() == 0),
            "{vetted:?}"
        );
    }

    /// The proxied client's resolver answers a vetted name with the addresses
    /// vetted for it, without their port, so that a request to another port
    /// of the host still takes its own; any other name, the proxy's, with the
    /// system's answer.
    #[tokio::test]
    async fn the_proxied_resolver_answers_a_vetted_name_with_its_vetted_addresses() {
        use reqwest::dns::Resolve as _;
        let pinned = super::VettedAddresses::default();
        pinned.pin(
            "Images.Example",
            &["203.0.113.7:8080".parse().expect("address")],
        );
        let vetted: Vec<std::net::SocketAddr> = pinned
            .resolve("images.example".parse().expect("name"))
            .await
            .expect("pinned")
            .collect();
        assert_eq!(vetted, ["203.0.113.7:0".parse().expect("address")]);
        let mut proxy = pinned
            .resolve("localhost".parse().expect("name"))
            .await
            .expect("the system's answer");
        assert!(proxy.all(|address| address.ip().is_loopback()));
    }

    /// The proxied client reaches a named host at the addresses last pinned
    /// for it, whatever the system would answer (this name it cannot), and on
    /// the URL's port, not the pin's.
    #[tokio::test]
    async fn the_proxied_client_connects_to_the_addresses_last_pinned_for_a_name() {
        use crate::mock_http::MockServer;
        let server = MockServer::start_raw(vec![MockServer::raw_ok("", b"ok")]).await;
        let port = server.base_url.rsplit(':').next().expect("port");
        let url = format!("http://pinned.invalid:{port}/a.png");
        let nothing: fn(std::net::IpAddr) -> bool = |_| false;
        let http = super::GuardedHttp::new(std::sync::Arc::new(nothing), true, &KIND);
        let proxied = http.proxied.clone().expect("a proxied client");

        // The server listens on 127.0.0.1 only.
        http.pinned
            .pin("pinned.invalid", &["127.0.0.2:9".parse().expect("address")]);
        assert!(proxied.get(&url).send().await.is_err(), "nothing there");
        http.pinned
            .pin("pinned.invalid", &["127.0.0.1:9".parse().expect("address")]);
        let response = proxied.get(&url).send().await.expect("the new pin");
        assert_eq!(response.text().await.expect("body"), "ok");
        assert_eq!(server.requests(), 1);
    }
}
