//! `MetadataEditorInfo` — port of the matching type in `MediaBrowser.Model.Dto`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::data::CollectionType;
use crate::dto::NameValuePair;
use crate::entities_media::ParentalRating;
use crate::globalization::{CountryInfo, CultureDto};
use crate::providers::ExternalIdInfo;

/// The reference data a client needs to render an item's metadata editor.
///
/// Port of `MediaBrowser.Model.Dto.MetadataEditorInfo`, returned by
/// `GET /Items/{itemId}/MetadataEditor`. Every list defaults empty (mirroring the
/// C# constructor), so a freshly-built value serializes as the "no options"
/// descriptor.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct MetadataEditorInfo {
    /// Gets or sets the parental rating options.
    pub parental_rating_options: Vec<ParentalRating>,

    /// Gets or sets the countries.
    pub countries: Vec<CountryInfo>,

    /// Gets or sets the cultures.
    pub cultures: Vec<CultureDto>,

    /// Gets or sets the external id infos.
    pub external_id_infos: Vec<ExternalIdInfo>,

    /// Gets or sets the content type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<CollectionType>,

    /// Gets or sets the content type options.
    pub content_type_options: Vec<NameValuePair>,
}
