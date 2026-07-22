//! Port of `MediaBrowser.Controller.Dto.DtoOptions`.

use hermit_model::entities::ImageType;
use hermit_model::querying::ItemFields;
use serde::{Deserialize, Serialize};

/// Fields excluded from the "all fields" set by default (C#
/// `DtoOptions.DefaultExcludedFields`): these are expensive/situational and are
/// only populated when explicitly requested.
const DEFAULT_EXCLUDED_FIELDS: [ItemFields; 2] =
    [ItemFields::SeasonUserData, ItemFields::RefreshState];

/// Every [`ItemFields`] variant, in C# declaration order. Used to build the
/// "all fields" set; kept beside the enum's port so the two stay in lockstep.
const ALL_ITEM_FIELDS: [ItemFields; 49] = [
    ItemFields::AirTime,
    ItemFields::CanDelete,
    ItemFields::CanDownload,
    ItemFields::ChannelInfo,
    ItemFields::Chapters,
    ItemFields::Trickplay,
    ItemFields::ChildCount,
    ItemFields::CumulativeRunTimeTicks,
    ItemFields::CustomRating,
    ItemFields::DateCreated,
    ItemFields::DateLastMediaAdded,
    ItemFields::DisplayPreferencesId,
    ItemFields::Etag,
    ItemFields::ExternalUrls,
    ItemFields::Genres,
    ItemFields::ItemCounts,
    ItemFields::MediaSourceCount,
    ItemFields::MediaSources,
    ItemFields::OriginalTitle,
    ItemFields::Overview,
    ItemFields::ParentId,
    ItemFields::Path,
    ItemFields::People,
    ItemFields::PlayAccess,
    ItemFields::ProductionLocations,
    ItemFields::ProviderIds,
    ItemFields::PrimaryImageAspectRatio,
    ItemFields::RecursiveItemCount,
    ItemFields::Settings,
    ItemFields::SeriesStudio,
    ItemFields::SortName,
    ItemFields::SpecialEpisodeNumbers,
    ItemFields::Studios,
    ItemFields::Taglines,
    ItemFields::Tags,
    ItemFields::RemoteTrailers,
    ItemFields::MediaStreams,
    ItemFields::SeasonUserData,
    ItemFields::DateLastRefreshed,
    ItemFields::DateLastSaved,
    ItemFields::RefreshState,
    ItemFields::ChannelImage,
    ItemFields::EnableMediaSourceDisplay,
    ItemFields::Width,
    ItemFields::Height,
    ItemFields::ExtraIds,
    ItemFields::LocalTrailerCount,
    ItemFields::IsHd,
    ItemFields::SpecialFeatureCount,
];

/// Every [`ImageType`] variant. Mirrors C# `Enum.GetValues<ImageType>()`.
const ALL_IMAGE_TYPES: [ImageType; 13] = [
    ImageType::Primary,
    ImageType::Art,
    ImageType::Backdrop,
    ImageType::Banner,
    ImageType::Logo,
    ImageType::Thumb,
    ImageType::Disc,
    ImageType::Box,
    ImageType::Screenshot,
    ImageType::Menu,
    ImageType::Chapter,
    ImageType::BoxRear,
    ImageType::Profile,
];

/// Options controlling which fields and images are populated when building a
/// [`hermit_model::dto::BaseItemDto`].
///
/// Mirrors C# `DtoOptions`. The two C# constructors map to constructors here:
/// `new DtoOptions()` → [`DtoOptions::default`] (all fields on), and
/// `new DtoOptions(bool)` → [`DtoOptions::with_all_fields`]. `ImageTypeLimit`
/// defaults to `int.MaxValue` ([`i32::MAX`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
// The four toggles are intrinsic to the C# `DtoOptions` contract, not a
// refactorable design.
#[allow(clippy::struct_excessive_bools)]
pub struct DtoOptions {
    /// The fields to populate on the DTO.
    pub fields: Vec<ItemFields>,

    /// The image types to populate on the DTO.
    pub image_types: Vec<ImageType>,

    /// The maximum number of images to return per image type.
    pub image_type_limit: i32,

    /// Whether image information is populated at all.
    pub enable_images: bool,

    /// Whether program recording information is populated.
    pub add_program_recording_info: bool,

    /// Whether user data is populated.
    pub enable_user_data: bool,

    /// Whether the currently-airing program is populated.
    pub add_current_program: bool,
}

impl DtoOptions {
    /// The set of all populatable fields, i.e. every [`ItemFields`] variant
    /// minus [`DEFAULT_EXCLUDED_FIELDS`]. Mirrors C# `AllItemFields`.
    #[must_use]
    pub fn all_fields() -> Vec<ItemFields> {
        ALL_ITEM_FIELDS
            .into_iter()
            .filter(|f| !DEFAULT_EXCLUDED_FIELDS.contains(f))
            .collect()
    }

    /// Constructs options, optionally pre-populating [`Self::fields`] with the
    /// full [`all_fields`](Self::all_fields) set. Mirrors C#
    /// `new DtoOptions(bool allFields)`.
    #[must_use]
    pub fn with_all_fields(all_fields: bool) -> Self {
        Self {
            fields: if all_fields {
                Self::all_fields()
            } else {
                Vec::new()
            },
            image_types: ALL_IMAGE_TYPES.to_vec(),
            image_type_limit: i32::MAX,
            enable_images: true,
            add_program_recording_info: false,
            enable_user_data: true,
            add_current_program: true,
        }
    }

    /// Returns whether the given field is in [`Self::fields`]. Mirrors C#
    /// `ContainsField`.
    #[must_use]
    pub fn contains_field(&self, field: ItemFields) -> bool {
        self.fields.contains(&field)
    }

    /// Returns the per-type image limit for `image_type`: [`Self::image_type_limit`]
    /// when images are enabled and the type is requested, otherwise `0`. Mirrors
    /// C# `GetImageLimit`.
    #[must_use]
    pub fn image_limit(&self, image_type: ImageType) -> i32 {
        if self.enable_images && self.image_types.contains(&image_type) {
            self.image_type_limit
        } else {
            0
        }
    }
}

impl Default for DtoOptions {
    /// All fields enabled — matches the parameterless C# `new DtoOptions()`,
    /// which delegates to `DtoOptions(true)`.
    fn default() -> Self {
        Self::with_all_fields(true)
    }
}

#[cfg(test)]
mod tests {
    use super::{DtoOptions, ImageType, ItemFields};

    #[test]
    fn default_enables_all_fields_and_images() {
        let opts = DtoOptions::default();
        assert_eq!(opts.image_type_limit, i32::MAX);
        assert!(opts.enable_images);
        assert!(opts.enable_user_data);
        assert!(opts.add_current_program);
        assert!(!opts.add_program_recording_info);
        assert!(opts.contains_field(ItemFields::Overview));
    }

    #[test]
    fn all_fields_excludes_the_default_excluded() {
        let fields = DtoOptions::all_fields();
        assert!(!fields.contains(&ItemFields::SeasonUserData));
        assert!(!fields.contains(&ItemFields::RefreshState));
        assert!(fields.contains(&ItemFields::Genres));
        // 49 total variants minus the 2 excluded.
        assert_eq!(fields.len(), 47);
    }

    #[test]
    fn with_all_fields_false_is_empty() {
        let opts = DtoOptions::with_all_fields(false);
        assert!(opts.fields.is_empty());
        assert!(!opts.contains_field(ItemFields::Overview));
        // Image types are still fully populated regardless of the field toggle.
        assert!(!opts.image_types.is_empty());
    }

    #[test]
    fn image_limit_respects_enable_and_membership() {
        let opts = DtoOptions::default();
        assert_eq!(opts.image_limit(ImageType::Primary), i32::MAX);

        let mut disabled = opts.clone();
        disabled.enable_images = false;
        assert_eq!(disabled.image_limit(ImageType::Primary), 0);

        let narrowed = DtoOptions {
            image_types: vec![ImageType::Backdrop],
            ..Default::default()
        };
        assert_eq!(narrowed.image_limit(ImageType::Primary), 0);
        assert_eq!(narrowed.image_limit(ImageType::Backdrop), i32::MAX);
    }

    #[test]
    fn serde_round_trips() {
        let opts = DtoOptions::with_all_fields(false);
        let json = serde_json::to_string(&opts).expect("serialize");
        let back: DtoOptions = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(opts, back);
    }
}
