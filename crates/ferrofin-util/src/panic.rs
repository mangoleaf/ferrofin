//! Reading a caught panic's payload.
//!
//! Where upstream's `catch (Exception ex)` records `ex.Message`, Ferrofin
//! catches the panic (`catch_unwind`, or a `JoinError` that `is_panic`) and
//! records its message through [`message`].

use std::any::Any;

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

#[cfg(test)]
mod tests {
    use super::{NO_MESSAGE, message};

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

    #[test]
    fn a_non_string_payload_yields_the_placeholder() {
        assert_eq!(
            message(caught(|| std::panic::panic_any(42_u32)).as_ref()),
            NO_MESSAGE
        );
    }
}
