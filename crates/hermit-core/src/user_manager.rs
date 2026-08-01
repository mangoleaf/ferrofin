//! [`HermitUserManager`] — the concrete [`UserManager`] over `hermit-db`.
//!
//! Port of `Jellyfin.Server.Implementations.Users.UserManager`. User accounts
//! persist in the `Users` table with their sibling `Permissions` / `Preferences`
//! / `AccessSchedules` rows; the C# `User` OOP behavior (permission/preference
//! access, default seeding, parental schedule) lives in
//! [`crate::user_entity_ext`] and is reused here rather than reimplemented.
//!
//! Password hashing is delegated to the injected [`AuthenticationManager`]
//! providers (the built-in [`DefaultAuthenticationProvider`] plus the fallback
//! [`InvalidAuthProvider`]); this manager never touches crypto directly. The C#
//! `_userLock` per-user async mutex is dropped — SQLite serializes writes and
//! the ported operations are individually atomic — and the `.NET` event manager
//! (`UserCreated`/`UserUpdated`/…) is out of scope (event wiring lands
//! separately).
//!
//! Faithful port simplifications, each flagged:
//! - `authenticate_user` cannot consult a `NetworkManager` (no such trait exists
//!   yet), so the C# "remote access disabled and caller not on the LAN" check is
//!   omitted; the disabled-account, lockout, and parental-schedule checks are
//!   preserved.
//! - `get_user_dto` assembles the full [`UserDto`](hermit_model::dto::UserDto)
//!   (policy + configuration) from the `Users` row and its
//!   `Permissions`/`Preferences`/`AccessSchedules`. The C# profile-image cache
//!   tag is not yet ported, so `PrimaryImageTag` is left unset.
//! - `update_policy` persists the flat `Users` columns and reflects the two
//!   load-bearing permission flags into `Permissions`; the broader
//!   folder/channel/schedule policy mapping is a flagged follow-up.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use hermit_db::Database;
use hermit_db::entities::users::UserEntity;
use hermit_db::enums::{PermissionKind, PreferenceKind};
use hermit_model::configuration::{SubtitlePlaybackMode, UserConfiguration};
use hermit_model::data::UnratedItem;
use hermit_model::dto::{NameIdPair, UserDto};
use hermit_model::users::{AccessSchedule, DynamicDayOfWeek, SyncPlayUserAccessType, UserPolicy};
use sqlx::Sqlite;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::library::UserManager;
use hermit_traits::security::AuthenticationManager;

use crate::auth_providers::{
    DEFAULT_AUTH_PROVIDER_ID, DefaultAuthenticationProvider, INVALID_AUTH_PROVIDER_ID,
    InvalidAuthProvider,
};
use crate::db_error::db_err;
use crate::user_entity_ext::{
    has_permission, is_parental_schedule_allowed, seed_defaults, set_permission, set_permission_tx,
    set_preference,
};

/// The C# type name of the default password-reset provider, stored on
/// `User.PasswordResetProviderId` for a freshly created user. The reset-provider
/// subsystem itself is deferred; only the id is needed to match upstream rows.
pub const DEFAULT_PASSWORD_RESET_PROVIDER_ID: &str =
    "Jellyfin.Server.Implementations.Users.DefaultPasswordResetProvider";

/// The default username assigned during first-run bootstrap when the host user
/// name is unusable (C# `"MyJellyfinUser"`).
const DEFAULT_BOOTSTRAP_USERNAME: &str = "MyJellyfinUser";

/// The concrete user manager.
///
/// Holds the registered [`AuthenticationManager`] providers by shared reference
/// so the composition root can supply LDAP/other backends alongside the
/// built-in default; the built-in [`DefaultAuthenticationProvider`] is kept
/// separately for its password-hash formatting helper (which is not on the
/// object-safe trait).
#[derive(Clone)]
pub struct HermitUserManager {
    db: Database,
    default_provider: DefaultAuthenticationProvider,
    invalid_provider: InvalidAuthProvider,
    providers: Vec<Arc<dyn AuthenticationManager>>,
    /// This server's stable id, stamped onto every [`UserDto.ServerId`] when the
    /// caller does not supply one. The web client keys its per-server api-client
    /// on it and throws (`getApiClient(null)`) if it is missing. Set by the
    /// composition root via [`with_server_id`](HermitUserManager::with_server_id);
    /// `None` in tests (which then omit `ServerId`, the prior behaviour).
    server_id: Option<String>,
    /// The directory user profile images are written under
    /// (`{dir}/{userId}/profile{ext}`). Set by the composition root; `None`
    /// leaves [`save_profile_image`](hermit_traits::library::UserManager::save_profile_image)
    /// deferred (tests).
    profile_image_dir: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for HermitUserManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitUserManager")
            .field("provider_count", &self.providers.len())
            .finish_non_exhaustive()
    }
}

impl HermitUserManager {
    /// Creates a user manager over the given database with only the built-in
    /// [`DefaultAuthenticationProvider`] registered.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self::with_providers(db, Vec::new())
    }

    /// Creates a user manager with additional pluggable authentication providers
    /// (e.g. LDAP), which are consulted after the built-in default.
    #[must_use]
    pub fn with_providers(db: Database, extra: Vec<Arc<dyn AuthenticationManager>>) -> Self {
        let mut providers: Vec<Arc<dyn AuthenticationManager>> =
            vec![Arc::new(DefaultAuthenticationProvider::new())];
        providers.extend(extra);
        Self {
            db,
            default_provider: DefaultAuthenticationProvider::new(),
            invalid_provider: InvalidAuthProvider::new(),
            providers,
            server_id: None,
            profile_image_dir: None,
        }
    }

    /// Sets the directory user profile images are stored under, enabling
    /// [`save_profile_image`](hermit_traits::library::UserManager::save_profile_image).
    #[must_use]
    pub fn with_profile_image_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.profile_image_dir = Some(dir);
        self
    }

    /// Sets this server's stable id, stamped onto every produced [`UserDto`] whose
    /// caller passes no explicit `server_id`. Called once by the composition root.
    #[must_use]
    pub fn with_server_id(mut self, server_id: impl Into<String>) -> Self {
        self.server_id = Some(server_id.into());
        self
    }

    /// Fetches a user row by id, or `None`.
    async fn fetch_user(&self, id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
        sqlx::query_as::<_, UserEntity>(r#"SELECT * FROM "Users" WHERE "Id" = ?1 LIMIT 1"#)
            .bind(id.to_string())
            .fetch_optional(self.db.pool())
            .await
            .map_err(db_err)
    }

    /// Fetches a user row by id or returns [`ServiceError::NotFound`].
    async fn require_user(&self, id: Uuid) -> Result<UserEntity, ServiceError> {
        self.fetch_user(id)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("user {id}")))
    }

    /// The count of user rows.
    async fn user_count(&self) -> Result<i64, ServiceError> {
        sqlx::query_scalar::<_, i64>(r#"SELECT COUNT(*) FROM "Users""#)
            .fetch_one(self.db.pool())
            .await
            .map_err(db_err)
    }

    /// The count of enabled administrator users (C# admin-deletion guard).
    async fn admin_count(&self) -> Result<i64, ServiceError> {
        sqlx::query_scalar::<_, i64>(
            r#"SELECT COUNT(*) FROM "Users" u
               WHERE EXISTS (
                   SELECT 1 FROM "Permissions" p
                   WHERE p."UserId" = u."Id" AND p."Kind" = ?1 AND p."Value" = 1
               )"#,
        )
        .bind(i32::from(PermissionKind::IsAdministrator))
        .fetch_one(self.db.pool())
        .await
        .map_err(db_err)
    }

    /// The next legacy `InternalId` (C# `max(InternalId) + 1`).
    async fn next_internal_id(&self) -> Result<i64, ServiceError> {
        let max: Option<i64> = sqlx::query_scalar(r#"SELECT MAX("InternalId") FROM "Users""#)
            .fetch_one(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(max.unwrap_or(0) + 1)
    }

    /// Inserts a fully-formed user row plus its default permissions/preferences
    /// inside one transaction (C# `CreateUserInternalAsync` + `Add`), returning
    /// the persisted row. `configure` runs against the open transaction so the
    /// caller can grant bootstrap permissions atomically.
    async fn insert_user<F>(&self, name: &str, configure: F) -> Result<UserEntity, ServiceError>
    where
        F: for<'t> FnOnce(
            &'t mut sqlx::Transaction<'_, Sqlite>,
            &'t str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), ServiceError>> + Send + 't>,
        >,
    {
        let id = Uuid::new_v4();
        let id_str = id.to_string();
        let internal_id = self.next_internal_id().await?;
        let normalized = name.to_uppercase();

        let mut tx = self.db.pool().begin().await.map_err(db_err)?;
        sqlx::query(
            r#"INSERT INTO "Users"
               ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
                "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
                "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
                "HidePlayedInLatest", "CastReceiverId", "InternalId", "InvalidLoginAttemptCount",
                "MaxActiveSessions", "MustUpdatePassword", "NormalizedUsername",
                "PasswordResetProviderId", "PlayDefaultAudioTrack",
                "RememberAudioSelections", "RememberSubtitleSelections",
                "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username")
               VALUES (?1, ?2, 0, 0, 0, 0, 1, 1, 1, 'F007D354', ?3, 0, 0, 0, ?4, ?5, 1, 1, 1, 0, 0, 0, ?6)"#,
        )
        .bind(&id_str)
        .bind(DEFAULT_AUTH_PROVIDER_ID)
        .bind(internal_id)
        .bind(&normalized)
        .bind(DEFAULT_PASSWORD_RESET_PROVIDER_ID)
        .bind(name)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;

        seed_defaults(&mut tx, &id_str).await?;
        configure(&mut tx, &id_str).await?;
        tx.commit().await.map_err(db_err)?;

        self.require_user(id).await
    }
}

/// The stored provider id for a provider `name` (C# `GetType().FullName`). The
/// two built-in providers map to their canonical `Jellyfin.*` type names so a
/// migrated database matches; any other provider is assumed to already report
/// its fully-qualified id as its name.
fn provider_id(name: &str) -> &str {
    match name {
        "Default" => DEFAULT_AUTH_PROVIDER_ID,
        "InvalidOrMissingAuthenticationProvider" => INVALID_AUTH_PROVIDER_ID,
        other => other,
    }
}

/// Validates a username against the C# `ValidUsernameRegex`
/// (`^(?!\s)[\w \-'._@+]+(?<!\s)$`) plus the `.`/`..` exclusions, without a
/// regex engine: allowed characters are word characters (Unicode alphanumerics
/// or `_`), space, and any of `- ' . _ @ +`; it must be non-empty, must not
/// start or end with whitespace, and must not be `.` or `..`.
fn valid_username(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    if name.starts_with(char::is_whitespace) || name.ends_with(char::is_whitespace) {
        return false;
    }
    name.chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | ' ' | '-' | '\'' | '.' | '@' | '+'))
}

/// Validates a username or returns [`ServiceError::InvalidInput`] (C#
/// `ThrowIfInvalidUsername`).
fn require_valid_username(name: &str) -> Result<(), ServiceError> {
    if valid_username(name) {
        Ok(())
    } else {
        Err(ServiceError::invalid_input(
            "Usernames can contain unicode symbols, numbers (0-9), dashes (-), \
             underscores (_), apostrophes ('), and periods (.)",
        ))
    }
}

#[async_trait]
impl UserManager for HermitUserManager {
    async fn get_users(&self) -> Result<Vec<UserEntity>, ServiceError> {
        sqlx::query_as::<_, UserEntity>(r#"SELECT * FROM "Users" ORDER BY "Username""#)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)
    }

    async fn get_user_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
        let ids: Vec<String> =
            sqlx::query_scalar(r#"SELECT "Id" FROM "Users" ORDER BY "Username""#)
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?;
        Ok(ids
            .into_iter()
            .filter_map(|id| Uuid::parse_str(&id).ok())
            .collect())
    }

    async fn initialize(&self) -> Result<(), ServiceError> {
        if self.user_count().await? > 0 {
            return Ok(());
        }

        // C# uses the host user name; here it is not readily available, so fall
        // back to the same default Jellyfin uses when the host name is unusable.
        let default_name = std::env::var("USER")
            .ok()
            .filter(|n| valid_username(n))
            .unwrap_or_else(|| DEFAULT_BOOTSTRAP_USERNAME.to_owned());

        self.insert_user(&default_name, |tx, uid| {
            Box::pin(async move {
                set_permission_tx(tx, uid, PermissionKind::IsAdministrator, true).await?;
                set_permission_tx(tx, uid, PermissionKind::EnableContentDeletion, true).await?;
                set_permission_tx(
                    tx,
                    uid,
                    PermissionKind::EnableRemoteControlOfOtherUsers,
                    true,
                )
                .await?;
                Ok(())
            })
        })
        .await?;
        Ok(())
    }

    async fn get_user_by_id(&self, id: Uuid) -> Result<Option<UserEntity>, ServiceError> {
        self.fetch_user(id).await
    }

    async fn get_first_user(&self) -> Result<Option<UserEntity>, ServiceError> {
        sqlx::query_as::<_, UserEntity>(r#"SELECT * FROM "Users" ORDER BY "InternalId" LIMIT 1"#)
            .fetch_optional(self.db.pool())
            .await
            .map_err(db_err)
    }

    async fn get_user_by_name(&self, name: &str) -> Result<Option<UserEntity>, ServiceError> {
        sqlx::query_as::<_, UserEntity>(
            r#"SELECT * FROM "Users" WHERE "NormalizedUsername" = ?1 LIMIT 1"#,
        )
        .bind(name.to_uppercase())
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)
    }

    async fn rename_user(
        &self,
        user_id: Uuid,
        old_name: &str,
        new_name: &str,
    ) -> Result<(), ServiceError> {
        require_valid_username(new_name)?;
        if old_name == new_name {
            return Err(ServiceError::invalid_input(
                "The new and old names must be different.",
            ));
        }

        let normalized = new_name.to_uppercase();
        let clash: Option<String> = sqlx::query_scalar(
            r#"SELECT "Id" FROM "Users" WHERE "NormalizedUsername" = ?1 AND "Id" != ?2 LIMIT 1"#,
        )
        .bind(&normalized)
        .bind(user_id.to_string())
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;
        if clash.is_some() {
            return Err(ServiceError::invalid_input(format!(
                "A user with the name '{new_name}' already exists."
            )));
        }

        // Ensure the user exists before updating (C# throws ResourceNotFound).
        self.require_user(user_id).await?;
        sqlx::query(
            r#"UPDATE "Users" SET "Username" = ?2, "NormalizedUsername" = ?3 WHERE "Id" = ?1"#,
        )
        .bind(user_id.to_string())
        .bind(new_name)
        .bind(&normalized)
        .execute(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn update_user(&self, user: &UserEntity) -> Result<(), ServiceError> {
        // Ensure the row exists (C# ResourceNotFound). The sibling
        // permission/preference/schedule collections are managed through their
        // own helpers; this persists the flat `Users` columns.
        let id = Uuid::parse_str(&user.id)
            .map_err(|_| ServiceError::invalid_input("user id is not a valid guid"))?;
        self.require_user(id).await?;

        sqlx::query(
            r#"UPDATE "Users" SET
                "AudioLanguagePreference" = ?2, "AuthenticationProviderId" = ?3,
                "CastReceiverId" = ?4, "DisplayCollectionsView" = ?5,
                "DisplayMissingEpisodes" = ?6, "EnableAutoLogin" = ?7,
                "EnableLocalPassword" = ?8, "EnableNextEpisodeAutoPlay" = ?9,
                "EnableUserPreferenceAccess" = ?10, "HidePlayedInLatest" = ?11,
                "InternalId" = ?12, "InvalidLoginAttemptCount" = ?13,
                "LastActivityDate" = ?14, "LastLoginDate" = ?15,
                "LoginAttemptsBeforeLockout" = ?16, "MaxActiveSessions" = ?17,
                "MaxParentalRatingScore" = ?18, "MaxParentalRatingSubScore" = ?19,
                "MustUpdatePassword" = ?20, "NormalizedUsername" = ?21,
                "Password" = ?22, "PasswordResetProviderId" = ?23,
                "PlayDefaultAudioTrack" = ?24, "RememberAudioSelections" = ?25,
                "RememberSubtitleSelections" = ?26, "RemoteClientBitrateLimit" = ?27,
                "SubtitleLanguagePreference" = ?28, "SubtitleMode" = ?29,
                "SyncPlayAccess" = ?30, "Username" = ?31,
                "RowVersion" = "RowVersion" + 1
               WHERE "Id" = ?1"#,
        )
        .bind(&user.id)
        .bind(&user.audio_language_preference)
        .bind(&user.authentication_provider_id)
        .bind(&user.cast_receiver_id)
        .bind(user.display_collections_view)
        .bind(user.display_missing_episodes)
        .bind(user.enable_auto_login)
        .bind(user.enable_local_password)
        .bind(user.enable_next_episode_auto_play)
        .bind(user.enable_user_preference_access)
        .bind(user.hide_played_in_latest)
        .bind(user.internal_id)
        .bind(user.invalid_login_attempt_count)
        .bind(user.last_activity_date)
        .bind(user.last_login_date)
        .bind(user.login_attempts_before_lockout)
        .bind(user.max_active_sessions)
        .bind(user.max_parental_rating_score)
        .bind(user.max_parental_rating_sub_score)
        .bind(user.must_update_password)
        .bind(&user.normalized_username)
        .bind(&user.password)
        .bind(&user.password_reset_provider_id)
        .bind(user.play_default_audio_track)
        .bind(user.remember_audio_selections)
        .bind(user.remember_subtitle_selections)
        .bind(user.remote_client_bitrate_limit)
        .bind(&user.subtitle_language_preference)
        .bind(user.subtitle_mode)
        .bind(user.sync_play_access)
        .bind(&user.username)
        .execute(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn create_user(&self, name: &str) -> Result<UserEntity, ServiceError> {
        require_valid_username(name)?;

        let existing: Option<String> = sqlx::query_scalar(
            r#"SELECT "Id" FROM "Users" WHERE "NormalizedUsername" = ?1 LIMIT 1"#,
        )
        .bind(name.to_uppercase())
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;
        if existing.is_some() {
            return Err(ServiceError::invalid_input(format!(
                "A user with the name '{name}' already exists."
            )));
        }

        self.insert_user(name, |_tx, _uid| Box::pin(async { Ok(()) }))
            .await
    }

    async fn delete_user(&self, user_id: Uuid) -> Result<(), ServiceError> {
        let user = self.require_user(user_id).await?;

        if self.user_count().await? == 1 {
            return Err(ServiceError::invalid_input(format!(
                "The user '{}' cannot be deleted because there must be at least one user \
                 in the system.",
                user.username
            )));
        }

        if has_permission(self.db.pool(), &user.id, PermissionKind::IsAdministrator).await?
            && self.admin_count().await? == 1
        {
            return Err(ServiceError::invalid_input(format!(
                "The user '{}' cannot be deleted because there must be at least one admin \
                 user in the system.",
                user.username
            )));
        }

        // The `Permissions`/`Preferences`/`AccessSchedules`/`ImageInfos` rows are
        // removed with the user; delete them explicitly so the port does not
        // depend on schema cascade configuration.
        for table in [
            "Permissions",
            "Preferences",
            "AccessSchedules",
            "ImageInfos",
        ] {
            let sql = format!(r#"DELETE FROM "{table}" WHERE "UserId" = ?1"#);
            sqlx::query(&sql)
                .bind(user_id.to_string())
                .execute(self.db.pool())
                .await
                .map_err(db_err)?;
        }
        sqlx::query(r#"DELETE FROM "Users" WHERE "Id" = ?1"#)
            .bind(user_id.to_string())
            .execute(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn reset_password(&self, user_id: Uuid) -> Result<(), ServiceError> {
        // C# `ResetPassword` == `ChangePassword(userId, string.Empty)`.
        self.change_password(user_id, "").await
    }

    async fn change_password(&self, user_id: Uuid, new_password: &str) -> Result<(), ServiceError> {
        let user = self.require_user(user_id).await?;

        if new_password.trim().is_empty()
            && has_permission(self.db.pool(), &user.id, PermissionKind::IsAdministrator).await?
        {
            return Err(ServiceError::invalid_input(
                "Admin user passwords must not be empty",
            ));
        }

        // The C# providers mutate the user's `Password` in place; our
        // `AuthenticationManager::change_password` is a no-op by design, so the
        // built-in default's hash formatter produces the stored value and this
        // manager owns the write (see `auth_providers`).
        let hash = self.default_provider.format_password_hash(new_password)?;
        sqlx::query(
            r#"UPDATE "Users" SET "Password" = ?2, "RowVersion" = "RowVersion" + 1
               WHERE "Id" = ?1"#,
        )
        .bind(user_id.to_string())
        .bind(hash)
        .execute(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn authenticate_user(
        &self,
        username: &str,
        password: &str,
        remote_endpoint: &str,
        is_user_session: bool,
    ) -> Result<Option<UserEntity>, ServiceError> {
        let _ = remote_endpoint; // No NetworkManager yet — see module docs.
        if username.trim().is_empty() {
            return Err(ServiceError::invalid_input("username must not be empty"));
        }

        let user = self.get_user_by_name(username).await?;

        // Select the candidate providers (C# `GetAuthenticationProviders(user)`):
        // when the user names a specific provider, only the matching enabled one
        // is tried; if none matches, the always-rejecting fallback stands in.
        let wanted = user
            .as_ref()
            .map(|u| u.authentication_provider_id.as_str())
            .filter(|id| !id.is_empty());
        let mut candidates: Vec<&dyn AuthenticationManager> = Vec::new();
        for provider in &self.providers {
            if !provider.is_enabled().await? {
                continue;
            }
            if wanted.is_none_or(|id| provider_id(provider.name()).eq_ignore_ascii_case(id)) {
                candidates.push(provider.as_ref());
            }
        }
        if candidates.is_empty() {
            candidates.push(&self.invalid_provider);
        }

        // Try each candidate in turn (C# `AuthenticateLocalUser`).
        let mut success = false;
        for provider in candidates {
            match provider
                .authenticate(username, password, user.as_ref())
                .await
            {
                Ok(_) => {
                    success = true;
                    break;
                }
                Err(ServiceError::Unauthorized(_)) => {}
                Err(other) => return Err(other),
            }
        }

        let Some(mut user) = user else {
            // No such user: an auth failure regardless of provider result.
            return Err(ServiceError::unauthorized(
                "Invalid username or password entered.",
            ));
        };

        if !success {
            // Count the failure and lock the account out once the threshold is
            // reached (C# increments then disables).
            let attempts = user.invalid_login_attempt_count + 1;
            if user
                .login_attempts_before_lockout
                .is_some_and(|max| max > 0 && attempts >= max)
            {
                set_permission(self.db.pool(), &user.id, PermissionKind::IsDisabled, true).await?;
            }
            sqlx::query(r#"UPDATE "Users" SET "InvalidLoginAttemptCount" = ?2 WHERE "Id" = ?1"#)
                .bind(&user.id)
                .bind(attempts)
                .execute(self.db.pool())
                .await
                .map_err(db_err)?;
            return Ok(None);
        }

        if has_permission(self.db.pool(), &user.id, PermissionKind::IsDisabled).await? {
            return Err(ServiceError::unauthorized(format!(
                "The {} account is currently disabled. Please consult with your administrator.",
                user.username
            )));
        }

        if !is_parental_schedule_allowed(self.db.pool(), &user.id, chrono::Local::now()).await? {
            return Err(ServiceError::unauthorized(
                "User is not allowed access at this time.",
            ));
        }

        // Success: reset the failure counter and, for a real user session, stamp
        // the login/activity dates.
        let now = chrono::Utc::now();
        if is_user_session {
            sqlx::query(
                r#"UPDATE "Users" SET "InvalidLoginAttemptCount" = 0,
                    "LastActivityDate" = ?2, "LastLoginDate" = ?2 WHERE "Id" = ?1"#,
            )
            .bind(&user.id)
            .bind(now)
            .execute(self.db.pool())
            .await
            .map_err(db_err)?;
            user.invalid_login_attempt_count = 0;
            user.last_activity_date = Some(now);
            user.last_login_date = Some(now);
        } else {
            sqlx::query(r#"UPDATE "Users" SET "InvalidLoginAttemptCount" = 0 WHERE "Id" = ?1"#)
                .bind(&user.id)
                .execute(self.db.pool())
                .await
                .map_err(db_err)?;
            user.invalid_login_attempt_count = 0;
        }

        Ok(Some(user))
    }

    async fn get_authentication_providers(&self) -> Result<Vec<NameIdPair>, ServiceError> {
        let mut pairs = Vec::new();
        for provider in &self.providers {
            if provider.is_enabled().await? {
                pairs.push(NameIdPair {
                    name: Some(provider.name().to_owned()),
                    id: Some(provider_id(provider.name()).to_owned()),
                });
            }
        }
        // Default provider first, then by name (C# ordering).
        pairs.sort_by(|a, b| {
            let rank =
                |p: &NameIdPair| i32::from(p.id.as_deref() != Some(DEFAULT_AUTH_PROVIDER_ID));
            rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name))
        });
        Ok(pairs)
    }

    async fn get_password_reset_providers(&self) -> Result<Vec<NameIdPair>, ServiceError> {
        // Only the built-in default reset provider is ported; the reset flow
        // itself is deferred, but its identity is surfaced so clients can list it.
        Ok(vec![NameIdPair {
            name: Some("Default Password Reset Provider".to_owned()),
            id: Some(DEFAULT_PASSWORD_RESET_PROVIDER_ID.to_owned()),
        }])
    }

    async fn get_user_dto(
        &self,
        user: &UserEntity,
        server_id: Option<String>,
    ) -> Result<UserDto, ServiceError> {
        let id = &user.id;
        let pool = self.db.pool();

        // Bulk-load this user's permissions and preferences up front — two indexed
        // queries — instead of the ~40 per-kind lookups the policy/config build
        // otherwise fans out (an N+1 that convoyed the pool under concurrent load).
        let perms = load_permission_map(pool, id).await?;
        let prefs = load_preference_map(pool, id).await?;

        // Config-only list-valued preferences → the DTO's Guid collections.
        let ordered_views = guid_pref(&prefs, PreferenceKind::OrderedViews);
        let grouped_folders = guid_pref(&prefs, PreferenceKind::GroupedFolders);
        let my_media_excludes = guid_pref(&prefs, PreferenceKind::MyMediaExcludes);
        let latest_items_excludes = guid_pref(&prefs, PreferenceKind::LatestItemExcludes);

        let configuration = UserConfiguration {
            subtitle_mode: subtitle_mode_from_i32(user.subtitle_mode),
            hide_played_in_latest: user.hide_played_in_latest,
            enable_local_password: user.enable_local_password,
            play_default_audio_track: user.play_default_audio_track,
            display_collections_view: user.display_collections_view,
            display_missing_episodes: user.display_missing_episodes,
            audio_language_preference: user.audio_language_preference.clone(),
            remember_audio_selections: user.remember_audio_selections,
            enable_next_episode_auto_play: user.enable_next_episode_auto_play,
            remember_subtitle_selections: user.remember_subtitle_selections,
            subtitle_language_preference: Some(
                user.subtitle_language_preference
                    .clone()
                    .unwrap_or_default(),
            ),
            ordered_views,
            grouped_folders,
            my_media_excludes,
            latest_items_excludes,
            cast_receiver_id: user.cast_receiver_id.clone(),
        };

        let policy = build_user_policy(pool, user, &perms, &prefs).await?;

        Ok(UserDto {
            name: Some(user.username.clone()),
            id: Uuid::parse_str(id).unwrap_or_else(|_| Uuid::nil()),
            // Caller-supplied id wins; otherwise fall back to the manager's own
            // (set by the composition root) so `UserDto.ServerId` is never null.
            server_id: server_id.or_else(|| self.server_id.clone()),
            enable_auto_login: Some(user.enable_auto_login),
            last_login_date: user.last_login_date,
            last_activity_date: user.last_activity_date,
            has_password: Some(user.password.is_some()),
            has_configured_password: Some(user.password.is_some()),
            configuration: Some(configuration),
            policy: Some(policy),
            ..UserDto::default()
        })
    }

    async fn update_configuration(
        &self,
        user_id: Uuid,
        config: &UserConfiguration,
    ) -> Result<(), ServiceError> {
        self.require_user(user_id).await?;
        let id = user_id.to_string();

        sqlx::query(
            r#"UPDATE "Users" SET
                "AudioLanguagePreference" = ?2, "PlayDefaultAudioTrack" = ?3,
                "SubtitleLanguagePreference" = ?4, "DisplayMissingEpisodes" = ?5,
                "SubtitleMode" = ?6, "DisplayCollectionsView" = ?7,
                "EnableLocalPassword" = ?8, "HidePlayedInLatest" = ?9,
                "RememberAudioSelections" = ?10, "RememberSubtitleSelections" = ?11,
                "EnableNextEpisodeAutoPlay" = ?12, "CastReceiverId" = ?13,
                "RowVersion" = "RowVersion" + 1
               WHERE "Id" = ?1"#,
        )
        .bind(&id)
        .bind(&config.audio_language_preference)
        .bind(config.play_default_audio_track)
        .bind(&config.subtitle_language_preference)
        .bind(config.display_missing_episodes)
        .bind(config.subtitle_mode as i32)
        .bind(config.display_collections_view)
        .bind(config.enable_local_password)
        .bind(config.hide_played_in_latest)
        .bind(config.remember_audio_selections)
        .bind(config.remember_subtitle_selections)
        .bind(config.enable_next_episode_auto_play)
        .bind(&config.cast_receiver_id)
        .execute(self.db.pool())
        .await
        .map_err(db_err)?;

        // The list-valued configuration fields are stored as user preferences.
        set_uuid_preference(
            self.db.pool(),
            &id,
            PreferenceKind::GroupedFolders,
            &config.grouped_folders,
        )
        .await?;
        set_uuid_preference(
            self.db.pool(),
            &id,
            PreferenceKind::OrderedViews,
            &config.ordered_views,
        )
        .await?;
        set_uuid_preference(
            self.db.pool(),
            &id,
            PreferenceKind::LatestItemExcludes,
            &config.latest_items_excludes,
        )
        .await?;
        set_uuid_preference(
            self.db.pool(),
            &id,
            PreferenceKind::MyMediaExcludes,
            &config.my_media_excludes,
        )
        .await?;
        Ok(())
    }

    async fn update_policy(&self, user_id: Uuid, policy: &UserPolicy) -> Result<(), ServiceError> {
        // The full `UserPolicy` → `Users`/`Permissions`/`AccessSchedules` mapping
        // is broad; the fields the `Users` table carries directly are persisted
        // here, and the two most load-bearing permission flags are reflected into
        // the `Permissions` table so authentication and access checks see them.
        // Remaining policy fields (blocked media folders, access schedules, the
        // many boolean permissions) are a deferred follow-up, flagged rather than
        // silently dropped.
        self.require_user(user_id).await?;
        let id = user_id.to_string();

        sqlx::query(
            r#"UPDATE "Users" SET
                "MaxActiveSessions" = ?2, "MaxParentalRatingScore" = ?3,
                "MaxParentalRatingSubScore" = ?4, "LoginAttemptsBeforeLockout" = ?5,
                "EnableUserPreferenceAccess" = ?6, "InvalidLoginAttemptCount" = ?7,
                "RemoteClientBitrateLimit" = ?8, "AuthenticationProviderId" = ?9,
                "PasswordResetProviderId" = ?10, "SyncPlayAccess" = ?11,
                "RowVersion" = "RowVersion" + 1
               WHERE "Id" = ?1"#,
        )
        .bind(&id)
        .bind(i64::from(policy.max_active_sessions))
        .bind(policy.max_parental_rating.map(i64::from))
        .bind(policy.max_parental_sub_rating.map(i64::from))
        .bind(i64::from(policy.login_attempts_before_lockout))
        .bind(policy.enable_user_preference_access)
        .bind(i64::from(policy.invalid_login_attempt_count))
        .bind(i64::from(policy.remote_client_bitrate_limit))
        .bind(&policy.authentication_provider_id)
        .bind(&policy.password_reset_provider_id)
        .bind(policy.sync_play_access as i32)
        .execute(self.db.pool())
        .await
        .map_err(db_err)?;

        set_permission(
            self.db.pool(),
            &id,
            PermissionKind::IsAdministrator,
            policy.is_administrator,
        )
        .await?;
        set_permission(
            self.db.pool(),
            &id,
            PermissionKind::IsDisabled,
            policy.is_disabled,
        )
        .await?;
        set_permission(
            self.db.pool(),
            &id,
            PermissionKind::EnableContentDeletion,
            policy.enable_content_deletion,
        )
        .await?;
        set_permission(
            self.db.pool(),
            &id,
            PermissionKind::EnableRemoteControlOfOtherUsers,
            policy.enable_remote_control_of_other_users,
        )
        .await?;
        Ok(())
    }

    async fn save_profile_image(
        &self,
        user: &UserEntity,
        content: &[u8],
        _mime_type: &str,
        extension: &str,
    ) -> Result<(), ServiceError> {
        let Some(base) = &self.profile_image_dir else {
            return Err(ServiceError::backend(
                "save_profile_image requires a profile-image directory",
            ));
        };
        // Write `{base}/{userId}/profile{ext}` (extension carries its dot).
        let dir = base.join(&user.id);
        let ext = extension.trim_start_matches('.');
        let dest = dir.join(format!("profile.{ext}"));
        std::fs::create_dir_all(&dir)
            .map_err(|e| ServiceError::backend(format!("create profile-image dir: {e}")))?;
        std::fs::write(&dest, content)
            .map_err(|e| ServiceError::backend(format!("write profile image: {e}")))?;
        // One image per user: replace any existing row (unique index on UserId).
        let mut tx = self.db.pool().begin().await.map_err(db_err)?;
        sqlx::query(r#"DELETE FROM "ImageInfos" WHERE "UserId" = ?1"#)
            .bind(&user.id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        sqlx::query(
            r#"INSERT INTO "ImageInfos" ("LastModified", "Path", "UserId") VALUES (?1, ?2, ?3)"#,
        )
        .bind(chrono::Utc::now())
        .bind(dest.to_string_lossy().as_ref())
        .bind(&user.id)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn clear_profile_image(&self, user: &UserEntity) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "ImageInfos" WHERE "UserId" = ?1"#)
            .bind(&user.id)
            .execute(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn get_profile_image(
        &self,
        user_id: Uuid,
    ) -> Result<Option<hermit_traits::options::ItemImageInfo>, ServiceError> {
        let row = sqlx::query_as::<_, hermit_db::entities::users::ImageInfoEntity>(
            r#"SELECT * FROM "ImageInfos" WHERE "UserId" = ?1 LIMIT 1"#,
        )
        .bind(user_id.to_string())
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(row.map(|r| hermit_traits::options::ItemImageInfo {
            path: r.path,
            image_type: hermit_model::entities::ImageType::Profile,
            date_modified: r.last_modified,
            width: 0,
            height: 0,
            blur_hash: None,
        }))
    }
}

/// Assembles a user's [`UserPolicy`] from the `Users` row plus its
/// `Permissions`/`Preferences`/`AccessSchedules` (the policy half of C#
/// `GetUserDto`). Extracted from `get_user_dto` so that method stays small.
///
/// The length is inherent: [`UserPolicy`] is a wide, flat 1:1 projection of many
/// permission flags, and a single struct literal reads best (splitting it would
/// only scatter the mapping), so the line-count lint is allowed here.
#[allow(clippy::too_many_lines)]
/// Loads all of a user's permission rows in one query, keyed by `Kind`.
///
/// Replaces the per-kind `has_permission` fan-out when building a `UserDto`: the
/// `(UserId, Kind)` unique index makes this one indexed scan of the handful of
/// rows a user owns.
async fn load_permission_map(
    pool: &sqlx::sqlite::SqlitePool,
    user_id: &str,
) -> Result<HashMap<i32, bool>, ServiceError> {
    let rows: Vec<(i32, bool)> =
        sqlx::query_as(r#"SELECT "Kind", "Value" FROM "Permissions" WHERE "UserId" = ?1"#)
            .bind(user_id)
            .fetch_all(pool)
            .await
            .map_err(db_err)?;
    Ok(rows.into_iter().collect())
}

/// Loads all of a user's preference rows in one query, keyed by `Kind` (the raw
/// stored string; list values are delimiter-joined and split by [`pref`]).
async fn load_preference_map(
    pool: &sqlx::sqlite::SqlitePool,
    user_id: &str,
) -> Result<HashMap<i32, String>, ServiceError> {
    let rows: Vec<(i32, String)> =
        sqlx::query_as(r#"SELECT "Kind", "Value" FROM "Preferences" WHERE "UserId" = ?1"#)
            .bind(user_id)
            .fetch_all(pool)
            .await
            .map_err(db_err)?;
    Ok(rows.into_iter().collect())
}

/// A permission's value from a preloaded map, defaulting to `false` when absent
/// (C# `Permissions.FirstOrDefault(...)?.Value ?? false`).
fn perm(map: &HashMap<i32, bool>, kind: PermissionKind) -> bool {
    map.get(&i32::from(kind)).copied().unwrap_or(false)
}

/// A list-valued preference from a preloaded map, split on the stored `,`
/// delimiter (the same split `get_preference` does per-row).
fn pref(map: &HashMap<i32, String>, kind: PreferenceKind) -> Vec<String> {
    match map.get(&i32::from(kind)) {
        Some(v) if !v.is_empty() => v.split(',').map(str::to_owned).collect(),
        _ => Vec::new(),
    }
}

/// A list-valued preference parsed as [`Uuid`]s, discarding unparseable entries
/// (C# `GetPreferenceValues<Guid>`).
fn guid_pref(map: &HashMap<i32, String>, kind: PreferenceKind) -> Vec<Uuid> {
    pref(map, kind)
        .iter()
        .filter_map(|v| Uuid::parse_str(v).ok())
        .collect()
}

async fn build_user_policy(
    pool: &sqlx::sqlite::SqlitePool,
    user: &UserEntity,
    perms: &HashMap<i32, bool>,
    prefs: &HashMap<i32, String>,
) -> Result<UserPolicy, ServiceError> {
    let id = &user.id;

    let blocked_tags = pref(prefs, PreferenceKind::BlockedTags);
    let allowed_tags = pref(prefs, PreferenceKind::AllowedTags);
    let enabled_channels = guid_pref(prefs, PreferenceKind::EnabledChannels);
    let enabled_devices = pref(prefs, PreferenceKind::EnabledDevices);
    let enabled_folders = guid_pref(prefs, PreferenceKind::EnabledFolders);
    let content_deletion_folders = pref(prefs, PreferenceKind::EnableContentDeletionFromFolders);
    let blocked_channels = guid_pref(prefs, PreferenceKind::BlockedChannels);
    let blocked_media_folders = guid_pref(prefs, PreferenceKind::BlockedMediaFolders);
    let block_unrated_items = pref(prefs, PreferenceKind::BlockUnratedItems)
        .iter()
        .filter_map(|v| parse_unrated_item(v))
        .collect();

    Ok(UserPolicy {
        max_parental_rating: user.max_parental_rating_score.map(cast_i32),
        max_parental_sub_rating: user.max_parental_rating_sub_score.map(cast_i32),
        enable_user_preference_access: user.enable_user_preference_access,
        remote_client_bitrate_limit: user.remote_client_bitrate_limit.map_or(0, cast_i32),
        authentication_provider_id: user.authentication_provider_id.clone(),
        password_reset_provider_id: user.password_reset_provider_id.clone(),
        invalid_login_attempt_count: cast_i32(user.invalid_login_attempt_count),
        login_attempts_before_lockout: user.login_attempts_before_lockout.map_or(-1, cast_i32),
        max_active_sessions: cast_i32(user.max_active_sessions),
        is_administrator: perm(perms, PermissionKind::IsAdministrator),
        is_hidden: perm(perms, PermissionKind::IsHidden),
        is_disabled: perm(perms, PermissionKind::IsDisabled),
        enable_shared_device_control: perm(perms, PermissionKind::EnableSharedDeviceControl),
        enable_remote_access: perm(perms, PermissionKind::EnableRemoteAccess),
        enable_live_tv_management: perm(perms, PermissionKind::EnableLiveTvManagement),
        enable_live_tv_access: perm(perms, PermissionKind::EnableLiveTvAccess),
        enable_media_playback: perm(perms, PermissionKind::EnableMediaPlayback),
        enable_audio_playback_transcoding: perm(
            perms,
            PermissionKind::EnableAudioPlaybackTranscoding,
        ),
        enable_video_playback_transcoding: perm(
            perms,
            PermissionKind::EnableVideoPlaybackTranscoding,
        ),
        enable_content_deletion: perm(perms, PermissionKind::EnableContentDeletion),
        enable_content_downloading: perm(perms, PermissionKind::EnableContentDownloading),
        enable_sync_transcoding: perm(perms, PermissionKind::EnableSyncTranscoding),
        enable_media_conversion: perm(perms, PermissionKind::EnableMediaConversion),
        enable_all_channels: perm(perms, PermissionKind::EnableAllChannels),
        enable_all_devices: perm(perms, PermissionKind::EnableAllDevices),
        enable_all_folders: perm(perms, PermissionKind::EnableAllFolders),
        enable_remote_control_of_other_users: perm(
            perms,
            PermissionKind::EnableRemoteControlOfOtherUsers,
        ),
        enable_playback_remuxing: perm(perms, PermissionKind::EnablePlaybackRemuxing),
        force_remote_source_transcoding: perm(perms, PermissionKind::ForceRemoteSourceTranscoding),
        enable_public_sharing: perm(perms, PermissionKind::EnablePublicSharing),
        enable_collection_management: perm(perms, PermissionKind::EnableCollectionManagement),
        enable_subtitle_management: perm(perms, PermissionKind::EnableSubtitleManagement),
        enable_lyric_management: perm(perms, PermissionKind::EnableLyricManagement),
        access_schedules: access_schedules(pool, id).await?,
        blocked_tags,
        allowed_tags,
        enabled_channels,
        enabled_devices,
        enabled_folders,
        enable_content_deletion_from_folders: content_deletion_folders,
        sync_play_access: sync_play_from_i32(user.sync_play_access),
        blocked_channels: Some(blocked_channels),
        blocked_media_folders: Some(blocked_media_folders),
        block_unrated_items,
    })
}

/// Writes a list of [`Uuid`]s to a list-valued preference, stored as their
/// hyphenated strings (the form C# persists for the folder/view preference
/// lists).
async fn set_uuid_preference(
    pool: &sqlx::sqlite::SqlitePool,
    user_id: &str,
    kind: PreferenceKind,
    values: &[Uuid],
) -> Result<(), ServiceError> {
    let strings: Vec<String> = values.iter().map(Uuid::to_string).collect();
    set_preference(pool, user_id, kind, &strings).await
}

/// Loads a user's access schedules as wire [`AccessSchedule`] rows.
async fn access_schedules(
    pool: &sqlx::sqlite::SqlitePool,
    user_id: &str,
) -> Result<Vec<AccessSchedule>, ServiceError> {
    let rows: Vec<(i64, i32, f64, f64)> = sqlx::query_as(
        r#"SELECT "Id", "DayOfWeek", "StartHour", "EndHour"
           FROM "AccessSchedules" WHERE "UserId" = ?1"#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    let uid = Uuid::parse_str(user_id).unwrap_or_else(|_| Uuid::nil());
    Ok(rows
        .into_iter()
        .map(|(id, day, start, end)| AccessSchedule {
            id: cast_i32(id),
            user_id: uid,
            day_of_week: dynamic_day_from_i32(day),
            start_hour: start,
            end_hour: end,
        })
        .collect())
}

/// Narrows a stored `i64` column to the DTO's `i32`, clamping on overflow.
fn cast_i32(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Maps a stored `SubtitleMode` discriminant to its enum (C# stores the enum's
/// ordinal). Unknown values fall back to the default.
fn subtitle_mode_from_i32(value: i32) -> SubtitlePlaybackMode {
    match value {
        1 => SubtitlePlaybackMode::Always,
        2 => SubtitlePlaybackMode::OnlyForced,
        3 => SubtitlePlaybackMode::None,
        4 => SubtitlePlaybackMode::Smart,
        _ => SubtitlePlaybackMode::Default,
    }
}

/// Maps a stored `SyncPlayAccess` discriminant to its enum.
fn sync_play_from_i32(value: i32) -> SyncPlayUserAccessType {
    match value {
        1 => SyncPlayUserAccessType::JoinGroups,
        2 => SyncPlayUserAccessType::None,
        _ => SyncPlayUserAccessType::CreateAndJoinGroups,
    }
}

/// Maps a stored `DayOfWeek` discriminant to its [`DynamicDayOfWeek`].
fn dynamic_day_from_i32(value: i32) -> DynamicDayOfWeek {
    match value {
        1 => DynamicDayOfWeek::Monday,
        2 => DynamicDayOfWeek::Tuesday,
        3 => DynamicDayOfWeek::Wednesday,
        4 => DynamicDayOfWeek::Thursday,
        5 => DynamicDayOfWeek::Friday,
        6 => DynamicDayOfWeek::Saturday,
        7 => DynamicDayOfWeek::Everyday,
        8 => DynamicDayOfWeek::Weekday,
        9 => DynamicDayOfWeek::Weekend,
        _ => DynamicDayOfWeek::Sunday,
    }
}

/// Parses a stored `BlockUnratedItems` entry into an [`UnratedItem`].
fn parse_unrated_item(value: &str) -> Option<UnratedItem> {
    match value {
        "Movie" => Some(UnratedItem::Movie),
        "Trailer" => Some(UnratedItem::Trailer),
        "Series" => Some(UnratedItem::Series),
        "Music" => Some(UnratedItem::Music),
        "Book" => Some(UnratedItem::Book),
        "LiveTvChannel" => Some(UnratedItem::LiveTvChannel),
        "LiveTvProgram" => Some(UnratedItem::LiveTvProgram),
        "ChannelContent" => Some(UnratedItem::ChannelContent),
        "Other" => Some(UnratedItem::Other),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_db;

    #[test]
    fn valid_username_accepts_and_rejects() {
        assert!(valid_username("alice"));
        assert!(valid_username("a-b_c.d@e+f 1"));
        assert!(!valid_username(""));
        assert!(!valid_username("."));
        assert!(!valid_username(".."));
        assert!(!valid_username(" leading"));
        assert!(!valid_username("trailing "));
        assert!(!valid_username("has\ttab"));
        assert!(!valid_username("bad/slash"));
    }

    #[tokio::test]
    async fn initialize_bootstraps_one_admin() {
        let db = test_db().await;
        let mgr = HermitUserManager::new(db.clone());
        mgr.initialize().await.expect("initialize");

        let users = mgr.get_users().await.expect("users");
        assert_eq!(users.len(), 1);
        assert!(
            has_permission(db.pool(), &users[0].id, PermissionKind::IsAdministrator)
                .await
                .expect("perm")
        );

        // Idempotent: a second call is a no-op.
        mgr.initialize().await.expect("initialize again");
        assert_eq!(mgr.get_users().await.expect("users").len(), 1);
    }

    #[tokio::test]
    async fn get_user_dto_projects_policy_and_config() {
        let db = test_db().await;
        let mgr = HermitUserManager::new(db.clone());
        let user = mgr.create_user("dave").await.expect("create");
        let id = Uuid::parse_str(&user.id).expect("uuid");

        // Persist the flat policy columns + the admin permission flag.
        let policy = UserPolicy {
            is_administrator: true,
            max_active_sessions: 3,
            ..UserPolicy::default()
        };
        mgr.update_policy(id, &policy).await.expect("policy");

        // Seed a list-valued preference directly (the read path parses Guids).
        let folder = Uuid::from_u128(0xF00D);
        set_uuid_preference(
            db.pool(),
            &user.id,
            PreferenceKind::EnabledFolders,
            &[folder],
        )
        .await
        .expect("seed pref");

        let excluded = Uuid::from_u128(0xBEEF);
        let config = UserConfiguration {
            hide_played_in_latest: false,
            latest_items_excludes: vec![excluded],
            subtitle_mode: SubtitlePlaybackMode::Always,
            ..UserConfiguration::default()
        };
        mgr.update_configuration(id, &config).await.expect("config");

        let reloaded = mgr.get_user_by_id(id).await.expect("reload").expect("some");
        let dto = mgr
            .get_user_dto(&reloaded, Some("srv-1".to_owned()))
            .await
            .expect("dto");

        assert_eq!(dto.name.as_deref(), Some("dave"));
        assert_eq!(dto.id, id);
        assert_eq!(dto.server_id.as_deref(), Some("srv-1"));
        let dto_policy = dto.policy.expect("policy");
        assert!(dto_policy.is_administrator);
        assert_eq!(dto_policy.max_active_sessions, 3);
        assert_eq!(dto_policy.enabled_folders, vec![folder]);
        let dto_config = dto.configuration.expect("config");
        assert!(!dto_config.hide_played_in_latest);
        assert_eq!(dto_config.latest_items_excludes, vec![excluded]);
        assert_eq!(dto_config.subtitle_mode, SubtitlePlaybackMode::Always);
    }

    #[tokio::test]
    async fn create_rename_and_duplicate_guard() {
        let db = test_db().await;
        let mgr = HermitUserManager::new(db);

        let user = mgr.create_user("alice").await.expect("create");
        assert_eq!(user.username, "alice");
        assert_eq!(user.normalized_username, "ALICE");
        assert_eq!(user.authentication_provider_id, DEFAULT_AUTH_PROVIDER_ID);
        // Jellyfin User defaults: Latest hides played, and the default Cast receiver id.
        assert!(user.hide_played_in_latest);
        assert_eq!(user.cast_receiver_id.as_deref(), Some("F007D354"));

        // Duplicate (case-insensitive) is rejected.
        assert!(matches!(
            mgr.create_user("ALICE").await,
            Err(ServiceError::InvalidInput(_))
        ));

        let id = Uuid::parse_str(&user.id).expect("uuid");
        mgr.rename_user(id, "alice", "bob").await.expect("rename");
        assert_eq!(
            mgr.get_user_by_name("bob")
                .await
                .expect("by name")
                .expect("some")
                .username,
            "bob"
        );

        // Invalid new name rejected.
        assert!(matches!(
            mgr.rename_user(id, "bob", "..").await,
            Err(ServiceError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn change_password_then_authenticate() {
        let db = test_db().await;
        let mgr = HermitUserManager::new(db);
        let user = mgr.create_user("carol").await.expect("create");
        let id = Uuid::parse_str(&user.id).expect("uuid");

        mgr.change_password(id, "s3cret").await.expect("set pw");

        let ok = mgr
            .authenticate_user("carol", "s3cret", "127.0.0.1", true)
            .await
            .expect("auth ok");
        assert!(ok.is_some());
        assert!(ok.unwrap().last_login_date.is_some());

        // Wrong password: not an error, but None.
        let bad = mgr
            .authenticate_user("carol", "nope", "127.0.0.1", true)
            .await
            .expect("auth call");
        assert!(bad.is_none());

        // Unknown user: unauthorized.
        assert!(matches!(
            mgr.authenticate_user("nobody", "x", "127.0.0.1", true)
                .await,
            Err(ServiceError::Unauthorized(_))
        ));
    }

    #[tokio::test]
    async fn admin_password_may_not_be_emptied() {
        let db = test_db().await;
        let mgr = HermitUserManager::new(db.clone());
        mgr.initialize().await.expect("init");
        let admin = mgr.get_first_user().await.expect("first").expect("some");
        let id = Uuid::parse_str(&admin.id).expect("uuid");
        assert!(matches!(
            mgr.change_password(id, "   ").await,
            Err(ServiceError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn lockout_disables_after_threshold() {
        let db = test_db().await;
        let mgr = HermitUserManager::new(db.clone());
        let user = mgr.create_user("dan").await.expect("create");
        mgr.change_password(Uuid::parse_str(&user.id).expect("uuid"), "pw")
            .await
            .expect("pw");

        // Lockout after 2 failures.
        sqlx::query(r#"UPDATE "Users" SET "LoginAttemptsBeforeLockout" = 2 WHERE "Id" = ?1"#)
            .bind(&user.id)
            .execute(db.pool())
            .await
            .expect("set threshold");

        assert!(
            mgr.authenticate_user("dan", "bad", "127.0.0.1", true)
                .await
                .expect("call")
                .is_none()
        );
        assert!(
            mgr.authenticate_user("dan", "bad", "127.0.0.1", true)
                .await
                .expect("call")
                .is_none()
        );

        assert!(
            has_permission(db.pool(), &user.id, PermissionKind::IsDisabled)
                .await
                .expect("perm")
        );
        // Now even the correct password is refused (disabled).
        assert!(matches!(
            mgr.authenticate_user("dan", "pw", "127.0.0.1", true).await,
            Err(ServiceError::Unauthorized(_))
        ));
    }

    #[tokio::test]
    async fn delete_guards_last_user_and_last_admin() {
        let db = test_db().await;
        let mgr = HermitUserManager::new(db);
        mgr.initialize().await.expect("init");
        let admin = mgr.get_first_user().await.expect("first").expect("some");
        let admin_id = Uuid::parse_str(&admin.id).expect("uuid");

        // Only user: cannot delete.
        assert!(matches!(
            mgr.delete_user(admin_id).await,
            Err(ServiceError::InvalidInput(_))
        ));

        // Add a second (non-admin) user: still cannot delete the only admin.
        let other = mgr.create_user("erin").await.expect("create");
        let other_id = Uuid::parse_str(&other.id).expect("uuid");
        assert!(matches!(
            mgr.delete_user(admin_id).await,
            Err(ServiceError::InvalidInput(_))
        ));

        // But the non-admin can be deleted.
        mgr.delete_user(other_id).await.expect("delete non-admin");
        assert_eq!(mgr.get_users().await.expect("users").len(), 1);
    }

    #[tokio::test]
    async fn providers_list_has_default_first() {
        let db = test_db().await;
        let mgr = HermitUserManager::new(db);
        let providers = mgr.get_authentication_providers().await.expect("providers");
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id.as_deref(), Some(DEFAULT_AUTH_PROVIDER_ID));
    }
}
