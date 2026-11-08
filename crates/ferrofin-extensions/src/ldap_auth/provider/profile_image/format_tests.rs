//! `LdapUtils.TryDetermineAttributeFormat` and the decoding of each
//! `ProfileImageFormat` (jellyfin-plugin-ldapauth v24, `d11c86a`). Upstream
//! ships no tests for the plugin; the expectations are .NET 10's
//! `Base64.IsValid`, `Convert.FromBase64String` and `Uri.TryCreate` as the
//! plugin calls them.

use rstest::rstest;

use super::*;

/// A JPEG's first bytes.
const JPEG: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
    0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
];

#[rstest]
// A control character other than tab, CR and LF, or DEL, is binary.
#[case::a_jpeg(JPEG)]
#[case::nul(b"abc\x00def")]
#[case::unit_separator(b"\x1f")]
#[case::start_of_heading(b"\x01QUJD")]
#[case::delete(b"QUJD\x7f")]
// Bytes that are no UTF-8 read as U+FFFD, which is no control character, but a
// JPEG always has one of those besides.
#[case::invalid_utf8_with_a_control(b"\xff\xd8\x00")]
fn a_control_character_makes_the_value_binary(#[case] value: &[u8]) {
    assert_eq!(detect_format(value), Ok(ProfileImageFormat::Binary));
}

#[rstest]
#[case::whole_groups("QUJD")]
#[case::two_pads("QQ==")]
#[case::one_pad("QUI=")]
#[case::whitespace_anywhere(" QU\tJD\r\n")]
#[case::whitespace_between_pads("QQ= =")]
// A four-letter word is valid Base64, as upstream reads it.
#[case::a_word("abcd")]
// So is empty text (`Base64.IsValid("")`).
#[case::empty("")]
#[case::whitespace_only(" \r\n\t")]
fn valid_base64_is_base64(#[case] value: &str) {
    assert_eq!(
        detect_format(value.as_bytes()),
        Ok(ProfileImageFormat::Base64)
    );
}

#[rstest]
#[case::https("https://example.org/ada.jpg")]
#[case::http_with_a_query("http://example.org/avatar?user=ada")]
#[case::upper_case_scheme("HTTP://EXAMPLE.ORG/ADA.JPG")]
#[case::surrounding_newlines("\nhttps://example.org/ada.jpg\r\n")]
fn an_absolute_http_or_https_url_is_a_url(#[case] value: &str) {
    assert_eq!(detect_format(value.as_bytes()), Ok(ProfileImageFormat::Url));
}

#[rstest]
// Not Base64: the unused bits of the last character before the padding are
// not zero (`Base64.IsValid` refuses it), so it is read as a relative URI.
#[case::base64_with_trailing_bits("QR==")]
// Not Base64: a length that is no multiple of four, or padding mid-text.
#[case::unpadded_base64("QQ")]
#[case::padding_inside("QQ==QQ==")]
#[case::a_file_name("ada.jpg")]
#[case::words("not a picture")]
fn a_relative_uri_is_an_error(#[case] value: &str) {
    assert_eq!(
        detect_format(value.as_bytes()),
        Err(FormatError::Relative(quoted(value)))
    );
}

#[rstest]
#[case::ftp("ftp://example.org/ada.jpg", "ftp")]
#[case::ldap("ldap://ldap.example.org/uid=ada", "ldap")]
#[case::mailto("mailto:ada@example.org", "mailto")]
#[case::file("file:///var/lib/pictures/ada.jpg", "file")]
fn another_scheme_is_an_error(#[case] value: &str, #[case] scheme: &str) {
    assert_eq!(
        detect_format(value.as_bytes()),
        Err(FormatError::Scheme(scheme.to_owned()))
    );
}

#[rstest]
#[case::no_host("http://")]
#[case::a_space_in_the_host("https://exa mple.org/ada.jpg")]
#[case::an_unclosed_address("http://[::1/ada.jpg")]
fn a_value_that_is_no_uri_is_an_error(#[case] value: &str) {
    assert_eq!(
        detect_format(value.as_bytes()),
        Err(FormatError::NotAUri(quoted(value)))
    );
}

/// An error names a value that is no URL by its length alone: the attribute
/// read may be another one entirely (here a password hash).
#[test]
fn an_error_names_a_value_that_is_no_url_by_its_length() {
    let error = detect_format(b"{SSHA}W6ph5Mm5Pz8GgiULbPgzG37mj9g=").expect_err("relative");
    assert_eq!(
        error.to_string(),
        "ProfileImage Format detection failed. Expected absolute URI but attribute value appears \
         to be a relative path. Got: a value of 34 characters"
    );
}

/// The value in an error never carries a URL's credentials or path.
#[test]
fn an_error_redacts_the_url_it_names() {
    let error = detect_format(b"https://ada:secret@exa mple.org/ada.jpg").expect_err("no URI");
    assert!(!error.to_string().contains("secret"), "{error}");
    assert!(
        error.to_string().starts_with(
            "ProfileImage Format detection failed. Attribute value is not a valid URI"
        ),
        "{error}"
    );
}

#[tokio::test]
async fn each_format_reads_the_value_its_own_way() {
    let images = FakeImages::default();
    images.answer("https://example.org/ada.jpg", Ok(JPEG.to_vec()));

    // Binary is the bytes as they are, even when they read as Base64.
    assert_eq!(
        picture_bytes(b"QUJD", ProfileImageFormat::Binary, &images).await,
        Ok(b"QUJD".to_vec())
    );
    // Base64 decodes, ignoring the unused bits `Base64.IsValid` refuses, as
    // `Convert.FromBase64String` does.
    assert_eq!(
        picture_bytes(b"QR==", ProfileImageFormat::Base64, &images).await,
        Ok(b"A".to_vec())
    );
    assert_eq!(
        picture_bytes(b"/9j/\r\n4AAQ", ProfileImageFormat::Base64, &images).await,
        Ok(vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10])
    );
    // A URL is fetched.
    assert_eq!(
        picture_bytes(
            b"https://example.org/ada.jpg",
            ProfileImageFormat::Url,
            &images
        )
        .await,
        Ok(JPEG.to_vec())
    );
    // Default reads what it detects.
    assert_eq!(
        picture_bytes(JPEG, ProfileImageFormat::Default, &images).await,
        Ok(JPEG.to_vec())
    );
    assert_eq!(
        picture_bytes(
            b"/9j/4AAQSkZJRgABAQAAAQABAAD/2Q==",
            ProfileImageFormat::Default,
            &images
        )
        .await,
        Ok(JPEG.to_vec())
    );
    assert_eq!(
        picture_bytes(
            b"https://example.org/ada.jpg",
            ProfileImageFormat::Default,
            &images
        )
        .await,
        Ok(JPEG.to_vec())
    );
    assert_eq!(
        images.fetched(),
        ["https://example.org/ada.jpg", "https://example.org/ada.jpg"]
    );
}

#[rstest]
#[case::not_base64("not base64!")]
#[case::unpadded("QQ")]
#[case::three_pads("Q===")]
#[tokio::test]
async fn base64_that_does_not_decode_is_an_error(#[case] value: &str) {
    assert_eq!(
        picture_bytes(
            value.as_bytes(),
            ProfileImageFormat::Base64,
            &FakeImages::default()
        )
        .await,
        Err(PictureError::Base64)
    );
}

#[tokio::test]
async fn a_failed_fetch_names_the_server_but_not_the_path() {
    let images = FakeImages::default();
    images.answer(
        "https://pictures.example.org/private/ada.jpg?token=secret",
        Err(RemoteImageError::Refused),
    );
    let error = picture_bytes(
        b"https://pictures.example.org/private/ada.jpg?token=secret",
        ProfileImageFormat::Url,
        &images,
    )
    .await
    .expect_err("refused");
    assert_eq!(
        error,
        PictureError::Fetch {
            url: "https://pictures.example.org".to_owned(),
            error: RemoteImageError::Refused,
        }
    );
    assert!(!error.to_string().contains("secret"), "{error}");
}

/// `Url` set against an attribute that holds no URL: the fetch fails, and the
/// error names the value by its length, never the value (a whole image).
#[tokio::test]
async fn a_failed_fetch_of_a_value_that_is_no_url_names_its_length_only() {
    let error = picture_bytes(
        b"/9j/4AAQSkZJRgABAQAAAQABAAD/2Q==",
        ProfileImageFormat::Url,
        &FakeImages::default(),
    )
    .await
    .expect_err("no URL");
    assert_eq!(
        error,
        PictureError::Fetch {
            url: "a value of 32 characters".to_owned(),
            error: RemoteImageError::Unavailable,
        }
    );
    assert!(!error.to_string().contains("/9j/"), "{error}");
}

#[tokio::test]
async fn default_does_not_guess_what_it_cannot_detect() {
    let images = FakeImages::default();
    assert_eq!(
        picture_bytes(b"ada.jpg", ProfileImageFormat::Default, &images).await,
        Err(PictureError::Format(FormatError::Relative(
            "a value of 7 characters".to_owned()
        )))
    );
    assert!(images.fetched().is_empty(), "nothing was fetched");
}

#[test]
fn the_avatar_hash_is_the_base64_of_the_md5() {
    // `Convert.ToBase64String(MD5.HashData(bytes))`, computed outside the code
    // under test.
    assert_eq!(avatar_hash(JPEG), "V9pvLg8TnhGMeTCudlcEkQ==");
    assert_eq!(avatar_hash(b""), "1B2M2Y8AsgTpgAmY7PhCfg==");
}
