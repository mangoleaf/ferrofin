//! SQLite persistence for Hermit — port of Jellyfin's `Jellyfin.Database`.
//!
//! **sqlx + SQLite** (runtime queries — no compile-time `query!` macros, so no
//! `DATABASE_URL` is needed to build). Provides the active-schema entity row
//! structs (`FromRow`), a single `m0001_initial` migration reflecting the EF
//! model snapshot's head schema, a `Database` connection handle, and
//! `From<entity>` conversions into `hermit-model` DTOs.
//!
//! The commented-out richer per-type schema in the C# `JellyfinDbContext`
//! (Movie/Episode/Metadata tables) is NOT active upstream and is not ported.
//! Filled by the Wave 3 PortJob. See `brain/PLAN_HERMIT_PORT.md`.

pub mod conversions;
pub mod database;
pub mod entities;
pub mod enums;
pub mod error;
pub mod store;

pub use database::Database;
pub use error::{DbError, Result};
