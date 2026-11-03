//! macOS: a `PF_ROUTE` routing socket, read as .NET 10's BSD
//! `SystemNative_ReadEvents` (`pal_networkchange.c`, `HAVE_RT_MSGHDR`) reads
//! it. The reader is plain byte parsing, so it is tested on every platform.

use std::collections::VecDeque;

use super::NetworkChange;

/// `RTM_ADD` (`<net/route.h>`).
const RTM_ADD: u8 = 0x1;
/// `RTM_DELETE`.
const RTM_DELETE: u8 = 0x2;
/// `RTM_REDIRECT`.
const RTM_REDIRECT: u8 = 0x6;
/// `RTM_NEWADDR`.
const RTM_NEWADDR: u8 = 0xc;
/// `RTM_DELADDR`.
const RTM_DELADDR: u8 = 0xd;
/// `RTM_VERSION` on Darwin.
const RTM_VERSION: u8 = 5;

/// The leading fields every routing message shares (`rtm_msglen`,
/// `rtm_version`, `rtm_type`).
const COMMON_HEADER_LEN: usize = 4;

/// The changes in one read, as the BSD `SystemNative_ReadEvents` reads them:
/// a message of another version ends the read; address messages are address
/// changes; a route added, deleted or redirected is an availability change and
/// ends the read. A zero message length also ends it (the C loop would not
/// advance). Each message needs only its common header here, where .NET's
/// BSD loop wants a whole `rt_msghdr`: on macOS an address message
/// (`ifa_msghdr` and its addresses) can be shorter than that.
pub(super) fn read_events(mut read: &[u8], changes: &mut VecDeque<NetworkChange>) {
    while read.len() >= COMMON_HEADER_LEN {
        // `rtm_msglen`, `rtm_version`, `rtm_type`.
        let length = usize::from(u16::from_ne_bytes([read[0], read[1]]));
        if read[2] != RTM_VERSION {
            return;
        }
        match read[3] {
            RTM_NEWADDR | RTM_DELADDR => changes.push_back(NetworkChange::Address),
            RTM_ADD | RTM_DELETE | RTM_REDIRECT => {
                changes.push_back(NetworkChange::Availability);
                return;
            }
            _ => {}
        }
        if length == 0 || length > read.len() {
            return;
        }
        read = &read[length..];
    }
}

#[cfg(target_os = "macos")]
pub use socket::RouteSocketChanges;

#[cfg(target_os = "macos")]
mod socket {
    use std::collections::VecDeque;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use tokio::io::unix::AsyncFd;

    use super::super::{NetworkChange, NetworkChangeSource};

    /// .NET's read buffer (`char buffer[4096]`).
    const BUFFER_LEN: usize = 4096;

    /// Network changes read from a routing socket.
    pub struct RouteSocketChanges {
        socket: AsyncFd<OwnedFd>,
        pending: VecDeque<NetworkChange>,
        buffer: Box<[u8]>,
    }

    impl RouteSocketChanges {
        /// Opens the socket, non-blocking.
        pub(in super::super) fn open() -> io::Result<Self> {
            // SAFETY: socket(2) with constant arguments; the result is checked.
            let fd = unsafe { libc::socket(libc::PF_ROUTE, libc::SOCK_RAW, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` was just created and nothing else owns it.
            let socket = unsafe { OwnedFd::from_raw_fd(fd) };
            let fd = socket.as_raw_fd();
            // SAFETY: fcntl(2) on a descriptor this function owns.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: as above.
            if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: as above.
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
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

    impl NetworkChangeSource for RouteSocketChanges {
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
                        libc::read(socket.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len())
                    };
                    usize::try_from(read).map_err(|_| io::Error::last_os_error())
                }) else {
                    continue;
                };
                match read {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            "the routing socket closed",
                        ));
                    }
                    Ok(length) => super::read_events(&buffer[..length.min(buffer.len())], pending),
                    // The kernel dropped messages the socket had no room
                    // for: a change happened, so refresh.
                    Err(error) if error.raw_os_error() == Some(libc::ENOBUFS) => {
                        tracing::warn!(
                            %error,
                            "network change messages were dropped; treating it as a change"
                        );
                        pending.push_back(NetworkChange::Availability);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::{
        NetworkChange, RTM_ADD, RTM_DELADDR, RTM_DELETE, RTM_NEWADDR, RTM_REDIRECT, RTM_VERSION,
        read_events,
    };

    /// Darwin's `sizeof(struct rt_msghdr)`.
    const HEADER: usize = 92;
    /// Darwin's `sizeof(struct ifa_msghdr)`.
    const ADDRESS_HEADER: usize = 20;

    /// A routing message of `kind`, `length` bytes long.
    fn message_of(kind: u8, version: u8, length: usize) -> Vec<u8> {
        let mut bytes = vec![0; length];
        bytes[..2].copy_from_slice(&u16::try_from(length).expect("small").to_ne_bytes());
        bytes[2] = version;
        bytes[3] = kind;
        bytes
    }

    /// An `rt_msghdr` of `kind`, `extra` bytes of addresses after it.
    fn message(kind: u8, version: u8, extra: usize) -> Vec<u8> {
        message_of(kind, version, HEADER + extra)
    }

    fn events(read: &[u8]) -> Vec<NetworkChange> {
        let mut changes = VecDeque::new();
        read_events(read, &mut changes);
        changes.into()
    }

    #[test]
    fn address_messages_are_address_changes() {
        let read = [
            message(RTM_NEWADDR, RTM_VERSION, 16),
            message(0xe, RTM_VERSION, 0), // RTM_IFINFO
            message(RTM_DELADDR, RTM_VERSION, 32),
        ]
        .concat();
        assert_eq!(
            events(&read),
            [NetworkChange::Address, NetworkChange::Address]
        );
    }

    /// An IPv4 address message (`ifa_msghdr` and three 16-byte addresses) is
    /// shorter than an `rt_msghdr`, and still read.
    #[test]
    fn a_short_address_message_is_read() {
        let read = [
            message_of(RTM_NEWADDR, RTM_VERSION, ADDRESS_HEADER + 48),
            message_of(RTM_DELADDR, RTM_VERSION, ADDRESS_HEADER + 48),
        ]
        .concat();
        assert_eq!(
            events(&read),
            [NetworkChange::Address, NetworkChange::Address]
        );
    }

    #[test]
    fn a_route_change_is_an_availability_change_that_ends_the_read() {
        for kind in [RTM_ADD, RTM_DELETE, RTM_REDIRECT] {
            let read = [
                message(kind, RTM_VERSION, 8),
                message(RTM_NEWADDR, RTM_VERSION, 0),
            ]
            .concat();
            assert_eq!(events(&read), [NetworkChange::Availability], "{kind}");
        }
    }

    #[test]
    fn another_version_a_partial_header_or_a_zero_length_ends_the_read() {
        let read = [
            message(RTM_NEWADDR, RTM_VERSION + 1, 0),
            message(RTM_NEWADDR, RTM_VERSION, 0),
        ]
        .concat();
        assert_eq!(events(&read), []);
        let whole = message(RTM_NEWADDR, RTM_VERSION, 0);
        assert_eq!(events(&whole[..3]), []);
        let mut zero = [
            message(RTM_NEWADDR, RTM_VERSION, 0),
            message(RTM_DELADDR, RTM_VERSION, 0),
        ]
        .concat();
        zero[..2].copy_from_slice(&0_u16.to_ne_bytes());
        assert_eq!(events(&zero), [NetworkChange::Address]);
    }
}
