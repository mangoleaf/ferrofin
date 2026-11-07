//! Profile images from the directory (B12): a user's picture is the first
//! value of the `LdapProfileImageAttribute` attribute, read as
//! `LdapProfileImageFormat` says, and is taken at a new user's first sign-in
//! and kept current by the `LdapProfileImageSync` task while
//! `EnableLdapProfileImageSync` is on.
//!
//! Port of jellyfin-plugin-ldapauth v24 (`d11c86a`):
//! `LdapUtils.TryDetermineAttributeFormat` ([`detect_format`]), the
//! profile-image block of `LdapAuthenticationProviderPlugin.Authenticate`
//! ([`LdapAuthProvider::first_picture`]), `ProfileImageUpdater.SetProfileImage`
//! and `LdapProfileImageSyncTask` ([`ProfileImageSyncTask`]). Which picture a
//! user's link was last given is the link's avatar hash (upstream's
//! `ProfileImageHash`): the Base64 of the picture's MD5.
//!
//! Accepted divergences (see `docs/PLUGINS_UPSTREAM.md`):
//! - A URL is fetched through the server's image download seam
//!   ([`RemoteImageFetcher`]): a private address is refused, on the request and
//!   on any redirect, unless `image_private_http_allow` lists it, the body is
//!   read under `image_download_max_mb`, and it must start like an image.
//!   Upstream fetches it with the default `HttpClient`, with none of these.
//! - A picture that cannot be read (format detection fails, the Base64 does
//!   not decode, the download fails) is a warning for that user. The sign-in
//!   goes on without a picture and the task goes on to the next user, where
//!   upstream's exception fails the sign-in of a user it has just created, or
//!   stops the task. At sign-in a picture that cannot be stored is a warning
//!   too: the account exists by then.
//! - The task fails, naming the step, when the directory cannot be reached or
//!   refuses the configured bind DN, rather than warning about each user in
//!   turn after the connect timeout (D6) and reporting the run completed.
//!   Upstream catches that `AuthenticationException` per user and goes on. A
//!   search the directory fails for one user is still a warning.
//! - A new user is linked before their picture is read, and its hash is
//!   recorded once it is stored, so a concurrent sign-in finds them by their
//!   uid while a URL picture downloads. The download still runs inside the
//!   sign-in, as upstream's does, bounded by the image download limits.
//! - An error names a value that is no URL by its length only ([`quoted`]).
//!   Upstream quotes it whole.
//! - An entry without the picture attribute is logged at debug: most users
//!   have none, and the task would warn about each of them every day.
//! - The task skips a link whose user is gone (links go with their user, so
//!   only a deletion racing the task leaves one) and a link with an empty uid,
//!   which no search can find. Upstream fails on the first and searches for
//!   the second, which finds nothing.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD};
use ferrofin_core::{ScheduledTask, TaskProgress};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::tasks::{TaskTriggerInfo, TaskTriggerInfoType};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::external_identities::ExternalIdentity;
use ferrofin_traits::library::UserManager;
use ferrofin_traits::remote_images::{RemoteImageError, RemoteImageFetcher};
use ferrofin_util::source_url::redact_url;

use super::{LdapAuthProvider, PROVIDER_ID, locate_ldap_user};
use crate::ldap_auth::{BoundSearchError, LdapConfig, LdapEntry, LdapError, ProfileImageFormat};

/// The media type every directory picture is stored as: upstream's
/// `ProfileImageUpdater` saves each one as `profile.jpg` with
/// `MediaTypeNames.Image.Jpeg`, whatever its format.
const STORED_MIME_TYPE: &str = "image/jpeg";
/// The extension every directory picture is stored under (`profile.jpg`).
const STORED_EXTENSION: &str = ".jpg";

/// `Convert.FromBase64String`'s decoding: padding required, the unused bits of
/// the last character ignored.
const FROM_BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
);

/// Why [`detect_format`] could not tell what a value is: upstream's
/// `InvalidFormatException` messages, naming the value as [`quoted`] does.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum FormatError {
    /// Not even a relative URI.
    #[error(
        "ProfileImage Format detection failed. Attribute value is not a valid URI of any sort. \
         Got: {0}"
    )]
    NotAUri(String),
    /// A relative URI: it names no server.
    #[error(
        "ProfileImage Format detection failed. Expected absolute URI but attribute value appears \
         to be a relative path. Got: {0}"
    )]
    Relative(String),
    /// An absolute URI of another scheme than http or https.
    #[error(
        "ProfileImage Format detection failed. Attribute value was a valid URI but had an \
         invalid Scheme, expected one of [http, https]. Got: {0}"
    )]
    Scheme(String),
}

/// Why a picture attribute gave no picture.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PictureError {
    /// The format could not be told (`Default`).
    #[error(transparent)]
    Format(#[from] FormatError),
    /// The value is not Base64 (`Convert.FromBase64String`'s
    /// `FormatException`).
    #[error("the attribute value is not valid Base64")]
    Base64,
    /// The URL gave no image.
    #[error("the picture could not be fetched from {url}: {error}")]
    Fetch {
        /// The value fetched, named as [`quoted`] names it (a URL by its
        /// scheme and host).
        url: String,
        /// Why.
        error: RemoteImageError,
    },
}

/// `LdapUtils.TryDetermineAttributeFormat`: what a picture attribute's value
/// is, from its text (Novell's `StringValue`, the bytes read as UTF-8). A
/// control character other than tab, CR and LF, or DEL, means binary; else
/// text that is valid Base64 (`Base64.IsValid`) is Base64; else an absolute
/// http or https URL is a URL. Anything else is an error, not a guess.
pub(crate) fn detect_format(value: &[u8]) -> Result<ProfileImageFormat, FormatError> {
    let text = String::from_utf8_lossy(value);
    if text
        .chars()
        .any(|c| c == '\u{7f}' || (c < ' ' && !matches!(c, '\n' | '\r' | '\t')))
    {
        tracing::debug!("the picture attribute holds a control character: binary");
        return Ok(ProfileImageFormat::Binary);
    }
    if is_base64(&text) {
        tracing::debug!("the picture attribute is valid Base64");
        return Ok(ProfileImageFormat::Base64);
    }
    tracing::debug!("the picture attribute is neither binary nor Base64; looking for a URL");
    match url::Url::parse(&text) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => Ok(ProfileImageFormat::Url),
        Ok(url) => Err(FormatError::Scheme(url.scheme().to_owned())),
        Err(url::ParseError::RelativeUrlWithoutBase) => Err(FormatError::Relative(quoted(&text))),
        Err(_) => Err(FormatError::NotAUri(quoted(&text))),
    }
}

/// How an error names a value it could not read: a value with `://` as
/// [`redact_url`] leaves it (a URL by its scheme and host, a `file:` URL
/// whole), anything else by its length only. Upstream quotes the value whole,
/// but an attribute that is not the picture (a password hash, a whole encoded
/// image) does not belong in the log.
pub(crate) fn quoted(text: &str) -> String {
    if text.contains("://") {
        redact_url(text)
    } else {
        format!("a value of {} characters", text.chars().count())
    }
}

/// .NET's Base64 whitespace, which both `Base64.IsValid` and
/// `Convert.FromBase64String` skip wherever it is.
fn without_whitespace(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, ' ' | '\t' | '\r' | '\n'))
        .collect()
}

/// `Base64.IsValid` (.NET 10): the standard alphabet, a length that is a
/// multiple of four, at most two `=` and only at the end, and, before
/// padding, a last character whose unused bits are zero. Whitespace is
/// skipped and empty text is valid.
fn is_base64(text: &str) -> bool {
    STANDARD.decode(without_whitespace(text)).is_ok()
}

/// `Convert.FromBase64String`: as [`is_base64`] checks, except that the unused
/// bits of the last character are ignored.
fn from_base64(text: &str) -> Option<Vec<u8>> {
    FROM_BASE64.decode(without_whitespace(text)).ok()
}

/// The avatar hash of a picture, upstream's `ProfileImageHash`:
/// `Convert.ToBase64String(MD5.HashData(bytes))`.
pub(crate) fn avatar_hash(bytes: &[u8]) -> String {
    STANDARD.encode(ferrofin_common::extensions::md5_digest(bytes))
}

/// The first value of the entry's picture attribute (Novell's `ByteValue` and
/// `StringValue` read the first), or `None` when the entry has none.
fn picture_value<'a>(entry: &'a LdapEntry, attribute: &str) -> Option<&'a [u8]> {
    let value = entry
        .attribute(attribute)
        .and_then(|attribute| attribute.values.first())
        .map(Vec::as_slice);
    if value.is_none() {
        tracing::debug!(attribute = ?attribute, dn = ?entry.dn, "the LDAP entry has no picture attribute");
    }
    value
}

/// The picture a value holds in `format`: `Default` detects it
/// ([`detect_format`]), `Binary` is the bytes, `Base64` decodes the text and
/// `Url` fetches it through `images`.
pub(crate) async fn picture_bytes(
    value: &[u8],
    format: ProfileImageFormat,
    images: &dyn RemoteImageFetcher,
) -> Result<Vec<u8>, PictureError> {
    let format = match format {
        ProfileImageFormat::Default => detect_format(value)?,
        format => format,
    };
    match format {
        // Detection never answers `Default`.
        ProfileImageFormat::Binary | ProfileImageFormat::Default => Ok(value.to_vec()),
        ProfileImageFormat::Base64 => {
            from_base64(&String::from_utf8_lossy(value)).ok_or(PictureError::Base64)
        }
        ProfileImageFormat::Url => {
            let url = String::from_utf8_lossy(value);
            images
                .fetch_image(&url)
                .await
                .map_err(|error| PictureError::Fetch {
                    url: quoted(&url),
                    error,
                })
        }
    }
}

/// `ProfileImageUpdater.SetProfileImage`: stores `bytes` as the user's
/// `profile.jpg`.
async fn save_picture(
    users: &dyn UserManager,
    user: &UserEntity,
    bytes: &[u8],
) -> Result<(), ServiceError> {
    users
        .save_profile_image(user, bytes, STORED_MIME_TYPE, STORED_EXTENSION)
        .await
}

/// The task's error for a directory it cannot connect to (`step`
/// `connect`), or that refuses the configured bind DN: no user can be synced
/// then.
fn unreachable(step: &str, error: &LdapError) -> ServiceError {
    ServiceError::backend(format!("LDAP profile image sync could not {step}: {error}"))
}

impl LdapAuthProvider {
    /// The picture in `value`, read as the configuration says.
    async fn read_picture(
        &self,
        config: &LdapConfig,
        value: &[u8],
    ) -> Result<Vec<u8>, PictureError> {
        picture_bytes(
            value,
            config.ldap_profile_image_format,
            self.images.as_ref(),
        )
        .await
    }

    /// The profile-image block of `Authenticate`, for a user it has just
    /// created: with sync on and a picture in the entry, stores it and
    /// returns its avatar hash for the user's link. Any failure is a warning
    /// and no picture (see the module's divergences).
    pub(super) async fn first_picture(
        &self,
        config: &LdapConfig,
        entry: &LdapEntry,
        user: &UserEntity,
    ) -> Option<String> {
        if !config.enable_ldap_profile_image_sync {
            return None;
        }
        let value = picture_value(entry, &config.ldap_profile_image_attribute)?;
        let bytes = match self.read_picture(config, value).await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(
                    user_id = %user.id,
                    dn = ?entry.dn,
                    error = ?error.to_string(),
                    "the new LDAP user's picture cannot be read; they get none"
                );
                return None;
            }
        };
        if let Err(error) = save_picture(self.users.as_ref(), user, &bytes).await {
            tracing::warn!(
                user_id = %user.id,
                error = ?error.to_string(),
                "the new LDAP user's picture cannot be stored; they get none"
            );
            return None;
        }
        Some(avatar_hash(&bytes))
    }

    /// `LdapProfileImageSyncTask.ExecuteAsync`: while the plugin is enabled,
    /// names a server and has `EnableLdapProfileImageSync` on, brings every
    /// LDAP user's picture in step with the directory
    /// (`sync_picture`).
    ///
    /// # Errors
    /// The directory cannot be reached or refuses the configured bind DN, or
    /// the configuration, a user, a picture or a link cannot be read or
    /// written. A search, a picture or a download that fails for one user is
    /// a warning instead, and the next user is synced.
    pub async fn sync_profile_images(&self) -> Result<(), ServiceError> {
        let Some(config) = self.usable_config().await? else {
            tracing::debug!("LDAP-Auth is disabled or names no server; no profile images to sync");
            return Ok(());
        };
        if !config.enable_ldap_profile_image_sync {
            tracing::debug!("Synchronizing profile images is deactivated");
            return Ok(());
        }
        for link in self.identities.list(PROVIDER_ID).await? {
            self.sync_picture(&config, &link).await?;
        }
        Ok(())
    }

    /// One user of the task: finds their entry by their uid (`LocateLdapUser`
    /// with the uid as the name). With a picture there, it is stored unless
    /// the user has one with the same avatar hash, the old one cleared first.
    /// Without one, `RemoveImagesNotInLdap` clears the user's picture.
    async fn sync_picture(
        &self,
        config: &LdapConfig,
        link: &ExternalIdentity,
    ) -> Result<(), ServiceError> {
        let Some(user) = self.users.get_user_by_id(link.user_id).await? else {
            tracing::debug!(user_id = %link.user_id, "the LDAP link's user is gone");
            return Ok(());
        };
        let entry = match link.subject.as_str() {
            "" => None,
            uid => match locate_ldap_user(self.directory.as_ref(), config, uid).await {
                Ok(entry) => entry,
                Err(BoundSearchError::Search(error)) => {
                    tracing::warn!(
                        user_id = %link.user_id,
                        ldap_uid = ?link.subject,
                        error = ?error.to_string(),
                        "the directory failed the search for the user; cannot synchronize the \
                         profile image"
                    );
                    return Ok(());
                }
                // Every user after this one would fail the same way, each
                // after the connect timeout.
                Err(BoundSearchError::Connect(error)) => {
                    return Err(unreachable("connect", &error));
                }
                Err(BoundSearchError::Bind(error)) => {
                    return Err(unreachable("bind as the configured bind DN", &error));
                }
            },
        };
        let Some(entry) = entry else {
            tracing::warn!(
                user_id = %link.user_id,
                ldap_uid = ?link.subject,
                "the user is not found in LDAP; cannot synchronize the profile image"
            );
            return Ok(());
        };
        let has_picture = self.users.get_profile_image(link.user_id).await?.is_some();
        let Some(value) = picture_value(&entry, &config.ldap_profile_image_attribute) else {
            if config.remove_images_not_in_ldap && has_picture {
                tracing::debug!(user_id = %link.user_id, "removing the profile image the directory no longer has");
                self.users.clear_profile_image(&user).await?;
                self.identities
                    .set_avatar_hash(Self::identity(&link.subject), None)
                    .await?;
            }
            return Ok(());
        };
        let bytes = match self.read_picture(config, value).await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(
                    user_id = %link.user_id,
                    dn = ?entry.dn,
                    error = ?error.to_string(),
                    "the user's picture in LDAP cannot be read; their profile image is left as it is"
                );
                return Ok(());
            }
        };
        let hash = avatar_hash(&bytes);
        if has_picture && link.avatar_hash.as_deref() == Some(hash.as_str()) {
            tracing::debug!(user_id = %link.user_id, "the profile image is already up to date");
            return Ok(());
        }
        // Upstream's order: the old picture goes before the new one is
        // stored, so a store that fails leaves none, and the next run tries
        // again (the hash is written only after).
        if has_picture {
            self.users.clear_profile_image(&user).await?;
        }
        save_picture(self.users.as_ref(), &user, &bytes).await?;
        self.identities
            .set_avatar_hash(Self::identity(&link.subject), Some(&hash))
            .await?;
        tracing::debug!(user_id = %link.user_id, "synchronized the profile image from LDAP");
        Ok(())
    }
}

/// 100-nanosecond ticks in 24 hours, the task's default interval.
const DAY_TICKS: i64 = 24 * 3600 * 10_000_000;

/// The "LDAP - Synchronize profile images" task, `LdapProfileImageSyncTask`:
/// [`LdapAuthProvider::sync_profile_images`] every 24 hours.
pub(crate) struct ProfileImageSyncTask {
    provider: Arc<LdapAuthProvider>,
}

impl ProfileImageSyncTask {
    /// The task over `provider`, as the extension builds it.
    pub(crate) fn new(provider: Arc<LdapAuthProvider>) -> Self {
        Self { provider }
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for ProfileImageSyncTask {
    fn key(&self) -> &str {
        "LdapProfileImageSync"
    }

    fn name(&self) -> &str {
        "LDAP - Synchronize profile images"
    }

    fn description(&self) -> &str {
        "Synchronizes user profile images from LDAP."
    }

    /// `TasksApplicationCategory`, as the server's own tasks give it.
    fn category(&self) -> &str {
        "Application"
    }

    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        vec![TaskTriggerInfo {
            type_: TaskTriggerInfoType::IntervalTrigger,
            interval_ticks: Some(DAY_TICKS),
            ..TaskTriggerInfo::default()
        }]
    }

    async fn execute(&self, _progress: &TaskProgress) -> Result<(), ServiceError> {
        self.provider.sync_profile_images().await
    }
}

#[cfg(test)]
mod format_tests;

/// What each URL a [`FakeImages`] knows answers.
#[cfg(test)]
type ScriptedAnswers = std::collections::HashMap<String, Result<Vec<u8>, RemoteImageError>>;

/// A [`RemoteImageFetcher`] answering each URL as scripted, and
/// [`RemoteImageError::Unavailable`] for any other.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct FakeImages {
    answers: Arc<std::sync::Mutex<ScriptedAnswers>>,
    fetched: Arc<std::sync::Mutex<Vec<String>>>,
}

#[cfg(test)]
impl FakeImages {
    /// `url` answers `answer` from now on.
    pub(crate) fn answer(&self, url: &str, answer: Result<Vec<u8>, RemoteImageError>) {
        self.answers
            .lock()
            .expect("answers")
            .insert(url.to_owned(), answer);
    }

    /// Every URL fetched, in order.
    pub(crate) fn fetched(&self) -> Vec<String> {
        self.fetched.lock().expect("fetched").clone()
    }
}

#[cfg(test)]
#[async_trait]
impl RemoteImageFetcher for FakeImages {
    async fn fetch_image(&self, url: &str) -> Result<Vec<u8>, RemoteImageError> {
        self.fetched.lock().expect("fetched").push(url.to_owned());
        self.answers
            .lock()
            .expect("answers")
            .get(url)
            .cloned()
            .unwrap_or(Err(RemoteImageError::Unavailable))
    }
}
