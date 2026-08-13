//! The request half of `UserViewManager::get_latest_items`.

use ferrofin_model::data::BaseItemKind;
use uuid::Uuid;

/// Port of C# `MediaBrowser.Model.Querying.LatestItemsQuery` — the request the
/// "Latest media" rows make, scoped per view.
///
/// `is_played` is deliberately absent: the played post-filter needs per-user
/// user-data rows, which the API layer already batch-loads (the C# query pushes
/// it into SQL; the portable seam applies it over the flat rows).
#[derive(Debug, Clone, Default)]
pub struct LatestItemsQuery {
    /// The user whose views/latest rows are requested.
    pub user_id: Uuid,
    /// When set, only this view's group is returned.
    pub parent_id: Option<Uuid>,
    /// Restrict the underlying rows to these kinds (C# `IncludeItemTypes`).
    pub include_item_types: Vec<BaseItemKind>,
    /// The number of items the caller will keep after grouping. The manager
    /// over-fetches (C# queries `limit * 2` before grouping) so post-filters
    /// don't starve the page.
    pub limit: i32,
}
