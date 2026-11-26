//! Where an item's art is kept under the metadata directory's `library`
//! folder, and the names it is saved under there: Jellyfin's layout
//! (`BaseItem.GetInternalMetadataPath`, `ImageSaver.GetStandardSavePath`) and
//! the folders earlier builds saved art in. One rule for the servers' saves
//! (`ferrofin-providers` re-exports it) and for the export, which moves the
//! art those builds left into Jellyfin's layout (`export::ArtRelocation`).

use std::path::{Path, PathBuf};

use ferrofin_model::entities::ImageType;
use uuid::Uuid;

use crate::store::guid_to_db;

/// The folder an item's art is saved in, `BaseItem.GetInternalMetadataPath`
/// (`BaseItem.cs:951-961`): `{root}/{idN[..2]}/{idN}`, the layout its
/// chapter images, subtitles and lyrics use too. The channel and Live TV
/// overrides are not ported, as in `PathManager`.
#[must_use]
pub fn own_art_dir(root: &Path, item_id: Uuid) -> PathBuf {
    let simple = item_id.simple().to_string();
    root.join(&simple[..2]).join(&simple)
}

/// `{root}/{ID}`, where earlier builds saved an item's art. Nothing is
/// written there any more. It is read and cleaned so that art already there
/// is found, pruned and removed with the item, and its stored paths keep
/// working.
#[must_use]
pub fn earlier_art_dir(root: &Path, item_id: Uuid) -> PathBuf {
    root.join(guid_to_db(item_id))
}

/// `{root}/{id}`, the item's id lower-cased: where older scans wrote music,
/// person, studio and embedded-cover art. Nothing is written there any more;
/// it is read and cleaned as the [`earlier_art_dir`] is.
#[must_use]
pub fn legacy_art_dir(root: &Path, item_id: Uuid) -> PathBuf {
    root.join(item_id.hyphenated().to_string())
}

/// The file name, without its extension, `ImageSaver.GetStandardSavePath`
/// gives an image saved in the item's own folder ([`own_art_dir`];
/// `saveLocally` false, `ImageSaver.cs:396-566`) for an item of `kind`, its
/// short type name: `folder` for a music album's, a music artist's, a photo
/// album's or a person's Primary and `poster` for any other; `clearart`,
/// `back`, `landscape`; `cdart` for a music album's disc and `disc` for any
/// other; `backdrop` for the first backdrop (a later one's `backdropN` is the
/// caller's, `GetBackdropSaveFilename`); else the type's name lower-cased.
/// A stored type name (`MediaBrowser.Controller.Entities.Audio.MusicAlbum`)
/// is read by its last part.
#[must_use]
pub fn internal_image_stem(kind: &str, image_type: ImageType) -> String {
    let kind = kind.rsplit('.').next().unwrap_or(kind);
    match image_type {
        ImageType::Primary
            if matches!(kind, "MusicAlbum" | "MusicArtist" | "PhotoAlbum" | "Person") =>
        {
            "folder".into()
        }
        ImageType::Primary => "poster".into(),
        ImageType::Art => "clearart".into(),
        ImageType::BoxRear => "back".into(),
        ImageType::Thumb => "landscape".into(),
        ImageType::Disc if kind == "MusicAlbum" => "cdart".into(),
        // `ToString()` of a value the enum does not name is its number.
        ImageType::Unrecognized(value) => value.to_string(),
        other => format!("{other:?}").to_ascii_lowercase(),
    }
}
