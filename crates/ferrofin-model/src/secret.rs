//! [`Secret`] — a string that stays out of logs.
//!
//! A newtype over [`secrecy::SecretString`] that Ferrofin uses for every
//! secret-bearing DTO field (access tokens, passwords, Quick Connect secrets).
//! Its `Debug` is redacted and the plaintext is only reachable through the
//! explicit, greppable [`Secret::expose`] — so accidentally logging a struct
//! that contains one cannot leak the value. The wrapped `SecretString` also
//! zeroizes the plaintext on drop.
//!
//! Serialization *does* expose the value (a client must receive its own token),
//! but the field must still present as a plain `"string"` in the OpenAPI spec —
//! so annotate DTO fields `#[schema(value_type = String)]` exactly as the
//! `Uuid`/`DateTime` fields already do (the OpenAPI contract is the API's law).

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A secret string that never appears in `Debug`/log output.
///
/// Construct with [`Secret::new`] or `.into()` from a `String`/`&str`; read the
/// plaintext only via [`Secret::expose`]. Equality compares the exposed bytes
/// (not constant-time — this is a leak guard, not a timing-attack defense).
#[derive(Clone, Default)]
pub struct Secret(SecretString);

impl Secret {
    /// Wraps a plaintext secret.
    #[must_use]
    pub fn new(plaintext: impl Into<String>) -> Self {
        Self(SecretString::from(plaintext.into()))
    }

    /// Exposes the plaintext. Every call is an intentional, reviewable
    /// disclosure — keep them at the boundary (SQL bind, outbound header, hash).
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.0.expose_secret() == other.0.expose_secret()
    }
}

impl Eq for Secret {}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Self(SecretString::from(s))
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Self(SecretString::from(s))
    }
}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.expose_secret())
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(SecretString::from(String::deserialize(deserializer)?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expose_round_trips() {
        assert_eq!(Secret::new("hunter2").expose(), "hunter2");
        assert_eq!(Secret::from("t".to_owned()).expose(), "t");
    }

    #[test]
    fn debug_is_redacted() {
        let s = Secret::new("super-secret-token");
        let rendered = format!("{s:?}");
        assert!(!rendered.contains("super-secret-token"));
        assert_eq!(rendered, "Secret([REDACTED])");
    }

    #[test]
    fn eq_compares_plaintext() {
        assert_eq!(Secret::new("a"), Secret::new("a"));
        assert_ne!(Secret::new("a"), Secret::new("b"));
    }

    #[test]
    fn serialize_exposes_deserialize_accepts() {
        let s = Secret::new("tok");
        assert_eq!(serde_json::to_string(&s).unwrap(), r#""tok""#);
        let back: Secret = serde_json::from_str(r#""tok""#).unwrap();
        assert_eq!(back.expose(), "tok");
    }

    #[test]
    fn default_is_empty() {
        assert_eq!(Secret::default().expose(), "");
    }
}
