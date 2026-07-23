//! `TimeSyncController` — the client/server clock-sync probe.
//!
//! Ports `GET /GetUtcTime`, returning the request-reception and
//! response-transmission UTC instants so a client can estimate server time and
//! round-trip latency. Anonymous (no auth). This is deliberately a "high-level
//! NTP" — the two timestamps bracket the handler body.

use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use hermit_model::sync_play::UtcTimeResponse;

use crate::state::AppState;

/// `GET /GetUtcTime` — the current UTC time bracket for clock sync.
///
/// Port of `TimeSyncController.GetUtcTime`: stamps the reception time first and
/// the transmission time last (here they coincide, as the body is trivial).
#[utoipa::path(
    get,
    path = "/GetUtcTime",
    responses((status = 200, description = "Time returned", body = UtcTimeResponse)),
    tag = "hermit"
)]
async fn get_utc_time() -> Json<UtcTimeResponse> {
    let request_reception_time = Utc::now();
    let response_transmission_time = Utc::now();
    Json(UtcTimeResponse {
        request_reception_time,
        response_transmission_time,
    })
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/GetUtcTime", get(get_utc_time))
}
