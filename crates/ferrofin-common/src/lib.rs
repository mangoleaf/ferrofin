//! Common infrastructure for Ferrofin — port of Jellyfin's `MediaBrowser.Common`.
//!
//! Modules mirror the C# namespaces:
//!
//! - [`providers`] — provider-id parsers (`MediaBrowser.Common.Providers`).
//! - [`path_attributes`] — the `[name=value]` attributes in file and folder
//!   names (`PathExtensions.GetAttributeValue`).
//! - [`extensions`] — string helpers (`MediaBrowser.Common.Extensions`).
//! - [`configuration`] — config-store value types + factory/validation traits
//!   (`MediaBrowser.Common.Configuration`).
//! - [`cryptography`] — password-hash parse/format + a real PBKDF2 provider
//!   (PBKDF2-HMAC-SHA512/SHA1, matching Jellyfin's `Rfc2898DeriveBytes.Pbkdf2`
//!   so existing Jellyfin hashes verify) (`MediaBrowser.Model.Cryptography` +
//!   the server crypto impl).
//! - [`error`] — the crypto-path error enum.
//!
//! Runtime machinery (DI app-host, plugin loader, host networking, ASP.NET
//! glue) is intentionally *not* ported — see the port charter.

pub mod configuration;
pub mod cryptography;
pub mod error;
pub mod extensions;
pub mod path_attributes;
pub mod providers;

pub use error::{CryptoError, Result};
