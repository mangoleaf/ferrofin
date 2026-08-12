//! Transliteration of `Jellyfin.Networking.Tests.NetworkParseTests`.

mod common;

use std::net::IpAddr;

use common::CapturingLogger;
use ferrofin_networking::net_utils;
use ferrofin_networking::{
    NetworkConfiguration, NetworkManager, RemoteAccessPolicyResult, StartupConfig,
};
use rstest::rstest;
use std::rc::Rc;

/// Splits a `,`/`;`-separated list, matching the C# `String.Split` calls.
fn split(s: &str, sep: char) -> Vec<String> {
    s.split(sep).map(str::to_owned).collect()
}

const MOCK_INTERFACES: &str = "192.168.1.208/24,-16,eth16|200.200.200.200/24,11,eth11";

/// `IgnoreVirtualInterfaces`.
#[rstest]
#[case(
    "192.168.1.208/24,-16,eth16|200.200.200.200/24,11,eth11",
    "192.168.1.0/24;200.200.200.0/24",
    "[192.168.1.208/24,200.200.200.200/24]"
)]
#[case(
    "192.168.1.208/24,-16,eth16|200.200.200.200/24,11,eth11",
    "192.168.1.0/24",
    "[192.168.1.208/24]"
)]
#[case(
    "192.168.1.208,-16,eth16|200.200.200.200,11,eth11",
    "192.168.1.0/24",
    "[192.168.1.208/32]"
)]
#[case(
    "192.168.1.208/24,-16,vEthernet1|192.168.2.208/24,-16,vEthernet212|200.200.200.200/24,11,eth11",
    "192.168.1.0/24",
    "[]"
)]
#[case(
    "192.168.1.200/24,-20,vEthernet1|192.168.2.208/24,-16,vEthernet212|200.200.200.200/24,11,eth11",
    "192.168.1.0/24;200.200.200.200/24",
    "[200.200.200.200/24]"
)]
#[case(
    "192.168.1.110/24,-20,br0|192.168.1.10/24,-16,br0|200.200.200.200/24,11,eth11",
    "192.168.1.0/24",
    "[192.168.1.110/24,192.168.1.10/24]"
)]
fn ignore_virtual_interfaces(#[case] interfaces: &str, #[case] lan: &str, #[case] value: &str) {
    let conf = NetworkConfiguration {
        enable_ipv6: true,
        enable_ipv4: true,
        local_network_subnets: split(lan, ';'),
        ..Default::default()
    };
    let nm = NetworkManager::with_defaults(conf, interfaces);

    let joined = nm
        .get_internal_bind_addresses()
        .iter()
        .map(|x| format!("{}/{}", x.address, x.subnet.prefix_length))
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(value, format!("[{joined}]"));
}

/// `TryParseValidIPStringsTrue`.
#[rstest]
#[case("127.0.0.1")]
#[case("127.0.0.1/8")]
#[case("192.168.1.2")]
#[case("192.168.1.2/24")]
#[case("fd23:184f:2029:0:3139:7386:67d7:d517")]
#[case("[fd23:184f:2029:0:3139:7386:67d7:d517]")]
#[case("fe80::7add:12ff:febb:c67b%16")]
#[case("[fe80::7add:12ff:febb:c67b%16]:123")]
#[case("fe80::7add:12ff:febb:c67b%16:123")]
#[case("[fe80::7add:12ff:febb:c67b%16]")]
#[case("fd23:184f:2029:0:3139:7386:67d7:d517/56")]
fn try_parse_valid_ip_strings_true(#[case] address: &str) {
    assert!(net_utils::try_parse_to_subnet(address, false).is_some());
    assert!(net_utils::try_parse_to_subnet(&format!("!{address}"), true).is_some());
}

/// `TryParseInvalidIPStringsFalse`.
#[rstest]
#[case("127.0.0.1#")]
#[case("localhost!")]
#[case("256.128.0.0.0.1")]
#[case("fd23:184f:2029:0:3139:7386:67d7:d517:1231")]
#[case("[fd23:184f:2029:0:3139:7386:67d7:d517:1231]")]
#[case("fd23:184f:2029:0100/56")]
fn try_parse_invalid_ip_strings_false(#[case] address: &str) {
    assert!(net_utils::try_parse_to_subnet(address, false).is_none());
}

/// `TryParseToSubnets_InvalidEntries_LogsWarnings`.
#[test]
fn try_parse_to_subnets_invalid_entries_logs_warnings() {
    let logger = CapturingLogger::new();
    let values = vec![
        "10.0.0.0/8".to_owned(),
        "fd23:184f:2029:0100/56".to_owned(),
        "not-an-address".to_owned(),
    ];
    let result = net_utils::try_parse_to_subnets(&values, false, Some(&logger));
    assert!(result.is_some());
    assert_eq!(result.unwrap().len(), 1);

    // IPv6 prefix-only notation should produce a specific, actionable warning.
    assert_eq!(
        logger.warning_count_containing("IPv6 prefix-only"),
        1,
        "warnings: {:?}",
        logger.warnings()
    );
    assert_eq!(logger.warning_count_containing("fd23:184f:2029:0100/56"), 1);

    // Other malformed entries should still produce a generic warning.
    assert_eq!(logger.warning_count_containing("not-an-address"), 1);
}

/// `TryParseToSubnets_PolarityMismatchIPv4_DoesNotWarn`.
#[test]
fn try_parse_to_subnets_polarity_mismatch_ipv4_does_not_warn() {
    let logger = CapturingLogger::new();
    let values = vec![
        "127.0.0.0/8".to_owned(),
        "192.168.178.0/24".to_owned(),
        "!10.0.0.0/8".to_owned(),
    ];

    let lan = net_utils::try_parse_to_subnets(&values, false, Some(&logger));
    assert_eq!(lan.as_ref().map(Vec::len), Some(2));

    let excluded = net_utils::try_parse_to_subnets(&values, true, Some(&logger));
    assert_eq!(excluded.as_ref().map(Vec::len), Some(1));

    assert_eq!(logger.warning_count(), 0);
}

/// `TryParseToSubnets_PolarityMismatchIPv6_DoesNotWarn`.
#[test]
fn try_parse_to_subnets_polarity_mismatch_ipv6_does_not_warn() {
    let logger = CapturingLogger::new();
    let values = vec![
        "fd00::/8".to_owned(),
        "fe80::/10".to_owned(),
        "!fd12:3456:789a::/48".to_owned(),
    ];

    let lan = net_utils::try_parse_to_subnets(&values, false, Some(&logger));
    assert_eq!(lan.as_ref().map(Vec::len), Some(2));

    let excluded = net_utils::try_parse_to_subnets(&values, true, Some(&logger));
    assert_eq!(excluded.as_ref().map(Vec::len), Some(1));

    assert_eq!(logger.warning_count(), 0);
}

/// `IPv4SubnetMaskMatchesValidIPAddress`.
#[rstest]
#[case("192.168.5.85/24", "192.168.5.1")]
#[case("192.168.5.85/24", "192.168.5.254")]
#[case("10.128.240.50/30", "10.128.240.48")]
#[case("10.128.240.50/30", "10.128.240.49")]
#[case("10.128.240.50/30", "10.128.240.50")]
#[case("10.128.240.50/30", "10.128.240.51")]
#[case("127.0.0.1/8", "127.0.0.1")]
fn ipv4_subnet_mask_matches_valid_ip_address(#[case] net_mask: &str, #[case] ip_address: &str) {
    let addr: IpAddr = ip_address.parse().unwrap();
    let matches = net_utils::try_parse_to_subnet(net_mask, false)
        .is_some_and(|subnet| net_utils::subnet_contains_address(subnet.subnet, addr));
    assert!(matches);
}

/// `IPv4SubnetMaskDoesNotMatchInvalidIPAddress`.
#[rstest]
#[case("192.168.5.85/24", "192.168.4.254")]
#[case("192.168.5.85/24", "191.168.5.254")]
#[case("10.128.240.50/30", "10.128.240.47")]
#[case("10.128.240.50/30", "10.128.240.52")]
#[case("10.128.240.50/30", "10.128.239.50")]
#[case("10.128.240.50/30", "10.127.240.51")]
fn ipv4_subnet_mask_does_not_match_invalid_ip_address(
    #[case] net_mask: &str,
    #[case] ip_address: &str,
) {
    let addr: IpAddr = ip_address.parse().unwrap();
    let matches = net_utils::try_parse_to_subnet(net_mask, false)
        .is_some_and(|subnet| net_utils::subnet_contains_address(subnet.subnet, addr));
    assert!(!matches);
}

/// `IPv6SubnetMaskMatchesValidIPAddress`.
#[rstest]
#[case("2001:db8:abcd:0012::0/64", "2001:0DB8:ABCD:0012:0000:0000:0000:0000")]
#[case("2001:db8:abcd:0012::0/64", "2001:0DB8:ABCD:0012:FFFF:FFFF:FFFF:FFFF")]
#[case("2001:db8:abcd:0012::0/64", "2001:0DB8:ABCD:0012:0001:0000:0000:0000")]
#[case("2001:db8:abcd:0012::0/64", "2001:0DB8:ABCD:0012:FFFF:FFFF:FFFF:FFF0")]
#[case("2001:db8:abcd:0012::0/128", "2001:0DB8:ABCD:0012:0000:0000:0000:0000")]
fn ipv6_subnet_mask_matches_valid_ip_address(#[case] net_mask: &str, #[case] ip_address: &str) {
    let addr: IpAddr = ip_address.parse().unwrap();
    let matches = net_utils::try_parse_to_subnet(net_mask, false)
        .is_some_and(|subnet| net_utils::subnet_contains_address(subnet.subnet, addr));
    assert!(matches);
}

/// `IPv6SubnetMaskDoesNotMatchInvalidIPAddress`.
#[rstest]
#[case("2001:db8:abcd:0012::0/64", "2001:0DB8:ABCD:0011:FFFF:FFFF:FFFF:FFFF")]
#[case("2001:db8:abcd:0012::0/64", "2001:0DB8:ABCD:0013:0000:0000:0000:0000")]
#[case("2001:db8:abcd:0012::0/64", "2001:0DB8:ABCD:0013:0001:0000:0000:0000")]
#[case("2001:db8:abcd:0012::0/64", "2001:0DB8:ABCD:0011:FFFF:FFFF:FFFF:FFF0")]
#[case("2001:db8:abcd:0012::0/128", "2001:0DB8:ABCD:0012:0000:0000:0000:0001")]
fn ipv6_subnet_mask_does_not_match_invalid_ip_address(
    #[case] net_mask: &str,
    #[case] ip_address: &str,
) {
    let addr: IpAddr = ip_address.parse().unwrap();
    let matches = net_utils::try_parse_to_subnet(net_mask, false)
        .is_some_and(|subnet| net_utils::subnet_contains_address(subnet.subnet, addr));
    assert!(!matches);
}

/// `TestBindInterfaces`.
#[rstest]
#[case("192.168.1.1", "eth16,eth11", false, "eth16")]
#[case("8.8.8.8", "eth16,eth11", false, "eth11")]
#[case("10.10.10.10", "eth16", false, "eth16")]
#[case("192.168.1.1", "", false, "eth16")]
#[case("jellyfin.org", "eth16", false, "eth16")]
#[case("jellyfin.org", "", false, "eth11")]
#[case("invalid.domain.test", "", false, "eth11")]
#[case("", "", false, "eth16")]
fn test_bind_interfaces(
    #[case] source: &str,
    #[case] bind_addresses: &str,
    #[case] ipv6enabled: bool,
    #[case] result: &str,
) {
    let conf = NetworkConfiguration {
        local_network_addresses: split(bind_addresses, ','),
        enable_ipv6: ipv6enabled,
        enable_ipv4: true,
        ..Default::default()
    };
    let nm = NetworkManager::with_defaults(conf, MOCK_INTERFACES);

    // Check to see if DNS resolution is working. If not, skip test.
    if net_utils::try_parse_host(source, true, ipv6enabled).is_none() {
        return;
    }

    let mut result = result.to_owned();
    if let Some(result_obj) = nm.try_parse_interface(&result) {
        result = result_obj[0].address.to_string();
        let (intf, _) = nm.get_bind_address(source);
        assert_eq!(intf, result);
    }
}

/// `TestBindInterfaceOverrides`.
#[rstest]
#[case(
    "192.168.1.1",
    "192.168.1.0/24",
    "eth16,eth11",
    false,
    "192.168.1.0/24=internal.jellyfin",
    "internal.jellyfin"
)]
#[case(
    "8.8.8.8",
    "192.168.1.0/24",
    "eth16,eth11",
    false,
    "all=http://helloworld.com",
    "http://helloworld.com"
)]
#[case(
    "10.10.10.10",
    "192.168.1.0/24",
    "eth16",
    false,
    "external=http://internalButNotDefinedAsLan.com",
    "http://internalButNotDefinedAsLan.com"
)]
#[case(
    "192.168.1.1",
    "192.168.1.0/24",
    "",
    false,
    "external=http://helloworld.com",
    "eth16"
)]
#[case(
    "jellyfin.org",
    "192.168.1.0/24",
    "eth16",
    false,
    "external=http://helloworld.com",
    "http://helloworld.com"
)]
#[case(
    "jellyfin.org",
    "192.168.1.0/24",
    "",
    false,
    "external=http://helloworld.com",
    "http://helloworld.com"
)]
#[case("", "192.168.1.0/24", "", false, "all=http://helloworld.com", "eth16")]
#[case(
    "192.168.1.1",
    "192.168.1.0/24",
    "",
    false,
    "eth16=http://helloworld.com",
    "http://helloworld.com"
)]
fn test_bind_interface_overrides(
    #[case] source: &str,
    #[case] lan: &str,
    #[case] bind_addresses: &str,
    #[case] ipv6enabled: bool,
    #[case] published_servers: &str,
    #[case] result: &str,
) {
    let conf = NetworkConfiguration {
        local_network_subnets: split(lan, ','),
        local_network_addresses: split(bind_addresses, ','),
        enable_ipv6: ipv6enabled,
        enable_ipv4: true,
        published_server_uri_by_subnet: vec![published_servers.to_owned()],
        ..Default::default()
    };
    let nm = NetworkManager::with_defaults(conf, MOCK_INTERFACES);

    let mut result = result.to_owned();
    if let Some(result_obj) = nm.try_parse_interface(&result) {
        result = result_obj[0].address.to_string();
    }

    let (intf, _) = nm.get_bind_address(source);
    assert_eq!(result, intf);
}

/// `HasRemoteAccess_GivenWhitelist_AllowsOnlyIPsInWhitelist`.
#[rstest]
#[case(
    "185.10.10.10,200.200.200.200",
    "79.2.3.4",
    RemoteAccessPolicyResult::RejectDueToNotAllowlistedRemoteIp
)]
#[case("185.10.10.10", "185.10.10.10", RemoteAccessPolicyResult::Allow)]
#[case("", "100.100.100.100", RemoteAccessPolicyResult::Allow)]
fn has_remote_access_given_whitelist_allows_only_ips_in_whitelist(
    #[case] addresses: &str,
    #[case] remote_ip: &str,
    #[case] expected: RemoteAccessPolicyResult,
) {
    let conf = NetworkConfiguration {
        enable_ipv4: true,
        remote_ip_filter: split(addresses, ','),
        is_remote_ip_filter_blacklist: false,
        ..Default::default()
    };
    let nm = NetworkManager::with_defaults(conf, "");
    assert_eq!(
        expected,
        nm.should_allow_server_access(remote_ip.parse().unwrap())
    );
}

/// `HasRemoteAccess_GivenRemoteAccessDisabled_IgnoresAllowlist`.
#[rstest]
#[case(
    "185.10.10.10,200.200.200.200",
    "79.2.3.4",
    RemoteAccessPolicyResult::RejectDueToRemoteAccessDisabled
)]
#[case("185.10.10.10", "127.0.0.1", RemoteAccessPolicyResult::Allow)]
#[case(
    "",
    "100.100.100.100",
    RemoteAccessPolicyResult::RejectDueToRemoteAccessDisabled
)]
fn has_remote_access_given_remote_access_disabled_ignores_allowlist(
    #[case] addresses: &str,
    #[case] remote_ip: &str,
    #[case] expected: RemoteAccessPolicyResult,
) {
    let conf = NetworkConfiguration {
        enable_ipv4: true,
        enable_remote_access: false,
        remote_ip_filter: split(addresses, ','),
        is_remote_ip_filter_blacklist: false,
        ..Default::default()
    };
    let nm = NetworkManager::with_defaults(conf, "");
    assert_eq!(
        expected,
        nm.should_allow_server_access(remote_ip.parse().unwrap())
    );
}

/// `HasRemoteAccess_GivenBlacklist_BlacklistTheIPs`.
#[rstest]
#[case("185.10.10.10", "79.2.3.4", RemoteAccessPolicyResult::Allow)]
#[case(
    "185.10.10.10",
    "185.10.10.10",
    RemoteAccessPolicyResult::RejectDueToIpBlocklist
)]
#[case("", "100.100.100.100", RemoteAccessPolicyResult::Allow)]
fn has_remote_access_given_blacklist_blacklist_the_ips(
    #[case] addresses: &str,
    #[case] remote_ip: &str,
    #[case] expected: RemoteAccessPolicyResult,
) {
    let conf = NetworkConfiguration {
        enable_ipv4: true,
        remote_ip_filter: split(addresses, ','),
        is_remote_ip_filter_blacklist: true,
        ..Default::default()
    };
    let nm = NetworkManager::with_defaults(conf, "");
    assert_eq!(
        expected,
        nm.should_allow_server_access(remote_ip.parse().unwrap())
    );
}

/// `GetBindInterface_NoSourceGiven_Success`.
#[rstest]
#[case("192.168.1.209/24,-16,eth16", "192.168.1.0/24", "", "192.168.1.209")]
#[case(
    "192.168.1.208/24,-16,eth16|10.0.0.1/24,10,eth7",
    "192.168.1.0/24",
    "",
    "192.168.1.208"
)]
#[case(
    "192.168.1.208/24,-16,eth16|10.0.0.1/24,10,eth7",
    "192.168.1.0/24",
    "10.0.0.1",
    "10.0.0.1"
)]
fn get_bind_interface_no_source_given_success(
    #[case] interfaces: &str,
    #[case] lan: &str,
    #[case] bind: &str,
    #[case] result: &str,
) {
    let conf = NetworkConfiguration {
        enable_ipv4: true,
        local_network_subnets: split(lan, ','),
        local_network_addresses: split(bind, ','),
        ..Default::default()
    };
    let nm = NetworkManager::with_defaults(conf, interfaces);
    let (interface_to_use, _) = nm.get_bind_address("");
    assert_eq!(result, interface_to_use);
}

/// `GetBindInterface_ValidSourceGiven_Success`.
#[rstest]
#[case(
    "192.168.1.209/24,-16,eth16",
    "192.168.1.0/24",
    "",
    "192.168.1.210",
    "192.168.1.209"
)]
#[case(
    "192.168.1.208/24,-16,eth16|10.0.0.1/24,10,eth7",
    "192.168.1.0/24",
    "",
    "192.168.1.209",
    "192.168.1.208"
)]
#[case(
    "192.168.1.208/24,-16,eth16|10.0.0.1/24,10,eth7",
    "192.168.1.0/24",
    "",
    "8.8.8.8",
    "10.0.0.1"
)]
#[case(
    "192.168.1.208/24,-16,eth16|10.0.0.1/24,10,eth7",
    "192.168.1.0/24",
    "10.0.0.1",
    "192.168.1.209",
    "10.0.0.1"
)]
#[case(
    "192.168.1.208/24,-16,eth16|10.0.0.1/24,10,eth7",
    "192.168.1.0/24",
    "192.168.1.208,10.0.0.1",
    "8.8.8.8",
    "10.0.0.1"
)]
#[case(
    "192.168.1.208/24,-16,eth16|10.0.0.1/24,10,eth7",
    "192.168.1.0/24",
    "192.168.1.208,10.0.0.1",
    "192.168.1.210",
    "192.168.1.208"
)]
#[case(
    "192.168.1.208/24,-16,eth16|fd00::1/64,10,eth7",
    "192.168.1.0/24",
    "",
    "192.168.2.100",
    "192.168.1.208"
)]
fn get_bind_interface_valid_source_given_success(
    #[case] interfaces: &str,
    #[case] lan: &str,
    #[case] bind: &str,
    #[case] source: &str,
    #[case] result: &str,
) {
    let conf = NetworkConfiguration {
        enable_ipv4: true,
        local_network_subnets: split(lan, ','),
        local_network_addresses: split(bind, ','),
        ..Default::default()
    };
    let nm = NetworkManager::with_defaults(conf, interfaces);
    let (interface_to_use, _) = nm.get_bind_address(source);
    assert_eq!(result, interface_to_use);
}

/// Startup `PublishedServerUrl` override wins over per-subnet config.
#[test]
fn startup_published_server_url_override_wins() {
    let conf = NetworkConfiguration {
        enable_ipv4: true,
        local_network_subnets: vec!["192.168.1.0/24".to_owned()],
        published_server_uri_by_subnet: vec!["all=http://ignored.example".to_owned()],
        ..Default::default()
    };
    let startup = StartupConfig::new().with(
        ferrofin_networking::config_keys::ADDRESS_OVERRIDE_KEY,
        "http://startup.example",
    );
    let nm = NetworkManager::new(
        conf,
        startup,
        MOCK_INTERFACES,
        Rc::new(ferrofin_networking::NullLogger),
    );
    let (intf, _) = nm.get_bind_address("192.168.1.1");
    assert_eq!(intf, "http://startup.example");
}
