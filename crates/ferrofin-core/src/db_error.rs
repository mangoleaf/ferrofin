//! Shared mapping from a `sqlx` error to a [`ServiceError`].
//!
//! Every repository/service in this crate runs `ferrofin-db` queries and needs to
//! surface `sqlx::Error` as the trait-level [`ServiceError`]. Rather than each
//! module defining its own private `db_err`, the single conversion lives here
//! (per `RULES_CODE_REUSE`): route through `ferrofin-db`'s `DbError` so the error
//! text and variant are consistent across the crate.

use ferrofin_model::entities::MediaStreamType;
use ferrofin_traits::error::ServiceError;

/// Wraps a `sqlx` error as a [`ServiceError`] via the `ferrofin-db` error type.
#[must_use]
pub fn db_err(err: sqlx::Error) -> ServiceError {
    ServiceError::from(ferrofin_db::DbError::from(err))
}

/// The stored `MediaStreamInfos.StreamType` discriminant for a wire
/// [`MediaStreamType`].
///
/// The `ferrofin-db` `MediaStreamTypeEntity` shares the model enum's discriminant
/// order (`Audio = 0`, `Video = 1`, …), so this is the single place the mapping
/// is spelled out for the raw-SQL stream queries in this crate.
#[must_use]
pub fn media_stream_type_disc(stream_type: MediaStreamType) -> i32 {
    stream_type.json_value()
}

/// The wire [`MediaStreamType`] for a stored `MediaStreamInfos.StreamType`
/// discriminant — the inverse of [`media_stream_type_disc`].
///
/// Unnamed C# enum values are retained so reading a row cannot change its type.
#[must_use]
pub fn media_stream_type_from_disc(disc: i32) -> MediaStreamType {
    MediaStreamType::from_json_value(disc)
}

/// The stored `StreamType` discriminant for a [`MediaStreamType`] — the inverse of
/// [`media_stream_type_from_disc`], used when persisting a probed stream.
#[must_use]
pub fn media_stream_type_to_disc(stream_type: MediaStreamType) -> i32 {
    stream_type.json_value()
}
