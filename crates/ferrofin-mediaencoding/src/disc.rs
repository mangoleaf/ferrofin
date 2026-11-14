//! DVD and Blu-ray folder rips as ffmpeg input.
//!
//! Ffmpeg cannot open a `VIDEO_TS` or `BDMV` folder, so upstream never hands
//! it one: it picks the disc's primary playlist
//! (`MediaEncoder.GetPrimaryPlaylistVobFiles` / `GetPrimaryPlaylistM2tsFiles`)
//! and feeds those files to ffmpeg, as a `concat:` input for subtitle
//! extraction and frame grabs (`GetInputPathArgument`) and as a concat-demuxer
//! file for a transcode (`GenerateConcatConfig`).
//!
//! Every reader here walks the file system: the async entry points run it on
//! the blocking pool, and the sync ones ([`primary_playlist_vob_files`],
//! [`primary_playlist_m2ts_files`]) must be called there.

mod bdrom;

use std::path::{Path, PathBuf};

use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::VideoType;
use ferrofin_model::media_info::MediaProtocol;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};

pub use bdrom::primary_playlist_m2ts_files;

use crate::encoder::encoding_utils::get_input_argument_multi;
use crate::encoder::media_encoder::source_input_argument;
use crate::error::MediaEncodingError;

/// A title with a `.vob` at least this large is a main feature, not an extra
/// (`GetPrimaryPlaylistVobFiles`' "multiple big titles (> 900 MB)"). Upstream's
/// literal; whether it becomes a setting is the owner's question in gl-1761.
const BIG_TITLE_BYTES: u64 = 900 * 1024 * 1024;

/// Whether `source` is a DVD or Blu-ray folder rip, whose input is its
/// primary playlist rather than its path.
#[must_use]
pub fn is_disc_rip(source: &MediaSourceInfo) -> bool {
    matches!(source.video_type, Some(VideoType::Dvd | VideoType::BluRay))
}

/// The title of a VOB name as upstream groups them: the text after the last
/// `_` of the stem (`GetFileNameWithoutExtension(..).RightPart('_')`), so
/// `VTS_01_2.VOB` is `2`.
fn vob_title(path: &Path) -> String {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    stem.rsplit('_').next().unwrap_or_default().to_owned()
}

/// Every file under `dir`, recursively, hidden ones included, skipping what
/// cannot be read (`GetFiles(path, true)`: `IgnoreInaccessible`, no
/// attributes skipped). A symlinked folder is not entered, so a link cycle
/// cannot recurse forever.
fn files_under(dir: &Path, out: &mut Vec<(PathBuf, u64)>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(path = %dir.display(), %error, "skipping an unreadable folder of a DVD rip");
            return;
        }
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            files_under(&path, out);
        } else if let Ok(m) = std::fs::metadata(&path)
            && m.is_file()
        {
            out.push((path, m.len()));
        }
    }
}

/// The `.vob` files of a DVD rip's main feature, in path order. Port of
/// `MediaEncoder.GetPrimaryPlaylistVobFiles`.
///
/// Menus and intros are dropped (`VIDEO_TS.VOB` and every `*_0.VOB`). A
/// `title_number` picks `VTS_<nn>_*` when it has files. Otherwise the titles
/// that hold a "big" (≥ 900 MB) VOB are kept, or the first VOB's title when
/// none does. Empty when the folder holds no playable VOB (upstream throws on
/// its `allVobs[0]` then; every caller here reports the empty list instead).
#[must_use]
pub fn primary_playlist_vob_files(path: &Path, title_number: Option<u32>) -> Vec<String> {
    let mut all = Vec::new();
    files_under(path, &mut all);
    let mut vobs: Vec<(PathBuf, u64)> = all
        .into_iter()
        .filter(|(p, _)| {
            let upper = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_ascii_uppercase();
            // `Extension` is the text from the last `.`, so `.vob` alone counts.
            upper.rsplit_once('.').is_some_and(|(_, ext)| ext == "VOB")
                && upper != "VIDEO_TS.VOB"
                && !upper.ends_with("_0.VOB")
        })
        .collect();
    vobs.sort_by(|a, b| a.0.cmp(&b.0));
    let full = |v: &[(PathBuf, u64)]| -> Vec<String> {
        v.iter()
            .map(|(p, _)| p.to_string_lossy().into_owned())
            .collect()
    };

    if let Some(title) = title_number {
        let prefix = format!("VTS_{title:02}_");
        let matching: Vec<_> = vobs
            .iter()
            .filter(|(p, _)| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_ascii_uppercase()
                    .starts_with(&prefix)
            })
            .cloned()
            .collect();
        if !matching.is_empty() {
            return full(&matching);
        }
        tracing::warn!(
            title,
            path = %path.display(),
            "could not determine the .vob files of this title"
        );
    }

    let mut titles: Vec<String> = Vec::new();
    for (p, len) in &vobs {
        let title = vob_title(p);
        if *len >= BIG_TITLE_BYTES && !titles.contains(&title) {
            titles.push(title);
        }
    }
    if titles.is_empty() {
        let Some((first, _)) = vobs.first() else {
            return Vec::new();
        };
        titles.push(vob_title(first));
    }
    vobs.retain(|(p, _)| titles.contains(&vob_title(p)));
    full(&vobs)
}

/// The primary playlist's files of a disc-rip source read from `path`, or
/// `None` when the source is not a DVD or Blu-ray folder. Reads the disc off
/// the async runtime: a rip on network storage is slow to list.
///
/// # Errors
///
/// A Blu-ray structure the playlist reader cannot use
/// ([`primary_playlist_m2ts_files`]).
pub async fn playlist_files(
    source: &MediaSourceInfo,
    path: &str,
) -> Result<Option<Vec<String>>, MediaEncodingError> {
    let video_type = source.video_type;
    let path = PathBuf::from(path);
    let read = match video_type {
        Some(VideoType::Dvd) => {
            tokio::task::spawn_blocking(move || Ok(primary_playlist_vob_files(&path, None))).await
        }
        Some(VideoType::BluRay) => {
            tokio::task::spawn_blocking(move || primary_playlist_m2ts_files(&path)).await
        }
        _ => return Ok(None),
    };
    read.map_err(|e| MediaEncodingError::process(e.to_string()))?
        .map(Some)
}

/// The ffmpeg `-i` value for `path` of `source`: a disc rip's primary
/// playlist as a `concat:"a|b"` input, anything else as
/// [`source_input_argument`] gives it (the ISO `bluray:` prefix, the source's
/// protocol). Port of `MediaEncoder.GetInputPathArgument`.
///
/// # Errors
///
/// A disc rip whose playlist cannot be read, or that has no playable files.
pub async fn input_path_argument(
    path: &str,
    source: &MediaSourceInfo,
) -> Result<String, MediaEncodingError> {
    match playlist_files(source, path).await? {
        None => Ok(source_input_argument(path, source)),
        Some(files) if files.is_empty() => Err(no_playable_files(path)),
        // `GetInputArgument(IReadOnlyList<string>, MediaSourceInfo)`.
        Some(files) => Ok(get_input_argument_multi("file", &files, source.protocol)),
    }
}

fn no_playable_files(path: &str) -> MediaEncodingError {
    MediaEncodingError::process(format!("no playable files in the disc rip {path}"))
}

/// Where a disc-rip source's concat file lives:
/// `{cache}/concat/{media source id}.concat`.
#[must_use]
pub fn concat_file_path(cache_path: &str, source: &MediaSourceInfo) -> PathBuf {
    Path::new(cache_path).join("concat").join(format!(
        "{}.concat",
        source.id.as_deref().unwrap_or_default()
    ))
}

/// The ffmpeg concat-demuxer script for `files`: a `file` stanza (single
/// quotes escaped as `'\''`) and a `duration` stanza in seconds per file.
fn concat_config(files: &[(String, f64)]) -> String {
    use std::fmt::Write as _;
    files
        .iter()
        .fold(String::new(), |mut out, (path, seconds)| {
            let _ = writeln!(out, "file '{}'", path.replace('\'', r"'\''"));
            let _ = writeln!(out, "duration {seconds}");
            out
        })
}

/// Writes the concat file of a disc-rip `source` to `concat_path` unless it
/// already exists: the `File.Exists` check of `EncodingHelper.GetInputArgument`
/// and `MediaEncoder.GenerateConcatConfig`, which probes each playlist file
/// for its run time. A written file is kept, as upstream keeps it: a rip
/// replaced in the same folder plays the old list until the cache clean-up
/// removes the file, or someone deletes `{cache}/concat/{id}.concat`.
///
/// A playlist file whose path holds a line break is refused: the concat
/// demuxer reads one directive per line, so it would add directives of its
/// own (upstream writes it as is).
///
/// # Errors
///
/// A playlist that cannot be read or is empty, a path with a line break, a
/// file that does not probe or has no run time, and a failed write.
pub async fn ensure_concat_config(
    encoder: &dyn MediaEncoder,
    source: &MediaSourceInfo,
    concat_path: &Path,
) -> Result<(), ServiceError> {
    if tokio::fs::try_exists(concat_path).await.unwrap_or(false) {
        return Ok(());
    }
    let path = source.path.clone().unwrap_or_default();
    let files = playlist_files(source, &path).await?.unwrap_or_default();
    if files.is_empty() {
        return Err(no_playable_files(&path).into());
    }
    if let Some(file) = files.iter().find(|f| f.contains(['\n', '\r'])) {
        return Err(MediaEncodingError::process(format!(
            "the disc rip file {file:?} has a line break in its path, which a concat file cannot hold"
        ))
        .into());
    }
    let mut entries = Vec::with_capacity(files.len());
    for file in files {
        let request = MediaInfoRequest {
            media_source: MediaSourceInfo {
                path: Some(file.clone()),
                protocol: MediaProtocol::File,
                video_type: source.video_type,
                ..MediaSourceInfo::default()
            },
            extract_chapters: false,
            media_is_audio: false,
        };
        let ticks = encoder
            .get_media_info(&request)
            .await?
            .run_time_ticks
            .ok_or_else(|| ServiceError::backend(format!("{file} has no run time")))?;
        #[allow(clippy::cast_precision_loss)] // `TimeSpan.FromTicks(..).TotalSeconds`
        entries.push((file, ticks as f64 / 10_000_000.0));
    }
    let parent = concat_path.parent().unwrap_or_else(|| Path::new("."));
    tokio::fs::create_dir_all(parent).await.map_err(|e| {
        MediaEncodingError::io(
            format!("cannot create the concat folder {}", parent.display()),
            e,
        )
    })?;
    // Written whole under a name of its own, then renamed: an interrupted
    // write never leaves a truncated file that the existence check would
    // keep, and two first plays of one rip never write into each other.
    let partial = concat_path.with_extension(format!("{}.partial", uuid::Uuid::new_v4().simple()));
    let written = match tokio::fs::write(&partial, concat_config(&entries)).await {
        Ok(()) => tokio::fs::rename(&partial, concat_path).await,
        Err(error) => Err(error),
    };
    if let Err(error) = written {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(MediaEncodingError::io(
            format!("cannot write the concat file {}", concat_path.display()),
            error,
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, len: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let f = std::fs::File::create(path).unwrap();
        f.set_len(len).unwrap();
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

    #[test]
    fn menus_go_and_the_big_titles_stay() {
        let dir = tempfile::tempdir().unwrap();
        let ts = dir.path().join("VIDEO_TS");
        write(&ts.join("VIDEO_TS.VOB"), 1);
        write(&ts.join("VTS_01_0.VOB"), 1);
        write(&ts.join("VTS_01_1.VOB"), 10);
        write(&ts.join("VTS_02_1.VOB"), BIG_TITLE_BYTES);
        write(&ts.join("VTS_02_2.VOB"), 5);
        write(&ts.join("VTS_03_1.VOB"), BIG_TITLE_BYTES + 1);
        // Upstream's title is the text after the LAST `_`, so `VTS_02_1` and
        // `VTS_03_1` are both title `1`, and every `_1` file joins it.
        assert_eq!(
            names(&primary_playlist_vob_files(dir.path(), None)),
            ["VTS_01_1.VOB", "VTS_02_1.VOB", "VTS_03_1.VOB"]
        );
        assert_eq!(
            names(&primary_playlist_vob_files(dir.path(), Some(2))),
            ["VTS_02_1.VOB", "VTS_02_2.VOB"]
        );
    }

    #[test]
    fn without_a_big_title_the_first_title_is_used() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("VIDEO_TS/VTS_01_1.vob"), 1);
        write(&dir.path().join("VIDEO_TS/VTS_01_2.VOB"), 1);
        write(&dir.path().join("VIDEO_TS/VTS_02_3.VOB"), 1);
        assert_eq!(
            names(&primary_playlist_vob_files(dir.path(), Some(9))),
            ["VTS_01_1.vob"]
        );
        assert_eq!(
            primary_playlist_vob_files(&dir.path().join("none"), None),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_concat_config_quotes_paths_and_lists_durations() {
        let config = concat_config(&[
            ("/m/it's/00001.m2ts".to_owned(), 1.5),
            ("/m/00002.m2ts".to_owned(), 60.0),
        ]);
        assert_eq!(
            config,
            "file '/m/it'\\''s/00001.m2ts'\nduration 1.5\nfile '/m/00002.m2ts'\nduration 60\n"
        );
    }

    #[test]
    fn the_concat_file_is_keyed_by_the_media_source_id() {
        let source = MediaSourceInfo {
            id: Some("abc".to_owned()),
            ..MediaSourceInfo::default()
        };
        assert_eq!(
            concat_file_path("/cache", &source),
            Path::new("/cache/concat/abc.concat")
        );
    }

    /// A [`MediaEncoder`] whose probe answers each file's run time from
    /// `seconds` (by file name; a missing name has none), counting probes.
    struct FakeProbe {
        seconds: std::collections::HashMap<&'static str, i64>,
        probes: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl MediaEncoder for FakeProbe {
        fn encoder_path(&self) -> String {
            "ffmpeg".to_owned()
        }
        fn probe_path(&self) -> String {
            "ffprobe".to_owned()
        }
        async fn set_ffmpeg_path(&self) -> Result<bool, ServiceError> {
            Ok(true)
        }
        async fn get_media_info(
            &self,
            request: &MediaInfoRequest,
        ) -> Result<MediaSourceInfo, ServiceError> {
            self.probes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let path = request.media_source.path.clone().unwrap_or_default();
            let name = Path::new(&path).file_name().unwrap().to_string_lossy();
            Ok(MediaSourceInfo {
                run_time_ticks: self.seconds.get(&*name).map(|s| s * 10_000_000),
                ..MediaSourceInfo::default()
            })
        }
        async fn extract_audio_image(
            &self,
            _path: &str,
            _image_stream_index: Option<i32>,
        ) -> Result<String, ServiceError> {
            unreachable!()
        }
        async fn extract_video_image(
            &self,
            _input_file: &str,
            _container: &str,
            _media_source: &MediaSourceInfo,
            _video_stream: &ferrofin_model::entities_media::MediaStream,
            _threed_format: Option<ferrofin_model::entities::Video3DFormat>,
            _offset_ticks: Option<i64>,
        ) -> Result<String, ServiceError> {
            unreachable!()
        }
        fn get_input_argument(&self, input_file: &str, media_source: &MediaSourceInfo) -> String {
            source_input_argument(input_file, media_source)
        }
        fn get_time_parameter(&self, _ticks: i64) -> String {
            unreachable!()
        }
        async fn convert_image(&self, _input: &str, _output: &str) -> Result<(), ServiceError> {
            unreachable!()
        }
    }

    fn probe(seconds: &[(&'static str, i64)]) -> FakeProbe {
        FakeProbe {
            seconds: seconds.iter().copied().collect(),
            probes: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn rip_source(path: &Path, video_type: VideoType) -> MediaSourceInfo {
        MediaSourceInfo {
            id: Some("rip".to_owned()),
            path: Some(path.to_string_lossy().into_owned()),
            video_type: Some(video_type),
            ..MediaSourceInfo::default()
        }
    }

    /// Every file of the playlist, in its order, with its own probed run
    /// time; nothing but the finished file is left in the folder.
    #[tokio::test]
    async fn the_concat_file_lists_each_playlist_file_with_its_run_time() {
        let dir = tempfile::tempdir().unwrap();
        let minute = 60 * 45_000;
        bdrom::tests::disc(
            dir.path(),
            &[(
                "00000.mpls",
                bdrom::tests::mpls(&[("00002", 0, minute, &[]), ("00001", 0, minute, &[])]),
            )],
            &["00001", "00002"],
        );
        let cache = tempfile::tempdir().unwrap();
        let concat = cache.path().join("concat/rip.concat");
        let source = rip_source(dir.path(), VideoType::BluRay);
        let encoder = probe(&[("00001.m2ts", 3), ("00002.m2ts", 90)]);

        ensure_concat_config(&encoder, &source, &concat)
            .await
            .unwrap();
        ensure_concat_config(&encoder, &source, &concat)
            .await
            .unwrap();

        let stream = |name: &str| dir.path().join("BDMV/STREAM").join(name);
        assert_eq!(
            std::fs::read_to_string(&concat).unwrap(),
            format!(
                "file '{}'\nduration 90\nfile '{}'\nduration 3\n",
                stream("00002.m2ts").display(),
                stream("00001.m2ts").display()
            )
        );
        assert_eq!(
            encoder.probes.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "probed once per file, and only for the first write"
        );
        assert_eq!(
            std::fs::read_dir(concat.parent().unwrap()).unwrap().count(),
            1
        );
    }

    /// The refusals write nothing: a path with a line break (it would add
    /// concat directives), a file with no run time, and an unwritable cache.
    #[tokio::test]
    async fn a_concat_file_that_cannot_be_right_is_not_written() {
        let cache = tempfile::tempdir().unwrap();
        let concat = cache.path().join("concat/rip.concat");

        let rip = tempfile::tempdir().unwrap();
        let broken = rip.path().join("a\nfile '/etc/passwd'");
        write(&broken.join("VIDEO_TS/VTS_01_1.VOB"), 1);
        let encoder = probe(&[("VTS_01_1.VOB", 60)]);
        let err = ensure_concat_config(&encoder, &rip_source(&broken, VideoType::Dvd), &concat)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("line break"), "{err}");
        assert_eq!(encoder.probes.load(std::sync::atomic::Ordering::SeqCst), 0);

        let dvd = rip.path().join("dvd");
        write(&dvd.join("VIDEO_TS/VTS_01_1.VOB"), 1);
        let err = ensure_concat_config(&probe(&[]), &rip_source(&dvd, VideoType::Dvd), &concat)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("has no run time"), "{err}");
        assert!(!cache.path().join("concat").exists());

        let blocked = cache.path().join("blocked");
        std::fs::write(&blocked, b"").unwrap();
        let err = ensure_concat_config(
            &encoder,
            &rip_source(&dvd, VideoType::Dvd),
            &blocked.join("rip.concat"),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("cannot create the concat folder"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_disc_rip_input_is_its_playlist_and_anything_else_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let minute = 60 * 45_000;
        bdrom::tests::disc(
            dir.path(),
            &[(
                "00000.mpls",
                bdrom::tests::mpls(&[("00001", 0, minute, &[]), ("00002", 0, minute, &[])]),
            )],
            &["00001", "00002"],
        );
        let path = dir.path().to_string_lossy().into_owned();
        let blu_ray = MediaSourceInfo {
            video_type: Some(VideoType::BluRay),
            ..MediaSourceInfo::default()
        };
        let stream = |name: &str| dir.path().join("BDMV/STREAM").join(name);
        assert_eq!(
            input_path_argument(&path, &blu_ray).await.unwrap(),
            format!(
                "concat:\"{}|{}\"",
                stream("00001.m2ts").display(),
                stream("00002.m2ts").display()
            )
        );

        let dvd = MediaSourceInfo {
            video_type: Some(VideoType::Dvd),
            ..MediaSourceInfo::default()
        };
        let err = input_path_argument(&path, &dvd).await.unwrap_err();
        assert!(err.to_string().contains("no playable files"), "{err}");

        let iso = MediaSourceInfo {
            iso_type: Some(ferrofin_model::entities::IsoType::BluRay),
            ..MediaSourceInfo::default()
        };
        assert_eq!(
            input_path_argument("/m/disc.iso", &iso).await.unwrap(),
            "bluray:\"/m/disc.iso\""
        );
        let remote = MediaSourceInfo {
            protocol: MediaProtocol::Http,
            ..MediaSourceInfo::default()
        };
        assert_eq!(
            input_path_argument("http://h/x.ts", &remote).await.unwrap(),
            "\"http://h/x.ts\""
        );
    }
}
