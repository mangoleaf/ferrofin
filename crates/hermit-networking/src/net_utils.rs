//! IP / subnet math — port of `MediaBrowser.Common.Net.NetworkUtils`.
//!
//! Pure address/subnet helpers: CIDR parsing (with .NET's "host bits must be
//! zero" rule), subnet containment, mask↔CIDR conversion, FQDN/host parsing and
//! URI-safe address formatting. Reuses the [`IpData`]/[`IpNetwork`] value types
//! from `hermit-model` rather than redefining them.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::LazyLock;

use fancy_regex::Regex;
use hermit_model::net::{AddressFamily, IpData, IpNetwork};

use crate::logger::Logger;
use crate::net_constants;

/// Fully-qualified domain name matcher.
///
/// Copied byte-for-byte from `NetworkUtils.FqdnGeneratedRegex` (`CheckHostName`
/// is not RFC 5892 compliant). The C# `(?im)` inline flags become the
/// case-insensitive + multi-line builder flags; the negative lookaheads require
/// `fancy-regex`.
static FQDN_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^(?!:\/\/)(?=.{1,255}$)((.{1,63}\.){0,127}(?![0-9]*$)[a-z0-9-]+\.?)(:(\d){1,5}){0,1}$")
        .expect("FQDN regex is a valid fancy-regex pattern")
});

/// The .NET `IPAddress.None` sentinel (`255.255.255.255`).
const IP_NONE: IpAddr = IpAddr::V4(Ipv4Addr::BROADCAST);

/// Returns `true` if `address` is an IPv6 link-local address (`fe80::/10`).
///
/// Mirrors `NetworkUtils.IsIPv6LinkLocal`, including the `IPv4MappedToIPv6`
/// unwrap (which makes a mapped IPv4 address return `false`).
#[must_use]
pub fn is_ipv6_link_local(address: IpAddr) -> bool {
    let address = unmap_v4_mapped(address);
    let IpAddr::V6(v6) = address else {
        return false;
    };

    let octets = v6.octets();
    let word = (u32::from(octets[0]) << 8) + u32::from(octets[1]);
    (0xfe80..=0xfebf).contains(&word) // fe80::/10 — link-local.
}

/// Converts a CIDR prefix length to a dotted-decimal mask address. IPv4 math
/// only (matches `NetworkUtils.CidrToMask`); the `family` selects the base
/// prefix width used in the shift.
#[must_use]
pub fn cidr_to_mask(cidr: u8, family: AddressFamily) -> Ipv4Addr {
    let base = if family == AddressFamily::InterNetwork {
        net_constants::MINIMUM_IPV4_PREFIX_SIZE
    } else {
        net_constants::MINIMUM_IPV6_PREFIX_SIZE
    };
    // C# computes `0xFFFFFFFF << (base - cidr)` then byte-swaps so that
    // `new IPAddress(uint)` (which reads the value in little-endian host order)
    // sees network byte order. Here the mask is interpreted directly as
    // big-endian, so the swap is unnecessary — the shifted value already yields
    // the correct dotted-decimal mask.
    let shift = u32::from(base.wrapping_sub(cidr));
    let addr: u32 = 0xFFFF_FFFFu32.wrapping_shl(shift);
    Ipv4Addr::from(addr.to_be_bytes())
}

/// Converts a subnet mask address to a CIDR prefix length. IPv4 only
/// (`NetworkUtils.MaskToCidr`). Returns the bitwise-complement byte for an
/// invalid (non-contiguous) mask, matching the C# fallback.
#[must_use]
pub fn mask_to_cidr(mask: Ipv4Addr) -> u8 {
    if mask == Ipv4Addr::UNSPECIFIED {
        return 0;
    }

    let bytes = mask.octets();
    let mut cidrnet: u8 = 0;
    let mut zeroed = false;
    for byte in bytes {
        let mut v: i32 = i32::from(byte);
        while (v & 0xFF) != 0 {
            if zeroed {
                // Invalid netmask.
                return !cidrnet;
            }

            if (v & 0x80) == 0 {
                zeroed = true;
            } else {
                cidrnet += 1;
            }

            v <<= 1;
        }
    }

    cidrnet
}

/// Converts an address into a URI-safe string. IPv6 addresses are wrapped in
/// `[ ]` with any `%scope` removed (`NetworkUtils.FormatIPString`).
#[must_use]
pub fn format_ip_string(address: Option<IpAddr>) -> String {
    let Some(address) = address else {
        return String::new();
    };

    match address {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => {
            let mut str = v6.to_string();
            if let Some(i) = str.find('%') {
                str.truncate(i);
            }
            format!("[{str}]")
        }
    }
}

/// Returns the broadcast address for `network` (`NetworkUtils.GetBroadcastAddress`).
///
/// IPv4 math only, matching the C# implementation.
#[must_use]
pub fn get_broadcast_address(network: IpNetwork) -> Ipv4Addr {
    let base = match network.base_address {
        IpAddr::V4(v4) => u32::from_be_bytes(v4.octets()),
        IpAddr::V6(_) => 0,
    };
    let mask = u32::from_be_bytes(
        cidr_to_mask(network.prefix_length, AddressFamily::InterNetwork).octets(),
    );
    // C# does `ipAddress | ~ipMaskV4` on little-endian byte reinterpretations;
    // because both operands go through the same byte reinterpretation the
    // result is order-independent, so plain big-endian math is equivalent.
    Ipv4Addr::from((base | !mask).to_be_bytes())
}

/// Returns `true` if `network` contains `address`, handling IPv4-mapped IPv6
/// (`NetworkUtils.SubnetContainsAddress`).
#[must_use]
pub fn subnet_contains_address(network: IpNetwork, address: IpAddr) -> bool {
    let address = unmap_v4_mapped(address);
    network_contains(network, address)
}

/// Tries to parse `value` into an [`IpData`], respecting `!` exclusions
/// (`NetworkUtils.TryParseToSubnet`).
///
/// Entries without a mask become a host subnet (`/32` or `/128`), except the
/// unspecified address which becomes the whole address space (`/0`).
#[must_use]
pub fn try_parse_to_subnet(value: &str, negated: bool) -> Option<IpData> {
    let mut value = value.trim();

    let mut is_address_negated = false;
    if let Some(rest) = value.strip_prefix('!') {
        is_address_negated = true;
        value = rest;
    }

    if is_address_negated != negated {
        return None;
    }

    if let Some(slash) = value.find('/') {
        let address = parse_ip(&value[..slash])?;
        let subnet = parse_ip_network(value)?;
        return Some(IpData::new(address, Some(subnet), String::new()));
    }

    let address = parse_ip(value)?;
    match address {
        IpAddr::V4(v4) => {
            if v4 == Ipv4Addr::UNSPECIFIED {
                Some(IpData::new(
                    address,
                    Some(net_constants::ipv4_any()),
                    String::new(),
                ))
            } else {
                Some(IpData::new(
                    address,
                    Some(IpNetwork::new(
                        address,
                        net_constants::MINIMUM_IPV4_PREFIX_SIZE,
                    )),
                    String::new(),
                ))
            }
        }
        IpAddr::V6(v6) => {
            if v6 == Ipv6Addr::UNSPECIFIED {
                Some(IpData::new(
                    address,
                    Some(net_constants::ipv6_any()),
                    String::new(),
                ))
            } else {
                Some(IpData::new(
                    address,
                    Some(IpNetwork::new(
                        address,
                        net_constants::MINIMUM_IPV6_PREFIX_SIZE,
                    )),
                    String::new(),
                ))
            }
        }
    }
}

/// Tries to parse an array of strings into [`IpData`] subnets, respecting `!`
/// polarity (`NetworkUtils.TryParseToSubnets`).
///
/// Off-polarity entries are skipped silently; entries matching this pass that
/// fail to parse are reported through `logger` (see [`log_invalid_subnet`]).
/// Returns `None` when nothing parsed (mirroring the C# `NotNullWhen` contract).
#[must_use]
pub fn try_parse_to_subnets(
    values: &[String],
    negated: bool,
    logger: Option<&dyn Logger>,
) -> Option<Vec<IpData>> {
    if values.is_empty() {
        return None;
    }

    let mut tmp: Option<Vec<IpData>> = None;
    for value in values {
        // Skip entries whose '!' polarity doesn't match this pass.
        let trimmed = value.trim();
        if trimmed.starts_with('!') != negated {
            continue;
        }

        if let Some(inner) = try_parse_to_subnet(value, negated) {
            tmp.get_or_insert_with(Vec::new).push(inner);
        } else {
            log_invalid_subnet(logger, value);
        }
    }

    tmp
}

/// Emits the warning(s) for an entry that failed to parse
/// (`NetworkUtils.LogInvalidSubnet`).
///
/// IPv6 prefix-only notation (a `/` and a `:` but no `::`) gets a specific,
/// actionable message; everything else gets the generic "will be ignored".
fn log_invalid_subnet(logger: Option<&dyn Logger>, value: &str) {
    let Some(logger) = logger else {
        return;
    };

    let mut trimmed = value.trim();
    if let Some(rest) = trimmed.strip_prefix('!') {
        trimmed = rest;
    }

    if let Some(slash) = trimmed.find('/')
        && trimmed.contains(':')
        && !trimmed.contains("::")
    {
        logger.warn(&format!(
            "Invalid IPv6 subnet '{value}': IPv6 prefix-only notation is not supported. Use the full notation including '::' (e.g. '{}::/{}').",
            &trimmed[..slash],
            &trimmed[slash + 1..]
        ));
        return;
    }

    logger.warn(&format!("Invalid subnet '{value}' will be ignored."));
}

/// Attempts to parse a host span into resolved addresses
/// (`NetworkUtils.TryParseHost`).
///
/// Handles bare IPs, `IP:port`, `[IPv6]`/`[IPv6]:port`, `IP/mask`, and FQDNs.
/// DNS resolution is delegated to the OS (`IPAddress`/hostname lookup); an
/// address-family filter mirrors the `isIPv4Enabled`/`isIPv6Enabled` gates.
/// Returns `None` when the host cannot be parsed into at least one address.
#[must_use]
pub fn try_parse_host(
    host: &str,
    is_ipv4_enabled: bool,
    is_ipv6_enabled: bool,
) -> Option<Vec<IpAddr>> {
    let host = host.trim();
    if host.is_empty() {
        return None;
    }

    // IPv6 with brackets, e.g. [::1] or [::1]:120.
    if host.starts_with('[') {
        if let Some(i) = host.find(']') {
            // C# recurses on host[1..(i-1)] — a `Range` whose end is exclusive,
            // so it drops the ']' *and* the char immediately before it. The
            // hosts in scope are ASCII, so byte indices equal char indices. The
            // `i >= 2` guard avoids underflow on a degenerate "[]" input.
            if i < 2 {
                return None;
            }
            return try_parse_host(&host[1..i - 1], true, false);
        }

        return None;
    }

    let hosts: Vec<&str> = host.split(':').collect();

    if hosts.len() <= 2 {
        let first_part = hosts[0];

        // Hostname or hostname:port.
        if is_match(first_part)
            && let Some(addrs) = resolve_host(first_part)
        {
            return Some(addrs);
        }

        // IPv4 or IPv4:port (or IP/mask).
        if let Some(address) = parse_ip(left_part(first_part, '/')) {
            let family = AddressFamily::of(address);
            if (family == AddressFamily::InterNetwork && (!is_ipv4_enabled && is_ipv6_enabled))
                || (family == AddressFamily::InterNetworkV6
                    && (is_ipv4_enabled && !is_ipv6_enabled))
            {
                return None;
            }

            return Some(vec![address]);
        }
    } else if !hosts.is_empty() && hosts.len() <= 9 {
        // 8 octets + port.
        if let Some(address) = parse_ip(left_part(host, '/')) {
            return Some(vec![address]);
        }
    }

    None
}

/// Parses `value` as an [`IpAddr`] with the leniencies of .NET
/// `IPAddress.Parse` for IPv6: a surrounding `[ ]` with an optional `:port`
/// after the bracket, and a `%zone` scope suffix (both of which `std`'s parser
/// rejects). The scope/port are irrelevant to the address value and dropped.
fn parse_ip(value: &str) -> Option<IpAddr> {
    // Bracketed IPv6: `[addr]`, `[addr]:port`, `[addr%scope]`, ...
    if let Some(rest) = value.strip_prefix('[') {
        let close = rest.find(']')?;
        let inner = &rest[..close];
        let after = &rest[close + 1..];
        // Anything after ']' must be empty or a `:port`.
        if !after.is_empty() {
            let port = after.strip_prefix(':')?;
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
        }
        return parse_v6_with_optional_scope(inner);
    }

    if let Ok(address) = value.parse::<IpAddr>() {
        return Some(address);
    }

    // Bare scoped IPv6 (`fe80::1%16`): only IPv6 carries a `%zone`.
    if value.contains('%') && value.contains(':') {
        return parse_v6_with_optional_scope(value);
    }

    None
}

/// Parses an IPv6 address that may carry a trailing `%zone` scope id.
fn parse_v6_with_optional_scope(value: &str) -> Option<IpAddr> {
    let base = value.split('%').next().unwrap_or(value);
    match base.parse::<IpAddr>() {
        Ok(IpAddr::V6(v6)) => Some(IpAddr::V6(v6)),
        _ => None,
    }
}

/// Returns the substring of `s` before the first `needle`, or all of `s`
/// (`Jellyfin.Extensions.StringExtensions.LeftPart`, ported in `hermit-util`).
fn left_part(s: &str, needle: char) -> &str {
    hermit_util::string_extensions::left_part(s, needle)
}

/// Runs the FQDN regex against `value`.
fn is_match(value: &str) -> bool {
    FQDN_REGEX.is_match(value).unwrap_or(false)
}

/// Resolves a hostname to its addresses using the OS resolver.
///
/// Mirrors `Dns.GetHostAddresses`: socket errors are swallowed (the caller only
/// cares whether *any* address came back). A DNS lookup that returns nothing
/// yields `None` so the caller falls through, matching the C# empty-array path.
fn resolve_host(host: &str) -> Option<Vec<IpAddr>> {
    use std::net::ToSocketAddrs;

    // Port is irrelevant; we only want the addresses.
    let addrs: Vec<IpAddr> = (host, 0u16)
        .to_socket_addrs()
        .ok()?
        .map(|sa| sa.ip())
        .collect();

    if addrs.is_empty() { None } else { Some(addrs) }
}

/// Unwraps an IPv4-mapped IPv6 address to plain IPv4 (`IsIPv4MappedToIPv6` +
/// `MapToIPv4`); other addresses pass through unchanged.
fn unmap_v4_mapped(address: IpAddr) -> IpAddr {
    if let IpAddr::V6(v6) = address
        && let Some(v4) = v6.to_ipv4_mapped()
    {
        return IpAddr::V4(v4);
    }
    address
}

/// The `IPAddress.None` sentinel, exposed for the manager's bind-address math.
#[must_use]
pub fn ip_none() -> IpAddr {
    IP_NONE
}

/// Parses a CIDR string into an [`IpNetwork`], applying .NET `IPNetwork.TryParse`
/// semantics: the prefix must be in range for the family, and any host bits
/// below the prefix are **cleared** so the base address is normalized (the .NET
/// constructor calls `ClearNonZeroBitsAfterNetworkPrefix`). Parsing fails only
/// on a malformed address or an out-of-range prefix.
fn parse_ip_network(value: &str) -> Option<IpNetwork> {
    let (addr_part, prefix_part) = value.split_once('/')?;
    let base = parse_ip(addr_part)?;
    let prefix: u8 = prefix_part.parse().ok()?;

    let normalized = match base {
        IpAddr::V4(v4) => {
            if prefix > 32 {
                return None;
            }
            let bits = u32::from_be_bytes(v4.octets());
            let cleared = clear_host_bits(u128::from(bits), 32, prefix);
            #[allow(clippy::cast_possible_truncation)]
            let cleared = cleared as u32;
            IpAddr::V4(Ipv4Addr::from(cleared.to_be_bytes()))
        }
        IpAddr::V6(v6) => {
            if prefix > 128 {
                return None;
            }
            let bits = u128::from_be_bytes(v6.octets());
            let cleared = clear_host_bits(bits, 128, prefix);
            IpAddr::V6(Ipv6Addr::from(cleared.to_be_bytes()))
        }
    };

    Some(IpNetwork::new(normalized, prefix))
}

/// Zeroes every bit below `prefix` of a `width`-bit address.
fn clear_host_bits(bits: u128, width: u8, prefix: u8) -> u128 {
    let host_bits = width - prefix;
    if host_bits == 0 {
        return bits;
    }
    if host_bits >= 128 {
        return 0;
    }
    let mask = !((1u128 << host_bits) - 1);
    bits & mask
}

/// Returns `true` if `network` contains `address` (family-aware CIDR match).
fn network_contains(network: IpNetwork, address: IpAddr) -> bool {
    match (network.base_address, address) {
        (IpAddr::V4(base), IpAddr::V4(addr)) => contains_bits(
            u128::from(u32::from_be_bytes(base.octets())),
            u128::from(u32::from_be_bytes(addr.octets())),
            32,
            network.prefix_length,
        ),
        (IpAddr::V6(base), IpAddr::V6(addr)) => contains_bits(
            u128::from_be_bytes(base.octets()),
            u128::from_be_bytes(addr.octets()),
            128,
            network.prefix_length,
        ),
        // Different families never contain each other.
        _ => false,
    }
}

/// Bit-level CIDR containment check on `width`-bit addresses.
fn contains_bits(base: u128, addr: u128, width: u8, prefix: u8) -> bool {
    if prefix == 0 {
        return true;
    }
    if prefix >= width {
        return base == addr;
    }
    let host_bits = width - prefix;
    let mask = !((1u128 << host_bits) - 1);
    (base & mask) == (addr & mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_to_mask_ipv4_common_prefixes() {
        assert_eq!(
            cidr_to_mask(24, AddressFamily::InterNetwork),
            Ipv4Addr::new(255, 255, 255, 0)
        );
        assert_eq!(
            cidr_to_mask(30, AddressFamily::InterNetwork),
            Ipv4Addr::new(255, 255, 255, 252)
        );
        assert_eq!(
            cidr_to_mask(8, AddressFamily::InterNetwork),
            Ipv4Addr::new(255, 0, 0, 0)
        );
    }

    #[test]
    fn mask_to_cidr_roundtrips_valid_masks() {
        assert_eq!(mask_to_cidr(Ipv4Addr::new(255, 255, 255, 0)), 24);
        assert_eq!(mask_to_cidr(Ipv4Addr::new(255, 255, 255, 252)), 30);
        assert_eq!(mask_to_cidr(Ipv4Addr::UNSPECIFIED), 0);
        assert_eq!(mask_to_cidr(Ipv4Addr::new(255, 0, 0, 0)), 8);
    }

    #[test]
    fn mask_to_cidr_non_contiguous_returns_complement() {
        // 255.255.255.1 has a set bit after a zero within the last byte, which
        // the C# algorithm flags as invalid, returning the bitwise complement.
        let cidr = mask_to_cidr(Ipv4Addr::new(255, 255, 255, 1));
        assert_eq!(cidr, !24u8);
    }

    #[test]
    fn format_ip_string_variants() {
        assert_eq!(format_ip_string(None), "");
        assert_eq!(
            format_ip_string(Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)))),
            "192.168.1.1"
        );
        assert_eq!(
            format_ip_string(Some(IpAddr::V6(Ipv6Addr::LOCALHOST))),
            "[::1]"
        );
    }

    #[test]
    fn get_broadcast_address_ipv4() {
        let net = IpNetwork::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 0)), 24);
        assert_eq!(get_broadcast_address(net), Ipv4Addr::new(192, 168, 1, 255));

        let net30 = IpNetwork::new(IpAddr::V4(Ipv4Addr::new(10, 128, 240, 48)), 30);
        assert_eq!(
            get_broadcast_address(net30),
            Ipv4Addr::new(10, 128, 240, 51)
        );
    }

    #[test]
    fn is_ipv6_link_local_detects_fe80() {
        let ll: IpAddr = "fe80::1".parse().unwrap();
        assert!(is_ipv6_link_local(ll));
        let global: IpAddr = "2001:db8::1".parse().unwrap();
        assert!(!is_ipv6_link_local(global));
        // A plain IPv4 address is never link-local by this predicate.
        assert!(!is_ipv6_link_local(IpAddr::V4(Ipv4Addr::new(
            192, 168, 1, 1
        ))));
    }

    #[test]
    fn ip_none_is_broadcast() {
        assert_eq!(ip_none(), IpAddr::V4(Ipv4Addr::BROADCAST));
    }

    #[test]
    fn try_parse_host_family_filter_rejects_disabled_family() {
        // The family filter only applies in the short-form (<=2 colon) branch:
        // an IPv4 address with only IPv6 enabled is rejected there.
        assert!(try_parse_host("192.168.1.1", false, true).is_none());
        // With IPv4 enabled it parses.
        assert!(try_parse_host("192.168.1.1", true, false).is_some());
    }

    #[test]
    fn try_parse_to_subnet_unspecified_maps_to_whole_space() {
        let v4 = try_parse_to_subnet("0.0.0.0", false).unwrap();
        assert_eq!(v4.subnet.prefix_length, 0);
        let v6 = try_parse_to_subnet("::", false).unwrap();
        assert_eq!(v6.subnet.prefix_length, 0);
    }
}
