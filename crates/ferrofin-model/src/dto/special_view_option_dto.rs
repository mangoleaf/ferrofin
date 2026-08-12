//! `SpecialViewOptionDto` — port of the matching type in
//! `MediaBrowser.Model.Library`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A grouping-eligible library view offered by
/// `GET /UserViews/GroupingOptions`.
///
/// Port of `MediaBrowser.Model.Library.SpecialViewOptionDto`. Both properties
/// are nullable strings in the contract; `id` is the view's guid rendered
/// without dashes (the C# `Id.ToString("N")`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct SpecialViewOptionDto {
    /// Gets or sets view option name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets view option id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}
