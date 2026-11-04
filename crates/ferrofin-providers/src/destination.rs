//! Which addresses an outbound fetch may reach.
//!
//! A URL a content source chose (a playlist's channel logo, a guide's
//! artwork, a metadata provider's image, a WASM plugin's request) must not
//! make the server a way into the networks it sits on: its own `/metrics`, a
//! router or NAS page, a cloud metadata service. This module holds the one
//! guard for that, used by the image downloads, the OpenSubtitles download
//! link and the WASM plugin host:
//! [`is_private_address`] names the ranges, a [`DestinationPolicy`] decides
//! which of them a fetch may still reach, and [`PublicOnlyResolver`] applies
//! the policy to every name a client resolves.
//!
//! Jellyfin has no destination policy (`Jellyfin.Server/Startup.cs` gives its
//! HTTP clients redirects and no address filter). The guard is an accepted
//! divergence (owner decision 2026-10-31, gl-1096), recorded on the `images`
//! row of `ferrofin_api::handlers::VERIFIED_SUBSYSTEMS`: image downloads
//! refuse private addresses unless the admin lists them ([`PrivateAllowList`])
//! or they are a configured Live TV tuner's host ([`ImageDestinations`]).
//! The OpenSubtitles download link has its own list,
//! `subtitle_private_http_allow` (owner decision 2026-11-03, gl-1160): the
//! image list does not apply to it.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

use async_trait::async_trait;
use ferrofin_model::net::IpNetwork;

/// Whether an address is in a range the private-destination policy denies
/// by default: loopback, link-local (incl. cloud metadata services),
/// RFC1918, CGNAT (`100.64.0.0/10` — Tailscale's default range), IPv6 ULA,
/// and the reserved/multicast blocks a public HTTP fetch should never
/// target. An IPv6 address that carries an IPv4 one reaches that IPv4 host,
/// so it takes the IPv4 rule: IPv4-mapped (`::ffff:0:0/96`),
/// IPv4-compatible (`::/96`, RFC 4291), NAT64 (`64:ff9b::/96`, RFC 6052) and
/// 6to4 (`2002::/16`, RFC 3056). The local-use NAT64 prefix `64:ff9b:1::/48`
/// (RFC 8215) is not globally reachable, so it is private whatever it
/// embeds.
///
/// The CGNAT and reserved-block checks are open-coded because
/// `Ipv4Addr::is_shared`/`is_reserved` are still nightly-only and we pin
/// stable.
#[must_use]
pub fn is_private_address(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_link_local()
                || v4.is_private()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()               // 224.0.0.0/4
                // CGNAT 100.64.0.0/10 (Tailscale, carrier NAT).
                || (a == 100 && (64..128).contains(&b))
                // Benchmarking 198.18.0.0/15.
                || (a == 198 && (b == 18 || b == 19))
                // IETF protocol assignments 192.0.0.0/24.
                || (a == 192 && b == 0 && v4.octets()[2] == 0)
                // Reserved for future use 240.0.0.0/4 (excl. broadcast, above).
                || a >= 240
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            let segments = v6.segments();
            let from_segments =
                |hi: u16, lo: u16| std::net::Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo));
            // `to_ipv4` is the mapped and the compatible form; `::1` and `::`
            // are caught above, since the compatible form reads them as
            // `0.0.0.1` and `0.0.0.0`.
            let embedded = v6.to_ipv4().or_else(|| match segments {
                [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] | [0x2002, hi, lo, ..] => {
                    Some(from_segments(hi, lo))
                }
                _ => None,
            });
            if let Some(v4) = embedded {
                return is_private_address(IpAddr::V4(v4));
            }
            v6.is_multicast()
                || v6.is_unicast_link_local()
                || v6.is_unique_local()
                || matches!(segments, [0x64, 0xff9b, 1, ..])
        }
    }
}

/// Which destinations a guarded fetch refuses.
#[async_trait]
pub trait DestinationPolicy: Send + Sync {
    /// Whether `host` (a URL's host: a name, or an address literal) may not
    /// be reached at `address`.
    async fn refuses(&self, host: &str, address: IpAddr) -> bool;
}

/// A plain address test: [`is_private_address`] for "every private address,
/// whatever the host", or a test's relaxed rule.
#[async_trait]
impl DestinationPolicy for fn(IpAddr) -> bool {
    async fn refuses(&self, _host: &str, address: IpAddr) -> bool {
        self(address)
    }
}

/// Why a destination was refused. Carried through reqwest's error chain by
/// [`PublicOnlyResolver`], so a caller can tell its user exactly that.
#[derive(Debug)]
pub enum DestinationRefused {
    /// The name resolves to an address the policy denies.
    Private {
        /// The URL's host.
        host: String,
        /// The address the policy denies.
        address: IpAddr,
    },
    /// The name does not resolve.
    Unresolvable {
        /// The URL's host.
        host: String,
        /// The resolver's error.
        error: std::io::Error,
    },
    /// The name resolves to nothing.
    Empty {
        /// The URL's host.
        host: String,
    },
}

impl std::fmt::Display for DestinationRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Private { host, address } => {
                write!(f, "`{host}` resolves to the private address {address}")
            }
            Self::Unresolvable { host, error } => write!(f, "could not resolve `{host}`: {error}"),
            Self::Empty { host } => write!(f, "`{host}` resolved to no addresses"),
        }
    }
}

impl std::error::Error for DestinationRefused {}

/// Resolves `host` with the system lookup, which blocks, on a blocking
/// thread (as reqwest's own resolver does). `port` only completes the
/// socket addresses.
///
/// # Errors
/// The lookup failed, or found no address.
pub(crate) async fn lookup(host: &str, port: u16) -> Result<Vec<SocketAddr>, DestinationRefused> {
    let name = host.to_owned();
    let resolved = tokio::task::spawn_blocking(move || {
        std::net::ToSocketAddrs::to_socket_addrs(&(name.as_str(), port))
            .map(Iterator::collect::<Vec<_>>)
    })
    .await
    .map_err(|join| DestinationRefused::Unresolvable {
        host: host.to_owned(),
        error: std::io::Error::other(join),
    })?;
    let addresses = resolved.map_err(|error| DestinationRefused::Unresolvable {
        host: host.to_owned(),
        error,
    })?;
    if addresses.is_empty() {
        return Err(DestinationRefused::Empty {
            host: host.to_owned(),
        });
    }
    Ok(addresses)
}

/// Resolves `host` and checks every address it has against `policy`: one
/// refused address refuses the name, since the connection may use any.
///
/// # Errors
/// The name does not resolve, or `policy` refuses one of its addresses.
pub(crate) async fn resolve_vetted(
    policy: &dyn DestinationPolicy,
    host: &str,
    port: u16,
) -> Result<Vec<SocketAddr>, DestinationRefused> {
    let addresses = lookup(host, port).await?;
    for address in &addresses {
        if policy.refuses(host, address.ip()).await {
            return Err(DestinationRefused::Private {
                host: host.to_owned(),
                address: address.ip(),
            });
        }
    }
    Ok(addresses)
}

/// A client's DNS resolver: the system lookup, refused when the policy
/// refuses ANY address of the name. It vets every resolution the client
/// makes, new pooled connections and redirect hops included, and the client
/// connects only to the addresses it returned, so a short-TTL record that
/// alternates public and private cannot win a re-resolve race (the
/// DNS-rebinding TOCTOU). An address literal never reaches a resolver: the
/// caller vets those itself. Nor does a name sent to a proxy, which resolves
/// it itself.
pub struct PublicOnlyResolver {
    policy: Arc<dyn DestinationPolicy>,
}

impl PublicOnlyResolver {
    /// A resolver refusing what `policy` refuses.
    #[must_use]
    pub fn new(policy: Arc<dyn DestinationPolicy>) -> Self {
        Self { policy }
    }
}

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let policy = Arc::clone(&self.policy);
        Box::pin(async move {
            let addresses = resolve_vetted(policy.as_ref(), &host, 0)
                .await
                .map_err(|refused| Box::new(refused) as Box<dyn std::error::Error + Send + Sync>)?;
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Whether the environment configures an outbound proxy reqwest would use:
/// a non-empty `ALL_PROXY`, `HTTP_PROXY` or `HTTPS_PROXY`, in either case.
/// `var` reads the environment (`std::env::var_os`, or a test's map).
///
/// A proxy resolves the destination itself, out of a resolver's sight, so a
/// guarded client that honours it must vet each name before it sends.
/// Over-reporting only sends fetches down that path, which vets them too.
pub fn environment_proxy(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    [
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
    ]
    .iter()
    .any(|name| var(name).is_some_and(|value| !value.is_empty()))
}

/// `host` as hosts are compared: lower case, without the brackets of an IPv6
/// literal or a trailing dot.
fn normalized_host(host: &str) -> String {
    host.trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// The host of an absolute `http`/`https` URL, as hosts are compared. `None`
/// for anything else, a filesystem path included.
#[must_use]
pub fn url_host(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.host_str()
        .filter(|host| !host.is_empty())
        .map(normalized_host)
}

/// The private hosts and networks an admin lets a kind of download reach
/// (`image_private_http_allow`, `subtitle_private_http_allow`): host names,
/// matched exactly in any case, and addresses or networks in CIDR form
/// (`192.168.1.20`, `10.0.0.0/8`, `fd00::/8`). An address listed this way
/// is allowed whatever name led to it; a host name, at whatever address it
/// resolves to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrivateAllowList {
    hosts: Vec<String>,
    networks: Vec<IpNetwork>,
}

impl PrivateAllowList {
    /// Parses the comma-separated entries of `raw`, blank ones skipped.
    /// Returns the list and each entry it refused with the reason, for the
    /// caller to warn about: a refused entry allows nothing.
    #[must_use]
    pub fn parse(raw: &str) -> (Self, Vec<(String, &'static str)>) {
        let mut list = Self::default();
        let mut refused = Vec::new();
        for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            match Self::entry(entry) {
                Ok(Entry::Host(host)) => list.hosts.push(host),
                Ok(Entry::Network(network)) => list.networks.push(network),
                Err(reason) => refused.push((entry.to_owned(), reason)),
            }
        }
        (list, refused)
    }

    /// One entry: a network in CIDR form, an address, or a host name.
    fn entry(entry: &str) -> Result<Entry, &'static str> {
        if entry.contains('/') {
            return ferrofin_networking::net_utils::try_parse_to_subnet(entry, false)
                .map(|data| Entry::Network(data.subnet))
                .ok_or("is not a network in CIDR form (address/prefix length)");
        }
        if entry.contains('*') {
            return Err("is a wildcard; write 0.0.0.0/0, ::/0 to allow every address");
        }
        if let Ok(address) = entry.trim_matches(['[', ']']).parse::<IpAddr>() {
            if address.is_unspecified() {
                return Err("names no host; write 0.0.0.0/0 or ::/0 to allow every address");
            }
            let length = if address.is_ipv4() { 32 } else { 128 };
            return Ok(Entry::Network(IpNetwork::new(address, length)));
        }
        // A host name is what a URL's host would be, and nothing else: no
        // port, path or credentials, and no form a URL would rewrite.
        let url = reqwest::Url::parse(&format!("http://{entry}/"))
            .map_err(|_| "is not a host name, an address or a network")?;
        let host = url.host_str().map(normalized_host);
        if host.is_none()
            || host.as_deref() != Some(normalized_host(entry).as_str())
            || url.port().is_some()
            || url.path() != "/"
            || !url.username().is_empty()
        {
            return Err("is not a host name, an address or a network (no port, path or scheme)");
        }
        Ok(Entry::Host(host.unwrap_or_default()))
    }

    /// Whether the list lets `host`, at `address`, be reached.
    #[must_use]
    pub fn allows(&self, host: &str, address: IpAddr) -> bool {
        self.networks.iter().any(|network| {
            ferrofin_networking::net_utils::subnet_contains_address(*network, address)
        }) || (!self.hosts.is_empty() && self.hosts.contains(&normalized_host(host)))
    }

    /// Whether the list allows nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty() && self.networks.is_empty()
    }
}

/// The list alone as a policy: every private address it does not list is
/// refused (the subtitle links', which no tuner exempts).
#[async_trait]
impl DestinationPolicy for PrivateAllowList {
    async fn refuses(&self, host: &str, address: IpAddr) -> bool {
        is_private_address(address) && !self.allows(host, address)
    }
}

/// One parsed [`PrivateAllowList`] entry.
enum Entry {
    Host(String),
    Network(IpNetwork),
}

/// The hosts of the configured Live TV tuners, read when a private
/// destination needs them, so a tuner added or removed counts at once.
pub type TunerHostSource =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Vec<String>> + Send>> + Send + Sync>;

/// What image downloads may reach beyond the public internet: the admin's
/// [`PrivateAllowList`], and the hosts of the configured Live TV tuners, so a
/// tvheadend `/imagecache/` or an HDHomeRun logo on the LAN keeps working.
/// The server sets it for each of its lifetimes
/// ([`set_image_destinations`]); until then every private address is
/// refused.
#[derive(Clone, Default)]
pub struct ImageDestinations {
    allowed: PrivateAllowList,
    tuner_hosts: Option<TunerHostSource>,
}

impl std::fmt::Debug for ImageDestinations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageDestinations")
            .field("allowed", &self.allowed)
            .field("tuner_hosts", &self.tuner_hosts.is_some())
            .finish()
    }
}

impl ImageDestinations {
    /// The destinations `allowed` lists, and no tuner's.
    #[must_use]
    pub fn new(allowed: PrivateAllowList) -> Self {
        Self {
            allowed,
            tuner_hosts: None,
        }
    }

    /// Also lets the hosts `source` names be reached: the configured Live TV
    /// tuners' (see [`url_host`]).
    #[must_use]
    pub fn with_tuner_hosts(mut self, source: TunerHostSource) -> Self {
        self.tuner_hosts = Some(source);
        self
    }
}

#[async_trait]
impl DestinationPolicy for ImageDestinations {
    async fn refuses(&self, host: &str, address: IpAddr) -> bool {
        if !is_private_address(address) || self.allowed.allows(host, address) {
            return false;
        }
        let Some(source) = &self.tuner_hosts else {
            return true;
        };
        let host = normalized_host(host);
        !source()
            .await
            .iter()
            .any(|tuner| normalized_host(tuner) == host)
    }
}

/// The [`ImageDestinations`] in force.
static IMAGE_DESTINATIONS: LazyLock<RwLock<Arc<ImageDestinations>>> =
    LazyLock::new(RwLock::default);

/// Sets what image downloads may reach from their next request on. The
/// server calls it once per lifetime, before any image is downloaded, with
/// its configuration's allow-list and its Live TV tuners.
pub fn set_image_destinations(destinations: ImageDestinations) {
    *IMAGE_DESTINATIONS
        .write()
        .unwrap_or_else(PoisonError::into_inner) = Arc::new(destinations);
}

/// The policy the image downloads apply: the last [`set_image_destinations`],
/// read at each check.
pub(crate) struct ConfiguredImageDestinations;

#[async_trait]
impl DestinationPolicy for ConfiguredImageDestinations {
    async fn refuses(&self, host: &str, address: IpAddr) -> bool {
        let destinations = Arc::clone(
            &IMAGE_DESTINATIONS
                .read()
                .unwrap_or_else(PoisonError::into_inner),
        );
        destinations.refuses(host, address).await
    }
}

/// Test-only hooks for other crates' tests. Enabled by the `test-util`
/// feature, which only `[dev-dependencies]` may turn on.
#[cfg(any(test, feature = "test-util"))]
pub mod test_support {
    /// Lets image downloads reach loopback, where tests serve their images.
    /// Nothing else private.
    pub fn allow_loopback_images() {
        let (allowed, _) = super::PrivateAllowList::parse("127.0.0.0/8, ::1");
        super::set_image_destinations(super::ImageDestinations::new(allowed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("127.0.0.1")]
    #[case::cloud_metadata("169.254.169.254")]
    #[case::rfc1918("10.1.2.3")]
    #[case("172.16.0.1")]
    #[case("192.168.1.1")]
    #[case::cgnat("100.64.0.1")]
    #[case::cgnat_interior("100.100.100.100")]
    #[case::benchmarking("198.18.0.5")]
    #[case::ietf_protocol_assignments("192.0.0.1")]
    #[case::reserved("240.0.0.1")]
    #[case::multicast("224.0.0.1")]
    #[case("::1")]
    #[case("fe80::1")]
    #[case("fd00::1")]
    #[case::v6_multicast("ff02::1")]
    #[case::v4_mapped_rfc1918("::ffff:10.0.0.1")]
    #[case::v4_mapped_cgnat("::ffff:100.64.0.1")]
    // IPv6 forms that carry a private IPv4 reach it through a NAT64 gateway,
    // a 6to4 relay or IPv4-compatible routing.
    #[case::nat64_cloud_metadata("64:ff9b::a9fe:a9fe")]
    #[case::nat64_loopback("64:ff9b::7f00:1")]
    #[case::local_use_nat64("64:ff9b:1::1")]
    #[case::sixto4_loopback("2002:7f00:1::")]
    #[case::sixto4_rfc1918("2002:c0a8:101::1")]
    #[case::sixto4_ten("2002:a00:1::")]
    #[case::v4_compatible_loopback("::127.0.0.1")]
    #[case::v4_compatible_rfc1918("::10.0.0.1")]
    fn private_addresses(#[case] address: &str) {
        assert!(is_private_address(address.parse().expect("address")));
    }

    #[rstest]
    #[case("1.1.1.1")]
    #[case("8.8.8.8")]
    #[case("93.184.216.34")]
    #[case::below_cgnat("100.63.255.255")]
    #[case::above_cgnat("100.128.0.0")]
    #[case("2606:4700:4700::1111")]
    #[case::nat64_public("64:ff9b::808:808")]
    #[case::sixto4_public("2002:808:808::1")]
    #[case::v4_compatible_public("::8.8.8.8")]
    #[case::past_local_use_nat64("64:ff9b:2::1")]
    #[case::not_the_nat64_prefix("64:ff9b:0:0:1::1")]
    fn public_addresses(#[case] address: &str) {
        assert!(!is_private_address(address.parse().expect("address")));
    }

    #[test]
    fn an_environment_proxy_is_any_non_empty_proxy_variable() {
        let env = |set: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                set.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| std::ffi::OsString::from(*value))
            }
        };
        assert!(!environment_proxy(env(&[])));
        assert!(!environment_proxy(env(&[("HTTP_PROXY", "")])));
        assert!(!environment_proxy(env(&[("NO_PROXY", "*")])));
        for name in [
            "ALL_PROXY",
            "all_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
        ] {
            let set: &'static [(&'static str, &'static str)] =
                Box::leak(vec![(name, "http://proxy:3128")].into_boxed_slice());
            assert!(environment_proxy(env(set)), "{name}");
        }
    }

    #[rstest]
    #[case::network("192.168.1.0/24", "nas.lan", "192.168.1.20", true)]
    #[case::outside_the_network("192.168.1.0/24", "nas.lan", "192.168.2.20", false)]
    #[case::host_bits_cleared("192.168.1.77/24", "x", "192.168.1.5", true)]
    #[case::mapped_address_in_a_v4_network("10.0.0.0/8", "x", "::ffff:10.1.2.3", true)]
    #[case::v6_network("fd00::/8", "x", "fd12::1", true)]
    #[case::address("192.168.1.20", "nas.lan", "192.168.1.20", true)]
    #[case::other_address("192.168.1.20", "nas.lan", "192.168.1.21", false)]
    #[case::bracketed_v6("[fd00::5]", "x", "fd00::5", true)]
    #[case::host_name_any_address("NAS.lan", "nas.LAN", "10.9.9.9", true)]
    #[case::host_name_trailing_dot("nas.lan", "nas.lan.", "10.9.9.9", true)]
    #[case::other_host_name("nas.lan", "router.lan", "10.9.9.9", false)]
    #[case::every_address("0.0.0.0/0", "x", "127.0.0.1", true)]
    #[case::several("nas.lan, 10.0.0.0/8", "x", "10.1.1.1", true)]
    fn the_allow_list_matches_hosts_addresses_and_networks(
        #[case] raw: &str,
        #[case] host: &str,
        #[case] address: &str,
        #[case] allowed: bool,
    ) {
        let (list, refused) = PrivateAllowList::parse(raw);
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(
            list.allows(host, address.parse().expect("address")),
            allowed
        );
    }

    #[rstest]
    #[case::port("nas.lan:8080")]
    #[case::path("nas.lan/images")]
    #[case::scheme("http://nas.lan")]
    #[case::credentials("user@nas.lan")]
    #[case::space("nas lan")]
    #[case::bad_prefix("10.0.0.0/33")]
    #[case::bad_network("10.0.0/8x")]
    #[case::negated("!10.0.0.0/8")]
    #[case::unspecified("0.0.0.0")]
    #[case::unspecified_v6("::")]
    #[case::rewritten_by_a_url("0x7f.1")]
    #[case::wildcard("*")]
    #[case::wildcard_domain("*.lan")]
    fn a_bad_entry_is_refused_and_allows_nothing(#[case] entry: &str) {
        let (list, refused) = PrivateAllowList::parse(&format!("{entry}, nas.lan"));
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert_eq!(refused[0].0, entry);
        assert_eq!(list, PrivateAllowList::parse("nas.lan").0);
    }

    #[test]
    fn blank_entries_are_skipped() {
        let (list, refused) = PrivateAllowList::parse(" , ,");
        assert!(list.is_empty());
        assert_eq!(refused, []);
        assert!(!PrivateAllowList::parse("nas.lan").0.is_empty());
    }

    #[rstest]
    #[case("http://192.168.1.50:5004/lineup.json", Some("192.168.1.50"))]
    #[case("https://TVH.lan:9981/playlist", Some("tvh.lan"))]
    #[case("http://[fd00::5]:80/", Some("fd00::5"))]
    #[case::a_file("/config/channels.m3u", None)]
    #[case::another_scheme("rtsp://cam.lan/stream", None)]
    #[case::no_scheme("192.168.1.50", None)]
    fn a_url_host(#[case] url: &str, #[case] host: Option<&str>) {
        assert_eq!(url_host(url).as_deref(), host);
    }

    fn tuners(hosts: &'static [&'static str]) -> TunerHostSource {
        Arc::new(move || Box::pin(async move { hosts.iter().map(|h| (*h).to_owned()).collect() }))
    }

    #[tokio::test]
    async fn image_destinations_refuse_private_addresses_but_what_they_list() {
        let loopback: IpAddr = "127.0.0.1".parse().expect("address");
        let lan: IpAddr = "192.168.1.50".parse().expect("address");
        let public: IpAddr = "93.184.216.34".parse().expect("address");

        let nothing = ImageDestinations::default();
        assert!(nothing.refuses("127.0.0.1", loopback).await);
        assert!(nothing.refuses("tvh.lan", lan).await);
        assert!(!nothing.refuses("example.com", public).await);

        let listed = ImageDestinations::new(PrivateAllowList::parse("nas.lan").0)
            .with_tuner_hosts(tuners(&["tvh.lan", "192.168.1.50"]));
        assert!(!listed.refuses("TVH.lan", lan).await, "a tuner's host name");
        assert!(
            !listed.refuses("192.168.1.50", lan).await,
            "a tuner's address"
        );
        assert!(!listed.refuses("nas.lan", loopback).await, "a listed host");
        assert!(listed.refuses("router.lan", lan).await, "a name for it");
        assert!(listed.refuses("localhost", loopback).await);
        assert!(!listed.refuses("example.com", public).await);
    }

    /// The subtitle links' policy is the list alone: no tuner exempts a
    /// host, and an empty list refuses every private address.
    #[tokio::test]
    async fn an_allow_list_alone_refuses_every_private_address_it_does_not_list() {
        let lan: IpAddr = "192.168.1.50".parse().expect("address");
        let public: IpAddr = "93.184.216.34".parse().expect("address");
        let nothing = PrivateAllowList::default();
        assert!(nothing.refuses("tvh.lan", lan).await);
        assert!(!nothing.refuses("example.com", public).await);
        let listed = PrivateAllowList::parse("mirror.lan, 10.0.0.0/8").0;
        assert!(!listed.refuses("mirror.lan", lan).await, "a listed host");
        assert!(
            !listed
                .refuses("x", "10.1.2.3".parse().expect("address"))
                .await,
            "a listed network"
        );
        assert!(listed.refuses("tvh.lan", lan).await);
    }

    /// The image downloads read the configured policy. (Every policy a test
    /// of this crate sets lets loopback through and nothing else private, so
    /// the tests serving images there never see another's.)
    #[tokio::test]
    async fn the_configured_image_destinations_apply() {
        test_support::allow_loopback_images();
        let lan: IpAddr = "192.168.1.50".parse().expect("address");
        let loopback: IpAddr = "127.0.0.1".parse().expect("address");
        assert!(ConfiguredImageDestinations.refuses("nas.lan", lan).await);
        assert!(
            !ConfiguredImageDestinations
                .refuses("localhost", loopback)
                .await
        );
    }

    #[tokio::test]
    async fn a_name_is_refused_when_any_of_its_addresses_is() {
        let refused = resolve_vetted(&(is_private_address as fn(IpAddr) -> bool), "localhost", 80)
            .await
            .expect_err("loopback");
        assert!(
            matches!(&refused, DestinationRefused::Private { host, address }
                if host == "localhost" && address.is_loopback()),
            "{refused}"
        );
        let open: fn(IpAddr) -> bool = |_| false;
        let addresses = resolve_vetted(&open, "localhost", 80)
            .await
            .expect("allowed");
        assert!(addresses.iter().all(|a| a.port() == 80));
        assert!(matches!(
            lookup("name.invalid", 80).await,
            Err(DestinationRefused::Unresolvable { .. } | DestinationRefused::Empty { .. })
        ));
    }
}
