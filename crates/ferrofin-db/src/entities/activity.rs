//! `FromRow` struct for the `ActivityLogs` table — server activity entries.
//!
//! Mirrors the `ActivityLogs` table one-to-one (see `migrations/0001_initial.sql`),
//! following the [module conventions](crate::entities): the `TEXT` `Guid`
//! columns stay [`String`] (the conversion layer parses them into `Uuid`), the
//! `TEXT` `DateTime` column becomes [`DateTime<Utc>`](chrono::DateTime), and the
//! `LogSeverity` enum discriminant is kept as an [`i32`].

use chrono::{DateTime, Utc};

/// A row of the `ActivityLogs` table — one recorded server activity entry.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct ActivityLogEntity {
    /// Surrogate primary key (`Id`).
    pub id: i64,
    /// When the entry was created (`DateCreated`).
    pub date_created: DateTime<Utc>,
    /// The associated item id, hyphen-free `Guid` string if present (`ItemId`).
    pub item_id: Option<String>,
    /// The log severity discriminant (`LogSeverity`, → `LogLevel`).
    pub log_severity: i32,
    /// The entry name (`Name`).
    pub name: String,
    /// The long-form overview (`Overview`), if any.
    pub overview: Option<String>,
    /// The optimistic-concurrency token (`RowVersion`).
    pub row_version: i64,
    /// The short-form overview (`ShortOverview`), if any.
    pub short_overview: Option<String>,
    /// The entry type key (`Type`).
    #[sqlx(rename = "Type")]
    pub type_: String,
    /// The associated user's `Guid`, hyphenated (`UserId`).
    pub user_id: String,
}
