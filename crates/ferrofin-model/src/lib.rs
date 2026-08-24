//! DTOs and enums for Ferrofin — the Rust port of Jellyfin's `MediaBrowser.Model`.
//!
//! The shared data universe (`BaseItemDto`, `UserDto`, `SystemInfo`,
//! `PlaybackInfoResponse`, all enums, and the DLNA profile/`StreamBuilder`
//! logic). Pure data + serde/utoipa derives — most-referenced crate.
//!
//! Ported wave-by-wave in dependency order; modules mirror the C# namespaces.

pub mod activity;
pub mod api_client;
pub mod branding;
pub mod channels;
pub mod collections;
pub mod configuration;
pub mod cryptography;
pub mod data;
pub mod devices;
pub mod dlna;
pub mod drawing;
pub mod dto;
pub mod entities;
pub mod entities_media;
pub mod environment_dtos;
pub mod extensions;
pub mod globalization;
pub mod io;
pub mod json;
pub mod library;
pub mod live_tv;
pub mod lyrics;
pub mod media_info;
pub mod media_segments;
pub mod net;
pub mod notifications;
pub mod playlists;
pub mod plugins;
pub mod providers;
pub mod querying;
pub mod quick_connect;
pub mod search;
pub mod secret;
pub mod security;
pub mod session;
pub mod subtitles;
pub mod sync_play;
pub mod system;
pub mod system_info_dtos;
pub mod tasks;
pub mod updates;
pub mod users;
