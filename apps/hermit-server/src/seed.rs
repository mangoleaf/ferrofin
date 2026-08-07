//! Fresh-install admin seeding — port of Jellyfin's first-run bootstrap.
//!
//! On a brand-new install the `Users` table is empty, so no one can log in and
//! the web client would be stuck at the setup wizard. Jellyfin covers this in two
//! places: `UserManager.InitializeAsync` creates one administrator on first run,
//! and the startup wizard's `StartupController.UpdateStartupUser` later renames
//! that user and sets its password. This module ports `InitializeAsync`: when the
//! database has no users, create the configured administrator and grant it the
//! administrator policy. It sets a password only when `admin_password` is
//! configured (a headless install); otherwise it leaves the admin **passwordless**
//! — exactly like Jellyfin — so the web setup wizard can set it. A generated
//! password would trip the wizard's "first user already has a password" guard and
//! lock setup out.
//!
//! Seeding is idempotent and a no-op once any user exists, so it is safe to call
//! unconditionally on every boot: an existing install is never disturbed.

use anyhow::Context as _;
use hermit_traits::library::UserManager;
use uuid::Uuid;

use crate::config::Config;

/// The outcome of a [`seed_default_admin`] call, so the composition root can log
/// exactly what happened (and surface a generated password once).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedOutcome {
    /// Users already existed; nothing was seeded.
    AlreadyInitialized,
    /// A default administrator was created with the operator-supplied password.
    SeededWithConfiguredPassword {
        /// The seeded administrator's username.
        username: String,
    },
    /// A default administrator was created **without a password** (no
    /// `admin_password` was configured), matching Jellyfin's `InitializeAsync`
    /// default so the web setup wizard can set the password via
    /// `POST /Startup/User`. The account cannot authenticate until a password is
    /// set (through the wizard, or by configuring `admin_password`).
    SeededPasswordless {
        /// The seeded administrator's username.
        username: String,
    },
}

/// Seeds a default administrator when the install is fresh (no users exist).
///
/// Port of `UserManager.InitializeAsync`:
///
/// 1. If any user already exists, return [`SeedOutcome::AlreadyInitialized`]
///    without touching the database (idempotent — safe on every boot).
/// 2. Otherwise create the [`Config::admin_user`] account.
/// 3. If [`Config::admin_password`] is configured (non-empty), set it — a headless,
///    ready-to-use admin. Otherwise leave the admin **passwordless** (Jellyfin's
///    default) so the web setup wizard sets it; `change_password` rejects an empty
///    admin password, so the passwordless path simply skips it.
/// 4. Grant it the administrator [`UserPolicy`] (`update_policy` does not require a
///    password, so a passwordless admin is valid until the wizard sets one).
///
/// Returns the [`SeedOutcome`] so the caller can log the result and, for a
/// generated password, surface it to the operator exactly once.
///
/// # Errors
///
/// Returns an error if querying existing users, creating the user, setting the
/// password, or applying the administrator policy fails.
pub async fn seed_default_admin(
    users: &dyn UserManager,
    config: &Config,
) -> anyhow::Result<SeedOutcome> {
    if !users
        .get_users()
        .await
        .context("failed to query existing users")?
        .is_empty()
    {
        return Ok(SeedOutcome::AlreadyInitialized);
    }

    let username = config.admin_user.clone();
    let user = users
        .create_user(&username)
        .await
        .with_context(|| format!("failed to create default admin user `{username}`"))?;
    let user_id = Uuid::parse_str(&user.id)
        .with_context(|| format!("created admin user has a non-UUID id `{}`", user.id))?;

    // Set the password only when one is configured. With no `admin_password`, seed
    // a PASSWORDLESS admin (Jellyfin's `InitializeAsync` default) so the web setup
    // wizard can set it via `POST /Startup/User` — the wizard's `UpdateStartupUser`
    // (like Jellyfin) forbids updating a first user that already has a password, so
    // a generated password would lock the wizard out. `change_password` rejects an
    // empty admin password, so for the passwordless case we simply skip it;
    // `update_policy` grants admin without touching the password.
    let configured = config.admin_password.trim();
    let outcome = if configured.is_empty() {
        SeedOutcome::SeededPasswordless {
            username: username.clone(),
        }
    } else {
        users
            .change_password(user_id, configured)
            .await
            .context("failed to set the default admin password")?;
        SeedOutcome::SeededWithConfiguredPassword {
            username: username.clone(),
        }
    };

    // create_user already set the Jellyfin per-user defaults (auth/password-reset provider
    // ids, preference access, …); fetch that policy and ELEVATE it to admin rather than
    // overwriting with UserPolicy::default(), which would blank the provider ids and reset
    // the per-user flags. Mirrors Jellyfin's admin: content deletion + remote control on,
    // login lockout disabled (-1).
    let mut policy = users
        .get_user_dto(&user, None)
        .await
        .context("failed to read the seeded admin's policy")?
        .policy
        .unwrap_or_default();
    policy.is_administrator = true;
    policy.enable_user_preference_access = true;
    policy.enable_content_deletion = true;
    policy.enable_remote_control_of_other_users = true;
    policy.login_attempts_before_lockout = -1;
    users
        .update_policy(user_id, &policy)
        .await
        .context("failed to grant the default admin administrator policy")?;

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermit_core::HermitUserManager;
    use hermit_db::Database;

    /// A bootstrap [`Config`] with the given admin credentials, pointing paths at
    /// throwaway values (seeding only reads `admin_user` / `admin_password`).
    fn seed_config(admin_user: &str, admin_password: &str) -> Config {
        Config {
            data_dir: std::path::PathBuf::from("/tmp/hermit"),
            config_dir: std::path::PathBuf::from("/tmp/hermit/config"),
            cache_dir: std::path::PathBuf::from("/tmp/hermit/cache"),
            web_dir: std::path::PathBuf::from("/tmp/hermit/web"),
            bind_addr: "127.0.0.1".parse().unwrap(),
            port: 8096,
            https_port: 8920,
            published_url: None,
            base_url: String::new(),
            omdb_api_key: String::new(),
            studios_repo_url: String::new(),
            tvdb_api_key: String::new(),
            tvdb_subscriber_pin: String::new(),
            ffmpeg_path: None,
            ffprobe_path: None,
            library_roots: Vec::new(),
            server_name: "hermit-test".to_owned(),
            log_level: "info".to_owned(),
            admin_user: admin_user.to_owned(),
            admin_password: admin_password.to_owned(),
            db_pool: None,
            enable_metrics: None,
            metrics_sample_interval: None,
            scan_progress_every: None,
        }
    }

    async fn fresh_db() -> Database {
        let db = Database::connect_in_memory()
            .await
            .expect("in-memory db opens");
        db.run_migrations().await.expect("migrations apply");
        db
    }

    #[tokio::test]
    async fn seeds_admin_with_configured_password() {
        let db = fresh_db().await;
        let users = HermitUserManager::new(db.clone());
        let config = seed_config("boss", "s3cret-pass");

        let outcome = seed_default_admin(&users, &config).await.unwrap();
        assert_eq!(
            outcome,
            SeedOutcome::SeededWithConfiguredPassword {
                username: "boss".to_owned()
            }
        );

        // The seeded user exists, is an administrator, and authenticates with the
        // configured password.
        let user = users
            .get_user_by_name("boss")
            .await
            .unwrap()
            .expect("seeded admin exists");
        let dto = users.get_user_dto(&user, None).await.unwrap();
        let policy = dto.policy.expect("seeded admin has a policy");
        assert!(policy.is_administrator);
        // Jellyfin's admin: content-deletion + remote-control permissions on, login lockout
        // disabled, and the per-user provider id create_user set is preserved (not blanked).
        assert!(policy.enable_content_deletion);
        assert!(policy.enable_remote_control_of_other_users);
        assert!(policy.enable_user_preference_access);
        assert_eq!(policy.login_attempts_before_lockout, -1);
        assert!(!policy.authentication_provider_id.is_empty());

        let authed = users
            .authenticate_user("boss", "s3cret-pass", "", true)
            .await
            .unwrap();
        assert!(authed.is_some(), "configured password authenticates");
    }

    #[tokio::test]
    async fn seeds_passwordless_admin_when_unconfigured() {
        let db = fresh_db().await;
        let users = HermitUserManager::new(db.clone());
        let config = seed_config("admin", "");

        let outcome = seed_default_admin(&users, &config).await.unwrap();
        assert_eq!(
            outcome,
            SeedOutcome::SeededPasswordless {
                username: "admin".to_owned()
            }
        );

        // The seeded admin exists and holds the administrator policy, but has NO
        // password yet, so the setup wizard's UpdateStartupUser can set one (its
        // "first user already has a password" guard must not trip).
        let user = users
            .get_user_by_name("admin")
            .await
            .unwrap()
            .expect("seeded admin exists");
        let dto = users.get_user_dto(&user, None).await.unwrap();
        assert!(dto.policy.is_some_and(|p| p.is_administrator));
        assert!(
            user.password.is_none(),
            "unconfigured seed leaves the admin passwordless for the wizard"
        );
    }

    #[tokio::test]
    async fn is_idempotent_once_a_user_exists() {
        let db = fresh_db().await;
        let users = HermitUserManager::new(db.clone());
        let config = seed_config("admin", "pw");

        // First seed creates the admin.
        seed_default_admin(&users, &config).await.unwrap();
        // Second call is a no-op: the install is no longer fresh.
        let again = seed_default_admin(&users, &config).await.unwrap();
        assert_eq!(again, SeedOutcome::AlreadyInitialized);

        // Still exactly one user.
        assert_eq!(users.get_users().await.unwrap().len(), 1);
    }
}
