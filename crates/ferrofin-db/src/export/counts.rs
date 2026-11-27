//! The rows Jellyfin's code migrations change on its first start, counted on
//! the staged copy for the loss report (gl-1902): what an operator cannot see
//! from the routines' names. Each count is the rows its routine deletes or
//! rewrites, read as the routine reads them (Jellyfin `v12.4`,
//! `Jellyfin.Server/Migrations/Routines/`). A database without the table a
//! count reads counts none.

use std::collections::HashMap;

use sqlx::SqliteConnection;

use super::paths::has_table;
use crate::Result;

/// Jellyfin's placeholder item, which holds the play state of items removed
/// from the library (`BaseItemRepository.PlaceholderId`).
const PLACEHOLDER_ID: &str = "00000000-0000-0000-0000-000000000001";

/// The rows each counted code migration changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FirstStartCounts {
    /// `BaseItems` rows sharing a path with another, beyond the first of each
    /// path: what `FixIncorrectOwnerIdRelationships` deletes. Counted whether
    /// or not the history leaves it to run: recorded as applied, it leaves
    /// them.
    pub shared_paths: u64,
    /// `MusicArtist` rows whose name equals another's ignoring case, beyond the
    /// first of each name: what `MergeDuplicateMusicArtists` merges and deletes.
    pub artists: u64,
    /// The same for `Person` rows: what `MergeDuplicatePeople` merges and
    /// deletes.
    pub persons: u64,
    /// The same for `Peoples` rows of one person type, which
    /// `MergeDuplicatePeople` merges too.
    pub credits: u64,
    /// A user's play-state rows of one item that disagree on the position, the
    /// play count, played, favourite or the last played date, beyond the first
    /// of each item and user, the placeholder item's aside: what
    /// `HarmonizeConflictingUserData` gives one row's values.
    pub play_states: u64,
}

impl FirstStartCounts {
    /// What the code migration named `routine` (its id without the date)
    /// changes in this database, for the loss report; `None` for one this does
    /// not count.
    #[must_use]
    pub fn note(&self, routine: &str) -> Option<String> {
        Some(match routine {
            "FixIncorrectOwnerIdRelationships" => count(
                self.shared_paths,
                "item repeats another's path, which it would delete",
                "items repeat another's path, which it would delete",
            ),
            "MergeDuplicateMusicArtists" => count(
                self.artists,
                "artist differs from another only by case",
                "artists differ from others only by case",
            ),
            "MergeDuplicatePeople" => format!(
                "{} and {} differ from others only by case",
                count(self.persons, "person", "people"),
                count(self.credits, "credit", "credits")
            ),
            "HarmonizeConflictingUserData" => count(
                self.play_states,
                "play state disagrees with another of its user and item",
                "play states disagree with others of their user and item",
            ),
            _ => return None,
        })
    }
}

/// `n` and what it counts, `one` or `many`.
fn count(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Counts what Jellyfin's counted code migrations change in the staged
/// database.
pub(super) async fn read(conn: &mut SqliteConnection) -> Result<FirstStartCounts> {
    let mut counts = FirstStartCounts::default();
    if has_table(conn, "BaseItems").await? {
        counts.shared_paths = scalar(
            conn,
            r#"SELECT COALESCE(SUM("n" - 1), 0) FROM
                 (SELECT COUNT(*) AS "n" FROM "BaseItems" WHERE "Path" IS NOT NULL GROUP BY "Path")"#,
        )
        .await?;
        counts.artists =
            case_duplicates(conn, "MediaBrowser.Controller.Entities.Audio.MusicArtist").await?;
        counts.persons = case_duplicates(conn, "MediaBrowser.Controller.Entities.Person").await?;
    }
    if has_table(conn, "Peoples").await? {
        let rows: Vec<(String, Option<String>)> =
            sqlx::query_as(r#"SELECT "Name", "PersonType" FROM "Peoples""#)
                .fetch_all(&mut *conn)
                .await?;
        counts.credits = beyond_first(rows.into_iter().map(|(name, kind)| (lower(&name), kind)));
    }
    if has_table(conn, "UserData").await? {
        // `HarmonizeConflictingUserData`: a group of rows of one item and
        // user, any of whose values differ.
        // The placeholder's id has no hex letter, so its spelling is one.
        counts.play_states = sqlx::query_scalar::<_, i64>(
            r#"SELECT COALESCE(SUM("n" - 1), 0) FROM
                 (SELECT COUNT(*) AS "n" FROM "UserData" WHERE "ItemId" <> ?1
                  GROUP BY "ItemId", "UserId"
                  HAVING COUNT(*) > 1
                     AND (MIN("PlaybackPositionTicks") IS NOT MAX("PlaybackPositionTicks")
                       OR MIN("PlayCount") IS NOT MAX("PlayCount")
                       OR MIN("Played") IS NOT MAX("Played")
                       OR MIN("IsFavorite") IS NOT MAX("IsFavorite")
                       OR MIN("LastPlayedDate") IS NOT MAX("LastPlayedDate")))"#,
        )
        .bind(PLACEHOLDER_ID)
        .fetch_one(&mut *conn)
        .await?
        .try_into()
        .unwrap_or(0);
    }
    Ok(counts)
}

/// The rows of `type_` whose name equals another's ignoring case (Jellyfin
/// groups by `Name.ToLowerInvariant()`), beyond the first of each name.
async fn case_duplicates(conn: &mut SqliteConnection, type_: &str) -> Result<u64> {
    let names: Vec<String> = sqlx::query_scalar(
        r#"SELECT "Name" FROM "BaseItems" WHERE "Type" = ?1 AND "Name" IS NOT NULL"#,
    )
    .bind(type_)
    .fetch_all(&mut *conn)
    .await?;
    Ok(beyond_first(names.iter().map(|name| lower(name))))
}

/// `name` lower-cased as .NET's `ToLowerInvariant` does. shortcut: Unicode's
/// full mapping, which differs from .NET's simple one for a few letters (`İ`,
/// and a word-final `Σ`, which becomes `ς` here and `σ` there); a count, not a
/// merge, so match .NET's table only if a report is ever off.
fn lower(name: &str) -> String {
    name.to_lowercase()
}

/// How many of `keys` repeat one before them.
fn beyond_first<K: std::hash::Hash + Eq>(keys: impl Iterator<Item = K>) -> u64 {
    let mut seen: HashMap<K, u64> = HashMap::new();
    for key in keys {
        *seen.entry(key).or_default() += 1;
    }
    seen.values().map(|n| n - 1).sum()
}

async fn scalar(conn: &mut SqliteConnection, sql: &'static str) -> Result<u64> {
    let n: i64 = sqlx::query_scalar(sql).fetch_one(&mut *conn).await?;
    Ok(u64::try_from(n).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use sqlx::Connection as _;

    use super::{FirstStartCounts, read};

    #[tokio::test]
    async fn each_count_is_the_rows_its_routine_changes() {
        let mut conn = sqlx::SqliteConnection::connect("sqlite::memory:")
            .await
            .unwrap();
        // A database without the tables counts none.
        assert_eq!(read(&mut conn).await.unwrap(), FirstStartCounts::default());
        for statement in [
            r#"CREATE TABLE "BaseItems" ("Id" TEXT, "Type" TEXT, "Name" TEXT, "Path" TEXT)"#,
            r#"CREATE TABLE "Peoples" ("Name" TEXT NOT NULL, "PersonType" TEXT)"#,
            r#"CREATE TABLE "UserData" ("ItemId" TEXT, "UserId" TEXT, "CustomDataKey" TEXT,
                 "PlaybackPositionTicks" INTEGER, "PlayCount" INTEGER, "Played" INTEGER,
                 "IsFavorite" INTEGER, "LastPlayedDate" TEXT)"#,
            // three rows on one path (two beyond the first), one alone, none without a path
            r#"INSERT INTO "BaseItems" VALUES
                 ('1', 'MediaBrowser.Controller.Entities.Movies.Movie', 'A', '/m/a.mkv'),
                 ('2', 'MediaBrowser.Controller.Entities.Video', 'A', '/m/a.mkv'),
                 ('3', 'MediaBrowser.Controller.Entities.Folder', 'A', '/m/a.mkv'),
                 ('4', 'MediaBrowser.Controller.Entities.Movies.Movie', 'B', '/m/b.mkv'),
                 ('5', 'MediaBrowser.Controller.Entities.Audio.MusicArtist', 'Ensemble', NULL),
                 ('6', 'MediaBrowser.Controller.Entities.Audio.MusicArtist', 'ENSEMBLE', NULL),
                 ('7', 'MediaBrowser.Controller.Entities.Audio.MusicArtist', 'Soloist', NULL),
                 ('8', 'MediaBrowser.Controller.Entities.Person', 'Émile', NULL),
                 ('9', 'MediaBrowser.Controller.Entities.Person', 'ÉMILE', NULL),
                 ('10', 'MediaBrowser.Controller.Entities.Person', 'émile', NULL)"#,
            // the same name ignoring case in one type merges, across types it does not
            r#"INSERT INTO "Peoples" VALUES ('Émile', 'Actor'), ('émile', 'Actor'), ('Émile', 'Director')"#,
            // item 1: two rows that disagree on the play count (one beyond the first); item 2:
            // two that agree; another user's row of item 1 alone; the placeholder's disagree
            r#"INSERT INTO "UserData" VALUES
                 ('1', 'u', 'tmdb', 0, 1, 1, 0, '2026-01-01'), ('1', 'u', 'imdb', 0, 2, 1, 0, '2026-01-01'),
                 ('2', 'u', 'tmdb', 5, 0, 0, 1, NULL), ('2', 'u', 'imdb', 5, 0, 0, 1, NULL),
                 ('1', 'v', 'tmdb', 0, 9, 1, 0, NULL),
                 ('00000000-0000-0000-0000-000000000001', 'u', 'a', 0, 1, 0, 0, NULL),
                 ('00000000-0000-0000-0000-000000000001', 'u', 'b', 0, 2, 0, 0, NULL)"#,
        ] {
            sqlx::query(statement).execute(&mut conn).await.unwrap();
        }
        let counts = read(&mut conn).await.unwrap();
        assert_eq!(
            counts,
            FirstStartCounts {
                shared_paths: 2,
                artists: 1,
                persons: 2,
                credits: 1,
                play_states: 1,
            }
        );
        assert_eq!(
            counts.note("FixIncorrectOwnerIdRelationships").unwrap(),
            "2 items repeat another's path, which it would delete"
        );
        assert_eq!(
            counts.note("MergeDuplicatePeople").unwrap(),
            "2 people and 1 credit differ from others only by case"
        );
        assert_eq!(
            counts.note("HarmonizeConflictingUserData").unwrap(),
            "1 play state disagrees with another of its user and item"
        );
        assert_eq!(counts.note("DisableLegacyAuthorization"), None);
    }
}
