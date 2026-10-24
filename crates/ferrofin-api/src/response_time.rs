//! The response-time header and slow-response log — port of C#
//! `ResponseTimeMiddleware`.
//!
//! Every response carries `X-Response-Time-ms`, the milliseconds from the
//! request reaching the server to its response head, as an invariant-culture
//! number. `EnableSlowResponseWarning` and `SlowResponseThresholdMs` gate only
//! the `debug` line for a response slower than the threshold; the header is
//! sent either way, as upstream's is.

use std::convert::Infallible;
use std::net::IpAddr;
use std::time::Instant;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;
use ferrofin_traits::configuration::ServerConfigurationManager;
use tower::{Service, ServiceExt as _};

use crate::state::AppState;

/// The header upstream names `ResponseHeaderResponseTime`.
pub const RESPONSE_TIME_HEADER: &str = "X-Response-Time-ms";

/// Stamps [`RESPONSE_TIME_HEADER`] on the response and logs a slow one.
///
/// Like upstream, the settings are read before the request runs and the time
/// is taken when the response head is ready (`OnStarting`), not after its body
/// has streamed. The log names the path, not the full URL upstream logs: the
/// query can carry an `api_key`. The client is upstream's
/// `GetNormalizedRemoteIP()`: the peer, or the forwarded client when the peer
/// is a configured `KnownProxy`.
pub async fn response_time_layer(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    with_response_time(
        state.config.as_ref(),
        |peer, forwarded_for| state.client_address_for(peer, forwarded_for),
        request,
        next,
    )
    .await
}

/// Answers `request` with `inner` as [`response_time_layer`] does: the
/// settings come from `config`, and `client_address` turns the peer and its
/// `X-Forwarded-For` chain into the client the slow-response line names
/// ([`AppState::client_address_for`] in the router).
///
/// Public so middleware the server runs in front of the router, like the HTTPS
/// redirect, can answer under it, as Jellyfin runs `ResponseTimeMiddleware`
/// before them.
pub async fn with_response_time<S, C>(
    config: &dyn ServerConfigurationManager,
    client_address: C,
    request: Request,
    inner: S,
) -> Response
where
    S: Service<Request, Response = Response, Error = Infallible> + Send,
    S::Future: Send,
    C: FnOnce(IpAddr, &[IpAddr]) -> IpAddr + Send,
{
    let start = Instant::now();
    // A configuration that cannot be read only loses the slow-response line;
    // the header never depends on it.
    let threshold_ms = config
        .configuration()
        .await
        .ok()
        .filter(|config| config.enable_slow_response_warning)
        .map(|config| config.slow_response_threshold_ms);
    // Captured only when a slow response would be logged (upstream's
    // `IsEnabled(LogLevel.Debug)`), so the common path allocates nothing.
    let log_context = threshold_ms
        .filter(|_| tracing::enabled!(tracing::Level::DEBUG))
        .map(|threshold_ms| {
            let peer = request
                .extensions()
                .get::<ConnectInfo<std::net::SocketAddr>>()
                .map_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), |ci| {
                    crate::state::normalize_ip(ci.0.ip())
                });
            let client = client_address(peer, &crate::state::forwarded_for(request.headers()));
            (
                threshold_ms,
                request.method().clone(),
                request.uri().path().to_owned(),
                client,
            )
        });
    let mut response = match inner.oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    let elapsed = start.elapsed();
    let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
    if let Some((threshold_ms, method, path, peer)) = log_context {
        #[allow(clippy::cast_precision_loss)] // a threshold in ms is far below 2^52
        if elapsed_ms > threshold_ms as f64 {
            tracing::debug!(
                %method,
                %path,
                remote_ip = %peer,
                elapsed = ?elapsed,
                status = response.status().as_u16(),
                "slow HTTP response"
            );
        }
    }
    if let Ok(value) = HeaderValue::from_str(&elapsed_ms.to_string()) {
        response.headers_mut().insert(RESPONSE_TIME_HEADER, value);
    }
    response
}
