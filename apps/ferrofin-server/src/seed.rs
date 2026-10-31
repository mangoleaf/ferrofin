//! Fresh-install seeding: the administrator (a port of Jellyfin's first-run
//! bootstrap) and the libraries named by `library_roots`.
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
//!
//! [`seed_library_roots`] is Ferrofin's own: Jellyfin has no setting that starts
//! a server with libraries. The composition root calls it only on the boot that
//! created the administrator, so it never runs on an existing install.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_traits::library::{LibraryManager, LibraryMonitor, UserManager, VirtualFolderManager};
use uuid::Uuid;

use crate::config::Config;

/// The outcome of a [`seed_default_admin`] call, so the composition root can log
/// exactly what happened (and warn about an administrator seeded without a
/// password).
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
    /// `POST /Startup/User`. Until a password is set (through the wizard, or by
    /// configuring `admin_password`), the account signs in with an EMPTY password,
    /// as in Jellyfin — which is why a server that has been set up refuses to
    /// start on a missing or empty database instead of reaching this seed.
    SeededPasswordless {
        /// The seeded administrator's username.
        username: String,
    },
}

impl SeedOutcome {
    /// Whether this boot created the administrator: the database had no users,
    /// so the install is fresh. False on every later boot, an in-process
    /// restart and an adopted Jellyfin database, which all have users.
    #[must_use]
    pub fn is_fresh_install(&self) -> bool {
        !matches!(self, Self::AlreadyInitialized)
    }
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
/// passwordless administrator, warn the operator to finish setup.
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

/// Adds one library per [`Config::library_roots`] entry, then restarts the
/// library monitor and queues the libraries' first scan.
///
/// The composition root calls this only when [`seed_default_admin`] created the
/// administrator ([`SeedOutcome::is_fresh_install`]), so it never runs on a
/// later boot or an adopted Jellyfin database, and never brings back a library
/// an administrator removed.
///
/// Each root becomes a library through
/// [`VirtualFolderManager::add_virtual_folder`], the call
/// `POST /Library/VirtualFolders` makes (see [`add_root_libraries`] for the
/// name, the options and the roots it skips). When at least one was added, the
/// library monitor restarts so it watches the new locations and one full scan
/// is queued, as a library added with `refreshLibrary=true` gets; the scan runs
/// in the background. Nothing here stops the boot: every failure is a warning.
pub async fn seed_library_roots(
    virtual_folders: &dyn VirtualFolderManager,
    monitor: &dyn LibraryMonitor,
    library: &dyn LibraryManager,
    roots: &[PathBuf],
) {
    if add_root_libraries(virtual_folders, roots).await == 0 {
        return;
    }
    if let Err(error) = monitor.stop().await {
        tracing::warn!(%error, "failed to stop the library monitor after seeding libraries");
    }
    if let Err(error) = monitor.start().await {
        tracing::warn!(%error, "failed to restart the library monitor after seeding libraries");
    }
    if let Err(error) = library.queue_library_scan().await {
        tracing::warn!(%error, "failed to queue the first scan of the seeded libraries");
    }
}

/// Adds a library for each root that can be one and returns how many it added.
///
/// A library is named after its root's folder, has no collection type (mixed
/// content, which the dashboard can change) and the default
/// [`LibraryOptions`], Jellyfin's `LibraryOptions` constructor, which is what
/// `POST /Library/VirtualFolders` without a body gets. The manager makes the
/// name a valid file name and numbers a taken one, so two roots called
/// `Movies` become `Movies` and `Movies2`.
///
/// A root is skipped with a warning when it is empty, not valid UTF-8 (the
/// location is stored as text), has no folder name (`/`), is already a
/// library's location (an earlier root, or a library the data directory kept
/// when only the database was replaced), does not exist or is not a directory,
/// and when the manager refuses it. A relative root is taken against the
/// working directory and stored absolute.
async fn add_root_libraries(
    virtual_folders: &dyn VirtualFolderManager,
    roots: &[PathBuf],
) -> usize {
    // Libraries are folders in the data directory, not database rows, so a
    // fresh database can still find some.
    let mut taken: Vec<String> = match virtual_folders.get_virtual_folders().await {
        Ok(folders) => folders
            .into_iter()
            .flat_map(|folder| folder.locations)
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "could not list the existing libraries before seeding library_roots");
            Vec::new()
        }
    };
    let mut added = 0;
    for root in roots {
        let (location, name) = match library_location(root, &taken).await {
            Ok(found) => found,
            Err(reason) => {
                tracing::warn!(
                    path = %root.display(),
                    reason = %reason,
                    "skipping a library_roots entry that cannot be a library"
                );
                continue;
            }
        };
        let options = LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: location.clone(),
            }],
            ..LibraryOptions::default()
        };
        match virtual_folders
            .add_virtual_folder(&name, None, &options)
            .await
        {
            Ok(item_id) => {
                let library = stored_name(virtual_folders, item_id).await.unwrap_or(name);
                tracing::info!(
                    library = %library,
                    item_id = item_id.map(tracing::field::display),
                    path = %location,
                    "seeded a library from library_roots"
                );
                taken.push(location);
                added += 1;
            }
            Err(error) => tracing::warn!(
                %error,
                path = %location,
                "could not add a library for a library_roots entry; skipping it"
            ),
        }
    }
    added
}

/// The name the library `item_id` was stored under, which is not the one asked
/// for when that was taken (`Movies2`); `None` when it cannot be listed.
async fn stored_name(
    virtual_folders: &dyn VirtualFolderManager,
    item_id: Option<Uuid>,
) -> Option<String> {
    let item_id = item_id?;
    virtual_folders
        .get_virtual_folders()
        .await
        .ok()?
        .into_iter()
        .find(|folder| {
            folder
                .item_id
                .as_deref()
                .and_then(|id| Uuid::parse_str(id).ok())
                == Some(item_id)
        })?
        .name
}

/// The location to store for `root` and the name to ask for its library, or
/// why it cannot be one. `taken` are the locations libraries already have.
async fn library_location(root: &Path, taken: &[String]) -> Result<(String, String), String> {
    if root.as_os_str().is_empty() {
        return Err("the path is empty".to_owned());
    }
    // Lexically absolute (symlinks are kept, as the operator wrote them), and
    // normalised by its components: no `.`, repeated or trailing separators.
    let absolute: PathBuf = std::path::absolute(root)
        .map_err(|error| format!("the path cannot be made absolute: {error}"))?
        .components()
        .collect();
    let location = absolute
        .to_str()
        .ok_or_else(|| "the path is not valid UTF-8".to_owned())?
        .to_owned();
    let name = absolute
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "the path has no folder name to name its library after".to_owned())?
        .to_owned();
    if taken.contains(&location) {
        return Err("the path is already a library's location".to_owned());
    }
    match tokio::fs::metadata(&absolute).await {
        Ok(metadata) if metadata.is_dir() => Ok((location, name)),
        Ok(_) => Err("the path is not a directory".to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err("the path does not exist".to_owned())
        }
        Err(error) => Err(format!("the path cannot be read: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrofin_core::{FerrofinUserManager, FerrofinVirtualFolderManager};
    use ferrofin_db::Database;
    use ferrofin_model::entities::CollectionTypeOptions;

    /// A bootstrap [`Config`] with the given admin credentials, pointing paths at
    /// throwaway values (seeding only reads `admin_user` / `admin_password`).
    fn seed_config(admin_user: &str, admin_password: &str) -> Config {
        Config {
            admin_user: admin_user.to_owned(),
            admin_password: admin_password.to_owned(),
            ..Config::test_stub(std::path::Path::new("/tmp/ferrofin"))
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
        let users = FerrofinUserManager::new(db.clone());
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
        assert_ne!(policy.authentication_provider_id, "");

        let authed = users
            .authenticate_user("boss", "s3cret-pass", "", true)
            .await
            .unwrap();
        assert!(authed.is_some(), "configured password authenticates");
    }

    #[tokio::test]
    async fn seeds_passwordless_admin_when_unconfigured() {
        let db = fresh_db().await;
        let users = FerrofinUserManager::new(db.clone());
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
        let users = FerrofinUserManager::new(db.clone());
        let config = seed_config("admin", "pw");

        // First seed creates the admin.
        seed_default_admin(&users, &config).await.unwrap();
        // Second call is a no-op: the install is no longer fresh.
        let again = seed_default_admin(&users, &config).await.unwrap();
        assert_eq!(again, SeedOutcome::AlreadyInitialized);

        // Still exactly one user.
        assert_eq!(users.get_users().await.unwrap().len(), 1);
    }

    #[test]
    fn only_a_seeded_administrator_means_a_fresh_install() {
        let username = "admin".to_owned();
        assert!(!SeedOutcome::AlreadyInitialized.is_fresh_install());
        assert!(
            SeedOutcome::SeededPasswordless {
                username: username.clone()
            }
            .is_fresh_install()
        );
        assert!(SeedOutcome::SeededWithConfiguredPassword { username }.is_fresh_install());
    }

    /// `(Name, Locations, CollectionType)` of every library `manager` lists,
    /// sorted by name.
    async fn listed(
        manager: &FerrofinVirtualFolderManager,
    ) -> Vec<(String, Vec<String>, Option<CollectionTypeOptions>)> {
        let mut rows: Vec<_> = manager
            .get_virtual_folders()
            .await
            .unwrap()
            .into_iter()
            .map(|folder| {
                (
                    folder.name.unwrap_or_default(),
                    folder.locations,
                    folder.collection_type,
                )
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }

    fn utf8(path: &Path) -> String {
        path.to_str().expect("UTF-8 temp path").to_owned()
    }

    #[tokio::test]
    async fn each_root_that_is_a_folder_becomes_a_library_named_after_it() {
        let tmp = tempfile::tempdir().unwrap();
        let movies = tmp.path().join("a").join("Movies");
        let more_movies = tmp.path().join("b").join("Movies");
        let music = tmp.path().join("Music");
        for dir in [&movies, &more_movies, &music] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let not_a_folder = tmp.path().join("notes.txt");
        std::fs::write(&not_a_folder, "text").unwrap();
        let manager = FerrofinVirtualFolderManager::new(tmp.path().join("root").join("default"));

        let roots = [
            // Stored normalised: no `.` and no trailing separator.
            PathBuf::from(format!("{}/./", movies.display())),
            tmp.path().join("missing"),
            not_a_folder,
            PathBuf::new(),
            PathBuf::from("/"),
            // The same folder name again: the manager numbers it.
            more_movies.clone(),
            // A root already seeded, spelled differently.
            movies.join("."),
            music.clone(),
        ];
        assert_eq!(add_root_libraries(&manager, &roots).await, 3);
        assert_eq!(
            listed(&manager).await,
            [
                ("Movies".to_owned(), vec![utf8(&movies)], None),
                ("Movies2".to_owned(), vec![utf8(&more_movies)], None),
                ("Music".to_owned(), vec![utf8(&music)], None),
            ]
        );
    }

    #[tokio::test]
    async fn no_roots_add_no_library() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = FerrofinVirtualFolderManager::new(tmp.path().join("root").join("default"));
        assert_eq!(add_root_libraries(&manager, &[]).await, 0);
        let libraries = listed(&manager).await;
        assert!(libraries.is_empty(), "{libraries:?}");
    }

    /// A database replaced under a data directory that kept its libraries: a
    /// root that is already one's location is not added a second time.
    #[tokio::test]
    async fn a_root_that_is_already_a_library_is_not_added_again() {
        let tmp = tempfile::tempdir().unwrap();
        let movies = tmp.path().join("Movies");
        let shows = tmp.path().join("Shows");
        for dir in [&movies, &shows] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let manager = FerrofinVirtualFolderManager::new(tmp.path().join("root").join("default"));
        let options = LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: utf8(&movies),
            }],
            ..LibraryOptions::default()
        };
        manager
            .add_virtual_folder("Films", Some(CollectionTypeOptions::movies), &options)
            .await
            .unwrap();

        assert_eq!(
            add_root_libraries(&manager, &[movies.clone(), shows.clone()]).await,
            1
        );
        assert_eq!(
            listed(&manager).await,
            [
                (
                    "Films".to_owned(),
                    vec![utf8(&movies)],
                    Some(CollectionTypeOptions::movies)
                ),
                ("Shows".to_owned(), vec![utf8(&shows)], None),
            ]
        );
    }

    #[tokio::test]
    async fn the_name_a_library_was_stored_under_is_found_by_its_id() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = FerrofinVirtualFolderManager::new(tmp.path().join("root").join("default"));
        let mut ids = Vec::new();
        for folder in ["a", "b"] {
            let path = tmp.path().join(folder);
            std::fs::create_dir_all(&path).unwrap();
            let options = LibraryOptions {
                path_infos: vec![MediaPathInfo { path: utf8(&path) }],
                ..LibraryOptions::default()
            };
            ids.push(
                manager
                    .add_virtual_folder("Movies", None, &options)
                    .await
                    .unwrap(),
            );
        }
        assert_eq!(
            stored_name(&manager, ids[1]).await.as_deref(),
            Some("Movies2")
        );
        assert_eq!(stored_name(&manager, None).await, None);
    }

    #[tokio::test]
    async fn a_root_is_refused_with_the_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("notes.txt");
        std::fs::write(&file, "text").unwrap();
        let taken = [utf8(tmp.path())];
        for (root, reason) in [
            (PathBuf::new(), "the path is empty"),
            (
                PathBuf::from("/"),
                "the path has no folder name to name its library after",
            ),
            (tmp.path().join("missing"), "the path does not exist"),
            (file, "the path is not a directory"),
            (
                tmp.path().to_owned(),
                "the path is already a library's location",
            ),
        ] {
            assert_eq!(
                library_location(&root, &taken).await,
                Err(reason.to_owned()),
                "{}",
                root.display()
            );
        }
    }

    #[tokio::test]
    async fn a_relative_root_is_stored_absolute() {
        // Tests run in the package directory, which holds `src`.
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            library_location(Path::new("src"), &[]).await,
            Ok((utf8(&cwd.join("src")), "src".to_owned()))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_root_that_is_not_utf8_is_refused() {
        use std::os::unix::ffi::OsStrExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(std::ffi::OsStr::from_bytes(b"bad\xff"));
        std::fs::create_dir(&root).unwrap();
        assert_eq!(
            library_location(&root, &[]).await,
            Err("the path is not valid UTF-8".to_owned())
        );
    }
}
