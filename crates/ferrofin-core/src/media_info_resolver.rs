//! Scan-time discovery of a video's **external** media streams — the sidecar
//! subtitle (`Movie.eng.forced.srt`) and audio (`Movie.commentary.mka`) files
//! that sit next to it. Port of `MediaBrowser.Providers/MediaInfo/MediaInfoResolver.cs`
//! and its two concrete flavours, `SubtitleResolver` and `AudioResolver`.
//!
//! The pipeline is upstream's exactly: list the files in the video's folder
//! (and its internal metadata folder, where downloaded/uploaded sidecars land
//! when the library is read-only), keep the ones whose stem is the video's stem
//! followed by a media-flag delimiter, parse the remainder with the naming
//! crate's [`ExternalPathParser`] (language / title / `default` / `forced` /
//! hearing-impaired tokens), then **ffprobe each file** for its real stream(s)
//! and merge the filename facts onto the probed stream. The resulting
//! [`MediaStream`]s carry `IsExternal = true` and `Path`, which is all the
//! playback side needs to serve them (`/Videos/{id}/{source}/Subtitles/…`).

use std::sync::Arc;

use ferrofin_model::dlna::DlnaProfileType;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaStream;
use ferrofin_model::io::FileSystemEntryType;
use ferrofin_model::media_info::MediaProtocol;
use ferrofin_naming::common::NamingOptions;
use ferrofin_naming::external_files::{ExternalPathParser, ExternalPathParserResult};
use ferrofin_naming::path as naming_path;
use ferrofin_traits::filesystem::FileSystem;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};

use crate::localization_manager::LocalizationManager;

/// The video an external-stream search is run for: the three path facts
/// upstream reads off the `Video` entity (`Path`, `ContainingFolderPath`,
/// `GetInternalMetadataPath()`), decoupled from any domain object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalMediaTarget {
    /// The video's own path (a file, or the folder of a disc rip).
    pub path: String,
    /// The folder whose files are candidates: the video file's parent, or the
    /// rip folder itself (`Video.ContainingFolderPath`).
    pub containing_folder: String,
    /// The item's internal metadata folder (`{metadata}/library/{id2}/{id}`),
    /// also searched — subtitles uploaded against a read-only library are
    /// written there. `None` when the caller has no metadata root.
    pub internal_metadata_path: Option<String>,
}

impl ExternalMediaTarget {
    /// A target for a plain video **file**: the containing folder is the
    /// file's directory (`Path.GetDirectoryName`).
    #[must_use]
    pub fn for_file(path: impl Into<String>, internal_metadata_path: Option<String>) -> Self {
        let path = path.into();
        let containing_folder = naming_path::directory_name(&path)
            .unwrap_or_default()
            .to_owned();
        Self {
            path,
            containing_folder,
            internal_metadata_path,
        }
    }

    /// The video's file name without extension — the prefix every external
    /// file's name must start with (`Video.FileNameWithoutExtension`).
    #[must_use]
    pub fn file_name_without_extension(&self) -> &str {
        naming_path::file_name_without_extension(&self.path)
    }
}

/// Whether a path is a local file rather than a network stream — the port of
/// `MediaSourceManager.GetPathProtocol(path) == MediaProtocol.File`, which
/// `BaseItem.IsFileProtocol` reads. External files are only ever looked for
/// next to a local file.
#[must_use]
pub fn is_file_protocol(path: &str) -> bool {
    const STREAM_SCHEMES: [&str; 6] = ["rtsp", "rtmp", "http", "rtp", "ftp", "udp"];
    let lower = path.to_ascii_lowercase();
    if STREAM_SCHEMES
        .iter()
        .any(|scheme| lower.starts_with(scheme))
    {
        return false;
    }
    // `IFileSystem.IsPathFile`: a scheme other than `file://` is not a file.
    !lower.contains("://") || lower.starts_with("file://")
}

/// Resolves external files of one [`DlnaProfileType`] (subtitle or audio)
/// for a video. Port of the abstract `MediaInfoResolver`; the
/// [`subtitle`](Self::subtitle) and [`audio`](Self::audio) constructors are
/// the `SubtitleResolver` / `AudioResolver` subclasses.
pub struct MediaInfoResolver {
    naming_options: Arc<NamingOptions>,
    localization: Arc<LocalizationManager>,
    media_encoder: Arc<dyn MediaEncoder>,
    file_system: Arc<dyn FileSystem>,
    profile_type: DlnaProfileType,
}

impl std::fmt::Debug for MediaInfoResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaInfoResolver")
            .field("profile_type", &self.profile_type)
            .finish_non_exhaustive()
    }
}

impl MediaInfoResolver {
    /// A resolver for files of `profile_type` (`Subtitle` or `Audio`; any
    /// other type matches no extension and resolves nothing).
    #[must_use]
    pub fn new(
        naming_options: Arc<NamingOptions>,
        localization: Arc<LocalizationManager>,
        media_encoder: Arc<dyn MediaEncoder>,
        file_system: Arc<dyn FileSystem>,
        profile_type: DlnaProfileType,
    ) -> Self {
        Self {
            naming_options,
            localization,
            media_encoder,
            file_system,
            profile_type,
        }
    }

    /// The external **subtitle** resolver (`SubtitleResolver`).
    #[must_use]
    pub fn subtitle(
        naming_options: Arc<NamingOptions>,
        localization: Arc<LocalizationManager>,
        media_encoder: Arc<dyn MediaEncoder>,
        file_system: Arc<dyn FileSystem>,
    ) -> Self {
        Self::new(
            naming_options,
            localization,
            media_encoder,
            file_system,
            DlnaProfileType::Subtitle,
        )
    }

    /// The external **audio**-track resolver (`AudioResolver`).
    #[must_use]
    pub fn audio(
        naming_options: Arc<NamingOptions>,
        localization: Arc<LocalizationManager>,
        media_encoder: Arc<dyn MediaEncoder>,
        file_system: Arc<dyn FileSystem>,
    ) -> Self {
        Self::new(
            naming_options,
            localization,
            media_encoder,
            file_system,
            DlnaProfileType::Audio,
        )
    }

    /// The stream type this resolver's files must probe as.
    fn wanted_stream_type(&self) -> Option<MediaStreamType> {
        match self.profile_type {
            DlnaProfileType::Audio => Some(MediaStreamType::Audio),
            DlnaProfileType::Subtitle => Some(MediaStreamType::Subtitle),
            _ => None,
        }
    }

    /// Finds and probes the external streams for `video`, numbering them from
    /// `start_index`. Port of `GetExternalStreamsAsync(video, startIndex, …)`.
    ///
    /// Each matched file is ffprobed. A file that yields exactly one stream of
    /// the wanted type takes the filename's `default`/`forced`/hearing-impaired
    /// flags (forced and hearing-impaired OR-ed with the probe's); a container
    /// with several streams keeps each stream's own flags. Title and language
    /// come from the probe when it has them, else from the filename. A probe
    /// failure logs and skips that file — one bad sidecar never loses the
    /// others. `.strm` sidecars are never probed.
    pub async fn get_external_streams(
        &self,
        video: &ExternalMediaTarget,
        mut start_index: i32,
    ) -> Vec<MediaStream> {
        if !is_file_protocol(&video.path) {
            return Vec::new();
        }
        let path_infos = self.get_external_files(video);
        if path_infos.is_empty() {
            return Vec::new();
        }
        let Some(wanted) = self.wanted_stream_type() else {
            return Vec::new();
        };

        let mut media_streams = Vec::new();
        for path_info in path_infos {
            if naming_path::extension(&path_info.path).eq_ignore_ascii_case(".strm") {
                continue;
            }
            let probed = match self.get_media_info(&path_info.path).await {
                Ok(probed) => probed,
                Err(err) => {
                    tracing::error!(
                        error = %err,
                        path = %path_info.path,
                        "error getting external streams"
                    );
                    continue;
                }
            };
            let mut streams = probed.media_streams;
            if streams.len() == 1 {
                let mut stream = streams.remove(0);
                if stream.stream_type == wanted {
                    stream.index = start_index;
                    start_index += 1;
                    stream.is_default = path_info.is_default;
                    stream.is_forced = path_info.is_forced || stream.is_forced;
                    stream.is_hearing_impaired =
                        path_info.is_hearing_impaired || stream.is_hearing_impaired;
                    media_streams.push(merge_metadata(stream, &path_info));
                }
            } else {
                for mut stream in streams {
                    if stream.stream_type == wanted {
                        stream.index = start_index;
                        start_index += 1;
                        media_streams.push(merge_metadata(stream, &path_info));
                    }
                }
            }
        }
        media_streams
    }

    /// The external files that belong to `video`, parsed: every file in the
    /// containing folder (and the internal metadata folder, when it exists)
    /// whose name is the video's stem, optionally followed by a media-flag
    /// delimiter and more, with an extension this resolver handles. Port of
    /// `GetExternalFiles(Video, …)`.
    #[must_use]
    pub fn get_external_files(&self, video: &ExternalMediaTarget) -> Vec<ExternalPathParserResult> {
        if !is_file_protocol(&video.path) {
            return Vec::new();
        }
        // Check if video folder exists
        let folder = video.containing_folder.as_str();
        if !self.file_system.directory_exists(folder) {
            return Vec::new();
        }

        let mut files = self.file_paths(folder);
        files.retain(|file| file != &video.path);
        if let Some(internal) = video
            .internal_metadata_path
            .as_deref()
            .filter(|p| self.file_system.directory_exists(p))
        {
            files.extend(self.file_paths(internal));
        }
        if files.is_empty() {
            return Vec::new();
        }

        let parser = ExternalPathParser::new(
            &self.naming_options,
            self.localization.as_ref(),
            self.profile_type,
        );
        let prefix = video.file_name_without_extension();
        let mut external_path_infos = Vec::new();
        for file in &files {
            let file_name_without_extension = naming_path::file_name_without_extension(file);
            let Some(rest) = strip_prefix_ignore_case(file_name_without_extension, prefix) else {
                continue;
            };
            let delimited = rest
                .chars()
                .next()
                .is_none_or(|first| self.naming_options.media_flag_delimiters.contains(&first));
            if !delimited {
                continue;
            }
            if let Some(info) = parser.parse_file(file, Some(rest)) {
                external_path_infos.push(info);
            }
        }
        external_path_infos
    }

    /// The paths of the plain files directly inside `folder`
    /// (`IDirectoryService.GetFilePaths`, non-recursive).
    fn file_paths(&self, folder: &str) -> Vec<String> {
        self.file_system
            .get_file_system_entries(folder)
            .into_iter()
            .filter(|entry| entry.type_ == FileSystemEntryType::File)
            .map(|entry| entry.path)
            .collect()
    }

    /// Probes one external file as this resolver's media type. Port of the
    /// private `GetMediaInfo(path, type, …)`.
    async fn get_media_info(
        &self,
        path: &str,
    ) -> Result<MediaSourceInfo, ferrofin_traits::error::ServiceError> {
        let request = MediaInfoRequest {
            media_source: MediaSourceInfo {
                path: Some(path.to_owned()),
                protocol: MediaProtocol::File,
                ..MediaSourceInfo::default()
            },
            extract_chapters: false,
            media_is_audio: self.profile_type == DlnaProfileType::Audio,
        };
        self.media_encoder.get_media_info(&request).await
    }
}

/// Merges the filename's facts onto a probed stream: the stream becomes
/// external at that path, and its title/language fall back to the filename's
/// when the probe left them empty. Port of `MergeMetadata`.
fn merge_metadata(mut stream: MediaStream, path_info: &ExternalPathParserResult) -> MediaStream {
    stream.path = Some(path_info.path.clone());
    stream.is_external = true;
    stream.title = non_empty(stream.title).or_else(|| non_empty(path_info.title.clone()));
    stream.language = non_empty(stream.language).or_else(|| non_empty(path_info.language.clone()));
    stream
}

/// `Some` only for a non-empty string (`string.IsNullOrEmpty` → `null`).
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

/// The remainder of `name` after `prefix`, compared char-by-char ignoring case
/// (`OrdinalIgnoreCase` on the prefix slice), or `None` when `name` does not
/// start with `prefix`.
fn strip_prefix_ignore_case<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    let mut name_chars = name.char_indices();
    for expected in prefix.chars() {
        let (_, actual) = name_chars.next()?;
        if !actual.to_lowercase().eq(expected.to_lowercase()) {
            return None;
        }
    }
    let consumed = name_chars.next().map_or(name.len(), |(i, _)| i);
    Some(&name[consumed..])
}

/// The resolver pair the scan runs for every video: subtitles, then audio.
///
/// Upstream `FFProbeVideoInfo.Fetch` adds the external subtitle streams first,
/// then the external audio streams, and only then the embedded streams — so
/// the external rows keep their indices when a remote video's embedded set
/// changes. [`external_streams`](Self::external_streams) reproduces that
/// order and numbering.
pub struct ExternalStreamResolvers {
    subtitles: MediaInfoResolver,
    audio: MediaInfoResolver,
}

impl std::fmt::Debug for ExternalStreamResolvers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalStreamResolvers")
            .finish_non_exhaustive()
    }
}

impl ExternalStreamResolvers {
    /// Builds both resolvers over shared naming options, localization, the
    /// ffprobe seam and the filesystem.
    #[must_use]
    pub fn new(
        naming_options: Arc<NamingOptions>,
        localization: Arc<LocalizationManager>,
        media_encoder: Arc<dyn MediaEncoder>,
        file_system: Arc<dyn FileSystem>,
    ) -> Self {
        Self {
            subtitles: MediaInfoResolver::subtitle(
                Arc::clone(&naming_options),
                Arc::clone(&localization),
                Arc::clone(&media_encoder),
                Arc::clone(&file_system),
            ),
            audio: MediaInfoResolver::audio(
                naming_options,
                localization,
                media_encoder,
                file_system,
            ),
        }
    }

    /// The external subtitle resolver.
    #[must_use]
    pub fn subtitles(&self) -> &MediaInfoResolver {
        &self.subtitles
    }

    /// The external audio resolver.
    #[must_use]
    pub fn audio(&self) -> &MediaInfoResolver {
        &self.audio
    }

    /// The search target for a video at `path`: its containing folder is the
    /// file's directory, or the folder itself for a disc rip (see
    /// [`containing_folder_for`]).
    #[must_use]
    pub fn target_for(
        &self,
        path: &str,
        internal_metadata_path: Option<String>,
    ) -> ExternalMediaTarget {
        ExternalMediaTarget {
            path: path.to_owned(),
            containing_folder: containing_folder_for(path, self.subtitles.file_system.as_ref()),
            internal_metadata_path,
        }
    }

    /// Every external stream of `video` — subtitles numbered from
    /// `start_index`, then audio continuing after them — in the order
    /// `FFProbeVideoInfo.Fetch` adds them.
    pub async fn external_streams(
        &self,
        video: &ExternalMediaTarget,
        start_index: i32,
    ) -> Vec<MediaStream> {
        let mut streams = self
            .subtitles
            .get_external_streams(video, start_index)
            .await;
        let next = streams
            .iter()
            .map(|s| s.index)
            .max()
            .map_or(start_index, |max| max + 1);
        streams.extend(self.audio.get_external_streams(video, next).await);
        streams
    }

    /// `external` followed by `embedded`, every stream renumbered from 0 in
    /// that order — the stream set `Fetch` saves for a video. The embedded
    /// streams' own indices are discarded: they were ffprobe's positions in
    /// the file, and the saved index must be unique across the whole set.
    #[must_use]
    pub fn merge_with_embedded(
        external: Vec<MediaStream>,
        embedded: Vec<MediaStream>,
    ) -> Vec<MediaStream> {
        let mut all = external;
        all.extend(embedded);
        for (index, stream) in all.iter_mut().enumerate() {
            stream.index = i32::try_from(index).unwrap_or(i32::MAX);
        }
        all
    }
}

/// Whether `path` names a directory on disk — a disc rip's containing folder
/// is the rip folder itself (`Video.ContainingFolderPath` for BluRay/DVD).
#[must_use]
pub fn containing_folder_for(path: &str, file_system: &dyn FileSystem) -> String {
    if file_system.directory_exists(path) {
        path.to_owned()
    } else {
        naming_path::directory_name(path)
            .unwrap_or_default()
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    //! Transliteration of `Jellyfin.Providers.Tests/MediaInfo/MediaInfoResolverTests.cs`,
    //! `SubtitleResolverTests.cs` and `AudioResolverTests.cs`. The C# mocks the
    //! directory service, filesystem and encoder; the fakes below play the same
    //! roles. The language lookup is the real culture table (the C# mock only
    //! answers `en.*` → English, which the real table also does).

    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use ferrofin_model::entities::{MediaStreamType, Video3DFormat};
    use ferrofin_model::io::{FileSystemEntryInfo, FileSystemEntryType};
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::filesystem::FileMetadata;
    use rstest::rstest;

    use super::*;

    const VIDEO_DIRECTORY_PATH: &str = "Test Data/Video";
    const METADATA_DIRECTORY_PATH: &str = "library/00/00000000000000000000000000000000";

    /// The C# `IDirectoryService` + `IFileSystem` mocks in one: which
    /// directories exist, what each lists, and which were queried.
    struct FakeFs {
        existing: Vec<String>,
        listings: Vec<(String, Vec<String>)>,
        queried: Mutex<Vec<String>>,
    }

    impl FakeFs {
        fn new(existing: &[&str], listings: Vec<(&str, Vec<String>)>) -> Arc<Self> {
            Arc::new(Self {
                existing: existing.iter().map(|s| (*s).to_owned()).collect(),
                listings: listings
                    .into_iter()
                    .map(|(dir, files)| (dir.to_owned(), files))
                    .collect(),
                queried: Mutex::new(Vec::new()),
            })
        }

        /// `GetDirectoryServiceForExternalFile`: the video and metadata
        /// directories both exist; `file` sits in one of them.
        fn for_external_file(file: &str, use_metadata_directory: bool) -> Arc<Self> {
            let (video_files, metadata_files) = if use_metadata_directory {
                (
                    Vec::new(),
                    vec![format!("{METADATA_DIRECTORY_PATH}/{file}")],
                )
            } else {
                (vec![format!("{VIDEO_DIRECTORY_PATH}/{file}")], Vec::new())
            };
            Self::new(
                &[VIDEO_DIRECTORY_PATH, METADATA_DIRECTORY_PATH],
                vec![
                    (VIDEO_DIRECTORY_PATH, video_files),
                    (METADATA_DIRECTORY_PATH, metadata_files),
                ],
            )
        }

        fn queried(&self) -> Vec<String> {
            self.queried.lock().map(|q| q.clone()).unwrap_or_default()
        }
    }

    impl FileSystem for FakeFs {
        fn get_file_system_entries(&self, path: &str) -> Vec<FileSystemEntryInfo> {
            if let Ok(mut queried) = self.queried.lock() {
                queried.push(path.to_owned());
            }
            self.listings
                .iter()
                .find(|(dir, _)| dir == path)
                .map(|(_, files)| {
                    files
                        .iter()
                        .map(|file| FileSystemEntryInfo {
                            name: naming_path::file_name(file).to_owned(),
                            path: file.clone(),
                            type_: FileSystemEntryType::File,
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        fn get_drives(&self) -> Vec<FileSystemEntryInfo> {
            Vec::new()
        }
        fn file_exists(&self, _path: &str) -> bool {
            false
        }
        fn directory_exists(&self, path: &str) -> bool {
            self.existing.iter().any(|d| d == path)
        }
        fn validate_writable(&self, _path: &str) -> Result<(), ServiceError> {
            Ok(())
        }
        fn get_files(&self, _path: &str, _extensions: &[&str]) -> Vec<FileMetadata> {
            Vec::new()
        }
        fn read_file(&self, _path: &str) -> Result<Vec<u8>, ServiceError> {
            Err(ServiceError::not_found("fake"))
        }
    }

    /// The `IMediaEncoder` mock: every probe answers with the same streams.
    struct FakeEncoder {
        streams: Vec<MediaStream>,
    }

    #[async_trait]
    impl MediaEncoder for FakeEncoder {
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
            _request: &MediaInfoRequest,
        ) -> Result<MediaSourceInfo, ServiceError> {
            Ok(MediaSourceInfo {
                media_streams: self.streams.clone(),
                ..MediaSourceInfo::default()
            })
        }
        async fn extract_audio_image(
            &self,
            _path: &str,
            _image_stream_index: Option<i32>,
        ) -> Result<String, ServiceError> {
            unreachable!("not probed here")
        }
        async fn extract_video_image(
            &self,
            _input_file: &str,
            _container: &str,
            _media_source: &MediaSourceInfo,
            _video_stream: &MediaStream,
            _threed_format: Option<Video3DFormat>,
            _offset_ticks: Option<i64>,
        ) -> Result<String, ServiceError> {
            unreachable!("not probed here")
        }
        fn get_input_argument(&self, input_file: &str, _media_source: &MediaSourceInfo) -> String {
            input_file.to_owned()
        }
        fn get_time_parameter(&self, _ticks: i64) -> String {
            String::new()
        }
        async fn convert_image(&self, _i: &str, _o: &str) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    fn localization() -> Arc<LocalizationManager> {
        Arc::new(LocalizationManager::new("US"))
    }

    fn resolver(
        profile_type: DlnaProfileType,
        streams: Vec<MediaStream>,
        fs: Arc<FakeFs>,
    ) -> MediaInfoResolver {
        MediaInfoResolver::new(
            Arc::new(NamingOptions::new()),
            localization(),
            Arc::new(FakeEncoder { streams }),
            fs,
            profile_type,
        )
    }

    /// A video at `{VIDEO_DIRECTORY_PATH}/{movie}` whose metadata folder is
    /// the test's metadata directory.
    fn video(movie: &str) -> ExternalMediaTarget {
        ExternalMediaTarget::for_file(
            format!("{VIDEO_DIRECTORY_PATH}/{movie}"),
            Some(METADATA_DIRECTORY_PATH.to_owned()),
        )
    }

    /// `CreateMediaStream(path, language, title, index, isForced, isDefault, isHearingImpaired)`.
    fn create_media_stream(
        path: &str,
        language: Option<&str>,
        title: Option<&str>,
        index: i32,
        is_forced: bool,
        is_default: bool,
        is_hearing_impaired: bool,
    ) -> MediaStream {
        MediaStream {
            index,
            stream_type: MediaStreamType::Subtitle,
            path: Some(path.to_owned()),
            is_default,
            is_forced,
            is_hearing_impaired,
            language: language.map(str::to_owned),
            title: title.map(str::to_owned),
            ..MediaStream::default()
        }
    }

    // GetExternalFiles_BadProtocol_ReturnsNoSubtitles
    #[test]
    fn get_external_files_bad_protocol_returns_no_subtitles() {
        let fs = FakeFs::for_external_file("My.Video.srt", false);
        let resolver = resolver(DlnaProfileType::Subtitle, vec![MediaStream::default()], fs);
        let video = ExternalMediaTarget::for_file("https://url.com/My.Video.mkv", None);
        assert!(resolver.get_external_files(&video).is_empty());
    }

    // GetExternalFiles_MissingDirectory_DirectoryNotQueried
    #[rstest]
    #[case(false)]
    #[case(true)]
    fn get_external_files_missing_directory_directory_not_queried(
        #[case] metadata_directory: bool,
    ) {
        let (containing_folder, metadata_path, existing) = if metadata_directory {
            (VIDEO_DIRECTORY_PATH, "invalid", vec![VIDEO_DIRECTORY_PATH])
        } else {
            (
                "invalid",
                METADATA_DIRECTORY_PATH,
                vec![METADATA_DIRECTORY_PATH],
            )
        };
        // any path other than test target exists and provides an empty listing
        let fs = FakeFs::new(&existing, Vec::new());
        let resolver = resolver(
            DlnaProfileType::Subtitle,
            vec![MediaStream::default()],
            Arc::clone(&fs),
        );
        let video = ExternalMediaTarget {
            path: format!("{VIDEO_DIRECTORY_PATH}/My.Video.mkv"),
            containing_folder: containing_folder.to_owned(),
            internal_metadata_path: Some(metadata_path.to_owned()),
        };

        let _ignored = resolver.get_external_files(&video);

        // The missing folder (video or metadata, per case) is never listed.
        assert!(
            !fs.queried().iter().any(|q| q == "invalid"),
            "a missing directory must not be listed: {:?}",
            fs.queried()
        );
    }

    // GetExternalFiles_NameMatching_MatchesAndParsesToken
    #[rstest]
    #[case("My.Video.mkv", "My.Video.srt", None, false)]
    #[case("My.Video.mkv", "My.Video.en.srt", Some("eng"), false)]
    #[case("My.Video.mkv", "My.Video.en.srt", Some("eng"), true)]
    #[case(
        "Example Movie (2021).mp4",
        "Example Movie (2021).English.Srt",
        Some("eng"),
        false
    )]
    #[case(
        "[LTDB] Who Framed Roger Rabbit (1998) - [Bluray-1080p].mkv",
        "[LTDB] Who Framed Roger Rabbit (1998) - [Bluray-1080p].en.srt",
        Some("eng"),
        false
    )]
    fn get_external_files_name_matching_matches_and_parses_token(
        #[case] movie: &str,
        #[case] file: &str,
        #[case] language: Option<&str>,
        #[case] metadata_directory: bool,
    ) {
        let fs = FakeFs::for_external_file(file, metadata_directory);
        let resolver = resolver(DlnaProfileType::Subtitle, vec![MediaStream::default()], fs);

        let streams = resolver.get_external_files(&video(movie));

        assert_eq!(streams.len(), 1, "{streams:?}");
        let actual = &streams[0];
        assert_eq!(actual.language.as_deref(), language);
        assert_eq!(actual.title, None);
    }

    // GetExternalFiles_NameMatching_RejectsNonMatches
    #[rstest]
    #[case("cover.jpg")]
    #[case("My.Video.mp3")]
    #[case("My.Video.png")]
    #[case("My.Video.txt")]
    #[case("My.Video Sequel.srt")]
    #[case("Some.Other.Video.srt")]
    fn get_external_files_name_matching_rejects_non_matches(#[case] file: &str) {
        let fs = FakeFs::for_external_file(file, false);
        let resolver = resolver(DlnaProfileType::Subtitle, vec![MediaStream::default()], fs);

        let streams = resolver.get_external_files(&video("My.Video.mkv"));

        assert!(streams.is_empty(), "{streams:?}");
    }

    // GetExternalStreams_BadPaths_ReturnsNoSubtitles
    #[rstest]
    #[case("https://url.com/My.Video.mkv")]
    #[case(VIDEO_DIRECTORY_PATH)] // valid but no files found for this test
    #[tokio::test]
    async fn get_external_streams_bad_paths_returns_no_subtitles(#[case] path: &str) {
        let fs = FakeFs::new(&[], Vec::new());
        let resolver = resolver(DlnaProfileType::Subtitle, Vec::new(), fs);
        let video = ExternalMediaTarget::for_file(path, None);

        let streams = resolver.get_external_streams(&video, 0).await;

        assert!(streams.is_empty());
    }

    /// `GetExternalStreams_MergeMetadata_HandlesOverridesCorrectly_Data` — the
    /// C# theory data, one entry per `data.Add`.
    #[allow(clippy::too_many_lines)]
    fn merge_metadata_data() -> Vec<(&'static str, Vec<MediaStream>, Vec<MediaStream>)> {
        let path = |file: &str| format!("{VIDEO_DIRECTORY_PATH}/{file}");
        vec![
            // filename and stream have no metadata set
            (
                "My.Video.srt",
                vec![create_media_stream(
                    &path("My.Video.srt"),
                    None,
                    None,
                    0,
                    false,
                    false,
                    false,
                )],
                vec![create_media_stream(
                    &path("My.Video.srt"),
                    None,
                    None,
                    0,
                    false,
                    false,
                    false,
                )],
            ),
            // filename has metadata
            (
                "My.Video.Title1.default.forced.sdh.en.srt",
                vec![create_media_stream(
                    &path("My.Video.Title1.default.forced.sdh.en.srt"),
                    None,
                    None,
                    0,
                    false,
                    false,
                    false,
                )],
                vec![create_media_stream(
                    &path("My.Video.Title1.default.forced.sdh.en.srt"),
                    Some("eng"),
                    Some("Title1"),
                    0,
                    true,
                    true,
                    true,
                )],
            ),
            // single stream with metadata
            (
                "My.Video.mks",
                vec![create_media_stream(
                    &path("My.Video.mks"),
                    Some("eng"),
                    Some("Title"),
                    0,
                    true,
                    true,
                    true,
                )],
                vec![create_media_stream(
                    &path("My.Video.mks"),
                    Some("eng"),
                    Some("Title"),
                    0,
                    true,
                    false,
                    true,
                )],
            ),
            // stream wins for title/language, filename wins for flags when conflicting
            (
                "My.Video.Title2.default.forced.sdh.en.srt",
                vec![create_media_stream(
                    &path("My.Video.Title2.default.forced.sdh.en.srt"),
                    Some("fra"),
                    Some("Metadata"),
                    0,
                    false,
                    false,
                    false,
                )],
                vec![create_media_stream(
                    &path("My.Video.Title2.default.forced.sdh.en.srt"),
                    Some("fra"),
                    Some("Metadata"),
                    0,
                    true,
                    true,
                    true,
                )],
            ),
            // multiple stream with metadata - filename flags ignored but other data filled in when missing from stream
            (
                "My.Video.Title3.default.forced.en.srt",
                vec![
                    create_media_stream(
                        &path("My.Video.Title3.default.forced.en.srt"),
                        None,
                        None,
                        0,
                        true,
                        true,
                        false,
                    ),
                    create_media_stream(
                        &path("My.Video.Title3.default.forced.en.srt"),
                        Some("fra"),
                        Some("Metadata"),
                        1,
                        false,
                        false,
                        false,
                    ),
                ],
                vec![
                    create_media_stream(
                        &path("My.Video.Title3.default.forced.en.srt"),
                        Some("eng"),
                        Some("Title3"),
                        0,
                        true,
                        true,
                        false,
                    ),
                    create_media_stream(
                        &path("My.Video.Title3.default.forced.en.srt"),
                        Some("fra"),
                        Some("Metadata"),
                        1,
                        false,
                        false,
                        false,
                    ),
                ],
            ),
        ]
    }

    // GetExternalStreams_MergeMetadata_HandlesOverridesCorrectly
    #[rstest]
    #[case(0)]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[case(4)]
    #[tokio::test]
    async fn get_external_streams_merge_metadata_handles_overrides_correctly(#[case] data: usize) {
        let (file, input_streams, expected_streams) = merge_metadata_data().swap_remove(data);
        let fs = FakeFs::for_external_file(file, false);
        let resolver = resolver(DlnaProfileType::Subtitle, input_streams, fs);

        let streams = resolver
            .get_external_streams(&video("My.Video.mkv"), 0)
            .await;

        assert_eq!(expected_streams.len(), streams.len(), "{streams:?}");
        for (expected, actual) in expected_streams.iter().zip(&streams) {
            assert!(actual.is_external);
            assert_eq!(expected.index, actual.index);
            assert_eq!(expected.stream_type, actual.stream_type);
            assert_eq!(expected.path, actual.path);
            assert_eq!(expected.is_default, actual.is_default, "IsDefault");
            assert_eq!(expected.is_forced, actual.is_forced, "IsForced");
            assert_eq!(
                expected.is_hearing_impaired, actual.is_hearing_impaired,
                "IsHearingImpaired"
            );
            assert_eq!(expected.language, actual.language, "Language");
            assert_eq!(expected.title, actual.title, "Title");
        }
    }

    // GetExternalStreams_StreamIndex_HandlesFilesAndContainers
    #[rstest]
    #[case(1, 1)]
    #[case(1, 2)]
    #[case(2, 1)]
    #[case(2, 2)]
    #[tokio::test]
    async fn get_external_streams_stream_index_handles_files_and_containers(
        #[case] file_count: usize,
        #[case] stream_count: usize,
    ) {
        let files: Vec<String> = (0..file_count)
            .map(|i| format!("{VIDEO_DIRECTORY_PATH}/My.Video.{i}.srt"))
            .collect();
        let fs = FakeFs::new(
            &[VIDEO_DIRECTORY_PATH, METADATA_DIRECTORY_PATH],
            vec![
                (VIDEO_DIRECTORY_PATH, files),
                (METADATA_DIRECTORY_PATH, Vec::new()),
            ],
        );
        let media_streams = (0..stream_count)
            .map(|_| MediaStream {
                stream_type: MediaStreamType::Subtitle,
                ..MediaStream::default()
            })
            .collect();
        let resolver = resolver(DlnaProfileType::Subtitle, media_streams, fs);

        let start_index = 1;
        let streams = resolver
            .get_external_streams(&video("My.Video.mkv"), start_index)
            .await;

        assert_eq!(file_count * stream_count, streams.len());
        for (i, stream) in streams.iter().enumerate() {
            assert_eq!(start_index + i32::try_from(i).expect("small"), stream.index);
            // intentional integer division to ensure correct number of streams come back from each file
            let expected_suffix = format!(".{}.srt", i / stream_count);
            assert!(
                stream
                    .path
                    .as_deref()
                    .is_some_and(|p| p.ends_with(&expected_suffix)),
                "{stream:?} should end with {expected_suffix}"
            );
        }
    }

    // SubtitleResolverTests.GetExternalStreams_MixedFilenames_PicksSubtitles
    #[rstest]
    #[case("My.Video.srt", false, true)]
    #[case("My.Video.mp3", false, false)]
    #[case("My.Video.srt", true, true)]
    #[case("My.Video.mp3", true, false)]
    #[tokio::test]
    async fn subtitle_resolver_mixed_filenames_picks_subtitles(
        #[case] file: &str,
        #[case] metadata_directory: bool,
        #[case] matches: bool,
    ) {
        let fs = FakeFs::for_external_file(file, metadata_directory);
        let resolver = MediaInfoResolver::subtitle(
            Arc::new(NamingOptions::new()),
            localization(),
            Arc::new(FakeEncoder {
                streams: vec![MediaStream {
                    stream_type: MediaStreamType::Subtitle,
                    ..MediaStream::default()
                }],
            }),
            fs,
        );

        let streams = resolver
            .get_external_streams(&video("My.Video.mkv"), 0)
            .await;

        if matches {
            assert_eq!(streams.len(), 1, "{streams:?}");
            assert_eq!(streams[0].stream_type, MediaStreamType::Subtitle);
        } else {
            assert!(streams.is_empty(), "{streams:?}");
        }
    }

    // AudioResolverTests.GetExternalStreams_MixedFilenames_PicksAudio
    #[rstest]
    #[case("My.Video.srt", false, false)]
    #[case("My.Video.mp3", false, true)]
    #[case("My.Video.srt", true, false)]
    #[case("My.Video.mp3", true, true)]
    #[tokio::test]
    async fn audio_resolver_mixed_filenames_picks_audio(
        #[case] file: &str,
        #[case] metadata_directory: bool,
        #[case] matches: bool,
    ) {
        let fs = FakeFs::for_external_file(file, metadata_directory);
        let resolver = MediaInfoResolver::audio(
            Arc::new(NamingOptions::new()),
            localization(),
            Arc::new(FakeEncoder {
                streams: vec![MediaStream {
                    stream_type: MediaStreamType::Audio,
                    ..MediaStream::default()
                }],
            }),
            fs,
        );

        let streams = resolver
            .get_external_streams(&video("My.Video.mkv"), 0)
            .await;

        if matches {
            assert_eq!(streams.len(), 1, "{streams:?}");
            assert_eq!(streams[0].stream_type, MediaStreamType::Audio);
        } else {
            assert!(streams.is_empty(), "{streams:?}");
        }
    }

    // The pair runs subtitles first, then audio, numbered contiguously, and
    // the embedded streams follow — `FFProbeVideoInfo.Fetch`'s order.
    #[tokio::test]
    async fn resolver_pair_orders_subtitles_then_audio_then_embedded() {
        let fs = FakeFs::new(
            &[VIDEO_DIRECTORY_PATH],
            vec![(
                VIDEO_DIRECTORY_PATH,
                vec![
                    format!("{VIDEO_DIRECTORY_PATH}/My.Video.commentary.mka"),
                    format!("{VIDEO_DIRECTORY_PATH}/My.Video.en.srt"),
                ],
            )],
        );
        // One fake answers both probes with a subtitle and an audio stream;
        // the subtitle resolver only ever probes the `.srt` (and keeps its
        // subtitle), the audio resolver only the `.mka` (and keeps its audio).
        let encoder = Arc::new(FakeEncoder {
            streams: vec![
                MediaStream {
                    stream_type: MediaStreamType::Subtitle,
                    ..MediaStream::default()
                },
                MediaStream {
                    stream_type: MediaStreamType::Audio,
                    ..MediaStream::default()
                },
            ],
        });
        let pair = ExternalStreamResolvers::new(
            Arc::new(NamingOptions::new()),
            localization(),
            encoder,
            fs,
        );
        let target = pair.target_for(&format!("{VIDEO_DIRECTORY_PATH}/My.Video.mkv"), None);
        assert_eq!(target.containing_folder, VIDEO_DIRECTORY_PATH);

        let external = pair.external_streams(&target, 0).await;
        let types: Vec<MediaStreamType> = external.iter().map(|s| s.stream_type).collect();
        assert_eq!(
            types,
            vec![MediaStreamType::Subtitle, MediaStreamType::Audio]
        );
        let indices: Vec<i32> = external.iter().map(|s| s.index).collect();
        assert_eq!(indices, vec![0, 1]);
        // A multi-stream probe result keeps the filename's language as a
        // fallback; the `.commentary` token is a title.
        assert_eq!(external[0].language.as_deref(), Some("eng"));
        assert_eq!(external[1].title.as_deref(), Some("commentary"));

        let embedded = vec![
            MediaStream {
                index: 0,
                stream_type: MediaStreamType::Video,
                ..MediaStream::default()
            },
            MediaStream {
                index: 1,
                stream_type: MediaStreamType::Audio,
                ..MediaStream::default()
            },
        ];
        let all = ExternalStreamResolvers::merge_with_embedded(external, embedded);
        let indices: Vec<i32> = all.iter().map(|s| s.index).collect();
        assert_eq!(indices, vec![0, 1, 2, 3]);
        assert_eq!(all[2].stream_type, MediaStreamType::Video);
        assert!(all[..2].iter().all(|s| s.is_external));
        assert!(all[2..].iter().all(|s| !s.is_external));
    }

    // A `.strm` sidecar is never probed, and a probe failure skips just that file.
    #[tokio::test]
    async fn strm_sidecars_are_skipped_and_probe_failures_are_isolated() {
        struct FailingEncoder;
        #[async_trait]
        impl MediaEncoder for FailingEncoder {
            fn encoder_path(&self) -> String {
                String::new()
            }
            fn probe_path(&self) -> String {
                String::new()
            }
            async fn set_ffmpeg_path(&self) -> Result<bool, ServiceError> {
                Ok(false)
            }
            async fn get_media_info(
                &self,
                request: &MediaInfoRequest,
            ) -> Result<MediaSourceInfo, ServiceError> {
                let path = request.media_source.path.clone().unwrap_or_default();
                assert!(
                    !naming_path::extension(&path).eq_ignore_ascii_case(".strm"),
                    "a .strm must never be probed"
                );
                if path.ends_with(".bad.mka") {
                    return Err(ServiceError::backend("unreadable"));
                }
                Ok(MediaSourceInfo {
                    media_streams: vec![MediaStream {
                        stream_type: MediaStreamType::Audio,
                        ..MediaStream::default()
                    }],
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
                _video_stream: &MediaStream,
                _threed_format: Option<Video3DFormat>,
                _offset_ticks: Option<i64>,
            ) -> Result<String, ServiceError> {
                unreachable!()
            }
            fn get_input_argument(&self, input_file: &str, _m: &MediaSourceInfo) -> String {
                input_file.to_owned()
            }
            fn get_time_parameter(&self, _ticks: i64) -> String {
                String::new()
            }
            async fn convert_image(&self, _i: &str, _o: &str) -> Result<(), ServiceError> {
                Ok(())
            }
        }

        let fs = FakeFs::new(
            &[VIDEO_DIRECTORY_PATH],
            vec![(
                VIDEO_DIRECTORY_PATH,
                vec![
                    format!("{VIDEO_DIRECTORY_PATH}/My.Video.strm"),
                    format!("{VIDEO_DIRECTORY_PATH}/My.Video.bad.mka"),
                    format!("{VIDEO_DIRECTORY_PATH}/My.Video.good.mka"),
                ],
            )],
        );
        let resolver = MediaInfoResolver::audio(
            Arc::new(NamingOptions::new()),
            localization(),
            Arc::new(FailingEncoder),
            fs,
        );

        let streams = resolver
            .get_external_streams(&video("My.Video.mkv"), 0)
            .await;

        assert_eq!(streams.len(), 1, "{streams:?}");
        assert!(
            streams[0]
                .path
                .as_deref()
                .is_some_and(|p| p.ends_with("My.Video.good.mka"))
        );
        assert_eq!(streams[0].index, 0);
    }

    #[rstest]
    #[case("/media/Movie.mkv", true)]
    #[case("file:///media/Movie.mkv", true)]
    #[case("C:\\media\\Movie.mkv", true)]
    #[case("https://url.com/My.Video.mkv", false)]
    #[case("HTTP://url.com/My.Video.mkv", false)]
    #[case("rtsp://cam/stream", false)]
    #[case("smb://nas/share/Movie.mkv", false)]
    fn is_file_protocol_matches_get_path_protocol(#[case] path: &str, #[case] is_file: bool) {
        assert_eq!(is_file_protocol(path), is_file);
    }

    #[rstest]
    #[case("My.Video", "My.Video", Some(""))]
    #[case("my.video.en", "My.Video", Some(".en"))]
    #[case("My.Video Sequel", "My.Video", Some(" Sequel"))]
    #[case("My.Vid", "My.Video", None)]
    #[case("Other.Video", "My.Video", None)]
    fn strip_prefix_ignores_case(
        #[case] name: &str,
        #[case] prefix: &str,
        #[case] rest: Option<&str>,
    ) {
        assert_eq!(strip_prefix_ignore_case(name, prefix), rest);
    }
}
