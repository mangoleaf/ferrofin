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

/// Rejects a request whose peer the network policy excludes.
///
/// Upstream's shape exactly:
///
/// - a request from the server's own machine is exempt (C# `IsLocal()`, which
///   compares the local and remote addresses of the connection — here, a
///   loopback peer, since a peer equal to the local address IS loopback for a
///   TCP connection);
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
    request: Request,
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
    if ip.is_loopback() {
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
    use crate::state::normalize_ip;

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
