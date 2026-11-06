//! Password-hashing provider.
//!
//! Port of `MediaBrowser.Model.Cryptography.ICryptoProvider` (the trait) and
//! `Emby.Server.Implementations.Cryptography.CryptographyProvider` (the impl).
//!
//! Faithful to Jellyfin: key derivation is **real PBKDF2** via
//! `.NET`'s `Rfc2898DeriveBytes.Pbkdf2` — PBKDF2-HMAC-SHA512 for the default
//! `PBKDF2-SHA512` id and PBKDF2-HMAC-SHA1 for the legacy `PBKDF2` id, honouring
//! the stored `iterations` parameter. This means Ferrofin can verify password
//! hashes produced by an existing Jellyfin server (user-DB migration works).

use pbkdf2::pbkdf2_hmac;
use rand::RngExt as _;
use sha1::Sha1;
use sha2::Sha512;

use crate::cryptography::constants::Constants;
use crate::cryptography::password_hash::PasswordHash;
use crate::error::{CryptoError, Result};

/// The PRF (HMAC hash) a PBKDF2 hash id uses.
#[derive(Debug, Clone, Copy)]
enum Prf {
    /// PBKDF2-HMAC-SHA1 — the legacy `PBKDF2` id (32-byte output).
    Sha1,
    /// PBKDF2-HMAC-SHA512 — the default `PBKDF2-SHA512` id.
    Sha512,
}

/// Abstraction over the password-hashing functions (port of `ICryptoProvider`).
pub trait CryptoProvider {
    /// The default hash method id.
    fn default_hash_method(&self) -> &'static str;

    /// Creates a new [`PasswordHash`] for `password`.
    ///
    /// # Errors
    ///
    /// Returns an error if the resulting [`PasswordHash`] is invalid.
    fn create_password_hash(&self, password: &str) -> Result<PasswordHash>;

    /// Verifies `password` against `hash`.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::Format`] when a required parameter is missing or
    /// invalid, or [`CryptoError::NotSupported`] for an unknown hash id.
    fn verify(&self, hash: &PasswordHash, password: &str) -> Result<bool>;

    /// Generates a salt of the default length.
    fn generate_salt(&self) -> Vec<u8>;

    /// Generates a salt of `length` bytes.
    fn generate_salt_with_length(&self, length: usize) -> Vec<u8>;
}

/// Concrete crypto provider (port of `CryptographyProvider`).
#[derive(Debug, Default, Clone, Copy)]
pub struct CryptographyProvider;

impl CryptographyProvider {
    /// Creates a new provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Derives `output_len` bytes for `password`/`salt` using PBKDF2 with the
    /// given PRF and iteration count — the exact primitive Jellyfin's
    /// `Rfc2898DeriveBytes.Pbkdf2` computes.
    fn derive(
        password: &str,
        salt: &[u8],
        iterations: u32,
        prf: Prf,
        output_len: usize,
    ) -> Vec<u8> {
        let mut out = vec![0u8; output_len];
        match prf {
            Prf::Sha1 => pbkdf2_hmac::<Sha1>(password.as_bytes(), salt, iterations, &mut out),
            Prf::Sha512 => pbkdf2_hmac::<Sha512>(password.as_bytes(), salt, iterations, &mut out),
        }
        out
    }

    /// Extracts and validates the `iterations` parameter from `hash`.
    ///
    /// Port of `CryptographyProvider.GetIterationsParameter`; preserves both
    /// FormatException messages verbatim.
    fn iterations_parameter(hash: &PasswordHash) -> Result<u32> {
        let Some((_, iterations_str)) = hash.parameters().iter().find(|(k, _)| k == "iterations")
        else {
            return Err(CryptoError::Format(format!(
                "Password hash with id '{}' is missing required 'iterations' parameter.",
                hash.id()
            )));
        };

        iterations_str.parse::<u32>().map_err(|_| {
            CryptoError::Format(format!(
                "Password hash with id '{}' has invalid 'iterations' parameter: '{iterations_str}'.",
                hash.id()
            ))
        })
    }
}

impl CryptoProvider for CryptographyProvider {
    fn default_hash_method(&self) -> &'static str {
        "PBKDF2-SHA512"
    }

    fn create_password_hash(&self, password: &str) -> Result<PasswordHash> {
        let salt = self.generate_salt();
        let hash = Self::derive(
            password,
            &salt,
            Constants::DEFAULT_ITERATIONS,
            Prf::Sha512,
            Constants::DEFAULT_OUTPUT_LENGTH,
        );
        PasswordHash::new(
            self.default_hash_method(),
            hash,
            salt,
            vec![(
                "iterations".to_owned(),
                Constants::DEFAULT_ITERATIONS.to_string(),
            )],
        )
    }

    fn verify(&self, hash: &PasswordHash, password: &str) -> Result<bool> {
        match hash.id() {
            // Legacy PBKDF2-HMAC-SHA1, 32-byte derived key.
            "PBKDF2" => {
                let iterations = Self::iterations_parameter(hash)?;
                let derived = Self::derive(password, hash.salt(), iterations, Prf::Sha1, 32);
                Ok(constant_time_eq(hash.hash(), &derived))
            }
            // Default PBKDF2-HMAC-SHA512.
            "PBKDF2-SHA512" => {
                let iterations = Self::iterations_parameter(hash)?;
                let derived = Self::derive(
                    password,
                    hash.salt(),
                    iterations,
                    Prf::Sha512,
                    Constants::DEFAULT_OUTPUT_LENGTH,
                );
                Ok(constant_time_eq(hash.hash(), &derived))
            }
            other => Err(CryptoError::NotSupported(format!(
                "Can't verify hash with id: {other}"
            ))),
        }
    }

    fn generate_salt(&self) -> Vec<u8> {
        self.generate_salt_with_length(Constants::DEFAULT_SALT_LENGTH)
    }

    fn generate_salt_with_length(&self, length: usize) -> Vec<u8> {
        // Port of RandomNumberGenerator.GetNonZeroBytes: CSPRNG bytes with each
        // zero redrawn, so every byte is uniform over 1..=255.
        let mut rng = rand::rng();
        let mut salt = vec![0u8; length];
        for b in &mut salt {
            while *b == 0 {
                *b = rng.random();
            }
        }
        salt
    }
}

/// Length-checked constant-time byte comparison (mirrors `SequenceEqual`).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }

    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    // RFC 6070 known-answer vectors for PBKDF2-HMAC-SHA1 (dkLen=20). These lock
    // the KDF to real PBKDF2 — a fake/substituted KDF cannot reproduce them.
    #[test]
    fn pbkdf2_sha1_matches_rfc6070() {
        assert_eq!(
            hex(&CryptographyProvider::derive(
                "password",
                b"salt",
                1,
                Prf::Sha1,
                20
            )),
            "0c60c80f961f0e71f3a9b524af6012062fe037a6"
        );
        assert_eq!(
            hex(&CryptographyProvider::derive(
                "password",
                b"salt",
                2,
                Prf::Sha1,
                20
            )),
            "ea6c014dc72d6f8ccd1ed92ace1d41f0d8de8957"
        );
        assert_eq!(
            hex(&CryptographyProvider::derive(
                "password",
                b"salt",
                4096,
                Prf::Sha1,
                20
            )),
            "4b007901b765489abead49d926f721d065a429c1"
        );
    }

    // Known-answer vector for PBKDF2-HMAC-SHA512 (P="password", S="salt", c=1,
    // dkLen=64) — guards the default hash path Jellyfin actually uses.
    #[test]
    fn pbkdf2_sha512_known_answer() {
        assert_eq!(
            hex(&CryptographyProvider::derive(
                "password",
                b"salt",
                1,
                Prf::Sha512,
                64
            )),
            "867f70cf1ade02cff3752599a3a53dc4af34c7a669815ae5d513554e1c8cf252\
             c02d470a285a0501bad999bfe943c08f050235d7d68b1da55e63f73b60a57fce"
        );
    }

    #[test]
    fn create_then_verify_round_trip() {
        let provider = CryptographyProvider::new();
        let hash = provider.create_password_hash("hunter2").unwrap();
        assert_eq!(hash.id(), "PBKDF2-SHA512");
        assert!(provider.verify(&hash, "hunter2").unwrap());
        assert!(!provider.verify(&hash, "wrong").unwrap());
    }

    // GetNonZeroBytes draws every byte uniformly from 1..=255: none is zero,
    // bytes 6 and 8 do not always carry a v4 UUID's version nibble and variant
    // bits, and 1 is no likelier than any other value.
    #[test]
    fn salt_bytes_are_uniform_and_non_zero() {
        let provider = CryptographyProvider::new();
        let salts: Vec<Vec<u8>> = (0..4096).map(|_| provider.generate_salt()).collect();
        let zero = salts.iter().find(|s| s.contains(&0));
        assert!(zero.is_none(), "{zero:?}");
        let fixed = |i: usize, shift: u8, bits: u8| salts.iter().all(|s| s[i] >> shift == bits);
        assert!(!fixed(6, 4, 0x4), "byte 6 is always 0x4_");
        assert!(!fixed(8, 6, 0b10), "byte 8 is always 0b10__");
        // 65,536 bytes: ~257 ones (sd 16) when uniform, ~512 with zeros mapped to 1.
        let ones = salts.iter().flatten().filter(|&&b| b == 1).count();
        assert!(ones < 384, "{ones} ones: zeros are being mapped to 1");
    }

    #[test]
    fn verify_unknown_id_is_not_supported() {
        let provider = CryptographyProvider::new();
        let hash = PasswordHash::new("SCRYPT", vec![1, 2, 3], vec![4, 5, 6], vec![]).unwrap();
        assert!(matches!(
            provider.verify(&hash, "x"),
            Err(CryptoError::NotSupported(_))
        ));
    }

    #[test]
    fn verify_missing_iterations_is_format_error() {
        let provider = CryptographyProvider::new();
        let hash =
            PasswordHash::new("PBKDF2-SHA512", vec![1, 2, 3], vec![4, 5, 6], vec![]).unwrap();
        assert!(matches!(
            provider.verify(&hash, "x"),
            Err(CryptoError::Format(_))
        ));
    }
}
