//! Operating-system interface snapshots for Jellyfin-compatible address selection.

use std::collections::HashMap;
use std::net::IpAddr;

use ferrofin_model::net::{IpData, IpNetwork};

use crate::net_constants;

/// Returns usable addresses, or the enabled loopbacks when enumeration fails/empties.
pub(crate) fn enumerate(ipv4: bool, ipv6: bool) -> Vec<IpData> {
    let interfaces = match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces,
        Err(error) => {
            tracing::warn!(%error, "Could not enumerate network interfaces; using loopback addresses");
            return loopbacks(ipv4, ipv6);
        }
    };
    map_interfaces(interfaces, ipv4, ipv6, is_up)
}

fn map_interfaces(
    interfaces: Vec<if_addrs::Interface>,
    ipv4: bool,
    ipv6: bool,
    mut operational: impl FnMut(&if_addrs::Interface) -> bool,
) -> Vec<IpData> {
    let mut states = HashMap::new();
    let result: Vec<_> = interfaces.into_iter().filter_map(|interface| {
        let up = *states.entry(interface.name.clone()).or_insert_with(|| operational(&interface));
        if !up || (interface.ip().is_ipv4() && !ipv4) || (interface.ip().is_ipv6() && !ipv6) {
            return None;
        }
        let index = interface.index.and_then(|index| i32::try_from(index).ok());
        let Some(index) = index else {
            tracing::warn!(interface = %interface.name, "Interface has no usable index; skipping address");
            return None;
        };
        let prefix = match &interface.addr {
            if_addrs::IfAddr::V4(address) => address.prefixlen,
            if_addrs::IfAddr::V6(address) => address.prefixlen,
        };
        let address = interface.ip();
        let mut data = IpData::new(address, Some(IpNetwork::new(address, prefix)), interface.name);
        data.index = index;
        // Multicast capability is not consumed by discovery or policy resolution.
        // if-addrs deliberately avoids additional metadata collection.
        Some(data)
    }).collect();
    if result.is_empty() {
        tracing::warn!("No usable network interfaces found; using loopback addresses");
        loopbacks(ipv4, ipv6)
    } else {
        result
    }
}

#[cfg(target_os = "linux")]
fn is_up(interface: &if_addrs::Interface) -> bool {
    // The local Jellyfin targets .NET 10: its native PAL requires IFF_UP and
    // IFF_RUNNING, not Linux operstate == up (loopback commonly has Unknown).
    // if-addrs supplies IFF_RUNNING; sysfs flags supplies administrative IFF_UP.
    if !interface.is_oper_up() {
        return false;
    }
    let path = std::path::Path::new("/sys/class/net")
        .join(&interface.name)
        .join("flags");
    match std::fs::read_to_string(path).and_then(|flags| {
        u32::from_str_radix(flags.trim().trim_start_matches("0x"), 16)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }) {
        Ok(flags) => flags & 1 != 0, // IFF_UP, Linux uapi/linux/if.h.
        Err(error) => {
            tracing::warn!(interface = %interface.name, %error, "Could not read interface flags; using interface running flag");
            true
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn is_up(interface: &if_addrs::Interface) -> bool {
    interface.is_oper_up()
}

#[cfg(target_os = "macos")]
fn is_up(interface: &if_addrs::Interface) -> bool {
    match super::macos_interfaces::is_up(&interface.name) {
        Ok(up) => up,
        Err(error) => {
            tracing::warn!(interface = %interface.name, %error, "Could not read interface media state; skipping address");
            false
        }
    }
}

fn loopbacks(ipv4: bool, ipv6: bool) -> Vec<IpData> {
    let mut interfaces = Vec::new();
    if ipv4 {
        interfaces.push(IpData::new(
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            Some(net_constants::ipv4_rfc5735_loopback()),
            "lo",
        ));
    }
    if ipv6 {
        interfaces.push(IpData::new(
            IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            Some(net_constants::ipv6_rfc4291_loopback()),
            "lo",
        ));
    }
    interfaces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_fallback_respects_enabled_families() {
        let found = loopbacks(false, false);
        assert!(found.is_empty(), "{found:?}");
        let v4 = loopbacks(true, false);
        assert_eq!(v4.len(), 1);
        assert!(v4[0].address.is_ipv4());
        assert_eq!(v4[0].subnet.prefix_length, 8);
        let v6 = loopbacks(false, true);
        assert_eq!(v6.len(), 1);
        assert!(v6[0].address.is_ipv6());
        assert_eq!(loopbacks(true, true).len(), 2);
    }

    fn v4_interface(
        name: &str,
        index: Option<u32>,
        status: if_addrs::IfOperStatus,
    ) -> if_addrs::Interface {
        if_addrs::Interface {
            name: name.to_owned(),
            index,
            oper_status: status,
            is_p2p: false,
            addr: if_addrs::IfAddr::V4(if_addrs::Ifv4Addr {
                ip: "192.168.1.2".parse().expect("address"),
                netmask: "255.255.255.0".parse().expect("mask"),
                prefixlen: 24,
                broadcast: None,
            }),
        }
    }

    #[test]
    fn adapter_keeps_prefix_index_and_name_and_rejects_down_or_unknown_index() {
        let result = map_interfaces(
            vec![
                v4_interface("lan", Some(5), if_addrs::IfOperStatus::Up),
                v4_interface("down", Some(6), if_addrs::IfOperStatus::Down),
                v4_interface("missing", None, if_addrs::IfOperStatus::Up),
                v4_interface("overflow", Some(u32::MAX), if_addrs::IfOperStatus::Up),
            ],
            true,
            false,
            if_addrs::Interface::is_oper_up,
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "lan");
        assert_eq!(result[0].index, 5);
        assert_eq!(result[0].subnet.prefix_length, 24);
        assert_eq!(result[0].address.to_string(), "192.168.1.2");
    }

    #[test]
    fn adapter_maps_ipv6_and_checks_state_once_per_interface() {
        let v4 = v4_interface("lan", Some(5), if_addrs::IfOperStatus::Up);
        let mut v6 = v4.clone();
        v6.addr = if_addrs::IfAddr::V6(if_addrs::Ifv6Addr {
            ip: "fd00::2".parse().expect("address"),
            netmask: "ffff:ffff:ffff:ffff::".parse().expect("mask"),
            prefixlen: 64,
            broadcast: None,
        });
        let mut calls = 0;
        let result = map_interfaces(vec![v4.clone(), v6.clone()], true, true, |_| {
            calls += 1;
            true
        });
        assert_eq!(calls, 1);
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].subnet.prefix_length, 64);
        assert_eq!(result[1].address.to_string(), "fd00::2");
        let found = map_interfaces(vec![v4.clone(), v6.clone()], false, false, |_| true);
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(map_interfaces(vec![v4, v6], false, true, |_| true).len(), 1);
    }

    #[test]
    fn no_usable_os_addresses_fall_back_to_enabled_loopbacks() {
        let interfaces = vec![v4_interface("down", Some(3), if_addrs::IfOperStatus::Down)];
        let result = map_interfaces(interfaces, true, true, if_addrs::Interface::is_oper_up);
        assert_eq!(result.len(), 2);
        assert!(
            result
                .iter()
                .all(|interface| interface.address.is_loopback())
        );
    }
}
