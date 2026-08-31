//! The `{Type}-{Name}` `PresentationUniqueKey` an item-by-name row carries, and
//! the boot-time repair that writes it onto rows an older Ferrofin left keyless.
//!
//! Five kinds override `BaseItem.CreatePresentationUniqueKey()` with
//! `GetUserDataKeys()[0]` — `GetType().Name + "-" + Name.RemoveDiacritics()`
//! (v10.11.8 `MediaBrowser.Controller/Entities/Genre.cs:37-47`, and the same
//! pair on `MusicGenre`/`Person`/`Studio`; `MusicArtist.cs:152` spells its
//! prefix `Artist`). Everything else keeps the base implementation, the row's
//! own id in the dashless `N` form.
//!
//! This module is the ONE spelling of that rule.
//! `ferrofin_core::kinds::presentation_unique_key` (the write path) and
//! [`backfill_by_name_presentation_keys`] (the repair path) both call
//! [`by_name_presentation_key`], so a backfilled row and a freshly inserted one
//! cannot disagree — which is exactly what went wrong when the repair was a
//! `lower(replace("Id", '-', ''))` in SQL: it wrote the row's own id where both
//! Ferrofin's inserts and Jellyfin write `Person-Bob Parity`.

use crate::Result;

/// The stored CLR type name of each kind that overrides the key, paired with
/// the prefix C# builds it from.
///
/// The prefix is the CLR **type** name, which is why `MusicArtist` is spelled
/// `Artist` (`MusicArtist.cs:152`). `Year` and every other item-by-name kind
/// keep the base `CreatePresentationUniqueKey()`, so they are deliberately
/// absent rather than derived from an "is by name" predicate.
const BY_NAME_PREFIXES: [(&str, &str); 5] = [
    ("MediaBrowser.Controller.Entities.Genre", "Genre"),
    (
        "MediaBrowser.Controller.Entities.Audio.MusicGenre",
        "MusicGenre",
    ),
    ("MediaBrowser.Controller.Entities.Person", "Person"),
    ("MediaBrowser.Controller.Entities.Studio", "Studio"),
    (
        "MediaBrowser.Controller.Entities.Audio.MusicArtist",
        "Artist",
    ),
];

/// The `PresentationUniqueKey` a by-name row of `stored_type` named `name`
/// carries, or `None` when the kind does not override the key (its key is its
/// own id, which the caller already has).
///
/// An empty name gives `None` too: C# would build the bare prefix `Genre-`,
/// which is not a distinguishing key, and `CreatePresentationUniqueKey`'s
/// own-id default is the honest answer for a nameless row.
#[must_use]
pub fn by_name_presentation_key(stored_type: &str, name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    BY_NAME_PREFIXES
        .iter()
        .find(|(type_name, _)| *type_name == stored_type)
        .map(|(_, prefix)| {
            format!(
                "{prefix}-{}",
                ferrofin_util::string_extensions::remove_diacritics(name)
            )
        })
}

/// Writes the `{Type}-{Name}` key onto every keyless item-by-name row.
///
/// Runs once per boot, right after the migration chain. It is the by-name half
/// of the repair migration `0028_presentation_key_backfill_all` performs for
/// every other kind: SQLite cannot express `RemoveDiacritics` (it is a Unicode
/// NFD fold plus a ligature map, `ferrofin_util::string_extensions`), so the
/// five kinds whose key is derived from their NAME are repaired here, where the
/// real function is in reach, instead of being guessed in SQL.
///
/// **Why any of this is load-bearing.** The recursive user universe is queried
/// with a bare `GROUP BY "PresentationUniqueKey"` — upstream's own
/// `dbQuery.GroupBy(e => e.PresentationUniqueKey)`
/// (`Jellyfin.Server.Implementations/Item/BaseItemRepository.cs:417`) — and
/// SQLite groups NULLs together. On a real 10.11.8 exactly one kind reaches
/// that `GROUP BY` keyless, `LiveTvProgram`, which is why an unfiltered
/// recursive page shows one `Program` and not the whole guide. Three older
/// Ferrofin write paths left by-name rows keyless too, and those rows then
/// shared the guide's NULL group: measured on the parity lab,
/// `GET /Items?userId=…&ids=<three Person ids>` answered with ONE person where
/// Jellyfin answered with three. The inserts are fixed; this repairs the rows
/// they already wrote.
///
/// Selecting on `Type` keeps the pass off a full table scan — it rides the
/// `(Type, CleanName)` index — and it is idempotent: a second boot matches no
/// rows.
///
/// # Errors
/// Returns [`DbError::Sqlx`](crate::DbError::Sqlx) if a statement fails.
pub async fn backfill_by_name_presentation_keys(writer: &sqlx::SqlitePool) -> Result<u64> {
    let mut placeholders = String::new();
    for i in 0..BY_NAME_PREFIXES.len() {
        if i > 0 {
            placeholders.push(',');
        }
        placeholders.push('?');
    }
    let select = format!(
        r#"SELECT "Id", "Type", "Name" FROM "BaseItems"
            WHERE ("PresentationUniqueKey" IS NULL OR "PresentationUniqueKey" = '')
              AND "Type" IN ({placeholders})"#
    );
    let mut query = sqlx::query_as::<_, (String, String, Option<String>)>(&select);
    for (type_name, _) in BY_NAME_PREFIXES {
        query = query.bind(type_name);
    }
    let rows = query.fetch_all(writer).await?;
    let mut repaired = 0_u64;
    for (id, stored_type, name) in rows {
        // A nameless by-name row falls back to `CreatePresentationUniqueKey()`'s
        // own-id default, the same value migration 0028 writes for every other
        // kind — anything is better than leaving it in the NULL group.
        let key = name
            .as_deref()
            .and_then(|n| by_name_presentation_key(&stored_type, n))
            .unwrap_or_else(|| id.replace('-', "").to_lowercase());
        let done =
            sqlx::query(r#"UPDATE "BaseItems" SET "PresentationUniqueKey" = ?1 WHERE "Id" = ?2"#)
                .bind(&key)
                .bind(&id)
                .execute(writer)
                .await?;
        repaired += done.rows_affected();
    }
    if repaired > 0 {
        tracing::info!(
            rows = repaired,
            "backfilled PresentationUniqueKey on item-by-name rows"
        );
    }
    Ok(repaired)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The five overriding kinds, spelled the way the oracle's own database
    /// spells them. `Béla Tarr` is the case migration 0027's SQL could never
    /// have produced and the reason this half is Rust: SQLite has no NFD fold.
    #[test]
    fn by_name_key_matches_the_oracle_spelling() {
        for (stored_type, name, expected) in [
            (
                "MediaBrowser.Controller.Entities.Genre",
                "Action",
                "Genre-Action",
            ),
            (
                "MediaBrowser.Controller.Entities.Audio.MusicGenre",
                "Ambient",
                "MusicGenre-Ambient",
            ),
            (
                "MediaBrowser.Controller.Entities.Person",
                "H. Jon Benjamin",
                "Person-H. Jon Benjamin",
            ),
            (
                "MediaBrowser.Controller.Entities.Person",
                "Béla Tarr",
                "Person-Bela Tarr",
            ),
            (
                "MediaBrowser.Controller.Entities.Studio",
                "Parity Pictures",
                "Studio-Parity Pictures",
            ),
            (
                // The CLR type name, not the stored path: `MusicArtist` is
                // spelled `Artist` (MusicArtist.cs:152).
                "MediaBrowser.Controller.Entities.Audio.MusicArtist",
                "Sigur Rós",
                "Artist-Sigur Ros",
            ),
        ] {
            assert_eq!(
                Some(expected.to_owned()),
                by_name_presentation_key(stored_type, name),
                "{stored_type} / {name}"
            );
        }
    }

    /// A kind that keeps `BaseItem.CreatePresentationUniqueKey()` has no by-name
    /// key — the caller uses the row's own id, which is what migration 0028
    /// writes for it.
    #[test]
    fn a_non_by_name_kind_has_no_by_name_key() {
        assert_eq!(
            None,
            by_name_presentation_key("MediaBrowser.Controller.Entities.Movies.Movie", "Inception")
        );
        // `Year` IS an `IItemByName` but overrides nothing.
        assert_eq!(
            None,
            by_name_presentation_key("MediaBrowser.Controller.Entities.Year", "1999")
        );
        // A nameless row falls back to the own-id default too.
        assert_eq!(
            None,
            by_name_presentation_key("MediaBrowser.Controller.Entities.Person", "")
        );
    }

    async fn keyless_row(db: &crate::Database, id: &str, stored_type: &str, name: &str) {
        sqlx::query(
            r#"INSERT INTO "BaseItems"
               ("Id","Type","Name","IsFolder","IsInMixedFolder","IsLocked",
                "IsMovie","IsRepeat","IsSeries","IsVirtualItem")
               VALUES (?1,?2,?3,0,0,0,0,0,0,0)"#,
        )
        .bind(id)
        .bind(stored_type)
        .bind(name)
        .execute(db.writer())
        .await
        .expect("keyless row");
    }

    async fn key_of(db: &crate::Database, id: &str) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>(
            r#"SELECT "PresentationUniqueKey" FROM "BaseItems" WHERE "Id" = ?1"#,
        )
        .bind(id)
        .fetch_one(db.pool())
        .await
        .expect("read back")
    }

    #[tokio::test]
    async fn backfill_keys_every_by_name_row_and_leaves_the_guide_alone() {
        let db = crate::Database::connect_in_memory().await.expect("db");
        db.run_migrations().await.expect("migrate");
        let person = "AAAAAAAA-0000-0000-0000-000000000001";
        let genre = "AAAAAAAA-0000-0000-0000-000000000002";
        let artist = "AAAAAAAA-0000-0000-0000-000000000003";
        let airing = "AAAAAAAA-0000-0000-0000-000000000004";
        keyless_row(
            &db,
            person,
            "MediaBrowser.Controller.Entities.Person",
            "Béla Tarr",
        )
        .await;
        keyless_row(
            &db,
            genre,
            "MediaBrowser.Controller.Entities.Audio.MusicGenre",
            "Ambient",
        )
        .await;
        keyless_row(
            &db,
            artist,
            "MediaBrowser.Controller.Entities.Audio.MusicArtist",
            "Sigur Rós",
        )
        .await;
        keyless_row(
            &db,
            airing,
            "MediaBrowser.Controller.LiveTv.LiveTvProgram",
            "The Nine O'Clock News",
        )
        .await;

        let repaired = backfill_by_name_presentation_keys(db.writer())
            .await
            .expect("backfill");
        assert_eq!(3, repaired, "only the by-name rows are repaired");
        assert_eq!(
            Some("Person-Bela Tarr".to_owned()),
            key_of(&db, person).await
        );
        assert_eq!(
            Some("MusicGenre-Ambient".to_owned()),
            key_of(&db, genre).await
        );
        assert_eq!(
            Some("Artist-Sigur Ros".to_owned()),
            key_of(&db, artist).await
        );
        // An airing stays keyless: upstream never writes one, and the NULL
        // group collapsing the guide into a single `Program` row is the
        // behaviour a recursive page depends on.
        assert_eq!(None, key_of(&db, airing).await);

        // Idempotent — a repaired database matches nothing on the next boot.
        assert_eq!(
            0,
            backfill_by_name_presentation_keys(db.writer())
                .await
                .expect("second pass")
        );
    }

    /// Migration 0028's own half: a keyless row of any OTHER kind gets the
    /// own-id default, and it is reached even with `TopParentId` NULL — the
    /// predicate that made migration 0027 miss every by-name row.
    #[tokio::test]
    async fn migration_0028_keys_a_parentless_non_by_name_row() {
        let db = crate::Database::connect_in_memory().await.expect("db");
        db.run_migrations().await.expect("migrate");
        let boxset = "AAAAAAAA-0000-0000-0000-00000000000B";
        keyless_row(
            &db,
            boxset,
            "MediaBrowser.Controller.Entities.Movies.BoxSet",
            "Orphan Collection",
        )
        .await;
        // Re-running the migration statement is what a fresh database sees at
        // boot; the row was written after the chain ran, so apply it directly.
        sqlx::query(include_str!(
            "../migrations/0028_presentation_key_backfill_all.sql"
        ))
        .execute(db.writer())
        .await
        .expect("0028");
        assert_eq!(
            Some("aaaaaaaa00000000000000000000000b".to_owned()),
            key_of(&db, boxset).await
        );
    }
}
