//! Service **parameter/option** types — the request-shaped structs that the
//! manager traits accept and that carry the crate's real, testable logic.
//!
//! These are ports of the C# option classes that live in
//! `MediaBrowser.Controller` (not `MediaBrowser.Model`), i.e. they are internal
//! to the service layer rather than wire DTOs:
//!
//! | C# | Rust |
//! |----|------|
//! | `Dto/DtoOptions` | [`DtoOptions`] |
//! | `Entities/InternalItemsQuery` | [`InternalItemsQuery`] |
//! | `Entities/InternalPeopleQuery` | [`InternalPeopleQuery`] |
//! | `Drawing/ImageProcessingOptions` | [`ImageProcessingOptions`] |
//! | `Drawing/ImageCollageOptions` | [`ImageCollageOptions`] |
//! | `Library/DeleteOptions` | [`DeleteOptions`] |
//! | `Entities/ItemImageInfo` | [`ItemImageInfo`] |
//! | `Net/AuthorizationInfo` | [`AuthorizationInfo`] |
//!
//! Port rules applied throughout: C# domain `BaseItem` arguments become
//! [`uuid::Uuid`] identities (plus an [`ItemImageInfo`] where the image row is
//! needed); the C# `User` entity becomes [`ferrofin_db::entities::users::UserEntity`];
//! enums are reused from `ferrofin-model` rather than redeclared.

mod authorization_info;
mod delete_options;
mod dto_options;
mod image_processing_options;
mod internal_items_query;
mod internal_people_query;
mod item_image_info;
mod latest_items_query;

pub use authorization_info::AuthorizationInfo;
pub use delete_options::DeleteOptions;
pub use dto_options::DtoOptions;
pub use image_processing_options::{ImageCollageOptions, ImageProcessingOptions};
pub use internal_items_query::{InternalItemsQuery, SourceType};
pub use internal_people_query::InternalPeopleQuery;
pub use item_image_info::ItemImageInfo;
pub use latest_items_query::{LATEST_ITEMS_FALLBACK_LIMIT, LatestItemsQuery};
