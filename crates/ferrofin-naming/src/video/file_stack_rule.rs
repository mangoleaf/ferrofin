//! Port of `Emby.Naming.Video.FileStackRule`.

use fancy_regex::Regex;

/// Result of a successful [`FileStackRule::match_input`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStackMatch {
    /// The stack name (the `filename` capture group).
    pub stack_name: String,
    /// The part type (the `parttype` group, or `"unknown"` when absent).
    pub part_type: String,
    /// The part number (the `number` capture group).
    pub part_number: String,
}

/// Regex based rule for file stacking (e.g. `disc1`, `disc2`).
#[derive(Debug, Clone)]
pub struct FileStackRule {
    token_regex: Regex,
    /// Whether the rule uses numerical or alphabetical numbering.
    pub is_numerical: bool,
}

impl FileStackRule {
    /// Creates a new [`FileStackRule`] from a regex token.
    ///
    /// # Panics
    ///
    /// Panics if `token` is not a valid regex; the token strings are the
    /// vendored byte-for-byte tables from `NamingOptions`, so this is a compile
    /// invariant, not a runtime path.
    #[must_use]
    pub fn new(token: &str, is_numerical: bool) -> Self {
        let token_regex = Regex::new(&format!("(?i){token}"))
            .expect("NamingOptions VideoFileStackingRules regex is valid");
        Self {
            token_regex,
            is_numerical,
        }
    }

    /// Matches the input against the rule regex.
    #[must_use]
    pub fn match_input(&self, input: &str) -> Option<FileStackMatch> {
        let captures = self.token_regex.captures(input).ok().flatten()?;

        let part_type = captures
            .name("parttype")
            .map_or_else(|| "unknown".to_string(), |m| m.as_str().to_string());
        Some(FileStackMatch {
            stack_name: captures
                .name("filename")
                .map_or_else(String::new, |m| m.as_str().to_string()),
            part_type,
            part_number: captures
                .name("number")
                .map_or_else(String::new, |m| m.as_str().to_string()),
        })
    }
}
