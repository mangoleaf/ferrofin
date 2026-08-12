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
