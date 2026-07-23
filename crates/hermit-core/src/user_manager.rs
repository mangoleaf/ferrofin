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
//! - `get_user_dto` is not ported (the `UserDto` assembly belongs to
//!   `DtoService`); the trait exposes rows, not DTOs.
//! - `update_policy` persists the flat `Users` columns and reflects the two
//!   load-bearing permission flags into `Permissions`; the broader
//!   folder/channel/schedule policy mapping is a flagged follow-up.

use std::sync::Arc;

use async_trait::async_trait;
use hermit_db::Database;
use hermit_db::entities::users::UserEntity;
use hermit_db::enums::{PermissionKind, PreferenceKind};
use hermit_model::configuration::UserConfiguration;
use hermit_model::dto::NameIdPair;
use hermit_model::users::UserPolicy;
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
        }
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
                "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
                "MaxActiveSessions", "MustUpdatePassword", "NormalizedUsername",
                "PasswordResetProviderId", "PlayDefaultAudioTrack",
                "RememberAudioSelections", "RememberSubtitleSelections",
                "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username")
               VALUES (?1, ?2, 0, 0, 0, 0, 1, 1, 0, ?3, 0, 0, 0, ?4, ?5, 1, 1, 1, 0, 0, 0, ?6)"#,
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
    async fn create_rename_and_duplicate_guard() {
        let db = test_db().await;
        let mgr = HermitUserManager::new(db);

        let user = mgr.create_user("alice").await.expect("create");
        assert_eq!(user.username, "alice");
        assert_eq!(user.normalized_username, "ALICE");
        assert_eq!(user.authentication_provider_id, DEFAULT_AUTH_PROVIDER_ID);

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
