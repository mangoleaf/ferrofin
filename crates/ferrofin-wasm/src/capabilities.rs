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
/// needs more should issue narrower queries (by parent id or kind).
/// Self-protective cap, not a tuning knob.
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
    /// User lookups, so a user-scoped query applies parental limits.
    pub users: Arc<dyn ferrofin_traits::library::UserManager>,
    /// Per-user item data (played/favorite/resume) for enriched summaries.
    pub user_data: Arc<dyn ferrofin_traits::library::UserDataManager>,
    /// The NextUp algorithm behind the `next-up` host function.
    pub tv: Arc<dyn ferrofin_traits::tv::TvSeriesManager>,
}

/// Whether an address is in a range the private-HTTP policy denies by
/// default: loopback, link-local (incl. cloud metadata services), RFC1918,
/// CGNAT (`100.64.0.0/10` — Tailscale's default range), IPv6 ULA, and the
/// reserved/multicast blocks a public HTTP fetch should never target.
/// IPv4-mapped IPv6 unwraps to its IPv4 rule.
///
/// The CGNAT and reserved-block checks are open-coded because
/// `Ipv4Addr::is_shared`/`is_reserved` are still nightly-only and we pin
/// stable.
#[must_use]
pub fn is_private_address(addr: std::net::IpAddr) -> bool {
    match addr {
        std::net::IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_link_local()
                || v4.is_private()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()               // 224.0.0.0/4
                // CGNAT 100.64.0.0/10 (Tailscale, carrier NAT).
                || (a == 100 && (64..128).contains(&b))
                // Benchmarking 198.18.0.0/15.
                || (a == 198 && (b == 18 || b == 19))
                // IETF protocol assignments 192.0.0.0/24.
                || (a == 192 && b == 0 && v4.octets()[2] == 0)
                // Reserved for future use 240.0.0.0/4 (excl. broadcast, above).
                || a >= 240
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_address(std::net::IpAddr::V4(v4));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unicast_link_local()
                || v6.is_unique_local()
        }
    }
}

/// Enforces the private-destination policy for one URL: every address the
/// host resolves to must be public unless the plugin was allowlisted.
/// Returns the vetted address so the caller can PIN the connection to it —
/// re-resolving at request time would reopen the DNS-rebinding TOCTOU
/// window this check exists to close.
fn check_destination(url: &reqwest::Url, plugin_name: &str) -> Result<std::net::IpAddr, String> {
    let host = url.host_str().ok_or("url has no host")?;
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<std::net::IpAddr> = std::net::ToSocketAddrs::to_socket_addrs(&(host, port))
        .map_err(|e| format!("could not resolve `{host}`: {e}"))?
        .map(|sa| sa.ip())
        .collect();
    if addrs.is_empty() {
        return Err(format!("`{host}` resolved to no addresses"));
    }
    if let Some(private) = addrs.iter().find(|a| is_private_address(**a)) {
        return Err(format!(
            "destination `{host}` resolves to the private/loopback address {private}, which \
             plugins may not reach by default. If you trust the plugin `{plugin_name}`, add \
             its id to FERROFIN_WASM_PRIVATE_HTTP_ALLOW"
        ));
    }
    Ok(addrs[0])
}

/// Executes `http-fetch` for a guest: http/https only, private/loopback
/// destinations denied unless the plugin is allowlisted
/// (`private_http_allowed`), response body capped at the plugin's memory
/// limit, destination debug-logged.
///
/// # Errors
/// Invalid URL/method/scheme, a denied private destination, a transport
/// failure, or an over-cap body — all as the guest-visible error string of
/// the WIT `result`.
pub fn http_fetch(
    client: &reqwest::blocking::Client,
    plugin_name: &str,
    body_cap_bytes: usize,
    private_http_allowed: bool,
    call_timeout: std::time::Duration,
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
    // For a HOSTNAME destination under the private-address policy, the
    // vetted address must also be the CONNECTED address: a one-off client
    // pinned via `resolve()` closes the DNS-rebinding TOCTOU (a short-TTL
    // record alternating public/private would otherwise win the re-resolve
    // race). IP-literal URLs never touch DNS, and an allowlisted plugin is
    // exempt from the policy entirely — both use the shared client.
    let pinned: Option<reqwest::blocking::Client> = if private_http_allowed {
        None
    } else {
        let vetted = check_destination(&url, plugin_name)?;
        let is_name = url
            .host_str()
            .is_some_and(|h| h.parse::<std::net::IpAddr>().is_err());
        if is_name {
            let host = url.host_str().unwrap_or_default();
            Some(
                reqwest::blocking::Client::builder()
                    .timeout(call_timeout)
                    .redirect(reqwest::redirect::Policy::none())
                    .user_agent(concat!("ferrofin-wasm/", env!("CARGO_PKG_VERSION")))
                    // Port 0 = "keep the URL's port"; only the address pins.
                    .resolve(host, std::net::SocketAddr::new(vetted, 0))
                    .build()
                    .map_err(|e| format!("building pinned http client: {e}"))?,
            )
        } else {
            None
        }
    };
    let client = pinned.as_ref().unwrap_or(client);
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
    internal.is_played = query.is_played;
    internal.is_favorite = query.is_favorite;
    internal.is_resumable = query.is_resumable;
    internal.genres.clone_from(&query.genres);
    for id in &query.ids {
        internal.item_ids.push(
            id.parse()
                .map_err(|_| format!("item id `{id}` is not a valid UUID"))?,
        );
    }
    if let Some(sort) = query.sort_by.as_deref().filter(|s| !s.is_empty()) {
        let key = parse_sort_by(sort)?;
        let order = if query.sort_descending {
            ferrofin_model::dto::SortOrder::Descending
        } else {
            ferrofin_model::dto::SortOrder::Ascending
        };
        internal.order_by = vec![(key, order)];
    }
    // A user-scoped query applies the user's parental limits and unlocks
    // the per-user summary fields below.
    let user_id: Option<Uuid> = match query.user_id.as_deref().filter(|u| !u.is_empty()) {
        Some(raw) => {
            let uid: Uuid = raw
                .parse()
                .map_err(|_| format!("user-id `{raw}` is not a valid UUID"))?;
            let user = cx
                .handle
                .block_on(cx.users.get_user_by_id(uid))
                .map_err(|e| format!("user lookup failed: {e}"))?
                .ok_or_else(|| format!("no such user {uid}"))?;
            internal.set_user(user);
            Some(uid)
        }
        None => None,
    };

    let entities = cx
        .handle
        .block_on(cx.library.get_item_list(&internal))
        .map_err(|e| format!("item query failed: {e}"))?;

    // Per-user enrichment: one batch read for all returned items.
    let user_data = match user_id {
        Some(uid) => {
            let ids: Vec<Uuid> = entities
                .iter()
                .filter_map(|e| Uuid::parse_str(&e.id).ok())
                .collect();
            cx.handle
                .block_on(cx.user_data.get_user_data_batch(&ids, uid))
                .map_err(|e| format!("user data read failed: {e}"))?
        }
        None => std::collections::HashMap::new(),
    };

    Ok(entities
        .into_iter()
        .map(|e| {
            let ud = Uuid::parse_str(&e.id).ok().and_then(|u| user_data.get(&u));
            summarize(&e, ud)
        })
        .collect())
}

/// Maps a WIT `sort-by` string onto the repository sort key.
fn parse_sort_by(sort: &str) -> Result<ferrofin_model::live_tv::ItemSortBy, String> {
    use ferrofin_model::live_tv::ItemSortBy;
    Ok(match sort {
        "SortName" => ItemSortBy::SortName,
        "DateCreated" => ItemSortBy::DateCreated,
        "DatePlayed" => ItemSortBy::DatePlayed,
        "PremiereDate" => ItemSortBy::PremiereDate,
        "CommunityRating" => ItemSortBy::CommunityRating,
        "Random" => ItemSortBy::Random,
        other => return Err(format!("unknown sort-by `{other}`")),
    })
}

/// Projects one entity (+ optional per-user data) into the WIT summary.
fn summarize(
    e: &ferrofin_db::entities::base_items::BaseItemEntity,
    user_data: Option<&ferrofin_model::dto::UserItemDataDto>,
) -> ItemSummary {
    ItemSummary {
        id: Uuid::parse_str(&e.id).map_or_else(|_| e.id.clone(), |u| u.to_string()),
        name: e.name.clone().unwrap_or_default(),
        kind: ferrofin_core::item_type_lookup::kind_from_type_name(&e.type_)
            .map_or_else(|| e.type_.clone(), |k| format!("{k:?}")),
        path: e.path.clone(),
        parent_id: e
            .parent_id
            .as_deref()
            .and_then(|p| Uuid::parse_str(p).ok())
            .map(|u| u.to_string()),
        run_time_ticks: e.run_time_ticks,
        genres: e
            .genres
            .as_deref()
            .map(|g| {
                g.split('|')
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        premiere_date: e.premiere_date.map(|d| d.to_rfc3339()),
        date_created: e.date_created.map(|d| d.to_rfc3339()),
        community_rating: e.community_rating,
        production_year: e.production_year.and_then(|y| i32::try_from(y).ok()),
        is_folder: e.is_folder,
        played: user_data.map(|u| u.played),
        is_favorite: user_data.map(|u| u.is_favorite),
        playback_position_ticks: user_data.map(|u| u.playback_position_ticks),
    }
}

/// Executes `next-up` for a guest: the user's next episodes, in order.
///
/// # Errors
/// Invalid user id, or a manager failure — as the guest-visible string.
pub fn next_up(cx: &Collaborators, user_id: &str, limit: u32) -> Result<Vec<ItemSummary>, String> {
    let uid: Uuid = user_id
        .parse()
        .map_err(|_| format!("user-id `{user_id}` is not a valid UUID"))?;
    let query = ferrofin_traits::tv::NextUpQuery {
        user_id: uid,
        limit: Some(i32::try_from(limit.min(MAX_QUERY_ROWS)).unwrap_or(i32::MAX)),
        ..Default::default()
    };
    let result = cx
        .handle
        .block_on(
            cx.tv
                .get_next_up(&query, &ferrofin_traits::options::DtoOptions::default()),
        )
        .map_err(|e| format!("next-up failed: {e}"))?;
    // The NextUp trait returns wire DTOs; re-fetch the entities by id so the
    // projection stays entity-based (one code path for summaries).
    let ids: Vec<String> = result.items.iter().map(|d| d.id.to_string()).collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let ordered = query_items(
        cx,
        &ItemQuery {
            kinds: Vec::new(),
            parent_id: None,
            search_term: None,
            limit: Some(MAX_QUERY_ROWS),
            user_id: Some(uid.to_string()),
            is_played: None,
            is_favorite: None,
            is_resumable: None,
            genres: Vec::new(),
            sort_by: None,
            sort_descending: false,
            ids: ids.clone(),
        },
    )?;
    // Restore NextUp's ordering (the id-fetch does not preserve it).
    let by_id: std::collections::HashMap<String, ItemSummary> =
        ordered.into_iter().map(|s| (s.id.clone(), s)).collect();
    Ok(ids.iter().filter_map(|id| by_id.get(id).cloned()).collect())
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

/// The plugin key/value state caps — hardcoded abuse guards in the
/// manifest-cap tradition (state is for settings/cursors, not blobs).
const STATE_KEY_MAX: usize = 256;
/// Max bytes for one state value.
const STATE_VALUE_MAX: usize = 1024 * 1024;
/// Max total logical bytes (keys + values) for one plugin's state.
const STATE_TOTAL_MAX: usize = 8 * 1024 * 1024;

/// Reads a plugin's state map from disk (missing/corrupt file = empty).
fn read_state(path: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Executes `get-state` for a guest.
#[must_use]
pub fn get_state(path: Option<&std::path::Path>, key: &str) -> Option<Vec<u8>> {
    read_state(path?).remove(key)
}

/// Executes `set-state` for a guest: `None` deletes; writes are atomic
/// (temp + rename) and capped (key/value/total).
///
/// # Errors
/// Cap violations or I/O failures, as the guest-visible string.
pub fn set_state(
    path: Option<&std::path::Path>,
    key: &str,
    value: Option<Vec<u8>>,
) -> Result<(), String> {
    let path = path.ok_or("state is not available in this context")?;
    if key.len() > STATE_KEY_MAX {
        return Err(format!("state key exceeds {STATE_KEY_MAX} bytes"));
    }
    if let Some(v) = &value
        && v.len() > STATE_VALUE_MAX
    {
        return Err(format!("state value exceeds {STATE_VALUE_MAX} bytes"));
    }
    let mut map = read_state(path);
    match value {
        Some(v) => {
            map.insert(key.to_owned(), v);
        }
        None => {
            map.remove(key);
        }
    }
    let total: usize = map.iter().map(|(k, v)| k.len() + v.len()).sum();
    if total > STATE_TOTAL_MAX {
        return Err(format!(
            "plugin state would exceed {STATE_TOTAL_MAX} bytes in total"
        ));
    }
    let bytes = serde_json::to_vec(&map).map_err(|e| format!("serialize state: {e}"))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("write state: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("commit state: {e}"))
}
