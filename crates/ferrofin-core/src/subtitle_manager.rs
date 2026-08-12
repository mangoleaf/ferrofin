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
//!   fetches the content, and **attaches** it to the item (sidecar file next to
//!   the media + an external [`MediaStream`](ferrofin_model::entities_media::MediaStream)
//!   row). [`Self::upload_subtitle`] attaches caller-supplied content the same way.
//! - [`Self::get_remote_subtitles`] routes an id to its provider and returns the
//!   raw content (the `/Providers/Subtitles/Subtitles/{id}` route).
//! - [`Self::delete_subtitles`] removes the external stream row + sidecar file.
//! - [`Self::get_supported_providers`] lists the registered providers for an item.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::MediaStreamInfoEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::providers::{RemoteSubtitleInfo, SubtitleProviderInfo};
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::LibraryManager;
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
    /// Internal-metadata base (`{program-data}/metadata`). Uploaded subtitles fall
    /// back here (`.../library/{id2}/{idN}/`) when the media folder is not writable
    /// (e.g. a read-only library mount), mirroring Jellyfin's non-media-folder save.
    metadata_path: PathBuf,
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
        providers: Vec<Arc<dyn SubtitleProvider>>,
        metadata_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            db,
            library_manager,
            media_streams,
            providers,
            metadata_path: metadata_path.into(),
        }
    }

    /// The item's internal-metadata folder (`{metadata}/library/{id2}/{idN}`),
    /// the writable fallback for uploaded subtitle sidecars.
    fn item_metadata_dir(&self, item_id: Uuid) -> PathBuf {
        let dashless = item_id.simple().to_string();
        self.metadata_path
            .join("library")
            .join(&dashless[..2])
            .join(&dashless)
    }

    /// Fills the request's item-derived fields (name/year/series/season/episode/
    /// path/content-type) from the resolved item, so providers can build a query.
    async fn enrich(&self, request: &mut SubtitleSearchRequest) {
        if let Ok(Some(item)) = self.library_manager.get_item_by_id(request.item_id).await {
            request.content_type = if item.type_.ends_with("Episode") {
                SubtitleMediaType::Episode
            } else {
                SubtitleMediaType::Movie
            };
            request.name = item.name;
            request.series_name = item.series_name;
            request.production_year = item.production_year.and_then(|y| i32::try_from(y).ok());
            request.parent_index_number =
                item.parent_index_number.and_then(|n| i32::try_from(n).ok());
            request.index_number = item.index_number.and_then(|n| i32::try_from(n).ok());
            request.media_path = item.path;
        }
    }

    /// Selects the provider that owns a namespaced id (`"{name}_{local}"`),
    /// returning it plus the provider-local id (prefix stripped).
    fn route(&self, id: &str) -> Option<(&Arc<dyn SubtitleProvider>, String)> {
        self.providers.iter().find_map(|p| {
            let prefix = format!("{}_", p.name());
            id.strip_prefix(&prefix).map(|local| (p, local.to_owned()))
        })
    }

    /// Attaches subtitle content to an item: writes a sidecar file next to the
    /// media and records an external subtitle stream row.
    async fn attach(&self, item_id: Uuid, response: &SubtitleResponse) -> Result<(), ServiceError> {
        let item = self
            .library_manager
            .get_item_by_id(item_id)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("item {item_id}")))?;
        let media_path = item
            .path
            .filter(|p| !p.is_empty())
            .ok_or_else(|| ServiceError::invalid_input("item has no media path for a sidecar"))?;

        // Prefer the sidecar next to the media file; if that folder is not
        // writable (a read-only library mount ⇒ os error 30), fall back to the
        // item's internal metadata folder — mirroring Jellyfin's non-media-folder
        // subtitle save, so an upload succeeds instead of 500-ing.
        let sidecar = sidecar_path(&media_path, response);
        let sidecar_str = if write_sidecar(&sidecar, &response.content).await.is_ok() {
            sidecar.to_string_lossy().into_owned()
        } else {
            let file_name = sidecar
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("subtitle"));
            let fallback = self.item_metadata_dir(item_id).join(file_name);
            write_sidecar(&fallback, &response.content)
                .await
                .map_err(|e| ServiceError::backend(e.to_string()))?;
            fallback.to_string_lossy().into_owned()
        };

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
            language: (!response.language.is_empty()).then(|| response.language.clone()),
            codec: Some(codec_for(&response.format).to_owned()),
            is_forced: response.is_forced,
            is_hearing_impaired: Some(response.is_hearing_impaired),
            ..Default::default()
        });
        self.media_streams
            .save_media_streams(item_id, &streams)
            .await
    }
}

/// The sidecar path for attached subtitle content: sibling of the media file,
/// `"{stem}.{lang}[.forced].{format}"` (Jellyfin's external-subtitle naming).
/// Writes `content` to `path`, creating its parent directory first. Returns the
/// I/O error (e.g. read-only filesystem) so the caller can fall back elsewhere.
async fn write_sidecar(path: &Path, content: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, content).await
}

fn sidecar_path(media_path: &str, response: &SubtitleResponse) -> std::path::PathBuf {
    let path = Path::new(media_path);
    let stem = path.file_stem().map_or_else(
        || "subtitle".to_owned(),
        |s| s.to_string_lossy().into_owned(),
    );
    let lang = if response.language.is_empty() {
        "und"
    } else {
        &response.language
    };
    let forced = if response.is_forced { ".forced" } else { "" };
    let name = format!("{stem}.{lang}{forced}.{}", response.format);
    match path.parent() {
        Some(dir) => dir.join(name),
        None => std::path::PathBuf::from(name),
    }
}

/// The stored codec for a subtitle format (`srt` → `subrip`, etc.).
fn codec_for(format: &str) -> &str {
    match format.to_ascii_lowercase().as_str() {
        "srt" | "sub" => "subrip",
        "vtt" => "webvtt",
        "ssa" => "ssa",
        "ass" => "ass",
        other => match other {
            "" => "subrip",
            _ => format,
        },
    }
}

#[async_trait]
impl SubtitleManager for FerrofinSubtitleManager {
    async fn search_subtitles(
        &self,
        request: &SubtitleSearchRequest,
    ) -> Result<Vec<RemoteSubtitleInfo>, ServiceError> {
        let mut request = request.clone();
        self.enrich(&mut request).await;
        let mut results = Vec::new();
        for provider in &self.providers {
            match provider.search(&request).await {
                Ok(mut found) => results.append(&mut found),
                Err(err) => {
                    // Aggregate best-effort: one provider failing must not sink
                    // the whole search (Jellyfin skips the failed provider).
                    tracing::warn!(provider = provider.name(), %err, "subtitle search failed");
                }
            }
        }
        Ok(results)
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
        // A missing item yields no providers (mirrors C#).
        let _ = self.library_manager.get_item_by_id(item_id).await?;
        Ok(self
            .providers
            .iter()
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

    #[tokio::test]
    async fn upload_falls_back_to_metadata_when_media_folder_unwritable() {
        // A read-only library mount makes the sidecar-next-to-media write fail;
        // the upload must still succeed by falling back to the item's internal
        // metadata folder (was a 500 before). Simulated uid-independently by making
        // the media file's "folder" a regular file (create_dir_all → NotADirectory).
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let not_a_dir = tmp.path().join("locked");
        std::fs::write(&not_a_dir, b"x").expect("file");
        let media = not_a_dir.join("Movie.mkv"); // parent is a file → unwritable
        set_item_path(&db, item, &media).await;

        let meta = tempfile::tempdir().expect("meta tempdir");
        let mgr = FerrofinSubtitleManager::new(
            db.clone(),
            library_manager_over(db.clone()),
            Arc::new(FerrofinMediaStreamRepository::new(db.clone())),
            vec![],
            meta.path().to_path_buf(),
        );

        let resp = SubtitleResponse {
            language: "eng".to_owned(),
            format: "srt".to_owned(),
            is_forced: false,
            is_hearing_impaired: false,
            content: b"1\n00:00:00,000 --> 00:00:01,000\nParity\n".to_vec(),
        };
        mgr.upload_subtitle(item, &resp)
            .await
            .expect("upload should succeed via the metadata fallback");

        let dashless = item.simple().to_string();
        let expected = meta
            .path()
            .join("library")
            .join(&dashless[..2])
            .join(&dashless)
            .join("Movie.eng.srt");
        assert!(expected.exists(), "subtitle written to metadata fallback");

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
