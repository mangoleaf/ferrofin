//! Linux: a `NETLINK_ROUTE` socket, read as .NET 10's
//! `SystemNative_CreateNetworkChangeListenerSocket` / `SystemNative_ReadEvents`
//! (`src/native/libs/System.Native/pal_networkchange.c`) read it.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use tokio::io::unix::AsyncFd;

use super::{NetworkChange, NetworkChangeSource};

/// .NET's multicast groups: links, IPv4 addresses, and IPv4 and IPv6 routes
/// (IPv6 addresses arrive as the routes they add).
#[allow(clippy::cast_sign_loss)] // Positive bit flags.
const GROUPS: u32 = (libc::RTMGRP_LINK
    | libc::RTMGRP_IPV4_IFADDR
    | libc::RTMGRP_IPV4_ROUTE
    | libc::RTMGRP_IPV6_ROUTE) as u32;

/// .NET's receive buffer (`char buffer[4096]`).
const BUFFER_LEN: usize = 4096;

/// `sizeof(struct nlmsghdr)`, which `NLMSG_HDRLEN` aligns to itself.
const NLMSG_HDRLEN: usize = 16;

/// Network changes read from a netlink socket.
pub struct NetlinkChanges {
    socket: AsyncFd<OwnedFd>,
    pending: VecDeque<NetworkChange>,
    buffer: Box<[u8]>,
}

impl NetlinkChanges {
    /// Opens and binds the socket.
    pub(super) fn open() -> io::Result<Self> {
        // SAFETY: socket(2) with constant arguments; the result is checked.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                libc::NETLINK_ROUTE,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just created and nothing else owns it.
        let socket = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: `sockaddr_nl` is plain data, for which all zeroes is valid.
        let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // AF_NETLINK is 16.
        {
            address.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        }
        address.nl_groups = GROUPS;
        #[allow(clippy::cast_possible_truncation)] // 12 bytes.
        let length = std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t;
        // SAFETY: `address` is a valid `sockaddr_nl` of `length` bytes.
        let bound = unsafe {
            libc::bind(
                socket.as_raw_fd(),
                (&raw const address).cast::<libc::sockaddr>(),
                length,
            )
        };
        if bound != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            socket: register(socket)?,
            pending: VecDeque::new(),
            buffer: vec![0; BUFFER_LEN].into_boxed_slice(),
        })
    }
}

/// Hands `socket` to the Tokio reactor.
fn register(socket: OwnedFd) -> io::Result<AsyncFd<OwnedFd>> {
    // SAFETY: an `OwnedFd` keeps its descriptor open, and returns that same
    // descriptor from `as_raw_fd`, until it is dropped with the `AsyncFd`.
    Ok(unsafe { AsyncFd::register(socket) }?)
}

impl NetworkChangeSource for NetlinkChanges {
    async fn recv(&mut self) -> io::Result<NetworkChange> {
        let Self {
            socket,
            pending,
            buffer,
        } = self;
        loop {
            if let Some(change) = pending.pop_front() {
                return Ok(change);
            }
            let mut ready = socket.readable().await?;
            let Ok(read) = ready.try_io(|socket| {
                // SAFETY: `buffer` is valid for writes of its whole length.
                let read = unsafe {
                    libc::recv(
                        socket.as_raw_fd(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                        0,
                    )
                };
                usize::try_from(read).map_err(|_| io::Error::last_os_error())
            }) else {
                continue;
            };
            match read {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "the netlink socket closed",
                    ));
                }
                Ok(length) => read_events(&buffer[..length.min(buffer.len())], pending),
                // The kernel dropped messages the socket had no room for: a
                // change happened, and nothing says which, so refresh. .NET
                // stops listening here.
                Err(error) if error.raw_os_error() == Some(libc::ENOBUFS) => {
                    tracing::warn!(%error, "network change messages were dropped; treating it as a change");
                    pending.push_back(NetworkChange::Availability);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }
}

/// The changes in one datagram, as `SystemNative_ReadEvents` reads them:
/// address messages are address changes; an `AF_UNSPEC` link message and a
/// main-table route message are availability changes, and a route message
/// ends the datagram; `NLMSG_DONE` and `NLMSG_ERROR` end it too.
fn read_events(mut datagram: &[u8], changes: &mut VecDeque<NetworkChange>) {
    // `NLMSG_OK` / `NLMSG_NEXT`.
    while datagram.len() >= NLMSG_HDRLEN {
        let length = u32::from_ne_bytes([datagram[0], datagram[1], datagram[2], datagram[3]]);
        let Ok(length) = usize::try_from(length) else {
            return;
        };
        if length < NLMSG_HDRLEN || length > datagram.len() {
            return;
        }
        let kind = u16::from_ne_bytes([datagram[4], datagram[5]]);
        let payload = &datagram[NLMSG_HDRLEN..length];
        match i32::from(kind) {
            libc::NLMSG_DONE | libc::NLMSG_ERROR => return,
            _ => {}
        }
        match kind {
            libc::RTM_NEWADDR | libc::RTM_DELADDR => changes.push_back(NetworkChange::Address),
            // `ifinfomsg.ifi_family`.
            libc::RTM_NEWLINK
                if payload
                    .first()
                    .is_some_and(|family| i32::from(*family) == libc::AF_UNSPEC) =>
            {
                changes.push_back(NetworkChange::Availability);
            }
            // `rtmsg.rtm_table`.
            libc::RTM_NEWROUTE | libc::RTM_DELROUTE
                if payload.get(4) == Some(&libc::RT_TABLE_MAIN) =>
            {
                changes.push_back(NetworkChange::Availability);
                return;
            }
            _ => {}
        }
        // `NLMSG_ALIGN(nlmsg_len)`.
        let next = length.next_multiple_of(4).min(datagram.len());
        datagram = &datagram[next..];
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::{NLMSG_HDRLEN, NetworkChange, read_events};

    /// One netlink message of `kind` whose payload starts with `payload`.
    fn message(kind: u16, payload: &[u8]) -> Vec<u8> {
        let length = NLMSG_HDRLEN + payload.len();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&u32::try_from(length).expect("small").to_ne_bytes());
        bytes.extend_from_slice(&kind.to_ne_bytes());
        bytes.extend_from_slice(&[0; 10]);
        bytes.extend_from_slice(payload);
        bytes.resize(length.next_multiple_of(4), 0);
        bytes
    }

    fn events(datagram: &[u8]) -> Vec<NetworkChange> {
        let mut changes = VecDeque::new();
        read_events(datagram, &mut changes);
        changes.into()
    }

    /// `rtmsg` with `rtm_table` = `table`.
    fn route(table: u8) -> [u8; 12] {
        [10, 64, 0, 0, table, 3, 0, 1, 0, 0, 0, 0]
    }

    /// `ifinfomsg` with `ifi_family` = `family`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn link(family: i32) -> [u8; 16] {
        let mut info = [0; 16];
        info[0] = family as u8;
        info
    }

    #[test]
    fn address_messages_are_address_changes() {
        let datagram = [
            message(libc::RTM_NEWADDR, &[2, 24, 0, 0, 3, 0, 0, 0]),
            message(libc::RTM_DELADDR, &[10, 64, 0, 0, 3, 0, 0, 0]),
        ]
        .concat();
        assert_eq!(
            events(&datagram),
            [NetworkChange::Address, NetworkChange::Address]
        );
    }

    #[test]
    fn only_an_unspecified_family_link_message_is_an_availability_change() {
        let datagram = [
            message(libc::RTM_NEWLINK, &link(libc::AF_UNSPEC)),
            message(libc::RTM_NEWLINK, &link(libc::AF_INET6)),
            message(libc::RTM_DELLINK, &link(libc::AF_UNSPEC)),
        ]
        .concat();
        assert_eq!(events(&datagram), [NetworkChange::Availability]);
    }

    #[test]
    fn a_main_table_route_message_is_an_availability_change_that_ends_the_datagram() {
        let datagram = [
            message(libc::RTM_NEWROUTE, &route(255)),
            message(libc::RTM_DELROUTE, &route(libc::RT_TABLE_MAIN)),
            message(libc::RTM_NEWADDR, &[2, 24, 0, 0, 3, 0, 0, 0]),
        ]
        .concat();
        assert_eq!(events(&datagram), [NetworkChange::Availability]);
    }

    #[test]
    fn done_and_error_messages_end_the_datagram() {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        for end in [libc::NLMSG_DONE as u16, libc::NLMSG_ERROR as u16] {
            let datagram = [
                message(libc::RTM_NEWADDR, &[2, 24, 0, 0, 3, 0, 0, 0]),
                message(end, &[0; 4]),
                message(libc::RTM_DELADDR, &[2, 24, 0, 0, 3, 0, 0, 0]),
            ]
            .concat();
            assert_eq!(events(&datagram), [NetworkChange::Address], "{end}");
        }
    }

    #[test]
    fn a_truncated_or_malformed_message_ends_the_datagram() {
        let whole = message(libc::RTM_NEWADDR, &[2, 24, 0, 0, 3, 0, 0, 0]);
        assert_eq!(events(&whole[..whole.len() - 1]), []);
        assert_eq!(events(&whole[..NLMSG_HDRLEN - 1]), []);
        let mut short = whole;
        short[..4].copy_from_slice(&4_u32.to_ne_bytes());
        assert_eq!(events(&short), []);
        let unknown = message(libc::RTM_NEWNEIGH, &[0; 12]);
        assert_eq!(events(&unknown), []);
    }

    /// The real socket opens and binds without privileges, and reports no
    /// change while nothing changes.
    #[tokio::test]
    async fn the_socket_opens_without_privileges() {
        use super::super::NetworkChangeSource as _;
        let mut changes = super::NetlinkChanges::open().expect("netlink socket");
        let waited =
            tokio::time::timeout(std::time::Duration::from_millis(50), changes.recv()).await;
        if let Ok(change) = waited {
            change.expect("a change, not a socket error");
        }
    }
}
