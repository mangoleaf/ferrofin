//! The paths the exported database stores under the source's directories,
//! rewritten onto the layout Jellyfin reads the output in (gl-1557, D5).
//!
//! Jellyfin stores the root folder, the user root, every library folder and
//! every image path absolute: `ReverseVirtualPath` tokenises only the data and
//! metadata directories (`ApplicationHost.cs`), and `Chapters.ImagePath` and
//! `ImageInfos.Path` are never tokenised. It has no repair for a program data
//! directory that moved, and its scan deletes a library whose stored folder
//! does not exist (`LibraryManager.ValidateTopLibraryFolders`). So every such
//! path is rewritten onto the output's layout, and none is left naming the
//! source: a Jellyfin on the same host would otherwise write into it, or
//! delete from it, through that row.
//!
//! The directories rewritten from are the ones the database's root and
//! library rows name, which is where the server ran, plus the source's own as
//! the export reaches them. `--data-dir` alone is not enough: a server that
//! ran in a container at `/data` and is exported from the host through the
//! volume's own path stored `/data`. Token paths (`%AppDataPath%`,
//! `%MetadataPath%`) are left as they are: Jellyfin expands them onto its own
//! directories.
//!
//! This is the inverse of Ferrofin's boot repair `rebase_program_data_paths`
//! over the same Jellyfin-owned columns; Ferrofin's own are dropped from the
//! export.

use std::path::Path;

use sqlx::SqliteConnection;

use super::layout::{CONFIG, Layout, METADATA, SourceDirs};
use crate::Result;

/// The Jellyfin columns that can hold an absolute path under program data.
const PATH_COLUMNS: [(&str, &str); 5] = [
    ("BaseItems", "Path"),
    ("BaseItemImageInfos", "Path"),
    ("Chapters", "ImagePath"),
    ("MediaStreamInfos", "Path"),
    ("ImageInfos", "Path"),
];

/// The stored types of the rows whose path has a fixed place under program
/// data, with that place: the aggregate root (`root`), the user root
/// (`root/default`), and a library folder (`root/default/<name>`, one more
/// component).
const PLACED_ROWS: [(&str, &str, usize); 3] = [
    (
        "MediaBrowser.Controller.Entities.AggregateFolder",
        "root",
        0,
    ),
    (
        "MediaBrowser.Controller.Entities.UserRootFolder",
        "root/default",
        0,
    ),
    (
        "MediaBrowser.Controller.Entities.CollectionFolder",
        "root/default",
        1,
    ),
];

/// What the export did to the paths its copy of the database stores.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathRewrite {
    /// The program data directories the database's root and library rows
    /// name, sorted: where the server ran, and any directory it ran in before
    /// that a row still names.
    pub stored: Vec<String>,
    /// The one the aggregate root names, which the server sets at each start:
    /// where it last ran. `None` when the database has no such row.
    pub ran_at: Option<String>,
    /// The rows whose path was rewritten, across every column.
    pub rows: u64,
}

/// Rewrites every path the staged database stores under one of the source's
/// directories onto `layout`: under a program data directory a placed row
/// names, or `dirs.program_data`, to the same place under
/// [`Layout::program_data`] (`config` to [`Layout::config`], `metadata` to
/// [`Layout::metadata`]); under `dirs.config` to [`Layout::config`]; under
/// `dirs.metadata` to [`Layout::metadata`]. The longest directory that holds a
/// path decides, matched by whole components. A table the database lacks is
/// skipped.
pub(super) async fn rewrite(
    conn: &mut SqliteConnection,
    dirs: &SourceDirs,
    layout: &Layout,
) -> Result<PathRewrite> {
    let (stored, ran_at) = if has_table(conn, "BaseItems").await? {
        stored_program_data(conn).await?
    } else {
        (Vec::new(), None)
    };
    let moves = moves(&stored, dirs, layout);
    let mut rows = 0;
    for (table, column) in PATH_COLUMNS {
        if !has_table(conn, table).await? {
            continue;
        }
        // One pass over the table for every directory; the directories are bound.
        let froms = serde_json::Value::from(
            moves
                .iter()
                .map(|(from, _)| from.clone())
                .collect::<Vec<_>>(),
        )
        .to_string();
        // Table and column names come from `PATH_COLUMNS`. The column is named
        // with its table: `json_each` has a `path` column of its own.
        let found: Vec<(i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            r#"SELECT rowid, "{column}" FROM "{table}"
               WHERE EXISTS (SELECT 1 FROM json_each(?1) AS d
                             WHERE "{table}"."{column}" = d.value
                                OR substr("{table}"."{column}", 1, length(d.value) + 1)
                                   = d.value || '/')"#
        )))
        .bind(froms)
        .fetch_all(&mut *conn)
        .await?;
        for (rowid, path) in found {
            let Some(to) = moved(&path, &moves).filter(|to| *to != path) else {
                continue;
            };
            rows += sqlx::query(sqlx::AssertSqlSafe(format!(
                r#"UPDATE "{table}" SET "{column}" = ?1 WHERE rowid = ?2"#
            )))
            .bind(to)
            .bind(rowid)
            .execute(&mut *conn)
            .await?
            .rows_affected();
        }
    }
    Ok(PathRewrite {
        stored,
        ran_at,
        rows,
    })
}

/// The program data directories the placed rows name, sorted, and the one
/// the aggregate root names.
async fn stored_program_data(conn: &mut SqliteConnection) -> Result<(Vec<String>, Option<String>)> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        r#"SELECT "Type", "Path" FROM "BaseItems"
           WHERE "Type" IN (?1, ?2, ?3) AND "Path" IS NOT NULL"#,
    )
    .bind(PLACED_ROWS[0].0)
    .bind(PLACED_ROWS[1].0)
    .bind(PLACED_ROWS[2].0)
    .fetch_all(&mut *conn)
    .await?;
    let (mut stored, mut ran_at) = (Vec::new(), None);
    for (kind, path) in rows {
        let Some(&(_, place, extra)) = PLACED_ROWS.iter().find(|(name, ..)| *name == kind) else {
            continue;
        };
        let Some(dir) = program_data_of(&path, place, extra) else {
            continue;
        };
        if kind == PLACED_ROWS[0].0 {
            ran_at = Some(dir.clone());
        }
        stored.push(dir);
    }
    stored.sort();
    stored.dedup();
    Ok((stored, ran_at))
}

/// The program data directory a row stored at `path` names, when the path
/// ends in `place` after `extra` more components: `/data` for `/data/root` with
/// `root`. A relative or token path names none, and neither does `/` (a
/// rewrite from it would move every path, media included).
fn program_data_of(path: &str, place: &str, extra: usize) -> Option<String> {
    let path = Path::new(path);
    let placed = path.ancestors().nth(extra)?;
    if !placed.ends_with(place) {
        return None;
    }
    let dir = placed
        .ancestors()
        .nth(Path::new(place).components().count())?;
    (dir.is_absolute() && dir.parent().is_some()).then(|| dir.to_string_lossy().into_owned())
}

/// `(from, to)` for every directory a stored path moves from, longest first.
fn moves(stored: &[String], dirs: &SourceDirs, layout: &Layout) -> Vec<(String, String)> {
    let metadata = layout.metadata();
    let mut program_data: Vec<String> = stored.to_vec();
    program_data.extend(spellings(&dirs.program_data));
    let mut moves = Vec::new();
    for from in &program_data {
        moves.push((format!("{from}/{CONFIG}"), layout.config.to_owned()));
        moves.push((format!("{from}/{METADATA}"), metadata.clone()));
        moves.push((from.clone(), layout.program_data.to_owned()));
    }
    for from in spellings(&dirs.config) {
        moves.push((from, layout.config.to_owned()));
    }
    for from in spellings(&dirs.metadata) {
        moves.push((from, metadata.clone()));
    }
    // A directory a token or `/` would be is no directory to move from.
    moves.retain(|(from, _)| from.starts_with('/') && from != "/");
    moves.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    moves.dedup_by(|a, b| a.0 == b.0);
    moves
}

/// `dir` as given and as its real path, without a trailing `/`.
fn spellings(dir: &Path) -> Vec<String> {
    let mut spellings = vec![dir.to_string_lossy().trim_end_matches('/').to_owned()];
    if let Ok(real) = std::fs::canonicalize(dir) {
        spellings.push(real.to_string_lossy().trim_end_matches('/').to_owned());
    }
    spellings
}

/// Where `path` moves: under the longest `from` of `moves` that holds it.
fn moved(path: &str, moves: &[(String, String)]) -> Option<String> {
    moves.iter().find_map(|(from, to)| {
        let rest = path.strip_prefix(from.as_str())?;
        (rest.is_empty() || rest.starts_with('/')).then(|| format!("{to}{rest}"))
    })
}

/// Whether the database has the table `name`.
pub(super) async fn has_table(conn: &mut SqliteConnection, name: &str) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
    )
    .bind(name)
    .fetch_one(&mut *conn)
    .await?
        > 0)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use sqlx::ConnectOptions as _;
    use sqlx::sqlite::SqliteConnectOptions;

    use super::{moved, moves, program_data_of, rewrite};
    use crate::export::layout::{DOCKER, SourceDirs};

    #[test]
    fn a_placed_row_names_its_program_data_directory() {
        assert_eq!(
            program_data_of("/data/root", "root", 0).as_deref(),
            Some("/data")
        );
        assert_eq!(
            program_data_of("/srv/ff/root/default", "root/default", 0).as_deref(),
            Some("/srv/ff")
        );
        assert_eq!(
            program_data_of("/data/root/default/Movies", "root/default", 1).as_deref(),
            Some("/data")
        );
        for (path, place, extra) in [
            ("/data/elsewhere/Movies", "root/default", 1),
            ("%AppDataPath%/root", "root", 0),
            ("/root", "root", 0),
            ("root", "root", 0),
        ] {
            assert_eq!(program_data_of(path, place, extra), None, "{path}");
        }
    }

    #[test]
    fn the_longest_directory_decides_by_whole_components() {
        let dirs = SourceDirs {
            config: "/etc/ferrofin".into(),
            metadata: "/data/meta".into(),
            ..SourceDirs::new(Path::new("/data"))
        };
        let moves = moves(&["/old".to_owned()], &dirs, &DOCKER);
        for (path, to) in [
            (
                "/data/root/default/Movies",
                Some("/config/root/default/Movies"),
            ),
            (
                "/data/meta/library/ab/poster.jpg",
                Some("/config/metadata/library/ab/poster.jpg"),
            ),
            ("/data/metadata/People/A", Some("/config/metadata/People/A")),
            (
                "/data/config/users/a/profile.png",
                Some("/config/config/users/a/profile.png"),
            ),
            (
                "/etc/ferrofin/users/a/profile.png",
                Some("/config/config/users/a/profile.png"),
            ),
            ("/old/data/trickplay", Some("/config/data/trickplay")),
            ("/data", Some("/config")),
            ("/database/x", None),
            ("/media/movies/a.mkv", None),
            ("%MetadataPath%/People/A", None),
        ] {
            assert_eq!(moved(path, &moves).as_deref(), to, "{path}");
        }
    }

    /// Every path under the source's directories is moved onto `/config`,
    /// those the rows name included; tokens, media and a table the database
    /// lacks are left alone.
    #[tokio::test]
    async fn rewrites_the_stored_paths_onto_the_layout() {
        let mut conn = SqliteConnectOptions::new()
            .in_memory(true)
            .connect()
            .await
            .unwrap();
        for statement in [
            r#"CREATE TABLE "BaseItems" ("Type" TEXT NOT NULL, "Path" TEXT)"#,
            r#"CREATE TABLE "BaseItemImageInfos" ("Path" TEXT NOT NULL)"#,
            r#"CREATE TABLE "Chapters" ("ImagePath" TEXT)"#,
            r#"INSERT INTO "BaseItems" VALUES
               ('MediaBrowser.Controller.Entities.AggregateFolder', '/data/root'),
               ('MediaBrowser.Controller.Entities.UserRootFolder', '/data/root/default'),
               ('MediaBrowser.Controller.Entities.CollectionFolder', '/data/root/default/Movies'),
               ('MediaBrowser.Controller.Entities.CollectionFolder', '/moved/root/default/Old'),
               ('MediaBrowser.Controller.Entities.Year', '/data/metadata/Year/2020'),
               ('MediaBrowser.Controller.Entities.Person', '%MetadataPath%/People/A/Actor'),
               ('MediaBrowser.Controller.Entities.Movies.Movie', '/media/movies/a.mkv'),
               ('MediaBrowser.Controller.Entities.Folder', NULL)"#,
            r#"INSERT INTO "BaseItemImageInfos" VALUES
               ('/data/metadata/library/ab/ab12/poster.jpg'),
               ('/srv/export-from/metadata/library/cd/primary.png'),
               ('/database-elsewhere/x.jpg')"#,
            r#"INSERT INTO "Chapters" VALUES ('/data/metadata/library/ab/ab12/chapters/1_0.jpg'), (NULL)"#,
        ] {
            sqlx::query(statement).execute(&mut conn).await.unwrap();
        }
        let dirs = SourceDirs::new(Path::new("/srv/export-from"));

        let rewrite = rewrite(&mut conn, &dirs, &DOCKER).await.unwrap();

        assert_eq!(rewrite.stored, ["/data", "/moved"]);
        assert_eq!(rewrite.ran_at.as_deref(), Some("/data"));
        assert_eq!(rewrite.rows, 8);
        let paths: Vec<Option<String>> =
            sqlx::query_scalar(r#"SELECT "Path" FROM "BaseItems" ORDER BY rowid"#)
                .fetch_all(&mut conn)
                .await
                .unwrap();
        assert_eq!(
            paths,
            [
                Some("/config/root"),
                Some("/config/root/default"),
                Some("/config/root/default/Movies"),
                Some("/config/root/default/Old"),
                Some("/config/metadata/Year/2020"),
                Some("%MetadataPath%/People/A/Actor"),
                Some("/media/movies/a.mkv"),
                None,
            ]
            .map(|path| path.map(str::to_owned))
        );
        let images: Vec<String> =
            sqlx::query_scalar(r#"SELECT "Path" FROM "BaseItemImageInfos" ORDER BY rowid"#)
                .fetch_all(&mut conn)
                .await
                .unwrap();
        assert_eq!(
            images,
            [
                "/config/metadata/library/ab/ab12/poster.jpg",
                "/config/metadata/library/cd/primary.png",
                "/database-elsewhere/x.jpg",
            ]
        );
        let chapters: Vec<Option<String>> =
            sqlx::query_scalar(r#"SELECT "ImagePath" FROM "Chapters" ORDER BY rowid"#)
                .fetch_all(&mut conn)
                .await
                .unwrap();
        assert_eq!(
            chapters,
            [
                Some("/config/metadata/library/ab/ab12/chapters/1_0.jpg".to_owned()),
                None
            ]
        );
    }

    /// A database at `/config` already is left as it is.
    #[tokio::test]
    async fn a_source_at_config_changes_nothing() {
        let mut conn = SqliteConnectOptions::new()
            .in_memory(true)
            .connect()
            .await
            .unwrap();
        for statement in [
            r#"CREATE TABLE "BaseItems" ("Type" TEXT NOT NULL, "Path" TEXT)"#,
            r#"INSERT INTO "BaseItems" VALUES
               ('MediaBrowser.Controller.Entities.AggregateFolder', '/config/root'),
               ('MediaBrowser.Controller.Entities.Year', '/config/metadata/Year/2020')"#,
        ] {
            sqlx::query(statement).execute(&mut conn).await.unwrap();
        }

        let rewrite = rewrite(&mut conn, &SourceDirs::new(Path::new("/config")), &DOCKER)
            .await
            .unwrap();

        assert_eq!(rewrite.stored, ["/config"]);
        assert_eq!(rewrite.rows, 0);
    }
}
