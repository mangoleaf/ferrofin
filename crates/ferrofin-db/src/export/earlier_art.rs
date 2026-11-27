//! The art earlier builds saved in an item's `{metadata}/library/{ID}` or
//! `{id}` folder, moved in the output into the item's own folder, where
//! Jellyfin keeps it (gl-1916).
//!
//! Ferrofin saves an item's art in `{metadata}/library/{idN[..2]}/{idN}`
//! under `ImageSaver.GetStandardSavePath`'s name ([`own_art_dir`],
//! [`internal_image_stem`]), and keeps the art its earlier builds saved in the
//! `{ID}` and `{id}` folders where it is, reading and cleaning those folders
//! as the item's own. Jellyfin counts only `BaseItem.GetInternalMetadataPath`
//! as the item's: it never redraws a collage or a library tile stored
//! elsewhere (`BaseDynamicImageProvider.FetchAsync`, `HasChanged`) and leaves
//! such a folder behind when it deletes the item
//! (`LibraryManager.GetInternalMetadataPaths`). So the export moves each image
//! a row stores in its own item's `{ID}` or `{id}` folder into the item's own
//! folder, in its copy only, under the name Jellyfin's save would give it, and
//! the row follows. The source is never written.
//!
//! A file no row names that holds the name is replaced, as Jellyfin's save
//! replaces it. A name a stored image already has is never taken: that row
//! stays where it is ([`ArtRelocation::kept`]). An image whose file is not in
//! the output, a row in another item's folder, and a file in such a folder
//! that no row names stay as they are.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ferrofin_model::entities::ImageType;
use ferrofin_model::net::mime_types;
use sqlx::SqliteConnection;
use uuid::Uuid;

use super::layout::Layout;
use super::paths::has_column;
use super::{blocking, failed};
use crate::Result;
use crate::art::{earlier_art_dir, internal_image_stem, legacy_art_dir, own_art_dir};

/// The token Jellyfin stores a path under its metadata directory with
/// (`ApplicationHost.ReverseVirtualPath`), which the export leaves as it is.
const METADATA_TOKEN: &str = "%MetadataPath%";

/// What the export did with the art earlier builds saved in an item's
/// `{ID}` or `{id}` folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtRelocation {
    /// Each image row moved into its item's own folder, with its file:
    /// `(from, to)`, spelled on the layout (as the exported database now
    /// stores `to`), in row order.
    pub moved: Vec<(String, String)>,
    /// The image rows left in such a folder because another stored image
    /// already has the path Jellyfin's save would give them.
    pub kept: Vec<String>,
    /// The folders the move made in the output, parents first, each with the
    /// folder the art it holds came from, whose mode and time it takes once
    /// the copy is finished: relative to the output.
    pub made: Vec<(PathBuf, PathBuf)>,
    /// The folders the move left empty, removed from the output: relative to
    /// it.
    pub removed: Vec<PathBuf>,
}

/// One image row of an item with art in the library folder.
#[derive(Debug, Clone, sqlx::FromRow)]
struct ImageRow {
    rowid: i64,
    item_id: String,
    image_type: i32,
    path: String,
    /// The item's stored type; none for a row without its item, which is
    /// named as any item's image is.
    kind: Option<String>,
}

/// One row to move: its rowid, and its path before and after, spelled on
/// the layout.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Move {
    rowid: i64,
    from: String,
    to: String,
}

/// Moves, in the output `out` (which holds the source's copy in `layout`),
/// each image of the staged database stored in its item's `{ID}` or `{id}`
/// folder into the item's own folder, and rewrites its row. Run after the
/// stored paths are rewritten onto `layout`. A database without the image
/// table's or the items' columns is left as it is.
pub(super) async fn relocate(
    conn: &mut SqliteConnection,
    out: &Path,
    layout: &Layout,
) -> Result<ArtRelocation> {
    for (table, column) in [
        ("BaseItemImageInfos", "ItemId"),
        ("BaseItemImageInfos", "ImageType"),
        ("BaseItemImageInfos", "Path"),
        ("BaseItems", "Id"),
        ("BaseItems", "Type"),
    ] {
        if !has_column(conn, table, column).await? {
            return Ok(ArtRelocation::default());
        }
    }
    let metadata = layout.metadata();
    let prefixes = (
        format!("{metadata}/library/"),
        format!("{METADATA_TOKEN}/library/"),
    );
    // Every image stored in the library folder, in either spelling: the
    // names taken there. Then each row of the items that have one, in the
    // order `/Images/{type}/{index}` numbers them.
    let in_library = r#"substr("Path", 1, length(?1)) = ?1 OR substr("Path", 1, length(?2)) = ?2"#;
    let stored: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        r#"SELECT "Path" FROM "BaseItemImageInfos" WHERE {in_library}"#
    )))
    .bind(&prefixes.0)
    .bind(&prefixes.1)
    .fetch_all(&mut *conn)
    .await?;
    let rows: Vec<ImageRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        r#"SELECT i.rowid, i."ItemId" AS item_id, i."ImageType" AS image_type,
                  i."Path" AS path, b."Type" AS kind
           FROM "BaseItemImageInfos" AS i LEFT JOIN "BaseItems" AS b ON b."Id" = i."ItemId"
           WHERE i."ItemId" IN (SELECT "ItemId" FROM "BaseItemImageInfos" WHERE {in_library})
           ORDER BY i."ItemId", i."ImageType", i.rowid"#
    )))
    .bind(&prefixes.0)
    .bind(&prefixes.1)
    .fetch_all(&mut *conn)
    .await?;
    if rows.is_empty() {
        return Ok(ArtRelocation::default());
    }
    let (out, program_data) = (out.to_owned(), layout.program_data);
    let (moves, mut relocation) = blocking(move || {
        let in_output = |path: &str| {
            Path::new(path)
                .strip_prefix(program_data)
                .ok()
                .map(|inside| out.join(inside))
        };
        let stored: Vec<String> = stored.iter().map(|path| spelled(path, &metadata)).collect();
        let (moves, kept) = plan(&rows, &stored, &metadata, |path| {
            in_output(path).is_some_and(|file| file.is_file())
        });
        let mut relocation = ArtRelocation {
            kept,
            ..ArtRelocation::default()
        };
        move_files(&moves, &stored, &out, in_output, &mut relocation)?;
        Ok((moves, relocation))
    })
    .await?;
    // The files moved before their rows: a failure from here fails the export,
    // whose output is then never renamed into place.
    for Move { rowid, from, to } in moves {
        sqlx::query(r#"UPDATE "BaseItemImageInfos" SET "Path" = ?1 WHERE rowid = ?2"#)
            .bind(&to)
            .bind(rowid)
            .execute(&mut *conn)
            .await?;
        relocation.moved.push((from, to));
    }
    Ok(relocation)
}

/// `path` with a leading [`METADATA_TOKEN`] spelled as `metadata`, the
/// layout's metadata directory, as Jellyfin expands it.
fn spelled(path: &str, metadata: &str) -> String {
    match path.strip_prefix(METADATA_TOKEN) {
        Some(rest) if rest.starts_with('/') => format!("{metadata}{rest}"),
        _ => path.to_owned(),
    }
}

/// Which of `rows` (each item's together, by type, then in stored order) move
/// and where, and which stay because their place is taken: by one of the
/// `stored` paths (spelled on the layout) or by a row moved before. A row
/// moves when its file, which `exists`, is in its own item's `{ID}` or `{id}`
/// folder under `metadata`'s `library`.
fn plan(
    rows: &[ImageRow],
    stored: &[String],
    metadata: &str,
    exists: impl Fn(&str) -> bool,
) -> (Vec<Move>, Vec<String>) {
    let library = PathBuf::from(format!("{metadata}/library"));
    let mut taken: HashSet<String> = stored.iter().cloned().collect();
    let (mut moves, mut kept) = (Vec::new(), Vec::new());
    for item in rows.chunk_by(|a, b| a.item_id == b.item_id) {
        let Ok(id) = Uuid::parse_str(&item[0].item_id) else {
            continue;
        };
        let earlier = [earlier_art_dir(&library, id), legacy_art_dir(&library, id)];
        let own = own_art_dir(&library, id);
        let movable: Vec<bool> = item
            .iter()
            .map(|row| {
                let path = spelled(&row.path, metadata);
                Path::new(&path)
                    .parent()
                    .is_some_and(|dir| earlier.iter().any(|earlier| earlier == dir))
                    && exists(&path)
            })
            .collect();
        // `GetBackdropSaveFilename`: a later backdrop takes the first
        // `backdropN` no backdrop of the item is named.
        let mut backdrops: Vec<String> = item
            .iter()
            .zip(&movable)
            .filter(|(row, movable)| !**movable && is_backdrop(row))
            .filter_map(|(row, _)| stem(&row.path))
            .collect();
        let mut index = 0;
        for (row, movable) in item.iter().zip(movable) {
            let backdrop = is_backdrop(row).then(|| {
                index += 1;
                index - 1
            });
            if !movable {
                continue;
            }
            let kind = row.kind.as_deref().unwrap_or_default();
            let image_type = ImageType::from_json_value(row.image_type);
            let name = match backdrop {
                // One of the first `len + 1` numbers is free.
                Some(index) if index > 0 => (1..=backdrops.len() + 1)
                    .map(|n| format!("backdrop{n}"))
                    .find(|name| !backdrops.contains(name))
                    .unwrap_or_default(),
                _ => internal_image_stem(kind, image_type),
            };
            let from = spelled(&row.path, metadata);
            let to = own
                .join(format!("{name}{}", extension(&from)))
                .to_string_lossy()
                .into_owned();
            if !taken.insert(to.clone()) {
                kept.push(row.path.clone());
                continue;
            }
            if backdrop.is_some() {
                backdrops.push(name);
            }
            moves.push(Move {
                rowid: row.rowid,
                from,
                to,
            });
        }
    }
    (moves, kept)
}

/// Whether `row` is a backdrop's.
fn is_backdrop(row: &ImageRow) -> bool {
    ImageType::from_json_value(row.image_type) == ImageType::Backdrop
}

/// The file name of `path` without its extension, lower-cased, as
/// `GetBackdropSaveFilename` compares them (ordinal, ignoring case).
fn stem(path: &str) -> Option<String> {
    Some(Path::new(path).file_stem()?.to_str()?.to_ascii_lowercase())
}

/// The extension `GetStandardSavePath` gives an image of the file `path`:
/// its MIME type's (`MimeTypes.ToExtension`), `.jpg` for `.jpeg`, lower-cased;
/// the file's own when its type is no image's.
fn extension(path: &str) -> String {
    let mime = mime_types::get_mime_type(path);
    let extension = mime_types::is_image(mime)
        .then(|| mime_types::to_extension(mime))
        .flatten()
        .map(str::to_owned)
        .or_else(|| {
            Path::new(path)
                .extension()
                .map(|extension| format!(".{}", extension.to_string_lossy()))
        })
        .unwrap_or_default()
        .to_ascii_lowercase();
    if extension == ".jpeg" {
        ".jpg".to_owned()
    } else {
        extension
    }
}

/// Moves each file of `moves` in the output, `in_output` mapping a stored
/// path to its copy: renamed when the last row naming it moves (`stored`
/// counts the rows naming each), copied with its permissions and time
/// otherwise, into folders made private to the owner, as the copy makes its
/// own. Then removes each folder a file moved from that is left empty.
/// Records in `relocation` the folders made, each with the one its file came
/// from ([`ArtRelocation::made`]), and those removed, relative to `out`.
fn move_files(
    moves: &[Move],
    stored: &[String],
    out: &Path,
    in_output: impl Fn(&str) -> Option<PathBuf>,
    relocation: &mut ArtRelocation,
) -> Result<()> {
    let relative = |dir: &Path| dir.strip_prefix(out).unwrap_or(dir).to_owned();
    let mut naming: HashMap<&str, usize> = HashMap::new();
    for path in stored {
        *naming.entry(path).or_default() += 1;
    }
    let mut left = Vec::new();
    for Move { from, to, .. } in moves {
        let (Some(from_file), Some(to_file)) = (in_output(from), in_output(to)) else {
            continue;
        };
        if let (Some(dir), Some(like)) = (to_file.parent(), from_file.parent()) {
            let mut missing = Vec::new();
            for dir in dir.ancestors() {
                if dir.exists() {
                    break;
                }
                missing.push(dir);
            }
            for dir in missing.into_iter().rev() {
                super::tree::private_dir(dir).map_err(failed(dir))?;
                relocation.made.push((relative(dir), relative(like)));
            }
        }
        let rows = naming.entry(from.as_str()).or_default();
        *rows = rows.saturating_sub(1);
        if *rows == 0 {
            std::fs::rename(&from_file, &to_file).map_err(failed(&to_file))?;
        } else {
            copy(&from_file, &to_file)?;
        }
        if let Some(dir) = from_file
            .parent()
            .filter(|dir| !left.iter().any(|d| d == dir))
        {
            left.push(dir.to_owned());
        }
    }
    for dir in left {
        match std::fs::remove_dir(&dir) {
            Ok(()) => relocation.removed.push(relative(&dir)),
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(error) => return Err(failed(&dir)(error)),
        }
    }
    Ok(())
}

/// Copies `from` to `to`, with its permissions and modification time.
fn copy(from: &Path, to: &Path) -> Result<()> {
    std::fs::copy(from, to).map_err(failed(to))?;
    let modified = std::fs::metadata(from)
        .and_then(|metadata| metadata.modified())
        .map_err(failed(from))?;
    std::fs::File::options()
        .write(true)
        .open(to)
        .and_then(|file| file.set_modified(modified))
        .map_err(failed(to))
}

#[cfg(test)]
mod tests {
    use super::{ImageRow, Move, extension, plan};

    const META: &str = "/config/metadata";
    const ID: &str = "0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0";

    fn row(rowid: i64, image_type: i32, path: &str, kind: &str) -> ImageRow {
        ImageRow {
            rowid,
            item_id: ID.to_owned(),
            image_type,
            path: path.to_owned(),
            kind: Some(kind.to_owned()),
        }
    }

    fn earlier(name: &str) -> String {
        format!("{META}/library/{ID}/{name}")
    }

    fn own(name: &str) -> String {
        format!("{META}/library/0f/0f1e2d3c4b5a69788796a5b4c3d2e1f0/{name}")
    }

    /// Each image takes `GetStandardSavePath`'s name in the item's own
    /// folder: by type and kind, the first backdrop `backdrop` and later ones
    /// the first `backdropN` no backdrop that stays is named; the extension
    /// its MIME type's, lower-cased.
    #[test]
    fn earlier_art_takes_jellyfins_names_in_the_own_folder() {
        let legacy = format!("{META}/library/{}/primary.JPEG", ID.to_ascii_lowercase());
        let rows = [
            row(
                1,
                0,
                &legacy,
                "MediaBrowser.Controller.Entities.Audio.MusicAlbum",
            ),
            row(2, 2, &earlier("backdrop.jpg"), "MusicAlbum"),
            row(3, 2, &earlier("backdrop1.png"), "MusicAlbum"),
            row(4, 2, &own("backdrop1.jpg"), "MusicAlbum"),
            row(5, 2, &earlier("backdrop2.jpg"), "MusicAlbum"),
            row(6, 6, &earlier("disc.webp"), "MusicAlbum"),
        ];
        let stored: Vec<String> = rows.iter().map(|row| row.path.clone()).collect();

        let (planned, kept) = plan(&rows, &stored, META, |_| true);

        let moved: Vec<(i64, &str, &str)> = planned
            .iter()
            .map(|Move { rowid, from, to }| (*rowid, from.as_str(), to.as_str()))
            .collect();
        assert_eq!(
            moved,
            [
                (1, legacy.as_str(), own("folder.jpg").as_str()),
                (
                    2,
                    earlier("backdrop.jpg").as_str(),
                    own("backdrop.jpg").as_str()
                ),
                (
                    3,
                    earlier("backdrop1.png").as_str(),
                    own("backdrop2.png").as_str()
                ),
                (
                    5,
                    earlier("backdrop2.jpg").as_str(),
                    own("backdrop3.jpg").as_str()
                ),
                (6, earlier("disc.webp").as_str(), own("cdart.webp").as_str()),
            ]
        );
        assert_eq!(kept, Vec::<String>::new());
    }

    /// A path a stored image has, in either spelling, or one a row moved
    /// before took, is never given to another: that row stays. A row whose
    /// file is not there, one outside its item's earlier folders and one in
    /// a folder under them stay too.
    #[test]
    fn a_taken_name_keeps_the_row_where_it_is() {
        let token = own("poster.jpg").replace(META, "%MetadataPath%");
        let rows = [
            row(1, 0, &earlier("primary.jpg"), "Movie"),
            row(2, 0, &token, "Movie"),
            row(3, 5, &earlier("thumb.jpg"), "Movie"),
            row(4, 5, &earlier("landscape.jpg"), "Movie"),
            row(5, 3, &earlier("banner.jpg"), "Movie"),
            row(6, 4, "/media/movies/a/logo.png", "Movie"),
            row(7, 1, &earlier("extra/art.jpg"), "Movie"),
        ];
        let stored: Vec<String> = rows
            .iter()
            .map(|row| super::spelled(&row.path, META))
            .collect();

        let (moves, kept) = plan(&rows, &stored, META, |path| !path.ends_with("banner.jpg"));

        assert_eq!(
            moves,
            [Move {
                rowid: 3,
                from: earlier("thumb.jpg"),
                to: own("landscape.jpg")
            }]
        );
        assert_eq!(kept, [earlier("primary.jpg"), earlier("landscape.jpg")]);
    }

    #[test]
    fn the_extension_is_the_mime_types() {
        for (path, ext) in [
            ("/a/primary.jpeg", ".jpg"),
            ("/a/primary.JPG", ".jpg"),
            ("/a/primary.png", ".png"),
            ("/a/primary.tbn", ".jpg"),
            ("/a/primary.Odd", ".odd"),
            ("/a/primary", ""),
        ] {
            assert_eq!(extension(path), ext, "{path}");
        }
    }
}
