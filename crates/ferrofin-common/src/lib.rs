//! Common infrastructure for Ferrofin — port of Jellyfin's `MediaBrowser.Common`.
//!
//! Modules mirror the C# namespaces:
//!
//! - [`crc32`] — zlib/IEEE CRC-32 (`MediaBrowser.Common`).
//! - [`providers`] — provider-id parsers (`MediaBrowser.Common.Providers`).
//! - [`extensions`] — string helpers (`MediaBrowser.Common.Extensions`).
//! - [`configuration`] — config-store value types + factory/validation traits
//!   (`MediaBrowser.Common.Configuration`).
//! - [`app_paths`] — the application-paths abstraction.
//! - [`cryptography`] — password-hash parse/format + a real PBKDF2 provider
//!   (PBKDF2-HMAC-SHA512/SHA1, matching Jellyfin's `Rfc2898DeriveBytes.Pbkdf2`
//!   so existing Jellyfin hashes verify) (`MediaBrowser.Model.Cryptography` +
//!   the server crypto impl).
//! - [`exceptions`] — the plain exception types.
//! - [`error`] — the crypto-path error enum.
//!
//! Runtime machinery (DI app-host, plugin loader, host networking, ASP.NET
//! glue) is intentionally *not* ported — see the port charter.

pub mod app_paths;
pub mod configuration;
pub mod crc32;
pub mod cryptography;
pub mod error;
pub mod exceptions;
pub mod extensions;
pub mod providers;

pub use error::{CryptoError, Result};
