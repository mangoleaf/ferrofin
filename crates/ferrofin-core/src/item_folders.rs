//! The folders named after an item's id — its art directory, its internal
//! metadata, trickplay, extracted subtitle and attachment folders — moved
//! to another id's names when the item is re-keyed: by the library scan
//! when a row's kind changes, and by the boot repair that gives a Jellyfin
//! 10.11 local version its primary's kind
//! ([`crate::adoption_repairs::rehome_local_versions`]).

use std::path::Path;

use ferrofin_db::store::guid_to_db;
use ferrofin_traits::system::PathManager;
use uuid::Uuid;

/// Renames the folders named after item `from` — its art directory, its
/// internal metadata, trickplay, extracted subtitle and attachment
/// folders — to `to`'s names ([`move_folders`]). Also returns the path
/// prefixes stored paths under them take: each renamed folder, resolved
/// and in the `%MetadataPath%` form adopted Jellyfin rows store. When
/// `to` is stored (a fold: the stored target folds into `from` first),
/// the folders at `to`'s names are its own, never an earlier attempt's
/// rename ([`move_folders`]). `art_root` is the item art root
/// (`{metadata}/library`).
pub(crate) fn move_item_folders(
    art_root: Option<&Path>,
    path_manager: Option<&dyn PathManager>,
    (from, to): (Uuid, Uuid),
    media_path: &str,
    to_stored: bool,
) -> (MovedFolders, Vec<(String, String)>) {
    let mut pairs: Vec<(String, String)> = Vec::new();
    if let Some(art) = art_root {
        for (old, new) in [
            (guid_to_db(from), guid_to_db(to)),
            (from.simple().to_string(), to.simple().to_string()),
        ] {
            let old = if old.len() == 32 {
                art.join(&old[..2]).join(&old)
            } else {
                art.join(&old)
            };
            let new = if new.len() == 32 {
                art.join(&new[..2]).join(&new)
            } else {
                art.join(&new)
            };
            pairs.push((
                old.to_string_lossy().into_owned(),
                new.to_string_lossy().into_owned(),
            ));
        }
    }
    if let Some(paths) = path_manager {
        if let (Some(old), Some(new)) = (
            paths.item_metadata_folder(from),
            paths.item_metadata_folder(to),
        ) {
            pairs.push((old, new));
        }
        // The internal folder, which the media's location does not name.
        let media = ferrofin_traits::system::MediaLocation::of_file(media_path);
        pairs.push((
            paths.trickplay_directory(from, media, false),
            paths.trickplay_directory(to, media, false),
        ));
        let (from_source, to_source) = (from.to_string(), to.to_string());
        for folder in [
            |p: &dyn PathManager, id: &str| p.subtitle_folder_path(id),
            |p: &dyn PathManager, id: &str| p.attachment_folder_path(id),
        ] {
            if let (Some(old), Some(new)) = (folder(paths, &from_source), folder(paths, &to_source))
            {
                pairs.push((old, new));
            }
        }
    }
    let folders = move_folders(pairs, !to_stored);
    // `%MetadataPath%` stands for the metadata root, the art root's parent.
    let root = art_root
        .and_then(Path::parent)
        .map(|root| root.to_string_lossy().into_owned());
    let mut prefixes = folders.renamed.clone();
    if let Some(root) = root {
        let virtual_form = |path: &str| {
            path.strip_prefix(root.as_str())
                .map(|rest| format!("%MetadataPath%{rest}"))
        };
        for (old, new) in &folders.renamed {
            if let (Some(old), Some(new)) = (virtual_form(old), virtual_form(new)) {
                prefixes.push((old, new));
            }
        }
    }
    (folders, prefixes)
}

/// What [`move_folders`] did, to be confirmed or undone once the database
/// move is known.
#[derive(Debug, Default)]
pub(crate) struct MovedFolders {
    /// The `(old, new)` folders renamed — or found already renamed by an
    /// earlier attempt cut short between its rename and its commit.
    renamed: Vec<(String, String)>,
    /// The `(aside, new)` folders found at a new name (an item no longer
    /// stored left them), moved aside until the move commits.
    aside: Vec<(String, String)>,
}

impl MovedFolders {
    /// The move committed: what was set aside goes.
    pub(crate) fn commit(self) {
        for (aside, _) in self.aside {
            if let Err(err) = std::fs::remove_dir_all(&aside) {
                tracing::warn!(%err, path = %aside, "could not remove a folder set aside");
            }
        }
    }

    /// The move was not made: every folder goes back.
    pub(crate) fn undo(self) {
        for (old, new) in self.renamed {
            if let Err(err) = std::fs::rename(&new, &old) {
                tracing::warn!(%err, from = %new, to = %old, "could not return an item's folder");
            }
        }
        for (aside, new) in self.aside {
            if let Err(err) = std::fs::rename(&aside, &new) {
                tracing::warn!(%err, from = %aside, to = %new, "could not restore a folder set aside");
            }
        }
    }
}

/// Renames each `(old, new)` folder that exists, a folder already at `new`
/// moved aside first; with `recover`, a pair whose `old` is gone and whose
/// `new` is there was renamed by an earlier attempt and counts as renamed.
/// Without it (a fold: the folder at `new` is the stored target's own), such
/// a pair is left alone, so undoing a failed move never renames the
/// target's folders. A failure is logged and leaves that folder where it
/// was.
///
/// The recovery reads the folders alone, so it cannot tell an earlier
/// attempt's work from a folder a pruned item with the same id left at `new`,
/// nor `old` recreated (by an image or trickplay pass on the still-unmoved
/// row) after a crash from the item's first `old`: in that window the
/// recreated content wins and the moved one is set aside. The window is one
/// scan's renames and transaction; the content in it is the item's own.
fn move_folders(mut pairs: Vec<(String, String)>, recover: bool) -> MovedFolders {
    pairs.sort();
    pairs.dedup();
    let mut moved = MovedFolders::default();
    for (old, new) in pairs {
        if old.is_empty() || old == new {
            continue;
        }
        let (old_path, new_path) = (Path::new(&old), Path::new(&new));
        let aside = format!("{new}.rekey-aside");
        if !old_path.is_dir() {
            if recover && new_path.is_dir() {
                // A folder set aside by that attempt goes, or comes back, with it.
                if Path::new(&aside).is_dir() {
                    moved.aside.push((aside, new.clone()));
                }
                moved.renamed.push((old, new));
            }
            continue;
        }
        let result = (|| {
            if new_path.exists() {
                if Path::new(&aside).exists() {
                    std::fs::remove_dir_all(&aside)?;
                }
                std::fs::rename(new_path, &aside)?;
            }
            if let Some(parent) = new_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(old_path, new_path)
        })();
        match result {
            Ok(()) => {
                if Path::new(&aside).exists() {
                    moved.aside.push((aside, new.clone()));
                }
                moved.renamed.push((old, new));
            }
            Err(err) => {
                tracing::warn!(
                    %err,
                    from = %old,
                    to = %new,
                    "could not move an item's folder to its new id; it stays"
                );
                if Path::new(&aside).exists() && !new_path.exists() {
                    let _ = std::fs::rename(&aside, new_path);
                }
            }
        }
    }
    moved
}

#[cfg(test)]
mod tests {
    /// An item's folders move to its new id's names: an existing one is
    /// renamed (one already at the new name set aside until the move
    /// commits), a missing one skipped, one an earlier attempt already
    /// renamed counted; a move not made is undone, the set-aside folder back.
    #[test]
    fn an_items_folders_move_to_its_new_id() {
        let tmp = tempfile::tempdir().unwrap();
        let at = |rel: &str| tmp.path().join(rel).to_string_lossy().into_owned();
        let mk = |rel: &str| std::fs::create_dir_all(tmp.path().join(rel)).unwrap();
        mk("art/OLD");
        std::fs::write(tmp.path().join("art/OLD/poster.jpg"), b"x").unwrap();
        mk("tp/NEW/kept");
        mk("tp/OLD");
        mk("done/NEW");
        let pairs = || {
            vec![
                (at("art/OLD"), at("art/NEW")),
                (at("tp/OLD"), at("tp/NEW")),
                (at("missing/OLD"), at("missing/NEW")),
                (at("done/OLD"), at("done/NEW")),
            ]
        };
        let moved = super::move_folders(pairs(), true);
        assert_eq!(
            moved.renamed,
            vec![
                (at("art/OLD"), at("art/NEW")),
                (at("done/OLD"), at("done/NEW")),
                (at("tp/OLD"), at("tp/NEW")),
            ]
        );
        assert!(tmp.path().join("art/NEW/poster.jpg").is_file());
        assert!(!tmp.path().join("tp/NEW/kept").exists(), "set aside");
        moved.undo();
        assert!(tmp.path().join("art/OLD/poster.jpg").is_file(), "undone");
        assert!(
            tmp.path().join("tp/NEW/kept").is_dir(),
            "the set-aside folder back"
        );
        assert!(tmp.path().join("done/OLD").is_dir());

        mk("done/NEW");
        std::fs::remove_dir_all(tmp.path().join("done/OLD")).unwrap();
        super::move_folders(pairs(), true).commit();
        assert!(tmp.path().join("tp/NEW").is_dir());
        assert!(
            !tmp.path().join("tp/NEW/kept").exists(),
            "gone once committed"
        );
        assert!(!std::path::Path::new(&format!("{}.rekey-aside", at("tp/NEW"))).exists());
    }

    /// A fold's folder move (the stored target's folders at the new names)
    /// counts nothing as renamed by an earlier attempt: undone after a failed
    /// move, the target's folders stay where they were, one set aside comes
    /// back, and the moving row's folder goes back to its old name.
    #[test]
    fn a_failed_folds_folder_move_leaves_the_targets_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let at = |rel: &str| tmp.path().join(rel).to_string_lossy().into_owned();
        let mk = |rel: &str| std::fs::create_dir_all(tmp.path().join(rel)).unwrap();
        mk("art/TARGET/own");
        mk("tp/TARGET/own");
        mk("tp/MOVING");
        let moved = super::move_folders(
            vec![
                (at("art/MOVING"), at("art/TARGET")),
                (at("tp/MOVING"), at("tp/TARGET")),
            ],
            false,
        );
        assert_eq!(moved.renamed, vec![(at("tp/MOVING"), at("tp/TARGET"))]);
        assert!(
            tmp.path().join("art/TARGET/own").is_dir(),
            "not the mover's"
        );
        moved.undo();
        for kept in ["art/TARGET/own", "tp/TARGET/own", "tp/MOVING"] {
            assert!(tmp.path().join(kept).is_dir(), "{kept}");
        }
        assert!(!tmp.path().join("art/MOVING").exists());
    }
}
