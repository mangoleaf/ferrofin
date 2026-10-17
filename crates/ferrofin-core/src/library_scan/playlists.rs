//! Playlists the library scan resolves off disk — port of `PlaylistResolver`
//! (`Emby.Server.Implementations/Library/Resolvers/PlaylistResolver.cs`) and
//! of the local readers that fill a resolved playlist's members: a playlist
//! folder's `playlist.xml` (`PlaylistXmlProvider`), a playlist file's own
//! entries (`PlaylistItemsProvider`).
//!
//! ACCEPTED DIVERGENCE (upstream bug, not ported): where Jellyfin 12.2 saves
//! a playlist's XML metadata — a library whose metadata savers are unset, as
//! one made through the API is (`IsSaverEnabledForItem`) — it leaves every
//! scanned playlist FILE empty. Its first save writes a `<name>.xml` sidecar
//! beside the file (`Playlist.IsSaveLocalMetadataEnabled` is always true)
//! with no `<PlaylistItems>` — `BaseXmlSaver` writes members only for a
//! playlist that is no file — and `PlaylistXmlProvider`, ordered before
//! `PlaylistItemsProvider`, then reads that sidecar, has metadata, and ends
//! the local readers before the file's entries are read (measured live).
//! Without the sidecar it fills the members from the file, as here. Here a
//! playlist file's members always come from the file, and a sidecar beside
//! it is never read for them.
//!
//! ACCEPTED DIVERGENCE (upstream bug, not ported): once `PlaylistItemsProvider`
//! has read a playlist file, 12.2's `PlaylistMetadataService.MergeData`
//! copies the reader's `PlaylistMediaType` — the `Unknown` of the fresh
//! `Playlist` it returns — over the resolver's `Audio` (measured live: the
//! `MediaType` column and `Data` turn `Unknown`). A playlist file keeps
//! `Audio` here.
//!
//! ACCEPTED DIVERGENCE (upstream bug, not ported): a refresh that keeps
//! metadata merges what a reader found into the stored members with
//! `DistinctBy(ItemId ?? Path ?? "")`. A `playlist.xml` entry carries a path
//! and the stored members an id, so the keys never meet: every stored member
//! is appended again after the XML's, and path-less `<ItemId>` entries all
//! share the key `""` and collapse into one. Here entries are told apart by
//! the item they resolve to, whatever named it.
//!
//! `PlaylistResolver` has `ResolverPriority.First`, so it answers before any
//! artist, album, series, movie or audio resolver:
//! - a DIRECTORY whose name contains `[playlist]` (any case; the marker is
//!   cut from the name), or that holds a `playlist.xml`, is a playlist in any
//!   library. `Playlist` loads and validates no children, so nothing inside
//!   it is resolved;
//! - a FILE with a playlist extension in a music or untyped library is a
//!   playlist named after the file, `IsInMixedFolder`, `PlaylistMediaType`
//!   `Audio`.
//!
//! Both are `OpenAccess` and owned by nobody. The resolver's third arm, any
//! directory in the internal `PlaylistsFolder`, keeps Jellyfin's walk of
//! `{data}/playlists` from pruning an API playlist whose `playlist.xml` is
//! gone. Ferrofin never walks that folder: its API playlists are database
//! rows, which no scan prunes.

use std::path::Path;

use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::entities_media::VirtualFolderInfo;
use ferrofin_model::io::{FileSystemEntryInfo, FileSystemEntryType};
use ferrofin_traits::persistence::{PlaylistMember, ScannedPlaylist};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
use unicode_normalization::UnicodeNormalization as _;
use uuid::Uuid;

use super::{LibraryScanner, PlanCtx, Planned, entry_name, file_stem, folder_name};

/// `PlaylistResolver`'s directory marker, matched case-insensitively.
const FOLDER_MARKER: &str = "[playlist]";

/// The name `dir` resolves with as a playlist folder, or `None` when it is
/// none — the directory arm of `PlaylistResolver.Resolve`. `holds_xml` is
/// asked only when the name alone does not decide.
pub(super) fn playlist_folder_name(dir: &str, holds_xml: impl FnOnce() -> bool) -> Option<String> {
    let name = folder_name(dir)?;
    let lower = name.to_ascii_lowercase();
    if lower.contains(FOLDER_MARKER) {
        // `Replace("[playlist]", "", OrdinalIgnoreCase).Trim()` — one
        // left-to-right pass — and `EnsureName` gives a name left empty the
        // folder's own.
        let mut cut = String::with_capacity(name.len());
        let mut rest = 0;
        for (at, _) in lower.match_indices(FOLDER_MARKER) {
            cut.push_str(&name[rest..at]);
            rest = at + FOLDER_MARKER.len();
        }
        cut.push_str(&name[rest..]);
        let cut = cut.trim();
        return Some(if cut.is_empty() { name } else { cut.to_owned() });
    }
    holds_xml().then_some(name)
}

/// Whether a folder's `entries` hold a `playlist.xml` — `filePaths.Any(f =>
/// f.EndsWith("playlist.xml", OrdinalIgnoreCase))` over its FILES.
///
/// ACCEPTED DIVERGENCE (upstream bug, not ported): a `<stem>.xml` beside a
/// playlist file `<stem>.m3u` is that file's metadata sidecar
/// (`PlaylistXmlSaver.GetSavePath`), which Jellyfin itself writes into the
/// library. Upstream's suffix test takes the one beside `playlist.m3u` or
/// `Road Trip Playlist.m3u` for a playlist folder's own, and the album folder
/// holding it — tracks and all — becomes an empty playlist on the next scan.
/// Such a sidecar does not count here.
pub(super) fn holds_playlist_xml(entries: &[FileSystemEntryInfo]) -> bool {
    let files = || {
        entries
            .iter()
            .filter(|entry| entry.type_ != FileSystemEntryType::Directory)
    };
    files().any(|entry| {
        entry_name(&entry.path)
            .to_ascii_lowercase()
            .ends_with("playlist.xml")
            && !files().any(|other| {
                is_playlist_file(&other.path)
                    && Path::new(&other.path).with_extension("xml") == Path::new(&entry.path)
            })
    })
}

/// Whether `path` is a playlist file (`Playlist.SupportedExtensions`, any
/// case) — which the file arm claims before `AudioResolver` would take a
/// `.pls` as a track.
pub(super) fn is_playlist_file(path: &str) -> bool {
    ferrofin_providers::is_playlist_file(path)
}

/// A playlist the walk saved whose local readers this refresh runs.
#[derive(Debug)]
pub(super) struct DuePlaylist {
    /// The playlist.
    pub(super) id: Uuid,
    /// Its file or folder.
    pub(super) path: String,
    /// Whether what the readers return replaces the stored members.
    pub(super) replace: bool,
}

/// Whether a refresh with `options` replaces a playlist's members with what
/// its reader returned (`RefreshWithProviders`' `shouldReplace`, `Metadata
/// Service.cs:914-917`) rather than putting them first and keeping the
/// stored ones after (`PlaylistMetadataService.MergeData`).
pub(super) fn replaces_members(options: &MetadataRefreshOptions) -> bool {
    let mode = options.metadata_refresh_mode;
    let past_validation = matches!(
        mode,
        MetadataRefreshMode::Default | MetadataRefreshMode::FullRefresh
    );
    (past_validation && options.replace_all_metadata)
        || (mode == MetadataRefreshMode::Default && !options.replace_all_metadata)
}

/// The physical locations `PlaylistItemsProvider` resolves entries in: those
/// of every library with a collection type, other than box sets (`livetv`
/// and `playlists` have no `CollectionTypeOptions`).
fn member_roots(folders: &[VirtualFolderInfo]) -> Vec<String> {
    folders
        .iter()
        .filter(|folder| {
            !matches!(
                folder.collection_type,
                None | Some(CollectionTypeOptions::mixed | CollectionTypeOptions::boxsets)
            )
        })
        .flat_map(|folder| folder.locations.iter().cloned())
        .collect()
}

/// A playlist folder's `<PlaylistMediaType>` (`PlaylistXmlParser`), one of
/// `MediaType`'s names, or `Unknown`.
fn folder_media_type(dir: &str) -> &'static str {
    let declared = std::fs::read(ferrofin_providers::playlist_xml_path(dir, false))
        .ok()
        .and_then(|bytes| {
            ferrofin_providers::parse_container_xml(&ferrofin_providers::decode_playlist(&bytes))
        })
        .and_then(|xml| xml.playlist_media_type);
    ["Video", "Audio", "Photo", "Book"]
        .into_iter()
        .find(|known| {
            declared
                .as_deref()
                .is_some_and(|d| d.eq_ignore_ascii_case(known))
        })
        .unwrap_or("Unknown")
}

/// Whether the playlist at `path` is a file (`Playlist.IsPlaylistFile`: an
/// extension, and no directory).
fn is_file_playlist(path: &str) -> bool {
    Path::new(path).extension().is_some() && !Path::new(path).is_dir()
}

/// What the local reader returns for the playlist at `path`, or `None` when
/// it has nothing (`HasMetadata` false), which leaves the stored members as
/// they are.
fn read_members(path: &str, roots: &[String]) -> Option<Vec<PlaylistMember>> {
    if !is_file_playlist(path) {
        // `PlaylistXmlProvider`: a `playlist.xml` that reads is metadata, an
        // empty item list too. Its media type is the planner's
        // ([`folder_media_type`]).
        //
        // TODO(parity, open work item): the rest of what `PlaylistXmlParser`
        // reads — `<LocalTitle>`, `<Overview>`, `<Shares>`, `<LockData>` —
        // is not applied, and `PlaylistXmlSaver` (which writes the folder's
        // `playlist.xml` back, and the `<name>.xml` sidecar beside a playlist
        // FILE whose read is the upstream bug this module does not port) has
        // no port. Port path: carry the parsed fields in `ScannedPlaylist`
        // and merge them in `sync_scanned_playlists` as `MergeBaseItemData`
        // does (locked fields kept, a blank title never taken); write the
        // folder's `playlist.xml` from the playlist manager's updates, never
        // a sidecar beside a file.
        let xml = ferrofin_providers::playlist_xml_path(path, false);
        let parsed = ferrofin_providers::parse_container_xml(
            &ferrofin_providers::decode_playlist(&std::fs::read(xml).ok()?),
        )?;
        return Some(
            parsed
                .children
                .into_iter()
                .map(|child| PlaylistMember {
                    item_id: child
                        .item_id
                        .as_deref()
                        .and_then(|id| Uuid::parse_str(id).ok()),
                    path: child.path,
                })
                .collect(),
        );
    }
    // `PlaylistItemsProvider`: only entries that resolve — none is no
    // metadata.
    if !is_playlist_file(path) {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let entries =
        ferrofin_providers::read_playlist_file(path, &ferrofin_providers::decode_playlist(&bytes));
    let members: Vec<PlaylistMember> = entries
        .iter()
        .filter_map(|entry| member_path(&entry.to_string_lossy(), roots))
        .map(|path| PlaylistMember {
            item_id: None,
            path: Some(path),
        })
        .collect();
    (!members.is_empty()).then_some(members)
}

/// `PlaylistItemsProvider.TryResolvePlaylistItem`'s file half: the entry when
/// it exists — as written, else in the other Unicode normal form
/// (`TryNormalizePath`) — and lies under a library location (an
/// `OrdinalIgnoreCase` prefix test); the item lookup is the store's.
fn member_path(entry: &str, roots: &[String]) -> Option<String> {
    let exists = |path: &str| Path::new(path).is_file();
    let path = if exists(entry) {
        entry.to_owned()
    } else {
        [entry.nfc().collect::<String>(), entry.nfd().collect()]
            .into_iter()
            .find(|form| form != entry && exists(form))?
    };
    let upper = ferrofin_util::string_extensions::upper_invariant(&path);
    roots
        .iter()
        .any(|root| upper.starts_with(&ferrofin_util::string_extensions::upper_invariant(root)))
        .then_some(path)
}

impl LibraryScanner {
    /// Plans the playlist `PlaylistResolver` makes of `path`: a playlist
    /// file (`folder` `None`), or the folder named `folder`.
    pub(super) fn plan_playlist(
        &self,
        path: &str,
        folder: Option<String>,
        cf: Uuid,
        (parent, ancestors): (Uuid, Vec<Uuid>),
        ctx: &PlanCtx<'_>,
        out: &mut Vec<Planned>,
    ) {
        let is_file = folder.is_none();
        if !is_file {
            // Nothing below a playlist folder resolves: rows an older scan
            // planned there (an album and its tracks, say) are reconciled
            // away. The playlist itself is planned, so it is never stale.
            ctx.excluded.borrow_mut().push(path.to_owned());
        }
        let name = folder.unwrap_or_else(|| file_stem(path));
        let Some((id, mut entity)) =
            self.base_item(ctx, BaseItemKind::Playlist, cf, parent, name, path, true)
        else {
            return;
        };
        // `MediaType => PlaylistMediaType`: `Audio` for a playlist file; a
        // folder's is its `playlist.xml`'s (`PlaylistXmlParser`), else
        // `Unknown`, the enum's default.
        let media_type = if is_file {
            "Audio"
        } else {
            folder_media_type(path)
        };
        entity.media_type = Some(media_type.to_owned());
        entity.is_in_mixed_folder = is_file;
        entity.data = Some(
            serde_json::json!({
                "OwnerUserId": crate::item_data::json_guid(Uuid::nil()),
                "OpenAccess": true,
                "Shares": [],
                "PlaylistMediaType": media_type,
            })
            .to_string(),
        );
        ctx.emit(
            out,
            Planned {
                id,
                entity,
                ancestors,
            },
        );
    }

    /// Plans each playlist file among `entries` under `parent` — the file
    /// arm, for a caller in a music or untyped library.
    pub(super) fn plan_playlist_files(
        &self,
        entries: &[FileSystemEntryInfo],
        cf: Uuid,
        (parent, ancestors): (Uuid, &[Uuid]),
        ctx: &PlanCtx<'_>,
        out: &mut Vec<Planned>,
    ) {
        for entry in entries {
            if entry.type_ != FileSystemEntryType::Directory && is_playlist_file(&entry.path) {
                self.plan_playlist(
                    &entry.path,
                    None,
                    cf,
                    (parent, ancestors.to_vec()),
                    ctx,
                    out,
                );
            }
        }
    }

    /// Plans `dir` when it is a playlist folder, which the walk then leaves
    /// alone, and says whether it was. `holds_xml` answers from the caller's
    /// listing of `dir`, asked only when the name does not decide: a walk
    /// that lists `dir` next anyway takes it ahead ([`Self::prelist`]).
    pub(super) fn plan_playlist_folder(
        &self,
        dir: &str,
        holds_xml: impl FnOnce() -> bool,
        cf: Uuid,
        (parent, ancestors): (Uuid, Vec<Uuid>),
        ctx: &PlanCtx<'_>,
        out: &mut Vec<Planned>,
    ) -> bool {
        let Some(name) = playlist_folder_name(dir, holds_xml) else {
            return false;
        };
        // Nothing below a playlist folder is walked.
        ctx.prelisted.borrow_mut().remove(dir);
        self.plan_playlist(dir, Some(name), cf, (parent, ancestors), ctx, out);
        true
    }

    /// Runs the local readers of every playlist in `due` and writes what
    /// they found, with each one's ownership row
    /// ([`ItemPersistenceService::sync_scanned_playlists`]). Once the walk
    /// has saved every row, so each entry finds its item. Best-effort, as
    /// the version links are: a failure is logged and the next refresh of
    /// the playlist retries.
    ///
    /// [`ItemPersistenceService::sync_scanned_playlists`]: ferrofin_traits::persistence::ItemPersistenceService::sync_scanned_playlists
    pub(super) async fn sync_playlists(&self, due: &[DuePlaylist], folders: &[VirtualFolderInfo]) {
        if due.is_empty() {
            return;
        }
        let roots = member_roots(folders);
        let reads: Vec<(Uuid, String, bool)> = due
            .iter()
            .map(|p| (p.id, p.path.clone(), p.replace))
            .collect();
        let playlists = match tokio::task::spawn_blocking(move || {
            reads
                .into_iter()
                .map(|(id, path, replace)| ScannedPlaylist {
                    id,
                    members: read_members(&path, &roots),
                    from_file: is_file_playlist(&path),
                    replace,
                })
                .collect::<Vec<_>>()
        })
        .await
        {
            Ok(playlists) => playlists,
            Err(err) => {
                tracing::warn!(%err, "could not read the playlists' files; the next refresh retries");
                return;
            }
        };
        if let Err(err) = self.persistence.sync_scanned_playlists(&playlists).await {
            tracing::warn!(
                %err,
                playlists = playlists.len(),
                "could not write the playlists' members; the next refresh retries"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> FileSystemEntryInfo {
        FileSystemEntryInfo {
            name: entry_name(path).to_owned(),
            path: path.to_owned(),
            type_: FileSystemEntryType::File,
        }
    }

    /// `PlaylistResolver`'s directory arm: the marker anywhere in the name,
    /// any case, cut out and trimmed; else a `playlist.xml` among the files.
    #[rstest::rstest]
    #[case::marker("/m/Road Trip [playlist]", &[], Some("Road Trip"))]
    #[case::marker_any_case("/m/[PlayList] Road Trip/", &[], Some("Road Trip"))]
    #[case::marker_twice("/m/A [playlist] B [playlist]", &[], Some("A  B"))]
    #[case::marker_one_pass("/m/[play[playlist]list]", &[], Some("[playlist]"))]
    #[case::marker_alone("/m/[playlist]", &[], Some("[playlist]"))]
    #[case::xml("/m/Mix", &["/m/Mix/playlist.xml"], Some("Mix"))]
    #[case::xml_suffix("/m/Mix", &["/m/Mix/MyPlaylist.XML"], Some("Mix"))]
    #[case::plain("/m/Album", &["/m/Album/01.flac"], None)]
    fn the_directory_arm(#[case] dir: &str, #[case] files: &[&str], #[case] name: Option<&str>) {
        let entries: Vec<_> = files.iter().map(|f| file(f)).collect();
        assert_eq!(
            playlist_folder_name(dir, || holds_playlist_xml(&entries)).as_deref(),
            name
        );
    }

    #[test]
    fn a_playlist_xml_folder_must_hold_it_as_a_file() {
        let dir = FileSystemEntryInfo {
            type_: FileSystemEntryType::Directory,
            ..file("/m/Mix/playlist.xml")
        };
        assert!(!holds_playlist_xml(&[dir]));
    }

    /// A playlist file's own `<stem>.xml` sidecar is no `playlist.xml`: the
    /// album folder holding `playlist.m3u` stays an album.
    #[rstest::rstest]
    #[case::sidecar_of_playlist_m3u(&["/m/A/playlist.m3u", "/m/A/playlist.xml"], false)]
    #[case::sidecar_any_case(&["/m/A/Road Trip Playlist.M3U", "/m/A/Road Trip Playlist.xml"], false)]
    #[case::xml_without_its_file(&["/m/A/Road Trip Playlist.xml"], true)]
    #[case::folder_xml_beside_another_file(&["/m/A/mix.m3u", "/m/A/playlist.xml"], true)]
    fn a_playlist_files_sidecar_is_no_playlist_xml(#[case] files: &[&str], #[case] holds: bool) {
        let entries: Vec<_> = files.iter().map(|f| file(f)).collect();
        assert_eq!(holds_playlist_xml(&entries), holds);
    }

    #[test]
    fn a_marked_name_needs_no_listing() {
        let name = playlist_folder_name("/m/Mix [playlist]", || panic!("listed"));
        assert_eq!(name.as_deref(), Some("Mix"));
    }

    /// `shouldReplace`: a Default scan and any replace-all refresh replace
    /// the members; a full refresh that keeps metadata, and a validation,
    /// merge the stored ones after.
    #[rstest::rstest]
    #[case(MetadataRefreshMode::Default, false, true)]
    #[case(MetadataRefreshMode::FullRefresh, true, true)]
    #[case(MetadataRefreshMode::Default, true, true)]
    #[case(MetadataRefreshMode::FullRefresh, false, false)]
    #[case(MetadataRefreshMode::ValidationOnly, false, false)]
    #[case(MetadataRefreshMode::ValidationOnly, true, false)]
    fn members_replace_as_should_replace_says(
        #[case] mode: MetadataRefreshMode,
        #[case] replace_all: bool,
        #[case] replaces: bool,
    ) {
        let options = MetadataRefreshOptions {
            metadata_refresh_mode: mode,
            replace_all_metadata: replace_all,
            ..MetadataRefreshOptions::default()
        };
        assert_eq!(replaces_members(&options), replaces);
    }

    #[test]
    fn entries_resolve_on_disk_under_a_library_and_in_either_normal_form() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_string_lossy().into_owned();
        // Stored decomposed, as macOS writes it; listed composed.
        let nfd: String = format!("{root}/Cafe\u{301}.mp3");
        std::fs::write(&nfd, b"").expect("write");
        let nfc = format!("{root}/Caf\u{e9}.mp3");
        let roots = vec![root.to_uppercase()];
        assert_eq!(member_path(&nfc, &roots), Some(nfd.clone()));
        assert_eq!(member_path(&nfd, &roots), Some(nfd.clone()));
        assert_eq!(member_path(&format!("{root}/missing.mp3"), &roots), None);
        assert_eq!(member_path(&nfd, &["/elsewhere".to_owned()]), None);
    }

    #[test]
    fn a_file_playlist_reads_its_entries_and_never_its_sidecar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_string_lossy().into_owned();
        let track = format!("{root}/a.mp3");
        std::fs::write(&track, b"").expect("write");
        let m3u = format!("{root}/Mix.m3u");
        std::fs::write(&m3u, "a.mp3\nmissing.mp3\na.mp3\n").expect("write");
        // The sidecar Jellyfin writes beside it, with no members.
        std::fs::write(
            format!("{root}/Mix.xml"),
            "<?xml version=\"1.0\"?><Item><LocalTitle>Mix</LocalTitle></Item>",
        )
        .expect("write");
        let roots = vec![root.clone()];
        let member = PlaylistMember {
            item_id: None,
            path: Some(track.clone()),
        };
        assert_eq!(
            read_members(&m3u, &roots),
            Some(vec![member.clone(), member])
        );
        // Nothing that resolves is no metadata.
        std::fs::write(&m3u, "missing.mp3\n").expect("write");
        assert_eq!(read_members(&m3u, &roots), None);
    }

    #[test]
    fn a_playlist_folder_reads_its_playlist_xml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let folder = dir.path().join("Road Trip [playlist]");
        std::fs::create_dir(&folder).expect("mkdir");
        let folder = folder.to_string_lossy().into_owned();
        // No `playlist.xml`: nothing to say.
        assert_eq!(read_members(&folder, &[]), None);
        let id = Uuid::new_v4();
        std::fs::write(
            format!("{folder}/playlist.xml"),
            format!(
                "<?xml version=\"1.0\"?><Item><PlaylistItems>\
                 <PlaylistItem><Path>/m/a.mp3</Path></PlaylistItem>\
                 <PlaylistItem><ItemId>{}</ItemId></PlaylistItem>\
                 </PlaylistItems></Item>",
                id.simple()
            ),
        )
        .expect("write");
        assert_eq!(
            read_members(&folder, &[]),
            Some(vec![
                PlaylistMember {
                    item_id: None,
                    path: Some("/m/a.mp3".to_owned())
                },
                PlaylistMember {
                    item_id: Some(id),
                    path: None
                },
            ])
        );
    }
}
