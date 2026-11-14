//! The `BaseItems` columns Jellyfin writes a "none" value for, read back as no
//! value.
//!
//! `BaseItemMapper.Map` (v12.4 `BaseItemMapper.cs:306-414`) writes every
//! column on every save, so an item with no channel or no genres still stores
//! the property's default: the zero GUID for `ChannelId` (and for
//! `SeriesId`/`SeasonId` on the kinds that have them) and `''` for the joined
//! lists. Ferrofin writes the same values, and an adopted database already
//! holds them. These types read them as `None`, which is what each one means,
//! for the `FromRow` structs' `#[sqlx(try_from = "…")]` fields: no reader may
//! take the zero GUID for a channel or a series, nor an empty list for a
//! changed one.
//!
//! `Width`/`Height` `0` and `MediaType` `Unknown` are not normalised: they are
//! values in their own right, which the guide writes and compares as such
//! (`ferrofin-livetv`'s programme projection), and the readers already treat
//! them as no size and no media type.

use sqlx::error::BoxDynError;
use sqlx::sqlite::{Sqlite, SqliteTypeInfo, SqliteValueRef};
use sqlx::{Decode, Type};

/// The zero GUID as Jellyfin stores it ([`crate::store::guid_to_db`] of
/// `Uuid::nil()`): what a `Guid` column holds for "none".
pub const NIL_GUID: &str = "00000000-0000-0000-0000-000000000000";

/// A nullable `TEXT` `Guid` column: NULL, `''` and the zero GUID (any case)
/// read as `None`. Converts into the field's `Option<String>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonNilGuid(Option<String>);

/// A nullable `TEXT` column: NULL and `''` read as `None`. Converts into the
/// field's `Option<String>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmptyText(Option<String>);

/// Implements `Type`/`Decode` for a wrapper of `Option<String>` that keeps a
/// decoded value only when `$keep` accepts it.
macro_rules! unset_reads_as_none {
    ($wrapper:ident, $keep:expr) => {
        impl Type<Sqlite> for $wrapper {
            fn type_info() -> SqliteTypeInfo {
                <String as Type<Sqlite>>::type_info()
            }

            fn compatible(ty: &SqliteTypeInfo) -> bool {
                <String as Type<Sqlite>>::compatible(ty)
            }
        }

        impl<'r> Decode<'r, Sqlite> for $wrapper {
            fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
                let keep: fn(&String) -> bool = $keep;
                <Option<String> as Decode<'r, Sqlite>>::decode(value).map(|v| Self(v.filter(keep)))
            }
        }

        impl From<$wrapper> for Option<String> {
            fn from(value: $wrapper) -> Self {
                value.0
            }
        }
    };
}

unset_reads_as_none!(NonNilGuid, |v| !v.is_empty()
    && !v.eq_ignore_ascii_case(NIL_GUID));
unset_reads_as_none!(NonEmptyText, |v| !v.is_empty());

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    #[derive(sqlx::FromRow)]
    struct Row {
        #[sqlx(try_from = "NonNilGuid")]
        guid: Option<String>,
        #[sqlx(try_from = "NonEmptyText")]
        text: Option<String>,
    }

    async fn read(sql: &'static str) -> Row {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .expect("pool");
        sqlx::query_as::<_, Row>(sql)
            .fetch_one(&pool)
            .await
            .expect("row")
    }

    #[tokio::test]
    async fn defaults_and_nulls_read_as_none() {
        for sql in [
            "SELECT NULL AS guid, NULL AS text",
            "SELECT '' AS guid, '' AS text",
            "SELECT '00000000-0000-0000-0000-000000000000' AS guid, '' AS text",
        ] {
            let row = read(sql).await;
            assert_eq!((row.guid, row.text), (None, None), "{sql}");
        }
    }

    #[tokio::test]
    async fn values_read_as_they_are() {
        let row =
            read("SELECT '0A0B0000-0000-0000-0000-000000000000' AS guid, 'Drama|Comedy' AS text")
                .await;
        assert_eq!(
            row.guid.as_deref(),
            Some("0A0B0000-0000-0000-0000-000000000000")
        );
        assert_eq!(row.text.as_deref(), Some("Drama|Comedy"));
    }
}
