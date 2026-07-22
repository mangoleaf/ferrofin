//! Filename → media parsing for Hermit — port of Jellyfin's `Emby.Naming`.
//!
//! Pure functions that classify paths into episodes/seasons/movies/years/
//! resolution etc. The regex and configuration tables in [`common`] are copied
//! byte-for-byte from the C# so behaviour matches Jellyfin exactly. Real dep
//! types ([`hermit_model::entities::ExtraType`],
//! [`hermit_model::entities::SeriesStatus`],
//! [`hermit_model::dlna::DlnaProfileType`],
//! [`hermit_model::data::CollectionType`],
//! [`hermit_model::globalization::CultureDto`]) are reused from `hermit-model`;
//! the `FileSystemMetadata` POCO and the localization seam have no `hermit-model`
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
