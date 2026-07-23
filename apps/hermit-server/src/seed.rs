//! Fresh-install admin seeding — port of Jellyfin's first-run bootstrap.
//!
//! On a brand-new install the `Users` table is empty, so no one can log in and
//! the web client would be stuck at the setup wizard. Jellyfin covers this in two
//! places: `UserManager.InitializeAsync` creates one administrator on first run,
//! and the startup wizard's `StartupController.UpdateStartupUser` later renames
//! that user and sets its password. A headless Hermit install has no interactive
//! wizard, so this module folds both into a single non-interactive seed: when the
//! database has no users, create the configured administrator, set its password
//! (the configured one, or a freshly generated random one when none is
//! configured — Jellyfin forbids an empty admin password), and grant it the
//! administrator policy.
//!
//! Seeding is idempotent and a no-op once any user exists, so it is safe to call
//! unconditionally on every boot: an existing install is never disturbed.

use anyhow::Context as _;
use hermit_model::users::UserPolicy;
use hermit_traits::library::UserManager;
use uuid::Uuid;

use crate::config::Config;

/// The number of random bytes used to generate an initial admin password when
/// none is configured. 24 bytes → a 48-character hex secret, comfortably beyond
/// brute-force reach while staying copy-pasteable from the startup log.
const GENERATED_PASSWORD_BYTES: usize = 24;

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
    /// A default administrator was created with a freshly generated password,
    /// carried here so the caller can print it once for the operator.
    SeededWithGeneratedPassword {
        /// The seeded administrator's username.
        username: String,
        /// The generated plaintext password — log it exactly once; it cannot be
        /// recovered afterwards (only its hash is persisted).
        password: String,
    },
}

/// Seeds a default administrator when the install is fresh (no users exist).
///
/// Port of the combined `UserManager.InitializeAsync` + startup-wizard
/// `UpdateStartupUser` path, collapsed for a headless first run:
///
/// 1. If any user already exists, return [`SeedOutcome::AlreadyInitialized`]
///    without touching the database (idempotent — safe on every boot).
/// 2. Otherwise create the [`Config::admin_user`] account.
/// 3. Set its password: the configured [`Config::admin_password`] when non-empty,
///    else a freshly generated random secret (Jellyfin refuses to leave an admin
///    password empty). The password is set *before* the administrator flag so the
///    manager's "admin passwords must not be empty" guard does not reject it.
/// 4. Grant it the administrator [`UserPolicy`].
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

    // Resolve the password before granting admin: the manager forbids an empty
    // password on an administrator, so an unconfigured password becomes a random
    // secret rather than a boot failure.
    let (password, generated) = match config.admin_password.trim() {
        "" => (generate_password(), true),
        configured => (configured.to_owned(), false),
    };

    users
        .change_password(user_id, &password)
        .await
        .context("failed to set the default admin password")?;

    users
        .update_policy(
            user_id,
            &UserPolicy {
                is_administrator: true,
                ..UserPolicy::default()
            },
        )
        .await
        .context("failed to grant the default admin administrator policy")?;

    Ok(if generated {
        SeedOutcome::SeededWithGeneratedPassword { username, password }
    } else {
        SeedOutcome::SeededWithConfiguredPassword { username }
    })
}

/// Generates a random, URL-safe hex password.
///
/// Uses [`Uuid::new_v4`] as the entropy source (the workspace's only random
/// dependency here), rendering each v4 UUID as its 32-character lower-hex
/// `simple` form and concatenating until [`GENERATED_PASSWORD_BYTES`] bytes' worth
/// of hex (two hex chars per byte) is reached, then truncating to that length.
fn generate_password() -> String {
    let target_len = GENERATED_PASSWORD_BYTES * 2;
    let mut hex = String::with_capacity(target_len);
    while hex.len() < target_len {
        hex.push_str(&Uuid::new_v4().simple().to_string());
    }
    hex.truncate(target_len);
    hex
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
            ffmpeg_path: None,
            ffprobe_path: None,
            library_roots: Vec::new(),
            server_name: "hermit-test".to_owned(),
            log_level: "info".to_owned(),
            admin_user: admin_user.to_owned(),
            admin_password: admin_password.to_owned(),
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
        assert!(dto.policy.is_some_and(|p| p.is_administrator));

        let authed = users
            .authenticate_user("boss", "s3cret-pass", "", true)
            .await
            .unwrap();
        assert!(authed.is_some(), "configured password authenticates");
    }

    #[tokio::test]
    async fn seeds_admin_with_generated_password_when_unconfigured() {
        let db = fresh_db().await;
        let users = HermitUserManager::new(db.clone());
        let config = seed_config("admin", "");

        let outcome = seed_default_admin(&users, &config).await.unwrap();
        let SeedOutcome::SeededWithGeneratedPassword { username, password } = outcome else {
            panic!("expected a generated-password outcome, got {outcome:?}");
        };
        assert_eq!(username, "admin");
        // Two hex chars per byte.
        assert_eq!(password.len(), GENERATED_PASSWORD_BYTES * 2);
        assert!(password.chars().all(|c| c.is_ascii_hexdigit()));

        // The generated password authenticates the seeded admin.
        let authed = users
            .authenticate_user("admin", &password, "", true)
            .await
            .unwrap();
        assert!(authed.is_some(), "generated password authenticates");
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

    #[tokio::test]
    async fn generated_passwords_differ_between_calls() {
        let a = generate_password();
        let b = generate_password();
        assert_ne!(a, b, "each generated password is unique");
        assert_eq!(a.len(), GENERATED_PASSWORD_BYTES * 2);
    }
}
