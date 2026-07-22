//! Conversions for the display-preferences entities.
//!
//! - [`DisplayPreferencesEntity`] → [`DisplayPreferencesDto`]

use std::collections::HashMap;

use hermit_model::dto::{DisplayPreferencesDto, ScrollDirection, SortOrder};

use crate::entities::display_preferences::DisplayPreferencesEntity;
use crate::enums::IndexingKind;
use crate::error::DbError;

impl TryFrom<DisplayPreferencesEntity> for DisplayPreferencesDto {
    type Error = DbError;

    /// Maps a stored `DisplayPreferences` row onto the wire DTO.
    ///
    /// Several DTO fields have no column on this table and take their defaults:
    /// `ViewType`/`SortBy`/`SortOrder`/`RememberSorting`/`RememberIndexing`
    /// live on `ItemDisplayPreferences`, `CustomPrefs` on
    /// `CustomItemDisplayPreferences`, and the primary-image dimensions are the
    /// DTO defaults (250). The associated `HomeSection` rows have no field in
    /// this DTO and are therefore not represented. The DTO's `Id` carries the
    /// owning user's `Guid` (kept as its stored string form).
    ///
    /// # Errors
    /// Returns [`DbError::InvalidEnumValue`] if the stored `IndexBy` or
    /// `ScrollDirection` discriminant is out of range.
    fn try_from(entity: DisplayPreferencesEntity) -> Result<Self, Self::Error> {
        let index_by = entity
            .index_by
            .map(|value| IndexingKind::try_from(value).map(indexing_kind_name))
            .transpose()?
            .map(str::to_owned);

        Ok(Self {
            id: Some(entity.user_id),
            view_type: None,
            sort_by: None,
            index_by,
            remember_indexing: false,
            primary_image_height: DisplayPreferencesDto::default().primary_image_height,
            primary_image_width: DisplayPreferencesDto::default().primary_image_width,
            custom_prefs: HashMap::new(),
            scroll_direction: scroll_direction_from_i32(entity.scroll_direction)?,
            show_backdrop: entity.show_backdrop,
            remember_sorting: false,
            sort_order: SortOrder::default(),
            show_sidebar: entity.show_sidebar,
            client: Some(entity.client),
        })
    }
}

/// The wire-contract string name for an [`IndexingKind`], matching the
/// `hermit-model` `IndexBy` string values.
fn indexing_kind_name(kind: IndexingKind) -> &'static str {
    match kind {
        IndexingKind::PremiereDate => "PremiereDate",
        IndexingKind::ProductionYear => "ProductionYear",
        IndexingKind::CommunityRating => "CommunityRating",
    }
}

/// Reads a [`ScrollDirection`] from its stored `INTEGER` discriminant
/// (Horizontal = 0, Vertical = 1).
///
/// # Errors
/// Returns [`DbError::InvalidEnumValue`] for a discriminant outside `0..=1`.
fn scroll_direction_from_i32(value: i32) -> Result<ScrollDirection, DbError> {
    let direction = match value {
        0 => ScrollDirection::Horizontal,
        1 => ScrollDirection::Vertical,
        other => {
            return Err(DbError::InvalidEnumValue {
                enum_name: "ScrollDirection",
                value: other,
            });
        }
    };
    Ok(direction)
}
