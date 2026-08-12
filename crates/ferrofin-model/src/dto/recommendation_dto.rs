//! `RecommendationDto` — port of `MediaBrowser.Model.Dto.RecommendationDto`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{BaseItemDto, RecommendationType};

/// A group of recommended items sharing a recommendation rationale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct RecommendationDto {
    /// Gets or sets the recommended items.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<BaseItemDto>>,

    /// Gets or sets the recommendation type.
    pub recommendation_type: RecommendationType,

    /// Gets or sets the baseline item name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_item_name: Option<String>,

    /// Gets or sets the category identifier.
    #[schema(value_type = String, format = "uuid")]
    pub category_id: Uuid,
}
