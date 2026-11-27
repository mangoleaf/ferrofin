//! The pace of the Single Sign-On routes (D23), under a clock the tests
//! hold: every `now` is passed in.

use ferrofin_traits::sso::SsoManager as _;
use rstest::rstest;

use super::*;

const START: OidcPacedRoute = OidcPacedRoute::Start;

fn ip(text: &str) -> IpAddr {
    text.parse().expect("an address")
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// The keys each pace holds.
fn tracked(pacing: &Pacing) -> (usize, usize) {
    let len = |pace: &Pace| pace.keys.lock().expect("keys").by_key.len();
    (len(&pacing.starts), len(&pacing.callbacks))
}

#[test]
fn the_limits_are_d23s_values() {
    assert_eq!(MAX_STARTS_PER_WINDOW, 10);
    assert_eq!(MAX_CALLBACKS_PER_WINDOW, 30);
    // The `429` page says "a minute".
    assert_eq!(PACE_WINDOW, secs(60));
    assert_eq!(
        MAX_PACED_ADDRESSES,
        10 * crate::sso::flows::MAX_PENDING_FLOWS
    );
}

#[rstest]
#[case::ipv4_as_itself("192.0.2.7", "192.0.2.7")]
#[case::loopback_as_itself("127.0.0.1", "127.0.0.1")]
#[case::ipv6_by_its_64("2001:db8:1:2:aaaa:bbbb:cccc:dddd", "2001:db8:1:2::")]
#[case::ipv6_loopback_by_its_64("::1", "::")]
#[case::ipv4_mapped_as_ipv4("::ffff:192.0.2.7", "192.0.2.7")]
fn a_client_is_keyed_by_its_address_or_its_ipv6_64(#[case] client: &str, #[case] key: &str) {
    assert_eq!(address_key(ip(client)), ip(key));
}

/// The eleventh start in a minute is refused, as is every later one until
/// the earliest of the ten is a minute old, which frees one start.
#[test]
fn the_eleventh_start_in_a_minute_is_refused_until_the_first_is_a_minute_old() {
    let pacing = Pacing::new(PaceLimits::DEFAULT);
    let client = ip("192.0.2.7");
    let t0 = Instant::now();
    for n in 0..10 {
        assert!(pacing.admit(START, client, t0 + secs(n)), "start {n}");
    }
    assert!(!pacing.admit(START, client, t0 + secs(10)), "the eleventh");
    assert!(!pacing.admit(START, client, t0 + secs(59)), "still");
    assert!(pacing.admit(START, client, t0 + secs(60)), "the first's");
    assert!(!pacing.admit(START, client, t0 + secs(60)), "one only");
    assert!(pacing.admit(START, client, t0 + secs(61)), "the second's");
}

/// No minute holds more than ten starts, however it falls: ten made across
/// the end of one minute and the start of the next are all a key gets.
#[test]
fn no_minute_holds_more_than_ten_starts_however_it_falls() {
    let pacing = Pacing::new(PaceLimits::DEFAULT);
    let client = ip("192.0.2.7");
    let t0 = Instant::now();
    assert!(pacing.admit(START, client, t0));
    for n in 0..9 {
        assert!(pacing.admit(START, client, t0 + secs(59)), "start {n}");
    }
    assert!(pacing.admit(START, client, t0 + secs(60)), "the first's");
    for at in 60..119 {
        assert!(!pacing.admit(START, client, t0 + secs(at)), "at {at} s");
    }
    for n in 0..9 {
        assert!(pacing.admit(START, client, t0 + secs(119)), "start {n}");
    }
}

/// A start another client sharing the key made is followed by fewer than a
/// cap of pending flows within its minute, however that client times its
/// own: the flow it created cannot be pushed out in its first minute.
#[test]
fn a_start_is_followed_by_fewer_than_a_flow_cap_within_its_minute() {
    let pacing = Pacing::new(PaceLimits::DEFAULT);
    let shared = ip("192.0.2.7");
    let t0 = Instant::now();
    for _ in 0..5 {
        assert!(pacing.admit(START, shared, t0));
    }
    let started = t0 + secs(30);
    assert!(pacing.admit(START, shared, started), "the start");
    // Every tenth of a second up to its minute.
    let later = (1..600_u64)
        .filter(|tenth| pacing.admit(START, shared, started + Duration::from_millis(100 * tenth)))
        .count();
    assert!(
        later < crate::sso::flows::MAX_PENDING_FLOWS_PER_ADDRESS,
        "{later} starts after it within its minute"
    );
}

/// The callback, the exchange and the Quick Connect confirmation share one
/// pace of 30 a minute, apart from the starts'.
#[test]
fn the_one_time_values_share_a_pace_of_thirty_apart_from_the_starts() {
    let pacing = Pacing::new(PaceLimits::DEFAULT);
    let client = ip("192.0.2.7");
    let t0 = Instant::now();
    let routes = [
        OidcPacedRoute::Callback,
        OidcPacedRoute::Exchange,
        OidcPacedRoute::QuickConnectConfirmation,
    ];
    for n in 0..30 {
        assert!(pacing.admit(routes[n % 3], client, t0), "value {n}");
    }
    for route in routes {
        assert!(!pacing.admit(route, client, t0 + secs(59)), "{route:?}");
    }
    assert!(
        pacing.admit(START, client, t0),
        "the starts are paced apart"
    );
    for route in routes {
        assert!(pacing.admit(route, client, t0 + secs(60)), "{route:?}");
    }
}

/// Two addresses in one /64 are one client; in two /64s, two; an
/// IPv4-mapped address is its IPv4 address.
#[rstest]
#[case::one_ipv6_64("2001:db8:1:2::1", "2001:db8:1:2:ffff::9", true)]
#[case::two_ipv6_64s("2001:db8:1:2::1", "2001:db8:1:3::1", false)]
#[case::mapped_and_its_ipv4("::ffff:192.0.2.7", "192.0.2.7", true)]
#[case::two_ipv4_addresses("192.0.2.7", "192.0.2.8", false)]
fn addresses_share_a_pace_by_their_key(
    #[case] first: &str,
    #[case] second: &str,
    #[case] shared: bool,
) {
    let pacing = Pacing::new(PaceLimits::DEFAULT);
    let t0 = Instant::now();
    for _ in 0..MAX_STARTS_PER_WINDOW {
        assert!(pacing.admit(START, ip(first), t0));
    }
    assert_eq!(pacing.admit(START, ip(second), t0), !shared);
}

/// The map never holds more than its bound, many addresses at once
/// included, and an idle key is forgotten at the first sweep after its
/// window is over.
#[test]
fn the_state_is_bounded_and_drops_idle_keys() {
    let limits = PaceLimits {
        max_tracked: 64,
        ..PaceLimits::DEFAULT
    };
    let pacing = Pacing::new(limits);
    let t0 = Instant::now();
    for n in 0..1000_u32 {
        pacing.admit(START, IpAddr::from(n.to_be_bytes()), t0);
        assert!(tracked(&pacing).0 <= limits.max_tracked, "{n}");
    }
    // A minute later the map reaches its next sweep within the bound's
    // worth of new addresses, which forgets every earlier one.
    let later: Vec<IpAddr> = (0..64_u32)
        .map(|n| IpAddr::from((n + 5000).to_be_bytes()))
        .collect();
    for &client in &later {
        pacing.admit(START, client, t0 + secs(60));
    }
    let keys = pacing.starts.keys.lock().expect("keys");
    assert!(
        keys.by_key.keys().all(|key| later.contains(key)),
        "{:?}",
        keys.by_key.len()
    );
    drop(keys);
    assert_eq!(tracked(&pacing).1, 0, "the other pace holds nothing");
}

/// At its bound, a pace forgets the keys under their limit first: a key at
/// its limit stays refused while new addresses arrive.
#[test]
fn at_its_bound_a_pace_keeps_the_keys_at_their_limit() {
    let pacing = Pacing::new(PaceLimits {
        max_tracked: 8,
        ..PaceLimits::DEFAULT
    });
    let busy = ip("192.0.2.1");
    let t0 = Instant::now();
    for _ in 0..MAX_STARTS_PER_WINDOW {
        assert!(pacing.admit(START, busy, t0));
    }
    for n in 0..100_u32 {
        assert!(pacing.admit(START, IpAddr::from((n + 1000).to_be_bytes()), t0));
        assert!(!pacing.admit(START, busy, t0), "{n}");
    }
}

/// Past its bound with every key at its limit (one start each here), a pace
/// forgets some of them all the same: the map never outgrows its bound.
#[test]
fn at_its_bound_with_every_key_at_its_limit_a_pace_still_keeps_its_bound() {
    let limits = PaceLimits {
        starts: 1,
        max_tracked: 8,
        ..PaceLimits::DEFAULT
    };
    let pacing = Pacing::new(limits);
    let t0 = Instant::now();
    for n in 0..20_u32 {
        assert!(pacing.admit(START, IpAddr::from((n + 1000).to_be_bytes()), t0));
        assert!(tracked(&pacing).0 <= limits.max_tracked, "{n}");
    }
}

/// The page reflects nothing from the request, and links back to
/// jellyfin-web from each route's path.
#[rstest]
#[case::start(OidcPacedRoute::Start, "../../../web/")]
#[case::callback(OidcPacedRoute::Callback, "../../../web/")]
#[case::exchange(OidcPacedRoute::Exchange, "../../web/")]
#[case::confirmation(OidcPacedRoute::QuickConnectConfirmation, "../web/")]
fn the_page_says_to_wait_and_links_home_from_its_route(
    #[case] route: OidcPacedRoute,
    #[case] home: &str,
) {
    let page = paced_page(route);
    assert_eq!(page.status, 429);
    assert_eq!(page.location, None);
    assert!(page.set_cookies.is_empty(), "no cookie");
    assert!(page.html.contains("Wait a minute, then try again."));
    assert!(
        page.html.contains(&format!("<a href=\"{home}\">")),
        "{}",
        page.html
    );
    assert!(page.content_security_policy.is_some());
}

/// Through the service: after ten starts from one address the eleventh is
/// answered the `429` page (the route then calls nothing else:
/// `tests/sso_handlers.rs`), while another address still starts; a service
/// under other limits paces by those.
#[tokio::test]
async fn the_service_answers_the_page_past_the_pace() {
    let harness = crate::sso::sign_in::tests::Harness::new().await;
    let client = crate::sso::sign_in::tests::CLIENT;
    for _ in 0..MAX_STARTS_PER_WINDOW {
        assert_eq!(harness.sso.pace_oidc(START, client), None);
        harness.start().await;
    }
    let paced = harness.sso.pace_oidc(START, client).expect("paced");
    assert_eq!(paced, paced_page(START));
    assert_eq!(harness.sso.pace_oidc(START, ip("192.0.2.200")), None);

    let sso = crate::sso::tests::service(true, None)
        .await
        .0
        .with_pace_limits(PaceLimits {
            starts: 1,
            ..PaceLimits::DEFAULT
        });
    assert_eq!(sso.pace_oidc(START, client), None);
    assert!(sso.pace_oidc(START, client).is_some());
}
