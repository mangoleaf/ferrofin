//! SQLite persistence for Ferrofin — port of Jellyfin's `Jellyfin.Database`.
//!
//! **sqlx + SQLite** (runtime queries — no compile-time `query!` macros, so no
//! `DATABASE_URL` is needed to build). Provides the active-schema entity row
//! structs (`FromRow`), a single `m0001_initial` migration reflecting the EF
//! model snapshot's head schema, a `Database` connection handle, and
//! `From<entity>` conversions into `ferrofin-model` DTOs.
//!
//! The commented-out richer per-type schema in the C# `JellyfinDbContext`
//! (Movie/Episode/Metadata tables) is NOT active upstream and is not ported.

pub mod conversions;
pub mod database;
pub mod entities;
pub mod enums;
pub mod error;
pub mod sqlite_random;
pub mod store;

pub use database::Database;

/// How many ids one `IN (…)` query binds at a time: stays far below SQLite's
/// conservative 999-host-variable floor (`SQLITE_MAX_VARIABLE_NUMBER` on old
/// builds), so every batched lookup chunks its ids by this.
pub const BATCH_BIND_CHUNK: usize = 500;
pub use error::{DbError, Result};
