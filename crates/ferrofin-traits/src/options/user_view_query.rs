//! Port of `MediaBrowser.Model.Library.UserViewQuery`.

use ferrofin_model::data::CollectionType;

/// What `UserViewManager.GetUserViews` lists for a user, besides the user.
///
/// Mirrors C# `UserViewQuery`, whose constructor sets
/// `IncludeExternalContent = true` and an empty `PresetViews`; [`Default`]
/// reproduces exactly that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserViewQuery {
    /// Whether the channels and the Live TV view are listed.
    pub include_external_content: bool,
    /// Whether views the user hid from the home screen (`MyMediaExcludes`)
    /// are listed.
    pub include_hidden: bool,
    /// The library types listed as their shadow views (`GetShadowView`)
    /// rather than as the libraries themselves.
    pub preset_views: Vec<CollectionType>,
}

impl Default for UserViewQuery {
    fn default() -> Self {
        Self {
            include_external_content: true,
            include_hidden: false,
            preset_views: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::UserViewQuery;

    #[test]
    fn defaults_match_the_c_sharp_constructor() {
        let query = UserViewQuery::default();
        assert!(query.include_external_content);
        assert!(!query.include_hidden);
        assert_eq!(query.preset_views, []);
    }
}
