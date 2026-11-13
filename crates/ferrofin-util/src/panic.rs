//! Reading a caught panic's payload.
//!
//! Where upstream's `catch (Exception ex)` records `ex.Message`, Ferrofin
//! catches the panic (`catch_unwind`, or a `JoinError` that `is_panic`) and
//! records its message through [`message`], and where it panicked through
//! [`take_location`].

use std::any::Any;
use std::cell::Cell;

/// What [`message`] returns for a payload that is not a string.
pub const NO_MESSAGE: &str = "no message";

/// A caught panic's message: the text a `panic!` with a literal or a format
/// string carries, or [`NO_MESSAGE`] for any other payload
/// (`std::panic::panic_any` with a non-string value).
#[must_use]
pub fn message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or(NO_MESSAGE)
}

thread_local! {
    /// Where the latest panic on this thread happened, as the panic hook saw it.
    static LOCATION: Cell<Option<String>> = const { Cell::new(None) };
}

/// Notes where a panic happened (`file:line:column`), for [`take_location`]
/// on the thread that catches it. The server's panic hook calls it for every
/// panic, before the unwind reaches the catch.
pub fn record_location(info: &std::panic::PanicHookInfo<'_>) {
    // A panic inside the hook aborts: a thread tearing down records nothing.
    let _ = LOCATION.try_with(|cell| cell.set(info.location().map(ToString::to_string)));
}

/// Where the panic just caught on this thread happened, when the panic hook
/// recorded it ([`record_location`]); `None` without that hook.
///
/// Every panic that unwinds to a catch ran the hook on the catching thread
/// first, except one re-raised with `std::panic::resume_unwind`, which runs no
/// hook: for that one this returns an earlier panic's location on this thread,
/// or `None`. Nothing re-raised reaches a scheduled task's catch today (the
/// scan's folder workers re-raise inside the scan's own spawned task).
#[must_use]
pub fn take_location() -> Option<String> {
    LOCATION.take()
}

#[cfg(test)]
mod tests {
    use super::{NO_MESSAGE, message, record_location, take_location};

    fn caught(panic: impl FnOnce() + std::panic::UnwindSafe) -> Box<dyn std::any::Any + Send> {
        std::panic::catch_unwind(panic).expect_err("the closure panics")
    }

    #[test]
    fn a_literal_panic_yields_its_text() {
        assert_eq!(message(caught(|| panic!("kaboom")).as_ref()), "kaboom");
    }

    #[test]
    fn a_formatted_panic_yields_the_formatted_text() {
        let code = 7;
        assert_eq!(
            message(caught(move || panic!("failed with {code}")).as_ref()),
            "failed with 7"
        );
    }

    /// The hook's location is taken once, by the catch on the panicking thread.
    #[test]
    fn the_recorded_location_is_taken_once() {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(record_location));
        let _ = caught(|| panic!("kaboom"));
        std::panic::set_hook(previous);
        let at = take_location();
        assert!(
            at.as_deref().is_some_and(|at| at.starts_with(file!())),
            "{at:?}"
        );
        assert_eq!(take_location(), None);
    }

    #[test]
    fn a_non_string_payload_yields_the_placeholder() {
        assert_eq!(
            message(caught(|| std::panic::panic_any(42_u32)).as_ref()),
            NO_MESSAGE
        );
    }
}
