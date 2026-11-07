//! Windows: `NotifyUnicastIpAddressChange` for every address family. .NET 10
//! waits on Winsock's `SIO_ADDRESS_LIST_CHANGE` for IPv4 and IPv6
//! (`NetworkAddressChange.Windows.cs`); both report an address added to or
//! removed from any interface, which .NET raises as `NetworkAddressChanged`.
//! A change to an address's parameters (its lifetimes, its duplicate-address
//! state) is not one, so only additions and deletions count.

use std::ffi::c_void;
use std::io;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use windows_sys::Win32::Foundation::{HANDLE, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    CancelMibChangeNotify2, MIB_NOTIFICATION_TYPE, MIB_UNICASTIPADDRESS_ROW, MibAddInstance,
    MibDeleteInstance, NotifyUnicastIpAddressChange,
};
use windows_sys::Win32::Networking::WinSock::AF_UNSPEC;

use super::{NetworkChange, NetworkChangeSource};

/// Network changes delivered by an IP Helper notification.
pub struct AddressChangeNotifications {
    changes: UnboundedReceiver<NetworkChange>,
    _registration: Registration,
}

/// The registered notification and the sender its callback writes to,
/// released together.
struct Registration {
    handle: HANDLE,
    sender: *mut UnboundedSender<NetworkChange>,
}

// SAFETY: the handle is only passed to `CancelMibChangeNotify2`, from any
// thread, and the sender it points at is `Send`.
unsafe impl Send for Registration {}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: `handle` came from `NotifyUnicastIpAddressChange`. Once
        // `CancelMibChangeNotify2` returns no callback is running or will
        // run, so the sender can be freed.
        unsafe {
            CancelMibChangeNotify2(self.handle);
            drop(Box::from_raw(self.sender));
        }
    }
}

impl AddressChangeNotifications {
    /// Registers the notification.
    pub(super) fn open() -> io::Result<Self> {
        let (sender, changes) = unbounded_channel();
        let sender = Box::into_raw(Box::new(sender));
        let mut handle: HANDLE = std::ptr::null_mut();
        // SAFETY: the callback reads `sender` only until the registration is
        // cancelled, which `Registration::drop` does before freeing it.
        let status = unsafe {
            NotifyUnicastIpAddressChange(
                AF_UNSPEC,
                Some(on_change),
                sender.cast_const().cast::<c_void>(),
                false,
                &raw mut handle,
            )
        };
        if status != NO_ERROR {
            // SAFETY: nothing was registered, so nothing else reads `sender`.
            drop(unsafe { Box::from_raw(sender) });
            #[allow(clippy::cast_possible_wrap)] // A Win32 error code.
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(Self {
            changes,
            _registration: Registration { handle, sender },
        })
    }
}

/// The notification callback: an address added or removed.
unsafe extern "system" fn on_change(
    context: *const c_void,
    _row: *const MIB_UNICASTIPADDRESS_ROW,
    kind: MIB_NOTIFICATION_TYPE,
) {
    if kind != MibAddInstance && kind != MibDeleteInstance {
        return;
    }
    // SAFETY: `context` is the sender `Registration` keeps alive until no
    // callback can run.
    let sender = unsafe { &*context.cast::<UnboundedSender<NetworkChange>>() };
    // The receiver is gone only while the registration is being cancelled.
    let _ = sender.send(NetworkChange::Address);
}

impl NetworkChangeSource for AddressChangeNotifications {
    async fn recv(&mut self) -> io::Result<NetworkChange> {
        self.changes.recv().await.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the address notification stopped",
            )
        })
    }
}
