//! Where the named configurations (`encoding`, `network`, `livetv`, …) live:
//! `{config}/{key}.json`, as upstream keeps `{config}/{key}.xml`
//! (`BaseConfigurationManager.GetConfigurationFile`).
//!
//! Releases before 1.6.0 kept them in `{config}/users/named/`. That is inside
//! the user-configuration directory, where every user's profile image has a
//! folder named after them, so a user named `named` had theirs written among
//! the server's configuration (gl-319). [`move_legacy`] moves them out at boot.

use std::path::{Path, PathBuf};

use crate::configuration_manager::FerrofinServerConfigurationManager as Manager;

/// The file of every named configuration Ferrofin stores.
pub const FILES: [&str; 6] = [
    Manager::BRANDING_FILE_NAME,
    Manager::ENCODING_FILE_NAME,
    Manager::NETWORK_FILE_NAME,
    Manager::METADATA_FILE_NAME,
    Manager::XBMC_METADATA_FILE_NAME,
    Manager::LIVE_TV_FILE_NAME,
];

/// Where releases before 1.6.0 kept the named configurations of the config
/// directory `config_dir`.
#[must_use]
pub fn legacy_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("users").join("named")
}

/// Whether the legacy entry whose metadata (not followed) is `found` holds
/// newer data than `current`: it is a file, not the link [`move_legacy`]
/// leaves, and `current` is missing, not a file, or older. Only a release
/// before 1.6.0 writes there (before an upgrade's first boot, beside this one
/// in a rolling update, or by restoring one of its backups), so its later
/// save wins, as it did over the one file both used to share.
fn holds_newer(found: &std::fs::Metadata, current: &Path) -> std::io::Result<bool> {
    if !found.is_file() {
        return Ok(false);
    }
    match std::fs::metadata(current) {
        Ok(kept) if kept.is_file() => Ok(found.modified()? > kept.modified()?),
        // A folder in `current`'s place is no configuration: the file wins.
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

/// The file to read the named configuration `file` of the config directory
/// `config_dir` from: `{config}/{file}`, or the file a release before 1.6.0
/// left while it holds newer data ([`holds_newer`]), as in a data directory
/// not booted since the upgrade. For the readers that can run before
/// [`move_legacy`]: the network configuration the listeners bind by, and the
/// export.
#[must_use]
pub fn readable(config_dir: &Path, file: &str) -> PathBuf {
    let (current, legacy) = (config_dir.join(file), legacy_dir(config_dir).join(file));
    let newer = std::fs::symlink_metadata(&legacy)
        .and_then(|found| holds_newer(&found, &current))
        .unwrap_or(false);
    if newer { legacy } else { current }
}

/// Moves the named configurations a release before 1.6.0 left in
/// `{config}/users/named/` to `{config}/`, before anything reads them. One
/// holding newer data ([`holds_newer`]) replaces the current file; one that
/// does not is removed. A link to the moved file is left in its place (on
/// Unix), so that release, still serving beside this one in a rolling
/// update, keeps reading its configuration until it stops; a save there
/// replaces the link with a file, which the next boot moves in turn.
///
/// Returns the links an earlier boot left, for [`remove_links`] to remove
/// once this server takes over from any release before it.
///
/// shortcut: a boot that stopped before its handover counts as an earlier
/// boot, so after a crash loop during a rolling update the links go at the
/// first handover, while the previous release is still being stopped; record
/// each handover in a marker if that window matters.
///
/// # Errors
///
/// Returns the error of a file that could not be moved or removed, or of a
/// link Ferrofin did not make, with its path: booting on the defaults would
/// drop the operator's settings, the remote-IP filter among them.
pub fn move_legacy(config_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let legacy_dir = legacy_dir(config_dir);
    match std::fs::metadata(&legacy_dir) {
        Ok(found) if found.is_dir() => {}
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            let message = format!("could not read {}: {error}", legacy_dir.display());
            return Err(std::io::Error::new(error.kind(), message));
        }
        _ => return Ok(Vec::new()),
    }
    let mut links = Vec::new();
    for file in FILES {
        let (legacy, current) = (legacy_dir.join(file), config_dir.join(file));
        let failed = |step: &str, error: std::io::Error| {
            std::io::Error::new(
                error.kind(),
                format!("could not {step} {}: {error}", legacy.display()),
            )
        };
        let found = match std::fs::symlink_metadata(&legacy) {
            Ok(found) => found,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(failed("read", error)),
        };
        if found.is_symlink() {
            if std::fs::read_link(&legacy).is_ok_and(|to| to == link_target(file)) {
                links.push(legacy);
                continue;
            }
            // Not ours: moving it could break its relative target, and
            // leaving it would boot on the defaults.
            return Err(failed(
                "move",
                std::io::Error::other(format!(
                    "it is a link Ferrofin did not make; remove it and put its file at {}",
                    current.display()
                )),
            ));
        }
        if holds_newer(&found, &current).map_err(|error| failed("compare", error))? {
            std::fs::rename(&legacy, &current).map_err(|error| failed("move", error))?;
            tracing::info!(
                path = %legacy.display(),
                to = %current.display(),
                "moved a named configuration out of the user configuration directory"
            );
        } else if found.is_file() {
            std::fs::remove_file(&legacy).map_err(|error| failed("remove", error))?;
            tracing::info!(
                path = %legacy.display(),
                kept = %current.display(),
                "removed a named configuration older than the one in the configuration directory"
            );
        } else {
            continue;
        }
        link_back(&legacy, file);
    }
    Ok(links)
}

/// Where the link [`move_legacy`] leaves at `{config}/users/named/{file}`
/// points: the moved `{config}/{file}`.
fn link_target(file: &str) -> PathBuf {
    Path::new("../..").join(file)
}

/// Leaves at `legacy` (`{config}/users/named/{file}`) a link to the moved
/// `{config}/{file}`.
#[cfg(unix)]
fn link_back(legacy: &Path, file: &str) {
    if let Err(error) = std::os::unix::fs::symlink(link_target(file), legacy) {
        tracing::warn!(
            path = %legacy.display(),
            %error,
            "could not link a moved named configuration back; a previous release still serving \
             beside this one reads its defaults for it until it stops"
        );
    }
}

/// No link on other platforms, which have no rolling update to serve.
#[cfg(not(unix))]
fn link_back(_legacy: &Path, _file: &str) {}

/// Removes the `links` [`move_legacy`] found, each while it is still a link,
/// then `{config}/users/named/` while it is empty: a user named `named`
/// keeps their profile image there.
pub fn remove_links(config_dir: &Path, links: &[PathBuf]) {
    for link in links {
        if !std::fs::symlink_metadata(link).is_ok_and(|found| found.is_symlink()) {
            continue;
        }
        if let Err(error) = std::fs::remove_file(link) {
            tracing::warn!(path = %link.display(), %error, "could not remove the link to a moved named configuration");
        }
    }
    let dir = legacy_dir(config_dir);
    match std::fs::remove_dir(&dir) {
        // Fails, as it should, while anything is left in it.
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
            ) => {}
        Err(error) => {
            tracing::debug!(path = %dir.display(), %error, "could not remove the emptied named configuration folder");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `body` at `path` with the modification time `secs` after the
    /// epoch.
    fn write_at(path: &Path, body: &str, secs: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn an_upgrade_moves_every_file_and_leaves_a_users_image() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path();
        for file in FILES {
            write_at(&legacy_dir(config).join(file), file, 100);
        }
        write_at(&legacy_dir(config).join("profile.png"), "avatar", 100);

        assert!(
            move_legacy(config).unwrap().is_empty(),
            "no link from before"
        );

        for file in FILES {
            assert_eq!(read(&config.join(file)), file);
            assert_eq!(readable(config, file), config.join(file));
            #[cfg(unix)]
            {
                let link = legacy_dir(config).join(file);
                assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
                assert_eq!(read(&link), file, "the previous release reads it");
            }
        }
        assert_eq!(read(&legacy_dir(config).join("profile.png")), "avatar");
    }

    #[test]
    fn the_newer_of_two_files_wins() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path();
        let legacy = legacy_dir(config);
        // Saved by the previous release after this one's last save.
        write_at(&config.join("encoding.json"), "current", 100);
        write_at(&legacy.join("encoding.json"), "later save", 200);
        // Saved by this release after the previous one's last save.
        write_at(&config.join("network.json"), "later save", 200);
        write_at(&legacy.join("network.json"), "older", 100);
        assert_eq!(
            readable(config, "encoding.json"),
            legacy.join("encoding.json")
        );
        assert_eq!(
            readable(config, "network.json"),
            config.join("network.json")
        );

        move_legacy(config).unwrap();

        assert_eq!(read(&config.join("encoding.json")), "later save");
        assert_eq!(read(&config.join("network.json")), "later save");
        assert_eq!(
            readable(config, "encoding.json"),
            config.join("encoding.json")
        );
        assert_eq!(
            readable(config, "network.json"),
            config.join("network.json")
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_next_boot_removes_the_links_and_the_empty_folder() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path();
        write_at(&legacy_dir(config).join("encoding.json"), "moved", 100);
        let earlier = move_legacy(config).unwrap();
        assert!(earlier.is_empty(), "{earlier:?}");
        // The first boot keeps its links to the end: a previous release may
        // still be serving beside it.
        remove_links(config, &[]);
        assert!(legacy_dir(config).is_dir());

        let links = move_legacy(config).unwrap();
        assert_eq!(links, [legacy_dir(config).join("encoding.json")]);
        remove_links(config, &links);

        assert!(!legacy_dir(config).exists());
        assert_eq!(read(&config.join("encoding.json")), "moved");
    }

    #[cfg(unix)]
    #[test]
    fn a_save_over_a_link_is_moved_by_the_next_boot_and_kept_until_then() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path();
        write_at(&legacy_dir(config).join("livetv.json"), "first", 100);
        move_legacy(config).unwrap();
        // The previous release saves: its atomic write renames a file over
        // the link.
        let link = legacy_dir(config).join("livetv.json");
        let staged = legacy_dir(config).join(".staged");
        write_at(&staged, "saved beside", 200);
        std::fs::rename(&staged, &link).unwrap();
        assert_eq!(
            readable(config, "livetv.json"),
            link,
            "read until the next boot"
        );

        let links = move_legacy(config).unwrap();
        assert!(links.is_empty(), "a file, not a link");
        remove_links(config, &links);

        assert_eq!(read(&config.join("livetv.json")), "saved beside");
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_replaced_by_a_save_before_the_handover_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path();
        for file in ["encoding.json", "network.json"] {
            write_at(&legacy_dir(config).join(file), file, 100);
        }
        move_legacy(config).unwrap();
        let links = move_legacy(config).unwrap();
        assert_eq!(links.len(), 2, "{links:?}");
        // The previous release saves between this boot's move and its handover.
        let saved = legacy_dir(config).join("network.json");
        std::fs::remove_file(&saved).unwrap();
        write_at(&saved, "saved beside", 300);

        remove_links(config, &links);

        assert!(!legacy_dir(config).join("encoding.json").exists());
        assert_eq!(read(&saved), "saved beside", "left for the next boot");
        assert_eq!(readable(config, "network.json"), saved);
    }

    #[test]
    fn what_cannot_be_moved_stops_the_boot_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path();
        // A folder of that name is not a configuration: left alone.
        std::fs::create_dir_all(legacy_dir(config).join("branding.json")).unwrap();
        // A file that cannot replace what is there.
        write_at(&legacy_dir(config).join("encoding.json"), "saved", 200);
        std::fs::create_dir_all(config.join("encoding.json").join("in the way")).unwrap();

        let error = move_legacy(config).unwrap_err();

        assert!(error.to_string().contains("encoding.json"), "{error}");
        assert!(legacy_dir(config).join("branding.json").is_dir());
        assert_eq!(read(&legacy_dir(config).join("encoding.json")), "saved");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_legacy_folder_stops_the_boot_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        // `users` a file: reading `users/named` fails, and not as missing.
        std::fs::write(dir.path().join("users"), "").unwrap();

        let error = move_legacy(dir.path()).unwrap_err();

        assert!(error.to_string().contains("named"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_ferrofin_did_not_make_stops_the_boot() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path();
        write_at(&config.join("shared/network.json"), "operator's", 100);
        std::fs::create_dir_all(legacy_dir(config)).unwrap();
        let link = legacy_dir(config).join("network.json");
        std::os::unix::fs::symlink(config.join("shared/network.json"), &link).unwrap();

        let error = move_legacy(config).unwrap_err();

        assert!(error.to_string().contains("did not make"), "{error}");
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert!(!config.join("network.json").exists());
    }

    #[test]
    fn a_fresh_install_has_nothing_to_move() {
        let dir = tempfile::tempdir().unwrap();
        let links = move_legacy(dir.path()).unwrap();
        assert!(links.is_empty(), "{links:?}");
        remove_links(dir.path(), &[]);
        assert_eq!(
            readable(dir.path(), "network.json"),
            dir.path().join("network.json")
        );
        assert!(!dir.path().join("users").exists());
    }
}
