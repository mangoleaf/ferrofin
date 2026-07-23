//! [`HermitSubtitleManager`] — a **minimal** [`SubtitleManager`] over the
//! `MediaStreamInfos` table.
//!
//! Port of `MediaBrowser.Providers.Subtitles.SubtitleManager`. Hermit ports the
//! **portable** slice — the stored external-subtitle rows on an item — and
//! documents the plugin-provider fan-out as a deferral:
//!
//! - [`Self::delete_subtitles`] deletes the *external* subtitle
//!   [`MediaStream`](hermit_model::entities_media::MediaStream) row at a given
//!   stream index (C# `DeleteSubtitles(item, index)` removes the sidecar file and
//!   drops the stream). The on-disk sidecar file is removed too when its `Path`
//!   is known; a missing file is not an error (delete is idempotent).
//! - [`Self::search_subtitles`], [`Self::download_subtitles`],
//!   [`Self::get_remote_subtitles`], [`Self::upload_subtitle`] and
//!   [`Self::get_supported_providers`] all drive the un-ported `ISubtitleProvider`
//!   plugin registry (OpenSubtitles et al.) and the naming/library-options-aware
//!   sidecar writer. No providers are ported for v1, so search/providers return
//!   empty, and download/get/upload reject with [`ServiceError::InvalidInput`] —
//!   the same honest "not enabled" posture as the deferred lyrics manager. These
//!   become real when a subtitle-provider host lands.
//!
//! On-the-fly subtitle *conversion* (the `SubtitleEncoder`, driving the
//! `Stream.{format}` / `.m3u8` routes) is a separate deferred subsystem handled
//! in the API layer (those routes stay on the `501` stub).

use std::sync::Arc;

use async_trait::async_trait;
use hermit_db::Database;
use hermit_model::providers::{RemoteSubtitleInfo, SubtitleProviderInfo};
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::library::LibraryManager;
use hermit_traits::subtitles::{SubtitleManager, SubtitleResponse, SubtitleSearchRequest};

use crate::db_error::{db_err, media_stream_type_disc};

/// The concrete (minimal) subtitle manager.
#[derive(Clone)]
pub struct HermitSubtitleManager {
    db: Database,
    library_manager: Arc<dyn LibraryManager>,
}

impl std::fmt::Debug for HermitSubtitleManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitSubtitleManager")
            .finish_non_exhaustive()
    }
}

impl HermitSubtitleManager {
    /// Creates a subtitle manager over the database and library seam.
    #[must_use]
    pub fn new(db: Database, library_manager: Arc<dyn LibraryManager>) -> Self {
        Self {
            db,
            library_manager,
        }
    }

    /// The shared rejection for the provider-driven operations while no subtitle
    /// provider host is wired.
    fn no_providers() -> ServiceError {
        ServiceError::invalid_input("subtitle providers are not enabled on this server")
    }
}

#[async_trait]
impl SubtitleManager for HermitSubtitleManager {
    async fn search_subtitles(
        &self,
        _request: &SubtitleSearchRequest,
    ) -> Result<Vec<RemoteSubtitleInfo>, ServiceError> {
        // No `ISubtitleProvider` registry is ported (documented deferral).
        Ok(Vec::new())
    }

    async fn download_subtitles(
        &self,
        _item_id: Uuid,
        _subtitle_id: &str,
    ) -> Result<(), ServiceError> {
        Err(Self::no_providers())
    }

    async fn upload_subtitle(
        &self,
        _item_id: Uuid,
        _response: &SubtitleResponse,
    ) -> Result<(), ServiceError> {
        // Writing a sidecar needs the library-options + naming layer (deferred).
        Err(Self::no_providers())
    }

    async fn get_remote_subtitles(&self, _id: &str) -> Result<SubtitleResponse, ServiceError> {
        Err(Self::no_providers())
    }

    async fn delete_subtitles(&self, item_id: Uuid, index: i32) -> Result<(), ServiceError> {
        // Resolve the row first so we can remove any on-disk sidecar, then drop
        // the external subtitle stream at that index (mirrors the C# order:
        // delete the file, then the stream row).
        let subtitle_disc = i64::from(media_stream_type_disc(
            hermit_model::entities::MediaStreamType::Subtitle,
        ));
        let path: Option<String> = sqlx::query_scalar(
            r#"SELECT "Path" FROM "MediaStreamInfos"
               WHERE "ItemId" = ?1 AND "StreamIndex" = ?2
                 AND "StreamType" = ?3 AND "IsExternal" = 1"#,
        )
        .bind(item_id.to_string())
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
        .bind(item_id.to_string())
        .bind(i64::from(index))
        .bind(subtitle_disc)
        .execute(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_supported_providers(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<SubtitleProviderInfo>, ServiceError> {
        // Resolving the item mirrors C# (a missing item yields no providers);
        // the provider registry itself is a documented deferral.
        let _ = self.library_manager.get_item_by_id(item_id).await?;
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use hermit_db::entities::base_items::MediaStreamInfoEntity;
    use hermit_model::data::BaseItemKind;
    use hermit_model::entities::MediaStreamType;
    use hermit_traits::error::ServiceError;
    use hermit_traits::persistence::MediaStreamRepository;
    use hermit_traits::subtitles::{SubtitleManager, SubtitleSearchRequest};
    use uuid::Uuid;

    use crate::db_error::media_stream_type_disc;
    use crate::media_stream_repository::HermitMediaStreamRepository;
    use crate::test_support::{library_manager_over, seed_item, test_db};

    use super::HermitSubtitleManager;

    fn subtitle_stream(index: i64, external: bool, path: Option<&str>) -> MediaStreamInfoEntity {
        MediaStreamInfoEntity {
            item_id: String::new(),
            stream_index: index,
            aspect_ratio: None,
            average_frame_rate: None,
            bit_depth: None,
            bit_rate: None,
            bl_present_flag: None,
            channel_layout: None,
            channels: None,
            codec: Some("subrip".to_owned()),
            codec_tag: None,
            codec_time_base: None,
            color_primaries: None,
            color_space: None,
            color_transfer: None,
            comment: None,
            dv_bl_signal_compatibility_id: None,
            dv_level: None,
            dv_profile: None,
            dv_version_major: None,
            dv_version_minor: None,
            el_present_flag: None,
            hdr10_plus_present_flag: None,
            height: None,
            is_anamorphic: None,
            is_avc: None,
            is_default: false,
            is_external: external,
            is_forced: false,
            is_hearing_impaired: None,
            is_interlaced: None,
            is_original: false,
            key_frames: None,
            language: Some("eng".to_owned()),
            level: None,
            nal_length_size: None,
            path: path.map(str::to_owned),
            pixel_format: None,
            profile: None,
            real_frame_rate: None,
            ref_frames: None,
            rotation: None,
            rpu_present_flag: None,
            sample_rate: None,
            stream_type: media_stream_type_disc(MediaStreamType::Subtitle),
            time_base: None,
            title: None,
            width: None,
        }
    }

    #[tokio::test]
    async fn delete_removes_external_stream_and_sidecar() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = HermitMediaStreamRepository::new(db.clone());

        // Write the sidecar file so the manager can remove it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let sidecar = tmp.path().join("movie.eng.srt");
        std::fs::write(&sidecar, b"1\n00:00:00,000 --> 00:00:01,000\nhi\n").expect("write sidecar");

        repo.save_media_streams(
            item,
            &[
                subtitle_stream(2, true, Some(sidecar.to_str().unwrap())),
                subtitle_stream(3, true, None),
            ],
        )
        .await
        .expect("save streams");

        let mgr = HermitSubtitleManager::new(db.clone(), library_manager_over(db.clone()));
        mgr.delete_subtitles(item, 2).await.expect("delete idx 2");

        assert!(!sidecar.exists(), "sidecar file should be removed");
        let remaining = repo
            .get_media_streams(&hermit_traits::persistence::MediaStreamQuery {
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
        let mgr = HermitSubtitleManager::new(db.clone(), library_manager_over(db.clone()));
        // Nothing stored → still succeeds.
        mgr.delete_subtitles(item, 9).await.expect("no-op delete");
    }

    #[tokio::test]
    async fn provider_paths_are_empty_or_rejected() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let mgr = HermitSubtitleManager::new(db.clone(), library_manager_over(db.clone()));

        assert!(
            mgr.search_subtitles(&SubtitleSearchRequest {
                item_id: item,
                language: "eng".to_owned(),
                is_perfect_match: None,
                is_automated: false,
            })
            .await
            .expect("search")
            .is_empty()
        );
        assert!(
            mgr.get_supported_providers(item)
                .await
                .expect("providers")
                .is_empty()
        );
        assert!(matches!(
            mgr.download_subtitles(item, "x").await,
            Err(ServiceError::InvalidInput(_))
        ));
        assert!(matches!(
            mgr.get_remote_subtitles("x").await,
            Err(ServiceError::InvalidInput(_))
        ));
    }
}
