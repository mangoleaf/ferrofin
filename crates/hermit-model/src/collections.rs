//! Port of `MediaBrowser.Model.Collections`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// The result of a collection creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct CollectionCreationResult {
    /// Gets or sets the collection id.
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,
}
