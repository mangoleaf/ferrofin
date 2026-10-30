//! The `BaseItems` rating and gain columns as Jellyfin 12.3 reads them.
//!
//! `BaseItem.CommunityRating`, `CriticRating`, `LUFS` and `NormalizationGain`
//! go through `FiniteOrNull` on every assignment, and `BaseItemMapper` assigns
//! them when it reads a row back. So a non-finite value a database already
//! holds (Jellyfin 12.2 could write an infinity; SQLite stores a NaN as NULL)
//! is read as no value. [`FiniteReal`] is that read, for the `FromRow`
//! structs' `#[sqlx(try_from = "FiniteReal")]` fields.

use ferrofin_util::finite::finite_or_none;
use sqlx::error::BoxDynError;
use sqlx::sqlite::{Sqlite, SqliteTypeInfo, SqliteValueRef};
use sqlx::{Decode, Type};

/// A nullable `REAL` column decoded through [`finite_or_none`]: NULL, NaN and
/// the infinities read as `None`. Converts into the field's `Option<f64>`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FiniteReal(Option<f64>);

impl Type<Sqlite> for FiniteReal {
    fn type_info() -> SqliteTypeInfo {
        <f64 as Type<Sqlite>>::type_info()
    }

    fn compatible(ty: &SqliteTypeInfo) -> bool {
        <f64 as Type<Sqlite>>::compatible(ty)
    }
}

impl<'r> Decode<'r, Sqlite> for FiniteReal {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        <Option<f64> as Decode<'r, Sqlite>>::decode(value).map(|v| Self(finite_or_none(v)))
    }
}

impl From<FiniteReal> for Option<f64> {
    fn from(value: FiniteReal) -> Self {
        value.0
    }
}
