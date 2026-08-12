//! PHC-like password-hash string parse/format value type.
//!
//! Faithful port of `MediaBrowser.Model.Cryptography.PasswordHash`. Format:
//! `$<id>[$<param>=<value>(,<param>=<value>)*][$<salt>[$<hash>]]` where — unlike
//! the PHC spec — the salt and hash bytes are UPPERCASE hex rather than base64.

use crate::error::{CryptoError, Result};

/// A parsed password hash: id, ordered parameters, salt, and hash bytes.
///
/// Port of `PasswordHash`. Parameters preserve insertion order (a `Vec` of
/// key/value pairs) so [`PasswordHash::to_string`] reproduces the C# output
/// byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordHash {
    id: String,
    parameters: Vec<(String, String)>,
    salt: Vec<u8>,
    hash: Vec<u8>,
}

impl PasswordHash {
    /// Creates a new [`PasswordHash`] from its components.
    ///
    /// Port of the four-argument `PasswordHash` constructor (the shorter C#
    /// overloads pass empty salt/parameters).
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::Argument`] if `id` is empty (matches
    /// `ArgumentException.ThrowIfNullOrEmpty(id)`).
    pub fn new(
        id: impl Into<String>,
        hash: Vec<u8>,
        salt: Vec<u8>,
        parameters: Vec<(String, String)>,
    ) -> Result<Self> {
        let id = id.into();
        if id.is_empty() {
            return Err(CryptoError::Argument("id".to_owned()));
        }

        Ok(Self {
            id,
            parameters,
            salt,
            hash,
        })
    }

    /// Creates a [`PasswordHash`] with only an id and hash (empty salt/params).
    ///
    /// Port of the `PasswordHash(string id, byte[] hash)` overload.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::Argument`] if `id` is empty.
    pub fn with_hash(id: impl Into<String>, hash: Vec<u8>) -> Result<Self> {
        Self::new(id, hash, Vec::new(), Vec::new())
    }

    /// The symbolic name for the function used.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The additional parameters used by the hash function, in insertion order.
    #[must_use]
    pub fn parameters(&self) -> &[(String, String)] {
        &self.parameters
    }

    /// The salt used for hashing the password.
    #[must_use]
    pub fn salt(&self) -> &[u8] {
        &self.salt
    }

    /// The hashed password.
    #[must_use]
    pub fn hash(&self) -> &[u8] {
        &self.hash
    }

    /// Parses a password-hash string.
    ///
    /// Faithful port of `PasswordHash.Parse`.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::Argument`] when `hash_string` is empty, or
    /// [`CryptoError::Format`] for any malformed segment.
    #[allow(clippy::too_many_lines)]
    pub fn parse(hash_string: &str) -> Result<Self> {
        if hash_string.is_empty() {
            return Err(CryptoError::Argument("String can't be empty".to_owned()));
        }

        if !hash_string.starts_with('$') {
            return Err(CryptoError::Format(
                "Hash string must start with a $".to_owned(),
            ));
        }

        // Ignore first $.
        let mut hash_string = &hash_string[1..];

        let mut next_segment = index_of(hash_string, '$');
        if hash_string.is_empty() || next_segment == Some(0) {
            return Err(CryptoError::Format(
                "Hash string must contain a valid id".to_owned(),
            ));
        }

        let Some(next) = next_segment else {
            return Self::with_hash(hash_string.to_owned(), Vec::new());
        };

        let id = &hash_string[..next];
        hash_string = &hash_string[next + 1..];
        let mut parameters: Vec<(String, String)> = Vec::new();

        next_segment = index_of(hash_string, '$');

        // Optional parameters.
        let parameters_span = match next_segment {
            None => hash_string,
            Some(n) => &hash_string[..n],
        };
        if parameters_span.contains('=') {
            let mut parameters_span = parameters_span;
            while !parameters_span.is_empty() {
                let parameter;
                match index_of(parameters_span, ',') {
                    None => {
                        parameter = parameters_span;
                        parameters_span = "";
                    }
                    Some(index) => {
                        parameter = &parameters_span[..index];
                        parameters_span = &parameters_span[index + 1..];
                    }
                }

                // C#: split_index == -1 || 0 || parameter.Length - 1 is malformed.
                match index_of(parameter, '=') {
                    Some(idx) if idx != 0 && idx != parameter.len() - 1 => {
                        parameters
                            .push((parameter[..idx].to_owned(), parameter[idx + 1..].to_owned()));
                    }
                    _ => {
                        return Err(CryptoError::Format(
                            "Malformed parameter in password hash string".to_owned(),
                        ));
                    }
                }
            }

            let Some(n) = next_segment else {
                return Self::new(id.to_owned(), Vec::new(), Vec::new(), parameters);
            };

            hash_string = &hash_string[n + 1..];
            next_segment = index_of(hash_string, '$');
        }

        if next_segment == Some(0) {
            return Err(CryptoError::Format(
                "Hash string contains an empty segment".to_owned(),
            ));
        }

        let hash: Vec<u8>;
        let salt: Vec<u8>;

        match next_segment {
            None => {
                salt = Vec::new();
                hash = from_hex_string(hash_string)?;
            }
            Some(n) => {
                salt = from_hex_string(&hash_string[..n])?;
                hash_string = &hash_string[n + 1..];
                let after = index_of(hash_string, '$');
                if after.is_some() {
                    return Err(CryptoError::Format(
                        "Hash string contains too many segments".to_owned(),
                    ));
                }

                if hash_string.is_empty() {
                    return Err(CryptoError::Format("Hash segment is empty".to_owned()));
                }

                hash = from_hex_string(hash_string)?;
            }
        }

        Self::new(id.to_owned(), hash, salt, parameters)
    }

    /// Appends the parameters segment to `out` (port of `SerializeParameters`).
    fn serialize_parameters(&self, out: &mut String) {
        if self.parameters.is_empty() {
            return;
        }

        out.push('$');
        for (key, value) in &self.parameters {
            out.push_str(key);
            out.push('=');
            out.push_str(value);
            out.push(',');
        }

        // Remove last ','.
        out.pop();
    }
}

impl std::fmt::Display for PasswordHash {
    /// Formats the hash back to its `$id[$params][$salt][$hash]` string.
    ///
    /// Faithful port of `PasswordHash.ToString` (UPPERCASE hex).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        out.push('$');
        out.push_str(&self.id);
        self.serialize_parameters(&mut out);

        if !self.salt.is_empty() {
            out.push('$');
            out.push_str(&to_hex_string(&self.salt));
        }

        if !self.hash.is_empty() {
            out.push('$');
            out.push_str(&to_hex_string(&self.hash));
        }

        f.write_str(&out)
    }
}

/// Returns the byte index of the first `needle` in `haystack`, if any.
///
/// Mirrors `ReadOnlySpan<char>.IndexOf(char)`; all delimiters here are ASCII.
fn index_of(haystack: &str, needle: char) -> Option<usize> {
    haystack.find(needle)
}

/// Parses an UPPERCASE-or-lowercase hex string into bytes.
///
/// Mirrors `Convert.FromHexString`: returns [`CryptoError::Format`] on
/// odd-length input or any non-hex character (C# throws `FormatException`).
fn from_hex_string(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(CryptoError::Format(
            "The input is not a valid hex string".to_owned(),
        ));
    }

    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = hex_val(pair[0])?;
        let lo = hex_val(pair[1])?;
        out.push((hi << 4) | lo);
    }

    Ok(out)
}

/// Converts one ASCII hex digit to its nibble value.
fn hex_val(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(CryptoError::Format(
            "The input is not a valid hex string".to_owned(),
        )),
    }
}

/// Formats bytes as UPPERCASE hex (mirrors `Convert.ToHexString`).
fn to_hex_string(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(out, "{b:02X}").expect("writing to a String never fails");
    }
    out
}
