//! Global cryptography constants (port of `MediaBrowser.Model.Cryptography.Constants`).

/// Global constants for Jellyfin cryptography.
///
/// These are surfaced as an associated-const bundle so a host can later expose
/// them as tunable settings without changing call sites.
pub struct Constants;

impl Constants {
    /// The default length for new salts (`128 / 8` = 16 bytes).
    pub const DEFAULT_SALT_LENGTH: usize = 128 / 8;

    /// The default output (hash) length (`512 / 8` = 64 bytes).
    pub const DEFAULT_OUTPUT_LENGTH: usize = 512 / 8;

    /// The default iteration count for hashing passwords.
    pub const DEFAULT_ITERATIONS: u32 = 210_000;
}
