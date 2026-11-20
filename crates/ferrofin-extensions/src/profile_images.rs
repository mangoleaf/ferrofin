//! A user's profile picture taken from an outside identity: LDAP-Auth's
//! directory picture ([`ldap_auth`](crate::ldap_auth)) and Single Sign-On's
//! `picture` claim ([`sso::avatar_sync`](crate::sso::avatar_sync)).
//!
//! Which picture a user's link was last given is the link's avatar hash
//! (LDAP-Auth's `ProfileImageHash`, jellyfin-plugin-ldapauth v24): the Base64
//! of the picture's MD5. A picture whose hash the link already holds is not
//! stored again.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::external_identities::{ExternalIdentityKey, ExternalIdentityStore};
use ferrofin_traits::library::UserManager;
use ferrofin_util::source_url::redact_url;

/// The media type every such picture is stored as: upstream's
/// `ProfileImageUpdater` saves each one as `profile.jpg` with
/// `MediaTypeNames.Image.Jpeg`, whatever its format.
const STORED_MIME_TYPE: &str = "image/jpeg";
/// The extension every such picture is stored under (`profile.jpg`).
const STORED_EXTENSION: &str = ".jpg";

/// The avatar hash of a picture, upstream's `ProfileImageHash`:
/// `Convert.ToBase64String(MD5.HashData(bytes))`.
pub(crate) fn avatar_hash(bytes: &[u8]) -> String {
    STANDARD.encode(ferrofin_common::extensions::md5_digest(bytes))
}

/// How an error names a value it could not read: a value with `://` as
/// [`redact_url`] leaves it (a URL by its scheme and host, a `file:` URL
/// whole), anything else by its length only. Upstream quotes the value whole,
/// but an attribute that is not the picture (a password hash, a whole encoded
/// image) does not belong in the log, and neither does a URL's query, where an
/// identity provider may put a signed token.
pub(crate) fn quoted(text: &str) -> String {
    if text.contains("://") {
        redact_url(text)
    } else {
        format!("a value of {} characters", text.chars().count())
    }
}

/// `ProfileImageUpdater.SetProfileImage`: stores `bytes` as the user's
/// `profile.jpg`.
pub(crate) async fn save_picture(
    users: &dyn UserManager,
    user: &UserEntity,
    bytes: &[u8],
) -> Result<(), ServiceError> {
    users
        .save_profile_image(user, bytes, STORED_MIME_TYPE, STORED_EXTENSION)
        .await
}

/// Stores `bytes` as `user`'s picture and records its avatar hash on
/// `identity`'s link, unless the user has a picture (`has_picture`) and the
/// link's hash (`recorded`) is the picture's. Returns whether it stored it.
///
/// Upstream's order: the old picture goes before the new one is stored, so a
/// store that fails leaves none, and the next sync tries again (the hash is
/// written only after).
pub(crate) async fn replace_picture(
    users: &dyn UserManager,
    identities: &dyn ExternalIdentityStore,
    identity: ExternalIdentityKey<'_>,
    user: &UserEntity,
    has_picture: bool,
    recorded: Option<&str>,
    bytes: &[u8],
) -> Result<bool, ServiceError> {
    let hash = avatar_hash(bytes);
    if has_picture && recorded == Some(hash.as_str()) {
        return Ok(false);
    }
    if has_picture {
        users.clear_profile_image(user).await?;
    }
    save_picture(users, user, bytes).await?;
    identities.set_avatar_hash(identity, Some(&hash)).await?;
    Ok(true)
}
