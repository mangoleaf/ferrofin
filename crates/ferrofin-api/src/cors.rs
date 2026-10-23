//! CORS from the configured `CorsHosts`, chosen per request so a saved change
//! applies without a restart. Port of `CorsPolicyProvider.GetPolicyAsync`
//! (`Jellyfin.Server/Configuration/CorsPolicyProvider.cs`), which builds the
//! policy from the live configuration on every request.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;
use ferrofin_traits::configuration::ServerConfigurationManager;
use tower::{Layer as _, Service, ServiceExt as _};
use tower_http::cors::{AllowCredentials, AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};

/// The CORS layer `cors_hosts` selects:
/// - no host, or exactly `*` (ordinal), is any origin. Upstream's
///   `AllowAnyOrigin().AllowAnyMethod().AllowAnyHeader()`, served by the
///   permissive layer Ferrofin always used;
/// - otherwise exactly those origins, with credentials and any method and
///   header (`WithOrigins(corsHosts).AllowCredentials()`). Each origin is
///   normalized as ASP.NET normalizes a configured one
///   ([`normalized_origin`]) and matched exactly against the request's
///   `Origin`. An entry that is `*` or not a valid header value can match no
///   request origin there, so it is dropped here.
pub(crate) fn layer_for(cors_hosts: &[String]) -> CorsLayer {
    if cors_hosts.is_empty() || matches!(cors_hosts, [only] if only == "*") {
        return CorsLayer::permissive();
    }
    let origins: Vec<HeaderValue> = cors_hosts
        .iter()
        .filter(|host| host.as_str() != "*")
        .filter_map(|host| HeaderValue::from_str(&normalized_origin(host)).ok())
        .collect();
    // Credentials only for a listed origin: ASP.NET answers any other with
    // no CORS headers at all.
    let listed = origins.clone();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_credentials(AllowCredentials::predicate(move |origin, _| {
            listed.contains(origin)
        }))
        .allow_methods(AllowMethods::mirror_request())
        .allow_headers(AllowHeaders::mirror_request())
}

/// A configured origin as `CorsPolicyBuilder.GetNormalizedOrigin` leaves it.
/// An http or https origin whose host is international (`Uri.IdnHost` differs
/// from `Uri.Host`) becomes `scheme://punycode-host`, with its port only when
/// it is not the scheme's default, as a browser sends it in `Origin`. Any
/// other entry is only lower-cased: a path, a trailing `/` or a default port
/// stays, and such an entry matches no browser's `Origin`, as upstream.
fn normalized_origin(entry: &str) -> String {
    if entry_host(entry).is_some_and(|host| !host.is_ascii())
        && let Ok(url) = url::Url::parse(entry)
        && matches!(url.scheme(), "http" | "https")
    {
        return url.origin().ascii_serialization();
    }
    entry.to_lowercase()
}

/// The host (with any port) of a configured origin, as written: after the
/// scheme's `://` and any user info, up to the path, query or fragment.
fn entry_host(entry: &str) -> Option<&str> {
    let (_, rest) = entry.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    Some(
        authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host),
    )
}

/// The CORS middleware: [`with_configured_cors`] around the rest of the
/// request.
pub(crate) async fn cors_layer(
    State(config): State<Arc<dyn ServerConfigurationManager>>,
    request: Request,
    next: Next,
) -> Response {
    with_configured_cors(config.as_ref(), request, next).await
}

/// Answers `request` with `inner` under the layer the live configuration's
/// `CorsHosts` selects ([`layer_for`]). A request with no `Origin` is no CORS
/// request and goes straight to `inner` with the policy unread, as ASP.NET's
/// `CorsMiddleware.Invoke` does. A configuration that cannot be read leaves the
/// default, any origin.
///
/// Public so middleware the server runs in front of the router, like the HTTPS
/// redirect, can answer under the same policy, which Jellyfin applies before
/// it.
pub async fn with_configured_cors<S>(
    config: &dyn ServerConfigurationManager,
    request: Request,
    inner: S,
) -> Response
where
    S: Service<Request, Response = Response, Error = Infallible> + Send,
    S::Future: Send,
{
    if !request.headers().contains_key(axum::http::header::ORIGIN) {
        return match inner.oneshot(request).await {
            Ok(response) => response,
            Err(never) => match never {},
        };
    }
    let layer = match config.configuration().await {
        Ok(configuration) => layer_for(&configuration.cors_hosts),
        // The live configuration manager's read cannot fail (it hands out its
        // snapshot); this keeps the default for one that does.
        Err(err) => {
            tracing::debug!(error = %err, "CORS: configuration unreadable; allowing any origin");
            CorsLayer::permissive()
        }
    };
    match layer.layer(inner).oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Method, StatusCode, header};
    use axum::routing::get;
    use ferrofin_model::branding::BrandingOptions;
    use ferrofin_model::configuration::ServerConfiguration;
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::system::ServerApplicationPaths;

    use super::*;

    /// A configuration whose `CorsHosts` a test changes between requests.
    struct Hosts(Mutex<Vec<String>>);

    #[async_trait]
    impl ServerConfigurationManager for Hosts {
        fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
            Arc::new(crate::test_support::FakePaths)
        }
        async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
            Ok(Arc::new(ServerConfiguration {
                cors_hosts: self.0.lock().unwrap().clone(),
                ..ServerConfiguration::default()
            }))
        }
        async fn update_configuration(&self, _: &ServerConfiguration) -> Result<(), ServiceError> {
            unimplemented!("unused")
        }
        async fn get_branding(&self) -> Result<BrandingOptions, ServiceError> {
            unimplemented!("unused")
        }
        async fn update_branding(&self, _: &BrandingOptions) -> Result<(), ServiceError> {
            unimplemented!("unused")
        }
    }

    async fn send(
        router: &Router,
        method: Method,
        origin: &str,
    ) -> axum::http::Response<axum::body::Body> {
        let mut request = Request::builder().method(method.clone()).uri("/");
        if !origin.is_empty() {
            request = request.header(header::ORIGIN, origin);
        }
        if method == Method::OPTIONS {
            request = request
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "authorization");
        }
        router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn header_of<'a>(response: &'a axum::http::Response<Body>, name: &str) -> Option<&'a str> {
        response.headers().get(name).and_then(|v| v.to_str().ok())
    }

    /// The default `*` (and no host at all) allows any origin without
    /// credentials; a configured list allows exactly those origins, with
    /// credentials, and follows a saved change on the next request. A request
    /// with no `Origin` gets no CORS headers at all.
    #[tokio::test]
    async fn cors_follows_the_configured_hosts() {
        let config = Arc::new(Hosts(Mutex::new(vec!["*".to_owned()])));
        let router = Router::new().route("/", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(
                Arc::clone(&config) as Arc<dyn ServerConfigurationManager>,
                cors_layer,
            ),
        );

        for hosts in [vec!["*".to_owned()], Vec::new()] {
            *config.0.lock().unwrap() = hosts;
            let any = send(&router, Method::GET, "https://anywhere.example").await;
            assert_eq!(any.status(), StatusCode::OK);
            assert_eq!(header_of(&any, "access-control-allow-origin"), Some("*"));
            assert_eq!(header_of(&any, "access-control-allow-credentials"), None);
            let no_origin = send(&router, Method::GET, "").await;
            assert_eq!(no_origin.status(), StatusCode::OK);
            assert_eq!(header_of(&no_origin, "access-control-allow-origin"), None);
        }

        *config.0.lock().unwrap() = vec![
            "https://Web.Example".to_owned(),
            "*".to_owned(),
            "https://other.example".to_owned(),
            "https://Bücher.example:443/".to_owned(),
            "https://ported.example:443".to_owned(),
        ];
        let listed = send(&router, Method::GET, "https://web.example").await;
        assert_eq!(listed.status(), StatusCode::OK);
        assert_eq!(
            header_of(&listed, "access-control-allow-origin"),
            Some("https://web.example"),
            "a listed origin, normalized to lower case"
        );
        assert_eq!(
            header_of(&listed, "access-control-allow-credentials"),
            Some("true")
        );
        let preflight = send(&router, Method::OPTIONS, "https://other.example").await;
        assert_eq!(
            header_of(&preflight, "access-control-allow-origin"),
            Some("https://other.example")
        );
        assert_eq!(
            header_of(&preflight, "access-control-allow-methods"),
            Some("POST"),
            "any method"
        );
        assert_eq!(
            header_of(&preflight, "access-control-allow-headers"),
            Some("authorization"),
            "any header"
        );
        let idn = send(&router, Method::GET, "https://xn--bcher-kva.example").await;
        assert_eq!(
            header_of(&idn, "access-control-allow-origin"),
            Some("https://xn--bcher-kva.example"),
            "an international host, as its punycode origin"
        );
        let ported = send(&router, Method::GET, "https://ported.example").await;
        assert_eq!(
            header_of(&ported, "access-control-allow-origin"),
            None,
            "an ASCII entry keeps its default port, so no browser origin matches it"
        );
        let unlisted = send(&router, Method::GET, "https://anywhere.example").await;
        assert_eq!(
            unlisted.status(),
            StatusCode::OK,
            "the request is still served"
        );
        assert_eq!(header_of(&unlisted, "access-control-allow-origin"), None);
        assert_eq!(
            header_of(&unlisted, "access-control-allow-credentials"),
            None
        );
    }
}
