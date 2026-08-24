//! The request half of `UserViewManager::get_latest_items`.

use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::data::BaseItemKind;
use uuid::Uuid;

/// The C# manager's fallback when the request carries no `Limit`
/// (`UserViewManager.GetItemsForLatestItems`: `request.Limit ?? 10`). The
/// controller always sends one (its own default is 20), so this only matters
/// for in-process callers.
pub const LATEST_ITEMS_FALLBACK_LIMIT: i32 = 10;

/// Port of C# `MediaBrowser.Model.Querying.LatestItemsQuery` — the request
/// behind `GET /Items/Latest`.
///
/// The manager runs **one** query across every parent (the user's views, or
/// the `parent_id` folder), over-fetching `limit * 5` rows ordered
/// `DateCreated DESC, SortName DESC, ProductionYear DESC`, and then groups the
/// rows by their index container (episode → series, track → album, photo →
/// photo album) until `limit` groups exist. `is_played` is pushed into the SQL
/// (it needs `user`), exactly as upstream's `InternalItemsQuery.IsPlayed`.
#[derive(Debug, Clone)]
pub struct LatestItemsQuery {
    /// The requesting user (C# `User`). Scopes the played predicate and the
    /// user-specific query context; `None` only for user-less in-process calls.
    pub user: Option<UserEntity>,
    /// Localizes the search to one folder (C# `ParentId`; `None`/nil = the
    /// user's views). A library folder scopes by `TopParentId`; any other folder
    /// (a series, a season) scopes by the `AncestorIds` closure.
    pub parent_id: Option<Uuid>,
    /// Restrict the underlying rows to these kinds (C# `IncludeItemTypes`).
    pub include_item_types: Vec<BaseItemKind>,
    /// Filter by played state (C# `IsPlayed`). Cleared when the EXPLICIT
    /// `parent_id` is a music library (the user's-views fallback keeps it),
    /// as upstream orders the two steps.
    pub is_played: Option<bool>,
    /// The number of **groups** to return (C# `Limit`); the SQL over-fetches
    /// `limit * 5` rows. `None` falls back to [`LATEST_ITEMS_FALLBACK_LIMIT`]
    /// for the over-fetch and never stops the grouping early (C#'s
    /// `list.Count >= null` is always false).
    pub limit: Option<i32>,
    /// Whether to collapse rows into their index container (C# `GroupItems`,
    /// default `true`).
    pub group_items: bool,
    /// The view ids the user excluded from "latest" (C#
    /// `PreferenceKind.LatestItemExcludes`); only consulted when the parents
    /// are the user's views.
    pub latest_item_excludes: Vec<Uuid>,
}

impl Default for LatestItemsQuery {
    /// The C# property defaults: `GroupItems = true`, everything else unset.
    fn default() -> Self {
        Self {
            user: None,
            parent_id: None,
            include_item_types: Vec::new(),
            is_played: None,
            limit: None,
            group_items: true,
            latest_item_excludes: Vec::new(),
        }
    }
}
