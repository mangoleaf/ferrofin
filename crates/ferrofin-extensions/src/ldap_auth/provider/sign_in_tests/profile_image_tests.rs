//! Profile images from the directory (B12): the picture a new user gets at
//! their first sign-in (`Authenticate`'s profile-image block) and the
//! `LdapProfileImageSync` task (`LdapProfileImageSyncTask`), jellyfin-plugin-ldapauth
//! v24 (`d11c86a`), over the fake directory, the real user manager's profile
//! images and the real identity store.

use ferrofin_core::ScheduledTask as _;
use ferrofin_traits::remote_images::RemoteImageError;

use super::super::super::ProfileImageFormat;
use super::super::profile_image::{self, ProfileImageSyncTask};
use super::*;

/// A JPEG's first bytes, which an image sniffer takes for one.
const JPEG: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
    0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
];
/// Base64 of [`JPEG`]'s MD5, upstream's `ProfileImageHash` (computed outside
/// the code under test).
const JPEG_HASH: &str = "V9pvLg8TnhGMeTCudlcEkQ==";

/// [`config`] with profile-image sync on.
fn sync_on() -> LdapConfig {
    LdapConfig {
        enable_ldap_profile_image_sync: true,
        ..config()
    }
}

/// `entry` with its picture attribute (upstream's default name) holding
/// `value`.
fn with_picture(mut entry: LdapEntry, value: &[u8]) -> LdapEntry {
    entry.attributes.push(LdapAttribute {
        name: "jpegPhoto".to_owned(),
        values: vec![value.to_vec()],
    });
    entry
}

impl Harness {
    /// The bytes of `user`'s profile image, or `None` without one.
    async fn picture(&self, user: &UserEntity) -> Option<Vec<u8>> {
        let image = self
            .users
            .get_profile_image(id_of(user))
            .await
            .expect("profile image")?;
        Some(std::fs::read(&image.path).expect("read the picture"))
    }
}

#[tokio::test]
async fn a_new_user_gets_the_picture_a_binary_attribute_holds() {
    let harness = Harness::new(&sync_on()).await;
    harness.answer(vec![with_picture(ada(), JPEG)]);

    let user = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");

    assert_eq!(harness.picture(&user).await.as_deref(), Some(JPEG));
    assert_eq!(
        harness
            .link("ada")
            .await
            .expect("linked")
            .avatar_hash
            .as_deref(),
        Some(JPEG_HASH)
    );
    let searched = harness.directory.state().searches[0].3.clone();
    assert_eq!(searched, ["uid", "uid", "jpegphoto"]);
}

/// Another picture, for a directory that changed it.
const OTHER_JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x04, 0x03, 0x04, 0xFF, 0xD9];
const PICTURE_URL: &str = "https://pictures.example.org/ada.jpg";

impl Harness {
    /// Signs `name` in for the first time, creating their account.
    async fn created(&self, name: &str, password: &str) -> UserEntity {
        self.sign_in(name, password)
            .await
            .expect("sign in")
            .expect("created")
    }

    /// The search for `name` answers `entry` from now on.
    fn answer_for(&self, name: &str, entry: LdapEntry) {
        let filter = user_search_filter(&config(), name);
        self.directory.state().by_filter.insert(
            filter,
            SearchOutcome {
                entries: vec![entry],
                references: Vec::new(),
            },
        );
    }

    /// Runs `LdapProfileImageSyncTask` as the scheduler does.
    async fn run_sync(&self) {
        ProfileImageSyncTask::new(Arc::new(self.provider()))
            .execute(&ferrofin_core::TaskProgress::default())
            .await
            .expect("sync");
    }

    /// Writes `bytes` over `user`'s stored picture file, leaving its row, so
    /// a test can tell whether the sync wrote the picture again.
    async fn mark_picture(&self, user: &UserEntity, bytes: &[u8]) {
        let image = self
            .users
            .get_profile_image(id_of(user))
            .await
            .expect("profile image")
            .expect("a picture");
        std::fs::write(&image.path, bytes).expect("mark the picture");
    }
}

#[tokio::test]
async fn a_new_user_gets_the_picture_a_base64_attribute_holds() {
    let harness = Harness::new(&LdapConfig {
        ldap_profile_image_format: ProfileImageFormat::Base64,
        ..sync_on()
    })
    .await;
    harness.answer(vec![with_picture(
        ada(),
        b"/9j/4AAQSkZJRgABAQAAAQABAAD/2Q==",
    )]);
    let user = harness.created("ada", ADA_PASSWORD).await;
    assert_eq!(harness.picture(&user).await.as_deref(), Some(JPEG));
    assert_eq!(
        harness
            .link("ada")
            .await
            .expect("linked")
            .avatar_hash
            .as_deref(),
        Some(JPEG_HASH)
    );
}

#[tokio::test]
async fn a_new_user_gets_the_picture_a_url_attribute_names() {
    let harness = Harness::new(&sync_on()).await;
    harness.images.answer(PICTURE_URL, Ok(JPEG.to_vec()));
    harness.answer(vec![with_picture(ada(), PICTURE_URL.as_bytes())]);
    let user = harness.created("ada", ADA_PASSWORD).await;
    assert_eq!(harness.picture(&user).await.as_deref(), Some(JPEG));
    assert_eq!(harness.images.fetched(), [PICTURE_URL]);
    assert_eq!(
        harness
            .link("ada")
            .await
            .expect("linked")
            .avatar_hash
            .as_deref(),
        Some(JPEG_HASH)
    );
}

/// A refused download, or a value no format fits, costs the picture and
/// never the sign-in (upstream's exception would fail it).
#[rstest]
#[case::a_refused_url(PICTURE_URL.as_bytes())]
#[case::no_format_fits(b"ada.jpg")]
#[tokio::test]
async fn a_picture_that_cannot_be_read_does_not_fail_the_sign_in(#[case] value: &[u8]) {
    let harness = Harness::new(&sync_on()).await;
    harness
        .images
        .answer(PICTURE_URL, Err(RemoteImageError::Refused));
    harness.answer(vec![with_picture(ada(), value)]);
    let user = harness.created("ada", ADA_PASSWORD).await;
    assert_eq!(harness.picture(&user).await, None);
    let link = harness.link("ada").await.expect("linked");
    assert_eq!(link.user_id, id_of(&user));
    assert_eq!(link.avatar_hash, None);
}

#[tokio::test]
async fn with_sync_off_the_picture_is_neither_asked_for_nor_taken() {
    let harness = Harness::new(&config()).await;
    harness.answer(vec![with_picture(ada(), JPEG)]);
    let user = harness.created("ada", ADA_PASSWORD).await;
    assert_eq!(harness.picture(&user).await, None);
    assert_eq!(harness.directory.state().searches[0].3, ["uid", "uid"]);

    // Nor does the task touch the directory.
    harness.directory.clear_calls();
    harness.run_sync().await;
    assert!(harness.directory.untouched());
}

/// Upstream sets the picture only when it creates the user; an existing
/// user's comes from the task.
#[tokio::test]
async fn a_later_sign_in_leaves_the_picture_to_the_task() {
    let harness = Harness::new(&sync_on()).await;
    harness.answer(vec![with_picture(ada(), JPEG)]);
    let user = harness.created("ada", ADA_PASSWORD).await;
    harness.answer(vec![with_picture(ada(), OTHER_JPEG)]);
    harness.created("ada", ADA_PASSWORD).await;
    assert_eq!(harness.picture(&user).await.as_deref(), Some(JPEG));
}

#[tokio::test]
async fn the_task_takes_a_changed_picture() {
    let harness = Harness::new(&sync_on()).await;
    harness.answer(vec![with_picture(ada(), JPEG)]);
    let user = harness.created("ada", ADA_PASSWORD).await;

    harness.answer(vec![with_picture(ada(), OTHER_JPEG)]);
    harness.run_sync().await;
    assert_eq!(harness.picture(&user).await.as_deref(), Some(OTHER_JPEG));
    let link = harness.link("ada").await.expect("linked");
    assert_eq!(
        link.avatar_hash.as_deref(),
        Some(profile_image::avatar_hash(OTHER_JPEG).as_str())
    );
    // The entry is found by the uid, through the user search filter, with
    // the picture attribute asked for.
    let searches = harness.directory.state().searches.clone();
    let sync_search = searches.last().expect("searched");
    assert_eq!(sync_search.2, user_search_filter(&sync_on(), "ada"));
    assert_eq!(sync_search.3, ["uid", "uid", "jpegphoto"]);
}

#[tokio::test]
async fn the_task_skips_an_unchanged_picture_but_restores_a_missing_one() {
    let harness = Harness::new(&sync_on()).await;
    harness.answer(vec![with_picture(ada(), JPEG)]);
    let user = harness.created("ada", ADA_PASSWORD).await;

    // Same hash and a picture stored: not written again.
    harness.mark_picture(&user, b"untouched").await;
    harness.run_sync().await;
    assert_eq!(
        harness.picture(&user).await.as_deref(),
        Some(&b"untouched"[..])
    );

    // Same hash but the user cleared their picture: stored again.
    harness
        .users
        .clear_profile_image(&user)
        .await
        .expect("clear");
    harness.run_sync().await;
    assert_eq!(harness.picture(&user).await.as_deref(), Some(JPEG));
}

#[rstest]
#[case::removed(true)]
#[case::kept(false)]
#[tokio::test]
async fn a_picture_the_directory_dropped_goes_only_with_remove_images_not_in_ldap(
    #[case] remove: bool,
) {
    let config = LdapConfig {
        remove_images_not_in_ldap: remove,
        ..sync_on()
    };
    let harness = Harness::new(&config).await;
    harness.answer(vec![with_picture(ada(), JPEG)]);
    let user = harness.created("ada", ADA_PASSWORD).await;

    harness.answer(vec![ada()]);
    harness.run_sync().await;
    let link = harness.link("ada").await.expect("linked");
    if remove {
        assert_eq!(harness.picture(&user).await, None);
        assert_eq!(link.avatar_hash, None);
    } else {
        assert_eq!(harness.picture(&user).await.as_deref(), Some(JPEG));
        assert_eq!(link.avatar_hash.as_deref(), Some(JPEG_HASH));
    }
}

/// One user the directory no longer has, or whose picture cannot be read,
/// is a warning; the next user is still synced.
#[tokio::test]
async fn one_users_failure_does_not_stop_the_task() {
    let harness = Harness::new(&sync_on()).await;
    harness.answer_for("ada", with_picture(ada(), JPEG));
    harness.answer_for("bob", with_picture(bob(), JPEG));
    harness.answer(Vec::new());
    let ada_user = harness.created("ada", ADA_PASSWORD).await;
    let bob_user = harness.created("bob", BOB_PASSWORD).await;

    // Ada's entry is gone; Bob's picture changed.
    harness.directory.state().by_filter.clear();
    harness.answer_for("bob", with_picture(bob(), OTHER_JPEG));
    harness.run_sync().await;
    assert_eq!(harness.picture(&ada_user).await.as_deref(), Some(JPEG));
    assert_eq!(
        harness.picture(&bob_user).await.as_deref(),
        Some(OTHER_JPEG)
    );

    // Ada's picture is a value no format fits; Bob's is synced still.
    harness.answer_for("ada", with_picture(ada(), b"ada.jpg"));
    harness.answer_for("bob", with_picture(bob(), JPEG));
    harness.run_sync().await;
    assert_eq!(harness.picture(&ada_user).await.as_deref(), Some(JPEG));
    assert_eq!(harness.picture(&bob_user).await.as_deref(), Some(JPEG));
}

/// A directory that cannot be reached, or refuses the bind DN, fails the
/// task at the first user rather than at each in turn; a search it fails for
/// one user is a warning.
#[tokio::test]
async fn a_directory_that_cannot_be_reached_fails_the_task() {
    let harness = Harness::new(&sync_on()).await;
    harness.answer_for("ada", with_picture(ada(), JPEG));
    harness.answer_for("bob", with_picture(bob(), JPEG));
    harness.answer(Vec::new());
    let ada_user = harness.created("ada", ADA_PASSWORD).await;
    let bob_user = harness.created("bob", BOB_PASSWORD).await;
    let task = ProfileImageSyncTask::new(Arc::new(harness.provider()));

    harness.directory.state().connect = Some(LdapError::Connect("connection refused".to_owned()));
    harness.directory.clear_calls();
    let error = task
        .execute(&ferrofin_core::TaskProgress::default())
        .await
        .expect_err("failed");
    assert!(
        error
            .to_string()
            .contains("LDAP profile image sync could not connect"),
        "{error}"
    );
    assert_eq!(
        harness.directory.state().connects,
        1,
        "it stopped at the first user"
    );
    harness.directory.state().connect = None;

    harness
        .directory
        .state()
        .bind
        .insert(READER_DN.to_owned(), invalid_credentials());
    let error = task
        .execute(&ferrofin_core::TaskProgress::default())
        .await
        .expect_err("failed");
    assert!(
        error.to_string().contains("bind as the configured bind DN"),
        "{error}"
    );
    harness.directory.state().bind.clear();

    // Ada's search fails; Bob's picture is still synced.
    harness
        .directory
        .state()
        .failing_filters
        .insert(user_search_filter(&config(), "ada"), invalid_credentials());
    harness.answer_for("bob", with_picture(bob(), OTHER_JPEG));
    harness.run_sync().await;
    assert_eq!(harness.picture(&ada_user).await.as_deref(), Some(JPEG));
    assert_eq!(
        harness.picture(&bob_user).await.as_deref(),
        Some(OTHER_JPEG)
    );
}

#[tokio::test]
async fn while_the_plugin_is_disabled_the_task_does_nothing() {
    let harness = Harness::disabled().await;
    harness.configure(&sync_on()).await;
    harness.run_sync().await;
    assert!(harness.directory.untouched());
}

#[tokio::test]
async fn the_task_fetches_a_picture_given_as_a_url() {
    let harness = Harness::new(&sync_on()).await;
    harness.answer(vec![ada()]);
    let user = harness.created("ada", ADA_PASSWORD).await;
    assert_eq!(harness.picture(&user).await, None);

    harness.images.answer(PICTURE_URL, Ok(OTHER_JPEG.to_vec()));
    harness.answer(vec![with_picture(ada(), PICTURE_URL.as_bytes())]);
    harness.run_sync().await;
    assert_eq!(harness.picture(&user).await.as_deref(), Some(OTHER_JPEG));
    assert_eq!(harness.images.fetched(), [PICTURE_URL]);
}

/// Novell's `ByteValue` and `StringValue` read an attribute's first value.
#[tokio::test]
async fn a_multi_valued_attribute_gives_its_first_value() {
    let harness = Harness::new(&sync_on()).await;
    let mut entry = ada();
    entry.attributes.push(LdapAttribute {
        name: "jpegPhoto".to_owned(),
        values: vec![JPEG.to_vec(), OTHER_JPEG.to_vec()],
    });
    harness.answer(vec![entry]);
    let user = harness.created("ada", ADA_PASSWORD).await;
    assert_eq!(harness.picture(&user).await.as_deref(), Some(JPEG));
}

/// An empty attribute name names nothing an entry could hold: it is not
/// sent, and no picture is taken.
#[tokio::test]
async fn an_empty_picture_attribute_is_not_asked_for() {
    let harness = Harness::new(&LdapConfig {
        ldap_profile_image_attribute: String::new(),
        ..sync_on()
    })
    .await;
    harness.answer(vec![with_picture(ada(), JPEG)]);
    let user = harness.created("ada", ADA_PASSWORD).await;
    assert_eq!(harness.directory.state().searches[0].3, ["uid", "uid"]);
    assert_eq!(harness.picture(&user).await, None);
}

/// A link stored with an empty uid (an adopted `LdapUsers` entry without
/// one) is warned about as not found, without a search for it.
#[tokio::test]
async fn a_link_with_an_empty_uid_is_skipped_without_a_search() {
    let harness = Harness::new(&sync_on()).await;
    let user = harness.users.create_user("grace").await.expect("create");
    harness
        .identities
        .link(id_of(&user), LdapAuthProvider::identity(""), None)
        .await
        .expect("link");
    harness.directory.clear_calls();
    harness.run_sync().await;
    assert!(
        harness.directory.state().searches.is_empty(),
        "no search was sent"
    );
    assert_eq!(harness.picture(&user).await, None);
}
