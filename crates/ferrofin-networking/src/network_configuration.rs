//! Port of `MediaBrowser.Common.Net.NetworkConfiguration`.
//!
//! This is the settings DTO the network manager reads. `BaseUrl` carries the
//! leading/trailing slash normalization from the C# property setter.
//!
//! The serialized names are the contract's, and several of them are pinned
//! with an explicit `rename` because `PascalCase` over `snake_case` does not
//! reproduce Jellyfin's capitalization (`EnableIPv4`, not `EnableIpv4`).
//! `served_field_names_match_the_vendored_contract` in
//! `tests/network_configuration_tests.rs` holds the whole set to the spec, so
//! adding a field means adding it there too — a name jellyfin-web does not
//! recognise is a setting that silently never arrives. Each corrected name
//! also carries a serde `alias` for the spelling Ferrofin used to write, or an
//! existing `network.json` would stop parsing.
//!
//! ## What actually acts on these settings
//!
//! The server constructs one shared [`crate::manager::NetworkManager`] for HTTP
//! access policy and peer-aware address advertisement. It applies configured LAN
//! subnets, trusted proxies, remote-IP filters, interface selection and published
//! subnet URL overrides. Saving the named network configuration updates this
//! manager; live interfaces are read at boot, on save and after an operating-system
//! network change (with `DetectNetworkChange` on, the default), and a peer URL is
//! resolved from that snapshot.
//!
//! `AutoDiscovery` is captured at the start of each server lifetime and controls
//! the UDP 7359 listener. Changing it requires a restart. `BaseUrl` is read the same
//! way: the server mounts, redirects to and advertises it for the whole lifetime,
//! below `FERROFIN_BASE_URL` and above bootstrap `base_url`, and saving a different
//! one flags a restart. `InternalHttpPort` and `InternalHttpsPort` are too: the server
//! listens on and advertises them below `--port`/`FERROFIN_PORT` (HTTPS:
//! `FERROFIN_HTTPS_PORT`) and above bootstrap `port`/`https_port`. A port at its
//! default leaves the bootstrap one, and so do equal ports (`ApplicationHost.Init`
//! resets them to the defaults); saving a different port flags a restart. The
//! advertised global URL comes from the server's bootstrap configuration
//! (CLI/environment/config.toml), not this DTO's corresponding fields. `EnableHttps`,
//! `RequireHttps` and the certificate fields do apply: at startup the server listens
//! with HTTPS on its HTTPS port when HTTPS is enabled and the certificate loads, and a
//! new certificate path flags a restart. `EnablePublishedServerUriByRequest` is read
//! live.
//!
//! `EnableUPnP` is a different case, and not a Ferrofin gap: upstream marks it
//! `[Obsolete("No longer supported")]`
//! (`MediaBrowser.Common/Net/NetworkConfiguration.cs:113`) and reads it nowhere
//! outside its own pre-startup migration DTOs. Jellyfin 10.11 does no UPnP port
//! forwarding either. The field is vestigial in both, carried because the
//! contract and the dashboard expect the key; ignoring it is exact parity, so
//! there is nothing here to warn about.

use percent_encoding::{AsciiSet, CONTROLS, percent_encode};
use serde::{Deserialize, Serialize};

/// The default value for the internal/public HTTP port.
pub const DEFAULT_HTTP_PORT: u16 = 8096;

/// The default value for the internal/public HTTPS port.
pub const DEFAULT_HTTPS_PORT: u16 = 8920;

/// Network-related server settings — port of `NetworkConfiguration`.
///
/// Field names use `snake_case`; the serde representation is `PascalCase` to
/// match the on-disk / OpenAPI contract Jellyfin uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// `default` at the container: this document is only ever written by Ferrofin
// itself, and the one caller that reads it typed
// (`POST /Startup/RemoteAccess`) writes the WHOLE struct back afterwards. So a
// field this version does not find must fall back to its default, never fail
// the parse — a failed parse there is silently replaced by defaults and
// persisted, which would wipe `RemoteIPFilter` and every other setting. That is
// how adding `EnableUPnP` alone would have broken every existing install.
#[serde(rename_all = "PascalCase", default)]
// The DTO faithfully mirrors the C# `NetworkConfiguration`, which carries this
// many independent boolean toggles; collapsing them into enums would diverge
// from the on-disk contract for no behavioral gain.
#[allow(clippy::struct_excessive_bools)]
pub struct NetworkConfiguration {
    /// URL prefix that the instance can be accessed at (normalized: leading
    /// slash, no trailing slash). The normalization is applied by
    /// [`NetworkConfiguration::set_base_url`]; assign through it rather than
    /// writing the field directly to preserve the invariant.
    #[serde(rename = "BaseUrl")]
    pub base_url: String,

    /// Whether to use HTTPS.
    pub enable_https: bool,

    /// Whether the server should force connections over HTTPS.
    pub require_https: bool,

    /// Filesystem path of an X.509 certificate to use for SSL.
    pub certificate_path: String,

    /// Password required to access the X.509 certificate data.
    pub certificate_password: String,

    /// The internal HTTP server port.
    pub internal_http_port: u16,

    /// The internal HTTPS server port.
    pub internal_https_port: u16,

    /// The public HTTP port.
    pub public_http_port: u16,

    /// The public HTTPS port.
    pub public_https_port: u16,

    /// Whether UDP server discovery is enabled (applied at server-lifetime startup).
    pub auto_discovery: bool,

    /// Whether to open the public ports with UPnP.
    ///
    /// Vestigial in both implementations — upstream marks it
    /// `[Obsolete("No longer supported")]` and never reads it. Carried so the
    /// served key set matches the contract; nothing acts on it here either.
    #[serde(rename = "EnableUPnP")]
    pub enable_upnp: bool,

    /// Whether IPv4 is enabled.
    ///
    /// `PascalCase` would give `EnableIpv4`; the contract and the C#
    /// `MediaBrowser.Common/Net/NetworkConfiguration.cs` both spell it
    /// `EnableIPv4`, so the name is pinned explicitly. Same for the three
    /// below.
    ///
    /// The `alias` reads a document Ferrofin wrote under the old, wrong name.
    /// Without it the parse fails and `POST /Startup/RemoteAccess` — which
    /// discards the error and writes the whole struct back — would reset every
    /// setting on the first upgraded boot.
    #[serde(rename = "EnableIPv4", alias = "EnableIpv4")]
    pub enable_ipv4: bool,

    /// Whether IPv6 is enabled.
    #[serde(rename = "EnableIPv6", alias = "EnableIpv6")]
    pub enable_ipv6: bool,

    /// Whether access from outside the LAN is permitted.
    pub enable_remote_access: bool,

    /// The subnets that are deemed to make up the LAN.
    pub local_network_subnets: Vec<String>,

    /// The interface addresses which the server will bind to. Empty = all.
    pub local_network_addresses: Vec<String>,

    /// The known proxies.
    pub known_proxies: Vec<String>,

    /// Whether interface names matching [`Self::virtual_interface_names`] are
    /// ignored for binding.
    pub ignore_virtual_interfaces: bool,

    /// Interface-name prefixes that should be ignored (case-insensitive).
    pub virtual_interface_names: Vec<String>,

    /// Whether the published server URI is derived from HTTP request info.
    pub enable_published_server_uri_by_request: bool,

    /// Published server URIs to advertise for specific subnets.
    pub published_server_uri_by_subnet: Vec<String>,

    /// Filter for remote IP connectivity (see
    /// [`Self::is_remote_ip_filter_blacklist`]).
    ///
    /// Persisted and served, but **not yet enforced**: the code that applies it
    /// lives in [`crate::manager::NetworkManager`], which the server does not
    /// construct — request handling falls back to a fixed RFC1918 check. Wiring
    /// the manager in is open work; until then this is a setting the dashboard
    /// can save and the server does not act on.
    #[serde(rename = "RemoteIPFilter", alias = "RemoteIpFilter")]
    pub remote_ip_filter: Vec<String>,

    /// Whether [`Self::remote_ip_filter`] is a blacklist (default is allowlist).
    #[serde(
        rename = "IsRemoteIPFilterBlacklist",
        alias = "IsRemoteIpFilterBlacklist"
    )]
    pub is_remote_ip_filter_blacklist: bool,
}

impl Default for NetworkConfiguration {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            enable_https: false,
            require_https: false,
            certificate_path: String::new(),
            certificate_password: String::new(),
            internal_http_port: DEFAULT_HTTP_PORT,
            internal_https_port: DEFAULT_HTTPS_PORT,
            public_http_port: DEFAULT_HTTP_PORT,
            public_https_port: DEFAULT_HTTPS_PORT,
            auto_discovery: true,
            // C# `EnableUPnP` has no initializer, so a fresh instance is false.
            enable_upnp: false,
            enable_ipv4: true,
            enable_ipv6: false,
            enable_remote_access: true,
            local_network_subnets: Vec::new(),
            local_network_addresses: Vec::new(),
            known_proxies: Vec::new(),
            ignore_virtual_interfaces: true,
            virtual_interface_names: vec!["veth".to_owned()],
            enable_published_server_uri_by_request: false,
            published_server_uri_by_subnet: Vec::new(),
            remote_ip_filter: Vec::new(),
            is_remote_ip_filter_blacklist: false,
        }
    }
}

impl NetworkConfiguration {
    /// The normalized base URL prefix (mirrors the C# `BaseUrl` getter).
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Sets the base URL: whitespace becomes empty, a leading `/` is ensured,
    /// and trailing slashes are removed. Canonicalization is idempotent so URL
    /// publication and HTTP mounting agree even on repeated configuration reads.
    pub fn set_base_url(&mut self, value: impl AsRef<str>) {
        self.base_url = normalize_base_url(value.as_ref());
    }

    /// Builder-style [`Self::set_base_url`].
    #[must_use]
    pub fn with_base_url(mut self, value: impl AsRef<str>) -> Self {
        self.set_base_url(value);
        self
    }
}

/// Normalizes a base path for both URL publication and HTTP mounting.
/// Removing every trailing slash keeps repeated normalization idempotent.
#[must_use]
pub fn normalize_base_url(value: &str) -> String {
    if value.trim().is_empty() {
        return String::new();
    }
    let mut normalized = value.to_owned();
    if !normalized.starts_with('/') {
        normalized.insert(0, '/');
    }
    normalized.truncate(normalized.trim_end_matches('/').len());
    normalized
}

/// What `UriBuilder.Path` escapes, plus `\`, but `%`, which [`escape_base_url`]
/// decides.
const URL_PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'\\')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// A normalized base URL as the path of a URL, escaped as `UriBuilder.Path`
/// escapes it (Jellyfin 12.4's discovery `Address`, probed live): `/my
/// jellyfin` is `/my%20jellyfin`, `/médias` is `/m%C3%A9dias`, an escape it
/// already holds is kept, and a `%` that starts none is `%25`. A `\` is `%5C`,
/// where `UriBuilder` writes a `/` that the mount would not answer.
#[must_use]
pub fn escape_base_url(base: &str) -> String {
    let mut chunks = base.split('%');
    let mut escaped: String = chunks
        .next()
        .map_or_default(|chunk| percent_encode(chunk.as_bytes(), URL_PATH).collect());
    for chunk in chunks {
        let escape = chunk
            .as_bytes()
            .get(..2)
            .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit));
        escaped.push_str(if escape { "%" } else { "%25" });
        escaped.extend(percent_encode(chunk.as_bytes(), URL_PATH));
    }
    escaped
}

/// How much of the raw `path` the base URL takes: the length of its prefix
/// that decodes to what `base` decodes to, ASCII case ignored, or `None`. So a
/// base written `/my jellyfin` or `/my%20jellyfin` matches `/my%20jellyfin/…`
/// alike, where ASP.NET compares one spelling with the other.
///
/// The match is a byte prefix, not a path segment: `/jellyfin` takes the first
/// nine bytes of `/jellyfin2/…`, so a caller checks that the rest starts with
/// `/`. `%2F` is compared as written on both sides (it is not a `/`), and an
/// empty `base` is `Some(0)`.
#[must_use]
pub fn base_url_prefix_len(path: &str, base: &str) -> Option<usize> {
    let (path, base) = (path.as_bytes(), base.as_bytes());
    let (mut at, mut matched) = (0, 0);
    while matched < base.len() {
        let (want, used) = decoded_byte(base, matched)?;
        let (byte, len) = decoded_byte(path, at)?;
        if !byte.eq_ignore_ascii_case(&want) {
            return None;
        }
        (at, matched) = (at + len, matched + used);
    }
    Some(at)
}

/// The byte at `raw[at]`, a `%XX` escape decoded, and how many raw bytes it
/// took. `%2F` is compared as written, as Kestrel leaves it: it is not a `/`.
fn decoded_byte(raw: &[u8], at: usize) -> Option<(u8, usize)> {
    let hex = |i: usize| raw.get(at + i).and_then(|&d| char::from(d).to_digit(16));
    let escaped = hex(1)
        .zip(hex(2))
        .and_then(|(hi, lo)| u8::try_from((hi << 4) | lo).ok());
    match (*raw.get(at)?, escaped) {
        (b'%', Some(byte)) if byte != b'/' => Some((byte, 3)),
        (byte, _) => Some((byte, 1)),
    }
}
