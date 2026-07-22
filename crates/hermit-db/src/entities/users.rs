//! `FromRow` structs for the user-area tables — `Users` and its dependents
//! (`AccessSchedules`, `Permissions`, `Preferences`, `ImageInfos`,
//! `ActivityLogs`).
//!
//! Each struct mirrors one table one-to-one: field names and order match the
//! columns in `migrations/0001_initial.sql`. Enum-valued columns are stored as
//! `INTEGER` discriminants and are kept as [`i32`] here; the conversion layer
//! maps them onto the [`crate::enums`] / `hermit-model` enum types. `RowVersion`
//! optimistic-concurrency tokens are stored as `INTEGER` and kept as [`i64`].
//! `Guid` columns are `TEXT` and kept as [`String`] (the hyphenated stored
//! form; the conversion layer parses them into `Uuid`).

use chrono::{DateTime, Utc};

/// A row of the `Users` table — a Hermit user account and its preferences.
///
/// `SubtitleMode` (`SubtitlePlaybackMode`) and `SyncPlayAccess`
/// (`SyncPlayUserAccessType`) are stored as `INTEGER` discriminants and kept
/// here as [`i32`].
// A 1:1 mirror of the 34-column `Users` table; its many boolean toggles are
// intrinsic to the schema, not a refactorable design.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct UserEntity {
    /// The user's `Guid` primary key, hyphenated (`Id`).
    pub id: String,
    /// Preferred audio language (`AudioLanguagePreference`), if set.
    pub audio_language_preference: Option<String>,
    /// The authentication provider id (`AuthenticationProviderId`).
    pub authentication_provider_id: String,
    /// The Chromecast receiver app id (`CastReceiverId`), if set.
    pub cast_receiver_id: Option<String>,
    /// Whether the collections view is shown (`DisplayCollectionsView`).
    pub display_collections_view: bool,
    /// Whether missing episodes are displayed (`DisplayMissingEpisodes`).
    pub display_missing_episodes: bool,
    /// Whether auto-login is enabled (`EnableAutoLogin`).
    pub enable_auto_login: bool,
    /// Whether a local password is enabled (`EnableLocalPassword`).
    pub enable_local_password: bool,
    /// Whether next-episode auto-play is enabled (`EnableNextEpisodeAutoPlay`).
    pub enable_next_episode_auto_play: bool,
    /// Whether the user may access preference settings
    /// (`EnableUserPreferenceAccess`).
    pub enable_user_preference_access: bool,
    /// Whether played items are hidden from Latest (`HidePlayedInLatest`).
    pub hide_played_in_latest: bool,
    /// The legacy internal integer id (`InternalId`).
    pub internal_id: i64,
    /// The number of consecutive failed logins (`InvalidLoginAttemptCount`).
    pub invalid_login_attempt_count: i64,
    /// When the user was last active (`LastActivityDate`), if ever.
    pub last_activity_date: Option<DateTime<Utc>>,
    /// When the user last logged in (`LastLoginDate`), if ever.
    pub last_login_date: Option<DateTime<Utc>>,
    /// The lockout threshold (`LoginAttemptsBeforeLockout`), if set.
    pub login_attempts_before_lockout: Option<i64>,
    /// The maximum number of concurrent sessions (`MaxActiveSessions`).
    pub max_active_sessions: i64,
    /// The maximum permitted parental rating score (`MaxParentalRatingScore`).
    pub max_parental_rating_score: Option<i64>,
    /// The maximum permitted parental rating sub-score
    /// (`MaxParentalRatingSubScore`).
    pub max_parental_rating_sub_score: Option<i64>,
    /// Whether the user must change their password (`MustUpdatePassword`).
    pub must_update_password: bool,
    /// The normalized (uppercased) username (`NormalizedUsername`, unique).
    pub normalized_username: String,
    /// The hashed password (`Password`), if one is set.
    pub password: Option<String>,
    /// The password-reset provider id (`PasswordResetProviderId`).
    pub password_reset_provider_id: String,
    /// Whether the default audio track is played (`PlayDefaultAudioTrack`).
    pub play_default_audio_track: bool,
    /// Whether audio-track selections are remembered (`RememberAudioSelections`).
    pub remember_audio_selections: bool,
    /// Whether subtitle selections are remembered
    /// (`RememberSubtitleSelections`).
    pub remember_subtitle_selections: bool,
    /// The remote client bitrate limit (`RemoteClientBitrateLimit`), if set.
    pub remote_client_bitrate_limit: Option<i64>,
    /// The optimistic-concurrency token (`RowVersion`).
    pub row_version: i64,
    /// Preferred subtitle language (`SubtitleLanguagePreference`), if set.
    pub subtitle_language_preference: Option<String>,
    /// The subtitle playback mode discriminant (`SubtitleMode`).
    pub subtitle_mode: i32,
    /// The `SyncPlay` access-level discriminant (`SyncPlayAccess`).
    pub sync_play_access: i32,
    /// The username as displayed (`Username`, unique).
    pub username: String,
}

/// A row of the `AccessSchedules` table — a time window during which a user
/// may access the server.
///
/// `DayOfWeek` (`DynamicDayOfWeek`) is stored as an `INTEGER` discriminant and
/// kept here as [`i32`].
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct AccessScheduleEntity {
    /// Surrogate primary key (`Id`).
    pub id: i64,
    /// The day-of-week discriminant the window applies to (`DayOfWeek`).
    pub day_of_week: i32,
    /// The window's end hour, as a fractional 24-hour value (`EndHour`).
    pub end_hour: f64,
    /// The window's start hour, as a fractional 24-hour value (`StartHour`).
    pub start_hour: f64,
    /// The owning user's `Guid`, hyphenated (`UserId`, FK → `Users`).
    pub user_id: String,
}

/// A row of the `Permissions` table — a single boolean permission for a user.
///
/// `Kind` (`PermissionKind`) is stored as an `INTEGER` discriminant and kept
/// here as [`i32`].
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct PermissionEntity {
    /// Surrogate primary key (`Id`).
    pub id: i64,
    /// The permission-kind discriminant (`Kind`).
    pub kind: i32,
    /// The permission's associated `Guid`, hyphenated
    /// (`Permission_Permissions_Guid`), if any.
    #[sqlx(rename = "Permission_Permissions_Guid")]
    pub permission_guid: Option<String>,
    /// The optimistic-concurrency token (`RowVersion`).
    pub row_version: i64,
    /// The owning user's `Guid`, hyphenated (`UserId`, FK → `Users`), if any.
    pub user_id: Option<String>,
    /// The boolean permission value (`Value`).
    pub value: bool,
}

/// A row of the `Preferences` table — a list-valued preference for a user.
///
/// `Kind` (`PreferenceKind`) is stored as an `INTEGER` discriminant and kept
/// here as [`i32`]. `Value` holds the delimited list as stored.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct PreferenceEntity {
    /// Surrogate primary key (`Id`).
    pub id: i64,
    /// The preference-kind discriminant (`Kind`).
    pub kind: i32,
    /// The preference's associated `Guid`, hyphenated
    /// (`Preference_Preferences_Guid`), if any.
    #[sqlx(rename = "Preference_Preferences_Guid")]
    pub preference_guid: Option<String>,
    /// The optimistic-concurrency token (`RowVersion`).
    pub row_version: i64,
    /// The owning user's `Guid`, hyphenated (`UserId`, FK → `Users`), if any.
    pub user_id: Option<String>,
    /// The stored preference value (`Value`).
    pub value: String,
}

/// A row of the `ImageInfos` table — a user's profile image.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct ImageInfoEntity {
    /// Surrogate primary key (`Id`).
    pub id: i64,
    /// When the image file was last modified (`LastModified`).
    pub last_modified: DateTime<Utc>,
    /// The image's file path (`Path`).
    pub path: String,
    /// The owning user's `Guid`, hyphenated (`UserId`, FK → `Users`), if any.
    pub user_id: Option<String>,
}

/// A row of the `ActivityLogs` table — a recorded activity/audit entry.
///
/// `LogSeverity` (`LogLevel`) is stored as an `INTEGER` discriminant and kept
/// here as [`i32`]. `ItemId` holds a `Guid` when present.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct ActivityLogEntity {
    /// Surrogate primary key (`Id`).
    pub id: i64,
    /// When the entry was created (`DateCreated`).
    pub date_created: DateTime<Utc>,
    /// The related item's `Guid`, hyphenated (`ItemId`), if any.
    pub item_id: Option<String>,
    /// The log-severity discriminant (`LogSeverity`).
    pub log_severity: i32,
    /// A short title for the entry (`Name`).
    pub name: String,
    /// A longer description (`Overview`), if any.
    pub overview: Option<String>,
    /// The optimistic-concurrency token (`RowVersion`).
    pub row_version: i64,
    /// A brief description (`ShortOverview`), if any.
    pub short_overview: Option<String>,
    /// The activity type key (`Type`).
    #[sqlx(rename = "Type")]
    pub type_: String,
    /// The user's `Guid`, hyphenated, that the activity relates to (`UserId`).
    pub user_id: String,
}
