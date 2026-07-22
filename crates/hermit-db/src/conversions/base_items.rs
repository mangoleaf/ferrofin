//! Conversions for the base-item entities.
//!
//! - `(PeopleEntity, PeopleBaseItemMapEntity)` → [`BaseItemPerson`]
//! - [`BaseItemImageInfoEntity`] → [`ImageInfo`]

use hermit_model::data::PersonKind;
use hermit_model::dto::{BaseItemPerson, ImageInfo};
use hermit_model::entities::ImageType;

use crate::conversions::parse_guid;
use crate::entities::base_items::{BaseItemImageInfoEntity, PeopleBaseItemMapEntity, PeopleEntity};
use crate::error::DbError;

/// A person (`Peoples`) paired with one of their credits
/// (`PeopleBaseItemMap`) on a specific item.
///
/// A local wrapper so the join of the two entity rows can implement
/// [`TryFrom`] for the foreign [`BaseItemPerson`] DTO (the orphan rule forbids
/// implementing it directly for a bare `(A, B)` tuple).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonCredit {
    /// The person's identity row.
    pub person: PeopleEntity,
    /// The credit linking the person to an item (supplies the `Role`).
    pub credit: PeopleBaseItemMapEntity,
}

impl TryFrom<PersonCredit> for BaseItemPerson {
    type Error = DbError;

    /// Builds a `BaseItemPerson` from the person (`Peoples`) and the credit
    /// (`PeopleBaseItemMap`) that links them to an item.
    ///
    /// The person supplies the identity (`Id`, `Name`, `Type`); the credit row
    /// supplies the credited `Role`. Image fields have no source column and are
    /// left `None`.
    ///
    /// # Errors
    /// Returns [`DbError::InvalidGuid`] if the person's stored `Id` is not a
    /// valid `Guid`.
    fn try_from(value: PersonCredit) -> Result<Self, Self::Error> {
        let PersonCredit { person, credit } = value;
        Ok(Self {
            name: Some(person.name),
            id: parse_guid("Peoples.Id", &person.id)?,
            role: Some(credit.role),
            type_: person
                .person_type
                .as_deref()
                .map_or(PersonKind::Unknown, person_kind_from_str),
            primary_image_tag: None,
            image_blur_hashes: None,
        })
    }
}

impl TryFrom<BaseItemImageInfoEntity> for ImageInfo {
    type Error = DbError;

    /// Maps a stored `BaseItemImageInfos` row onto the wire DTO.
    ///
    /// The blurhash is stored as the raw bytes of its string form and decoded
    /// UTF-8-lossily. `Size` is not stored on the row, so it is `0`. Pixel
    /// dimensions that overflow [`i32`] become `None` (the stored form is
    /// [`i64`]; real image dimensions never overflow `i32`).
    ///
    /// # Errors
    /// Returns [`DbError::InvalidEnumValue`] if the stored `ImageType`
    /// discriminant is out of range.
    fn try_from(entity: BaseItemImageInfoEntity) -> Result<Self, Self::Error> {
        Ok(Self {
            image_type: image_type_from_i32(entity.image_type)?,
            image_index: None,
            image_tag: None,
            path: Some(entity.path),
            blur_hash: entity
                .blurhash
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
            height: i32::try_from(entity.height).ok(),
            width: i32::try_from(entity.width).ok(),
            size: 0,
        })
    }
}

/// Maps a stored `PersonType` string onto a [`PersonKind`].
///
/// The strings are the wire-contract `PersonKind` names (PascalCase); an
/// unrecognized value maps to [`PersonKind::Unknown`], matching the upstream
/// tolerant behaviour.
fn person_kind_from_str(value: &str) -> PersonKind {
    match value {
        "Actor" => PersonKind::Actor,
        "Director" => PersonKind::Director,
        "Composer" => PersonKind::Composer,
        "Writer" => PersonKind::Writer,
        "GuestStar" => PersonKind::GuestStar,
        "Producer" => PersonKind::Producer,
        "Conductor" => PersonKind::Conductor,
        "Lyricist" => PersonKind::Lyricist,
        "Arranger" => PersonKind::Arranger,
        "Engineer" => PersonKind::Engineer,
        "Mixer" => PersonKind::Mixer,
        "Remixer" => PersonKind::Remixer,
        "Creator" => PersonKind::Creator,
        "Artist" => PersonKind::Artist,
        "AlbumArtist" => PersonKind::AlbumArtist,
        "Author" => PersonKind::Author,
        "Illustrator" => PersonKind::Illustrator,
        "Penciller" => PersonKind::Penciller,
        "Inker" => PersonKind::Inker,
        "Colorist" => PersonKind::Colorist,
        "Letterer" => PersonKind::Letterer,
        "CoverArtist" => PersonKind::CoverArtist,
        "Editor" => PersonKind::Editor,
        "Translator" => PersonKind::Translator,
        "Narrator" => PersonKind::Narrator,
        _ => PersonKind::Unknown,
    }
}

/// Reads an [`ImageType`] from its stored `INTEGER` discriminant (0-based,
/// matching the C# `ImageType` declaration order mirrored by the target enum).
///
/// # Errors
/// Returns [`DbError::InvalidEnumValue`] for a discriminant outside `0..=12`.
fn image_type_from_i32(value: i32) -> Result<ImageType, DbError> {
    let kind = match value {
        0 => ImageType::Primary,
        1 => ImageType::Art,
        2 => ImageType::Backdrop,
        3 => ImageType::Banner,
        4 => ImageType::Logo,
        5 => ImageType::Thumb,
        6 => ImageType::Disc,
        7 => ImageType::Box,
        8 => ImageType::Screenshot,
        9 => ImageType::Menu,
        10 => ImageType::Chapter,
        11 => ImageType::BoxRear,
        12 => ImageType::Profile,
        other => {
            return Err(DbError::InvalidEnumValue {
                enum_name: "ImageType",
                value: other,
            });
        }
    };
    Ok(kind)
}
