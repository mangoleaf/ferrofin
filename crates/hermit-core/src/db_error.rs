//! Shared mapping from a `sqlx` error to a [`ServiceError`].
//!
//! Every repository/service in this crate runs `hermit-db` queries and needs to
//! surface `sqlx::Error` as the trait-level [`ServiceError`]. Rather than each
//! module defining its own private `db_err`, the single conversion lives here
//! (per `RULES_CODE_REUSE`): route through `hermit-db`'s `DbError` so the error
//! text and variant are consistent across the crate.

use hermit_model::entities::MediaStreamType;
use hermit_traits::error::ServiceError;

/// Wraps a `sqlx` error as a [`ServiceError`] via the `hermit-db` error type.
#[must_use]
pub fn db_err(err: sqlx::Error) -> ServiceError {
    ServiceError::from(hermit_db::DbError::from(err))
}

/// The stored `MediaStreamInfos.StreamType` discriminant for a wire
/// [`MediaStreamType`].
///
/// The `hermit-db` `MediaStreamTypeEntity` shares the model enum's discriminant
/// order (`Audio = 0`, `Video = 1`, …), so this is the single place the mapping
/// is spelled out for the raw-SQL stream queries in this crate.
#[must_use]
pub fn media_stream_type_disc(stream_type: MediaStreamType) -> i32 {
    match stream_type {
        MediaStreamType::Audio => 0,
        MediaStreamType::Video => 1,
        MediaStreamType::Subtitle => 2,
        MediaStreamType::EmbeddedImage => 3,
        MediaStreamType::Data => 4,
        MediaStreamType::Lyric => 5,
    }
}

/// The wire [`MediaStreamType`] for a stored `MediaStreamInfos.StreamType`
/// discriminant — the inverse of [`media_stream_type_disc`].
///
/// An unknown discriminant maps to [`MediaStreamType::Data`] (the neutral
/// "other" bucket), since a stored row should never carry an out-of-range value
/// and rejecting the whole read would be worse than a benign default.
#[must_use]
pub fn media_stream_type_from_disc(disc: i32) -> MediaStreamType {
    match disc {
        1 => MediaStreamType::Video,
        2 => MediaStreamType::Subtitle,
        3 => MediaStreamType::EmbeddedImage,
        5 => MediaStreamType::Lyric,
        0 => MediaStreamType::Audio,
        _ => MediaStreamType::Data,
    }
}
