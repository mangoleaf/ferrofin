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
    /// Stream rows behind `media-info` (has-audio/has-video).
    pub media_streams: Arc<dyn ferrofin_traits::persistence::MediaStreamRepository>,
    /// The decoder behind `extract-audio`/`extract-frames`.
    pub extractor: Arc<dyn ferrofin_traits::media_analysis::MediaExtractor>,
    /// Lyric persistence behind `write-lyrics`.
    pub lyrics: Arc<dyn ferrofin_traits::stubs::LyricManager>,
    /// Subtitle persistence behind `write-subtitles`.
    pub subtitles: Arc<dyn ferrofin_traits::subtitles::SubtitleManager>,
    /// Collection creation/updates behind the plugin-owned collection fns.
    pub collections: Arc<dyn ferrofin_traits::collections::CollectionManager>,
    /// Global analysis-decode budget shared by every plugin (per-plugin
    /// concurrency is already 1 — each plugin's calls serialize on its own
    /// runtime thread).
    pub analysis: Arc<tokio::sync::Semaphore>,
}

/// A plugin's declared public-egress allowlist, parsed once at load from
/// its `declared-egress` export. DENY BY DEFAULT: an empty policy grants no
/// public destination at all.
#[derive(Debug, Clone, Default)]
pub struct EgressPolicy {
    /// The plugin declared `*` — any public host (logged loudly at load).
    pub allow_any: bool,
    /// Exact hosts (lowercased) the plugin may contact.
    pub hosts: Vec<String>,
    /// `*.suffix` wildcard entries, stored as the lowercased suffix
    /// including the leading dot (`.fanart.tv`).
    pub suffixes: Vec<String>,
}

impl EgressPolicy {
    /// Parses the guest's declared entries (invalid/empty entries dropped).
    #[must_use]
    pub fn parse(declared: &[String]) -> Self {
        let mut policy = Self::default();
        for entry in declared {
            let entry = entry.trim().to_lowercase();
            if entry.is_empty() {
                continue;
            }
            if entry == "*" {
                policy.allow_any = true;
            } else if let Some(suffix) = entry.strip_prefix("*.") {
                policy.suffixes.push(format!(".{suffix}"));
            } else {
                policy.hosts.push(entry);
            }
        }
        policy
    }

    /// Whether `host` (a URL host string — name or IP literal) is declared.
    #[must_use]
    pub fn allows(&self, host: &str) -> bool {
        if self.allow_any {
            return true;
        }
        // url.host_str() brackets IPv6 literals (`[2001:db8::1]`) — strip
        // so a declared v6 literal can match.
        let host = host.trim_matches(['[', ']']).to_lowercase();
        self.hosts.contains(&host) || self.suffixes.iter().any(|s| host.ends_with(s.as_str()))
    }
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
    egress: &EgressPolicy,
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
    // The declared-egress gate runs on the HOST STRING, before any DNS
    // resolution — a denied fetch must not leak data through the query
    // itself. A plugin the admin granted private-network access is exempt
    // (that grant is the larger, explicit trust); everyone else may only
    // contact what their artifact declares. Deny-by-default.
    let host = url.host_str().ok_or("url has no host")?;
    if !private_http_allowed && !egress.allows(host) {
        return Err(format!(
            "destination `{host}` is not in the plugin's declared egress allowlist \
             (the plugin `{plugin_name}` declares its reachable hosts in its own \
             manifest; an empty list means no public network access)"
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

/// Downloads one remote-artwork candidate ON THE PLUGIN'S BEHALF — the
/// guest only names URLs; bytes never enter guest memory. The download runs
/// through the exact same gate as `http-fetch`: declared-egress allowlist
/// checked pre-DNS, private-address vetting with the DNS-rebinding pin,
/// redirects off. `size_cap` bounds the body
/// (`FERROFIN_WASM_IMAGE_DOWNLOAD_MB`) and `timeout` the whole GET
/// (`FERROFIN_WASM_IMAGE_TIMEOUT_SECS`).
///
/// # Errors
/// The same refusals as [`http_fetch`], a non-200 status, or an over-cap
/// body — as a plain string for the scan warn line.
pub fn download_image(
    plugin_name: &str,
    private_http_allowed: bool,
    egress: &EgressPolicy,
    size_cap: usize,
    timeout: std::time::Duration,
    url: &str,
) -> Result<Vec<u8>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("ferrofin-wasm/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("building image download client: {e}"))?;
    let response = http_fetch(
        &client,
        plugin_name,
        size_cap,
        private_http_allowed,
        egress,
        timeout,
        &HttpRequest {
            method: String::from("GET"),
            url: url.to_owned(),
            headers: Vec::new(),
            body: None,
        },
    )?;
    if response.status != 200 {
        return Err(format!("image download returned HTTP {}", response.status));
    }
    if response.body.is_empty() {
        return Err(String::from("image download returned an empty body"));
    }
    Ok(response.body)
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
pub(crate) fn summarize(
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

/// Analysis caps — hardcoded abuse guards (PLAN_MEDIA_ANALYSIS_CAPABILITY):
/// longest audio window per call, in seconds.
const AUDIO_WINDOW_MAX_SECS: f64 = 60.0;
/// Most frames per `extract-frames` call.
const FRAMES_PER_CALL_MAX: usize = 16;
/// Longest output edge for a sampled frame, in pixels.
const FRAME_DIMENSION_MAX: u32 = 320;
/// Ticks per second (Jellyfin's 100 ns unit).
const TICKS_PER_SEC: f64 = 10_000_000.0;

/// Resolves a guest-named library item to its filesystem path — the ONLY
/// way media bytes are ever addressed (a guest never supplies paths).
fn resolve_media_path(cx: &Collaborators, item_id: &str) -> Result<String, String> {
    let id: Uuid = item_id
        .parse()
        .map_err(|_| format!("item-id `{item_id}` is not a valid UUID"))?;
    let entity = cx
        .handle
        .block_on(cx.library.get_item_by_id(id))
        .map_err(|e| format!("item lookup failed: {e}"))?
        .ok_or_else(|| format!("no such library item {id}"))?;
    let path = entity
        .path
        .filter(|p| !p.is_empty())
        .ok_or_else(|| format!("item {id} has no media path"))?;
    Ok(path)
}

/// Executes `media-info` for a guest.
///
/// # Errors
/// Unknown/pathless item or repository failure, as the guest-visible string.
pub fn media_info(
    cx: &Collaborators,
    item_id: &str,
) -> Result<crate::bindings::types::MediaTechnicalInfo, String> {
    let id: Uuid = item_id
        .parse()
        .map_err(|_| format!("item-id `{item_id}` is not a valid UUID"))?;
    let entity = cx
        .handle
        .block_on(cx.library.get_item_by_id(id))
        .map_err(|e| format!("item lookup failed: {e}"))?
        .ok_or_else(|| format!("no such library item {id}"))?;
    let streams = cx
        .handle
        .block_on(cx.media_streams.get_media_streams(
            &ferrofin_traits::persistence::MediaStreamQuery {
                item_id: id,
                stream_type: None,
                index: None,
            },
        ))
        .map_err(|e| format!("stream lookup failed: {e}"))?;
    // Stream-type discriminants per the DB mapping: 0 = Audio, 1 = Video.
    Ok(crate::bindings::types::MediaTechnicalInfo {
        duration_ticks: entity.run_time_ticks.unwrap_or(0),
        has_audio: streams.iter().any(|s| s.stream_type == 0),
        has_video: streams.iter().any(|s| s.stream_type == 1),
        container: entity
            .path
            .as_deref()
            .and_then(|p| std::path::Path::new(p).extension())
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_lowercase(),
    })
}

/// Executes `extract-audio` for a guest: caps the window, clamps the spec,
/// resolves the item, and decodes under the global analysis budget.
///
/// # Errors
/// Cap violations, unknown items, or decode failures.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // tick↔second conversions: values are bounded (≤60 s windows) long before
// any cast, so precision/truncation cannot bite.
pub fn extract_audio(
    cx: &Collaborators,
    memory_limit_bytes: usize,
    window: &crate::bindings::types::AudioWindow,
) -> Result<crate::bindings::types::AudioChunk, String> {
    let duration_secs = window.duration_ticks as f64 / TICKS_PER_SEC;
    if !(0.0..=AUDIO_WINDOW_MAX_SECS).contains(&duration_secs) || duration_secs == 0.0 {
        return Err(format!(
            "audio window must be 0..{AUDIO_WINDOW_MAX_SECS} seconds (got {duration_secs:.1})"
        ));
    }
    let spec = ferrofin_traits::media_analysis::AudioSpec {
        sample_rate: window.spec.sample_rate.clamp(8_000, 48_000),
        channels: window.spec.channels.clamp(1, 2),
    };
    // Refuse windows whose decoded size cannot fit the byte budget (a
    // quarter of the plugin's memory limit) BEFORE decoding anything.
    let bytes_cap = memory_limit_bytes / 4;
    let decoded_bytes =
        (duration_secs * f64::from(spec.sample_rate) * f64::from(spec.channels) * 2.0) as usize;
    if decoded_bytes > bytes_cap {
        return Err(format!(
            "decoded window (~{decoded_bytes} bytes) exceeds the plugin's analysis budget ({bytes_cap} bytes) — request a shorter window or lower rate"
        ));
    }
    let path = resolve_media_path(cx, &window.item_id)?;
    let start_secs = (window.start_ticks.max(0)) as f64 / TICKS_PER_SEC;
    let samples = cx
        .handle
        .block_on(async {
            let _permit = cx.analysis.acquire().await;
            cx.extractor
                .extract_audio(&path, start_secs, duration_secs, spec)
                .await
        })
        .map_err(|e| format!("audio extraction failed: {e}"))?;
    Ok(crate::bindings::types::AudioChunk {
        spec: crate::bindings::types::AudioSpec {
            sample_rate: spec.sample_rate,
            channels: spec.channels,
        },
        start_ticks: window.start_ticks,
        samples,
    })
}

/// Executes `extract-frames` for a guest.
///
/// # Errors
/// Cap violations, unknown items, or decode failures.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
// tick↔second conversions on bounded timestamps.
pub fn extract_frames(
    cx: &Collaborators,
    request: &crate::bindings::types::FrameRequest,
) -> Result<Vec<crate::bindings::types::VideoFrame>, String> {
    if request.timestamps_ticks.is_empty() {
        return Ok(Vec::new());
    }
    if request.timestamps_ticks.len() > FRAMES_PER_CALL_MAX {
        return Err(format!(
            "at most {FRAMES_PER_CALL_MAX} frames per call (got {})",
            request.timestamps_ticks.len()
        ));
    }
    let max_dimension = request.max_dimension.clamp(16, FRAME_DIMENSION_MAX);
    let jpeg = matches!(request.format, crate::bindings::types::FrameFormat::Jpeg);
    let path = resolve_media_path(cx, &request.item_id)?;
    let timestamps: Vec<f64> = request
        .timestamps_ticks
        .iter()
        .map(|t| (*t).max(0) as f64 / TICKS_PER_SEC)
        .collect();
    let frames = cx
        .handle
        .block_on(async {
            let _permit = cx.analysis.acquire().await;
            cx.extractor
                .extract_frames(&path, &timestamps, max_dimension, jpeg)
                .await
        })
        .map_err(|e| format!("frame extraction failed: {e}"))?;
    Ok(frames
        .into_iter()
        .map(|f| crate::bindings::types::VideoFrame {
            ticks: (f.seconds * TICKS_PER_SEC) as i64,
            width: f.width,
            height: f.height,
            format: if f.jpeg {
                crate::bindings::types::FrameFormat::Jpeg
            } else {
                crate::bindings::types::FrameFormat::Gray8
            },
            data: f.data,
        })
        .collect())
}

/// Executes `extract-subtitle-track`. `size_cap` bounds the extracted
/// track (`FERROFIN_WASM_SUBTITLE_EXTRACT_MB`).
///
/// # Errors
/// Unknown item, decode failure, or an over-cap track.
pub fn extract_subtitle_track(
    cx: &Collaborators,
    size_cap: usize,
    item_id: &str,
    stream_index: u32,
) -> Result<Vec<u8>, String> {
    let path = resolve_media_path(cx, item_id)?;
    let bytes = cx
        .handle
        .block_on(async {
            let _permit = cx.analysis.acquire().await;
            cx.extractor.extract_subtitle(&path, stream_index).await
        })
        .map_err(|e| format!("subtitle extraction failed: {e}"))?;
    if bytes.len() > size_cap {
        return Err(format!("extracted track exceeds the {size_cap}-byte cap"));
    }
    Ok(bytes)
}

/// The plugin key/value state caps — hardcoded abuse guards in the
/// manifest-cap tradition (state is for settings/cursors, not blobs).
const STATE_KEY_MAX: usize = 256;
/// Max bytes for one state value.
const STATE_VALUE_MAX: usize = 1024 * 1024;
/// Default max total logical bytes (keys + values) for one plugin's
/// state (`FERROFIN_WASM_STATE_LIMIT_MB` overrides).
pub const STATE_TOTAL_DEFAULT: usize = 8 * 1024 * 1024;

/// Hex-encodes a state value (values are stored as hex strings — a JSON
/// `Vec<u8>` would serialize as a number array, ~4× the logical size).
fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Decodes a hex-encoded state value (`None` on malformed input).
fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// Reads a plugin's state map from disk, leniently: missing OR corrupt
/// file = empty. Only safe for READS — see [`read_state_for_write`].
fn read_state(path: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Reads the state map for a WRITE: only a genuinely-absent file may read
/// as empty — any other failure (permissions, fd exhaustion, torn write)
/// must ERROR, or a transient fault would silently wipe the plugin's
/// state on the rewrite and report success.
fn read_state_for_write(
    path: &std::path::Path,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("plugin state file is corrupt; refusing to overwrite: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(std::collections::BTreeMap::new()),
        Err(e) => Err(format!("reading plugin state: {e}")),
    }
}

/// Executes `get-state` for a guest.
#[must_use]
pub fn get_state(path: Option<&std::path::Path>, key: &str) -> Option<Vec<u8>> {
    read_state(path?).get(key).and_then(|v| from_hex(v))
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
    set_state_capped(path, key, value, STATE_TOTAL_DEFAULT)
}

/// [`set_state`] with an explicit total cap (the host passes the
/// operator-configured limit; the default wrapper serves tests/tools).
///
/// # Errors
/// Cap violations or I/O failures, as the guest-visible string.
pub fn set_state_capped(
    path: Option<&std::path::Path>,
    key: &str,
    value: Option<Vec<u8>>,
    total_cap: usize,
) -> Result<(), String> {
    let path = path.ok_or("state is not available in this context")?;
    let map = state_after_set(path, key, value, total_cap)?;
    let bytes = serde_json::to_vec(&map).map_err(|e| format!("serialize state: {e}"))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("write state: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("commit state: {e}"))
}

/// The would-be state map after setting `key` to `value`, refused at the
/// same caps as [`set_state_capped`] — WITHOUT writing. `create-collection`
/// pre-flights its ledger append through this, so a cap refusal happens
/// before the collection exists rather than after (which would orphan an
/// unowned, unmanageable collection).
fn state_after_set(
    path: &std::path::Path,
    key: &str,
    value: Option<Vec<u8>>,
    total_cap: usize,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    if key.len() > STATE_KEY_MAX {
        return Err(format!("state key exceeds {STATE_KEY_MAX} bytes"));
    }
    if let Some(v) = &value
        && v.len() > STATE_VALUE_MAX
    {
        return Err(format!("state value exceeds {STATE_VALUE_MAX} bytes"));
    }
    let mut map = read_state_for_write(path)?;
    match value {
        Some(v) => {
            map.insert(key.to_owned(), to_hex(&v));
        }
        None => {
            map.remove(key);
        }
    }
    // Logical size: hex stores two chars per byte.
    let total: usize = map.iter().map(|(k, v)| k.len() + v.len() / 2).sum();
    if total > total_cap {
        return Err(format!(
            "plugin state would exceed {total_cap} bytes in total"
        ));
    }
    Ok(map)
}

/// Cap on collection membership operations per call.
const COLLECTION_IDS_MAX: usize = 1000;
/// Cap on a created collection's name — the one guest string in the write
/// family that had no bound (same abuse-guard size as state keys).
const COLLECTION_NAME_MAX: usize = 256;
/// The host-reserved state key listing collection ids a plugin owns.
const OWNED_COLLECTIONS_KEY: &str = "host:collections";

fn parse_uuid(what: &str, raw: &str) -> Result<Uuid, String> {
    raw.parse()
        .map_err(|_| format!("{what} `{raw}` is not a valid UUID"))
}

/// Executes `set-user-data` — the strongest write a plugin has; logged
/// per call with the plugin named (see the WIT trust note).
///
/// # Errors
/// Invalid ids or a manager failure, as the guest-visible string.
pub fn set_user_data(
    cx: &Collaborators,
    plugin_name: &str,
    user_id: &str,
    item_id: &str,
    update: &crate::bindings::types::UserDataUpdate,
) -> Result<(), String> {
    let uid = parse_uuid("user-id", user_id)?;
    let iid = parse_uuid("item-id", item_id)?;
    tracing::info!(
        plugin = plugin_name,
        user = %uid,
        item = %iid,
        played = ?update.played,
        favorite = ?update.favorite,
        position = ?update.playback_position_ticks,
        "wasm plugin writes user data"
    );
    let dto = ferrofin_model::dto::UpdateUserItemDataDto {
        played: update.played,
        is_favorite: update.favorite,
        playback_position_ticks: update.playback_position_ticks,
        ..Default::default()
    };
    cx.handle
        .block_on(cx.user_data.save_user_data(uid, iid, &dto))
        .map_err(|e| format!("user-data write failed: {e}"))
}

/// Executes `write-lyrics`. `size_cap` bounds the payload
/// (`FERROFIN_WASM_WRITE_CONTENT_MB` — settings-class writes, not media).
///
/// # Errors
/// Cap/format violations or a manager failure.
pub fn write_lyrics(
    cx: &Collaborators,
    size_cap: usize,
    item_id: &str,
    format: &str,
    content: &[u8],
) -> Result<(), String> {
    if content.len() > size_cap {
        return Err(format!("lyrics exceed the {size_cap}-byte cap"));
    }
    let iid = parse_uuid("item-id", item_id)?;
    let text = std::str::from_utf8(content).map_err(|_| "lyrics must be UTF-8".to_owned())?;
    cx.handle
        .block_on(cx.lyrics.save_lyric(iid, format, text))
        .map(|_| ())
        .map_err(|e| format!("lyric write failed: {e}"))
}

/// Executes `write-subtitles`. `size_cap` bounds the payload
/// (`FERROFIN_WASM_WRITE_CONTENT_MB`).
///
/// # Errors
/// Cap violations or a manager failure.
pub fn write_subtitles(
    cx: &Collaborators,
    size_cap: usize,
    item_id: &str,
    language: &str,
    format: &str,
    content: &[u8],
) -> Result<(), String> {
    if content.len() > size_cap {
        return Err(format!("subtitles exceed the {size_cap}-byte cap"));
    }
    let iid = parse_uuid("item-id", item_id)?;
    let response = ferrofin_traits::subtitles::SubtitleResponse {
        language: language.to_owned(),
        format: format.to_owned(),
        is_forced: false,
        is_hearing_impaired: false,
        content: content.to_vec(),
    };
    cx.handle
        .block_on(cx.subtitles.upload_subtitle(iid, &response))
        .map_err(|e| format!("subtitle write failed: {e}"))
}

/// The plugin's owned-collection ledger (host-reserved state).
fn owned_collections(state_path: Option<&std::path::Path>) -> Vec<String> {
    state_path
        .and_then(|p| get_state(Some(p), OWNED_COLLECTIONS_KEY))
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Executes `create-collection` (plugin-owned; recorded in the ledger).
///
/// # Errors
/// Cap violations, missing state, or a manager failure.
pub fn create_collection(
    cx: &Collaborators,
    state_path: Option<&std::path::Path>,
    total_cap: usize,
    name: &str,
    item_ids: &[String],
) -> Result<String, String> {
    if item_ids.len() > COLLECTION_IDS_MAX {
        return Err(format!("at most {COLLECTION_IDS_MAX} items per call"));
    }
    if name.len() > COLLECTION_NAME_MAX {
        return Err(format!(
            "collection name exceeds {COLLECTION_NAME_MAX} bytes"
        ));
    }
    let ids: Vec<Uuid> = item_ids
        .iter()
        .map(|s| parse_uuid("item id", s))
        .collect::<Result<_, _>>()?;
    // Pre-flight the ledger append with a same-size placeholder (every
    // canonical UUID renders to 36 chars), so a refusal — no state in this
    // context, or the state cap — happens BEFORE the collection exists.
    let p = state_path.ok_or("state is not available in this context")?;
    let mut prospective = owned_collections(state_path);
    prospective.push(Uuid::nil().to_string());
    let bytes = serde_json::to_vec(&prospective).map_err(|e| format!("ledger: {e}"))?;
    state_after_set(p, OWNED_COLLECTIONS_KEY, Some(bytes), total_cap)?;
    let options = ferrofin_traits::collections::CollectionCreationOptions {
        name: name.to_owned(),
        parent_id: None,
        is_locked: false,
        provider_ids: std::collections::HashMap::new(),
        item_id_list: ids,
        user_ids: Vec::new(),
    };
    let entity = cx
        .handle
        .block_on(cx.collections.create_collection(&options))
        .map_err(|e| format!("collection create failed: {e}"))?;
    let id = Uuid::parse_str(&entity.id)
        .map(|u| u.to_string())
        .unwrap_or(entity.id);
    let mut owned = owned_collections(state_path);
    owned.push(id.clone());
    let bytes = serde_json::to_vec(&owned).map_err(|e| format!("ledger: {e}"))?;
    set_state_capped(state_path, OWNED_COLLECTIONS_KEY, Some(bytes), total_cap)?;
    Ok(id)
}

/// Executes `update-collection` — refused for collections the plugin does
/// not own (the ledger is host-reserved state a guest cannot edit).
///
/// # Errors
/// Ownership/cap violations or a manager failure.
pub fn update_collection(
    cx: &Collaborators,
    state_path: Option<&std::path::Path>,
    collection_id: &str,
    add: &[String],
    remove: &[String],
) -> Result<(), String> {
    if add.len() > COLLECTION_IDS_MAX || remove.len() > COLLECTION_IDS_MAX {
        return Err(format!("at most {COLLECTION_IDS_MAX} items per call"));
    }
    let cid = parse_uuid("collection-id", collection_id)?;
    // Compare the canonical rendering, not the raw guest string, so a
    // valid-but-unhyphenated UUID is not spuriously refused (the ledger
    // stores canonical ids).
    let canonical = cid.to_string();
    if !owned_collections(state_path)
        .iter()
        .any(|c| c.eq_ignore_ascii_case(&canonical))
    {
        return Err(format!(
            "collection {collection_id} is not owned by this plugin — plugins may only \
modify collections they created"
        ));
    }
    let to_uuid = |v: &[String]| -> Result<Vec<Uuid>, String> {
        v.iter().map(|s| parse_uuid("item id", s)).collect()
    };
    let add = to_uuid(add)?;
    let remove = to_uuid(remove)?;
    if !add.is_empty() {
        cx.handle
            .block_on(cx.collections.add_to_collection(cid, &add))
            .map_err(|e| format!("collection add failed: {e}"))?;
    }
    if !remove.is_empty() {
        cx.handle
            .block_on(cx.collections.remove_from_collection(cid, &remove))
            .map_err(|e| format!("collection remove failed: {e}"))?;
    }
    Ok(())
}
