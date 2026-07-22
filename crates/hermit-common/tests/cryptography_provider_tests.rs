//! Transliteration of
//! `Jellyfin.Server.Implementations.Tests/Cryptography/CryptographyProviderTests.cs`.

use hermit_common::CryptoError;
use hermit_common::cryptography::{CryptoProvider, CryptographyProvider, PasswordHash};
use rstest::rstest;

/// The system under test (mirrors the C# `_sut` field).
fn sut() -> CryptographyProvider {
    CryptographyProvider::new()
}

#[test]
fn create_password_hash_with_password_returns_hash_with_iterations() {
    let hash = sut().create_password_hash("testpassword").unwrap();

    assert_eq!("PBKDF2-SHA512", hash.id());
    assert!(hash.parameters().iter().any(|(k, _)| k == "iterations"));
    assert!(!hash.salt().is_empty());
    assert!(!hash.hash().is_empty());
}

#[test]
fn verify_with_valid_password_returns_true() {
    let password = "testpassword";
    let hash = sut().create_password_hash(password).unwrap();

    assert!(sut().verify(&hash, password).unwrap());
}

#[test]
fn verify_with_wrong_password_returns_false() {
    let hash = sut().create_password_hash("correctpassword").unwrap();

    assert!(!sut().verify(&hash, "wrongpassword").unwrap());
}

#[test]
fn verify_pbkdf2_missing_iterations_throws_format_exception() {
    let hash = PasswordHash::parse(
        "$PBKDF2$69F420$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D",
    )
    .unwrap();

    let err = sut().verify(&hash, "password").unwrap_err();
    match err {
        CryptoError::Format(msg) => {
            assert!(msg.contains("missing required 'iterations' parameter"));
        }
        other => panic!("expected Format error, got {other:?}"),
    }
}

#[test]
fn verify_pbkdf2sha512_missing_iterations_throws_format_exception() {
    let hash = PasswordHash::parse(
        "$PBKDF2-SHA512$69F420$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D",
    )
    .unwrap();

    let err = sut().verify(&hash, "password").unwrap_err();
    match err {
        CryptoError::Format(msg) => {
            assert!(msg.contains("missing required 'iterations' parameter"));
        }
        other => panic!("expected Format error, got {other:?}"),
    }
}

#[test]
fn verify_pbkdf2_invalid_iterations_format_throws_format_exception() {
    let hash = PasswordHash::parse(
        "$PBKDF2$iterations=abc$69F420$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D",
    )
    .unwrap();

    let err = sut().verify(&hash, "password").unwrap_err();
    match err {
        CryptoError::Format(msg) => assert!(msg.contains("invalid 'iterations' parameter")),
        other => panic!("expected Format error, got {other:?}"),
    }
}

#[test]
fn verify_pbkdf2sha512_invalid_iterations_format_throws_format_exception() {
    let hash = PasswordHash::parse(
        "$PBKDF2-SHA512$iterations=notanumber$69F420$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D",
    )
    .unwrap();

    let err = sut().verify(&hash, "password").unwrap_err();
    match err {
        CryptoError::Format(msg) => assert!(msg.contains("invalid 'iterations' parameter")),
        other => panic!("expected Format error, got {other:?}"),
    }
}

#[test]
fn verify_unsupported_hash_id_throws_not_supported_exception() {
    let hash = PasswordHash::parse(
        "$UNKNOWN$69F420$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D",
    )
    .unwrap();

    assert!(matches!(
        sut().verify(&hash, "password"),
        Err(CryptoError::NotSupported(_))
    ));
}

#[test]
fn generate_salt_returns_non_empty_array() {
    let salt = sut().generate_salt();

    assert!(!salt.is_empty());
}

#[rstest]
#[case(16)]
#[case(32)]
#[case(64)]
fn generate_salt_with_length_returns_array_of_specified_length(#[case] length: usize) {
    let salt = sut().generate_salt_with_length(length);

    assert_eq!(length, salt.len());
}
