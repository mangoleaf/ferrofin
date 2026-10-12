//! Port of `MediaBrowser.Model.Library`.
//!
//! [`PlayAccess`] is the canonical home for the enum that [`crate::dto`]
//! previously stubbed as a forward reference. `UserViewQuery` is deferred: it
//! carries a server-side `User` entity that is not part of this port unit.

use serde::Serialize;
use utoipa::ToSchema;

/// The play access of an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[repr(i32)]
pub enum PlayAccess {
    /// The item can be played.
    #[default]
    Full = 0,
    /// The item cannot be played.
    None = 1,
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    PlayAccess, None, {
        Full => ("Full", 0),
        None => ("None", 1),
    }
}
