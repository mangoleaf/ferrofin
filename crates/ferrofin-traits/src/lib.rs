//! Service/manager **traits** for Ferrofin — the DI seam.
//!
//! Port of the *interfaces* in Jellyfin's `MediaBrowser.Controller`. This crate
//! is trait-centric: request handlers depend on these `#[async_trait]` traits,
//! and concrete implementations (in `ferrofin-core`, Wave 6) satisfy them. Nothing
//! here depends on `ferrofin-core` — that arrow points the other way, which breaks
//! the C# impl↔API reference cycle Cargo forbids.
//!
//! **Not ported:** the C# `BaseItem`/`Folder`/`Video` OOP hierarchy (a service
//! layer in inheritance disguise). Trait signatures instead use `ferrofin-db`
//! entities (persistence), `ferrofin-model` DTOs (presentation), and
//! [`uuid::Uuid`] (identity). Marker/mixin interfaces (`IHas*`) are dropped;
//! deferred subsystems (Live TV, SyncPlay, plugins, channels, lyrics) get one
//! minimal stub trait each.
//!
//! **Coverage:** this is a definitions crate (trait bodies live in `ferrofin-core`),
//! so it is exempt from the 80% line-coverage gate; the `options` module (real
//! logic) carries its own tests. Filled by the Wave 4 PortJob.

pub mod activity;
pub mod chapters;
pub mod collections;
pub mod configuration;
pub mod devices;
pub mod drawing;
pub mod dto;
pub mod error;
pub mod events;
pub mod filesystem;
pub mod library;
pub mod localization;
pub mod media_encoding;
pub mod media_segments;
pub mod merge_versions;
pub mod metrics;
pub mod net;
pub mod options;
pub mod persistence;
pub mod plugins;
pub mod providers;
pub mod security;
pub mod session;
pub mod session_bus;
pub mod stubs;
pub mod subtitles;
pub mod system;
pub mod tasks;
pub mod trickplay;
pub mod tv;

pub use error::ServiceError;
