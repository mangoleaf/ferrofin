//! `FromRow` structs for the security-area tables — `ApiKeys`, `Devices`,
//! and `DeviceOptions`.
//!
//! Each struct mirrors one table one-to-one: field names and order match the
//! columns in `migrations/0001_initial.sql` (which reflects the EF model
//! snapshot). Column-to-Rust type mapping follows the conventions in the
//! [module docs](crate::entities):
//! - `INTEGER` surrogate keys → [`i64`],
//! - `TEXT` `Guid` columns → [`String`] (the hyphenated form as stored; the
//!   conversion layer parses these into `Uuid`),
//! - `TEXT` `DateTime` columns → [`DateTime<Utc>`](chrono::DateTime),
//! - `INTEGER` booleans → [`bool`].

use chrono::{DateTime, Utc};

/// A row of the `ApiKeys` table — a long-lived server API key.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct ApiKeyEntity {
    /// Surrogate primary key (`Id`).
    pub id: i64,
    /// The generated access token (`AccessToken`, unique).
    pub access_token: String,
    /// When the key was created (`DateCreated`).
    pub date_created: DateTime<Utc>,
    /// When the key was last used (`DateLastActivity`).
    pub date_last_activity: DateTime<Utc>,
    /// A human-readable name for the key (`Name`).
    pub name: String,
}

/// A row of the `Devices` table — a client device paired to a user.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct DeviceEntity {
    /// Surrogate primary key (`Id`).
    pub id: i64,
    /// The device's session access token (`AccessToken`).
    pub access_token: String,
    /// The client application name (`AppName`).
    pub app_name: String,
    /// The client application version (`AppVersion`).
    pub app_version: String,
    /// When the device was first registered (`DateCreated`).
    pub date_created: DateTime<Utc>,
    /// When the device was last used (`DateLastActivity`).
    pub date_last_activity: DateTime<Utc>,
    /// When the device record was last modified (`DateModified`).
    pub date_modified: DateTime<Utc>,
    /// The client-reported device identifier (`DeviceId`).
    pub device_id: String,
    /// The client-reported device name (`DeviceName`).
    pub device_name: String,
    /// Whether the device is currently active (`IsActive`).
    pub is_active: bool,
    /// The owning user's `Guid`, hyphenated (`UserId`, FK → `Users`).
    pub user_id: String,
}

/// A row of the `DeviceOptions` table — per-device customisation.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct DeviceOptionsEntity {
    /// Surrogate primary key (`Id`).
    pub id: i64,
    /// A user-assigned custom name for the device (`CustomName`), if any.
    pub custom_name: Option<String>,
    /// The client-reported device identifier (`DeviceId`, unique).
    pub device_id: String,
}
