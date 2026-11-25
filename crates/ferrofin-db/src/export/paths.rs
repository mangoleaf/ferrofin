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
//! Over those columns this is the inverse of Ferrofin's boot repair
//! `rebase_program_data_paths`; Ferrofin's own columns are dropped from the
//! export.
//!
//! `BaseItems.Data` holds paths too: each item's own path lists, which
//! Jellyfin serialises with the item (`BaseItemMapper.Map`): a library's and
//! the root's `PhysicalLocationsList`, a video's `AdditionalParts`,
//! `LocalAlternateVersions`, `LinkedAlternateVersions[].Path`, `SubtitleFiles`
//! and `AudioFiles`, a song's `LyricFiles`, and `ShortcutPath`. Every string
//! value there that lies under a source directory moves the same way, and
//! every other byte of the document stays as it was.

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

/// The Jellyfin JSON columns whose string values can hold such a path. Its
/// free text (names, overviews) lives in columns of its own, so a string
/// value here that a source directory holds is a path.
const JSON_PATH_COLUMNS: [(&str, &str); 1] = [("BaseItems", "Data")];

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
    /// The stored paths rewritten, across every column and every `Data`
    /// document.
    pub rewritten: u64,
    /// `(from, to)` for every directory a stored path moves from, longest
    /// first: what [`PathRewrite::moved`] applies to the paths the export
    /// writes into files rather than the database.
    pub moves: Vec<(String, String)>,
}

impl PathRewrite {
    /// Where `path` moves in the output's layout: under the longest directory
    /// of [`Self::moves`] that holds it, by whole components. `None` for a
    /// path under none of them (media, a token, a relative path).
    #[must_use]
    pub fn moved(&self, path: &str) -> Option<String> {
        moved(path, &self.moves)
    }
}

/// Rewrites every path the staged database stores under one of the source's
/// directories onto `layout`: under a program data directory a placed row
/// names, or `dirs.program_data`, to the same place under
/// [`Layout::program_data`] (`config` to [`Layout::config`], `metadata` to
/// [`Layout::metadata`]); under `dirs.config` to [`Layout::config`]; under
/// `dirs.metadata` to [`Layout::metadata`]. The longest directory that holds a
/// path decides, matched by whole components, in the path columns and in
/// every string value of an item's `Data`. A table or `Data` column the
/// database lacks is skipped.
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
    // One pass over each table for every directory; the directories are bound.
    let froms = serde_json::Value::from(
        moves
            .iter()
            .map(|(from, _)| from.clone())
            .collect::<Vec<_>>(),
    )
    .to_string();
    let mut rewritten = 0;
    for (table, column) in PATH_COLUMNS {
        if !has_table(conn, table).await? {
            continue;
        }
        // Table and column names come from `PATH_COLUMNS`. The column is named
        // with its table: `json_each` has a `path` column of its own.
        let found: Vec<(i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            r#"SELECT rowid, "{column}" FROM "{table}"
               WHERE EXISTS (SELECT 1 FROM json_each(?1) AS d
                             WHERE "{table}"."{column}" = d.value
                                OR substr("{table}"."{column}", 1, length(d.value) + 1)
                                   = d.value || '/')"#
        )))
        .bind(&froms)
        .fetch_all(&mut *conn)
        .await?;
        for (rowid, path) in found {
            let Some(to) = moved(&path, &moves).filter(|to| *to != path) else {
                continue;
            };
            rewritten += update(conn, table, column, &to, rowid).await?;
        }
    }
    for (table, column) in JSON_PATH_COLUMNS {
        if !has_column(conn, table, column).await? {
            continue;
        }
        // The documents holding a string that starts with `/`, however it is
        // spelled (`/`, `\/`, `\u002F`): every directory a path moves from
        // is absolute. Which of them moves is `moved_json`'s to say.
        let found: Vec<(i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            r#"SELECT rowid, "{column}" FROM "{table}"
               WHERE "{column}" IS NOT NULL
                 AND (instr("{column}", '"/') > 0 OR instr("{column}", '"\/') > 0
                      OR instr(lower("{column}"), '"\u002f') > 0)"#
        )))
        .fetch_all(&mut *conn)
        .await?;
        for (rowid, json) in found {
            if let Some((to, paths)) = moved_json(&json, &moves)
                && update(conn, table, column, &to, rowid).await? > 0
            {
                rewritten += paths;
            }
        }
    }
    Ok(PathRewrite {
        stored,
        ran_at,
        rewritten,
        moves,
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
    let (from, to) = move_of(path, moves)?;
    Some(format!("{to}{}", &path[from.len()..]))
}

/// The longest `(from, to)` of `moves` whose `from` holds `path`, by whole
/// components.
fn move_of<'a>(path: &str, moves: &'a [(String, String)]) -> Option<&'a (String, String)> {
    moves.iter().find(|(from, _)| {
        path.strip_prefix(from.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// `json` with every string value [`moved`] moves replaced, every other byte
/// as it was, and how many moved; `None` when none does. Object keys are
/// never values. A value that spells the directory plainly keeps its own
/// text past it, escapes included; one that escapes it is written afresh.
fn moved_json(json: &str, moves: &[(String, String)]) -> Option<(String, u64)> {
    let bytes = json.as_bytes();
    let (mut out, mut copied, mut at, mut count) = (String::new(), 0, 0, 0);
    while let Some(offset) = json[at..].find('"') {
        let start = at + offset;
        // The closing quote: the first one no backslash escapes. A document
        // that ends inside a string is left as it is.
        let mut end = start + 1;
        while *bytes.get(end)? != b'"' {
            end += if bytes[end] == b'\\' { 2 } else { 1 };
        }
        at = end + 1;
        if json[at..].trim_start().starts_with(':') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<String>(&json[start..at]) else {
            continue;
        };
        let Some((from, to)) = move_of(&value, moves).filter(|(from, to)| from != to) else {
            continue;
        };
        let raw = &json[start + 1..end];
        let rest = match raw.strip_prefix(from.as_str()) {
            Some(rest) => rest.to_owned(),
            None => escaped(&value[from.len()..])?,
        };
        out.push_str(&json[copied..start]);
        out.push('"');
        out.push_str(&escaped(to)?);
        out.push_str(&rest);
        out.push('"');
        copied = at;
        count += 1;
    }
    (count > 0).then(|| (out + &json[copied..], count))
}

/// `text` as a JSON string's content, without its quotes.
fn escaped(text: &str) -> Option<String> {
    let quoted = serde_json::to_string(text).ok()?;
    Some(quoted[1..quoted.len() - 1].to_owned())
}

/// Sets `table`.`column` of the row `rowid` to `value`: the rows it changed.
async fn update(
    conn: &mut SqliteConnection,
    table: &str,
    column: &str,
    value: &str,
    rowid: i64,
) -> Result<u64> {
    // Table and column names come from the column lists above.
    Ok(sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"UPDATE "{table}" SET "{column}" = ?1 WHERE rowid = ?2"#
    )))
    .bind(value)
    .bind(rowid)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// Whether the database has the table `table` with the column `column`.
pub(super) async fn has_column(
    conn: &mut SqliteConnection,
    table: &str,
    column: &str,
) -> Result<bool> {
    Ok(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2")
            .bind(table)
            .bind(column)
            .fetch_one(&mut *conn)
            .await?
            > 0,
    )
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

    use super::{moved, moved_json, moves, program_data_of, rewrite};
    use crate::export::layout::{DOCKER, SourceDirs};

    /// A document's string values under a source directory move, Jellyfin's
    /// escapes past the directory kept; keys, other strings and every other
    /// byte stay as they were.
    #[test]
    fn a_documents_paths_move_and_every_other_byte_stays() {
        let moves = moves(
            &["/data".to_owned()],
            &SourceDirs::new(Path::new("/srv/x")),
            &DOCKER,
        );
        let json = concat!(
            r#"{"PhysicalLocationsList":["/data/root/default/Caf\u00E9 \u0026 Bar","/media/movies"],"#,
            r#""/data":1, "SubtitleFiles" : [ "/data/metadata/library/ab/x.srt" ],"#,
            r#""RemoteTrailers":[{"Url":"https://x/data/y"}],"ShortcutPath":"%MetadataPath%/x","#,
            r#""LinkedAlternateVersions":[{"Path":"\/data\/root"},{"Path":"/data"}],"#,
            r#""Note":"a \"quoted\" /data/x","IsHD":false}"#
        );
        assert_eq!(
            moved_json(json, &moves),
            Some((
                concat!(
                r#"{"PhysicalLocationsList":["/config/root/default/Caf\u00E9 \u0026 Bar","/media/movies"],"#,
                r#""/data":1, "SubtitleFiles" : [ "/config/metadata/library/ab/x.srt" ],"#,
                r#""RemoteTrailers":[{"Url":"https://x/data/y"}],"ShortcutPath":"%MetadataPath%/x","#,
                r#""LinkedAlternateVersions":[{"Path":"/config/root"},{"Path":"/config"}],"#,
                r#""Note":"a \"quoted\" /data/x","IsHD":false}"#
                )
                .to_owned(),
                4
            ))
        );
        for unmoved in [
            r#"{"A":["/media/a"],"B":"/database/x","/data/c":true}"#,
            r#"{"A":"/data/x"#,
        ] {
            assert_eq!(moved_json(unmoved, &moves), None, "{unmoved}");
        }
    }

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
        assert_eq!(rewrite.rewritten, 8);
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

    /// The paths a library's, the root's and a video's `Data` store under the
    /// source move with the columns, each counted; a document holding none is
    /// left byte for byte.
    #[tokio::test]
    async fn rewrites_the_paths_items_store_in_their_data() {
        let mut conn = SqliteConnectOptions::new()
            .in_memory(true)
            .connect()
            .await
            .unwrap();
        for statement in [
            r#"CREATE TABLE "BaseItems" ("Type" TEXT NOT NULL, "Path" TEXT, "Data" TEXT)"#,
            r#"INSERT INTO "BaseItems" VALUES
               ('MediaBrowser.Controller.Entities.AggregateFolder', '/data/root',
                '{"PhysicalLocationsList":["/data/root","/data/data/collections","/media/movies"]}'),
               ('MediaBrowser.Controller.Entities.CollectionFolder', '/media/elsewhere',
                '{"PhysicalLocationsList":["/data/root/default/Collections","/data/data/collections"],"IsRoot":false}'),
               ('MediaBrowser.Controller.Entities.Movies.Movie', '/media/movies/a.mkv',
                '{"SubtitleFiles":["/data/metadata/library/ab/ab12/a.en.srt"],"AdditionalParts":[]}'),
               ('MediaBrowser.Controller.Entities.Movies.Movie', '/media/movies/b.mkv',
                '{"SubtitleFiles":["/media/movies/b.en.srt"],"Note":"\u0026 /data"}'),
               ('MediaBrowser.Controller.Entities.CollectionFolder', '/media/esc',
                '{"PhysicalLocationsList":["\/data\/root\/default\/Esc","/data/root/default/Café","\u002Fdata/root/Uni"],"Name":"日本"}'),
               ('MediaBrowser.Controller.Entities.Video', '/media/v.mkv',
                '{"SubtitleFiles":["\u002Fdata/metadata/v.srt"]}'),
               ('MediaBrowser.Controller.Entities.Folder', NULL, NULL)"#,
        ] {
            sqlx::query(statement).execute(&mut conn).await.unwrap();
        }

        let rewrite = rewrite(&mut conn, &SourceDirs::new(Path::new("/data")), &DOCKER)
            .await
            .unwrap();

        // The root's path, and nine paths in five documents, three of them
        // spelling their directory with escapes (one only so: the pre-filter
        // must still find it).
        assert_eq!(rewrite.rewritten, 10);
        let data: Vec<Option<String>> =
            sqlx::query_scalar(r#"SELECT "Data" FROM "BaseItems" ORDER BY rowid"#)
                .fetch_all(&mut conn)
                .await
                .unwrap();
        assert_eq!(
            data,
            [
                Some(r#"{"PhysicalLocationsList":["/config/root","/config/data/collections","/media/movies"]}"#),
                Some(r#"{"PhysicalLocationsList":["/config/root/default/Collections","/config/data/collections"],"IsRoot":false}"#),
                Some(r#"{"SubtitleFiles":["/config/metadata/library/ab/ab12/a.en.srt"],"AdditionalParts":[]}"#),
                Some(r#"{"SubtitleFiles":["/media/movies/b.en.srt"],"Note":"\u0026 /data"}"#),
                Some(
                    r#"{"PhysicalLocationsList":["/config/root/default/Esc","/config/root/default/Café","/config/root/Uni"],"Name":"日本"}"#
                ),
                Some(r#"{"SubtitleFiles":["/config/metadata/v.srt"]}"#),
                None,
            ]
            .map(|data| data.map(str::to_owned))
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
        assert_eq!(rewrite.rewritten, 0);
    }
}
