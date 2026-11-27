//! The pending-flow store's bounds (D19).

use std::net::{Ipv4Addr, Ipv6Addr};

use super::*;

/// A flow from `client`, started `age` ago.
fn flow(client: IpAddr, age: Duration) -> PendingFlow {
    PendingFlow {
        provider_key: "fake".to_owned(),
        issuer: "https://id.example.org".to_owned(),
        client_id: "ferrofin".to_owned(),
        redirect_uri: "https://media.example.org/sso/oidc/fake/callback".to_owned(),
        verifier: PkceCodeVerifier::new("verifier".to_owned()),
        nonce: Nonce::new("nonce".to_owned()),
        binding_hash: [0; 32],
        created: Instant::now()
            .checked_sub(age)
            .expect("the clock reaches back"),
        client,
        purpose: FlowPurpose::SignIn,
    }
}

fn address(n: u32) -> IpAddr {
    IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + n))
}

/// An exchange code issued `age` ago.
fn exchange(age: Duration) -> PendingExchange {
    PendingExchange {
        binding_hash: [0; 32],
        user_id: Uuid::nil(),
        identity: VerifiedIdentity {
            provider_key: "fake".to_owned(),
            issuer: "https://id.example.org".to_owned(),
            subject: "subject".to_owned(),
            username: None,
            claims: serde_json::Map::new(),
        },
        purpose: FlowPurpose::SignIn,
        created: Instant::now()
            .checked_sub(age)
            .expect("the clock reaches back"),
    }
}

#[test]
fn the_limits_are_d19s_values() {
    assert_eq!(FLOW_LIFETIME, Duration::from_secs(600));
    assert_eq!(EXCHANGE_CODE_LIFETIME, Duration::from_secs(60));
    assert_eq!(
        MAX_PENDING_FLOWS_PER_ADDRESS,
        ferrofin_core::quick_connect_manager::DEFAULT_PENDING_PER_IP
    );
    assert_eq!(MAX_PENDING_FLOWS, 1000);
}

#[test]
fn an_address_keeps_its_newest_flows_up_to_its_cap() {
    let flows = PendingFlows::default();
    let client = address(1);
    let cap = u64::try_from(MAX_PENDING_FLOWS_PER_ADDRESS).expect("small");
    // Oldest first: state-0 started longest ago.
    for n in 0..=cap {
        flows.insert(
            format!("state-{n}"),
            flow(client, Duration::from_secs(cap + 1 - n)),
        );
    }
    flows.insert("other".to_owned(), flow(address(2), Duration::ZERO));
    assert_eq!(flows.len(), MAX_PENDING_FLOWS_PER_ADDRESS + 1);
    assert!(flows.take("state-0").is_none(), "the oldest went");
    assert!(flows.take(&format!("state-{cap}")).is_some());
    assert!(
        flows.take("other").is_some(),
        "another address keeps its own"
    );
}

/// D23: every address of one IPv6 /64 counts toward one cap, so a host with
/// a whole /64 cannot fill the server's cap, and another /64's pending
/// sign-in is never pushed out by it.
#[test]
fn one_ipv6_64_shares_a_cap_and_cannot_push_out_anothers_flow() {
    let flows = PendingFlows::default();
    let victim = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 3, 0, 0, 0, 1));
    flows.insert("victim".to_owned(), flow(victim, Duration::from_secs(5)));
    let cap = u64::try_from(MAX_PENDING_FLOWS).expect("small");
    for n in 0..cap {
        // A server's worth of starts, each from its own address of one /64.
        let address = Ipv6Addr::from_bits(0x2001_0db8_0001_0002_u128 << 64 | u128::from(n));
        flows.insert(
            format!("state-{n}"),
            flow(IpAddr::V6(address), Duration::from_millis(cap - n)),
        );
    }
    assert_eq!(flows.len(), MAX_PENDING_FLOWS_PER_ADDRESS + 1);
    assert!(flows.take("victim").is_some(), "another /64 keeps its own");
    assert!(flows.take(&format!("state-{}", cap - 1)).is_some());
    assert!(flows.take("state-0").is_none(), "the /64's oldest went");
}

/// An IPv4-mapped IPv6 address counts with its IPv4 address.
#[test]
fn an_ipv4_mapped_address_counts_with_its_ipv4_address() {
    let flows = PendingFlows::default();
    let cap = u64::try_from(MAX_PENDING_FLOWS_PER_ADDRESS).expect("small");
    for n in 0..cap {
        flows.insert(
            format!("state-{n}"),
            flow(address(1), Duration::from_secs(cap + 1 - n)),
        );
    }
    let IpAddr::V4(v4) = address(1) else {
        unreachable!("an IPv4 address")
    };
    flows.insert(
        "mapped".to_owned(),
        flow(IpAddr::V6(v4.to_ipv6_mapped()), Duration::ZERO),
    );
    assert_eq!(flows.len(), MAX_PENDING_FLOWS_PER_ADDRESS);
    assert!(flows.take("state-0").is_none(), "the oldest went");
    assert!(flows.take("mapped").is_some());
}

#[test]
fn the_server_keeps_its_newest_flows_up_to_its_cap() {
    let flows = PendingFlows::default();
    let cap = u32::try_from(MAX_PENDING_FLOWS).expect("small");
    for n in 0..=cap {
        let age = Duration::from_millis(u64::from(cap + 1 - n));
        flows.insert(format!("state-{n}"), flow(address(n), age));
    }
    assert_eq!(flows.len(), MAX_PENDING_FLOWS);
    assert!(flows.take("state-0").is_none(), "the oldest went");
    assert!(flows.take("state-1").is_some());
    assert!(flows.take(&format!("state-{cap}")).is_some());
}

#[test]
fn an_expired_flow_is_dropped_and_a_flow_is_taken_once() {
    let flows = PendingFlows::default();
    flows.insert(
        "stale".to_owned(),
        flow(address(1), FLOW_LIFETIME + Duration::from_secs(1)),
    );
    // Taken expired: its callback refuses it.
    let stale = flows.take("stale").expect("still there");
    assert!(stale.expired());
    assert!(flows.is_empty());

    flows.insert("stale".to_owned(), flow(address(1), FLOW_LIFETIME));
    flows.insert("fresh".to_owned(), flow(address(2), Duration::ZERO));
    assert_eq!(flows.len(), 1, "the next start drops it");
    let fresh = flows.take("fresh").expect("live");
    assert!(!fresh.expired());
    assert!(flows.take("fresh").is_none(), "once");
}

#[test]
fn an_exchange_code_is_taken_once_and_expires_after_a_minute() {
    let codes = ExchangeCodes::default();
    codes.insert("stale".to_owned(), exchange(EXCHANGE_CODE_LIFETIME));
    assert!(codes.take("stale").expect("still there").expired());
    codes.insert(
        "stale".to_owned(),
        exchange(EXCHANGE_CODE_LIFETIME + Duration::from_secs(1)),
    );
    codes.insert("fresh".to_owned(), exchange(Duration::from_secs(59)));
    assert_eq!(codes.len(), 1, "the next code drops it");
    assert!(!codes.take("fresh").expect("live").expired());
    assert!(codes.take("fresh").is_none(), "once");
    assert!(codes.is_empty());
}

#[test]
fn the_server_keeps_its_newest_exchange_codes_up_to_the_flows_cap() {
    let codes = ExchangeCodes::default();
    let cap = u64::try_from(MAX_PENDING_FLOWS).expect("small");
    for n in 0..=cap {
        codes.insert(
            format!("code-{n}"),
            exchange(Duration::from_millis(cap + 1 - n)),
        );
    }
    assert_eq!(codes.len(), MAX_PENDING_FLOWS);
    assert!(codes.take("code-0").is_none(), "the oldest went");
    assert!(codes.take("code-1").is_some());
    assert!(codes.take(&format!("code-{cap}")).is_some());
}
