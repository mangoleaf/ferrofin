//! Startup request-localization options used by the NFO current-culture comparer.
//! Pinned Jellyfin Startup configures query, cookie and Accept-Language providers
//! in that order; a subsequent UICulture save does not rebuild these options.
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Uri};
use axum::middleware::Next;
use axum::response::Response;
use ferrofin_util::current_culture::{self, CultureError};

#[derive(Debug)]
pub(crate) struct RequestCultureOptions {
    default: String,
    default_ui: String,
    supported: Vec<String>,
}

impl RequestCultureOptions {
    pub(crate) fn new(default: &str, supported: Vec<String>) -> Result<Self, CultureError> {
        let default_ui =
            current_culture::display_name(if default.is_empty() { "en-US" } else { default })?;
        let default =
            current_culture::canonical_name(if default.is_empty() { "en-US" } else { default })?;
        let mut supported: Vec<_> = supported
            .into_iter()
            .filter_map(|name| current_culture::canonical_name(&name.replace('_', "-")).ok())
            .collect();
        if !supported
            .iter()
            .any(|name| name.eq_ignore_ascii_case("en-US"))
        {
            supported.push("en-US".into());
        }
        Ok(Self {
            default,
            default_ui,
            supported,
        })
    }

    fn match_supported(&self, names: &[String]) -> Option<String> {
        for name in names {
            let mut name = name.clone();
            for _ in 0..=5 {
                if let Some(supported) = self
                    .supported
                    .iter()
                    .find(|supported| supported.eq_ignore_ascii_case(&name))
                {
                    return Some(supported.clone());
                }
                let Ok(Some(parent)) = current_culture::parent_name(&name) else {
                    break;
                };
                name = parent;
            }
        }
        None
    }

    fn select(&self, uri: &Uri, headers: &HeaderMap) -> (String, String) {
        let query = query_cultures(uri);
        let cookie = cookie_cultures(headers);
        let languages = header_cultures(headers);
        for provider in [query, cookie, languages].into_iter().flatten() {
            let culture = self.match_supported(&provider.0);
            let ui = self.match_supported(&provider.1);
            // A provider with either supported side wins; the unmatched side
            // uses the startup default instead of trying the next provider.
            if culture.is_some() || ui.is_some() {
                return (
                    culture.unwrap_or_else(|| self.default.clone()),
                    ui.unwrap_or_else(|| self.default_ui.clone()),
                );
            }
        }
        (self.default.clone(), self.default_ui.clone())
    }
}

type Candidates = (Vec<String>, Vec<String>);

fn pair(culture: Option<String>, ui: Option<String>) -> Option<Candidates> {
    match (culture, ui) {
        (None, None) => None,
        (Some(culture), None) => Some((vec![culture.clone()], vec![culture])),
        (None, Some(ui)) => Some((vec![ui.clone()], vec![ui])),
        (Some(culture), Some(ui)) => Some((vec![culture], vec![ui])),
    }
}

fn query_cultures(uri: &Uri) -> Option<Candidates> {
    let mut culture = Vec::new();
    let mut ui = Vec::new();
    for (key, value) in form_urlencoded::parse(uri.query()?.as_bytes()) {
        if key.eq_ignore_ascii_case("culture") {
            culture.push(value.into_owned());
        } else if key.eq_ignore_ascii_case("ui-culture") {
            ui.push(value.into_owned());
        }
    }
    pair(
        (!culture.is_empty()).then(|| culture.join(",")),
        (!ui.is_empty()).then(|| ui.join(",")),
    )
}

fn cookie_cultures(headers: &HeaderMap) -> Option<Candidates> {
    let value = headers.get_all(axum::http::header::COOKIE).iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| header.split(';'))
        .filter_map(|part| {
            let (key, value) = part.trim_start_matches([' ', '\t']).split_once('=')?;
            if !key.eq_ignore_ascii_case(".AspNetCore.Culture") { return None; }
            let value = value.trim_end_matches([' ', '\t']);
            if value.is_empty() { return None; }
            let cookie_octet = |byte: u8| matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e);
            let content = if value.starts_with('"') { value.strip_prefix('"')?.strip_suffix('"')? } else { value };
            content.bytes().all(cookie_octet).then_some(value)
        })
        // RequestCookieCollection overwrites duplicate names case-insensitively.
        // Retain source quotes: the culture provider rejects a quoted prefix.
        .next_back()?;
    // Uri.UnescapeDataString decodes percent bytes alone; '+' and '&' are
    // literal cookie data, never query separators.
    let value = percent_encoding::percent_decode_str(value).decode_utf8_lossy();
    let mut parts = value.split('|').filter(|part| !part.is_empty());
    let culture = parts.next()?.strip_prefix("c=")?;
    let ui = parts.next()?.strip_prefix("uic=")?;
    if parts.next().is_some() {
        return None;
    }
    pair(
        (!culture.is_empty()).then(|| culture.into()),
        (!ui.is_empty()).then(|| ui.into()),
    )
}

fn language_entry(value: &str) -> Option<(String, u32)> {
    let mut parts = value.trim().split(';');
    let name = parts.next()?.trim();
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
    {
        return None;
    }
    let mut quality = 100_000_000;
    if let Some(parameter) = parts.next() {
        let (key, value) = parameter.trim().split_once('=')?;
        if !key.trim().eq_ignore_ascii_case("q") || parts.next().is_some() {
            return None;
        }
        let value = value.trim();
        let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
        // HeaderUtilities permits at most ten characters, including up to
        // eight fractional digits; retain their quality ordering exactly.
        if value.len() > 10
            || !matches!(whole, "0" | "1")
            || fraction.len() > 8
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let fraction: u32 = if fraction.is_empty() {
            0
        } else {
            fraction.parse::<u32>().ok()? * 10_u32.pow(u32::try_from(8 - fraction.len()).ok()?)
        };
        if whole == "1" && fraction != 0 {
            return None;
        }
        quality = if whole == "1" { 100_000_000 } else { fraction };
    }
    Some((name.into(), quality))
}

fn header_cultures(headers: &HeaderMap) -> Option<Candidates> {
    let mut names: Vec<_> = headers
        .get_all(axum::http::header::ACCEPT_LANGUAGE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| header.split(','))
        .filter_map(language_entry)
        // Source truncates before sorting by quality, retaining stable ties.
        .take(3)
        .collect();
    names.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| (left.0 == "*").cmp(&(right.0 == "*")))
    });
    let names: Vec<_> = names.into_iter().map(|(name, _)| name).collect();
    (!names.is_empty()).then(|| (names.clone(), names))
}

pub(crate) async fn request_culture_layer(
    State(options): State<Arc<RequestCultureOptions>>,
    request: Request,
    next: Next,
) -> Response {
    let (culture, ui) = options.select(request.uri(), request.headers());
    let mut response = current_culture::scope(culture, next.run(request)).await;
    // Startup sets ApplyCurrentCultureToResponseHeaders alongside these options.
    if let Ok(ui) = ui.parse() {
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_LANGUAGE, ui);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::{Body, to_bytes},
        middleware,
        routing::get,
    };
    use tower::ServiceExt;

    fn options() -> RequestCultureOptions {
        RequestCultureOptions::new("en-US", vec!["en-US".into(), "sv".into(), "de-DE".into()])
            .unwrap()
    }
    fn selected(query: &str, cookie: &str, language: &str) -> (String, String) {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::COOKIE, cookie.parse().unwrap());
        headers.insert(
            axum::http::header::ACCEPT_LANGUAGE,
            language.parse().unwrap(),
        );
        options().select(&format!("/nfo?{query}").parse().unwrap(), &headers)
    }

    #[test]
    fn provider_precedence_and_partial_pair_use_startup_defaults() {
        assert_eq!(
            selected(
                "culture=sv-SE",
                ".AspNetCore.Culture=c=de-DE|uic=de-DE",
                "en-US"
            ),
            ("sv".into(), "sv".into())
        );
        assert_eq!(
            selected("culture=unsupported&ui-culture=sv", "", "de-DE"),
            ("en-US".into(), "sv".into())
        );
        assert_eq!(
            selected(
                "culture=unsupported",
                ".AspNetCore.Culture=c=sv|uic=sv",
                "de-DE"
            ),
            ("sv".into(), "sv".into())
        );
        assert_eq!(
            selected("", ".AspNetCore.Culture=c%3D%7Cuic%3Dsv", "de-DE"),
            ("sv".into(), "sv".into())
        );
        assert_eq!(
            selected("", ".AspNetCore.Culture=c=sv|uic=sv&extra", "de-DE"),
            ("sv".into(), "en-US".into())
        );
        assert_eq!(
            selected("", ".AspNetCore.Culture=c=sv|uic=sv+extra", "de-DE"),
            ("sv".into(), "en-US".into())
        );
        assert_eq!(
            selected("", ".aspnetcore.cULTURE=c=sv|uic=sv", "de-DE"),
            ("sv".into(), "sv".into())
        );
        assert_eq!(
            selected(
                "",
                ".AspNetCore.Culture=c=de-DE|uic=de-DE; .aspnetcore.cULTURE=c=sv|uic=sv",
                "en-US"
            ),
            ("sv".into(), "sv".into())
        );
        assert_eq!(
            selected(
                "",
                ".AspNetCore.Culture=c=sv|uic=sv; .aspnetcore.culture=",
                "de-DE"
            ),
            ("sv".into(), "sv".into())
        );
        assert_eq!(
            selected(
                "",
                ".AspNetCore.Culture=c=sv|uic=sv; .aspnetcore.culture=\"\"",
                "de-DE"
            ),
            ("de-DE".into(), "de-DE".into())
        );
        assert_eq!(
            selected("", ".AspNetCore.Culture=\"c=sv|uic=sv\"", "de-DE"),
            ("de-DE".into(), "de-DE".into())
        );
        assert_eq!(
            selected("", ".AspNetCore.Culture= c=sv|uic=sv", "de-DE"),
            ("de-DE".into(), "de-DE".into())
        );
        assert_eq!(
            selected("culture=sv-SE-u-ca-gregory", "", "de-DE"),
            ("sv".into(), "sv".into())
        );
        assert_eq!(
            selected("culture=sv-u-ca-gregory", "", "en-US"),
            ("en-US".into(), "en-US".into())
        );
        assert_eq!(
            selected("ui-culture=sv", "", "de-DE"),
            ("sv".into(), "sv".into())
        );
        assert_eq!(
            selected("culture=&ui-culture=sv", "", "de-DE"),
            ("en-US".into(), "sv".into())
        );
        assert_eq!(
            selected("", ".AspNetCore.Culture=wrong", "de-DE"),
            ("de-DE".into(), "de-DE".into())
        );
        assert_eq!(
            selected("", ".AspNetCore.Culture=c=|uic=", ""),
            ("en-US".into(), "en-US".into())
        );
        assert_eq!(
            selected("culture=sv&culture=de-DE", "", "en-US"),
            ("en-US".into(), "en-US".into())
        );
    }

    #[test]
    fn language_provider_limits_before_quality_sort_and_preserves_zero_quality() {
        assert_eq!(
            selected("", "", "unsupported,missing,unknown,sv;q=1"),
            ("en-US".into(), "en-US".into())
        );
        assert_eq!(
            selected("", "", "en-US;q=0.2,sv;q=0.9,de-DE;q=0.4"),
            ("sv".into(), "sv".into())
        );
        assert_eq!(
            selected("", "", "en-US;q=0.9,sv; q = 0.9001,de-DE;q=0.4"),
            ("sv".into(), "sv".into())
        );
        assert_eq!(
            selected("", "", "en-US;q=0.9,sv;q=0.90000001"),
            ("sv".into(), "sv".into())
        );
        assert_eq!(selected("", "", "sv;q=0"), ("sv".into(), "sv".into()));
        assert_eq!(
            selected("", "", "*;q=0.9,sv;q=0.9"),
            ("sv".into(), "sv".into())
        );
        assert!(RequestCultureOptions::new("bad___culture", vec![]).is_err());
        assert_eq!(
            RequestCultureOptions::new("", vec![]).unwrap().default,
            "en-US"
        );
    }

    #[tokio::test]
    async fn requests_keep_separate_cultures_after_await_and_export_ui_header() {
        async fn probe() -> String {
            let first = current_culture::capture();
            tokio::task::yield_now().await;
            assert_eq!(current_culture::capture(), first);
            first
        }
        async fn read(router: Router, uri: &str) -> (String, String) {
            let response = router
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let language = response.headers()[axum::http::header::CONTENT_LANGUAGE]
                .to_str()
                .unwrap()
                .to_owned();
            let body =
                String::from_utf8(to_bytes(response.into_body(), 100).await.unwrap().to_vec())
                    .unwrap();
            (body, language)
        }
        let router = Router::new()
            .route("/nfo", get(probe))
            .layer(middleware::from_fn_with_state(
                Arc::new(options()),
                request_culture_layer,
            ));
        let (sv, de) = tokio::join!(
            read(router.clone(), "/nfo?culture=sv-SE"),
            read(router, "/nfo?culture=de-DE&ui-culture=sv")
        );
        assert_eq!(sv, ("sv".into(), "sv".into()));
        assert_eq!(de, ("de-DE".into(), "sv".into()));
    }
    #[test]
    fn startup_preserves_raw_dashboard_default_and_public_header_name() {
        for name in ["es_419", "ar_SA", "es_DO", "ur_PK", "sv_SE"] {
            let options = RequestCultureOptions::new(
                name,
                vec!["es".into(), "es-419".into(), "ar".into(), "sv".into()],
            )
            .unwrap();
            assert_eq!(
                options.select(&"/nfo".parse().unwrap(), &HeaderMap::new()),
                (name.into(), name.into())
            );
        }
        let options = RequestCultureOptions::new(
            "de-DE-u-co-phonebk",
            vec!["de-DE".into(), "de".into(), "es".into(), "es-419".into()],
        )
        .unwrap();
        assert_eq!(
            options.select(&"/nfo".parse().unwrap(), &HeaderMap::new()),
            ("de-DE_phoneboo".into(), "de-DE".into())
        );
        for (query, expected) in [
            ("culture=de-DE_phoneb", "de-DE"),
            ("culture=de-DE-u-co-phonebk", "de-DE"),
            ("culture=es_419", "es"),
        ] {
            assert_eq!(
                options.select(&format!("/nfo?{query}").parse().unwrap(), &HeaderMap::new()),
                (expected.into(), expected.into())
            );
        }
    }
    #[tokio::test]
    async fn raw_swedish_startup_default_reaches_comparer_and_actual_header() {
        async fn probe() -> String {
            let culture = current_culture::capture();
            let order = current_culture::comparer(&culture)
                .unwrap()
                .compare("/ä.jpg", "/z.jpg");
            format!("{culture}|{order:?}")
        }
        let options = Arc::new(RequestCultureOptions::new("sv_SE", vec!["sv".into()]).unwrap());
        let router = Router::new()
            .route("/nfo", get(probe))
            .layer(middleware::from_fn_with_state(
                options,
                request_culture_layer,
            ));
        for (query, expected, header) in [
            ("/nfo", "sv_SE|Greater", "sv_SE"),
            ("/nfo?culture=sv_SE", "sv|Greater", "sv"),
        ] {
            let response = router
                .clone()
                .oneshot(Request::builder().uri(query).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.headers()[axum::http::header::CONTENT_LANGUAGE],
                header
            );
            let body = to_bytes(response.into_body(), 100).await.unwrap();
            assert_eq!(body.as_ref(), expected.as_bytes());
        }
    }
    #[test]
    fn mixed_keyword_order_selects_the_source_supported_parent() {
        let options =
            RequestCultureOptions::new("en-US", vec!["fr-CA".into(), "fr".into(), "sv".into()])
                .unwrap();
        for (query, expected) in [
            ("culture=fr-CA-u-nu-latn-co-phonebk", "fr"),
            ("culture=fr-CA-u-co-phonebk-nu-latn", "fr-CA"),
            ("culture=sv-u-ca-gregory-co-phonebk", "en-US"),
            ("culture=sv-u-co-phonebk-ca-gregory", "en-US"),
        ] {
            assert_eq!(
                options.select(&format!("/nfo?{query}").parse().unwrap(), &HeaderMap::new()),
                (expected.into(), expected.into())
            );
        }
        let options = RequestCultureOptions::new("fr-CA-u-nu-latn-co-phonebk", vec![]).unwrap();
        assert_eq!(
            options.select(&"/nfo".parse().unwrap(), &HeaderMap::new()),
            (
                "fr-CA-u-nu-latn-co-phonebk_phoneboo".into(),
                "fr-CA-u-nu-latn-co-phonebk".into()
            )
        );
    }
}
