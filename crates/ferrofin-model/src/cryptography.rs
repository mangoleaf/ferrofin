//! Port of `MediaBrowser.Model.Cryptography` — `PasswordHash`, `Constants`, and
//! the `ICryptoProvider` trait.
//!
//! `PasswordHash` implements the PHC-inspired storage format
//! (`$<id>[$<param>=<value>(,<param>=<value>)*][$<salt>[$<hash>]]`), except the
//! salt/hash bytes are written as uppercase hex rather than padding-stripped
//! Base64. See the [PHC string format spec][phc].
//!
//! [phc]: https://github.com/P-H-C/phc-string-format/blob/master/phc-sf-spec.md

use std::fmt;

/// Global constants for Jellyfin cryptography.
pub mod constants {
    /// The default length for new salts (128 bits).
    pub const DEFAULT_SALT_LENGTH: usize = 128 / 8;

    /// The default output length (512 bits).
    pub const DEFAULT_OUTPUT_LENGTH: usize = 512 / 8;

    /// The default amount of iterations for hashing passwords.
    pub const DEFAULT_ITERATIONS: u32 = 210_000;
}

/// Error returned when constructing or parsing a [`PasswordHash`] fails.
///
/// `EmptyId` mirrors the C# `ArgumentException`/`ArgumentNullException` thrown
/// for a null/empty id; every other variant mirrors a `FormatException`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PasswordHashError {
    /// The id was null or empty.
    #[error("id can't be empty")]
    EmptyId,
    /// The hash string was empty.
    #[error("string can't be empty")]
    EmptyString,
    /// The hash string did not start with `$`.
    #[error("hash string must start with a $")]
    MissingLeadingDollar,
    /// The hash string did not contain a valid id.
    #[error("hash string must contain a valid id")]
    MissingId,
    /// The hash string contained an empty segment.
    #[error("hash string contains an empty segment")]
    EmptySegment,
    /// A parameter (`key=value`) was malformed.
    #[error("malformed parameter in password hash string")]
    MalformedParameter,
    /// The hash string contained too many `$`-delimited segments.
    #[error("hash string contains too many segments")]
    TooManySegments,
    /// The hash segment was empty.
    #[error("hash segment is empty")]
    EmptyHashSegment,
    /// A salt or hash segment was not valid hexadecimal.
    #[error("invalid hex in password hash string")]
    InvalidHex,
}

/// A parsed password hash in the Jellyfin PHC-style storage format.
///
/// Defined from the [PHC string format spec][phc], with salt/hash bytes stored
/// as uppercase hex rather than Base64.
///
/// [phc]: https://github.com/P-H-C/phc-string-format/blob/master/phc-sf-spec.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordHash {
    id: String,
    // Insertion-ordered to preserve round-trip fidelity (C# `Dictionary`
    // preserves insertion order absent removals, which `ToString` relies on).
    parameters: Vec<(String, String)>,
    salt: Vec<u8>,
    hash: Vec<u8>,
}

impl PasswordHash {
    /// Creates a new password hash from an id, hash, salt and parameters.
    ///
    /// # Errors
    ///
    /// Returns [`PasswordHashError::EmptyId`] when `id` is empty.
    pub fn new(
        id: impl Into<String>,
        hash: Vec<u8>,
        salt: Vec<u8>,
        parameters: Vec<(String, String)>,
    ) -> Result<Self, PasswordHashError> {
        let id = id.into();
        if id.is_empty() {
            return Err(PasswordHashError::EmptyId);
        }
        Ok(Self {
            id,
            parameters,
            salt,
            hash,
        })
    }

    /// Creates a new password hash from just an id and hash (empty salt/params).
    ///
    /// # Errors
    ///
    /// Returns [`PasswordHashError::EmptyId`] when `id` is empty.
    pub fn with_hash(id: impl Into<String>, hash: Vec<u8>) -> Result<Self, PasswordHashError> {
        Self::new(id, hash, Vec::new(), Vec::new())
    }

    /// Gets the symbolic name for the function used.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Gets the additional parameters used by the hash function, in insertion
    /// order.
    #[must_use]
    pub fn parameters(&self) -> &[(String, String)] {
        &self.parameters
    }

    /// Gets the salt used for hashing the password.
    #[must_use]
    pub fn salt(&self) -> &[u8] {
        &self.salt
    }

    /// Gets the hashed password.
    #[must_use]
    pub fn hash(&self) -> &[u8] {
        &self.hash
    }

    /// Parses a `PasswordHash` from its string representation.
    ///
    /// # Errors
    ///
    /// Returns a [`PasswordHashError`] variant mirroring the C# exception for
    /// each malformed-input case (empty string, missing `$`, empty/invalid
    /// segments, malformed parameters, invalid hex, extra segments).
    pub fn parse(hash_string: &str) -> Result<Self, PasswordHashError> {
        if hash_string.is_empty() {
            return Err(PasswordHashError::EmptyString);
        }

        let bytes = hash_string.as_bytes();
        if bytes[0] != b'$' {
            return Err(PasswordHashError::MissingLeadingDollar);
        }

        // Ignore first `$`.
        let mut rest = &hash_string[1..];

        let next_segment = index_of(rest, '$');
        if rest.is_empty() || next_segment == Some(0) {
            return Err(PasswordHashError::MissingId);
        }

        let Some(next) = next_segment else {
            // Id only.
            return Self::with_hash(rest.to_owned(), Vec::new());
        };

        let id = &rest[..next];
        rest = &rest[next + 1..];
        let mut parameters: Vec<(String, String)> = Vec::new();

        let mut next_segment = index_of(rest, '$');

        // Optional parameters.
        let parameters_span = match next_segment {
            None => rest,
            Some(n) => &rest[..n],
        };
        if parameters_span.contains('=') {
            let mut span = parameters_span;
            while !span.is_empty() {
                let parameter;
                match index_of(span, ',') {
                    None => {
                        parameter = span;
                        span = "";
                    }
                    Some(index) => {
                        parameter = &span[..index];
                        span = &span[index + 1..];
                    }
                }

                match index_of(parameter, '=') {
                    Some(split_index) if split_index != 0 && split_index != parameter.len() - 1 => {
                        parameters.push((
                            parameter[..split_index].to_owned(),
                            parameter[split_index + 1..].to_owned(),
                        ));
                    }
                    _ => return Err(PasswordHashError::MalformedParameter),
                }
            }

            let Some(segment) = next_segment else {
                return Ok(Self {
                    id: id.to_owned(),
                    parameters,
                    salt: Vec::new(),
                    hash: Vec::new(),
                });
            };

            rest = &rest[segment + 1..];
            next_segment = index_of(rest, '$');
        }

        if next_segment == Some(0) {
            return Err(PasswordHashError::EmptySegment);
        }

        let salt;
        let hash;

        match next_segment {
            None => {
                salt = Vec::new();
                hash = from_hex(rest)?;
            }
            Some(n) => {
                salt = from_hex(&rest[..n])?;
                rest = &rest[n + 1..];
                if index_of(rest, '$').is_some() {
                    return Err(PasswordHashError::TooManySegments);
                }
                if rest.is_empty() {
                    return Err(PasswordHashError::EmptyHashSegment);
                }
                hash = from_hex(rest)?;
            }
        }

        Ok(Self {
            id: id.to_owned(),
            parameters,
            salt,
            hash,
        })
    }
}

impl fmt::Display for PasswordHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "${}", self.id)?;

        if !self.parameters.is_empty() {
            f.write_str("$")?;
            let mut first = true;
            for (key, value) in &self.parameters {
                if !first {
                    f.write_str(",")?;
                }
                write!(f, "{key}={value}")?;
                first = false;
            }
        }

        if !self.salt.is_empty() {
            write!(f, "${}", to_hex(&self.salt))?;
        }

        if !self.hash.is_empty() {
            write!(f, "${}", to_hex(&self.hash))?;
        }

        Ok(())
    }
}

/// Trait `ICryptoProvider` — creates and verifies [`PasswordHash`]es.
pub trait CryptoProvider {
    /// Gets the default hash method used when creating new hashes.
    fn default_hash_method(&self) -> &str;

    /// Creates a new [`PasswordHash`] for `password`.
    fn create_password_hash(&self, password: &str) -> PasswordHash;

    /// Verifies `password` against an existing `hash`.
    fn verify(&self, hash: &PasswordHash, password: &str) -> bool;

    /// Generates a new salt of the default length.
    fn generate_salt(&self) -> Vec<u8>;

    /// Generates a new salt of the given `length`.
    fn generate_salt_with_length(&self, length: usize) -> Vec<u8>;
}

/// Returns the byte index of the first occurrence of `needle` in `haystack`,
/// mirroring C# `ReadOnlySpan<char>.IndexOf` for the ASCII delimiters used
/// here.
fn index_of(haystack: &str, needle: char) -> Option<usize> {
    haystack.find(needle)
}

/// Encodes bytes as uppercase hex (mirrors `Convert.ToHexString`).
fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        use fmt::Write;
        let _ = write!(out, "{b:02X}");
    }
    out
}

/// Decodes an uppercase-or-lowercase hex string into bytes (mirrors
/// `Convert.FromHexString`, which is case-insensitive and rejects odd lengths /
/// non-hex digits).
fn from_hex(s: &str) -> Result<Vec<u8>, PasswordHashError> {
    if !s.len().is_multiple_of(2) {
        return Err(PasswordHashError::InvalidHex);
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_digit(bytes[i])?;
        let lo = hex_digit(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

/// Converts a single ASCII hex digit to its numeric value.
fn hex_digit(c: u8) -> Result<u8, PasswordHashError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(PasswordHashError::InvalidHex),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn ctor_empty_returns_error() {
        assert_eq!(
            PasswordHash::with_hash("", Vec::new()).unwrap_err(),
            PasswordHashError::EmptyId
        );
    }

    #[rstest]
    #[case("$PBKDF2", "PBKDF2", &[], "", "")]
    #[case("$PBKDF2$iterations=1000", "PBKDF2", &[("iterations", "1000")], "", "")]
    #[case(
        "$PBKDF2$iterations=1000,m=120",
        "PBKDF2",
        &[("iterations", "1000"), ("m", "120")],
        "",
        ""
    )]
    #[case(
        "$PBKDF2$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D",
        "PBKDF2",
        &[],
        "",
        "62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D"
    )]
    #[case(
        "$PBKDF2$69F420$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D",
        "PBKDF2",
        &[],
        "69F420",
        "62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D"
    )]
    #[case(
        "$PBKDF2$iterations=1000$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D",
        "PBKDF2",
        &[("iterations", "1000")],
        "",
        "62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D"
    )]
    #[case(
        "$PBKDF2$iterations=1000,m=120$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D",
        "PBKDF2",
        &[("iterations", "1000"), ("m", "120")],
        "",
        "62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D"
    )]
    #[case(
        "$PBKDF2$iterations=1000,m=120$69F420$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D",
        "PBKDF2",
        &[("iterations", "1000"), ("m", "120")],
        "69F420",
        "62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D"
    )]
    fn parse_valid_success(
        #[case] input: &str,
        #[case] expected_id: &str,
        #[case] expected_params: &[(&str, &str)],
        #[case] expected_salt_hex: &str,
        #[case] expected_hash_hex: &str,
    ) {
        let parsed = PasswordHash::parse(input).expect("valid hash");
        assert_eq!(expected_id, parsed.id());
        assert_eq!(params(expected_params), parsed.parameters());
        assert_eq!(from_hex(expected_salt_hex).unwrap(), parsed.salt());
        assert_eq!(from_hex(expected_hash_hex).unwrap(), parsed.hash());

        // Round-trip: ToString of the parsed value equals the expected value
        // constructed from the same parts.
        let expected = PasswordHash::new(
            expected_id,
            from_hex(expected_hash_hex).unwrap(),
            from_hex(expected_salt_hex).unwrap(),
            params(expected_params),
        )
        .unwrap();
        assert_eq!(expected.to_string(), parsed.to_string());
    }

    #[rstest]
    #[case("$PBKDF2")]
    #[case("$PBKDF2$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D")]
    #[case("$PBKDF2$69F420$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D")]
    #[case(
        "$PBKDF2$iterations=1000$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D"
    )]
    #[case(
        "$PBKDF2$iterations=1000,m=120$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D"
    )]
    #[case(
        "$PBKDF2$iterations=1000,m=120$69F420$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D"
    )]
    #[case("$PBKDF2$iterations=1000,m=120")]
    fn to_string_roundtrip_success(#[case] input: &str) {
        assert_eq!(input, PasswordHash::parse(input).unwrap().to_string());
    }

    #[test]
    fn parse_empty_returns_error() {
        assert_eq!(
            PasswordHash::parse("").unwrap_err(),
            PasswordHashError::EmptyString
        );
    }

    #[rstest]
    #[case("$")] // No id
    #[case("$$")] // Empty segments
    #[case("PBKDF2$")] // Doesn't start with $
    #[case("$PBKDF2$$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D")] // Empty segment
    #[case(
        "$PBKDF2$iterations=1000$$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D"
    )] // Empty salt segment
    #[case("$PBKDF2$iterations=1000$69F420$")] // Empty hash segment
    #[case("$PBKDF2$=$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D")] // Invalid parameter
    #[case("$PBKDF2$=1000$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D")] // Invalid parameter
    #[case("$PBKDF2$iterations=$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D")] // Invalid parameter
    #[case(
        "$PBKDF2$iterations=1000$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D$"
    )] // Ends on $
    #[case(
        "$PBKDF2$iterations=1000$69F420$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D$"
    )] // Extra segment
    #[case(
        "$PBKDF2$iterations=1000$69F420$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D$anotherone"
    )] // Extra segment
    #[case(
        "$PBKDF2$iterations=1000$invalidstalt$62FBA410AFCA5B4475F35137AB2E8596B127E4D927BA23F6CC05C067E897042D"
    )] // Invalid salt
    #[case("$PBKDF2$iterations=1000$69F420$invalid hash")] // Invalid hash
    #[case("$PBKDF2$69F420$")] // Empty hash
    fn parse_invalid_format_returns_error(#[case] input: &str) {
        assert!(PasswordHash::parse(input).is_err());
    }
}
