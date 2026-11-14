//! The Blu-ray main-playlist reader: the subset of the BDInfo library
//! (`BDROM`, `TSPlaylistFile`, `TSStreamClipFile`; the `BDInfo` 0.8.0 package
//! Jellyfin 12.4 pins; Copyright 2010 Cinema Squid, LGPL-2.1-or-later,
//! github.com/jellyfin/BDInfo, ported here under the GPL as LGPL-2.1 section 3
//! allows) that `BdInfoExaminer.GetDiscInfo` needs for its `Files`:
//! the clip files of the longest valid playlist, angle 0 only (upstream PR 17824).
//!
//! BDInfo reads much more (stream codecs from the clip-info and `.m2ts` files,
//! chapters), which only the disc's media streams need. This reader parses the
//! play items of every `.mpls` and checks the clip-info header of every
//! `.clpi`, which is all the file list depends on. A playlist's stream table
//! and chapter marks, and a clip-info file's body, are not read, so a disc
//! damaged only there plays here where BDInfo throws on the whole disc. The
//! probe half of disc-rip support (gl-87) extends this reader with them.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use crate::error::MediaEncodingError;

/// MPLS timestamps count a 45 kHz clock.
const MPLS_CLOCK_HZ: f64 = 45_000.0;

/// One clip of a playlist (`TSStreamClip`).
#[derive(Debug, Clone, PartialEq)]
struct Clip {
    /// `{item}.M2TS`, the key into the stream files.
    name: String,
    /// 0 for the main path, `n` for the `n`th alternative angle.
    angle_index: u32,
    time_in: f64,
    length: f64,
}

/// A parsed playlist (`TSPlaylistFile`).
#[derive(Debug)]
struct Playlist {
    clips: Vec<Clip>,
}

impl Playlist {
    /// `TSPlaylistFile.TotalLength`: the main path's clips, in seconds.
    fn total_length(&self) -> f64 {
        self.clips
            .iter()
            .filter(|c| c.angle_index == 0)
            .map(|c| c.length)
            .sum()
    }

    /// `TSPlaylistFile.IsValid` under Jellyfin's `BDInfoSettings`: looping
    /// playlists are filtered (`FilterLoopingPlaylists`), short ones are not
    /// (`FilterShortPlaylists` is off). `HasLoops` comes from `Initialize`: a
    /// main-path clip played twice from the same in-time.
    fn is_valid(&self) -> bool {
        let mut seen: HashMap<&str, Vec<f64>> = HashMap::new();
        for clip in self.clips.iter().filter(|c| c.angle_index == 0) {
            let times = seen.entry(clip.name.as_str()).or_default();
            if times.contains(&clip.time_in) {
                return false;
            }
            times.push(clip.time_in);
        }
        true
    }
}

/// The main playlist's clip files, angle 0 only, as full paths (port of
/// `BdInfoExaminer.GetDiscInfo(path).Files`). It reads the disc: call it on
/// the blocking pool.
///
/// An empty list means the disc has no valid playlist, as upstream returns no
/// files then.
///
/// # Errors
///
/// The cases where BDInfo throws: no `BDMV` structure (`BDMV` with `CLIPINF`
/// and `PLAYLIST`), an unreadable file, a clip-info file of an unknown type, a
/// playlist of an unknown type, truncated, or naming a clip with no `.clpi`
/// (or an angle with no `.m2ts`), and a main playlist naming a missing `.m2ts`.
pub fn primary_playlist_m2ts_files(path: &Path) -> Result<Vec<String>, MediaEncodingError> {
    let bdmv = bdmv_directory(path).ok_or_else(|| unable_to_locate(path))?;
    let clipinf = child_directory(&bdmv, "CLIPINF").ok_or_else(|| unable_to_locate(path))?;
    let playlist_dir = child_directory(&bdmv, "PLAYLIST").ok_or_else(|| unable_to_locate(path))?;
    let stream_files: HashMap<String, PathBuf> = child_directory(&bdmv, "STREAM")
        .map(|dir| files_with_extension(&dir, "m2ts"))
        .transpose()?
        .unwrap_or_default();
    let clip_files = files_with_extension(&clipinf, "clpi")?;
    // `BDROM.Scan` scans every clip-info file before any playlist; only the
    // type header decides anything here.
    for (name, file) in &clip_files {
        let mut header = [0u8; 8];
        std::fs::File::open(file)
            .and_then(|mut f| f.read_exact(&mut header))
            .map_err(|e| unreadable(file, e))?;
        check_clip_info(name, &header)?;
    }
    let mut playlists: Vec<(String, Playlist)> = Vec::new();
    for (name, file) in files_with_extension(&playlist_dir, "mpls")? {
        let data = std::fs::read(&file).map_err(|e| unreadable(&file, e))?;
        let playlist = parse_playlist(&name, &data, &stream_files, &clip_files)?;
        playlists.push((name, playlist));
    }
    // BDInfo keeps the directory's enumeration order, so equally long
    // playlists tie on whatever the file system lists first; ordering by name
    // first makes the pick repeatable.
    playlists.sort_by(|a, b| a.0.cmp(&b.0));
    // `OrderByDescending(p => p.TotalLength).FirstOrDefault(p => p.IsValid)`:
    // a stable sort, so the first of equally long playlists wins.
    playlists.sort_by(|a, b| b.1.total_length().total_cmp(&a.1.total_length()));
    let Some((_, main)) = playlists.iter().find(|(_, p)| p.is_valid()) else {
        return Ok(Vec::new());
    };
    main.clips
        .iter()
        .filter(|c| c.angle_index == 0)
        .map(|c| {
            stream_files
                .get(&c.name)
                .map(|p| p.to_string_lossy().into_owned())
                .ok_or_else(|| {
                    MediaEncodingError::process(format!(
                        "playlist references missing file {}",
                        c.name
                    ))
                })
        })
        .collect()
}

fn unreadable(file: &Path, error: std::io::Error) -> MediaEncodingError {
    MediaEncodingError::io(format!("cannot read {}", file.display()), error)
}

fn unable_to_locate(path: &Path) -> String {
    format!("unable to locate BD structure in {}", path.display())
}

/// `BDROM.GetDirectoryBDMV`: the path itself or an ancestor named `BDMV`,
/// else a `BDMV` child of the path.
fn bdmv_directory(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|dir| dir.file_name().is_some_and(|n| n == "BDMV"))
        .map(Path::to_path_buf)
        .or_else(|| child_directory(path, "BDMV"))
}

/// `BDROM.GetDirectory(name, dir, 0)`: a direct, non-hidden child directory
/// whose name matches exactly.
fn child_directory(dir: &Path, name: &str) -> Option<PathBuf> {
    let child = dir.join(name);
    std::fs::read_dir(dir)
        .inspect_err(|error| {
            tracing::debug!(path = %dir.display(), %error, "cannot list a Blu-ray rip's folder");
        })
        .ok()?
        .filter_map(Result::ok)
        .any(|e| e.file_name() == name && e.path().is_dir())
        .then_some(child)
}

/// The non-hidden files of `dir` with the given extension, any case, keyed by
/// their upper-cased name (`BdInfoDirectoryInfo.GetFiles(".ext")` through
/// Jellyfin's case-insensitive extension filter, then `Name.ToUpper()`).
fn files_with_extension(
    dir: &Path,
    ext: &str,
) -> Result<HashMap<String, PathBuf>, MediaEncodingError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| MediaEncodingError::io(format!("cannot list {}", dir.display()), e))?;
    Ok(entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_file())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let matches = !name.starts_with('.')
                && Path::new(&name)
                    .extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case(ext));
            matches.then(|| (name.to_uppercase(), e.path()))
        })
        .collect())
}

/// The header check of `TSStreamClipFile.Scan`.
fn check_clip_info(name: &str, data: &[u8]) -> Result<(), String> {
    let file_type = data
        .get(..8)
        .map(String::from_utf8_lossy)
        .unwrap_or_default();
    if matches!(&*file_type, "HDMV0100" | "HDMV0200" | "HDMV0300") {
        Ok(())
    } else {
        Err(format!(
            "clip info file {name} has an unknown file type {file_type}"
        ))
    }
}

/// A bounds-checked big-endian reader over the playlist bytes; running off
/// the end is the `IndexOutOfRangeException` BDInfo throws.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    name: &'a str,
}

impl Reader<'_> {
    fn bytes(&mut self, n: usize) -> Result<&[u8], String> {
        let out = self
            .pos
            .checked_add(n)
            .and_then(|end| self.data.get(self.pos..end))
            .ok_or_else(|| format!("playlist {} is truncated", self.name))?;
        self.pos += n;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, String> {
        let b = self.bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn ascii(&mut self, n: usize) -> Result<String, String> {
        Ok(String::from_utf8_lossy(self.bytes(n)?).into_owned())
    }

    fn skip(&mut self, n: usize) -> Result<(), String> {
        self.bytes(n).map(|_| ())
    }

    /// `ReadInt32` of a time, with its sign bit masked off.
    fn seconds(&mut self) -> Result<f64, String> {
        Ok(f64::from(self.u32()? & 0x7FFF_FFFF) / MPLS_CLOCK_HZ)
    }
}

/// The play-item half of `TSPlaylistFile.Scan`: each item's clip, in and out
/// time, and its alternative angles. The per-item stream entries are skipped
/// (upstream re-positions to the item's end after reading them), and the
/// chapter marks are not read.
fn parse_playlist(
    name: &str,
    data: &[u8],
    stream_files: &HashMap<String, PathBuf>,
    clip_files: &HashMap<String, PathBuf>,
) -> Result<Playlist, String> {
    let mut r = Reader { data, pos: 0, name };
    let file_type = r.ascii(8)?;
    if !matches!(file_type.as_str(), "MPLS0100" | "MPLS0200" | "MPLS0300") {
        return Err(format!(
            "playlist {name} has an unknown file type {file_type}"
        ));
    }
    let playlist_offset = r.u32()?;
    // The chapter and extension offsets, then the misc-flags byte BDInfo
    // reads at 0x38 (a header shorter than that throws there).
    r.skip(8)?;
    r.pos = 0x38;
    r.u8()?;
    r.pos = usize::try_from(playlist_offset).map_err(|e| e.to_string())?;
    r.skip(4 + 2)?; // length, reserved
    let item_count = r.u16()?;
    r.skip(2)?; // sub-path count
    let mut clips = Vec::new();
    for _ in 0..item_count {
        let item_start = r.pos;
        let item_length = usize::from(r.u16()?);
        let item_name = r.ascii(5)?;
        r.skip(4)?; // codec id
        let clip_name = format!("{item_name}.M2TS");
        if !clip_files.contains_key(&format!("{item_name}.CLPI")) {
            return Err(format!(
                "playlist {name} referenced missing file {clip_name}"
            ));
        }
        r.skip(1)?;
        let multi_angle = (r.u8()? >> 4) & 0x01 != 0;
        r.skip(1)?;
        let time_in = r.seconds()?;
        let time_out = r.seconds()?;
        let length = time_out - time_in;
        clips.push(Clip {
            name: clip_name,
            angle_index: 0,
            time_in,
            length,
        });
        r.skip(12)?;
        if multi_angle {
            let angles = r.u8()?;
            r.skip(1)?;
            for angle in 1..u32::from(angles) {
                let angle_name = r.ascii(5)?;
                r.skip(4 + 1)?; // codec id, STC id
                let file = format!("{angle_name}.M2TS");
                if !stream_files.contains_key(&file)
                    || !clip_files.contains_key(&format!("{angle_name}.CLPI"))
                {
                    return Err(format!(
                        "playlist {name} referenced missing angle file {file}"
                    ));
                }
                clips.push(Clip {
                    name: file,
                    angle_index: angle,
                    time_in,
                    length,
                });
            }
        }
        // `pos += itemLength - (pos - itemStart) + 2`: the item's length
        // field does not count itself.
        r.pos = item_start + item_length + 2;
    }
    Ok(Playlist { clips })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// One play item: `(clip, in, out, angle clips)`, times in 45 kHz ticks.
    pub(crate) type Item<'a> = (&'a str, u32, u32, &'a [&'a str]);

    /// An MPLS file with the given play items, laid out as BDInfo reads it.
    pub(crate) fn mpls(items: &[Item<'_>]) -> Vec<u8> {
        let mut out = b"MPLS0200".to_vec();
        let playlist_offset: u32 = 0x3A;
        out.extend(playlist_offset.to_be_bytes());
        out.extend(0u32.to_be_bytes()); // chapters
        out.extend(0u32.to_be_bytes()); // extensions
        out.resize(0x3A, 0);
        let mut body = Vec::new();
        body.extend(0u16.to_be_bytes()); // reserved
        body.extend(u16::try_from(items.len()).unwrap().to_be_bytes());
        body.extend(0u16.to_be_bytes()); // sub-paths
        for (clip, time_in, time_out, angles) in items {
            let mut item = Vec::new();
            item.extend(clip.as_bytes());
            item.extend(b"M2TS");
            item.push(0);
            item.push(if angles.is_empty() { 0 } else { 0x10 });
            item.push(0);
            item.extend(time_in.to_be_bytes());
            item.extend(time_out.to_be_bytes());
            item.extend([0u8; 12]);
            if !angles.is_empty() {
                item.push(u8::try_from(angles.len() + 1).unwrap());
                item.push(0);
                for angle in *angles {
                    item.extend(angle.as_bytes());
                    item.extend(b"M2TS");
                    item.push(0);
                }
            }
            // An empty STN table.
            item.extend(16u16.to_be_bytes());
            item.extend([0u8; 16]);
            body.extend(u16::try_from(item.len()).unwrap().to_be_bytes());
            body.extend(item);
        }
        out.extend(u32::try_from(body.len()).unwrap().to_be_bytes());
        out.extend(body);
        out
    }

    /// A disc under `root/BDMV`: the playlists, plus a `.clpi` and an `.m2ts`
    /// for every clip they name.
    pub(crate) fn disc(root: &Path, playlists: &[(&str, Vec<u8>)], clips: &[&str]) {
        let bdmv = root.join("BDMV");
        for dir in ["PLAYLIST", "CLIPINF", "STREAM"] {
            std::fs::create_dir_all(bdmv.join(dir)).unwrap();
        }
        for (name, data) in playlists {
            std::fs::write(bdmv.join("PLAYLIST").join(name), data).unwrap();
        }
        for clip in clips {
            std::fs::write(
                bdmv.join("CLIPINF").join(format!("{clip}.clpi")),
                b"HDMV0200",
            )
            .unwrap();
            std::fs::write(bdmv.join("STREAM").join(format!("{clip}.m2ts")), b"").unwrap();
        }
    }

    fn names(files: &[String]) -> Vec<String> {
        files
            .iter()
            .map(|f| {
                Path::new(f)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    const MINUTE: u32 = 60 * 45_000;

    #[test]
    fn the_longest_playlist_wins_and_alternative_angles_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        disc(
            dir.path(),
            &[
                ("00000.mpls", mpls(&[("00009", 0, MINUTE, &[])])),
                (
                    "00001.mpls",
                    mpls(&[
                        ("00001", 0, 30 * MINUTE, &["00011"]),
                        ("00002", 0, 30 * MINUTE, &["00012"]),
                    ]),
                ),
            ],
            &["00009", "00001", "00002", "00011", "00012"],
        );
        let files = primary_playlist_m2ts_files(dir.path()).unwrap();
        assert_eq!(names(&files), ["00001.m2ts", "00002.m2ts"]);
        assert!(files[0].starts_with(&dir.path().to_string_lossy().into_owned()));
    }

    #[test]
    fn a_looping_playlist_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        disc(
            dir.path(),
            &[
                (
                    "00000.mpls",
                    mpls(&[("00001", 0, MINUTE, &[]), ("00001", 0, MINUTE, &[])]),
                ),
                ("00001.mpls", mpls(&[("00002", 0, MINUTE, &[])])),
            ],
            &["00001", "00002"],
        );
        let files = primary_playlist_m2ts_files(dir.path()).unwrap();
        assert_eq!(names(&files), ["00002.m2ts"]);
    }

    #[test]
    fn a_path_inside_bdmv_finds_the_structure() {
        let dir = tempfile::tempdir().unwrap();
        disc(
            dir.path(),
            &[("00000.MPLS", mpls(&[("00001", 0, MINUTE, &[])]))],
            &["00001"],
        );
        let files = primary_playlist_m2ts_files(&dir.path().join("BDMV")).unwrap();
        assert_eq!(names(&files), ["00001.m2ts"]);
    }

    #[test]
    fn broken_structures_are_errors_as_bdinfo_throws() {
        let err = |dir: &Path| primary_playlist_m2ts_files(dir).unwrap_err().to_string();
        let dir = tempfile::tempdir().unwrap();
        assert!(err(dir.path()).contains("unable to locate BD structure"));

        // A play item whose clip has no `.clpi`.
        disc(
            dir.path(),
            &[("00000.mpls", mpls(&[("00001", 0, MINUTE, &[])]))],
            &[],
        );
        assert!(err(dir.path()).contains("missing file 00001.M2TS"));

        let dir = tempfile::tempdir().unwrap();
        disc(dir.path(), &[("00000.mpls", b"MPLS0200".to_vec())], &[]);
        assert!(err(dir.path()).contains("truncated"));

        let dir = tempfile::tempdir().unwrap();
        disc(dir.path(), &[("00000.mpls", b"MPLS9999".to_vec())], &[]);
        assert!(err(dir.path()).contains("unknown file type MPLS9999"));

        let dir = tempfile::tempdir().unwrap();
        disc(dir.path(), &[], &["00001"]);
        std::fs::write(dir.path().join("BDMV/CLIPINF/00001.clpi"), b"HDMV9999").unwrap();
        assert!(err(dir.path()).contains("unknown file type HDMV9999"));

        // An alternative angle with a `.clpi` but no `.m2ts`.
        let dir = tempfile::tempdir().unwrap();
        disc(
            dir.path(),
            &[("00000.mpls", mpls(&[("00001", 0, MINUTE, &["00011"])]))],
            &["00001", "00011"],
        );
        std::fs::remove_file(dir.path().join("BDMV/STREAM/00011.m2ts")).unwrap();
        assert!(err(dir.path()).contains("missing angle file 00011.M2TS"));

        // The main playlist's clip has a `.clpi` but no `.m2ts`: BDInfo only
        // notes it while scanning, and the file list then fails on it.
        let dir = tempfile::tempdir().unwrap();
        disc(
            dir.path(),
            &[("00000.mpls", mpls(&[("00001", 0, MINUTE, &[])]))],
            &["00001"],
        );
        std::fs::remove_file(dir.path().join("BDMV/STREAM/00001.m2ts")).unwrap();
        assert!(err(dir.path()).contains("references missing file 00001.M2TS"));
    }

    #[test]
    fn a_disc_whose_every_playlist_loops_has_no_files() {
        let dir = tempfile::tempdir().unwrap();
        disc(
            dir.path(),
            &[(
                "00000.mpls",
                mpls(&[("00001", 0, MINUTE, &[]), ("00001", 0, MINUTE, &[])]),
            )],
            &["00001"],
        );
        assert_eq!(
            primary_playlist_m2ts_files(dir.path()).unwrap(),
            Vec::<String>::new()
        );
    }
}
