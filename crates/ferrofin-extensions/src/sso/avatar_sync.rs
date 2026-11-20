//! Profile pictures from the identity provider (B16, D18): with a provider's
//! `SyncProfilePictures` on, a user's picture follows the `picture` claim of
//! each sign-in.
//!
//! The sync starts once the sign-in is complete, just after its session is
//! minted or the Quick Connect code it approved is authorized
//! ([`SsoService::start_avatar_sync`]), as a task of its own that the
//! sign-in never waits for. The task holds nothing of the request: it owns
//! the identity's issuer and subject, the account's id and the URL. It
//! 1. downloads the URL through the server's image downloads
//!    ([`RemoteImageFetcher`], `ExtensionContext::remote_images`), never
//!    through the identity provider's HTTP client: the URL comes from a
//!    claim, not from the admin, so it is held to the image limits (no
//!    private address unless `image_private_http_allow` lists it, the size
//!    cap, a body that starts like an image);
//! 2. stores it as the user's picture and records its avatar hash on the
//!    identity's link, unless the link's hash is the image's and the user
//!    still has a picture ([`replace_picture`], shared with LDAP-Auth).
//!
//! Step 2 runs under one lock and reads the link again, so of two sign-ins
//! close together the second finds the hash the first recorded and stores
//! nothing. A download that fails (a URL that cannot be fetched or is
//! refused, a timeout, a body that is no image) is one `warn` line and
//! changes nothing; the next sign-in tries again. A store that fails is one
//! `warn` line too; failing after the old picture is cleared (LDAP-Auth's
//! order), it leaves none until the next sign-in. A sign-in without the
//! claim, or with an empty one, leaves the picture as it is: no sign-in
//! removes it. A URL is logged by
//! its scheme and host only ([`quoted`]): identity providers put signed tokens
//! in its query.

use std::sync::Arc;

use ferrofin_model::sso::OidcProvider;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::external_identities::{ExternalIdentityKey, ExternalIdentityStore};
use ferrofin_traits::library::UserManager;
use ferrofin_traits::remote_images::RemoteImageFetcher;
use serde_json::{Map, Value};
use tracing::Instrument as _;
use uuid::Uuid;

use super::SsoService;
use super::accounts::PROVIDER_ID;
use super::sign_in::VerifiedIdentity;
use crate::profile_images::{quoted, replace_picture};

/// The claim a user's picture URL is read from (OpenID Connect Core 1.0,
/// 5.1).
pub const PICTURE_CLAIM: &str = "picture";

/// What the picture sync needs beside the service's accounts and links: the
/// server's image downloads, and the lock its stores take turns under.
#[derive(Clone)]
pub(super) struct AvatarSync {
    images: Arc<dyn RemoteImageFetcher>,
    storing: Arc<tokio::sync::Mutex<()>>,
}

/// Whether a sign-in at `provider` whose ID token holds `claims` asks the
/// userinfo endpoint for the picture: only with picture sync on and the claim
/// missing from the ID token.
pub(super) fn wants_userinfo(provider: &OidcProvider, claims: &Map<String, Value>) -> bool {
    provider.sync_profile_pictures && !claims.contains_key(PICTURE_CLAIM)
}

impl SsoService {
    /// The service, downloading the profile pictures of providers that sync
    /// them through `images`: the server's image downloads, which apply its
    /// image limits (`ExtensionContext::remote_images`). Without it, picture
    /// sync does nothing.
    #[must_use]
    pub fn with_remote_images(mut self, images: Arc<dyn RemoteImageFetcher>) -> Self {
        self.avatars = Some(AvatarSync {
            images,
            storing: Arc::default(),
        });
        self
    }

    /// Starts the picture sync of a completed sign-in of the account
    /// `user_id` by `identity` at `provider`, when the provider syncs
    /// pictures and the identity has a `picture` URL: a task of its own,
    /// answered so a test can wait for it. A sign-in drops it unawaited.
    pub(super) fn start_avatar_sync(
        &self,
        provider: &OidcProvider,
        identity: &VerifiedIdentity,
        user_id: Uuid,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if !provider.sync_profile_pictures {
            return None;
        }
        let Some(sync) = self.avatars.clone() else {
            tracing::debug!(oidc_provider = %provider.key, "no image downloads to sync profile pictures through");
            return None;
        };
        let url = identity.claims.get(PICTURE_CLAIM).and_then(Value::as_str);
        let Some(url) = url.filter(|url| !url.trim().is_empty()) else {
            tracing::debug!(oidc_provider = %provider.key, %user_id, "the sign-in has no picture claim; the profile picture is left as it is");
            return None;
        };
        let task = Task {
            sync,
            users: Arc::clone(&self.users),
            identities: Arc::clone(&self.identities),
            issuer: identity.issuer.clone(),
            subject: identity.subject.clone(),
            user_id,
            url: url.to_owned(),
        };
        // A root of its own: the request it started from has answered.
        let span = tracing::info_span!(
            parent: None,
            "oidc_avatar_sync",
            oidc_provider = %provider.key,
            %user_id,
            trigger = "api"
        );
        Some(tokio::spawn(task.run().instrument(span)))
    }
}

/// One picture sync, owning everything it reads.
struct Task {
    sync: AvatarSync,
    users: Arc<dyn UserManager>,
    identities: Arc<dyn ExternalIdentityStore>,
    issuer: String,
    subject: String,
    user_id: Uuid,
    url: String,
}

impl Task {
    /// Downloads the picture and stores it ([`store`](Self::store)); a
    /// failure is logged here, once.
    async fn run(self) {
        let bytes = match self.sync.images.fetch_image(&self.url).await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(
                    url = %quoted(&self.url),
                    %error,
                    "the picture claim gave no image; the profile picture is left as it is"
                );
                return;
            }
        };
        if let Err(error) = self.store(&bytes).await {
            tracing::warn!(%error, "the picture from the picture claim cannot be stored");
        }
    }

    /// Stores `bytes` as the account's picture unless its link already
    /// records them, reading the link again under the lock, so that a store
    /// never acts on a hash another sign-in's store has replaced.
    ///
    /// shortcut: of two sign-ins whose pictures differ, the one whose
    /// download ends last is kept until the next sign-in; order them by
    /// sign-in if a provider is seen to change pictures between sign-ins
    /// seconds apart.
    async fn store(&self, bytes: &[u8]) -> Result<(), ServiceError> {
        let _storing = self.sync.storing.lock().await;
        let identity = ExternalIdentityKey {
            provider_id: PROVIDER_ID,
            issuer: &self.issuer,
            subject: &self.subject,
        };
        let link = self.identities.find(identity).await?;
        let Some(link) = link.filter(|link| link.user_id == self.user_id) else {
            tracing::debug!(
                "the identity is no longer linked to the account; its picture is not stored"
            );
            return Ok(());
        };
        let Some(user) = self.users.get_user_by_id(self.user_id).await? else {
            tracing::debug!("the account is gone; its picture is not stored");
            return Ok(());
        };
        let has_picture = self.users.get_profile_image(self.user_id).await?.is_some();
        let stored = replace_picture(
            self.users.as_ref(),
            self.identities.as_ref(),
            identity,
            &user,
            has_picture,
            link.avatar_hash.as_deref(),
            bytes,
        )
        .await?;
        if stored {
            tracing::debug!("stored the profile picture from the picture claim");
        } else {
            tracing::debug!("the profile picture is already up to date");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
