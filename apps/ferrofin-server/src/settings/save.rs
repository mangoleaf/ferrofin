//! Saving the settings page's changes to `config.toml`
//! (`POST /Ferrofin/Settings`, through [`FerrofinServerSettings`]'s `update`).
//!
//! A save is all or nothing. First every change is checked: its key is a
//! setting the page may change (not read-only or environment-only, and not
//! set by a flag or a variable: owner decision D1), and its value is one of
//! the row's kind that meets the row's own rule ([`SettingSpec::check`]).
//! Then the file is edited as a TOML document, so its comments, its layout and
//! every key the save does not name stay as they were, and the result is
//! written to a new file beside it. That file is loaded by the boot loader
//! ([`Config::load_from`]) under the process environment and the server's
//! flags, so the boot rules decide, and only a file that loads is renamed over
//! `config.toml`. An existing file keeps its mode; a new one is created
//! owner-only, since it may hold provider keys.
//!
//! No message here quotes a value: a value may be a key or a password (owner
//! decision D3). The loader's own messages quote only the settings whose rule
//! they state, never a secret.
//!
//! [`FerrofinServerSettings`]: super::FerrofinServerSettings

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ferrofin_traits::error::ServiceError;
use serde_json::Value;
use toml_edit::DocumentMut;

use super::{SETTINGS, SettingKind, SettingSpec, locked_reason, no_longer_loads, source};
use crate::config::{self, Cli, Config, Env};

/// The mode of a `config.toml` a save creates: owner-only, since the file may
/// hold provider keys and the seed password.
#[cfg(unix)]
const NEW_FILE_MODE: u32 = 0o600;

/// Saves `changes` to `running`'s `config.toml`: a value sets its key, `None`
/// removes it. Returns the rows whose entry changed, in key order; when none
/// did, nothing is written.
///
/// # Errors
///
/// [`ServiceError::InvalidInput`] naming the key for a change refused before
/// anything is written, and for a file the loader refuses; and
/// [`ServiceError::Conflict`] naming the path when the file cannot be read,
/// no longer parses, or cannot be written. Nothing changes on disk either way.
pub(super) fn save(
    running: &Config,
    env: &dyn Env,
    changes: &BTreeMap<String, Option<Value>>,
) -> Result<Vec<&'static SettingSpec>, ServiceError> {
    let shown = running.config_file.as_path();
    let file_keys = config::config_file_keys(shown).map_err(|err| no_longer_loads(shown, &err))?;
    let edits = changes
        .iter()
        .map(|(key, value)| checked(running, env, &file_keys, key, value.as_ref()))
        .collect::<Result<Vec<_>, _>>()?;

    let target = write_target(shown)?;
    let mut document = read_document(&target, shown)?;
    let mut edited = Vec::new();
    for (spec, value) in edits {
        if apply(&mut document, spec.key, value) {
            edited.push(spec);
        }
    }
    if edited.is_empty() {
        return Ok(edited);
    }

    ensure_writable(&target, shown)?;
    let staged = Staged::write(&target, shown, document.to_string().as_bytes())?;
    validate(running, env, &staged.path, shown)?;
    staged.commit(&target, shown)?;
    Ok(edited)
}

/// The row of `key` and its `config.toml` value (`None` to remove it), once
/// the page may change it and `value` is one of its kind.
fn checked(
    running: &Config,
    env: &dyn Env,
    file_keys: &[String],
    key: &str,
    value: Option<&Value>,
) -> Result<(&'static SettingSpec, Option<toml_edit::Value>), ServiceError> {
    let spec = SETTINGS
        .iter()
        .find(|spec| spec.key == key)
        .ok_or_else(|| ServiceError::invalid_input(format!("`{key}` is not a Ferrofin setting")))?;
    if let Some(reason) = locked_reason(spec, source(spec, &running.cli, env, file_keys), env) {
        return Err(ServiceError::invalid_input(format!(
            "`{key}` cannot be changed here: {reason}"
        )));
    }
    let value = value
        .map(|value| to_toml(spec, value))
        .transpose()
        .map_err(|why| ServiceError::invalid_input(format!("`{key}` {why}")))?;
    Ok((spec, value))
}

/// `value` as `spec`'s `config.toml` value, or why it is not one, in words
/// that follow the key and never quote the value.
fn to_toml(spec: &SettingSpec, value: &Value) -> Result<toml_edit::Value, String> {
    let toml = match spec.kind {
        SettingKind::Bool => value
            .as_bool()
            .map(toml_edit::Value::from)
            .ok_or_else(|| "takes true or false".to_owned())?,
        SettingKind::Integer { min, max } => value
            .as_i64()
            .filter(|n| (min..=max).contains(n))
            .map(toml_edit::Value::from)
            .ok_or_else(|| format!("takes a whole number from {min} to {max}"))?,
        // Text is the loader's to judge: `auto`, and its message for anything else.
        SettingKind::CountOrAuto => match value {
            Value::String(text) => toml_edit::Value::from(text.as_str()),
            _ => value
                .as_i64()
                .map(toml_edit::Value::from)
                .ok_or_else(|| "takes a whole number or \"auto\"".to_owned())?,
        },
        SettingKind::Text | SettingKind::Secret | SettingKind::Path => {
            toml_edit::Value::from(text(value)?)
        }
        // Trimmed: the providers take the value as it is, and only an empty
        // one as the default.
        SettingKind::Url => {
            let url = text(value)?.trim();
            if !url.is_empty() && !is_web_url(url) {
                return Err("takes an http or https URL, or nothing for the default".to_owned());
            }
            toml_edit::Value::from(url)
        }
        SettingKind::PathList => value
            .as_array()
            .and_then(|list| list.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
            .map(|paths| toml_edit::Value::Array(paths.into_iter().collect()))
            .ok_or_else(|| "takes a list of paths".to_owned())?,
        SettingKind::NumberList => value
            .as_array()
            .and_then(|list| list.iter().map(Value::as_f64).collect::<Option<Vec<_>>>())
            .map(|numbers| toml_edit::Value::Array(numbers.into_iter().collect()))
            .ok_or_else(|| "takes a list of numbers".to_owned())?,
    };
    (spec.check)(value)?;
    Ok(toml)
}

/// `value`'s text, or why a text setting cannot take it.
fn text(value: &Value) -> Result<&str, String> {
    value.as_str().ok_or_else(|| "takes text".to_owned())
}

/// Whether `url` is an absolute `http` or `https` URL with a host.
fn is_web_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
}

/// Sets `key` to `value` in `document`, or removes it when `value` is `None`;
/// says whether the document changed.
///
/// A key already in the file keeps its place, its spelling and the comments
/// around it, the one at the end of its line included. A removed key's line
/// goes, the comment at its end with it; the comment lines above it stay, in
/// front of the next key, or at the end of the file when it was the last.
fn apply(document: &mut DocumentMut, key: &str, value: Option<toml_edit::Value>) -> bool {
    let Some(mut value) = value else {
        return remove(document, key);
    };
    match document.get_mut(key) {
        Some(item) => {
            if let Some(old) = item.as_value() {
                *value.decor_mut() = old.decor().clone();
            }
            *item = toml_edit::Item::Value(value);
        }
        None => {
            document.insert(key, toml_edit::Item::Value(value));
        }
    }
    true
}

/// [`apply`]'s removal of `key`.
fn remove(document: &mut DocumentMut, key: &str) -> bool {
    let table = document.as_table_mut();
    let Some(index) = table.iter().position(|(name, _)| name == key) else {
        return false;
    };
    let Some((removed, _)) = table.remove_entry(key) else {
        return false;
    };
    let above = removed
        .leaf_decor()
        .prefix()
        .and_then(|raw| raw.as_str())
        .unwrap_or_default();
    if above.trim().is_empty() {
        return true;
    }
    let next = table
        .iter_mut()
        .skip(index)
        .find(|(_, item)| item.is_value())
        .map(|(mut next, _)| {
            let decor = next.leaf_decor_mut();
            let own = decor
                .prefix()
                .and_then(|raw| raw.as_str())
                .unwrap_or_default();
            let joined = format!("{above}{own}");
            decor.set_prefix(joined);
        });
    if next.is_none() {
        let trailing = document.trailing().as_str().unwrap_or_default();
        let trailing = format!("{above}{trailing}");
        document.set_trailing(trailing);
    }
    true
}

/// The file a save of `path` writes: `path`, or the file it links to, so a
/// linked `config.toml` stays a link.
///
/// # Errors
///
/// [`ServiceError::Conflict`] for a link that cannot be followed.
pub(super) fn write_target(path: &Path) -> Result<PathBuf, ServiceError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(path).map_err(|err| {
            ServiceError::conflict(format!(
                "`{}` is a link that cannot be followed: {err}",
                path.display()
            ))
        }),
        _ => Ok(path.to_path_buf()),
    }
}

/// Whether `path` exists and is read-only (no write permission for anyone),
/// which a save respects even when the server could write it anyway.
pub(super) fn is_read_only(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().readonly())
}

/// `shown`, with the file it links to when the save writes that one instead.
fn named(target: &Path, shown: &Path) -> String {
    if target == shown {
        format!("`{}`", shown.display())
    } else {
        format!("`{}` (a link to `{}`)", shown.display(), target.display())
    }
}

/// `target` read as a TOML document: an empty one when it does not exist.
fn read_document(target: &Path, shown: &Path) -> Result<DocumentMut, ServiceError> {
    match std::fs::read_to_string(target) {
        // The parser's message quotes the line at fault, which may hold a secret.
        Ok(text) => text.parse::<DocumentMut>().map_err(|err| {
            let why = config::without_source_lines(&err.to_string());
            no_longer_loads(shown, &anyhow::anyhow!(why))
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(err) => Err(ServiceError::conflict(format!(
            "{} cannot be read: {err}",
            named(target, shown)
        ))),
    }
}

/// [`ServiceError::Conflict`] unless the existing `target` may be replaced: it
/// is not read-only, and the server may open it for writing (a file another
/// user owns, a read-only mount). A missing file is the directory's to allow
/// ([`Staged::write`]).
fn ensure_writable(target: &Path, shown: &Path) -> Result<(), ServiceError> {
    let refused = |why: String| {
        ServiceError::conflict(format!("{} cannot be written: {why}", named(target, shown)))
    };
    if is_read_only(target) {
        return Err(refused("it is read-only".to_owned()));
    }
    match std::fs::OpenOptions::new().write(true).open(target) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(refused(err.to_string())),
    }
}

/// Loads the configuration the server would start with on `staged` in place
/// of `config.toml`: `running`'s flags and data directory, and `env`.
fn validate(
    running: &Config,
    env: &dyn Env,
    staged: &Path,
    shown: &Path,
) -> Result<(), ServiceError> {
    let cli = Cli {
        data_dir: Some(running.data_dir.clone()),
        config_file: Some(staged.to_path_buf()),
        ..running.cli.clone()
    };
    Config::load_from(cli, env).map(drop).map_err(|err| {
        ServiceError::invalid_input(
            format!("{err:#}").replace(&staged.display().to_string(), &shown.display().to_string()),
        )
    })
}

/// The new `config.toml`, written beside the file it replaces and removed
/// again unless [`commit`](Self::commit) renames it over that file.
struct Staged {
    /// Where it is written: a hidden file named `*.toml`, the only name the
    /// loader reads as TOML.
    path: PathBuf,
    /// Set once it is renamed over `config.toml`.
    committed: bool,
}

impl Staged {
    /// Writes `contents` beside `target` with `target`'s mode, and its owner
    /// and group where the server may set them, or owner-only when `target`
    /// does not exist, and syncs it.
    fn write(target: &Path, shown: &Path, contents: &[u8]) -> Result<Self, ServiceError> {
        use std::io::Write as _;

        let dir = target
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let path = dir.join(format!(
            ".ferrofin-settings-save.{}.toml",
            uuid::Uuid::new_v4()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(NEW_FILE_MODE);
        }
        let mut file = options.open(&path).map_err(|err| {
            ServiceError::conflict(format!(
                "{} cannot be written: its directory `{}` takes no new file, which a save \
                 writes and renames over it: {err}",
                named(target, shown),
                dir.display()
            ))
        })?;
        let staged = Self {
            path,
            committed: false,
        };
        let written = (|| {
            // A rename would leave the server's owner and the umask's mode on
            // a file someone else keeps: a container's root saving a host user's
            // file. Only root may give a file away, so that part is best effort:
            // a member of the file's group can still keep the group.
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
                let existing = std::fs::metadata(target).ok();
                if let Some(meta) = &existing
                    && let Err(err) =
                        std::os::unix::fs::fchown(&file, Some(meta.uid()), Some(meta.gid()))
                            .or_else(|_| std::os::unix::fs::fchown(&file, None, Some(meta.gid())))
                {
                    tracing::debug!(path = %shown.display(), error = %err, "could not keep the settings file's owner or group");
                }
                let mode = existing.map_or(NEW_FILE_MODE, |meta| meta.mode() & 0o777);
                file.set_permissions(std::fs::Permissions::from_mode(mode))?;
            }
            file.write_all(contents)?;
            file.sync_all()
        })();
        written.map_err(|err| {
            ServiceError::conflict(format!("{} cannot be written: {err}", named(target, shown)))
        })?;
        Ok(staged)
    }

    /// Renames the file over `target`, then syncs the directory, so the new
    /// `config.toml` survives a crash.
    fn commit(mut self, target: &Path, shown: &Path) -> Result<(), ServiceError> {
        std::fs::rename(&self.path, target).map_err(|err| {
            ServiceError::conflict(format!("{} cannot be written: {err}", named(target, shown)))
        })?;
        self.committed = true;
        if let Some(dir) = target.parent().filter(|dir| !dir.as_os_str().is_empty())
            && let Err(err) = std::fs::File::open(dir).and_then(|dir| dir.sync_all())
        {
            tracing::debug!(path = %dir.display(), error = %err, "could not sync the settings directory");
        }
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.committed
            && let Err(err) = std::fs::remove_file(&self.path)
        {
            tracing::warn!(path = %self.path.display(), error = %err, "could not remove a settings file a save left");
        }
    }
}

#[cfg(test)]
mod tests;
