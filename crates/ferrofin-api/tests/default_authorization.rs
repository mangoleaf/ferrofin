//! The shared default policy must cover ordinary routes without changing the
//! upstream elevation and IgnoreParentalControl exceptions.
use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
    routing::get,
};
use ferrofin_api::auth::{
    RequireAdmin, RequireAdminWithDefault, RequireAuth, RequireAuthIgnoringSchedule,
};
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{authed_fake_state_with_policy, elevated_fake_state};
use ferrofin_model::users::{AccessSchedule, DynamicDayOfWeek, UserPolicy};
use ferrofin_networking::{NetworkConfiguration, NetworkManager};
use std::sync::{Arc, RwLock};
use tower::ServiceExt;

fn routes(state: AppState) -> Router {
    Router::new()
        .route("/normal", get(|_: RequireAuth| async { StatusCode::OK }))
        .route(
            "/admin-default",
            get(|_: RequireAdminWithDefault| async { StatusCode::OK }),
        )
        .route("/admin", get(|_: RequireAdmin| async { StatusCode::OK }))
        .route(
            "/ignore-schedule",
            get(|_: RequireAuthIgnoringSchedule| async { StatusCode::OK }),
        )
        .with_state(state)
}
async fn probe(router: &Router, path: &str, remote: &str, forwarded: Option<&str>) -> StatusCode {
    let mut req = Request::builder()
        .uri(path)
        .extension(ConnectInfo(remote.parse::<std::net::SocketAddr>().unwrap()));
    if let Some(forwarded) = forwarded {
        req = req.header("X-Forwarded-For", forwarded);
    }
    router
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}
#[tokio::test]
async fn remote_policy_precedes_admin_on_default_routes_only() {
    for admin in [false, true] {
        for allowed in [false, true] {
            let router = routes(authed_fake_state_with_policy(UserPolicy {
                is_administrator: admin,
                enable_remote_access: allowed,
                ..Default::default()
            }));
            assert_eq!(
                probe(&router, "/admin-default", "203.0.113.8:9000", None).await,
                if admin && allowed {
                    StatusCode::OK
                } else {
                    StatusCode::FORBIDDEN
                }
            );
            for path in ["/normal", "/ignore-schedule"] {
                assert_eq!(
                    probe(&router, path, "203.0.113.8:9000", None).await,
                    if allowed {
                        StatusCode::OK
                    } else {
                        StatusCode::FORBIDDEN
                    }
                );
                assert_eq!(
                    probe(&router, path, "127.0.0.1:9000", None).await,
                    StatusCode::OK
                );
            }
            assert_eq!(
                probe(&router, "/admin", "203.0.113.8:9000", None).await,
                if admin {
                    StatusCode::OK
                } else {
                    StatusCode::FORBIDDEN
                }
            );
        }
    }
    let router = routes(elevated_fake_state());
    assert_eq!(
        probe(&router, "/normal", "203.0.113.8:9000", None).await,
        StatusCode::OK,
        "API keys are unrestricted"
    );
}
#[tokio::test]
async fn schedule_changes_gate_ordinary_users_but_preserve_route_exceptions() {
    for admin in [false, true] {
        let router = routes(authed_fake_state_with_policy(UserPolicy {
            is_administrator: admin,
            enable_remote_access: true,
            access_schedules: vec![AccessSchedule {
                id: 1,
                user_id: uuid::Uuid::nil(),
                day_of_week: DynamicDayOfWeek::Everyday,
                start_hour: 25.0,
                end_hour: 26.0,
            }],
            ..Default::default()
        }));
        assert_eq!(
            probe(&router, "/normal", "127.0.0.1:9000", None).await,
            if admin {
                StatusCode::OK
            } else {
                StatusCode::FORBIDDEN
            }
        );
        assert_eq!(
            probe(&router, "/ignore-schedule", "127.0.0.1:9000", None).await,
            StatusCode::OK
        );
    }
}
#[tokio::test]
async fn forwarded_clients_use_only_trusted_proxies_and_live_lan_settings() {
    let mut config = NetworkConfiguration {
        known_proxies: vec!["127.0.0.1".into()],
        ..Default::default()
    };
    let network = Arc::new(RwLock::new(NetworkManager::with_defaults(
        config.clone(),
        "127.0.0.1,1,lo",
    )));
    let router = routes(
        authed_fake_state_with_policy(UserPolicy {
            enable_remote_access: false,
            ..Default::default()
        })
        .with_network(network.clone()),
    );
    assert_eq!(
        probe(&router, "/normal", "127.0.0.1:9000", Some("203.0.113.8")).await,
        StatusCode::FORBIDDEN
    );
    config.local_network_subnets = vec!["203.0.113.0/24".into()];
    network.write().unwrap().update_settings(&config);
    assert_eq!(
        probe(&router, "/normal", "127.0.0.1:9000", Some("203.0.113.8")).await,
        StatusCode::OK
    );
    config.known_proxies.clear();
    config.local_network_subnets.clear();
    network.write().unwrap().update_settings(&config);
    assert_eq!(
        probe(&router, "/normal", "127.0.0.1:9000", Some("203.0.113.8")).await,
        StatusCode::OK,
        "an untrusted forwarded header is ignored"
    );
}
