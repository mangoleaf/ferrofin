//! The pending-flow store's bounds (D19).

use std::net::Ipv4Addr;

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

#[test]
fn the_limits_are_d19s_values() {
    assert_eq!(FLOW_LIFETIME, Duration::from_secs(600));
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
