//! Port of `MediaBrowser.Controller.Library.DeleteOptions`.

use serde::{Deserialize, Serialize};

/// Controls the side effects of deleting an item.
///
/// Mirrors C# `DeleteOptions`. Note the C# constructor default:
/// `DeleteFromExternalProvider` is `true`, while `DeleteFileLocation` is `false`
/// — [`Default`] reproduces exactly that, so `DeleteOptions::default()` deletes
/// the item from any external metadata provider but leaves the file on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DeleteOptions {
    /// Whether the underlying file on disk should also be removed.
    pub delete_file_location: bool,

    /// Whether the item should be removed from its external metadata provider.
    pub delete_from_external_provider: bool,
}

impl Default for DeleteOptions {
    fn default() -> Self {
        Self {
            delete_file_location: false,
            delete_from_external_provider: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DeleteOptions;

    #[test]
    fn default_matches_csharp_constructor() {
        let opts = DeleteOptions::default();
        assert!(!opts.delete_file_location);
        assert!(opts.delete_from_external_provider);
    }

    #[test]
    fn serde_round_trips() {
        let opts = DeleteOptions {
            delete_file_location: true,
            delete_from_external_provider: false,
        };
        let json = serde_json::to_string(&opts).expect("serialize");
        let back: DeleteOptions = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(opts, back);
    }

    #[test]
    fn serializes_pascal_case() {
        let json = serde_json::to_value(DeleteOptions::default()).expect("serialize");
        assert!(json.get("DeleteFileLocation").is_some());
        assert!(json.get("DeleteFromExternalProvider").is_some());
    }
}
