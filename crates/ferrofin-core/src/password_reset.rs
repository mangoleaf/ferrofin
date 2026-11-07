//! The built-in password-reset provider: a PIN file in the program data
//! directory.
//!
//! Port of `Jellyfin.Server.Implementations.Users.DefaultPasswordResetProvider`.
//! A forgotten-password request from the local network for an account that
//! exists writes `passwordreset<md5>.json` (the entered name's
//! invariant-uppercase UTF-16 MD5) holding a PIN valid for 30 minutes. Every
//! caller gets the same `PinCode` answer and file path, so the answer does not
//! tell which accounts exist. Redeeming the PIN sets the account's password to
//! the PIN as entered and removes the file.
//!
//! The provider borrows the user manager it belongs to, which looks the
//! account up and saves the password, so the two never hold each other.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::users::{ForgotPasswordAction, ForgotPasswordResult, PinRedeemResult};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::UserManager;
use ferrofin_traits::security::PasswordResetManager;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::user_manager::{DEFAULT_PASSWORD_RESET_PROVIDER_ID, FerrofinUserManager};

/// The built-in provider's display name (C# `Name`).
pub const DEFAULT_PASSWORD_RESET_PROVIDER_NAME: &str = "Default Password Reset Provider";

/// Filename prefix for a pending password-reset record under the data dir
/// (`passwordreset<md5>.json`). Port of C#
/// `DefaultPasswordResetProvider._passwordResetFileBase`.
const PASSWORD_RESET_PREFIX: &str = "passwordreset";

/// How long an issued forgot-password pin stays valid. Port of C#
/// `DefaultPasswordResetProvider` (30 minutes).
const PIN_TTL_MINUTES: i64 = 30;

/// The on-disk record of an in-progress forgot-password reset.
///
/// Port of C# `DefaultPasswordResetProvider.SerializablePasswordReset`, written
/// to the program-data directory when a pin is issued and consumed by
/// the `/Pin` redemption.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SerializablePasswordReset {
    /// When the pin expires.
    expiration_date: chrono::DateTime<chrono::Utc>,
    /// The issued pin (dash-grouped uppercase hex, e.g. `1A-2B-3C-4D`).
    pin: String,
    /// The absolute path of this record (echoed to the client as `PinFile`).
    pin_file: String,
    /// The username this reset belongs to.
    user_name: String,
}

/// Generates a fresh reset pin from 4 random bytes, formatted like C#'s
/// `BitConverter.ToString` (`1A-2B-3C-4D`).
///
/// ponytail: seeds from a v4 UUID's random bytes instead of pulling in a
/// dedicated RNG crate — 32 bits of entropy over a 30-minute window is ample for
/// a single-use, admin-visible reset pin.
fn generate_reset_pin() -> String {
    let b = Uuid::new_v4().into_bytes();
    format!("{:02X}-{:02X}-{:02X}-{:02X}", b[0], b[1], b[2], b[3])
}

/// Comparison ignores grouping dashes, but preserves case (C# Ordinal).
fn normalize_pin(pin: &str) -> String {
    pin.replace('-', "")
}

/// Every username receives the same response shape. Only a known local user
/// gets a recovery file; the path uses the entered name's invariant-uppercase
/// UTF-16 MD5, matching Jellyfin's built-in provider.
fn issue_reset_pin(
    data_dir: &Path,
    entered_username: &str,
    user_name: Option<&str>,
) -> Result<ForgotPasswordResult, ServiceError> {
    let expiration = chrono::Utc::now() + chrono::Duration::minutes(PIN_TTL_MINUTES);
    let hash = ferrofin_common::extensions::get_md5(
        &ferrofin_util::string_extensions::upper_invariant(entered_username),
    );
    let path = data_dir.join(format!("{PASSWORD_RESET_PREFIX}{}.json", hash.simple()));
    let pin_file = path.to_string_lossy().into_owned();
    if let Some(user_name) = user_name {
        let record = SerializablePasswordReset {
            expiration_date: expiration,
            pin: generate_reset_pin(),
            pin_file: pin_file.clone(),
            user_name: user_name.to_owned(),
        };
        std::fs::create_dir_all(data_dir)
            .map_err(|e| ServiceError::backend(format!("create reset directory: {e}")))?;
        let bytes = serde_json::to_vec(&record)
            .map_err(|e| ServiceError::backend(format!("serialize reset record: {e}")))?;
        ferrofin_util::file_helper::atomic_write(&path, &bytes)
            .map_err(|e| ServiceError::backend(format!("write reset record: {e}")))?;
    }
    Ok(ForgotPasswordResult {
        action: ForgotPasswordAction::PinCode,
        pin_file: Some(pin_file),
        pin_expiration_date: Some(expiration),
    })
}

/// Find matches and purge expired records. Matching files remain until their
/// password update succeeds, so a failed update does not consume recovery.
fn redeem_reset_pins(
    data_dir: &Path,
    entered_pin: &str,
) -> Result<Vec<(String, PathBuf)>, ServiceError> {
    let entered = normalize_pin(entered_pin);
    let mut matches = Vec::new();
    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(matches),
        Err(e) => return Err(ServiceError::backend(format!("read reset directory: {e}"))),
    };
    for entry in entries {
        let path = entry
            .map_err(|e| ServiceError::backend(format!("read reset entry: {e}")))?
            .path();
        if !path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(PASSWORD_RESET_PREFIX))
            || !path.is_file()
        {
            continue;
        }
        let bytes = std::fs::read(&path)
            .map_err(|e| ServiceError::backend(format!("read reset record: {e}")))?;
        let record = serde_json::from_slice::<Option<SerializablePasswordReset>>(&bytes)
            .map_err(|e| ServiceError::backend(format!("parse reset record: {e}")))?
            .ok_or_else(|| ServiceError::not_found("invalid reset record"))?;
        if record.expiration_date < chrono::Utc::now() {
            std::fs::remove_file(&path)
                .map_err(|e| ServiceError::backend(format!("remove expired reset: {e}")))?;
        } else if normalize_pin(&record.pin) == entered {
            matches.push((record.user_name, path));
        }
    }
    Ok(matches)
}

/// The built-in provider, borrowing the user manager it belongs to for one
/// request (C# `DefaultPasswordResetProvider`).
pub(crate) struct DefaultPasswordResetProvider<'a> {
    users: &'a FerrofinUserManager,
}

impl<'a> DefaultPasswordResetProvider<'a> {
    /// The provider over `users`, whose
    /// [`with_password_reset_dir`](FerrofinUserManager::with_password_reset_dir)
    /// names the PIN files' directory.
    pub(crate) fn new(users: &'a FerrofinUserManager) -> Self {
        Self { users }
    }

    fn dir(&self) -> Result<&'a Path, ServiceError> {
        self.users
            .password_reset_dir()
            .ok_or_else(|| ServiceError::backend("no password-reset directory is configured"))
    }
}

#[async_trait]
impl PasswordResetManager for DefaultPasswordResetProvider<'_> {
    fn name(&self) -> &str {
        DEFAULT_PASSWORD_RESET_PROVIDER_NAME
    }

    fn id(&self) -> &str {
        DEFAULT_PASSWORD_RESET_PROVIDER_ID
    }

    async fn is_enabled(&self) -> Result<bool, ServiceError> {
        Ok(true)
    }

    async fn start_forgot_password_process(
        &self,
        user: Option<&UserEntity>,
        entered_username: &str,
        is_in_network: bool,
    ) -> Result<ForgotPasswordResult, ServiceError> {
        // Only an account that exists, asked for from the local network, gets
        // a file; everyone gets the same answer.
        let user_name = user
            .filter(|_| is_in_network)
            .map(|user| user.username.as_str());
        let dir = self.dir()?;
        // A redemption never reads a record half written.
        let _guard = self.users.password_reset_lock().lock().await;
        issue_reset_pin(dir, entered_username, user_name)
    }

    /// No matching request answers `success` false, which the user manager
    /// turns into upstream's `ResourceNotFoundException` (404) once no other
    /// provider redeems the PIN. A matched record whose account is gone, or
    /// whose password cannot be saved, stops the redemption with that error
    /// and keeps its file, as upstream's throw does.
    async fn redeem_password_reset_pin(&self, pin: &str) -> Result<PinRedeemResult, ServiceError> {
        let dir = self.dir()?;
        // Held through the password save and the file's removal, so two
        // redemptions of one PIN reset the password once.
        let _guard = self.users.password_reset_lock().lock().await;
        let mut users_reset = Vec::new();
        for (user_name, path) in redeem_reset_pins(dir, pin)? {
            let user = self
                .users
                .get_user_by_name(&user_name)
                .await?
                .ok_or_else(|| ServiceError::not_found("user for recovery record"))?;
            let user_id = Uuid::parse_str(&user.id)
                .map_err(|error| ServiceError::backend(error.to_string()))?;
            self.users.change_password(user_id, pin).await?;
            self.users
                .log_password_changed(user_id, &user.username)
                .await;
            users_reset.push(user.username);
            std::fs::remove_file(path)
                .map_err(|e| ServiceError::backend(format!("remove consumed reset: {e}")))?;
        }
        Ok(PinRedeemResult {
            success: !users_reset.is_empty(),
            users_reset,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PASSWORD_RESET_PREFIX, SerializablePasswordReset, generate_reset_pin, issue_reset_pin,
        normalize_pin, redeem_reset_pins,
    };
    use ferrofin_model::users::ForgotPasswordAction;

    #[test]
    fn reset_pin_format_and_normalization() {
        let pin = generate_reset_pin();
        // `XX-XX-XX-XX`: 8 hex digits in four dash-separated pairs.
        let parts: Vec<&str> = pin.split('-').collect();
        assert_eq!(parts.len(), 4);
        assert!(
            parts
                .iter()
                .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
        );
        assert_eq!(normalize_pin("1a-2b"), "1a2b");
        assert_ne!(normalize_pin("AA-BB"), normalize_pin("aa-bb"));
    }

    #[test]
    fn issue_then_redeem_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let result = issue_reset_pin(dir.path(), "aLiCe", Some("alice")).unwrap();
        assert_eq!(result.action, ForgotPasswordAction::PinCode);
        assert!(result.pin_expiration_date.is_some());
        let file = std::path::PathBuf::from(result.pin_file.unwrap());
        let expected = dir.path().join(format!(
            "{PASSWORD_RESET_PREFIX}{}.json",
            ferrofin_common::extensions::get_md5("ALICE").simple()
        ));
        assert_eq!(file, expected);
        let record: SerializablePasswordReset =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        let found = redeem_reset_pins(dir.path(), "wrong").unwrap();
        assert!(found.is_empty(), "{found:?}");
        let found = redeem_reset_pins(dir.path(), "").unwrap();
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(
            redeem_reset_pins(dir.path(), &record.pin.replace('-', "")).unwrap(),
            vec![("alice".to_owned(), file.clone())]
        );
        assert!(
            file.exists(),
            "only a successfully applied reset consumes the file"
        );
        let absent = issue_reset_pin(dir.path(), "unknown", None).unwrap();
        assert_eq!(absent.action, ForgotPasswordAction::PinCode);
        assert!(!std::path::Path::new(&absent.pin_file.unwrap()).exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn expired_record_is_deleted_and_never_matches() {
        let dir = tempfile::tempdir().unwrap();
        let record = SerializablePasswordReset {
            expiration_date: chrono::Utc::now() - chrono::Duration::minutes(1),
            pin: "AA-BB-CC-DD".to_owned(),
            pin_file: String::new(),
            user_name: "bob".to_owned(),
        };
        let file = dir
            .path()
            .join(format!("{PASSWORD_RESET_PREFIX}user-2.json"));
        std::fs::write(&file, serde_json::to_vec(&record).unwrap()).unwrap();

        // Even with the correct pin, an expired record yields no match and is purged.
        let found = redeem_reset_pins(dir.path(), "AA-BB-CC-DD").unwrap();
        assert!(found.is_empty(), "{found:?}");
        assert!(!file.exists());
    }

    #[test]
    fn redeem_on_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let found = redeem_reset_pins(&missing, "AA-BB-CC-DD").unwrap();
        assert!(found.is_empty(), "{found:?}");
    }
}
