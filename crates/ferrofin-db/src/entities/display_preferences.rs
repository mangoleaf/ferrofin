//! `FromRow` structs for the display-preferences-area tables —
//! `DisplayPreferences` and its `HomeSection` children, plus the standalone
//! `ItemDisplayPreferences` and `CustomItemDisplayPreferences` tables.
//!
//! Each struct mirrors one table one-to-one: field names and order match the
//! columns in `migrations/0001_initial.sql`. Enum-valued columns are stored as
//! `INTEGER` discriminants and are kept as [`i32`] here; the conversion layer
//! maps them onto the [`crate::enums`] / `ferrofin-model` enum types. `Guid`
//! columns are `TEXT` and kept as [`String`] (the hyphenated stored form; the
//! conversion layer parses them into `Uuid`).

/// A row of the `DisplayPreferences` table — a user's per-client, per-item
/// display settings (theme, scroll direction, skip lengths, and so on).
///
/// `ScrollDirection` (`ScrollDirection`) and `IndexBy` (`IndexingKind`) are
/// stored as `INTEGER` discriminants and kept here as [`i32`].
// A 1:1 mirror of the `DisplayPreferences` table; its several boolean toggles
// are intrinsic to the schema, not a refactorable design.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct DisplayPreferencesEntity {
    /// Surrogate primary key (`Id`).
    pub id: i64,
    /// The Chromecast receiver version (`ChromecastVersion`).
    pub chromecast_version: i32,
    /// The client these preferences apply to (`Client`).
    pub client: String,
    /// The dashboard theme (`DashboardTheme`), if set.
    pub dashboard_theme: Option<String>,
    /// Whether the next-video info overlay is shown
    /// (`EnableNextVideoInfoOverlay`).
    pub enable_next_video_info_overlay: bool,
    /// The indexing-kind discriminant (`IndexBy`), if set.
    pub index_by: Option<i32>,
    /// The item these preferences apply to as a `Guid`, hyphenated (`ItemId`).
    pub item_id: String,
    /// The scroll-direction discriminant (`ScrollDirection`).
    pub scroll_direction: i32,
    /// Whether the backdrop is shown (`ShowBackdrop`).
    pub show_backdrop: bool,
    /// Whether the sidebar is shown (`ShowSidebar`).
    pub show_sidebar: bool,
    /// The rewind skip length, in seconds (`SkipBackwardLength`).
    pub skip_backward_length: i32,
    /// The fast-forward skip length, in seconds (`SkipForwardLength`).
    pub skip_forward_length: i32,
    /// The TV home layout (`TvHome`), if set.
    pub tv_home: Option<String>,
    /// The owning user's `Guid`, hyphenated (`UserId`, FK → `Users`).
    pub user_id: String,
}

/// A row of the `HomeSection` table — one configured section of a user's home
/// screen, owned by a [`DisplayPreferencesEntity`].
///
/// `Type` (`HomeSectionType`) is stored as an `INTEGER` discriminant and kept
/// here as [`i32`].
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct HomeSectionEntity {
    /// Surrogate primary key (`Id`).
    pub id: i64,
    /// The owning display-preferences row's id (`DisplayPreferencesId`,
    /// FK → `DisplayPreferences`).
    pub display_preferences_id: i32,
    /// The section's position on the home screen (`Order`).
    pub order: i32,
    /// The home-section-type discriminant (`Type`).
    #[sqlx(rename = "Type")]
    pub type_: i32,
}

/// A row of the `ItemDisplayPreferences` table — a user's per-client sorting
/// and indexing preferences for a specific item.
///
/// `IndexBy` (`IndexingKind`), `SortOrder` (`SortOrder`), and `ViewType`
/// (`ViewType`) are stored as `INTEGER` discriminants and kept here as [`i32`].
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct ItemDisplayPreferencesEntity {
    /// Surrogate primary key (`Id`).
    pub id: i64,
    /// The client these preferences apply to (`Client`).
    pub client: String,
    /// The indexing-kind discriminant (`IndexBy`), if set.
    pub index_by: Option<i32>,
    /// The item these preferences apply to as a `Guid`, hyphenated (`ItemId`).
    pub item_id: String,
    /// Whether the chosen indexing is remembered (`RememberIndexing`).
    pub remember_indexing: bool,
    /// Whether the chosen sorting is remembered (`RememberSorting`).
    pub remember_sorting: bool,
    /// The field to sort by (`SortBy`).
    pub sort_by: String,
    /// The sort-order discriminant (`SortOrder`).
    pub sort_order: i32,
    /// The owning user's `Guid`, hyphenated (`UserId`, FK → `Users`).
    pub user_id: String,
    /// The view-type discriminant (`ViewType`).
    pub view_type: i32,
}

/// A row of the `CustomItemDisplayPreferences` table — an arbitrary
/// key/value display preference scoped to a user, item, and client.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
pub struct CustomItemDisplayPreferencesEntity {
    /// Surrogate primary key (`Id`).
    pub id: i64,
    /// The client this preference applies to (`Client`).
    pub client: String,
    /// The item this preference applies to as a `Guid`, hyphenated (`ItemId`).
    pub item_id: String,
    /// The preference key (`Key`).
    pub key: String,
    /// The owning user's `Guid`, hyphenated (`UserId`).
    pub user_id: String,
    /// The stored preference value (`Value`), if any.
    pub value: Option<String>,
}
