//! [`FerrofinSubtitleManager`] — subtitle search/download/upload/delete over the
//! `MediaStreamInfos` table and a registry of [`SubtitleProvider`]s.
//!
//! Port of `MediaBrowser.Providers.Subtitles.SubtitleManager`:
//!
//! - `Self::search_subtitles` enriches the request from the resolved item
//!   (name/year/series/season/episode/path) and fans it out across the
//!   registered providers (OpenSubtitles, …), aggregating the candidates. A
//!   provider that errors is skipped (logged) rather than failing the whole
//!   search, matching Jellyfin's per-provider aggregation.
//! - `Self::download_subtitles` routes the namespaced id back to its provider,
//!   fetches the content, and **attaches** it to the item: a sidecar in the
//!   library's configured media or internal-metadata folder. The stream is left
//!   to the refresh the caller runs next, whose probe finds the file.
//!   `Self::upload_subtitle` attaches caller-supplied content the same way.
//!   A failed download is recorded in the activity log
//!   (`Self::report_download_failure`, `SubtitleDownloadFailureLogger`).
//! - `Self::get_remote_subtitles` routes an id to its provider and returns the
//!   raw content (the `/Providers/Subtitles/Subtitles/{id}` route).
//! - `Self::delete_subtitles` deletes an external subtitle's file; the item
//!   refresh the route then awaits drops its stream.
//! - `Self::drop_missing_subtitle_rows` drops the external subtitle rows whose
//!   file is gone and, when it drops one, stores the video's `HasSubtitles`
//!   from the rows left; the route runs it after that refresh, for one that
//!   cannot probe.
//! - `Self::get_supported_providers` lists the registered providers for an item.

use ferrofin_util::directory_path::DirectoryPath;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::providers::{RemoteSubtitleInfo, SubtitleProviderInfo};
use uuid::Uuid;

use ferrofin_traits::activity::{ActivityLogCreate, ActivityManager};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, SelfWrite, SelfWriteGuard, VirtualFolderManager};
use ferrofin_traits::persistence::{MediaStreamQuery, MediaStreamRepository};
use ferrofin_traits::subtitles::{
    ProviderLogin, SubtitleManager, SubtitleMediaType, SubtitleProvider, SubtitleResponse,
    SubtitleSearchRequest, is_rate_limit_exceeded,
};

use crate::db_error::media_stream_type_disc;

/// The concrete subtitle manager.
#[derive(Clone)]
pub struct FerrofinSubtitleManager {
    db: Database,
    library_manager: Arc<dyn LibraryManager>,
    media_streams: Arc<dyn MediaStreamRepository>,
    providers: Vec<Arc<dyn SubtitleProvider>>,
    /// Live owning-library options for selecting the subtitle destination.
    virtual_folders: Option<Arc<dyn VirtualFolderManager>>,
    /// Internal-metadata base (`{program-data}/metadata`). Subtitles are written
    /// here (`.../library/{id2}/{idN}/`) when SaveSubtitlesWithMedia is disabled.
    metadata_path: DirectoryPath,
    /// Brackets the files written beside the media, so the library monitor
    /// takes them for the server's own writes (`ILibraryMonitor`). `None` in
    /// unit tests.
    self_writes: Option<Arc<dyn SelfWriteGuard>>,
    /// Where a failed download is recorded (`SubtitleDownloadFailureLogger`).
    /// `None` in unit tests.
    activity: Option<Arc<dyn ActivityManager>>,
}

impl std::fmt::Debug for FerrofinSubtitleManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinSubtitleManager")
            .field("providers", &self.providers.len())
            .finish_non_exhaustive()
    }
}

impl FerrofinSubtitleManager {
    /// Creates a subtitle manager over the database, library seam, media-stream
    /// repository, and the registered subtitle providers.
    #[must_use]
    pub fn new(
        db: Database,
        library_manager: Arc<dyn LibraryManager>,
        media_streams: Arc<dyn MediaStreamRepository>,
        mut providers: Vec<Arc<dyn SubtitleProvider>>,
        metadata_path: impl Into<DirectoryPath>,
    ) -> Self {
        providers.sort_by_key(|provider| provider.order());
        Self {
            db,
            library_manager,
            media_streams,
            providers,
            virtual_folders: None,
            metadata_path: metadata_path.into(),
            self_writes: None,
            activity: None,
        }
    }

    /// Records each failed download in the activity log, as
    /// `SubtitleDownloadFailureLogger` does. Called once by the composition
    /// root.
    #[must_use]
    pub fn with_activity(mut self, activity: Arc<dyn ActivityManager>) -> Self {
        self.activity = Some(activity);
        self
    }

    /// Brackets each subtitle written or deleted, as `SubtitleManager` does
    /// (`ReportFileSystemChangeBeginning`/`Complete`). Called once by the
    /// composition root.
    #[must_use]
    pub fn with_self_writes(mut self, guard: Arc<dyn SelfWriteGuard>) -> Self {
        self.self_writes = Some(guard);
        self
    }

    /// Resolves the saved subtitle destination from the item's owning library.
    /// Changes take effect on the next download or upload without a restart.
    #[must_use]
    pub fn with_virtual_folders(mut self, folders: Arc<dyn VirtualFolderManager>) -> Self {
        self.virtual_folders = Some(folders);
        self
    }

    /// The item's internal-metadata folder (`{metadata}/library/{id2}/{idN}`).
    fn item_metadata_dir(&self, item_id: Uuid) -> PathBuf {
        let dashless = item_id.simple().to_string();
        self.metadata_path
            .join("library")
            .join(&dashless[..2])
            .join(&dashless)
    }

    /// Fills the request's two-letter language and its item-derived fields
    /// (name/year/series/season/episode/path/content-type) from the resolved
    /// item, so providers can build a query.
    async fn enrich(&self, request: &mut SubtitleSearchRequest) -> Result<bool, ServiceError> {
        // SubtitleManager.SearchSubtitles: FindLanguageInfo(Language)?.TwoLetterISOLanguageName.
        if let Some(culture) = crate::localization_manager::find_culture(&request.language) {
            request.two_letter_iso_language_name =
                Some(culture.two_letter_iso_language_name.clone());
        }
        if let Some(item) = self.library_manager.get_item_by_id(request.item_id).await? {
            // SearchSubtitles(Video, …): a disc rip or an ISO is never searched.
            if crate::item_data::video_format(item.data.as_deref()).video_type
                != ferrofin_model::entities::VideoType::VideoFile
            {
                return Ok(false);
            }
            let Some(content_type) = subtitle_media_type(&item.type_) else {
                return Ok(false);
            };
            request.content_type = content_type;
            request.name = item.name;
            request.series_name = item.series_name;
            request.production_year = item.production_year.and_then(|y| i32::try_from(y).ok());
            request.parent_index_number =
                item.parent_index_number.and_then(|n| i32::try_from(n).ok());
            request.index_number = item.index_number.and_then(|n| i32::try_from(n).ok());
            request.runtime_ticks = item.run_time_ticks;
            request.index_number_end = if content_type == SubtitleMediaType::Episode {
                crate::item_data::parse_data(item.data.as_deref())
                    .get("IndexNumberEnd")
                    .and_then(serde_json::Value::as_i64)
                    .and_then(|number| i32::try_from(number).ok())
            } else {
                None
            };
            request.provider_ids = self
                .db
                .provider_ids_for_items(&[guid_to_db(request.item_id)])
                .await
                .map_err(ServiceError::from)?
                .into_iter()
                .map(|(_, key, value)| (key, value))
                .collect();
            // The built-in OpenSubtitles REST adapter consumes this convenience
            // field; plugins receive the complete upstream ProviderIds map.
            request.imdb_id = request
                .provider_ids
                .iter()
                .find(|(key, _)| {
                    ferrofin_util::string_extensions::equals_ordinal_ignore_case(key, "Imdb")
                })
                .map(|(_, value)| value.clone());
            request.media_path = item.path;
        }
        Ok(true)
    }

    /// Selects the provider that owns a namespaced id (`"{provider_id}_{local}"`),
    /// returning it plus the provider-local id. Like `String.Split('_', 2)` with
    /// `parts[0]`/`parts[^1]`, an id without `_` is both the provider id and the
    /// local id.
    fn route(&self, id: &str) -> Option<(&Arc<dyn SubtitleProvider>, String)> {
        let (head, local) = id.split_once('_').unwrap_or((id, id));
        self.providers
            .iter()
            .find(|p| provider_id(p.name()) == head)
            .map(|p| (p, local.to_owned()))
    }

    /// A failed provider contributes no candidates, letting ordered fallback
    /// proceed. An all-provider search calls this concurrently but keeps the
    /// configured provider order when assembling results.
    async fn search_provider(
        provider: &Arc<dyn SubtitleProvider>,
        request: &SubtitleSearchRequest,
    ) -> Vec<RemoteSubtitleInfo> {
        match provider.search(request).await {
            Ok(mut found) => {
                if request.is_perfect_match == Some(true) {
                    found.retain(|result| result.is_hash_match == Some(true));
                }
                // SubtitleManager.Normalize: namespace every candidate id with
                // the MD5 id of the candidate's own provider name.
                for result in &mut found {
                    let namespace = provider_id(
                        result
                            .provider_name
                            .as_deref()
                            .unwrap_or_else(|| provider.name()),
                    );
                    result.id = Some(format!(
                        "{namespace}_{}",
                        result.id.as_deref().unwrap_or("")
                    ));
                }
                found
            }
            Err(error) => {
                tracing::warn!(provider = provider.name(), %error, "subtitle search failed");
                Vec::new()
            }
        }
    }

    /// Writes subtitle content to the configured destination, preserving any
    /// existing subtitle file (`TrySaveSubtitle`). Like upstream it records no
    /// stream: the refresh the caller runs next probes the new file.
    async fn attach(&self, item_id: Uuid, response: &SubtitleResponse) -> Result<(), ServiceError> {
        let item = self
            .library_manager
            .get_item_by_id(item_id)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("item {item_id}")))?;
        let media_path = item
            .path
            .as_deref()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| ServiceError::invalid_input("item has no media path for a sidecar"))?;

        let save_with_media = if let Some(folders) = &self.virtual_folders {
            let folders = folders.library_folders().await?;
            ferrofin_model::entities_media::owning_library(
                &folders,
                item.top_parent_id.as_deref(),
                Some(media_path),
            )
            .and_then(|folder| folder.library_options.as_ref())
            .is_none_or(|options| options.save_subtitles_with_media)
        } else {
            // LibraryOptions.SaveSubtitlesWithMedia defaults to true.
            true
        };
        let name = sidecar_name(media_path, response)?;
        let destination = if save_with_media {
            // `Video.ContainingFolderPath`: a DVD or Blu-ray folder rip's own
            // folder, else the media file's. A root path has none (`Path.Combine`
            // throws on upstream's null).
            let folder = self
                .library_manager
                .containing_folder_path(&item)
                .await
                .ok_or_else(|| ServiceError::invalid_input("item has no folder for a sidecar"))?;
            PathBuf::from(folder).join(name)
        } else {
            self.item_metadata_dir(item_id).join(name)
        };
        // The pinned SubtitleManager selects one destination. An unwritable
        // media folder is an error, not permission to write into metadata.
        write_sidecar(&destination, &response.content, self.self_writes.as_ref())
            .await
            .map(drop)
            .map_err(|error| ServiceError::backend(error.to_string()))
    }
}

/// `SubtitleManager.GetProviderId`: the .NET `GetMD5().ToString("N")` of the
/// lowercased provider name. It namespaces candidate ids and identifies the
/// provider in `GetSupportedProviders`.
fn provider_id(name: &str) -> String {
    ferrofin_common::extensions::get_md5(&ferrofin_util::string_extensions::lower_invariant(name))
        .simple()
        .to_string()
}

/// `SubtitleDownloadFailureLogger.GetItemName` for the videos a subtitle is
/// downloaded for: an episode is `{series} - S{season}, Ep{n} - {name}`, a
/// music video `{first artist} - {name}`.
fn item_name(item: &BaseItemEntity) -> String {
    let mut name = item.name.clone().unwrap_or_default();
    match item.type_.rsplit('.').next() {
        Some("Episode") => {
            if let Some(episode) = item.index_number {
                name = format!("Ep{episode} - {name}");
            }
            if let Some(season) = item.parent_index_number {
                name = format!("S{season}, {name}");
            }
            // `IHasSeries`: C# concatenates a null `SeriesName` as "".
            name = format!(
                "{} - {name}",
                item.series_name.as_deref().unwrap_or_default()
            );
        }
        Some("MusicVideo") => {
            if let Some(artist) = crate::dto_service::split_multi(item.artists.as_deref()).first() {
                name = format!("{artist} - {name}");
            }
        }
        _ => {}
    }
    name
}

/// Only Movies and Episodes are supported by the subtitle manager's item
/// overload; other video kinds do not inherit Movie provider support.
fn subtitle_media_type(type_name: &str) -> Option<SubtitleMediaType> {
    match type_name.rsplit('.').next()? {
        "Movie" => Some(SubtitleMediaType::Movie),
        "Episode" => Some(SubtitleMediaType::Episode),
        _ => None,
    }
}

/// Writes a new subtitle without replacing an existing subtitle's bytes.
/// Collisions use `{stem}.0.{extension}`, `{stem}.1.{extension}`, ... just as
/// `SubtitleManager.TrySaveToFiles` does. CreateNew also guards the final open
/// against concurrent uploads selecting the same available filename.
///
/// The free name it writes is bracketed on `self_writes`, as upstream
/// brackets the name it settles on.
async fn write_sidecar(
    path: &Path,
    content: &[u8],
    self_writes: Option<&Arc<dyn SelfWriteGuard>>,
) -> std::io::Result<PathBuf> {
    use tokio::io::AsyncWriteExt as _;

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let stem = path.file_stem().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "subtitle has no filename")
    })?;
    let extension = path.extension().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "subtitle has no extension",
        )
    })?;
    let mut candidate = path.to_path_buf();
    let mut counter = 0_u64;
    loop {
        // `File.Exists` first, as upstream settles on a free name before it
        // brackets it; `create_new` still guards the race with a concurrent
        // upload of the same name.
        let taken = tokio::fs::try_exists(&candidate).await.unwrap_or(false);
        let _write = (!taken).then(|| SelfWrite::begin(self_writes, candidate.to_string_lossy()));
        let opened = if taken {
            Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists))
        } else {
            tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
                .await
        };
        match opened {
            Ok(mut file) => {
                file.write_all(content).await?;
                file.flush().await?;
                return Ok(candidate);
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists
                    && tokio::fs::metadata(&candidate)
                        .await
                        .is_ok_and(|metadata| metadata.is_file()) =>
            {
                candidate = path.with_file_name(format!(
                    "{}.{counter}.{}",
                    stem.to_string_lossy(),
                    extension.to_string_lossy()
                ));
                counter = counter
                    .checked_add(1)
                    .ok_or_else(|| std::io::Error::other("subtitle filename counter exhausted"))?;
            }
            Err(error) => return Err(error),
        }
    }
}

/// `NamingOptions.SubtitleFileExtensions` at the pinned upstream revision.
const SUBTITLE_EXTENSIONS: &[&str] = &[
    "ass", "mks", "sami", "smi", "srt", "ssa", "sub", "sup", "vtt",
];

/// Validates the format before using it as a filename extension. Upstream
/// accepts these names case-insensitively without trimming or adding a dot.
fn subtitle_extension(format: &str) -> Result<String, ServiceError> {
    let extension = format.to_ascii_lowercase();
    if SUBTITLE_EXTENSIONS.contains(&extension.as_str()) {
        Ok(extension)
    } else {
        Err(ServiceError::invalid_input(format!(
            "unsupported subtitle format: {format}"
        )))
    }
}

/// Builds the upstream `{stem}.{language}[.forced][.sdh].{format}` filename.
fn sidecar_name(media_path: &str, response: &SubtitleResponse) -> Result<String, ServiceError> {
    let stem = Path::new(media_path).file_stem().map_or_else(
        || "subtitle".to_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    );
    if response
        .language
        .chars()
        .any(|character| std::path::is_separator(character) || character == '\0')
    {
        return Err(ServiceError::invalid_input(
            "subtitle language contains invalid characters",
        ));
    }
    let language = response.language.to_lowercase();
    let extension = subtitle_extension(&response.format)?;
    let forced = if response.is_forced { ".forced" } else { "" };
    let hearing_impaired = if response.is_hearing_impaired {
        ".sdh"
    } else {
        ""
    };
    Ok(format!(
        "{stem}.{language}{forced}{hearing_impaired}.{extension}"
    ))
}

#[async_trait]
impl SubtitleManager for FerrofinSubtitleManager {
    async fn search_subtitles(
        &self,
        request: &SubtitleSearchRequest,
    ) -> Result<Vec<RemoteSubtitleInfo>, ServiceError> {
        let mut request = request.clone();
        if !self.enrich(&mut request).await? {
            return Ok(Vec::new());
        }
        let mut providers: Vec<_> = self
            .providers
            .iter()
            .filter(|provider| {
                provider
                    .supported_media_types()
                    .contains(&request.content_type)
                    && !request
                        .disabled_subtitle_fetchers
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(provider.name()))
            })
            .collect();
        providers.sort_by_key(|provider| {
            request
                .subtitle_fetcher_order
                .iter()
                .position(|name| name == provider.name())
                .unwrap_or(usize::MAX)
        });
        if !request.search_all_providers.unwrap_or(true) {
            for provider in providers {
                let found = Self::search_provider(provider, &request).await;
                if !found.is_empty() {
                    return Ok(found);
                }
            }
            return Ok(Vec::new());
        }
        let results = futures_util::future::join_all(
            providers
                .into_iter()
                .map(|provider| Self::search_provider(provider, &request)),
        )
        .await;
        Ok(results.into_iter().flatten().collect())
    }

    async fn download_subtitles(
        &self,
        item_id: Uuid,
        subtitle_id: &str,
    ) -> Result<(), ServiceError> {
        let (provider, local) = self
            .route(subtitle_id)
            .ok_or_else(|| ServiceError::invalid_input("unknown subtitle provider for id"))?;
        let downloaded = async {
            let response = provider.get_subtitles(&local).await?;
            self.attach(item_id, &response).await
        }
        .await;
        if let Err(error) = &downloaded {
            self.report_download_failure(item_id, subtitle_id, error)
                .await;
        }
        downloaded
    }

    async fn upload_subtitle(
        &self,
        item_id: Uuid,
        response: &SubtitleResponse,
    ) -> Result<(), ServiceError> {
        self.attach(item_id, response).await
    }

    async fn get_remote_subtitles(&self, id: &str) -> Result<SubtitleResponse, ServiceError> {
        let (provider, local) = self
            .route(id)
            .ok_or_else(|| ServiceError::invalid_input("unknown subtitle provider for id"))?;
        provider.get_subtitles(&local).await
    }

    async fn report_download_failure(
        &self,
        item_id: Uuid,
        subtitle_id: &str,
        error: &ServiceError,
    ) {
        // `GetProvider` runs before the `try`, and `catch
        // (RateLimitExceededException) { throw; }` raises nothing.
        let (Some(activity), Some((provider, _))) = (&self.activity, self.route(subtitle_id))
        else {
            return;
        };
        if is_rate_limit_exceeded(error) {
            return;
        }
        // The item was found before the download began; one gone since gets
        // an empty name, and `ItemId` still names it.
        let item = self
            .library_manager
            .get_item_by_id(item_id)
            .await
            .ok()
            .flatten()
            .map_or_default(|item| item_name(&item));
        let entry = ActivityLogCreate {
            name: format!(
                "Subtitles failed to download from {} for {item}",
                provider.name()
            ),
            type_: "SubtitleDownloadFailure".to_owned(),
            item_id: Some(item_id),
            short_overview: Some(error.message()),
            ..Default::default()
        };
        if let Err(write) = activity.create_entry(entry).await {
            tracing::warn!(
                %item_id,
                error = %write,
                "could not record the failed subtitle download in the activity log"
            );
        }
    }

    async fn delete_subtitles(&self, item_id: Uuid, index: i32) -> Result<(), ServiceError> {
        // `GetMediaStreams(…)[0]` throws for no subtitle stream at `index`, and
        // `ReportFileSystemChangeBeginning` for one with no path (an embedded
        // stream): both are `ArgumentException`s. Upstream stores no path on an
        // embedded row, so skipping one that has a path changes no answer and
        // keeps a stray path from deleting the media file. The stream
        // repository expands a stored `%MetadataPath%`/`%AppDataPath%` path
        // (`RestorePath`), so the file removed is the one on disk.
        let path = self
            .media_streams
            .get_media_streams(&MediaStreamQuery {
                item_id,
                stream_type: Some(ferrofin_model::entities::MediaStreamType::Subtitle),
                index: Some(index),
            })
            .await?
            .into_iter()
            .find(|stream| stream.is_external)
            .and_then(|stream| stream.path)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                ServiceError::invalid_input(format!("no subtitle file at stream index {index}"))
            })?;
        // `File.Delete` ignores a file already gone, but throws
        // `DirectoryNotFoundException` (a 404, before the refresh) when its
        // folder is missing or a folder in the path is a file. The stream rows
        // are left to the item refresh the caller awaits next
        // (`item.RefreshMetadata`).
        let _write = SelfWrite::begin(self.self_writes.as_ref(), path.as_str());
        let folder_missing = |reason: &str| {
            // The 404 is not logged where it is answered; name the row here.
            tracing::debug!(
                %item_id,
                path = %ferrofin_util::source_url::redact_url(&path),
                reason,
                "subtitle folder not found"
            );
            ServiceError::not_found("subtitle folder not found")
        };
        let file = Path::new(&path);
        // `File.Delete` resolves a relative path against the working
        // directory, where a URL row's folder is missing; never touch the disk
        // for one (a relative row whose folder exists there is a divergence).
        if !file.is_absolute() {
            return Err(folder_missing("relative path"));
        }
        settle_unlink(file, tokio::fs::remove_file(file).await, folder_missing).await
    }

    async fn drop_missing_subtitle_rows(&self, item_id: Uuid) -> Result<(), ServiceError> {
        let streams = self
            .media_streams
            .get_media_streams(&MediaStreamQuery {
                item_id,
                ..MediaStreamQuery::default()
            })
            .await?;
        let subtitle = media_stream_type_disc(ferrofin_model::entities::MediaStreamType::Subtitle);
        let before = streams.len();
        let mut kept = Vec::with_capacity(before);
        for stream in streams {
            // Only an absolute path that is surely gone: a URL row, or one
            // whose file cannot be checked, stays.
            let gone = match stream.path.as_deref() {
                Some(path) if stream.is_external && stream.stream_type == subtitle => {
                    Path::new(path).is_absolute()
                        && matches!(tokio::fs::try_exists(path).await, Ok(false))
                }
                _ => false,
            };
            if !gone {
                kept.push(stream);
            }
        }
        if kept.len() == before {
            return Ok(());
        }
        self.media_streams
            .save_media_streams(item_id, &kept)
            .await?;
        // The refresh that could not probe left the flag too: a video whose
        // last subtitle went has none (`FFProbeVideoInfo.cs:270`).
        self.media_streams.sync_has_subtitles(item_id).await
    }

    async fn get_supported_providers(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<SubtitleProviderInfo>, ServiceError> {
        let Some(item) = self.library_manager.get_item_by_id(item_id).await? else {
            return Ok(Vec::new());
        };
        let Some(content_type) = subtitle_media_type(&item.type_) else {
            return Ok(Vec::new());
        };
        Ok(self
            .providers
            .iter()
            .filter(|provider| provider.supported_media_types().contains(&content_type))
            .map(|p| SubtitleProviderInfo {
                name: Some(p.name().to_owned()),
                id: Some(provider_id(p.name())),
            })
            .collect())
    }

    async fn validate_provider_login(
        &self,
        provider_name: &str,
        config_json: &[u8],
    ) -> Result<ProviderLogin, ServiceError> {
        let provider = self
            .providers
            .iter()
            .find(|p| p.name() == provider_name)
            .ok_or_else(|| ServiceError::not_found(format!("subtitle provider {provider_name}")))?;
        provider.validate_login(config_json).await
    }
}

/// Answers an `unlink` of a subtitle file as `File.Delete` does on Linux
/// (dotnet/runtime `FileSystem.Unix.cs` `DeleteFile`).
async fn settle_unlink(
    file: &Path,
    unlinked: std::io::Result<()>,
    folder_missing: impl Fn(&str) -> ServiceError,
) -> Result<(), ServiceError> {
    match unlinked {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let folder = file.parent().unwrap_or(file);
            if tokio::fs::metadata(folder).await.is_ok_and(|m| m.is_dir()) {
                Ok(())
            } else {
                Err(folder_missing("no such folder"))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => {
            Err(folder_missing("a folder in the path is a file"))
        }
        // A read-only mount answers `EROFS` before it looks the file up, so a
        // file already gone is no error when an `lstat` finds nothing either
        // (dotnet/runtime issue 22382); anything else at the path still fails.
        Err(e) if e.kind() == std::io::ErrorKind::ReadOnlyFilesystem => {
            match tokio::fs::symlink_metadata(file).await {
                Err(stat) if stat.kind() == std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(ServiceError::backend(e.to_string())),
            }
        }
        Err(e) => Err(ServiceError::backend(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use ferrofin_db::entities::base_items::MediaStreamInfoEntity;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::entities::MediaStreamType;
    use ferrofin_traits::persistence::{ItemPersistenceService as _, MediaStreamRepository};
    use ferrofin_traits::subtitles::{SubtitleProvider, SubtitleResponse, SubtitleSearchRequest};
    use uuid::Uuid;

    use crate::db_error::media_stream_type_disc;
    use crate::media_stream_repository::FerrofinMediaStreamRepository;
    use crate::test_support::{library_manager_over, seed_item, seed_item_with_data, test_db};

    use super::*;

    /// A canned provider: search returns one namespaced candidate; get_subtitles
    /// returns fixed bytes.
    struct FakeProvider;

    #[async_trait]
    impl SubtitleProvider for FakeProvider {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn search(
            &self,
            request: &SubtitleSearchRequest,
        ) -> Result<Vec<RemoteSubtitleInfo>, ServiceError> {
            Ok(vec![RemoteSubtitleInfo {
                id: Some("42".to_owned()),
                provider_name: Some("fake".to_owned()),
                name: request.name.clone(),
                ..Default::default()
            }])
        }
        async fn get_subtitles(&self, local: &str) -> Result<SubtitleResponse, ServiceError> {
            assert_eq!(local, "42");
            Ok(SubtitleResponse {
                language: "eng".to_owned(),
                format: "srt".to_owned(),
                is_forced: false,
                is_hearing_impaired: false,
                content: b"1\n00:00:00,000 --> 00:00:01,000\nhi\n".to_vec(),
            })
        }
    }

    fn manager(db: Database, providers: Vec<Arc<dyn SubtitleProvider>>) -> FerrofinSubtitleManager {
        FerrofinSubtitleManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(FerrofinMediaStreamRepository::new(db)),
            providers,
            std::env::temp_dir(),
        )
    }

    /// Points a seeded item's `Path` at `media` — the one shared setup UPDATE,
    /// so each test doesn't add its own raw query (the ferrofin-db sql_boundary
    /// ratchet counts them).
    async fn set_item_path(db: &Database, item: Uuid, media: &std::path::Path) {
        sqlx::query(r#"UPDATE "BaseItems" SET "Path" = ?1 WHERE "Id" = ?2"#)
            .bind(media.to_str().unwrap())
            .bind(guid_to_db(item))
            .execute(db.writer())
            .await
            .expect("set path");
    }

    fn subtitle_stream(index: i64, external: bool, path: Option<&str>) -> MediaStreamInfoEntity {
        MediaStreamInfoEntity {
            stream_index: index,
            codec: Some("subrip".to_owned()),
            is_external: external,
            language: Some("eng".to_owned()),
            path: path.map(str::to_owned),
            stream_type: media_stream_type_disc(MediaStreamType::Subtitle),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn search_fans_out_and_enriches() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let mgr = manager(db, vec![Arc::new(FakeProvider)]);
        let results = mgr
            .search_subtitles(&SubtitleSearchRequest {
                item_id: item,
                language: "eng".to_owned(),
                ..Default::default()
            })
            .await
            .expect("search");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, Some(format!("{}_42", provider_id("fake"))));
    }

    struct CapturingProvider(Arc<std::sync::Mutex<Vec<SubtitleSearchRequest>>>);

    #[async_trait]
    impl SubtitleProvider for CapturingProvider {
        fn name(&self) -> &'static str {
            "capture"
        }
        async fn search(
            &self,
            request: &SubtitleSearchRequest,
        ) -> Result<Vec<RemoteSubtitleInfo>, ServiceError> {
            self.0.lock().unwrap().push(request.clone());
            Ok(vec![RemoteSubtitleInfo {
                is_hash_match: Some(false),
                ..Default::default()
            }])
        }
        async fn get_subtitles(&self, _: &str) -> Result<SubtitleResponse, ServiceError> {
            unreachable!("search only")
        }
    }

    /// gl-1524: `SearchSubtitles(Video, …)` returns `[]` before any provider
    /// is asked unless the video's `VideoType` is `VideoFile`.
    #[rstest::rstest]
    #[case(r#"{"VideoType":"Dvd"}"#, false)]
    #[case(r#"{"VideoType":"BluRay"}"#, false)]
    #[case(r#"{"VideoType":"Iso","IsoType":"Dvd"}"#, false)]
    #[case(r#"{"VideoType":"VideoFile"}"#, true)]
    #[case("{}", true)]
    #[tokio::test]
    async fn search_asks_providers_only_for_a_video_file(
        #[case] data: &str,
        #[case] searched: bool,
    ) {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let mut row = library_manager_over(db.clone())
            .get_item_by_id(item)
            .await
            .unwrap()
            .unwrap();
        row.data = Some(data.to_owned());
        crate::test_support::save_item(&db, &row).await;
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mgr = manager(db, vec![Arc::new(CapturingProvider(captured.clone()))]);
        let results = mgr
            .search_subtitles(&SubtitleSearchRequest {
                item_id: item,
                language: "eng".to_owned(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(captured.lock().unwrap().len(), usize::from(searched));
        assert_eq!(results.len(), usize::from(searched));
    }

    /// gl-1462: `SearchSubtitles` hands providers the two-letter code of the
    /// culture `FindLanguageInfo` finds for the language, by any of its names.
    #[rstest::rstest]
    #[case("est", Some("et"))]
    #[case("ces", Some("cs"))]
    #[case("cze", Some("cs"))]
    #[case("slk", Some("sk"))]
    #[case("bos", Some("bs"))]
    #[case("gre", Some("el"))]
    #[case("ger", Some("de"))]
    #[case("German", Some("de"))]
    #[case("por", Some("pt"))]
    #[case("chi", Some("zh"))]
    #[case("und", Some(""))]
    #[case("xyz", None)]
    #[tokio::test]
    async fn search_gives_providers_the_two_letter_language(
        #[case] language: &str,
        #[case] expected: Option<&str>,
    ) {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mgr = manager(db, vec![Arc::new(CapturingProvider(captured.clone()))]);
        mgr.search_subtitles(&SubtitleSearchRequest {
            item_id: item,
            language: language.to_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
        let seen = captured.lock().unwrap()[0].clone();
        assert_eq!(seen.two_letter_iso_language_name.as_deref(), expected);
        assert_eq!(seen.language, language);
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn search_enriches_ids_episode_range_and_runtime_from_saved_item() {
        use crate::test_support::{save_item, seed_provider_id};
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let library = library_manager_over(db.clone());
        let mut row = library.get_item_by_id(item).await.unwrap().unwrap();
        row.name = Some("Two episodes".to_owned());
        row.series_name = Some("Saved series".to_owned());
        row.index_number = Some(2);
        row.parent_index_number = Some(1);
        row.run_time_ticks = Some(42_000_000);
        row.data = Some(r#"{"IndexNumberEnd":3}"#.to_owned());
        save_item(&db, &row).await;
        for (key, value) in [
            ("IMDB", "tt1234567"),
            ("Tvdb", "321"),
            ("Custom", "unchanged"),
        ] {
            seed_provider_id(&db, item, key, value).await;
        }
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mgr = manager(
            db.clone(),
            vec![Arc::new(CapturingProvider(captured.clone()))],
        );
        let request = SubtitleSearchRequest {
            item_id: item,
            language: "eng".to_owned(),
            is_perfect_match: Some(true),
            imdb_id: Some("stale".to_owned()),
            index_number_end: Some(99),
            provider_ids: std::collections::HashMap::from([("Stale".to_owned(), "old".to_owned())]),
            ..Default::default()
        };
        assert!(
            mgr.search_subtitles(&request).await.unwrap().is_empty(),
            "perfect matching still rejects this candidate"
        );
        let seen = captured.lock().unwrap()[0].clone();
        assert_eq!(seen.imdb_id.as_deref(), Some("tt1234567"));
        assert_eq!(seen.provider_ids.len(), 3);
        assert_eq!(seen.provider_ids["Tvdb"], "321");
        assert_eq!(seen.provider_ids["Custom"], "unchanged");
        assert_eq!(seen.index_number, Some(2));
        assert_eq!(seen.index_number_end, Some(3));
        assert_eq!(seen.parent_index_number, Some(1));
        assert_eq!(seen.runtime_ticks, Some(42_000_000));
        assert_eq!(seen.is_perfect_match, Some(true));
        assert_eq!(seen.language, "eng");
        // Missing/invalid range data clears a stale caller value; movies never
        // acquire an episode range, even with an adopted stray Data property.
        for (kind, data) in [
            (BaseItemKind::Episode, "{}"),
            (BaseItemKind::Episode, r#"{"IndexNumberEnd":2147483648}"#),
            (BaseItemKind::Episode, "invalid JSON"),
            (BaseItemKind::Movie, r#"{"IndexNumberEnd":3}"#),
        ] {
            row.type_ = crate::item_type_lookup::stored_type_name(kind)
                .unwrap()
                .to_owned();
            row.data = Some(data.to_owned());
            row.run_time_ticks = None;
            save_item(&db, &row).await;
            mgr.search_subtitles(&request).await.unwrap();
            let seen = captured.lock().unwrap().last().unwrap().clone();
            assert_eq!(seen.index_number_end, None);
            assert_eq!(seen.runtime_ticks, None);
        }
        crate::item_persistence_service::FerrofinItemPersistenceService::new(db)
            .replace_provider_ids(item, &[])
            .await
            .unwrap();
        mgr.search_subtitles(&request).await.unwrap();
        let seen = captured.lock().unwrap().last().unwrap().clone();
        assert!(seen.provider_ids.is_empty());
        assert_eq!(
            seen.imdb_id, None,
            "absent saved IDs clear stale caller metadata"
        );
    }

    /// `TrySaveSubtitle` writes the file and nothing else: the stream is the
    /// queued refresh's probe to record, in the probe's order and with its
    /// codec (gl-1641).
    #[tokio::test]
    async fn download_writes_the_sidecar_and_leaves_the_stream_to_the_refresh() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        // Give the item a media path so the sidecar has a home.
        let tmp = tempfile::tempdir().expect("tempdir");
        let media = tmp.path().join("Movie.mkv");
        std::fs::write(&media, b"x").expect("media");
        set_item_path(&db, item, &media).await;
        let repo = FerrofinMediaStreamRepository::new(db.clone());
        let probed = vec![MediaStreamInfoEntity {
            stream_index: 0,
            codec: Some("h264".to_owned()),
            stream_type: media_stream_type_disc(MediaStreamType::Video),
            ..Default::default()
        }];
        repo.save_media_streams(item, &probed).await.expect("seed");
        let all = MediaStreamQuery {
            item_id: item,
            stream_type: None,
            index: None,
        };
        let before = repo.get_media_streams(&all).await.expect("streams");

        let mgr = manager(db.clone(), vec![Arc::new(FakeProvider)]);
        mgr.download_subtitles(item, &format!("{}_42", provider_id("fake")))
            .await
            .expect("download");

        let sidecar = tmp.path().join("Movie.eng.srt");
        assert!(sidecar.exists(), "sidecar written");
        let after = repo.get_media_streams(&all).await.expect("streams");
        assert_eq!(after, before, "the download must not record a stream");
        let data = crate::test_support::fetch_item(&db, item).await.data;
        assert!(
            crate::item_data::parse_data(data.as_deref())
                .get("HasSubtitles")
                .is_none(),
            "{data:?}"
        );
    }

    /// A provider whose downloads fail: `quota` with the plugin's quota error,
    /// `nfo` with content in a format no sidecar takes, anything else with a
    /// refused account.
    struct FailingProvider;

    #[async_trait]
    impl SubtitleProvider for FailingProvider {
        fn name(&self) -> &'static str {
            "Failing"
        }
        async fn search(
            &self,
            _: &SubtitleSearchRequest,
        ) -> Result<Vec<RemoteSubtitleInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_subtitles(&self, local: &str) -> Result<SubtitleResponse, ServiceError> {
            match local {
                "quota" => Err(ServiceError::backend_source(
                    ferrofin_traits::subtitles::RateLimitExceeded("quota spent".to_owned()),
                )),
                "nfo" => Ok(SubtitleResponse {
                    language: "eng".to_owned(),
                    format: "nfo".to_owned(),
                    ..Default::default()
                }),
                _ => Err(ServiceError::unauthorized("account not set up")),
            }
        }
    }

    /// `SubtitleManager.DownloadSubtitles` raises `SubtitleDownloadFailure`
    /// for a failed fetch or save, and `SubtitleDownloadFailureLogger` records
    /// it; a spent quota (`catch (RateLimitExceededException) { throw; }`) and
    /// an id naming no provider (`GetProvider`, before the `try`) record
    /// nothing.
    #[tokio::test]
    async fn a_failed_download_records_an_activity_entry_but_a_spent_quota_does_not() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        let tmp = tempfile::tempdir().expect("tempdir");
        let media = tmp.path().join("Pilot.mkv");
        std::fs::write(&media, b"x").expect("media");
        crate::test_support::save_item(
            &db,
            &ferrofin_db::entities::base_items::BaseItemEntity {
                id: guid_to_db(item),
                type_: "MediaBrowser.Controller.Entities.TV.Episode".to_owned(),
                name: Some("Pilot".to_owned()),
                series_name: Some("Show".to_owned()),
                parent_index_number: Some(1),
                index_number: Some(2),
                path: Some(media.to_string_lossy().into_owned()),
                ..Default::default()
            },
        )
        .await;
        let activity = Arc::new(crate::FerrofinActivityManager::new(db.clone()));
        let mgr = manager(db, vec![Arc::new(FailingProvider)]).with_activity(activity.clone());
        let namespace = provider_id("Failing");

        for local in ["auth", "quota", "nfo"] {
            mgr.download_subtitles(item, &format!("{namespace}_{local}"))
                .await
                .expect_err(local);
        }
        mgr.download_subtitles(item, "nobody_auth")
            .await
            .expect_err("no provider");

        let mut entries = activity
            .get_paged_result(&ferrofin_traits::activity::ActivityLogQuery::default())
            .await
            .expect("entries")
            .items;
        entries.sort_by_key(|entry| entry.id);
        let overviews: Vec<_> = entries
            .iter()
            .map(|entry| entry.short_overview.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(
            overviews,
            ["account not set up", "unsupported subtitle format: nfo"]
        );
        for entry in &entries {
            assert_eq!(
                entry.name,
                "Subtitles failed to download from Failing for Show - S1, Ep2 - Pilot"
            );
            assert_eq!(entry.type_, "SubtitleDownloadFailure");
            assert_eq!(entry.item_id, Some(item.simple().to_string()));
            assert_eq!(entry.user_id, Uuid::nil());
            assert_eq!(
                entry.severity,
                ferrofin_model::activity::LogLevel::Information
            );
        }
    }

    /// `SubtitleDownloadFailureLogger.GetItemName` for the videos a subtitle
    /// downloads for.
    #[rstest::rstest]
    #[case::movie("Movie", Some("Dune"), None, None, None, None, "Dune")]
    #[case::episode(
        "TV.Episode",
        Some("Pilot"),
        Some("Show"),
        Some(1),
        Some(2),
        None,
        "Show - S1, Ep2 - Pilot"
    )]
    #[case::episode_without_numbers_or_series(
        "TV.Episode",
        Some("Pilot"),
        None,
        None,
        None,
        None,
        " - Pilot"
    )]
    #[case::music_video(
        "MusicVideo",
        Some("Song"),
        None,
        None,
        None,
        Some("Band|Guest"),
        "Band - Song"
    )]
    #[case::music_video_without_artists(
        "MusicVideo",
        Some("Song"),
        None,
        None,
        None,
        Some(""),
        "Song"
    )]
    fn failure_entries_name_the_item_as_upstream(
        #[case] kind: &str,
        #[case] name: Option<&str>,
        #[case] series_name: Option<&str>,
        #[case] season: Option<i64>,
        #[case] episode: Option<i64>,
        #[case] artists: Option<&str>,
        #[case] expected: &str,
    ) {
        let item = ferrofin_db::entities::base_items::BaseItemEntity {
            type_: format!("MediaBrowser.Controller.Entities.{kind}"),
            name: name.map(str::to_owned),
            series_name: series_name.map(str::to_owned),
            parent_index_number: season,
            index_number: episode,
            artists: artists.map(str::to_owned),
            ..Default::default()
        };
        assert_eq!(item_name(&item), expected);
    }

    async fn configured_library(
        media: &Path,
        save_with_media: bool,
    ) -> (
        Arc<crate::virtual_folder_manager::FerrofinVirtualFolderManager>,
        ferrofin_model::configuration::LibraryOptions,
    ) {
        use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};

        let folders = Arc::new(
            crate::virtual_folder_manager::FerrofinVirtualFolderManager::new(media.join("views")),
        );
        let options = LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: media.to_string_lossy().into_owned(),
            }],
            save_subtitles_with_media: save_with_media,
            ..Default::default()
        };
        folders
            .add_virtual_folder("Movies", None, &options)
            .await
            .expect("library");
        (folders, options)
    }

    fn upload_response() -> SubtitleResponse {
        SubtitleResponse {
            language: "ENG".to_owned(),
            format: "SRT".to_owned(),
            is_forced: false,
            is_hearing_impaired: false,
            content: b"1\n00:00:00,000 --> 00:00:01,000\nhi\n".to_vec(),
        }
    }

    #[test]
    fn sidecar_name_rejects_invalid_input_and_preserves_upstream_names() {
        let mut response = upload_response();
        for format in ["", ".VTT", "nfo", "strm", "srt ", "../srt"] {
            response.format = format.to_owned();
            assert!(matches!(
                sidecar_name("/library/movie/Movie.mkv", &response),
                Err(ServiceError::InvalidInput(_))
            ));
        }
        response.format = "srt".to_owned();
        for language in ["x/../../outside/pwned", "eng\0"] {
            response.language = language.to_owned();
            assert!(matches!(
                sidecar_name("/library/movie/Movie.mkv", &response),
                Err(ServiceError::InvalidInput(_))
            ));
        }
        response.language = "pt-BR".to_owned();
        response.is_forced = true;
        response.is_hearing_impaired = true;
        for extension in SUBTITLE_EXTENSIONS {
            response.format = extension.to_ascii_uppercase();
            assert_eq!(
                sidecar_name("/library/movie/Movie.mkv", &response).unwrap(),
                format!("Movie.pt-br.forced.sdh.{extension}")
            );
        }
        response.language.clear();
        response.format = "srt".to_owned();
        response.is_forced = false;
        response.is_hearing_impaired = false;
        assert_eq!(
            sidecar_name("/library/movie/Movie.mkv", &response).unwrap(),
            "Movie..srt"
        );
    }

    /// `TrySaveSubtitle` writes into `Video.ContainingFolderPath`: a DVD or
    /// Blu-ray folder rip's own folder, else the folder of the file (of the
    /// first part, for a stacked rip; a `.disc` placeholder's too).
    #[rstest::rstest]
    #[case::dvd_rip("Film", r#"{"VideoType":"Dvd"}"#, "Film/Film.eng.srt")]
    #[case::bluray_rip("Film", r#"{"VideoType":"BluRay"}"#, "Film/Film.eng.srt")]
    #[case::iso("Film/Film.iso", r#"{"VideoType":"Iso"}"#, "Film/Film.eng.srt")]
    #[case::stacked_rip(
        "Set/Set - Disc 1",
        r#"{"VideoType":"Dvd","AdditionalParts":["Set/Set - Disc 2"]}"#,
        "Set/Set - Disc 1.eng.srt"
    )]
    #[case::placeholder(
        "Film/Film.disc",
        r#"{"VideoType":"Dvd","IsPlaceHolder":true}"#,
        "Film/Film.eng.srt"
    )]
    #[tokio::test]
    async fn a_subtitle_is_saved_into_the_videos_containing_folder(
        #[case] media: &str,
        #[case] data: &str,
        #[case] expected: &str,
    ) {
        let db = test_db().await;
        let item = Uuid::new_v4();
        crate::test_support::seed_item_with_data(&db, item, BaseItemKind::Movie, "Film", data)
            .await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let media = tmp.path().join(media);
        std::fs::create_dir_all(media.parent().unwrap()).unwrap();
        set_item_path(&db, item, &media).await;
        let response = upload_response();
        manager(db, vec![])
            .upload_subtitle(item, &response)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join(expected)).unwrap(),
            response.content
        );
    }

    /// A root path has no containing folder, so nothing is written (never
    /// into the working directory).
    #[tokio::test]
    async fn a_root_path_has_no_folder_for_a_sidecar() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        set_item_path(&db, item, Path::new("/")).await;
        assert!(matches!(
            manager(db, vec![])
                .upload_subtitle(item, &upload_response())
                .await,
            Err(ServiceError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn invalid_uploads_never_write_or_record_streams() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let media = tmp.path().join("Movie.mkv");
        std::fs::write(&media, b"media").unwrap();
        set_item_path(&db, item, &media).await;
        let mgr = manager(db.clone(), vec![]);
        let mut response = upload_response();
        response.language = "x/../../outside/pwned".to_owned();
        assert!(matches!(
            mgr.upload_subtitle(item, &response).await,
            Err(ServiceError::InvalidInput(_))
        ));
        response.language = "eng".to_owned();
        response.format = "strm".to_owned();
        assert!(matches!(
            mgr.upload_subtitle(item, &response).await,
            Err(ServiceError::InvalidInput(_))
        ));
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
        let media_streams = FerrofinMediaStreamRepository::new(db)
            .get_media_streams(&MediaStreamQuery {
                item_id: item,
                stream_type: None,
                index: None,
            })
            .await
            .unwrap();
        assert!(media_streams.is_empty(), "{media_streams:?}");
    }

    #[tokio::test]
    async fn uploaded_image_subtitles_and_containers_are_saved_for_the_probe() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let media = tmp.path().join("Movie.mkv");
        std::fs::write(&media, b"media").unwrap();
        set_item_path(&db, item, &media).await;
        let mgr = manager(db.clone(), vec![]);
        let mut response = upload_response();
        response.format = "SUP".to_owned();
        // The upload writer validates extension/destination rather than parsing
        // content; the subsequent refresh will probe these external bytes.
        response.content = b"PG".to_vec();
        mgr.upload_subtitle(item, &response).await.unwrap();
        response.format = "MKS".to_owned();
        mgr.upload_subtitle(item, &response).await.unwrap();
        for name in ["Movie.eng.sup", "Movie.eng.mks"] {
            assert_eq!(
                std::fs::read(tmp.path().join(name)).unwrap(),
                b"PG",
                "{name}"
            );
        }
        let streams = FerrofinMediaStreamRepository::new(db)
            .get_media_streams(&MediaStreamQuery {
                item_id: item,
                stream_type: None,
                index: None,
            })
            .await
            .unwrap();
        assert!(streams.is_empty(), "{streams:?}");
    }

    #[tokio::test]
    async fn unwritable_media_destination_returns_error_without_metadata_retry() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let blocked = tmp.path().join("locked");
        std::fs::write(&blocked, b"file").unwrap();
        set_item_path(&db, item, &blocked.join("Movie.mkv")).await;
        let meta = tempfile::tempdir().expect("metadata");
        let mgr = FerrofinSubtitleManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(FerrofinMediaStreamRepository::new(db.clone())),
            vec![],
            meta.path().to_path_buf(),
        );
        assert!(matches!(
            mgr.upload_subtitle(item, &upload_response()).await,
            Err(ServiceError::Backend(_))
        ));
        assert!(!mgr.item_metadata_dir(item).exists());
        let media_streams = FerrofinMediaStreamRepository::new(db)
            .get_media_streams(&MediaStreamQuery {
                item_id: item,
                stream_type: None,
                index: None,
            })
            .await
            .unwrap();
        assert!(media_streams.is_empty(), "{media_streams:?}");
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn live_library_destination_applies_to_uploads_and_downloads_without_overwriting() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let media = tmp.path().join("Movie.mkv");
        std::fs::write(&media, b"media").unwrap();
        set_item_path(&db, item, &media).await;
        // An adopted item's TopParentId is its physical folder, not the virtual
        // collection-folder id. Its configured library must be found by path.
        let library = library_manager_over(db.clone());
        let mut row = library.get_item_by_id(item).await.unwrap().unwrap();
        row.top_parent_id = Some(guid_to_db(Uuid::new_v4()));
        crate::item_persistence_service::FerrofinItemPersistenceService::new(db.clone())
            .save_items(&[row])
            .await
            .unwrap();
        let (folders, mut options) = configured_library(tmp.path(), true).await;
        let meta = tempfile::tempdir().expect("metadata");
        let mgr = FerrofinSubtitleManager::new(
            db.clone(),
            library,
            Arc::new(FerrofinMediaStreamRepository::new(db.clone())),
            vec![Arc::new(FakeProvider)],
            meta.path().to_path_buf(),
        )
        .with_virtual_folders(folders.clone());
        let response = upload_response();
        let original = tmp.path().join("Movie.eng.srt");
        std::fs::write(&original, b"original").unwrap();
        mgr.upload_subtitle(item, &response).await.unwrap();
        mgr.download_subtitles(item, &format!("{}_42", provider_id("fake")))
            .await
            .unwrap();
        for name in ["Movie.eng.0.srt", "Movie.eng.1.srt"] {
            assert_eq!(
                std::fs::read(tmp.path().join(name)).unwrap(),
                response.content
            );
        }
        assert_eq!(std::fs::read(&original).unwrap(), b"original");
        assert!(!mgr.item_metadata_dir(item).exists());

        options.save_subtitles_with_media = false;
        folders
            .update_library_options("Movies", &options)
            .await
            .unwrap();
        mgr.upload_subtitle(item, &response).await.unwrap();
        mgr.download_subtitles(item, &format!("{}_42", provider_id("fake")))
            .await
            .unwrap();
        let internal = mgr.item_metadata_dir(item);
        for name in ["Movie.eng.srt", "Movie.eng.0.srt"] {
            assert_eq!(
                std::fs::read(internal.join(name)).unwrap(),
                response.content
            );
        }
        assert!(!tmp.path().join("Movie.eng.2.srt").exists());

        options.save_subtitles_with_media = true;
        folders
            .update_library_options("Movies", &options)
            .await
            .unwrap();
        mgr.upload_subtitle(item, &response).await.unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("Movie.eng.2.srt")).unwrap(),
            response.content
        );
        assert_eq!(std::fs::read(&original).unwrap(), b"original");
        assert!(
            internal.join("Movie.eng.srt").is_file(),
            "toggle preserves earlier subtitles"
        );
        let streams = FerrofinMediaStreamRepository::new(db)
            .get_media_streams(&MediaStreamQuery {
                item_id: item,
                stream_type: None,
                index: None,
            })
            .await
            .unwrap();
        assert!(streams.is_empty(), "the refresh records them: {streams:?}");
    }

    /// A [`SelfWriteGuard`] recording each call, in order.
    #[derive(Default)]
    struct Recording(std::sync::Mutex<Vec<String>>);

    impl ferrofin_traits::library::SelfWriteGuard for Recording {
        fn begin(&self, path: &str) {
            self.0.lock().unwrap().push(format!("begin {path}"));
        }
        fn complete(&self, path: &str, refresh_path: bool) {
            self.0
                .lock()
                .unwrap()
                .push(format!("complete {path} {refresh_path}"));
        }
    }

    impl Recording {
        fn calls(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    /// `TrySaveToFiles` brackets the subtitle file it writes
    /// (`SubtitleManager.cs:270-303`): the free name it settles on, not a
    /// name already taken.
    #[tokio::test]
    async fn a_sidecar_write_brackets_each_name_it_tries() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("Movie.eng.srt");
        std::fs::write(&path, b"taken").expect("existing");
        let recording = std::sync::Arc::new(Recording::default());
        let guard: std::sync::Arc<dyn ferrofin_traits::library::SelfWriteGuard> = recording.clone();
        let written = write_sidecar(&path, b"new", Some(&guard))
            .await
            .expect("written");
        let next = tmp.path().join("Movie.eng.0.srt");
        assert_eq!(written, next);
        let next = next.display();
        assert_eq!(
            recording.calls(),
            [format!("begin {next}"), format!("complete {next} false")],
            "only the free name it writes"
        );
    }

    #[tokio::test]
    async fn concurrent_sidecar_writes_do_not_replace_existing_bytes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("Movie.eng.srt");
        let (first, second) = tokio::join!(
            write_sidecar(&path, b"first", None),
            write_sidecar(&path, b"second", None)
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(first).unwrap(), b"first");
        assert_eq!(std::fs::read(second).unwrap(), b"second");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 2);
        let blocked = tmp.path().join("Movie.fra.srt");
        std::fs::create_dir(&blocked).unwrap();
        assert!(write_sidecar(&blocked, b"content", None).await.is_err());
        assert!(!tmp.path().join("Movie.fra.0.srt").exists());
    }
    #[tokio::test]
    async fn metadata_destination_uses_live_root_with_an_unwritable_media_folder() {
        // Selecting internal metadata must not attempt a media-folder write.
        // The invalid media parent makes an unwanted sidecar write fail on any uid.
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let not_a_dir = tmp.path().join("locked");
        std::fs::write(&not_a_dir, b"x").expect("file");
        let media = not_a_dir.join("Movie.mkv"); // parent is a file → unwritable
        set_item_path(&db, item, &media).await;

        let (folders, _) = configured_library(tmp.path(), false).await;
        let meta = tempfile::tempdir().expect("meta tempdir");
        let current = Arc::new(std::sync::RwLock::new(meta.path().to_path_buf()));
        let source = Arc::clone(&current);
        let mgr = FerrofinSubtitleManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(FerrofinMediaStreamRepository::new(db.clone())),
            vec![],
            DirectoryPath::live(move || source.read().unwrap().clone()),
        )
        .with_virtual_folders(folders);

        let resp = SubtitleResponse {
            language: "eng".to_owned(),
            format: "srt".to_owned(),
            is_forced: false,
            is_hearing_impaired: false,
            content: b"1\n00:00:00,000 --> 00:00:01,000\nParity\n".to_vec(),
        };
        mgr.upload_subtitle(item, &resp)
            .await
            .expect("upload should use the selected metadata destination");

        let dashless = item.simple().to_string();
        let expected = meta
            .path()
            .join("library")
            .join(&dashless[..2])
            .join(&dashless)
            .join("Movie.eng.srt");
        assert!(
            expected.exists(),
            "subtitle written to configured metadata destination"
        );

        let replacement = tempfile::tempdir().expect("new metadata");
        *current.write().unwrap() = replacement.path().to_path_buf();
        mgr.upload_subtitle(item, &resp)
            .await
            .expect("upload after change");
        let changed = replacement
            .path()
            .join("library")
            .join(&dashless[..2])
            .join(&dashless)
            .join("Movie.eng.srt");
        assert_eq!(std::fs::read(&changed).unwrap(), resp.content);
        assert!(expected.is_file(), "previous metadata is not relocated");
    }

    #[tokio::test]
    async fn get_remote_routes_by_prefix() {
        let db = test_db().await;
        let mgr = manager(db, vec![Arc::new(FakeProvider)]);
        let resp = mgr
            .get_remote_subtitles(&format!("{}_42", provider_id("fake")))
            .await
            .expect("remote");
        assert_eq!(resp.format, "srt");
        assert!(matches!(
            mgr.get_remote_subtitles("unknown_1").await,
            Err(ServiceError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn supported_providers_lists_registry() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let mgr = manager(db, vec![Arc::new(FakeProvider)]);
        let providers = mgr.get_supported_providers(item).await.expect("providers");
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].name.as_deref(), Some("fake"));
        assert_eq!(
            providers[0].id.as_deref(),
            Some("fb54b3c3796fa82a7cd4be5f371d176c")
        );
    }

    #[test]
    fn provider_id_is_dotnet_md5_of_lowercase_name() {
        // .NET: "opensubtitles".ToLowerInvariant().GetMD5().ToString("N")
        assert_eq!(
            provider_id("OpenSubtitles"),
            "29c0c3633fefe8ab4d5b9af1b0275f3d"
        );
        assert_eq!(provider_id("opensubtitles"), provider_id("OPENSUBTITLES"));
    }

    #[tokio::test]
    async fn routing_uses_provider_id_and_keeps_later_underscores() {
        let db = test_db().await;
        let mgr = manager(db, vec![Arc::new(FakeProvider)]);
        // The readable-name prefix is not a namespace.
        assert!(mgr.get_remote_subtitles("fake_42").await.is_err());
        // Only the first underscore splits; the rest belongs to the local id.
        let routed = mgr.route(&format!("{}_a_b", provider_id("fake")));
        assert_eq!(routed.map(|(_, local)| local).as_deref(), Some("a_b"));
        // No underscore: the whole id is both provider id and local id.
        assert!(mgr.route(&provider_id("fake")).is_some());
    }

    struct RankedProvider(&'static str, bool);

    #[async_trait]
    impl SubtitleProvider for RankedProvider {
        fn name(&self) -> &'static str {
            self.0
        }
        async fn search(
            &self,
            _: &SubtitleSearchRequest,
        ) -> Result<Vec<RemoteSubtitleInfo>, ServiceError> {
            Ok(vec![RemoteSubtitleInfo {
                id: Some("1".to_owned()),
                is_hash_match: Some(self.1),
                ..Default::default()
            }])
        }
        async fn get_subtitles(&self, _: &str) -> Result<SubtitleResponse, ServiceError> {
            unreachable!("search only")
        }
    }

    #[tokio::test]
    async fn automatic_search_honors_order_disabling_and_perfect_match() {
        let mgr = manager(
            test_db().await,
            vec![
                Arc::new(RankedProvider("first", false)),
                Arc::new(RankedProvider("second", true)),
            ],
        );
        let mut request = SubtitleSearchRequest {
            is_automated: true,
            search_all_providers: Some(false),
            subtitle_fetcher_order: vec!["second".to_owned(), "first".to_owned()],
            ..Default::default()
        };
        let results = mgr.search_subtitles(&request).await.unwrap();
        assert_eq!(
            results.len(),
            1,
            "stop after the preferred provider answers"
        );
        assert_eq!(results[0].id, Some(format!("{}_1", provider_id("second"))));
        request.subtitle_fetcher_order.clear();
        request.is_perfect_match = Some(true);
        let results = mgr.search_subtitles(&request).await.unwrap();
        assert_eq!(
            results[0].id,
            Some(format!("{}_1", provider_id("second"))),
            "skip a nonmatching provider result"
        );
        request.disabled_subtitle_fetchers = vec!["SECOND".to_owned()];
        let subtitles = mgr.search_subtitles(&request).await.unwrap();
        assert!(subtitles.is_empty(), "{subtitles:?}");
        request.is_perfect_match = None;
        request.disabled_subtitle_fetchers.clear();
        request.is_automated = false;
        request.search_all_providers = None;
        assert_eq!(
            mgr.search_subtitles(&request).await.unwrap().len(),
            2,
            "interactive searches retain provider fan-out"
        );
    }

    #[derive(Clone, Copy)]
    enum SearchOutcome {
        Failed,
        Empty,
        Found,
    }

    struct SelectionProvider {
        name: &'static str,
        kinds: &'static [SubtitleMediaType],
        intrinsic_order: i32,
        outcome: SearchOutcome,
        calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl SubtitleProvider for SelectionProvider {
        fn name(&self) -> &'static str {
            self.name
        }
        fn supported_media_types(&self) -> &'static [SubtitleMediaType] {
            self.kinds
        }
        fn order(&self) -> i32 {
            self.intrinsic_order
        }
        async fn search(
            &self,
            _: &SubtitleSearchRequest,
        ) -> Result<Vec<RemoteSubtitleInfo>, ServiceError> {
            self.calls.lock().unwrap().push(self.name);
            match self.outcome {
                SearchOutcome::Failed => Err(ServiceError::backend("fixture provider failed")),
                SearchOutcome::Empty => Ok(Vec::new()),
                SearchOutcome::Found => Ok(vec![RemoteSubtitleInfo {
                    id: Some("1".to_owned()),
                    ..Default::default()
                }]),
            }
        }
        async fn get_subtitles(&self, _: &str) -> Result<SubtitleResponse, ServiceError> {
            unreachable!("selection test searches only")
        }
    }

    #[tokio::test]
    async fn saved_provider_selection_preserves_exact_order_and_best_effort_fallback() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let providers: Vec<Arc<dyn SubtitleProvider>> = [
            ("Fallback", 5, SearchOutcome::Found),
            ("Movies", 0, SearchOutcome::Found),
            ("Failed", -2, SearchOutcome::Failed),
            ("Empty", -1, SearchOutcome::Empty),
        ]
        .into_iter()
        .map(|(name, intrinsic_order, outcome)| {
            Arc::new(SelectionProvider {
                name,
                kinds: &[SubtitleMediaType::Movie],
                intrinsic_order,
                outcome,
                calls: calls.clone(),
            }) as Arc<dyn SubtitleProvider>
        })
        .collect();
        let mgr = manager(test_db().await, providers);
        let mut request = SubtitleSearchRequest {
            search_all_providers: Some(false),
            subtitle_fetcher_order: vec!["MOVIES".to_owned(), "Fallback".to_owned()],
            ..Default::default()
        };
        let results = mgr.search_subtitles(&request).await.unwrap();
        assert_eq!(
            results[0].id,
            Some(format!("{}_1", provider_id("Fallback"))),
            "saved provider names use exact casing"
        );
        assert_eq!(*calls.lock().unwrap(), ["Fallback"]);
        calls.lock().unwrap().clear();
        request.disabled_subtitle_fetchers = vec!["FALLBACK".to_owned()];
        let results = mgr.search_subtitles(&request).await.unwrap();
        assert_eq!(results[0].id, Some(format!("{}_1", provider_id("Movies"))));
        assert_eq!(
            *calls.lock().unwrap(),
            ["Failed", "Empty", "Movies"],
            "failed and empty providers fall through in intrinsic order"
        );
        calls.lock().unwrap().clear();
        request.disabled_subtitle_fetchers.clear();
        request.subtitle_fetcher_order = vec!["Movies".to_owned(), "Failed".to_owned()];
        assert_eq!(
            mgr.search_subtitles(&request).await.unwrap()[0].id,
            Some(format!("{}_1", provider_id("Movies")))
        );
        assert_eq!(*calls.lock().unwrap(), ["Movies"]);
        calls.lock().unwrap().clear();
        request.disabled_subtitle_fetchers = vec![
            "movies".to_owned(),
            "fallback".to_owned(),
            "failed".to_owned(),
            "empty".to_owned(),
        ];
        let subtitles = mgr.search_subtitles(&request).await.unwrap();
        assert!(subtitles.is_empty(), "{subtitles:?}");
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn provider_support_filters_searches_and_dashboard_descriptors() {
        let db = test_db().await;
        let movie = Uuid::new_v4();
        let episode = Uuid::new_v4();
        let audio = Uuid::new_v4();
        for (id, kind) in [
            (movie, BaseItemKind::Movie),
            (episode, BaseItemKind::Episode),
            (audio, BaseItemKind::Audio),
        ] {
            seed_item(&db, id, kind).await;
        }
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let providers: Vec<Arc<dyn SubtitleProvider>> = [
            ("Movies", &[SubtitleMediaType::Movie][..]),
            ("Episodes", &[SubtitleMediaType::Episode][..]),
        ]
        .into_iter()
        .map(|(name, kinds)| {
            Arc::new(SelectionProvider {
                name,
                kinds,
                intrinsic_order: 0,
                outcome: SearchOutcome::Found,
                calls: calls.clone(),
            }) as Arc<dyn SubtitleProvider>
        })
        .collect();
        let mgr = manager(db, providers);
        for (id, expected) in [(movie, "Movies"), (episode, "Episodes")] {
            let request = SubtitleSearchRequest {
                item_id: id,
                ..Default::default()
            };
            let results = mgr.search_subtitles(&request).await.unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].id, Some(format!("{}_1", provider_id(expected))));
            let descriptors = mgr.get_supported_providers(id).await.unwrap();
            assert_eq!(descriptors.len(), 1);
            assert_eq!(descriptors[0].name.as_deref(), Some(expected));
        }
        let subtitles = mgr
            .search_subtitles(&SubtitleSearchRequest {
                item_id: audio,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(subtitles.is_empty(), "{subtitles:?}");
        let supported_providers = mgr.get_supported_providers(audio).await.unwrap();
        assert!(supported_providers.is_empty(), "{supported_providers:?}");
        let supported_providers = mgr.get_supported_providers(Uuid::new_v4()).await.unwrap();
        assert!(supported_providers.is_empty(), "{supported_providers:?}");
        assert_eq!(*calls.lock().unwrap(), ["Movies", "Episodes"]);
    }

    struct ConcurrentProvider {
        name: &'static str,
        barrier: Arc<tokio::sync::Barrier>,
    }

    #[async_trait]
    impl SubtitleProvider for ConcurrentProvider {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn search(
            &self,
            _: &SubtitleSearchRequest,
        ) -> Result<Vec<RemoteSubtitleInfo>, ServiceError> {
            self.barrier.wait().await;
            Ok(vec![RemoteSubtitleInfo {
                id: Some("1".to_owned()),
                ..Default::default()
            }])
        }
        async fn get_subtitles(&self, _: &str) -> Result<SubtitleResponse, ServiceError> {
            unreachable!("concurrent search test")
        }
    }

    #[tokio::test]
    async fn all_provider_search_runs_concurrently_and_is_independent_of_automation() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let providers: Vec<Arc<dyn SubtitleProvider>> = ["Alpha", "Beta"]
            .into_iter()
            .map(|name| {
                Arc::new(ConcurrentProvider {
                    name,
                    barrier: barrier.clone(),
                }) as Arc<dyn SubtitleProvider>
            })
            .collect();
        let mgr = manager(test_db().await, providers);
        let request = SubtitleSearchRequest {
            is_automated: true,
            subtitle_fetcher_order: vec!["Beta".to_owned(), "Alpha".to_owned()],
            ..Default::default()
        };
        let results = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            mgr.search_subtitles(&request),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            results
                .iter()
                .map(|result| result.id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            [
                format!("{}_1", provider_id("Beta")),
                format!("{}_1", provider_id("Alpha"))
            ]
        );
        let mgr = manager(
            test_db().await,
            vec![
                Arc::new(RankedProvider("first", true)),
                Arc::new(RankedProvider("second", true)),
            ],
        );
        let request = SubtitleSearchRequest {
            is_automated: false,
            search_all_providers: Some(false),
            ..Default::default()
        };
        assert_eq!(
            mgr.search_subtitles(&request).await.unwrap().len(),
            1,
            "interactive request may explicitly stop after its first provider"
        );
    }

    #[tokio::test]
    async fn search_with_no_providers_is_empty() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let mgr = manager(db, Vec::new());
        let subtitles = mgr
            .search_subtitles(&SubtitleSearchRequest {
                item_id: item,
                ..Default::default()
            })
            .await
            .expect("search");
        assert!(subtitles.is_empty(), "{subtitles:?}");
    }

    /// The stream rows stay: the refresh the handler awaits re-probes them, as
    /// upstream's `DeleteSubtitles` leaves them to `item.RefreshMetadata`.
    #[tokio::test]
    async fn delete_removes_the_sidecar_and_leaves_the_rows_to_the_refresh() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = FerrofinMediaStreamRepository::new(db.clone());

        let tmp = tempfile::tempdir().expect("tempdir");
        let sidecar = tmp.path().join("movie.eng.srt");
        std::fs::write(&sidecar, b"1\n").expect("write sidecar");

        repo.save_media_streams(
            item,
            &[
                subtitle_stream(2, true, Some(sidecar.to_str().unwrap())),
                subtitle_stream(3, true, None),
            ],
        )
        .await
        .expect("save streams");

        let recording = Arc::new(Recording::default());
        let mgr = manager(db.clone(), Vec::new()).with_self_writes(recording.clone());
        mgr.delete_subtitles(item, 2).await.expect("delete idx 2");

        assert!(!sidecar.exists(), "sidecar file should be removed");
        let shown = sidecar.display();
        assert_eq!(
            recording.calls(),
            [format!("begin {shown}"), format!("complete {shown} false")],
            "the delete is bracketed (`SubtitleManager.cs:392-400`)"
        );
        let remaining = repo
            .get_media_streams(&MediaStreamQuery {
                item_id: item,
                stream_type: Some(MediaStreamType::Subtitle),
                index: None,
            })
            .await
            .expect("remaining");
        let indexes: Vec<_> = remaining.iter().map(|s| s.stream_index).collect();
        assert_eq!(indexes, [2, 3]);
    }

    /// gl-1581: a sidecar in the metadata directory is stored `%MetadataPath%/…`,
    /// and the delete removes the file it points at, read through the stream
    /// repository (`RestorePath`), not the stored text. The row is left to the
    /// refresh.
    #[tokio::test]
    async fn delete_removes_a_sidecar_stored_tokenised() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = crate::app_paths::test_paths(tmp.path());
        let metadata =
            ferrofin_traits::system::ServerApplicationPaths::internal_metadata_path(&*paths);
        let repo = Arc::new(
            FerrofinMediaStreamRepository::new(db.clone()).with_virtual_paths(
                crate::virtual_paths::VirtualPathExpander::from_paths(Arc::clone(&paths)),
            ),
        );
        let dir = std::path::Path::new(&metadata).join("library/ab/abcd");
        std::fs::create_dir_all(&dir).expect("metadata dir");
        let sidecar = dir.join("movie.eng.srt");
        std::fs::write(&sidecar, b"1\n").expect("write sidecar");
        repo.save_media_streams(item, &[subtitle_stream(2, true, sidecar.to_str())])
            .await
            .expect("save streams");

        let mgr = FerrofinSubtitleManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            repo.clone(),
            Vec::new(),
            std::path::PathBuf::from(&metadata),
        );
        mgr.delete_subtitles(item, 2).await.expect("delete");
        assert!(!sidecar.exists(), "the file the token points at is removed");
        let remaining = repo
            .get_media_streams(&MediaStreamQuery {
                item_id: item,
                stream_type: Some(MediaStreamType::Subtitle),
                index: None,
            })
            .await
            .expect("remaining");
        let indexes: Vec<_> = remaining.iter().map(|s| s.stream_index).collect();
        assert_eq!(indexes, [2]);
    }

    /// The stored indexes of every row, in order.
    async fn indexes(repo: &FerrofinMediaStreamRepository, item: Uuid) -> Vec<i64> {
        repo.get_media_streams(&MediaStreamQuery {
            item_id: item,
            ..MediaStreamQuery::default()
        })
        .await
        .expect("rows")
        .iter()
        .map(|s| s.stream_index)
        .collect()
    }

    /// gl-1875: only an external subtitle row whose absolute path is surely
    /// gone is dropped; the rest keep their indexes. A URL or relative row, one
    /// whose path cannot be checked, another kind of stream and an embedded row
    /// stay.
    #[tokio::test]
    async fn drop_missing_subtitle_rows_drops_only_gone_subtitle_files() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = FerrofinMediaStreamRepository::new(db.clone());
        let tmp = tempfile::tempdir().expect("tempdir");
        let present = tmp.path().join("movie.eng.srt");
        std::fs::write(&present, b"1\n").expect("write sidecar");
        let gone = tmp.path().join("movie.fre.srt");
        let gone = gone.to_str().unwrap();
        repo.save_media_streams(
            item,
            &[
                MediaStreamInfoEntity {
                    stream_index: 0,
                    stream_type: media_stream_type_disc(MediaStreamType::Video),
                    ..Default::default()
                },
                subtitle_stream(1, true, Some(present.to_str().unwrap())),
                subtitle_stream(2, true, Some(gone)),
                subtitle_stream(3, true, Some("http://relay.invalid/remote.en.srt")),
                subtitle_stream(4, true, Some("relative.en.srt")),
                MediaStreamInfoEntity {
                    stream_index: 5,
                    stream_type: media_stream_type_disc(MediaStreamType::Audio),
                    is_external: true,
                    path: Some(
                        tmp.path()
                            .join("movie.fre.mka")
                            .to_str()
                            .unwrap()
                            .to_owned(),
                    ),
                    ..Default::default()
                },
                subtitle_stream(6, false, None),
                // A folder in the path is a file: `ENOTDIR`, not surely gone.
                subtitle_stream(7, true, Some(present.join("x.srt").to_str().unwrap())),
            ],
        )
        .await
        .expect("save streams");

        let mgr = manager(db, Vec::new());
        mgr.drop_missing_subtitle_rows(item).await.expect("sweep");
        assert_eq!(indexes(&repo, item).await, [0, 1, 3, 4, 5, 6, 7]);
        mgr.drop_missing_subtitle_rows(item)
            .await
            .expect("sweep again");
        assert_eq!(indexes(&repo, item).await, [0, 1, 3, 4, 5, 6, 7]);
    }

    /// The sweep after a refresh that could not probe keeps the video's
    /// `HasSubtitles` in step with the rows it leaves (gl-1914): false once
    /// its last subtitle went, true while another stays; every other key of
    /// `Data` stays.
    #[rstest::rstest]
    #[case::last_one_gone(false, false)]
    #[case::another_stays(true, true)]
    #[tokio::test]
    async fn the_sweep_keeps_has_subtitles_in_step_with_the_rows(
        #[case] embedded: bool,
        #[case] expected: bool,
    ) {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item_with_data(
            &db,
            item,
            BaseItemKind::Movie,
            "Movie",
            r#"{"HasSubtitles":true,"Container":"matroska"}"#,
        )
        .await;
        let repo = FerrofinMediaStreamRepository::new(db.clone());
        let tmp = tempfile::tempdir().expect("tempdir");
        let gone = tmp.path().join("movie.eng.srt");
        let mut rows = vec![
            MediaStreamInfoEntity {
                stream_index: 0,
                stream_type: media_stream_type_disc(MediaStreamType::Video),
                ..Default::default()
            },
            subtitle_stream(1, true, gone.to_str()),
        ];
        if embedded {
            rows.push(subtitle_stream(2, false, None));
        }
        repo.save_media_streams(item, &rows)
            .await
            .expect("save streams");

        manager(db.clone(), Vec::new())
            .drop_missing_subtitle_rows(item)
            .await
            .expect("sweep");

        let data = crate::test_support::fetch_item(&db, item).await.data;
        let data: serde_json::Value =
            serde_json::from_str(data.as_deref().expect("data")).expect("json");
        assert_eq!(data["HasSubtitles"], expected, "{data}");
        assert_eq!(data["Container"], "matroska", "{data}");
    }

    /// gl-1875, with no refresh that probes between the steps: a second delete
    /// by the index the client then reads finds its row, and each delete's
    /// sweep leaves no row of a deleted file.
    #[tokio::test]
    async fn a_second_delete_by_index_follows_the_first() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = FerrofinMediaStreamRepository::new(db.clone());
        let tmp = tempfile::tempdir().expect("tempdir");
        let (eng, fre) = (tmp.path().join("m.eng.srt"), tmp.path().join("m.fre.srt"));
        std::fs::write(&eng, b"1\n").expect("write eng");
        std::fs::write(&fre, b"1\n").expect("write fre");
        repo.save_media_streams(
            item,
            &[
                MediaStreamInfoEntity {
                    stream_index: 0,
                    stream_type: media_stream_type_disc(MediaStreamType::Video),
                    ..Default::default()
                },
                subtitle_stream(1, true, Some(eng.to_str().unwrap())),
                subtitle_stream(2, true, Some(fre.to_str().unwrap())),
            ],
        )
        .await
        .expect("save streams");

        let mgr = manager(db, Vec::new());
        mgr.delete_subtitles(item, 1).await.expect("delete eng");
        mgr.drop_missing_subtitle_rows(item).await.expect("sweep");
        assert_eq!(indexes(&repo, item).await, [0, 2]);
        mgr.delete_subtitles(item, 2)
            .await
            .expect("delete fre by its index");
        mgr.drop_missing_subtitle_rows(item).await.expect("sweep");
        assert_eq!(indexes(&repo, item).await, [0]);
        assert!(!eng.exists() && !fre.exists());
    }

    /// gl-1723: `File.Delete` ignores a file that is already gone (the delete
    /// succeeds and the refresh drops the stream), but throws
    /// `DirectoryNotFoundException`, a `404` before the refresh, when the
    /// file's folder is missing (`ENOENT`, no parent directory) or a folder in
    /// the path is a file (`ENOTDIR`). A URL row is resolved against upstream's
    /// working directory, where its folder is missing; here no relative path
    /// reaches the disk, even one that would name a file. Each attempt is
    /// bracketed.
    #[rstest::rstest]
    #[case::file_missing("{tmp}/movie.fre.srt", true)]
    #[case::folder_missing("{tmp}/NoSuchFolder/missing.en.srt", false)]
    #[case::folder_is_a_file("{tmp}/movie.mkv/missing.en.srt", false)]
    #[case::url("http://relay.invalid/remote.en.srt", false)]
    #[case::relative_to_a_real_file("{relative_tmp}/movie.mkv", false)]
    #[tokio::test]
    async fn delete_of_a_missing_sidecar_fails_only_for_a_missing_folder(
        #[case] row: &str,
        #[case] succeeds: bool,
    ) {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let media = tmp.path().join("movie.mkv");
        std::fs::write(&media, b"media").expect("write media");
        let root = tmp.path().to_str().unwrap();
        let cwd = std::env::current_dir().expect("cwd");
        let up = "../".repeat(cwd.components().count() - 1);
        let path = row
            .replace("{tmp}", root)
            .replace("{relative_tmp}", &format!("{up}{}", &root[1..]));
        FerrofinMediaStreamRepository::new(db.clone())
            .save_media_streams(item, &[subtitle_stream(2, true, Some(&path))])
            .await
            .expect("save streams");

        let recording = Arc::new(Recording::default());
        let deleted = manager(db, Vec::new())
            .with_self_writes(recording.clone())
            .delete_subtitles(item, 2)
            .await;
        if succeeds {
            deleted.expect("a missing file is no error");
        } else {
            let error = deleted.expect_err("a missing folder is");
            assert!(matches!(error, ServiceError::NotFound(_)), "{error:?}");
        }
        assert_eq!(
            recording.calls(),
            [format!("begin {path}"), format!("complete {path} false")]
        );
        assert!(media.exists(), "no other file is deleted");
    }

    /// gl-1724: on a read-only mount `unlink` fails a file already gone with
    /// `EROFS`, not `ENOENT`. `File.Delete` then returns when an `lstat` of
    /// the path fails with `ENOENT`, and throws (a `500`) for anything else:
    /// a file, a dangling link, or an `lstat` failing otherwise (`ENOTDIR`).
    #[cfg(unix)]
    #[rstest::rstest]
    #[case::file_missing("movie.fre.srt", true)]
    #[case::file_present("movie.mkv", false)]
    #[case::dangling_link("dangling.srt", false)]
    #[case::folder_is_a_file("movie.mkv/missing.srt", false)]
    #[tokio::test]
    async fn read_only_unlink_succeeds_only_for_a_file_already_gone(
        #[case] name: &str,
        #[case] succeeds: bool,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("movie.mkv"), b"media").expect("write media");
        std::os::unix::fs::symlink(tmp.path().join("gone.srt"), tmp.path().join("dangling.srt"))
            .expect("symlink");
        let erofs = std::io::Error::from(std::io::ErrorKind::ReadOnlyFilesystem);

        let settled = settle_unlink(&tmp.path().join(name), Err(erofs), |reason: &str| {
            ServiceError::not_found(reason)
        })
        .await;
        if succeeds {
            settled.expect("a file already gone is no error");
        } else {
            let error = settled.expect_err("something is at the path");
            assert!(matches!(error, ServiceError::Backend(_)), "{error:?}");
        }
    }

    /// gl-846: `DeleteSubtitles` throws an `ArgumentException`, a `400`, for an
    /// index with no subtitle stream (`GetMediaStreams(…)[0]`) and for one with
    /// no file (`ReportFileSystemChangeBeginning(null)`), deleting nothing.
    #[rstest::rstest]
    #[case::no_stream_at_the_index(9)]
    #[case::not_a_subtitle_stream(0)]
    #[case::embedded(1)]
    #[case::external_without_a_path(3)]
    #[case::embedded_naming_the_media_file(4)]
    #[case::external_with_an_empty_path(5)]
    #[tokio::test]
    async fn delete_without_a_subtitle_file_is_invalid_input(#[case] index: i32) {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let media = tmp.path().join("movie.mkv");
        std::fs::write(&media, b"media").expect("write media");
        FerrofinMediaStreamRepository::new(db.clone())
            .save_media_streams(
                item,
                &[
                    MediaStreamInfoEntity {
                        stream_index: 0,
                        stream_type: media_stream_type_disc(MediaStreamType::Video),
                        ..Default::default()
                    },
                    subtitle_stream(1, false, None),
                    subtitle_stream(3, true, None),
                    subtitle_stream(4, false, Some(media.to_str().unwrap())),
                    subtitle_stream(5, true, Some("")),
                ],
            )
            .await
            .expect("save streams");

        let error = manager(db, Vec::new())
            .delete_subtitles(item, index)
            .await
            .expect_err("no subtitle file at the index");
        assert!(matches!(error, ServiceError::InvalidInput(_)), "{error:?}");
        assert!(
            media.exists(),
            "an embedded stream's row never names a file to delete"
        );
    }
}
