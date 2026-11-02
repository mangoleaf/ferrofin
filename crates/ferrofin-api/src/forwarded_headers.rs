//! `ForwardedHeadersMiddleware`'s scheme and host, as Jellyfin configures it.
//!
//! Jellyfin trusts `X-Forwarded-For`, `X-Forwarded-Proto` and `X-Forwarded-Host`
//! from its known proxies only (`ApiServiceCollectionExtensions.ConfigureForwardHeaders`).
//! The client address walk is [`AppState::client_address`](crate::state::AppState::client_address);
//! this module gives the scheme and host the same hops vouch for. Without known
//! proxies nothing is forwarded (`ForwardedHeaders.None`).

/// How a request reached the server, as a request extension the server's
/// listeners insert: the scheme the request has before any forwarding
/// (`HttpRequest.IsHttps` on the connection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Plain HTTP.
    Plain,
    /// HTTPS: a TLS connection to an HTTPS listener.
    Tls,
}

impl Transport {
    /// The scheme this transport gives a request.
    #[must_use]
    pub fn scheme(self) -> &'static str {
        match self {
            Self::Plain => "http",
            Self::Tls => "https",
        }
    }
}

/// `HttpRequest.Scheme` after `ForwardedHeadersMiddleware`, lowercased as
/// `UriBuilder` writes it: the scheme known proxies forwarded, else the
/// listener's, else `http` (no listener named).
#[must_use]
pub fn effective_scheme(forwarded: Option<&str>, transport: Option<Transport>) -> String {
    forwarded.map_or_else(
        || transport.unwrap_or(Transport::Plain).scheme().to_owned(),
        str::to_ascii_lowercase,
    )
}

/// What known proxies say about a request: the scheme and host
/// `ForwardedHeadersMiddleware` gives it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForwardedRequest {
    /// The forwarded scheme, when a known proxy supplied a valid one.
    pub scheme: Option<String>,
    /// The forwarded host (`host[:port]`), when a known proxy supplied a valid one.
    pub host: Option<String>,
}

/// Port of `ForwardedHeadersMiddleware.ApplyForwarders` for the scheme and
/// host, as Jellyfin configures it (`ConfigureForwardHeaders`: `X-Forwarded-For`,
/// `-Proto` and `-Host`, no forward limit, only from known proxies).
///
/// The comma-separated entries are walked from the right, one hop per step,
/// while the current remote address `is_known_proxy`: a parsable `For` entry
/// becomes the next remote address and an unparsable one ends the walk, and a
/// valid scheme or host replaces the one taken so far.
///
/// The client address comes from [`forwarded_for`](crate::state::forwarded_for),
/// which ends the `For` list at the same unparsable entry; this walk keeps the
/// three headers' entries aligned, as the scheme and host depend on which hop
/// supplied them.
#[must_use]
pub fn walk_forwarded_headers(
    peer: std::net::IpAddr,
    headers: &axum::http::HeaderMap,
    is_known_proxy: impl Fn(std::net::IpAddr) -> bool,
) -> ForwardedRequest {
    let for_entries = comma_separated_values(headers, "x-forwarded-for");
    let proto_entries = comma_separated_values(headers, "x-forwarded-proto");
    let host_entries = comma_separated_values(headers, "x-forwarded-host");
    let hops = for_entries
        .len()
        .max(proto_entries.len())
        .max(host_entries.len());
    let mut forwarded = ForwardedRequest::default();
    let mut remote = peer;
    for hop in 0..hops {
        if !is_known_proxy(remote) {
            break;
        }
        if let Some(entry) = from_right(&for_entries, hop) {
            match crate::state::parse_forwarded_entry(entry) {
                Some(address) => remote = address,
                None => break,
            }
        }
        if let Some(scheme) = from_right(&proto_entries, hop).filter(|s| is_valid_scheme(s)) {
            forwarded.scheme = Some(scheme.to_owned());
        }
        if let Some(host) = from_right(&host_entries, hop).filter(|h| is_valid_host(h)) {
            forwarded.host = Some(host.to_owned());
        }
    }
    forwarded
}

/// Every `name` header's comma-separated entries, in order
/// (`GetCommaSeparatedValues`: trimmed, unquoted, empty entries skipped).
pub(crate) fn comma_separated_values(headers: &axum::http::HeaderMap, name: &str) -> Vec<String> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|v| {
            let v = v.trim();
            v.strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(v)
                .to_owned()
        })
        .filter(|v| !v.is_empty())
        .collect()
}

/// The entry `hop` places from the right of `entries`, the hop order.
fn from_right(entries: &[String], hop: usize) -> Option<&str> {
    entries
        .len()
        .checked_sub(hop + 1)
        .map(|index| entries[index].as_str())
}

/// [`walk_forwarded_headers`] under `network`'s known proxies.
#[must_use]
pub fn forwarded_through(
    network: &std::sync::RwLock<ferrofin_networking::NetworkManager>,
    peer: std::net::IpAddr,
    headers: &axum::http::HeaderMap,
) -> ForwardedRequest {
    let network = network
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    walk_forwarded_headers(peer, headers, |address| network.is_known_proxy(address))
}

/// The client a request came from (upstream's `GetNormalizedRemoteIP()` after
/// `ForwardedHeadersMiddleware`): `NetworkManager::client_address` over
/// `network`'s known proxies. No policy (`None`) means no known proxies, and
/// upstream then ignores `X-Forwarded-For` entirely, so it is the peer.
#[must_use]
pub fn client_address_through(
    network: Option<&std::sync::RwLock<ferrofin_networking::NetworkManager>>,
    peer: std::net::IpAddr,
    forwarded_for: &[std::net::IpAddr],
) -> std::net::IpAddr {
    match network {
        Some(network) => network
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .client_address(peer, forwarded_for),
        None => peer,
    }
}

/// Whether `scheme` holds only RFC 3986 scheme characters
/// (`ForwardedHeadersMiddleware.SchemeChars`).
fn is_valid_scheme(scheme: &str) -> bool {
    !scheme.is_empty()
        && scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

/// Port of `ForwardedHeadersMiddleware.TryValidateHost`: a host of Kestrel's
/// host characters, or a bracketed IPv6 address, with an optional numeric port.
#[must_use]
pub fn is_valid_host(host: &str) -> bool {
    fn valid_port(rest: &str) -> bool {
        rest.strip_prefix(':')
            .is_some_and(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
    }
    if let Some(v6) = host.strip_prefix('[') {
        let end = v6
            .bytes()
            .position(|b| !(b.is_ascii_hexdigit() || matches!(b, b':' | b'.')));
        return match end {
            // `[::1]` is the shortest valid IPv6 host.
            Some(end) if v6.as_bytes()[end] == b']' && end >= 3 => {
                let rest = &v6[end + 1..];
                rest.is_empty() || valid_port(rest)
            }
            _ => false,
        };
    }
    if host.starts_with(':') {
        return false;
    }
    match host.bytes().position(|b| {
        !(b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'-' | b'.' | b'_' | b'~'
            ))
    }) {
        None => true,
        Some(end) => valid_port(&host[end..]),
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use axum::http::HeaderMap;
    use rstest::rstest;

    use super::*;

    /// ASP.NET's default known proxies, which its tests run under: loopback.
    fn loopback(address: IpAddr) -> bool {
        address.is_loopback()
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, value.parse().expect("header value"));
        }
        headers
    }

    fn scheme(proto: &str, forwarded_for: &str, remote: &str) -> Option<String> {
        walk_forwarded_headers(
            remote.parse().expect("remote address"),
            &headers(&[
                ("x-forwarded-proto", proto),
                ("x-forwarded-for", forwarded_for),
            ]),
            loopback,
        )
        .scheme
    }

    /// `ForwardedHeadersMiddlewareTest.XForwardedProtoOverrideCanBeIndependentOfXForwardedForCount`
    /// (`RequireHeaderSymmetry = false`, Jellyfin's setting), the cases whose
    /// `ForwardLimit` does not cut the walk short: Jellyfin sets no limit.
    #[rstest]
    #[case("", "::1", None)]
    #[case("h1", "", Some("h1"))]
    #[case("h1", "::1", Some("h1"))]
    #[case("h1", "::1, ::1", Some("h1"))]
    #[case("h2, h1", "::1", Some("h2"))]
    #[case("h2, h1", "::1, ::1", Some("h2"))]
    #[case("h3, h2, h1", "::1, ::1, ::1", Some("h3"))]
    #[case("h3, h2, h1", "::1, badip, ::1", Some("h1"))]
    fn the_scheme_follows_known_proxies_hop_by_hop(
        #[case] proto: &str,
        #[case] forwarded_for: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(scheme(proto, forwarded_for, "::1").as_deref(), expected);
    }

    /// `XForwardedProtoOverrideLimitedByLoopback`, the cases with known
    /// proxies and headers of equal length (it requires symmetry; Jellyfin
    /// does not).
    #[rstest]
    #[case("", "", "::1", None)]
    #[case("h1", "F::", "::1", Some("h1"))]
    #[case("h1", "", "F::", None)]
    #[case("h1", "E::", "F::", None)]
    #[case("h2, h1", "F::, D::", "::1", Some("h1"))]
    #[case("h2, h1", "E::, D::", "F::", None)]
    fn the_scheme_stops_at_the_first_unknown_proxy(
        #[case] proto: &str,
        #[case] forwarded_for: &str,
        #[case] remote: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(scheme(proto, forwarded_for, remote).as_deref(), expected);
    }

    #[test]
    fn nothing_is_forwarded_without_known_proxies() {
        let forwarded = walk_forwarded_headers(
            "127.0.0.1".parse().expect("address"),
            &headers(&[
                ("x-forwarded-proto", "https"),
                ("x-forwarded-host", "example.com"),
            ]),
            |_| false,
        );
        assert_eq!(forwarded, ForwardedRequest::default());
    }

    #[test]
    fn the_host_comes_from_the_same_hops_and_split_headers_are_one_list() {
        let forwarded = walk_forwarded_headers(
            "127.0.0.1".parse().expect("address"),
            &headers(&[
                ("x-forwarded-for", "203.0.113.9"),
                ("x-forwarded-for", "127.0.0.2"),
                ("x-forwarded-proto", "\"https\", http"),
                ("x-forwarded-host", "outer.example, inner.example:8096"),
            ]),
            loopback,
        );
        // Hop 1 (from 127.0.0.1): `http`, `inner.example:8096`, next 127.0.0.2.
        // Hop 2 (from 127.0.0.2, known): the quoted `https`, `outer.example`.
        assert_eq!(forwarded.scheme.as_deref(), Some("https"));
        assert_eq!(forwarded.host.as_deref(), Some("outer.example"));
    }

    #[test]
    fn an_invalid_scheme_or_host_is_skipped_without_ending_the_walk() {
        let forwarded = walk_forwarded_headers(
            "127.0.0.1".parse().expect("address"),
            &headers(&[
                ("x-forwarded-for", "127.0.0.3, 127.0.0.2"),
                ("x-forwarded-proto", "https, ht/tp"),
                ("x-forwarded-host", "good.example, bad host"),
            ]),
            loopback,
        );
        assert_eq!(forwarded.scheme.as_deref(), Some("https"));
        assert_eq!(forwarded.host.as_deref(), Some("good.example"));
    }

    /// `ForwardedHeadersMiddlewareTest.HostHeaderData`.
    #[rstest]
    #[case("z")]
    #[case("1")]
    #[case("y:1")]
    #[case("1:1")]
    #[case("[ABCdef]")]
    #[case("[abcDEF]:0")]
    #[case("[abcdef:127.2355.1246.114]:0")]
    #[case("[::1]:80")]
    #[case("127.0.0.1:80")]
    #[case("900.900.900.900:9523547852")]
    #[case("foo")]
    #[case("foo:234")]
    #[case("foo.bar.baz")]
    #[case("foo.BAR.baz:46245")]
    #[case("foo.ba-ar.baz:46245")]
    #[case("-foo:1234")]
    #[case("xn--c1yn36f:134")]
    #[case("-")]
    #[case("_")]
    #[case("~")]
    #[case("!")]
    #[case("$")]
    #[case("'")]
    #[case("(")]
    #[case(")")]
    fn valid_hosts_are_accepted(#[case] host: &str) {
        assert!(is_valid_host(host), "{host}");
    }

    /// `ForwardedHeadersMiddlewareTest.HostHeaderInvalidData` (the empty
    /// value never reaches the check: empty entries are dropped).
    #[test]
    fn invalid_hosts_are_rejected() {
        let mut invalid: Vec<String> = [
            "[]",
            "[::]",
            "[ghijkl]",
            "[afd:adf:123",
            "[afd:adf]123",
            "[afd:adf]:",
            "[afd adf]",
            "[ad-314]",
            ":1234",
            "a:b:c",
            "::1",
            "::",
            "abcd:1abcd",
            "abcd:1.2",
            "1.2.3.4:",
            "1.2 .4",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        invalid.extend("\"#%*+/;<=>?@[]\\^`{}|".chars().map(String::from));
        invalid.extend(
            "!\"#$%&'()*+,/;<=>?@[]\\^_`{}|~-"
                .chars()
                .map(|ch| format!("[abd{ch}]:1234")),
        );
        invalid.extend(
            "!\"#$%&'()*+/;<=>?@[]\\^_`{}|~:abcABC-."
                .chars()
                .map(|ch| format!("a.b.c:{ch}")),
        );
        for host in invalid {
            assert!(!is_valid_host(&host), "{host}");
        }
    }

    /// Only a known proxy's `X-Forwarded-For` names the client; without a
    /// policy the peer is the client.
    #[rstest]
    #[case::known_proxy(Some("10.0.0.1"), "10.0.0.1", "203.0.113.9")]
    #[case::unknown_peer(Some("10.0.0.1"), "192.168.1.20", "192.168.1.20")]
    #[case::no_policy(None, "10.0.0.1", "10.0.0.1")]
    fn the_client_is_forwarded_only_by_a_known_proxy(
        #[case] known_proxy: Option<&str>,
        #[case] peer: &str,
        #[case] expected: &str,
    ) {
        let network = known_proxy.map(|proxy| {
            std::sync::RwLock::new(ferrofin_networking::NetworkManager::with_interfaces(
                ferrofin_networking::NetworkConfiguration {
                    known_proxies: vec![proxy.to_owned()],
                    ..ferrofin_networking::NetworkConfiguration::default()
                },
                Vec::new(),
            ))
        });
        let client = client_address_through(
            network.as_ref(),
            peer.parse().expect("peer"),
            &["203.0.113.9".parse().expect("client")],
        );
        assert_eq!(client, expected.parse::<IpAddr>().expect("expected"));
    }

    /// `ApplyForwarders` stops at the first entry from the right that does not
    /// parse and keeps the last address it trusted, so a known proxy that
    /// writes an obfuscated token for its client stays the client, and the
    /// entries left of the token (the client's own) are never taken. Entries
    /// are unquoted and empty ones skipped, as `GetCommaSeparatedValues` does,
    /// and split headers are one list.
    #[rstest]
    #[case::token_after_a_forged_entry(&["192.168.1.10, unknown"], "10.0.0.1")]
    #[case::token_split_into_its_own_header(&["192.168.1.10", "unknown"], "10.0.0.1")]
    #[case::token_before_the_client(&["unknown, 203.0.113.9"], "203.0.113.9")]
    #[case::quoted_entry(&["\"203.0.113.9\""], "203.0.113.9")]
    #[case::entry_with_a_port(&["198.51.100.7, 203.0.113.9:41234"], "203.0.113.9")]
    #[case::empty_entries_are_skipped(&["198.51.100.7, , 203.0.113.9"], "203.0.113.9")]
    #[case::unknown_hop_ends_the_walk(&["198.51.100.7, 203.0.113.9, 192.0.2.4"], "192.0.2.4")]
    #[case::only_a_token(&["unknown"], "10.0.0.1")]
    #[case::bracketed_ipv6_with_a_port(&["unknown, [2001:db8::7]:41234"], "2001:db8::7")]
    fn the_client_walk_stops_at_an_unparsable_entry(
        #[case] forwarded_for: &[&str],
        #[case] expected: &str,
    ) {
        let network = std::sync::RwLock::new(ferrofin_networking::NetworkManager::with_interfaces(
            ferrofin_networking::NetworkConfiguration {
                known_proxies: vec!["10.0.0.1".to_owned()],
                ..ferrofin_networking::NetworkConfiguration::default()
            },
            Vec::new(),
        ));
        let pairs: Vec<(&'static str, &str)> = forwarded_for
            .iter()
            .map(|value| ("x-forwarded-for", *value))
            .collect();
        let chain = crate::state::forwarded_for(&headers(&pairs));
        let client =
            client_address_through(Some(&network), "10.0.0.1".parse().expect("peer"), &chain);
        assert_eq!(client, expected.parse::<IpAddr>().expect("expected"));
    }

    /// The scheme a request has, as the API resolves it for the addresses it
    /// publishes (`AppState::request_scheme`).
    fn request_scheme(
        known_proxy: Option<&str>,
        peer: Option<&str>,
        transport: Option<Transport>,
        forwarded_proto: Option<&str>,
    ) -> String {
        let mut state = crate::test_support::fake_state();
        if let Some(proxy) = known_proxy {
            let config = ferrofin_networking::NetworkConfiguration {
                known_proxies: vec![proxy.to_owned()],
                ..ferrofin_networking::NetworkConfiguration::default()
            };
            state = state.with_network(std::sync::Arc::new(std::sync::RwLock::new(
                ferrofin_networking::NetworkManager::with_interfaces(config, Vec::new()),
            )));
        }
        let mut request = axum::http::Request::get("/System/Info/Public");
        if let Some(proto) = forwarded_proto {
            request = request.header("x-forwarded-proto", proto);
        }
        let (mut parts, ()) = request.body(()).expect("request").into_parts();
        if let Some(peer) = peer {
            parts
                .extensions
                .insert(axum::extract::ConnectInfo(std::net::SocketAddr::new(
                    peer.parse().expect("peer"),
                    50_000,
                )));
        }
        if let Some(transport) = transport {
            parts.extensions.insert(transport);
        }
        state.request_scheme(&parts)
    }

    #[rstest]
    #[case::over_tls(
        Some("10.0.0.1"),
        Some("192.168.1.20"),
        Some(Transport::Tls),
        None,
        "https"
    )]
    #[case::over_plain_http(
        Some("10.0.0.1"),
        Some("192.168.1.20"),
        Some(Transport::Plain),
        None,
        "http"
    )]
    #[case::no_listener_named(None, None, None, None, "http")]
    #[case::a_known_proxys_scheme_wins(
        Some("10.0.0.1"),
        Some("10.0.0.1"),
        Some(Transport::Plain),
        Some("https"),
        "https"
    )]
    #[case::lowercased(
        Some("10.0.0.1"),
        Some("10.0.0.1"),
        Some(Transport::Plain),
        Some("HTTPS"),
        "https"
    )]
    #[case::a_known_proxy_forwarding_http_over_tls(
        Some("10.0.0.1"),
        Some("10.0.0.1"),
        Some(Transport::Tls),
        Some("http"),
        "http"
    )]
    #[case::an_unknown_peers_header_is_ignored(
        Some("10.0.0.1"),
        Some("192.168.1.20"),
        Some(Transport::Plain),
        Some("https"),
        "http"
    )]
    #[case::no_peer_falls_back_to_the_listener(
        Some("10.0.0.1"),
        None,
        Some(Transport::Tls),
        Some("http"),
        "https"
    )]
    #[case::no_network_policy_trusts_no_header(
        None,
        Some("10.0.0.1"),
        Some(Transport::Plain),
        Some("https"),
        "http"
    )]
    fn the_request_scheme_is_the_listeners_unless_a_known_proxy_forwards_one(
        #[case] known_proxy: Option<&str>,
        #[case] peer: Option<&str>,
        #[case] transport: Option<Transport>,
        #[case] forwarded_proto: Option<&str>,
        #[case] expected: &str,
    ) {
        assert_eq!(
            request_scheme(known_proxy, peer, transport, forwarded_proto),
            expected
        );
    }

    #[rstest]
    #[case("https", true)]
    #[case("HTTP", true)]
    #[case("svn+ssh", true)]
    #[case("a.b-c", true)]
    #[case("", false)]
    #[case("ht tp", false)]
    #[case("https:", false)]
    fn schemes_are_checked_against_rfc_3986(#[case] scheme: &str, #[case] valid: bool) {
        assert_eq!(is_valid_scheme(scheme), valid);
    }
}
