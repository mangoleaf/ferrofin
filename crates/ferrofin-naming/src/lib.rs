//! Filename → media parsing for Ferrofin — port of Jellyfin's `Emby.Naming`.
//!
//! Pure functions that classify paths into episodes/seasons/movies/years/
//! resolution etc. The regex and configuration tables in [`common`] are copied
//! byte-for-byte from the C# so behaviour matches Jellyfin exactly. Real dep
//! types ([`ferrofin_model::entities::ExtraType`],
//! [`ferrofin_model::entities::SeriesStatus`],
//! [`ferrofin_model::dlna::DlnaProfileType`],
//! [`ferrofin_model::data::CollectionType`],
//! [`ferrofin_model::globalization::CultureDto`]) are reused from `ferrofin-model`;
//! the `FileSystemMetadata` POCO and the localization seam have no `ferrofin-model`
//! analogue and are defined locally ([`io`], [`external_files`]).

pub mod audio;
pub mod audiobook;
pub mod book;
pub mod common;
pub mod external_files;
pub mod io;
mod path;
pub mod tv;
pub mod video;
