//! The HTTP listeners: which addresses the server binds, and how.
//!
//! Jellyfin binds `NetworkManager.GetAllBindInterfaces(false)` — the dashboard's
//! `LocalNetworkAddresses` when any are set, otherwise every interface of the
//! enabled address families (a dual-stack `::` when both IPv4 and IPv6 are on,
//! `0.0.0.0` for IPv4 only, the host's IPv6 addresses for IPv6 only). An
//! address the operator pins at startup (`--bind`, `FERROFIN_BIND_ADDR`,
//! `config.toml`) is an explicit deployment choice and stays the one listener.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::Context as _;

/// What `axum::serve` accepts: a TCP listener with Nagle's algorithm off on
/// every accepted connection.
pub type Listener = axum::serve::TapIo<tokio::net::TcpListener, fn(&mut tokio::net::TcpStream)>;

/// An address the network manager says to bind, with its interface index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindInterface {
    /// The interface's address.
    pub address: IpAddr,
    /// The interface index, which scopes a link-local IPv6 address (the
    /// `ScopeId` a .NET `IPAddress` carries); `0` when unknown.
    pub index: u32,
}

/// The socket addresses to listen on.
///
/// `explicit` is the operator's startup choice, when there is one; otherwise
/// the network manager's bind interfaces (`GetAllBindInterfaces`) decide.
#[must_use]
pub fn plan_addresses(
    explicit: Option<IpAddr>,
    port: u16,
    bind_interfaces: &[BindInterface],
) -> Vec<SocketAddr> {
    match explicit {
        Some(address) => vec![SocketAddr::new(address, port)],
        None => bind_interfaces
            .iter()
            .map(|interface| match interface.address {
                // A link-local IPv6 address cannot be bound without the
                // interface it lives on.
                IpAddr::V6(v6) if v6.is_unicast_link_local() => {
                    SocketAddr::V6(std::net::SocketAddrV6::new(v6, port, 0, interface.index))
                }
                address => SocketAddr::new(address, port),
            })
            .collect(),
    }
}

/// Binds every address. An unspecified IPv6 address (`::`) is bound dual-stack;
/// where the host cannot do IPv6 at all it falls back to IPv4 (`0.0.0.0`) with
/// a warning, as Jellyfin drops IPv6 when `Socket.OSSupportsIPv6` is false.
///
/// # Errors
///
/// An address that cannot be bound for any other reason.
pub fn bind_all(addresses: &[SocketAddr]) -> anyhow::Result<Vec<Listener>> {
    let mut listeners = Vec::with_capacity(addresses.len());
    for address in addresses {
        match bind(*address) {
            Ok(listener) => listeners.push(listener),
            Err(error)
                if address.ip() == IpAddr::V6(Ipv6Addr::UNSPECIFIED) && ipv6_missing(&error) =>
            {
                tracing::warn!(%error, "IPv6 is not available on this host; listening on IPv4 only");
                let fallback = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), address.port());
                listeners
                    .push(bind(fallback).with_context(|| format!("failed to bind {fallback}"))?);
            }
            Err(error) => return Err(error).with_context(|| format!("failed to bind {address}")),
        }
    }
    Ok(listeners)
}

/// Whether `error` says the host has no IPv6 (an unsupported address family or
/// an address that does not exist on it).
fn ipv6_missing(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
    ) || error.raw_os_error() == Some(97) // EAFNOSUPPORT on Linux
}

/// Binds one address with the options `tokio::net::TcpListener::bind` would
/// set (address reuse on unix), and — for `::` — dual-stack explicitly rather
/// than whatever the platform default is (Windows and the BSDs default to
/// IPv6-only, which would silently drop IPv4 clients).
///
/// # Errors
///
/// The socket error.
pub fn bind(address: SocketAddr) -> std::io::Result<Listener> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(address),
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    if address.is_ipv6() && address.ip().is_unspecified() {
        socket.set_only_v6(false)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&address.into())?;
    socket.listen(1024)?;
    let listener = tokio::net::TcpListener::from_std(socket.into())?;
    Ok(axum::serve::ListenerExt::tap_io(
        listener,
        crate::disable_nagle as fn(&mut tokio::net::TcpStream),
    ))
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    use axum::serve::Listener as _;

    use super::{BindInterface, bind, bind_all, plan_addresses};

    #[test]
    fn an_operators_bind_wins_otherwise_the_network_settings_decide() {
        let interface = |address: IpAddr, index| BindInterface { address, index };
        let any6 = interface(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0);
        let lan = interface("192.168.1.5".parse().unwrap(), 2);
        let loopback_address = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let loopback = interface(loopback_address, 1);
        // Pinned at startup: exactly that address, whatever the dashboard says.
        assert_eq!(
            plan_addresses(Some(loopback_address), 8096, &[any6]),
            ["127.0.0.1:8096".parse::<SocketAddr>().unwrap()]
        );
        // Not pinned: each bind interface, on the port.
        assert_eq!(
            plan_addresses(None, 8096, &[lan, loopback]),
            [
                "192.168.1.5:8096".parse::<SocketAddr>().unwrap(),
                "127.0.0.1:8096".parse::<SocketAddr>().unwrap(),
            ]
        );
        assert_eq!(
            plan_addresses(None, 80, &[any6]),
            ["[::]:80".parse::<SocketAddr>().unwrap()]
        );
        assert!(plan_addresses(None, 8096, &[]).is_empty());
        // A link-local IPv6 address binds on its interface.
        let link_local = interface("fe80::1".parse().unwrap(), 3);
        let planned = plan_addresses(None, 8096, &[link_local]);
        let SocketAddr::V6(v6) = planned[0] else {
            panic!("an IPv6 socket address");
        };
        assert_eq!((v6.port(), v6.scope_id()), (8096, 3));
        assert_eq!(v6.to_string(), "[fe80::1%3]:8096");
    }

    #[tokio::test]
    async fn binding_every_planned_address_yields_one_listener_each() {
        let listeners = bind_all(&[
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        ])
        .expect("bind");
        assert_eq!(listeners.len(), 2);
        let ports: Vec<u16> = listeners
            .iter()
            .map(|listener| listener.local_addr().expect("addr").port())
            .collect();
        assert_ne!(ports[0], ports[1], "two ephemeral binds are distinct");
        assert!(bind_all(&[]).expect("none").is_empty());
    }

    /// `::` listens dual-stack: an IPv4 client reaches it through the
    /// v4-mapped address. Skipped where the host has no IPv6.
    #[tokio::test]
    async fn the_unspecified_ipv6_address_accepts_ipv4_clients() {
        let Ok(listener) = bind("[::]:0".parse().unwrap()) else {
            eprintln!("skipped: no IPv6 on this host");
            return;
        };
        let port = listener.local_addr().expect("addr").port();
        let client = tokio::net::TcpStream::connect(("127.0.0.1", port)).await;
        assert!(client.is_ok(), "{client:?}");
    }

    #[tokio::test]
    async fn an_address_in_use_is_an_error_naming_it() {
        let first = bind("127.0.0.1:0".parse().unwrap()).expect("bind");
        let taken = first.local_addr().expect("addr");
        let Err(error) = bind_all(&[taken]) else {
            panic!("a second bind of {taken} must fail");
        };
        assert!(error.to_string().contains(&taken.to_string()), "{error}");
    }
}
