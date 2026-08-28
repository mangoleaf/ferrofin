//! Transliteration of
//! `Jellyfin.Networking.Tests.Configuration.NetworkConfigurationTests`.

use ferrofin_networking::NetworkConfiguration;
use rstest::rstest;

/// `BaseUrl_ReturnsNormalized`.
#[rstest]
#[case("", "")]
#[case("/Test", "/Test")]
#[case("/Test", "Test")]
#[case("/Test", "Test/")]
#[case("/Test", "/Test/")]
#[case("/Test/2", "/Test/2")]
#[case("/Test/2", "Test/2")]
#[case("/Test/2", "Test/2/")]
#[case("/Test/2", "/Test/2/")]
fn base_url_returns_normalized(#[case] expected: &str, #[case] input: &str) {
    let config = NetworkConfiguration::default().with_base_url(input);
    assert_eq!(expected, config.base_url());
}

/// The serialized field names must be exactly the contract's, character for
/// character.
///
/// This is not cosmetic. `PascalCase` over `snake_case` renders `enable_ipv4`
/// as `EnableIpv4`, and jellyfin-web writes `EnableIPv4` — so the dashboard's
/// IPv4/IPv6 toggles and `RemoteIPFilter` land in keys the other side ignores.
/// Serde drops an unknown key without complaint, which is why this needs a test
/// rather than a careful reading.
#[test]
fn served_field_names_match_the_vendored_contract() {
    let spec_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/jellyfin-openapi-10.11.8.json"
    );
    let spec: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(spec_path).expect("the vendored contract is committed"),
    )
    .expect("the contract is valid JSON");
    let mut expected: Vec<&str> =
        spec["components"]["schemas"]["NetworkConfiguration"]["properties"]
            .as_object()
            .expect("the contract describes NetworkConfiguration")
            .keys()
            .map(String::as_str)
            .collect();
    expected.sort_unstable();

    let served = serde_json::to_value(NetworkConfiguration::default()).expect("serializes");
    let served = served.as_object().expect("serializes to an object");
    let mut got: Vec<&str> = served.keys().map(String::as_str).collect();
    got.sort_unstable();

    assert_eq!(expected, got);
}

/// A `network.json` an older Ferrofin wrote — the four names in the spelling a
/// `PascalCase` derive produced, and no `EnableUPnP` at all. Taken verbatim
/// from a live deployment.
const LEGACY_NETWORK_JSON: &str = r#"{
  "BaseUrl": "/jf",
  "EnableHttps": false,
  "RequireHttps": false,
  "CertificatePath": "",
  "CertificatePassword": "",
  "InternalHttpPort": 8097,
  "InternalHttpsPort": 8920,
  "PublicHttpPort": 8096,
  "PublicHttpsPort": 8920,
  "AutoDiscovery": true,
  "EnableIpv4": false,
  "EnableIpv6": true,
  "EnableRemoteAccess": true,
  "LocalNetworkSubnets": ["10.0.0.0/8"],
  "LocalNetworkAddresses": [],
  "KnownProxies": ["10.1.2.3"],
  "IgnoreVirtualInterfaces": true,
  "VirtualInterfaceNames": ["veth"],
  "EnablePublishedServerUriByRequest": false,
  "PublishedServerUriBySubnet": [],
  "RemoteIpFilter": ["192.168.1.5"],
  "IsRemoteIpFilterBlacklist": true
}"#;

/// Correcting the names must not orphan the documents written under the old
/// ones.
///
/// This is the upgrade path, and it is unforgiving: `POST /Startup/RemoteAccess`
/// reads this file into the struct, changes one field, and writes the whole
/// struct back — discarding a parse error and using the defaults. So a document
/// this version cannot read is not "ignored", it is *overwritten*, and the
/// operator loses their ports, subnets, proxies and remote-IP filter on the
/// first wizard visit after an upgrade.
#[test]
fn a_configuration_written_before_the_names_were_corrected_still_loads() {
    let config: NetworkConfiguration =
        serde_json::from_str(LEGACY_NETWORK_JSON).expect("an older network.json still parses");

    // The four renamed fields, read through their aliases.
    assert!(!config.enable_ipv4);
    assert!(config.enable_ipv6);
    assert_eq!(config.remote_ip_filter, ["192.168.1.5"]);
    assert!(config.is_remote_ip_filter_blacklist);
    // The field that did not exist then, defaulted rather than fatal.
    assert!(!config.enable_upnp);
    // And everything around them, which the same write-back would have reset.
    assert_eq!(config.base_url(), "/jf");
    assert_eq!(config.internal_http_port, 8097);
    assert_eq!(config.local_network_subnets, ["10.0.0.0/8"]);
    assert_eq!(config.known_proxies, ["10.1.2.3"]);
}

/// Re-serializing an old document yields the contract's spelling. That is what
/// lets a later write — the wizard, or a dashboard Save — repair `network.json`
/// on disk, and what lets the GET hand the page the right names meanwhile.
#[test]
fn loading_an_older_configuration_rewrites_it_under_the_contract_names() {
    let config: NetworkConfiguration = serde_json::from_str(LEGACY_NETWORK_JSON).expect("parses");
    let round_tripped = serde_json::to_value(&config).expect("serializes");
    let object = round_tripped.as_object().expect("an object");

    assert_eq!(object["RemoteIPFilter"], serde_json::json!(["192.168.1.5"]));
    assert_eq!(object["EnableIPv4"], serde_json::json!(false));
    for gone in [
        "RemoteIpFilter",
        "EnableIpv4",
        "EnableIpv6",
        "IsRemoteIpFilterBlacklist",
    ] {
        assert!(!object.contains_key(gone), "{gone} must not survive");
    }
}
