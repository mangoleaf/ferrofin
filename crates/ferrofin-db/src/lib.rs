//! SQLite persistence for Ferrofin — port of Jellyfin's `Jellyfin.Database`.
//!
//! **sqlx + SQLite** (runtime queries — no compile-time `query!` macros, so no
//! `DATABASE_URL` is needed to build). Provides the active-schema entity row
//! structs (`FromRow`), a single `m0001_initial` migration reflecting the EF
//! model snapshot's head schema, and a `Database` connection handle. The
//! crates that read the entities build the `ferrofin-model` DTOs from them.
//!
//! The commented-out richer per-type schema in the C# `JellyfinDbContext`
//! (Movie/Episode/Metadata tables) is NOT active upstream and is not ported.

pub mod database;
pub mod entities;
pub mod enums;
pub mod error;
pub mod export;
pub mod normalized_usernames;
mod placeholders;
pub mod presentation_key;
pub mod row_cache;
mod sqlite_casing;
pub mod sqlite_random;
pub mod store;
mod wal_checkpoints;

pub use database::{
    AdoptionState, DEFAULT_STATEMENT_CACHE_CAPACITY, Database, ImageParentRow,
    JELLYFIN_GENERATIONS, JellyfinGeneration, MigrationHistory, init_sqlite,
    inspect_migration_history, schema_head,
};

/// How many ids one unpadded `IN (…)` query binds at a time, and the most
/// parameters one multi-row insert binds: stays far below SQLite's conservative
/// 999-host-variable floor (`SQLITE_MAX_VARIABLE_NUMBER` on old builds). The
/// request-path reads pad their lists instead and chunk by [`PADDED_LIST_MAX`].
pub const BATCH_BIND_CHUNK: usize = 500;

/// The placeholder item row the initial migration seeds (C# `PlaceholderId`),
/// in its stored GUID form; every real query excludes it.
pub const PLACEHOLDER_ITEM_ID: &str = "00000000-0000-0000-0000-000000000001";
pub use error::{DbError, Result};
pub use placeholders::{
    PADDED_LIST_MAX, numbered_placeholders, padded_in_chunks, padded_in_list, power_of_two_chunks,
    values_rows,
};
