//! [`FerrofinSubtitleManager`] — subtitle search/download/upload/delete over the
//! `MediaStreamInfos` table and a registry of [`SubtitleProvider`]s.
//!
//! Port of `MediaBrowser.Providers.Subtitles.SubtitleManager`:
//!
//! - [`Self::search_subtitles`] enriches the request from the resolved item
//!   (name/year/series/season/episode/path) and fans it out across the
//!   registered providers (OpenSubtitles, …), aggregating the candidates. A
//!   provider that errors is skipped (logged) rather than failing the whole
//!   search, matching Jellyfin's per-provider aggregation.
//! - [`Self::download_subtitles`] routes the namespaced id back to its provider,
//!   fetches the content, and **attaches** it to the item (a sidecar in the library
//!   configured media or internal-metadata folder + an external [`MediaStream`](ferrofin_model::entities_media::MediaStream)
//!   row). [`Self::upload_subtitle`] attaches caller-supplied content the same way.
//! - [`Self::get_remote_subtitles`] routes an id to its provider and returns the
//!   raw content (the `/Providers/Subtitles/Subtitles/{id}` route).
//! - [`Self::delete_subtitles`] removes the external stream row + sidecar file.
//! - [`Self::get_supported_providers`] lists the registered providers for an item.

use ferrofin_util::directory_path::DirectoryPath;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::MediaStreamInfoEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::providers::{RemoteSubtitleInfo, SubtitleProviderInfo};
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, VirtualFolderManager};
use ferrofin_traits::persistence::{MediaStreamQuery, MediaStreamRepository};
use ferrofin_traits::subtitles::{
    SubtitleManager, SubtitleMediaType, SubtitleProvider, SubtitleResponse, SubtitleSearchRequest,
};

use crate::db_error::{db_err, media_stream_type_disc};

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
        }
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

    /// Fills the request's item-derived fields (name/year/series/season/episode/
    /// path/content-type) from the resolved item, so providers can build a query.
    async fn enrich(&self, request: &mut SubtitleSearchRequest) -> Result<bool, ServiceError> {
        if let Some(item) = self.library_manager.get_item_by_id(request.item_id).await? {
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

    /// Selects the provider that owns a namespaced id (`"{name}_{local}"`),
    /// returning it plus the provider-local id (prefix stripped).
    fn route(&self, id: &str) -> Option<(&Arc<dyn SubtitleProvider>, String)> {
        self.providers.iter().find_map(|p| {
            let prefix = format!("{}_", p.name());
            id.strip_prefix(&prefix).map(|local| (p, local.to_owned()))
        })
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
                found
            }
            Err(error) => {
                tracing::warn!(provider = provider.name(), %error, "subtitle search failed");
                Vec::new()
            }
        }
    }

    /// Writes subtitle content to the configured destination, preserving any
    /// existing subtitle file, then records an external subtitle stream row.
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
            let folders = folders.get_virtual_folders().await?;
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
        let sidecar = sidecar_path(media_path, response)?;
        let destination = if save_with_media {
            sidecar
        } else {
            let filename = sidecar
                .file_name()
                .ok_or_else(|| ServiceError::invalid_input("subtitle has no filename"))?;
            self.item_metadata_dir(item_id).join(filename)
        };
        // The pinned SubtitleManager selects one destination. An unwritable
        // media folder is an error, not permission to write into metadata.
        let sidecar = write_sidecar(&destination, &response.content)
            .await
            .map_err(|error| ServiceError::backend(error.to_string()))?;
        let sidecar_str = sidecar.to_string_lossy().into_owned();

        // Append the new external subtitle to the item's stream set (the repo
        // save is a full replace, so re-save the existing streams alongside it).
        let mut streams = self
            .media_streams
            .get_media_streams(&MediaStreamQuery {
                item_id,
                stream_type: None,
                index: None,
            })
            .await?;
        let next_index = streams
            .iter()
            .map(|s| s.stream_index)
            .max()
            .map_or(0, |m| m + 1);
        streams.push(MediaStreamInfoEntity {
            stream_index: next_index,
            stream_type: media_stream_type_disc(
                ferrofin_model::entities::MediaStreamType::Subtitle,
            ),
            is_external: true,
            path: Some(sidecar_str),
            language: (!response.language.is_empty()).then(|| response.language.to_lowercase()),
            codec: codec_for(&response.format.to_ascii_lowercase()).map(str::to_owned),
            is_forced: response.is_forced,
            is_hearing_impaired: Some(response.is_hearing_impaired),
            ..Default::default()
        });
        self.media_streams
            .save_media_streams(item_id, &streams)
            .await
    }
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
async fn write_sidecar(path: &Path, content: &[u8]) -> std::io::Result<PathBuf> {
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
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .await
        {
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
fn sidecar_path(media_path: &str, response: &SubtitleResponse) -> Result<PathBuf, ServiceError> {
    let path = Path::new(media_path);
    let stem = path.file_stem().map_or_else(
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
    let name = format!("{stem}.{language}{forced}{hearing_impaired}.{extension}");
    Ok(path
        .parent()
        .map_or_else(|| PathBuf::from(&name), |parent| parent.join(&name)))
}

/// Codec for the immediate external-stream record. Matroska subtitle files
/// can contain several codecs; the queued probe resolves their contents.
fn codec_for(extension: &str) -> Option<&str> {
    match extension {
        "vtt" => Some("webvtt"),
        "ssa" => Some("ssa"),
        "ass" => Some("ass"),
        "smi" | "sami" => Some("sami"),
        "sup" => Some("hdmv_pgs_subtitle"),
        "mks" => None,
        _ => Some("subrip"),
    }
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
        let response = provider.get_subtitles(&local).await?;
        self.attach(item_id, &response).await
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

    async fn delete_subtitles(&self, item_id: Uuid, index: i32) -> Result<(), ServiceError> {
        // Resolve the row first so we can remove any on-disk sidecar, then drop
        // the external subtitle stream at that index (mirrors the C# order:
        // delete the file, then the stream row).
        let subtitle_disc = i64::from(media_stream_type_disc(
            ferrofin_model::entities::MediaStreamType::Subtitle,
        ));
        let path: Option<String> = sqlx::query_scalar(
            r#"SELECT "Path" FROM "MediaStreamInfos"
               WHERE "ItemId" = ?1 AND "StreamIndex" = ?2
                 AND "StreamType" = ?3 AND "IsExternal" = 1"#,
        )
        .bind(guid_to_db(item_id))
        .bind(i64::from(index))
        .bind(subtitle_disc)
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?
        .flatten();

        if let Some(path) = path.as_deref()
            && !path.is_empty()
        {
            // A missing file is fine — the goal is that it no longer exists.
            match tokio::fs::remove_file(path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(ServiceError::backend(e.to_string())),
            }
        }

        sqlx::query(
            r#"DELETE FROM "MediaStreamInfos"
               WHERE "ItemId" = ?1 AND "StreamIndex" = ?2
                 AND "StreamType" = ?3 AND "IsExternal" = 1"#,
        )
        .bind(guid_to_db(item_id))
        .bind(i64::from(index))
        .bind(subtitle_disc)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
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
                id: Some(p.name().to_owned()),
            })
            .collect())
    }

    async fn validate_provider_login(
        &self,
        provider_name: &str,
        config_json: &[u8],
    ) -> Result<(), ServiceError> {
        let provider = self
            .providers
            .iter()
            .find(|p| p.name() == provider_name)
            .ok_or_else(|| ServiceError::not_found(format!("subtitle provider {provider_name}")))?;
        provider.validate_login(config_json).await
    }
}

#[cfg(test)]
mod tests {
    use ferrofin_db::entities::base_items::MediaStreamInfoEntity;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::entities::MediaStreamType;
    use ferrofin_traits::persistence::MediaStreamRepository;
    use ferrofin_traits::subtitles::{SubtitleProvider, SubtitleResponse, SubtitleSearchRequest};
    use uuid::Uuid;

    use crate::db_error::media_stream_type_disc;
    use crate::media_stream_repository::FerrofinMediaStreamRepository;
    use crate::test_support::{library_manager_over, seed_item, test_db};

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
                id: Some("fake_42".to_owned()),
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
        assert_eq!(results[0].id.as_deref(), Some("fake_42"));
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

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn search_enriches_ids_episode_range_and_runtime_from_saved_item() {
        use ferrofin_traits::persistence::ItemPersistenceService as _;

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

    #[tokio::test]
    async fn download_routes_to_provider_and_attaches() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        // Give the item a media path so the sidecar has a home.
        let tmp = tempfile::tempdir().expect("tempdir");
        let media = tmp.path().join("Movie.mkv");
        std::fs::write(&media, b"x").expect("media");
        set_item_path(&db, item, &media).await;

        let mgr = manager(db.clone(), vec![Arc::new(FakeProvider)]);
        mgr.download_subtitles(item, "fake_42")
            .await
            .expect("download");

        // The sidecar was written and an external subtitle row recorded.
        let sidecar = tmp.path().join("Movie.eng.srt");
        assert!(sidecar.exists(), "sidecar written");
        let repo = FerrofinMediaStreamRepository::new(db);
        let streams = repo
            .get_media_streams(&MediaStreamQuery {
                item_id: item,
                stream_type: Some(MediaStreamType::Subtitle),
                index: None,
            })
            .await
            .expect("streams");
        assert_eq!(streams.len(), 1);
        assert!(streams[0].is_external);
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
    fn sidecar_path_rejects_invalid_input_and_preserves_upstream_names() {
        let mut response = upload_response();
        for format in ["", ".VTT", "nfo", "strm", "srt ", "../srt"] {
            response.format = format.to_owned();
            assert!(matches!(
                sidecar_path("/library/movie/Movie.mkv", &response),
                Err(ServiceError::InvalidInput(_))
            ));
        }
        response.format = "srt".to_owned();
        for language in ["x/../../outside/pwned", "eng\0"] {
            response.language = language.to_owned();
            assert!(matches!(
                sidecar_path("/library/movie/Movie.mkv", &response),
                Err(ServiceError::InvalidInput(_))
            ));
        }
        response.language = "pt-BR".to_owned();
        response.is_forced = true;
        response.is_hearing_impaired = true;
        for extension in SUBTITLE_EXTENSIONS {
            response.format = extension.to_ascii_uppercase();
            assert_eq!(
                sidecar_path("/library/movie/Movie.mkv", &response).unwrap(),
                Path::new(&format!(
                    "/library/movie/Movie.pt-br.forced.sdh.{extension}"
                ))
            );
        }
        response.language.clear();
        response.format = "srt".to_owned();
        response.is_forced = false;
        response.is_hearing_impaired = false;
        assert_eq!(
            sidecar_path("/library/movie/Movie.mkv", &response).unwrap(),
            Path::new("/library/movie/Movie..srt")
        );
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
        assert!(
            FerrofinMediaStreamRepository::new(db)
                .get_media_streams(&MediaStreamQuery {
                    item_id: item,
                    stream_type: None,
                    index: None,
                })
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn uploaded_image_subtitles_keep_image_codecs_and_containers_require_probing() {
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
        let streams = FerrofinMediaStreamRepository::new(db)
            .get_media_streams(&MediaStreamQuery {
                item_id: item,
                stream_type: Some(MediaStreamType::Subtitle),
                index: None,
            })
            .await
            .unwrap();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].codec.as_deref(), Some("hdmv_pgs_subtitle"));
        assert_eq!(
            streams[0].path.as_deref(),
            tmp.path().join("Movie.eng.sup").to_str()
        );
        let image_subtitle = ferrofin_model::entities_media::MediaStream {
            stream_type: MediaStreamType::Subtitle,
            codec: streams[0].codec.clone(),
            is_external: true,
            ..Default::default()
        };
        assert!(!image_subtitle.is_text_subtitle_stream());
        assert!(
            streams[1].codec.is_none(),
            "MKS contents determine the codec at probe time"
        );
        assert_eq!(
            streams[1].path.as_deref(),
            tmp.path().join("Movie.eng.mks").to_str()
        );
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
        assert!(
            FerrofinMediaStreamRepository::new(db)
                .get_media_streams(&MediaStreamQuery {
                    item_id: item,
                    stream_type: None,
                    index: None,
                })
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn live_library_destination_applies_to_uploads_and_downloads_without_overwriting() {
        use ferrofin_traits::persistence::ItemPersistenceService as _;

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
        mgr.download_subtitles(item, "fake_42").await.unwrap();
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
        mgr.download_subtitles(item, "fake_42").await.unwrap();
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
                stream_type: Some(MediaStreamType::Subtitle),
                index: None,
            })
            .await
            .unwrap();
        assert_eq!(streams.len(), 5);
        for (index, expected) in [
            tmp.path().join("Movie.eng.0.srt"),
            tmp.path().join("Movie.eng.1.srt"),
            internal.join("Movie.eng.srt"),
            internal.join("Movie.eng.0.srt"),
            tmp.path().join("Movie.eng.2.srt"),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(streams[index].path.as_deref(), expected.to_str());
            assert_eq!(streams[index].language.as_deref(), Some("eng"));
            assert!(streams[index].is_external);
        }
    }

    #[tokio::test]
    async fn concurrent_sidecar_writes_do_not_replace_existing_bytes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("Movie.eng.srt");
        let (first, second) = tokio::join!(
            write_sidecar(&path, b"first"),
            write_sidecar(&path, b"second")
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(first).unwrap(), b"first");
        assert_eq!(std::fs::read(second).unwrap(), b"second");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 2);
        let blocked = tmp.path().join("Movie.fra.srt");
        std::fs::create_dir(&blocked).unwrap();
        assert!(write_sidecar(&blocked, b"content").await.is_err());
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

        let repo = FerrofinMediaStreamRepository::new(db);
        let streams = repo
            .get_media_streams(&MediaStreamQuery {
                item_id: item,
                stream_type: Some(MediaStreamType::Subtitle),
                index: None,
            })
            .await
            .expect("streams");
        assert_eq!(streams.len(), 1);
        assert!(streams[0].is_external);
        assert_eq!(streams[0].path.as_deref(), expected.to_str());
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
        let resp = mgr.get_remote_subtitles("fake_42").await.expect("remote");
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
                id: Some(format!("{}_1", self.0)),
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
        assert_eq!(results[0].id.as_deref(), Some("second_1"));
        request.subtitle_fetcher_order.clear();
        request.is_perfect_match = Some(true);
        let results = mgr.search_subtitles(&request).await.unwrap();
        assert_eq!(
            results[0].id.as_deref(),
            Some("second_1"),
            "skip a nonmatching provider result"
        );
        request.disabled_subtitle_fetchers = vec!["SECOND".to_owned()];
        assert!(mgr.search_subtitles(&request).await.unwrap().is_empty());
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
                    id: Some(format!("{}_1", self.name)),
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
            results[0].id.as_deref(),
            Some("Fallback_1"),
            "saved provider names use exact casing"
        );
        assert_eq!(*calls.lock().unwrap(), ["Fallback"]);
        calls.lock().unwrap().clear();
        request.disabled_subtitle_fetchers = vec!["FALLBACK".to_owned()];
        let results = mgr.search_subtitles(&request).await.unwrap();
        assert_eq!(results[0].id.as_deref(), Some("Movies_1"));
        assert_eq!(
            *calls.lock().unwrap(),
            ["Failed", "Empty", "Movies"],
            "failed and empty providers fall through in intrinsic order"
        );
        calls.lock().unwrap().clear();
        request.disabled_subtitle_fetchers.clear();
        request.subtitle_fetcher_order = vec!["Movies".to_owned(), "Failed".to_owned()];
        assert_eq!(
            mgr.search_subtitles(&request).await.unwrap()[0]
                .id
                .as_deref(),
            Some("Movies_1")
        );
        assert_eq!(*calls.lock().unwrap(), ["Movies"]);
        calls.lock().unwrap().clear();
        request.disabled_subtitle_fetchers = vec![
            "movies".to_owned(),
            "fallback".to_owned(),
            "failed".to_owned(),
            "empty".to_owned(),
        ];
        assert!(mgr.search_subtitles(&request).await.unwrap().is_empty());
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
            assert_eq!(
                results[0].id.as_deref(),
                Some(format!("{expected}_1").as_str())
            );
            let descriptors = mgr.get_supported_providers(id).await.unwrap();
            assert_eq!(descriptors.len(), 1);
            assert_eq!(descriptors[0].name.as_deref(), Some(expected));
        }
        assert!(
            mgr.search_subtitles(&SubtitleSearchRequest {
                item_id: audio,
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty()
        );
        assert!(mgr.get_supported_providers(audio).await.unwrap().is_empty());
        assert!(
            mgr.get_supported_providers(Uuid::new_v4())
                .await
                .unwrap()
                .is_empty()
        );
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
                id: Some(format!("{}_1", self.name)),
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
            ["Beta_1", "Alpha_1"]
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
        assert!(
            mgr.search_subtitles(&SubtitleSearchRequest {
                item_id: item,
                ..Default::default()
            })
            .await
            .expect("search")
            .is_empty()
        );
    }

    #[tokio::test]
    async fn delete_removes_external_stream_and_sidecar() {
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

        let mgr = manager(db.clone(), Vec::new());
        mgr.delete_subtitles(item, 2).await.expect("delete idx 2");

        assert!(!sidecar.exists(), "sidecar file should be removed");
        let remaining = repo
            .get_media_streams(&MediaStreamQuery {
                item_id: item,
                stream_type: Some(MediaStreamType::Subtitle),
                index: None,
            })
            .await
            .expect("remaining");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].stream_index, 3);
    }

    #[tokio::test]
    async fn delete_missing_index_is_idempotent() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let mgr = manager(db, Vec::new());
        mgr.delete_subtitles(item, 9).await.expect("no-op delete");
    }
}
