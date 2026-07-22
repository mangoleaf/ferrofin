//! Port of `Emby.Naming.Video.ExtraRuleType`.

/// Determines against what an [`crate::video::ExtraRule`] token is matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtraRuleType {
    /// Match the token against a suffix in the file name.
    Suffix = 0,
    /// Match the token against the file name, excluding the file extension.
    Filename = 1,
    /// Match the token (as a regex) against the file name, including extension.
    Regex = 2,
    /// Match the token against the name of the directory containing the file.
    DirectoryName = 3,
}
