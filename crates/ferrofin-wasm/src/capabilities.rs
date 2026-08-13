//! The E2 capability implementations behind the `ferrofin:plugin/host`
//! interface: host-mediated HTTP, read-only item queries, and media-segment
//! writes. Each is called synchronously from a plugin's runtime thread (a
//! plain OS thread — safe to block); async manager calls go through the
//! runtime [`Handle`] captured at wiring time.

use std::io::Read as _;
use std::sync::Arc;

use tracing::debug;
use uuid::Uuid;

use ferrofin_model::data::BaseItemKind;
use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
use ferrofin_traits::library::LibraryManager;
use ferrofin_traits::media_segments::MediaSegmentManager;
use ferrofin_traits::options::InternalItemsQuery;

use crate::bindings::types::{HttpRequest, HttpResponse, ItemQuery, ItemSummary, MediaSegment};

/// The most rows one `query-items` call may return, regardless of the
/// guest's requested limit. Documented in the WIT contract; a plugin that
/// needs more pages by parent/kind. Self-protective cap, not a tuning knob.
pub const MAX_QUERY_ROWS: u32 = 1000;

/// The manager handles a plugin's host functions call into. Installed once
/// by the composition root after the managers exist (plugin **loading**
/// happens earlier, so a guest calling these during its own load gets a
/// clean "not available" error, not a hang).
pub struct Collaborators {
    /// Runtime handle for blocking on the async manager traits.
    pub handle: tokio::runtime::Handle,
    /// Read-only item queries.
    pub library: Arc<dyn LibraryManager>,
    /// Media-segment persistence.
    pub media_segments: Arc<dyn MediaSegmentManager>,
    /// Enabled-flag reads for the dynamic-metadata adapter (and any future
    /// host path that must self-gate outside a task run).
    pub plugins: Arc<dyn ferrofin_traits::plugins::PluginManager>,
}

/// Executes `http-fetch` for a guest: http/https only, response body capped
/// at the plugin's memory limit, destination debug-logged.
///
/// # Errors
/// Invalid URL/method/scheme, a transport failure, or an over-cap body —
/// all as the guest-visible error string of the WIT `result`.
pub fn http_fetch(
    client: &reqwest::blocking::Client,
    plugin_name: &str,
    body_cap_bytes: usize,
    request: &HttpRequest,
) -> Result<HttpResponse, String> {
    let url: reqwest::Url = request
        .url
        .parse()
        .map_err(|e| format!("invalid url: {e}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(format!(
            "scheme `{}` is not allowed (http/https only)",
            url.scheme()
        ));
    }
    let method: reqwest::Method = request
        .method
        .parse()
        .map_err(|_| format!("invalid method `{}`", request.method))?;
    debug!(
        plugin = plugin_name,
        method = %method,
        url = %url,
        "wasm plugin http-fetch"
    );

    let mut builder = client.request(method, url);
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    if let Some(body) = &request.body {
        builder = builder.body(body.clone());
    }
    let response = builder.send().map_err(|e| format!("request failed: {e}"))?;

    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(n, v)| {
            (
                n.to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    // Refuse bodies that could not fit in guest memory anyway — an unbounded
    // read here would balloon HOST memory on the guest's behalf.
    let mut body = Vec::new();
    let mut limited = response.take(body_cap_bytes as u64 + 1);
    limited
        .read_to_end(&mut body)
        .map_err(|e| format!("reading response body failed: {e}"))?;
    if body.len() > body_cap_bytes {
        return Err(format!(
            "response body exceeds the plugin memory limit ({body_cap_bytes} bytes)"
        ));
    }
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

/// Executes `query-items` for a guest: filters → `InternalItemsQuery`,
/// entities → the small stable [`ItemSummary`] projection.
///
/// # Errors
/// Unknown kind names, an unparsable parent id, or a repository failure —
/// as the guest-visible error string.
pub fn query_items(cx: &Collaborators, query: &ItemQuery) -> Result<Vec<ItemSummary>, String> {
    let mut internal = InternalItemsQuery::default();
    for kind in &query.kinds {
        let parsed: BaseItemKind = serde_json::from_value(serde_json::Value::String(kind.clone()))
            .map_err(|_| format!("unknown item kind `{kind}`"))?;
        internal.include_item_types.push(parsed);
    }
    if let Some(parent) = &query.parent_id {
        internal.parent_id = parent
            .parse()
            .map_err(|_| format!("parent-id `{parent}` is not a valid UUID"))?;
    }
    internal.search_term = query.search_term.clone().filter(|s| !s.is_empty());
    let limit = query.limit.unwrap_or(MAX_QUERY_ROWS).min(MAX_QUERY_ROWS);
    internal.limit = Some(i32::try_from(limit).unwrap_or(i32::MAX));

    let entities = cx
        .handle
        .block_on(cx.library.get_item_list(&internal))
        .map_err(|e| format!("item query failed: {e}"))?;

    Ok(entities
        .into_iter()
        .map(|e| ItemSummary {
            id: Uuid::parse_str(&e.id)
                .map(|u| u.to_string())
                .unwrap_or(e.id),
            name: e.name.unwrap_or_default(),
            kind: ferrofin_core::item_type_lookup::kind_from_type_name(&e.type_)
                .map_or_else(|| e.type_.clone(), |k| format!("{k:?}")),
            path: e.path,
            parent_id: e
                .parent_id
                .and_then(|p| Uuid::parse_str(&p).ok())
                .map(|u| u.to_string()),
            run_time_ticks: e.run_time_ticks,
        })
        .collect())
}

/// Executes `write-media-segments` for a guest: replaces the segments this
/// plugin previously wrote for the item (its provider id scopes the delete),
/// leaving other providers' and user-authored segments alone.
///
/// # Errors
/// An unparsable item id, an unknown segment type, an invalid tick range,
/// or a persistence failure — as the guest-visible error string.
pub fn write_media_segments(
    cx: &Collaborators,
    provider_id: &str,
    item_id: &str,
    segments: &[MediaSegment],
) -> Result<(), String> {
    let item: Uuid = item_id
        .parse()
        .map_err(|_| format!("item-id `{item_id}` is not a valid UUID"))?;
    let mut parsed = Vec::with_capacity(segments.len());
    for segment in segments {
        let type_ = parse_segment_type(&segment.segment_type)?;
        if segment.start_ticks < 0 || segment.end_ticks <= segment.start_ticks {
            return Err(format!(
                "invalid segment range [{}, {})",
                segment.start_ticks, segment.end_ticks
            ));
        }
        parsed.push(MediaSegmentDto {
            id: Uuid::new_v4(),
            item_id: item,
            type_,
            start_ticks: segment.start_ticks,
            end_ticks: segment.end_ticks,
        });
    }

    cx.handle.block_on(async {
        cx.media_segments
            .delete_provider_segments(item, provider_id, None)
            .await
            .map_err(|e| format!("clearing previous segments failed: {e}"))?;
        for dto in &parsed {
            cx.media_segments
                .create_segment(dto, provider_id)
                .await
                .map_err(|e| format!("writing segment failed: {e}"))?;
        }
        Ok(())
    })
}

/// Parses the WIT `segment-type` name into the model enum (strict names,
/// matching the API's serde spelling).
fn parse_segment_type(name: &str) -> Result<MediaSegmentType, String> {
    match name {
        "Intro" => Ok(MediaSegmentType::Intro),
        "Outro" => Ok(MediaSegmentType::Outro),
        "Recap" => Ok(MediaSegmentType::Recap),
        "Preview" => Ok(MediaSegmentType::Preview),
        "Commercial" => Ok(MediaSegmentType::Commercial),
        other => Err(format!(
            "unknown segment-type `{other}` (expected Intro/Outro/Recap/Preview/Commercial)"
        )),
    }
}
