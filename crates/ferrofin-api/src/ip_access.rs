//! The IP-based access gate — port of C#
//! `IPBasedAccessValidationMiddleware`.
//!
//! `RemoteIPFilter`, `IsRemoteIPFilterBlacklist` and `EnableRemoteAccess` are
//! settable through `POST /System/Configuration/network` and were, until this
//! layer existed, enforced nowhere: an operator could blocklist a subnet, see
//! it persisted and served back, and every request from it would still be
//! answered. The policy itself
//! ([`ferrofin_networking::NetworkManager::should_allow_server_access`]) was
//! already ported and tested; only the call site was missing.

use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use ferrofin_networking::RemoteAccessPolicyResult;

use crate::state::AppState;

/// The address a request's connection was accepted on, which the server's
/// accept loop records beside `ConnectInfo` (the peer). C# `IsLocal()`
/// compares the two.
#[derive(Debug, Clone, Copy)]
pub struct ConnectionLocalAddr(pub std::net::SocketAddr);

/// The client address [`ip_access_layer`] resolved for a request, which
/// [`AppState::client_address`] answers with from then on: the policy checks
/// behind the gate need the same address, and resolving it walks the forwarded
/// chain.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResolvedClientAddress(pub(crate) std::net::IpAddr);

/// Rejects a request whose peer the network policy excludes.
///
/// Upstream's shape exactly:
///
/// - a request from the server's own machine is exempt: C# `IsLocal()`, the
///   connection's local address equal to its remote one (the server's own
///   ffmpeg reading a Live TV stream through a LAN address, say), and any
///   loopback peer, which no other host can be;
/// - otherwise `ShouldAllowServerAccess(GetNormalizedRemoteIP())` decides, and
///   anything but `Allow` returns **503**, not 403 — a blocked client is meant
///   to look like an unavailable server, not a refused one;
/// - the rejection is logged at `warn` with the path, the peer and the reason.
///
/// A request with no peer address (a unit test, or a transport that does not
/// provide one) is treated as loopback, exactly as `GetNormalizedRemoteIP`
/// defaults it.
pub async fn ip_access_layer(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), |ci| {
            crate::state::normalize_ip(ci.0.ip())
        });
    // Behind a reverse proxy the peer is the PROXY, so filtering it would
    // filter the wrong host — and let every real client through. The forwarded
    // chain is consulted only when the peer is a configured `KnownProxy`.
    let ip = state.client_address_for(peer, &crate::state::forwarded_for(request.headers()));
    request.extensions_mut().insert(ResolvedClientAddress(ip));
    // Absent where the accept loop could not read the connection's local
    // address (or a router built without it): only loopback is exempt then.
    let local = request
        .extensions()
        .get::<ConnectionLocalAddr>()
        .map(|local| crate::state::normalize_ip(local.0.ip()));
    if ip.is_loopback() || local == Some(ip) {
        return next.run(request).await;
    }
    let result = state.remote_access_policy(ip);
    if result == RemoteAccessPolicyResult::Allow {
        return next.run(request).await;
    }
    tracing::warn!(
        path = %request.uri().path(),
        remote_ip = %ip,
        reason = ?result,
        "blocking request due to an IP filtering rule"
    );
    Response::builder()
        .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
        .body(axum::body::Body::empty())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{ResolvedClientAddress, ip_access_layer};
    use crate::state::normalize_ip;
    use crate::test_support::fake_state;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::{ConnectInfo, State};
    use axum::http::request::Parts;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use ferrofin_networking::{NetworkConfiguration, NetworkManager};
    use std::net::{IpAddr, SocketAddr};
    use std::sync::{Arc, RwLock};
    use tower::ServiceExt as _;

    /// The gate leaves what it resolved for the checks behind it, and they
    /// get the forwarded client from it.
    #[tokio::test]
    async fn the_gate_hands_the_resolved_client_to_what_runs_behind_it() {
        async fn echo(State(state): State<crate::state::AppState>, parts: Parts) -> String {
            let resolved = parts
                .extensions
                .get::<ResolvedClientAddress>()
                .expect("resolved by the gate");
            assert_eq!(resolved.0, state.client_address(&parts));
            resolved.0.to_string()
        }
        let network = NetworkManager::with_defaults(
            NetworkConfiguration {
                known_proxies: vec!["10.0.0.0/8".to_owned()],
                ..NetworkConfiguration::default()
            },
            "",
        );
        let state = fake_state().with_network(Arc::new(RwLock::new(network)));
        let router = Router::new()
            .route("/echo", get(echo))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                ip_access_layer,
            ))
            .with_state(state);
        let mut request = Request::builder()
            .uri("/echo")
            .header("x-forwarded-for", "203.0.113.7")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(ConnectInfo(
            "10.4.0.9:5000".parse::<SocketAddr>().expect("peer"),
        ));
        let response = router.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(&body[..], b"203.0.113.7");
    }

    /// `client_address` reads what the gate resolved rather than resolving it
    /// again, and resolves it itself where no gate ran.
    #[test]
    fn client_address_answers_with_the_resolved_client() {
        let state = fake_state();
        let (mut parts, ()) = Request::new(()).into_parts();
        assert_eq!(
            state.client_address(&parts),
            IpAddr::from([127, 0, 0, 1]),
            "no gate ran, no peer: loopback"
        );
        let client = IpAddr::from([203, 0, 113, 7]);
        parts.extensions.insert(ResolvedClientAddress(client));
        assert_eq!(state.client_address(&parts), client);
    }

    #[test]
    fn an_ipv4_mapped_peer_is_compared_as_ipv4() {
        let mapped: std::net::IpAddr = "::ffff:192.168.1.5".parse().expect("valid");
        assert_eq!(normalize_ip(mapped).to_string(), "192.168.1.5");
    }

    #[test]
    fn a_real_ipv6_peer_is_left_alone() {
        let v6: std::net::IpAddr = "2001:db8::1".parse().expect("valid");
        assert_eq!(normalize_ip(v6), v6);
    }
}
