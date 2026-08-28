//! The IP-based access gate — port of C#
//! `IPBasedAccessValidationMiddleware`.
//!
//! `RemoteIPFilter`, `IsRemoteIPFilterBlacklist` and `EnableRemoteAccess` were
//! settable through the dashboard and enforced nowhere: the policy that decides
//! them was ported and unit-tested in `ferrofin-networking`, but nothing on the
//! request path ever called it. These tests pin the call site.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::router::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::authed_fake_state;
use ferrofin_networking::{NetworkConfiguration, NetworkManager};
use std::sync::{Arc, RwLock};
use tower::ServiceExt;

/// A state whose network policy is built from `config`.
fn state_with_network(config: NetworkConfiguration) -> AppState {
    authed_fake_state().with_network(Arc::new(RwLock::new(NetworkManager::with_defaults(
        config, "",
    ))))
}

/// `GET /System/Info/Public` from `peer` — an unauthenticated route, so what
/// the status reports is the IP gate and nothing else.
async fn public_info_from(app: AppState, peer: &str) -> StatusCode {
    public_info_forwarded(app, peer, None).await
}

/// [`public_info_from`] with an `X-Forwarded-For` header.
async fn public_info_forwarded(app: AppState, peer: &str, xff: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().uri("/System/Info/Public");
    if let Some(xff) = xff {
        builder = builder.header("X-Forwarded-For", xff);
    }
    let mut request = builder.body(Body::empty()).expect("request");
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        peer.parse::<std::net::SocketAddr>().expect("peer address"),
    ));
    create_router(app)
        .oneshot(request)
        .await
        .expect("response")
        .status()
}

/// An allowlist (`IsRemoteIPFilterBlacklist = false`) admits the named subnet
/// and refuses everything else remote — with 503, which is what upstream
/// answers so a blocked client sees an unavailable server rather than a refusal
/// that confirms it exists.
#[tokio::test]
async fn a_remote_ip_allowlist_admits_only_what_it_names() {
    let config = NetworkConfiguration {
        remote_ip_filter: vec!["203.0.113.0/24".to_owned()],
        is_remote_ip_filter_blacklist: false,
        ..NetworkConfiguration::default()
    };
    assert_eq!(
        public_info_from(state_with_network(config.clone()), "203.0.113.9:5000").await,
        StatusCode::OK,
        "the allowlisted subnet is served"
    );
    assert_eq!(
        public_info_from(state_with_network(config), "198.51.100.4:5000").await,
        StatusCode::SERVICE_UNAVAILABLE,
        "every other remote peer is refused"
    );
}

/// A blocklist is the same rule inverted.
#[tokio::test]
async fn a_remote_ip_blocklist_refuses_only_what_it_names() {
    let config = NetworkConfiguration {
        remote_ip_filter: vec!["198.51.100.0/24".to_owned()],
        is_remote_ip_filter_blacklist: true,
        ..NetworkConfiguration::default()
    };
    assert_eq!(
        public_info_from(state_with_network(config.clone()), "198.51.100.4:5000").await,
        StatusCode::SERVICE_UNAVAILABLE,
        "the blocklisted subnet is refused"
    );
    assert_eq!(
        public_info_from(state_with_network(config), "203.0.113.9:5000").await,
        StatusCode::OK,
        "every other remote peer is served"
    );
}

/// The filter never locks the operator out of their own machine: C# exempts a
/// local request before it consults the policy at all.
#[tokio::test]
async fn a_loopback_peer_is_never_filtered() {
    let config = NetworkConfiguration {
        // Blocks everything remote.
        remote_ip_filter: vec!["0.0.0.0/0".to_owned()],
        is_remote_ip_filter_blacklist: true,
        ..NetworkConfiguration::default()
    };
    for peer in ["127.0.0.1:5000", "[::1]:5000"] {
        assert_eq!(
            public_info_from(state_with_network(config.clone()), peer).await,
            StatusCode::OK,
            "{peer} is the machine itself"
        );
    }
}

/// `EnableRemoteAccess = false` is the coarse form of the same gate: the LAN
/// still works, everything off it does not.
#[tokio::test]
async fn disabling_remote_access_keeps_only_the_local_network() {
    let config = NetworkConfiguration {
        enable_remote_access: false,
        ..NetworkConfiguration::default()
    };
    assert_eq!(
        public_info_from(state_with_network(config.clone()), "192.168.1.5:5000").await,
        StatusCode::OK,
        "the LAN is still served"
    );
    assert_eq!(
        public_info_from(state_with_network(config), "203.0.113.9:5000").await,
        StatusCode::SERVICE_UNAVAILABLE,
        "the internet is not"
    );
}

/// An IPv4-mapped IPv6 peer is matched as the IPv4 address it is (C#
/// `GetNormalizedRemoteIP`) — otherwise a dual-stack listener would slip every
/// filter, which is how these rules quietly stop working in production.
#[tokio::test]
async fn an_ipv4_mapped_peer_is_matched_by_an_ipv4_rule() {
    let config = NetworkConfiguration {
        remote_ip_filter: vec!["198.51.100.0/24".to_owned()],
        is_remote_ip_filter_blacklist: true,
        ..NetworkConfiguration::default()
    };
    assert_eq!(
        public_info_from(state_with_network(config), "[::ffff:198.51.100.4]:5000").await,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

/// With no policy wired — every unit test, and any build that has not called
/// `with_network` — nothing is filtered. The gate must not become an accidental
/// deny-all.
#[tokio::test]
async fn no_configured_policy_filters_nothing() {
    assert_eq!(
        public_info_from(authed_fake_state(), "203.0.113.9:5000").await,
        StatusCode::OK
    );
}

/// Behind a reverse proxy the peer is the proxy, so the filter has to read the
/// forwarded chain — otherwise the rules an operator writes apply to their own
/// ingress and to nobody else.
#[tokio::test]
async fn a_known_proxy_s_forwarded_client_is_what_gets_filtered() {
    let config = NetworkConfiguration {
        known_proxies: vec!["10.0.0.0/8".to_owned()],
        remote_ip_filter: vec!["198.51.100.0/24".to_owned()],
        is_remote_ip_filter_blacklist: true,
        ..NetworkConfiguration::default()
    };
    assert_eq!(
        public_info_forwarded(
            state_with_network(config.clone()),
            "10.4.0.9:5000",
            Some("198.51.100.4")
        )
        .await,
        StatusCode::SERVICE_UNAVAILABLE,
        "the blocklisted CLIENT is refused, though the peer is the proxy"
    );
    assert_eq!(
        public_info_forwarded(
            state_with_network(config),
            "10.4.0.9:5000",
            Some("203.0.113.7")
        )
        .await,
        StatusCode::OK,
        "and an unlisted client still gets through the same proxy"
    );
}

/// The header is trusted ONLY from a configured proxy. Any client can send one,
/// so honouring it from an arbitrary peer would let anyone walk straight through
/// the filter by naming an address the operator allows.
#[tokio::test]
async fn a_forwarded_header_from_a_stranger_is_ignored() {
    let config = NetworkConfiguration {
        known_proxies: vec!["10.0.0.0/8".to_owned()],
        remote_ip_filter: vec!["198.51.100.0/24".to_owned()],
        is_remote_ip_filter_blacklist: true,
        ..NetworkConfiguration::default()
    };
    assert_eq!(
        public_info_forwarded(
            state_with_network(config),
            // The blocklisted host itself, claiming to be someone else.
            "198.51.100.4:5000",
            Some("203.0.113.7")
        )
        .await,
        StatusCode::SERVICE_UNAVAILABLE,
        "a peer that is not a known proxy cannot forge its own address"
    );
}

/// …and a client that prepends entries of its own cannot push its real address
/// out of view: the walk stops at the first hop that is not a known proxy.
#[tokio::test]
async fn a_forged_chain_stops_at_the_first_unknown_hop() {
    let config = NetworkConfiguration {
        known_proxies: vec!["10.0.0.0/8".to_owned()],
        remote_ip_filter: vec!["198.51.100.0/24".to_owned()],
        is_remote_ip_filter_blacklist: true,
        ..NetworkConfiguration::default()
    };
    assert_eq!(
        public_info_forwarded(
            state_with_network(config),
            "10.4.0.9:5000",
            // The blocklisted client claimed a friendly address ahead of itself.
            Some("203.0.113.7, 198.51.100.4")
        )
        .await,
        StatusCode::SERVICE_UNAVAILABLE,
        "the rightmost entry is the one the known proxy vouched for"
    );
}

/// With no `KnownProxies` configured the header is ignored entirely — upstream
/// sets `ForwardedHeaders.None`, which is the only safe default.
#[tokio::test]
async fn without_known_proxies_the_header_is_ignored() {
    let config = NetworkConfiguration {
        remote_ip_filter: vec!["198.51.100.0/24".to_owned()],
        is_remote_ip_filter_blacklist: true,
        ..NetworkConfiguration::default()
    };
    assert_eq!(
        public_info_forwarded(
            state_with_network(config.clone()),
            "203.0.113.9:5000",
            Some("198.51.100.4")
        )
        .await,
        StatusCode::OK,
        "a blocklisted address in an untrusted header changes nothing"
    );
    assert_eq!(
        public_info_forwarded(
            state_with_network(config),
            "198.51.100.4:5000",
            Some("203.0.113.7")
        )
        .await,
        StatusCode::SERVICE_UNAVAILABLE,
        "…and cannot excuse a blocklisted peer either"
    );
}

/// Real proxies write ports and brackets; both spellings are read.
#[tokio::test]
async fn a_forwarded_entry_may_carry_a_port() {
    let config = NetworkConfiguration {
        known_proxies: vec!["10.0.0.0/8".to_owned()],
        remote_ip_filter: vec!["198.51.100.0/24".to_owned()],
        is_remote_ip_filter_blacklist: true,
        ..NetworkConfiguration::default()
    };
    assert_eq!(
        public_info_forwarded(
            state_with_network(config),
            "10.4.0.9:5000",
            Some("198.51.100.4:41234")
        )
        .await,
        StatusCode::SERVICE_UNAVAILABLE
    );
}
