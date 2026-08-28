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
    let mut request = Request::builder()
        .uri("/System/Info/Public")
        .body(Body::empty())
        .expect("request");
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
