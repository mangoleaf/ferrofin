//! DTO-projection **service** trait — the presentation seam.
//!
//! Port of `IDtoService` in `MediaBrowser.Controller.Dto`. This is the layer
//! that turns persisted [`BaseItemEntity`] rows into the wire-shaped
//! [`BaseItemDto`] the API returns, honoring the field/image toggles carried by
//! [`DtoOptions`].
//!
//! Port rules applied:
//! - Item **identity** / owner arguments become [`uuid::Uuid`]; the C# `BaseItem
//!   Item`/`BaseItem? owner` domain objects are not ported.
//! - The `User?` argument becomes an optional [`UserEntity`] row (the domain
//!   `User` maps to the persistence entity per the crate-wide rule).
//! - Projected results are `ferrofin-model` DTOs ([`BaseItemDto`]).
//! - The `IReadOnlyList<BaseItemDto> GetBaseItemDtos` overload collapses to a
//!   single slice-in / `Vec`-out method; the `skipVisibilityCheck` flag is kept
//!   because it changes behaviour rather than being an OOP-tree artifact.
//! - Synchronous C# methods become `async fn -> Result<_, ServiceError>`, since
//!   a real projection touches image/user-data lookups behind the trait.
//!
//! The trait is object-safe (`AppState` holds it behind `Arc<dyn _>`) and
//! carries a [`_assert_object_safe_dto_service`] compile-time assertion.

use async_trait::async_trait;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::BaseItemDto;
use uuid::Uuid;

use crate::error::ServiceError;
use crate::options::DtoOptions;

/// Projects persisted item rows into wire-shaped [`BaseItemDto`]s.
///
/// Port of `IDtoService`. Implementations (Wave 6, `ferrofin-core`) resolve the
/// per-field/image toggles in [`DtoOptions`], fold in the requesting user's
/// play-state, and produce the presentation DTO the API serializes.
#[async_trait]
pub trait DtoService: Send + Sync {
    /// Gets the primary-image aspect ratio for an item, or `None` when it has
    /// no primary image. Port of `GetPrimaryImageAspectRatio(BaseItem)`; the
    /// domain item becomes an [`item_id`](Uuid).
    async fn get_primary_image_aspect_ratio(
        &self,
        item_id: Uuid,
    ) -> Result<Option<f64>, ServiceError>;

    /// Projects a single item row into a [`BaseItemDto`].
    ///
    /// Port of `GetBaseItemDto(BaseItem, DtoOptions, User?, BaseItem? owner)`:
    /// the item is passed as its persisted [`BaseItemEntity`] row, the optional
    /// requesting user as an optional [`UserEntity`], and the optional owning
    /// item as an [`owner_id`](Uuid).
    async fn get_base_item_dto(
        &self,
        item: &BaseItemEntity,
        options: &DtoOptions,
        user: Option<&UserEntity>,
        owner_id: Option<Uuid>,
    ) -> Result<BaseItemDto, ServiceError>;

    /// Projects many item rows into [`BaseItemDto`]s in one pass.
    ///
    /// Port of `GetBaseItemDtos(...)`. `skip_visibility_check` skips the
    /// redundant per-item visibility filter when the caller has already
    /// filtered the input set.
    async fn get_base_item_dtos(
        &self,
        items: &[BaseItemEntity],
        options: &DtoOptions,
        user: Option<&UserEntity>,
        owner_id: Option<Uuid>,
        skip_visibility_check: bool,
    ) -> Result<Vec<BaseItemDto>, ServiceError>;

    /// Projects an item-by-name row (a genre/studio/person/…) into a
    /// [`BaseItemDto`], counting the items tagged with it.
    ///
    /// Port of `GetItemByNameDto(BaseItem, DtoOptions, List<BaseItem>?
    /// taggedItems, User?)`; the tagged items are passed as their id list.
    async fn get_item_by_name_dto(
        &self,
        item: &BaseItemEntity,
        options: &DtoOptions,
        tagged_item_ids: Option<&[Uuid]>,
        user: Option<&UserEntity>,
    ) -> Result<BaseItemDto, ServiceError>;
}

/// Compile-time assertion that [`DtoService`] is object-safe, so it can be
/// stored as `Arc<dyn DtoService>` in `AppState`.
fn _assert_object_safe_dto_service(_: &dyn DtoService) {}
