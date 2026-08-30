//! The request-body binder — Ferrofin's port of ASP.NET's `[ApiController]`
//! model-binding rejection.
//!
//! Every Jellyfin controller derives from `BaseJellyfinApiController`, which
//! carries `[ApiController]` (v10.11.8 Jellyfin.Api/BaseJellyfinApiController.cs:12-18).
//! That attribute installs the automatic model-state filter, and the repository
//! customises nothing — `git grep -n "InvalidModelStateResponseFactory|
//! SuppressModelStateInvalidFilter|ValidationProblemDetails" v10.11.8 -- '*.cs'`
//! returns no matches — so a body the binder cannot read is answered with the
//! framework default: **400** and a `ValidationProblemDetails` document as
//! `application/json; charset=utf-8`.
//!
//! `axum::Json`'s own rejection is **422** with a `text/plain` body, so every
//! body-taking route in the server diverged from the contract in the same way.
//! It is fixed here as a class, once, rather than per route: [`JsonBody`]
//! replaces `axum::Json` in extractor position everywhere (`Json` stays the
//! *response* type), [`JsonSeqBody`] is its variant for the two operations whose
//! body is a JSON array, and [`JsonValueBody`] the one for the two that bind
//! `[FromBody] JsonDocument` and therefore take any JSON kind.
//!
//! Two behaviours beyond the status matter, and both were measured against a
//! live Jellyfin 10.11.8:
//!
//! * **A wrong (or missing) content type is 415**, also as a problem document —
//!   `{"type":"…#section-15.5.16","title":"Unsupported Media Type","status":415}`
//!   — except on an OPTIONAL body with nothing in it, where ASP.NET never
//!   selects a formatter at all and simply binds null.
//! * **The top-level JSON kind is checked.** `System.Text.Json` refuses to bind
//!   a sequence to an object DTO, so `[]` and `["StartDate"]` are 400. Serde's
//!   derived `Deserialize` accepts a sequence positionally, so before this
//!   extractor Ferrofin answered `POST /LiveTv/Programs` with `[]` **200 and the
//!   whole guide** — a malformed body silently accepted as a valid one, which is
//!   the more dangerous half of the divergence.
//!
//! What is deliberately NOT reproduced is the `errors` dictionary's contents:
//! its keys and messages carry .NET type names (`Jellyfin.Data.Enums.ItemSortBy`)
//! and its `traceId` is a per-request ASP.NET activity id. The status, the
//! content type and the document's shape are the contract; the diagnostic text
//! is Ferrofin's own, and no parity probe compares it.

use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::extract::{FromRequest, OptionalFromRequest, Request};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The `type` URI ASP.NET stamps on a validation failure.
const VALIDATION_TYPE: &str = "https://tools.ietf.org/html/rfc9110#section-15.5.1";
/// The `type` URI ASP.NET stamps on an unsupported media type.
const UNSUPPORTED_MEDIA_TYPE_TYPE: &str = "https://tools.ietf.org/html/rfc9110#section-15.5.16";
/// The `title` of a validation failure, verbatim from ASP.NET.
const VALIDATION_TITLE: &str = "One or more validation errors occurred.";

/// ASP.NET always names the bound parameter alongside the specific complaint;
/// every measured rejection carried this entry.
const BODY_REQUIRED: &str = "The body field is required.";
/// The message ASP.NET uses for an absent (or JSON `null`) body.
const NON_EMPTY_REQUIRED: &str = "A non-empty request body is required.";

/// A `ProblemDetails` document — the body ASP.NET's model binder returns when it
/// cannot bind a request.
///
/// `traceId` is deliberately absent: it is a per-request ASP.NET activity id,
/// Ferrofin has no equivalent, and inventing one would be a value no client can
/// use and no probe may compare.
#[derive(Debug, Serialize)]
pub struct ProblemDetails {
    /// The problem-type URI.
    #[serde(rename = "type")]
    pub type_: &'static str,
    /// The human-readable summary.
    pub title: &'static str,
    /// The HTTP status, repeated in the document as ASP.NET repeats it.
    pub status: u16,
    /// The per-member complaints, keyed the way `ValidationProblemDetails` keys
    /// them. Omitted entirely on a 415, which carries none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<BTreeMap<String, Vec<String>>>,
}

impl ProblemDetails {
    /// A 415: the request's content type is not one the JSON binder reads.
    #[must_use]
    pub fn unsupported_media_type() -> Self {
        Self {
            type_: UNSUPPORTED_MEDIA_TYPE_TYPE,
            title: "Unsupported Media Type",
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE.as_u16(),
            errors: None,
        }
    }

    /// A 400 naming one failing member: `key` is the `errors` dictionary key
    /// (`""` for the body as a whole, `$…` for a JSON path) and `message` the
    /// complaint.
    #[must_use]
    pub fn validation(key: &str, message: impl Into<String>) -> Self {
        let mut errors = BTreeMap::new();
        errors.insert(key.to_owned(), vec![message.into()]);
        errors.insert("body".to_owned(), vec![BODY_REQUIRED.to_owned()]);
        Self {
            type_: VALIDATION_TYPE,
            title: VALIDATION_TITLE,
            status: StatusCode::BAD_REQUEST.as_u16(),
            errors: Some(errors),
        }
    }

    /// The rejection for an absent, blank or `null` body.
    #[must_use]
    fn empty_body() -> Self {
        Self::validation("", NON_EMPTY_REQUIRED)
    }
}

impl IntoResponse for ProblemDetails {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::BAD_REQUEST);
        // `application/json; charset=utf-8`, NOT `application/problem+json`:
        // measured against 10.11.8, whose `[Produces(MediaTypeNames.Application.Json, …)]`
        // on `BaseJellyfinApiController` decides the content type for the
        // filter's response too.
        let body = serde_json::to_string(&self).unwrap_or_else(|_| {
            format!(
                r#"{{"title":"{VALIDATION_TITLE}","status":{}}}"#,
                self.status
            )
        });
        (
            status,
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            body,
        )
            .into_response()
    }
}

/// Whether the request's content type is one the JSON binder accepts.
///
/// ASP.NET's `SystemTextJsonInputFormatter` claims `application/json`,
/// `text/json` and any `application/*+json`; anything else (including an absent
/// header) never reaches a formatter and is answered 415.
fn is_json_content_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let essence = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    essence == "application/json"
        || essence == "text/json"
        || (essence.starts_with("application/") && essence.ends_with("+json"))
}

/// The top-level JSON kind a DTO can be bound from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopLevel {
    /// A JSON object — every struct DTO.
    Object,
    /// A JSON array — the handful of operations whose body is a list.
    Array,
    /// Any JSON value — the port of `[FromBody] JsonDocument`, which
    /// `System.Text.Json` fills from an object, an array or a scalar alike.
    Any,
}

impl TopLevel {
    /// Whether `value` is the kind this binder will bind.
    fn accepts(self, value: &serde_json::Value) -> bool {
        match self {
            Self::Object => value.is_object(),
            Self::Array => value.is_array(),
            Self::Any => true,
        }
    }
}

/// Parses `bytes` into a JSON value, or `None` when the body says "nothing was
/// supplied" — blank, or the literal `null`.
///
/// Measured against 10.11.8: an optional body (`[FromBody] X? dto`) binds all
/// three of an absent body, an empty body and `null` to null and serves the
/// request 200; a REQUIRED one answers all three
/// "A non-empty request body is required.". So the two cases differ only in
/// what they do with this `None`.
fn parse_value(bytes: &[u8]) -> Result<Option<serde_json::Value>, ProblemDetails> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| ProblemDetails::validation("$", e.to_string()))?;
    Ok((!value.is_null()).then_some(value))
}

/// Binds a parsed `value` into `T`, rejecting exactly what `System.Text.Json`
/// rejects for the DTO's expected top-level kind.
fn bind_value<T: DeserializeOwned>(
    value: serde_json::Value,
    want: TopLevel,
) -> Result<T, ProblemDetails> {
    if !want.accepts(&value) {
        return Err(ProblemDetails::validation(
            "$",
            "The JSON value could not be converted to the expected type.",
        ));
    }
    serde_path_to_error::deserialize(value).map_err(|e| {
        let path = e.path().to_string();
        let key = if path.is_empty() || path == "." {
            "$".to_owned()
        } else {
            format!("$.{path}")
        };
        ProblemDetails::validation(&key, e.into_inner().to_string())
    })
}

/// Binds `bytes` into a REQUIRED `T`.
fn bind<T: DeserializeOwned>(bytes: &[u8], want: TopLevel) -> Result<T, ProblemDetails> {
    match parse_value(bytes)? {
        Some(value) => bind_value(value, want),
        None => Err(ProblemDetails::empty_body()),
    }
}

/// Reads the body of a REQUIRED parameter.
///
/// A content type the JSON input formatter cannot read is 415 whatever the body
/// says — measured on `POST /LiveTv/Programs`, which answers 415 to
/// `text/plain`, to `application/x-www-form-urlencoded` with an empty body, and
/// to no content type at all. The gate runs BEFORE the body is buffered, so a
/// large non-JSON upload is refused without being read.
async fn read_required<S: Send + Sync>(req: Request, state: &S) -> Result<Bytes, ProblemDetails> {
    if !is_json_content_type(req.headers()) {
        return Err(ProblemDetails::unsupported_media_type());
    }
    Bytes::from_request(req, state)
        .await
        .map_err(|e| ProblemDetails::validation("", e.body_text()))
}

/// Reads the body of an OPTIONAL parameter, reporting whether its content type
/// is one the JSON binder accepts.
///
/// The gate cannot run first here: ASP.NET only selects an input formatter when
/// there is something to read, so an EMPTY body binds to null whatever the
/// content type says. Measured on `POST /Items/{id}/PlaybackInfo`
/// (`[FromBody] PlaybackInfoDto?`): `text/plain` with an empty body is 200,
/// `text/plain` with `{}` is 415.
async fn read_optional<S: Send + Sync>(
    req: Request,
    state: &S,
) -> Result<(bool, Bytes), ProblemDetails> {
    let json_content_type = is_json_content_type(req.headers());
    let bytes = Bytes::from_request(req, state)
        .await
        .map_err(|e| ProblemDetails::validation("", e.body_text()))?;
    Ok((json_content_type, bytes))
}

/// The JSON **object** body extractor — the replacement for `axum::Json` in
/// extractor position.
///
/// `Json` remains the response type; only the binding side changes, so a
/// handler reads `JsonBody(dto): JsonBody<Dto>` and still returns
/// `Ok(Json(dto))`.
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonBody<T>(pub T);

impl<T, S> FromRequest<S> for JsonBody<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ProblemDetails;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        bind(&read_required(req, state).await?, TopLevel::Object).map(Self)
    }
}

impl<T, S> OptionalFromRequest<S> for JsonBody<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ProblemDetails;

    /// `[FromBody] T? body` semantics, measured against 10.11.8 on
    /// `POST /Items/{id}/PlaybackInfo`: an absent body, an empty body and the
    /// literal `null` all bind as `None` and the request is served; a body that
    /// is present and malformed — `[]`, a wrong-typed member, a syntax error —
    /// is still a 400. Turning THAT into "no body supplied" is what axum's
    /// `Option<Json<T>>` did with a missing content type only, while a
    /// malformed one became a 422; neither matched.
    async fn from_request(req: Request, state: &S) -> Result<Option<Self>, Self::Rejection> {
        let (json_content_type, bytes) = read_optional(req, state).await?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        if !json_content_type {
            return Err(ProblemDetails::unsupported_media_type());
        }
        match parse_value(&bytes)? {
            Some(value) => bind_value(value, TopLevel::Object).map(|v| Some(Self(v))),
            None => Ok(None),
        }
    }
}

/// The JSON **array** body extractor.
///
/// Kept separate from [`JsonBody`] rather than inferring the shape from `T`:
/// the expected top-level kind is a property of the operation's DTO, and the
/// check is the whole point — a binder that guesses cannot reject `[]` for an
/// object DTO, which is the leniency this class fix exists to close.
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonSeqBody<T>(pub T);

impl<T, S> FromRequest<S> for JsonSeqBody<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ProblemDetails;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        bind(&read_required(req, state).await?, TopLevel::Array).map(Self)
    }
}

/// The **any-shape** body extractor — the port of `[FromBody] JsonDocument`.
///
/// Exactly two operations bind one upstream (`POST /System/Configuration/{key}`,
/// v10.11.8 Jellyfin.Api/Controllers/ConfigurationController.cs:96, and the
/// intro-skipper analyzer-actions route), and `JsonDocument` takes any JSON
/// kind — so tightening those two to an object would be a divergence Ferrofin
/// invented. An empty or `null` body is still refused: the parameter is
/// `[Required]`.
#[derive(Debug, Clone, Default)]
pub struct JsonValueBody<T>(pub T);

impl<T, S> FromRequest<S> for JsonValueBody<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ProblemDetails;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        bind(&read_required(req, state).await?, TopLevel::Any).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize, Default)]
    #[serde(rename_all = "PascalCase", default)]
    struct Dto {
        limit: Option<i32>,
        names: Vec<String>,
    }

    #[test]
    fn an_object_binds_and_a_sequence_does_not() {
        let dto: Dto = bind(br#"{"Limit":3}"#, TopLevel::Object).expect("object binds");
        assert_eq!(dto.limit, Some(3));
        // The leniency this extractor exists to close: serde's derived impl
        // would take `[]` positionally and hand the handler a defaulted DTO.
        let rejected = bind::<Dto>(b"[]", TopLevel::Object).expect_err("a sequence is not a DTO");
        assert_eq!(rejected.status, 400);
        assert!(bind::<Dto>(br#"["Name"]"#, TopLevel::Object).is_err());
    }

    #[test]
    fn an_any_shape_body_takes_every_json_kind_but_still_needs_one() {
        // `[FromBody] JsonDocument` fills from an object, an array or a scalar.
        for body in [br#"{"a":1}"#.as_slice(), b"[1,2]", b"5", br#""x""#] {
            assert!(bind::<serde_json::Value>(body, TopLevel::Any).is_ok());
        }
        // …but `[Required]` still refuses an absent one.
        for body in [b"".as_slice(), b"null"] {
            assert!(bind::<serde_json::Value>(body, TopLevel::Any).is_err());
        }
    }

    #[test]
    fn a_sequence_body_binds_only_a_sequence() {
        let list: Vec<String> = bind(br#"["a","b"]"#, TopLevel::Array).expect("array binds");
        assert_eq!(list, ["a", "b"]);
        assert!(bind::<Vec<String>>(b"{}", TopLevel::Array).is_err());
    }

    #[test]
    fn an_optional_body_is_absent_for_nothing_and_still_strict_for_something() {
        // Measured on Jellyfin 10.11.8, POST /Items/{id}/PlaybackInfo: all three
        // of these are served 200 with a null DTO.
        for body in [b"".as_slice(), b"   ", b"null"] {
            assert!(
                parse_value(body).expect("not a syntax error").is_none(),
                "an optional body treats {body:?} as absent"
            );
        }
        // …and a body that IS present still binds strictly.
        let value = parse_value(b"[]").expect("parses").expect("present");
        assert!(bind_value::<Dto>(value, TopLevel::Object).is_err());
    }

    #[test]
    fn an_empty_or_null_body_is_the_non_empty_complaint() {
        for body in [b"".as_slice(), b"   ", b"null"] {
            let rejected = bind::<Dto>(body, TopLevel::Object).expect_err("rejected");
            assert_eq!(rejected.status, 400);
            assert_eq!(
                rejected.errors.as_ref().and_then(|e| e.get("")),
                Some(&vec![NON_EMPTY_REQUIRED.to_owned()])
            );
        }
    }

    #[test]
    fn a_member_that_will_not_convert_is_named_by_path() {
        let rejected = bind::<Dto>(br#"{"Names":[1]}"#, TopLevel::Object).expect_err("rejected");
        assert_eq!(rejected.status, 400);
        assert!(
            rejected
                .errors
                .as_ref()
                .is_some_and(|e| e.keys().any(|k| k.starts_with("$.Names"))),
            "the failing member is keyed by its path, not by the DTO's type name: {:?}",
            rejected.errors
        );
        // Every ASP.NET rejection also names the bound parameter.
        assert!(
            rejected
                .errors
                .as_ref()
                .is_some_and(|e| e.contains_key("body"))
        );
    }

    #[test]
    fn a_syntax_error_is_keyed_at_the_document_root() {
        let rejected = bind::<Dto>(b"{", TopLevel::Object).expect_err("rejected");
        assert!(
            rejected
                .errors
                .as_ref()
                .is_some_and(|e| e.contains_key("$"))
        );
    }

    #[test]
    fn the_content_type_gate_matches_the_input_formatter() {
        let mut headers = HeaderMap::new();
        assert!(!is_json_content_type(&headers), "an absent header is 415");
        for ok in [
            "application/json",
            "application/json; charset=utf-8",
            "TEXT/JSON",
            "application/merge-patch+json",
        ] {
            headers.insert(header::CONTENT_TYPE, ok.parse().expect("header"));
            assert!(is_json_content_type(&headers), "{ok} is JSON");
        }
        for bad in ["text/plain", "application/xml", "application/jsonish"] {
            headers.insert(header::CONTENT_TYPE, bad.parse().expect("header"));
            assert!(!is_json_content_type(&headers), "{bad} is not JSON");
        }
    }

    #[test]
    fn the_problem_document_carries_the_shape_aspnet_sends() {
        let body = serde_json::to_value(ProblemDetails::validation("$", "nope")).expect("json");
        assert_eq!(body["type"], VALIDATION_TYPE);
        assert_eq!(body["title"], VALIDATION_TITLE);
        assert_eq!(body["status"], 400);
        assert_eq!(body["errors"]["$"][0], "nope");
        // A 415 carries no `errors` member at all.
        let media = serde_json::to_value(ProblemDetails::unsupported_media_type()).expect("json");
        assert_eq!(media["status"], 415);
        assert!(media.get("errors").is_none());
    }
}
