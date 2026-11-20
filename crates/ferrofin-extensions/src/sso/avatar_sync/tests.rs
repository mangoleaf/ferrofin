//! Profile pictures from the `picture` claim (B16, D18), over the fake
//! identity provider, fake image downloads, the real user manager's profile
//! images and the real identity store. No network: a picture URL is answered
//! by the fake downloads.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use ferrofin_traits::external_identities::{ExternalIdentity, ExternalIdentityKey};
use ferrofin_traits::remote_images::RemoteImageError;
use serde_json::{Value, json};

use super::*;
use crate::ldap_auth::provider::profile_image::FakeImages;
use crate::profile_images::avatar_hash;
use crate::sso::fake_idp::{FakeResponse, SUBJECT, USERINFO_PATH, USERNAME};
use crate::sso::sign_in::tests::{Harness, PUBLIC_URL};

/// A JPEG's first bytes, which an image sniffer takes for one.
const JPEG: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
    0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
];
const OTHER_JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x04, 0x03, 0x04, 0xFF, 0xD9];
/// A picture URL with a signed token in its query, as identity providers
/// hand them out.
const PICTURE_URL: &str = "https://pictures.example.org/avatar/max.jpg?sig=signed-token-55";

/// The service syncing the fake provider's pictures through `images`, and
/// the fake's sign-ins carrying [`PICTURE_URL`].
async fn syncing(images: Arc<dyn RemoteImageFetcher>) -> Harness {
    let mut harness = Harness::with(Some(PUBLIC_URL), |provider| {
        provider.sync_profile_pictures = true;
    })
    .await;
    harness.sso = harness.sso.with_remote_images(images);
    picture_claim(&harness, json!(PICTURE_URL));
    harness
}

/// The fake's next ID tokens carry `picture` (`null` removes the claim).
fn picture_claim(harness: &Harness, picture: Value) {
    harness.fake.sign_in(|sign_in| {
        sign_in.claims.insert(PICTURE_CLAIM.to_owned(), picture);
    });
}

/// The fake's identity with `claims`, as a callback verifies it.
fn identity(harness: &Harness, claims: Value) -> VerifiedIdentity {
    let Value::Object(claims) = claims else {
        panic!("claims are an object");
    };
    VerifiedIdentity {
        provider_key: "fake".to_owned(),
        issuer: harness.fake.issuer(),
        subject: SUBJECT.to_owned(),
        username: Some(USERNAME.to_owned()),
        claims,
    }
}

impl Harness {
    /// A first sign-in without a picture claim, which creates and links the
    /// account and starts no sync: the account's id.
    async fn account_without_picture(&self) -> Uuid {
        picture_claim(self, Value::Null);
        self.session("first-device").await.expect("signed in");
        self.account_id().await.expect("the account")
    }

    async fn account_id(&self) -> Option<Uuid> {
        let user = self
            .users
            .get_user_by_name(USERNAME)
            .await
            .expect("lookup")?;
        Some(Uuid::parse_str(&user.id).expect("a user id"))
    }

    /// The sync a completed sign-in with `picture` as its claim starts,
    /// waited for.
    async fn sync(&self, user_id: Uuid, picture: Value) {
        let provider = self.provider("fake").await;
        let identity = identity(self, json!({ PICTURE_CLAIM: picture }));
        self.sso
            .start_avatar_sync(&provider, &identity, user_id)
            .expect("a sync starts")
            .await
            .expect("the sync ends");
    }

    /// The bytes of the account's picture, or `None` without one.
    async fn picture(&self, user_id: Uuid) -> Option<Vec<u8>> {
        let image = self
            .users
            .get_profile_image(user_id)
            .await
            .expect("profile image")?;
        Some(std::fs::read(&image.path).expect("read the picture"))
    }

    async fn link(&self) -> ExternalIdentity {
        self.identities
            .find(ExternalIdentityKey {
                provider_id: PROVIDER_ID,
                issuer: &self.fake.issuer(),
                subject: SUBJECT,
            })
            .await
            .expect("lookup")
            .expect("linked")
    }

    /// Waits up to five seconds for the account's picture to be `bytes`.
    async fn picture_becomes(&self, user_id: Uuid, bytes: &[u8]) {
        let waited = tokio::time::timeout(Duration::from_secs(5), async {
            while self.picture(user_id).await.as_deref() != Some(bytes) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(waited.is_ok(), "the picture was never stored");
    }
}

/// Fake downloads that answer every URL with `bytes`, once as many fetches
/// as `gate` counts have all started: so concurrent syncs all hold their
/// download before any of them stores it.
struct Gated {
    gate: tokio::sync::Barrier,
    bytes: Vec<u8>,
}

#[async_trait]
impl RemoteImageFetcher for Gated {
    async fn fetch_image(&self, _url: &str) -> Result<Vec<u8>, RemoteImageError> {
        self.gate.wait().await;
        Ok(self.bytes.clone())
    }
}

/// A picture host that never answers.
struct Hanging;

#[async_trait]
impl RemoteImageFetcher for Hanging {
    async fn fetch_image(&self, _url: &str) -> Result<Vec<u8>, RemoteImageError> {
        std::future::pending().await
    }
}

/// The identity store, counting the avatar hashes written.
struct CountingLinks {
    inner: Arc<dyn ExternalIdentityStore>,
    hash_writes: AtomicUsize,
}

#[async_trait]
impl ExternalIdentityStore for CountingLinks {
    async fn find(
        &self,
        identity: ExternalIdentityKey<'_>,
    ) -> Result<Option<ExternalIdentity>, ServiceError> {
        self.inner.find(identity).await
    }
    async fn list(&self, provider_id: &str) -> Result<Vec<ExternalIdentity>, ServiceError> {
        self.inner.list(provider_id).await
    }
    async fn link(
        &self,
        user_id: Uuid,
        identity: ExternalIdentityKey<'_>,
        avatar_hash: Option<&str>,
    ) -> Result<ExternalIdentity, ServiceError> {
        self.inner.link(user_id, identity, avatar_hash).await
    }
    async fn set_avatar_hash(
        &self,
        identity: ExternalIdentityKey<'_>,
        avatar_hash: Option<&str>,
    ) -> Result<bool, ServiceError> {
        self.hash_writes.fetch_add(1, Ordering::SeqCst);
        self.inner.set_avatar_hash(identity, avatar_hash).await
    }
    async fn unlink(&self, identity: ExternalIdentityKey<'_>) -> Result<bool, ServiceError> {
        self.inner.unlink(identity).await
    }
}

/// B16: a sign-in whose ID token names a picture gets it, downloaded through
/// the server's image downloads after the session is minted, and its hash
/// recorded on the link.
#[tokio::test]
async fn a_sign_in_sets_the_picture_its_claim_names() {
    let images = FakeImages::default();
    images.answer(PICTURE_URL, Ok(JPEG.to_vec()));
    let harness = syncing(Arc::new(images.clone())).await;

    harness.session("first-device").await.expect("signed in");
    let user_id = harness.account_id().await.expect("the account");
    harness.picture_becomes(user_id, JPEG).await;
    assert_eq!(harness.link().await.avatar_hash, Some(avatar_hash(JPEG)));
    assert_eq!(images.fetched(), [PICTURE_URL]);
}

/// B16: the same image again stores nothing; a changed one replaces the
/// picture and its hash.
#[tokio::test]
async fn an_unchanged_picture_is_not_stored_again_and_a_changed_one_replaces_it() {
    let images = FakeImages::default();
    images.answer(PICTURE_URL, Ok(JPEG.to_vec()));
    let harness = syncing(Arc::new(images.clone())).await;
    let user_id = harness.account_without_picture().await;
    harness.sync(user_id, json!(PICTURE_URL)).await;
    assert_eq!(harness.picture(user_id).await.as_deref(), Some(JPEG));

    // A store of the same image would write the file again.
    let stored = harness
        .users
        .get_profile_image(user_id)
        .await
        .expect("lookup")
        .expect("a picture");
    std::fs::write(&stored.path, b"untouched").expect("mark the file");
    harness.sync(user_id, json!(PICTURE_URL)).await;
    assert_eq!(
        harness.picture(user_id).await.as_deref(),
        Some(&b"untouched"[..])
    );
    assert_eq!(images.fetched().len(), 2, "downloaded both times");

    images.answer(PICTURE_URL, Ok(OTHER_JPEG.to_vec()));
    harness.sync(user_id, json!(PICTURE_URL)).await;
    assert_eq!(harness.picture(user_id).await.as_deref(), Some(OTHER_JPEG));
    assert_eq!(
        harness.link().await.avatar_hash,
        Some(avatar_hash(OTHER_JPEG))
    );
}

/// The pitfall of two sign-ins close together: both downloads finish before
/// either stores, and the second store finds the hash the first recorded.
#[tokio::test]
async fn two_syncs_at_once_store_the_picture_once() {
    let mut harness = syncing(Arc::new(FakeImages::default())).await;
    let user_id = harness.account_without_picture().await;
    let links = Arc::new(CountingLinks {
        inner: Arc::clone(&harness.identities) as Arc<dyn ExternalIdentityStore>,
        hash_writes: AtomicUsize::new(0),
    });
    harness.sso = SsoService::new(
        Arc::clone(&harness.sso.plugins),
        Arc::clone(&harness.sso.users),
        Arc::clone(&links) as Arc<dyn ExternalIdentityStore>,
        Arc::clone(&harness.sso.sessions),
        Arc::clone(&harness.sso.activity),
        Some(PUBLIC_URL.to_owned()),
    )
    .with_remote_images(Arc::new(Gated {
        gate: tokio::sync::Barrier::new(2),
        bytes: JPEG.to_vec(),
    }));
    let provider = harness.provider("fake").await;
    let identity = identity(&harness, json!({ PICTURE_CLAIM: PICTURE_URL }));
    let first = harness.sso.start_avatar_sync(&provider, &identity, user_id);
    let second = harness.sso.start_avatar_sync(&provider, &identity, user_id);
    let (first, second) = tokio::join!(first.expect("a sync"), second.expect("a sync"));
    first.expect("the first ends");
    second.expect("the second ends");

    assert_eq!(links.hash_writes.load(Ordering::SeqCst), 1);
    assert_eq!(harness.picture(user_id).await.as_deref(), Some(JPEG));
}

/// B16: a refused address, a host that cannot be reached, a body that is no
/// image, one over the size limit, or a missing image leaves the picture and
/// its hash as they are.
#[tokio::test]
async fn a_failed_download_leaves_the_picture_as_it_is() {
    let images = FakeImages::default();
    images.answer(PICTURE_URL, Ok(JPEG.to_vec()));
    let harness = syncing(Arc::new(images.clone())).await;
    let user_id = harness.account_without_picture().await;
    harness.sync(user_id, json!(PICTURE_URL)).await;

    for error in [
        RemoteImageError::Refused,
        RemoteImageError::Unavailable,
        RemoteImageError::NotAnImage,
        RemoteImageError::TooLarge,
        RemoteImageError::Missing,
    ] {
        images.answer(PICTURE_URL, Err(error));
        harness.sync(user_id, json!(PICTURE_URL)).await;
        assert_eq!(
            harness.picture(user_id).await.as_deref(),
            Some(JPEG),
            "{error:?}"
        );
        assert_eq!(harness.link().await.avatar_hash, Some(avatar_hash(JPEG)));
    }
    // A URL no download can use fails the same way.
    harness.sync(user_id, json!("not a URL")).await;
    assert_eq!(harness.picture(user_id).await.as_deref(), Some(JPEG));
}

/// B16: a sign-in without the claim, or with one that is no string or
/// blank, starts nothing and removes nothing.
#[tokio::test]
async fn a_sign_in_without_a_picture_claim_keeps_the_picture() {
    let images = FakeImages::default();
    images.answer(PICTURE_URL, Ok(JPEG.to_vec()));
    let harness = syncing(Arc::new(images.clone())).await;
    let user_id = harness.account_without_picture().await;
    harness.sync(user_id, json!(PICTURE_URL)).await;

    let provider = harness.provider("fake").await;
    for claims in [
        json!({}),
        json!({ PICTURE_CLAIM: 7 }),
        json!({ PICTURE_CLAIM: " " }),
    ] {
        let identity = identity(&harness, claims);
        assert!(
            harness
                .sso
                .start_avatar_sync(&provider, &identity, user_id)
                .is_none()
        );
    }
    harness.session("second-device").await.expect("signed in");
    assert_eq!(harness.picture(user_id).await.as_deref(), Some(JPEG));
    assert_eq!(images.fetched(), [PICTURE_URL], "fetched once");
}

/// With the switch off, or no image downloads given, a sign-in never starts
/// a sync: nothing is fetched and no picture is set.
#[tokio::test]
async fn without_the_switch_or_the_downloads_nothing_runs() {
    let images = FakeImages::default();
    images.answer(PICTURE_URL, Ok(JPEG.to_vec()));
    let mut harness = Harness::new().await;
    harness.sso = harness.sso.with_remote_images(Arc::new(images.clone()));
    picture_claim(&harness, json!(PICTURE_URL));
    harness.session("first-device").await.expect("signed in");
    let user_id = harness.account_id().await.expect("the account");
    let off = harness.provider("fake").await;
    assert!(!off.sync_profile_pictures, "off by default");
    let identity = identity(&harness, json!({ PICTURE_CLAIM: PICTURE_URL }));
    assert!(
        harness
            .sso
            .start_avatar_sync(&off, &identity, user_id)
            .is_none()
    );

    let plain = Harness::with(Some(PUBLIC_URL), |provider| {
        provider.sync_profile_pictures = true;
    })
    .await;
    let on = plain.provider("fake").await;
    assert!(
        plain
            .sso
            .start_avatar_sync(&on, &identity, user_id)
            .is_none()
    );

    assert_eq!(images.fetched(), Vec::<String>::new());
    assert_eq!(harness.picture(user_id).await, None);
}

/// B16: a picture host that never answers holds neither the sign-in nor a
/// second one.
#[tokio::test]
async fn a_download_that_never_ends_holds_no_sign_in() {
    let harness = syncing(Arc::new(Hanging)).await;
    for device in ["first-device", "second-device"] {
        let signed_in = tokio::time::timeout(Duration::from_secs(10), harness.session(device))
            .await
            .expect("the sign-in does not wait for the picture");
        signed_in.expect("signed in");
    }
    let user_id = harness.account_id().await.expect("the account");
    assert_eq!(harness.picture(user_id).await, None);
}

/// The ID token may leave the picture out (some providers put profile
/// claims in userinfo only): with picture sync on, userinfo is asked, and
/// its failure fails no sign-in. Off, it is never asked for the picture.
#[tokio::test]
async fn a_picture_claim_only_userinfo_holds_is_read_from_it() {
    let userinfo_calls = |harness: &Harness| {
        harness
            .fake
            .requests()
            .iter()
            .filter(|request| request.path == harness.fake.path(USERINFO_PATH))
            .count()
    };
    let images = FakeImages::default();
    images.answer(PICTURE_URL, Ok(JPEG.to_vec()));
    let harness = syncing(Arc::new(images.clone())).await;
    picture_claim(&harness, Value::Null);
    harness.fake.sign_in(|sign_in| {
        sign_in
            .userinfo
            .insert(PICTURE_CLAIM.to_owned(), json!(PICTURE_URL));
    });
    harness.session("first-device").await.expect("signed in");
    let user_id = harness.account_id().await.expect("the account");
    harness.picture_becomes(user_id, JPEG).await;
    assert_eq!(userinfo_calls(&harness), 1);

    // A userinfo that fails leaves the claim out and the sign-in whole.
    harness
        .fake
        .set(&harness.fake.path(USERINFO_PATH), FakeResponse::status(500));
    harness.session("second-device").await.expect("signed in");
    assert_eq!(userinfo_calls(&harness), 2);
    // So does one for another subject.
    harness.fake.set(
        &harness.fake.path(USERINFO_PATH),
        FakeResponse::json(
            &json!({ "sub": "someone-else", PICTURE_CLAIM: "https://elsewhere.example.org/x.jpg" }),
        ),
    );
    harness.session("third-device").await.expect("signed in");
    assert_eq!(userinfo_calls(&harness), 3);
    assert_eq!(images.fetched(), [PICTURE_URL]);

    let plain = Harness::new().await;
    plain.session("first-device").await.expect("signed in");
    assert_eq!(userinfo_calls(&plain), 0, "no picture sync, no userinfo");
}

/// A link removed or moved to another account since the sign-in: the
/// picture is not stored on the account it signed in to.
#[tokio::test]
async fn an_identity_no_longer_linked_stores_nothing() {
    let images = FakeImages::default();
    images.answer(PICTURE_URL, Ok(JPEG.to_vec()));
    let harness = syncing(Arc::new(images.clone())).await;
    let user_id = harness.account_without_picture().await;
    harness
        .identities
        .unlink(ExternalIdentityKey {
            provider_id: PROVIDER_ID,
            issuer: &harness.fake.issuer(),
            subject: SUBJECT,
        })
        .await
        .expect("unlinked");
    harness.sync(user_id, json!(PICTURE_URL)).await;
    assert_eq!(images.fetched(), [PICTURE_URL]);
    assert_eq!(harness.picture(user_id).await, None);
}

/// A picture URL is logged by its scheme and host: the signed token in its
/// query stays out of the log.
#[test]
fn a_logged_picture_url_keeps_its_scheme_and_host_only() {
    assert_eq!(quoted(PICTURE_URL), "https://pictures.example.org");
    assert_eq!(
        quoted("data:image/png;base64,iVBORw0K"),
        "a value of 30 characters"
    );
}
