//! The post-refresh TrickplayProvider, selected independently of the probe.

use std::sync::Arc;

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::configuration::{LibraryOptions, TrickplayScanBehavior};
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
use ferrofin_traits::trickplay::TrickplayManager;
use uuid::Uuid;

use super::{ItemRefreshPlan, ScanCancel, item_type_lookup};

/// Shares the manager and reads global scan timing live for every refresh.
#[derive(Clone)]
pub(super) struct ScanTrickplay {
    pub(super) manager: Arc<dyn TrickplayManager>,
    pub(super) behavior: Arc<dyn Fn() -> TrickplayScanBehavior + Send + Sync>,
}

impl ScanTrickplay {
    /// Whether the item waits for its extraction (Blocking). Read once per
    /// item, so its refresh runs at one timing even if the setting changes
    /// meanwhile; an unrecognized value runs detached, as upstream's `else`.
    pub(super) fn blocking(&self) -> bool {
        (self.behavior)() == TrickplayScanBehavior::Blocking
    }

    /// Blocking timing: extracts from the caller's row and streams and waits.
    /// An existing item runs from its fresh probe before its final save, so
    /// the metadata Etag stays put until extraction finishes.
    pub(super) async fn refresh(
        &self,
        id: Uuid,
        entity: &BaseItemEntity,
        streams: Option<&[ferrofin_db::entities::base_items::MediaStreamInfoEntity]>,
        library: &LibraryOptions,
        options: &MetadataRefreshOptions,
        cancel: &ScanCancel,
    ) -> bool {
        let work = self.manager.refresh_trickplay_for_media(
            id,
            entity,
            streams,
            replace(options),
            library,
        );
        outcome(id, cancel.unless_cancelled(work).await)
    }

    /// NonBlocking timing: a detached refresh by id, which reads the row and
    /// streams back when it starts rather than carrying copies of the scan's
    /// (gl-233). Call it only after the item's final save and its probe's
    /// stream rows, which it then reads. Cancellation still follows the
    /// originating refresh after return.
    ///
    /// TODO(gl-1824, open work item): the task carries its own copy of the
    /// library's options from the start, and it runs at once, so the refresh
    /// then holds the row and streams it read until its generation slot: a
    /// scan's backlog still holds all three per video. Un-defer path (an
    /// owner decision): hold only the id until a slot is free, by a queue
    /// drained by as many workers as the generation slots, with the options
    /// shared per library rather than copied per video.
    pub(super) fn spawn(
        &self,
        id: Uuid,
        library: LibraryOptions,
        options: &MetadataRefreshOptions,
        cancel: &ScanCancel,
    ) {
        let manager = Arc::clone(&self.manager);
        let cancel = cancel.clone();
        let replace = replace(options);
        // The shared production extractor's ImageEncodingJobPool bounds
        // running ffmpeg processes, including jobs from scheduled tasks.
        tokio::spawn(async move {
            let work = manager.refresh_trickplay_data(id, replace, &library);
            outcome(id, cancel.unless_cancelled(work).await)
        });
    }
}

/// TrickplayProvider only honors replacement above Default, regardless of
/// ReplaceAllMetadata or the image refresh mode.
fn replace(options: &MetadataRefreshOptions) -> bool {
    options.regenerate_trickplay
        && options.metadata_refresh_mode == MetadataRefreshMode::FullRefresh
}

/// `false` only when the refresh was cancelled.
fn outcome(id: Uuid, result: Option<Result<(), ServiceError>>) -> bool {
    match result {
        Some(Ok(())) => true,
        Some(Err(error)) => {
            tracing::warn!(item_id = %id, %error, "scan trickplay refresh failed");
            // TODO(parity, open work item — NOT an accepted divergence;
            // gl-1826): since Jellyfin 12.1 `RunCustomProvider` counts an
            // ordinary error (`Failures++`), so a Blocking refresh that fails
            // leaves the item's `DateLastRefreshed` where it was; here the
            // scan stamps it. Un-defer path: report a failure apart from a
            // cancel, have `scan_item` write the kept stamp after a failed
            // Blocking refresh, and flip the tests that pin the 12.0 rule
            // (`blocking_custom_provider_service_error_still_updates_the_refresh_stamp`
            // and `blocking_cancellation_drops_work_and_provider_failures_do_not_abort_the_refresh`).
            // A detached refresh's result stays dropped.
            true
        }
        None => false,
    }
}

/// MetadataService.GetProviders + TrickplayProvider.HasChanged/FetchInternal.
/// A subtitle-only/NFO-only change does not make the media mtime monitor fire.
pub(super) fn selected(
    row: &BaseItemEntity,
    previous: Option<&BaseItemEntity>,
    plan: ItemRefreshPlan,
    options: &MetadataRefreshOptions,
    library: Option<&LibraryOptions>,
    locked: bool,
) -> bool {
    if locked
        || options.metadata_refresh_mode == MetadataRefreshMode::None
        || !library.is_some_and(|o| o.extract_trickplay_images_during_library_scan)
        || !matches!(
            item_type_lookup::kind_from_type_name(&row.type_),
            Some(
                BaseItemKind::Episode
                    | BaseItemKind::MusicVideo
                    | BaseItemKind::Movie
                    | BaseItemKind::Trailer
                    | BaseItemKind::Video
            )
        )
    {
        return false;
    }
    plan.run_all_providers
        || previous.is_some_and(|previous| {
            row.date_modified.is_some_and(|mtime| {
                crate::refresh_plan::file_changed(previous.date_modified, mtime)
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use chrono::{Duration, Utc};
    use ferrofin_db::entities::playback::TrickplayInfoEntity;
    use tokio::sync::Notify;

    fn movie() -> BaseItemEntity {
        BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.Movies.Movie".to_owned(),
            date_modified: Some(Utc::now()),
            ..Default::default()
        }
    }

    fn during_scan() -> LibraryOptions {
        LibraryOptions {
            extract_trickplay_images_during_library_scan: true,
            ..Default::default()
        }
    }

    #[test]
    fn independent_scan_flag_kind_lock_and_provider_selection() {
        let row = movie();
        let full = ItemRefreshPlan {
            run_all_providers: true,
            ..ItemRefreshPlan::IDLE
        };
        let options = MetadataRefreshOptions::default();
        let library = during_scan();
        assert!(selected(
            &row,
            Some(&row),
            full,
            &options,
            Some(&library),
            false
        ));
        // EnableTrickplayImageExtraction remains the manager's gate. The scan
        // provider must still run with it false so managed tiles are pruned.
        assert!(!library.enable_trickplay_image_extraction);
        for saved in [None, Some(&LibraryOptions::default())] {
            assert!(!selected(&row, None, full, &options, saved, false));
        }
        assert!(!selected(&row, None, full, &options, Some(&library), true));
        assert!(!selected(
            &row,
            None,
            full,
            &MetadataRefreshOptions {
                metadata_refresh_mode: MetadataRefreshMode::None,
                ..options.clone()
            },
            Some(&library),
            false
        ));
        assert!(!selected(
            &row,
            None,
            ItemRefreshPlan::IDLE,
            &MetadataRefreshOptions {
                metadata_refresh_mode: MetadataRefreshMode::ValidationOnly,
                ..options.clone()
            },
            Some(&library),
            false
        ));
        assert!(
            !selected(
                &row,
                Some(&row),
                ItemRefreshPlan {
                    remote_metadata: true,
                    ..ItemRefreshPlan::IDLE
                },
                &options,
                Some(&library),
                false
            ),
            "the separate D2 backfill heuristic is not a source remote change-monitor signal"
        );
        let mut audio = row;
        audio.type_ = "MediaBrowser.Controller.Entities.Audio.Audio".to_owned();
        assert!(!selected(
            &audio,
            None,
            full,
            &options,
            Some(&library),
            false
        ));
    }

    #[test]
    fn actual_media_change_and_validation_mode_select_execution() {
        let row = movie();
        let full = ItemRefreshPlan {
            run_all_providers: true,
            ..ItemRefreshPlan::IDLE
        };
        let options = MetadataRefreshOptions::default();
        let library = during_scan();
        let mut old = row.clone();
        old.date_modified = row.date_modified.map(|mtime| mtime - Duration::seconds(2));
        assert!(selected(
            &row,
            Some(&old),
            ItemRefreshPlan::IDLE,
            &options,
            Some(&library),
            false
        ));
        assert!(
            !selected(
                &row,
                Some(&row),
                ItemRefreshPlan {
                    probe: true,
                    local_monitor_fired: true,
                    ..ItemRefreshPlan::IDLE
                },
                &options,
                Some(&library),
                false
            ),
            "external subtitles alone do not change the video file"
        );
        old.date_modified = row
            .date_modified
            .map(|mtime| mtime - Duration::milliseconds(999));
        assert!(!selected(
            &row,
            Some(&old),
            ItemRefreshPlan::IDLE,
            &options,
            Some(&library),
            false
        ));
        assert!(
            selected(
                &row,
                Some(&row),
                full,
                &MetadataRefreshOptions {
                    metadata_refresh_mode: MetadataRefreshMode::ValidationOnly,
                    ..options
                },
                Some(&library),
                false
            ),
            "ReplaceAllMetadata can select custom providers in validation mode"
        );
    }

    #[derive(Default)]
    struct HeldManager {
        entered: Notify,
        release: Notify,
        dropped: Arc<AtomicBool>,
        /// `(id, replace, extraction enabled, handed the caller's row)`.
        calls: Mutex<Vec<(Uuid, bool, bool, bool)>>,
        fail: bool,
    }

    impl HeldManager {
        async fn held(
            &self,
            id: Uuid,
            replace: bool,
            options: &LibraryOptions,
            with_row: bool,
        ) -> Result<(), ServiceError> {
            let _dropped = std::mem::DropGuard::new(Arc::clone(&self.dropped), |dropped| {
                dropped.store(true, Ordering::SeqCst);
            });
            self.calls.lock().unwrap().push((
                id,
                replace,
                options.enable_trickplay_image_extraction,
                with_row,
            ));
            self.entered.notify_one();
            self.release.notified().await;
            if self.fail {
                Err(ServiceError::backend("fixture failure"))
            } else {
                Ok(())
            }
        }
    }

    #[async_trait]
    impl TrickplayManager for HeldManager {
        async fn refresh_trickplay_data(
            &self,
            id: Uuid,
            replace: bool,
            options: &LibraryOptions,
        ) -> Result<(), ServiceError> {
            self.held(id, replace, options, false).await
        }
        async fn refresh_trickplay_for_media(
            &self,
            id: Uuid,
            _entity: &BaseItemEntity,
            _streams: Option<&[ferrofin_db::entities::base_items::MediaStreamInfoEntity]>,
            replace: bool,
            options: &LibraryOptions,
        ) -> Result<(), ServiceError> {
            self.held(id, replace, options, true).await
        }
        async fn get_trickplay_resolutions(
            &self,
            _id: Uuid,
        ) -> Result<HashMap<i32, TrickplayInfoEntity>, ServiceError> {
            Ok(HashMap::new())
        }
        async fn get_trickplay_items(
            &self,
            _limit: i32,
            _offset: i32,
        ) -> Result<Vec<TrickplayInfoEntity>, ServiceError> {
            Ok(Vec::new())
        }
        async fn save_trickplay_info(
            &self,
            _info: &TrickplayInfoEntity,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn delete_trickplay_data(&self, _id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_hls_playlist(
            &self,
            _id: Uuid,
            _width: i32,
            _key: Option<&str>,
        ) -> Result<Option<String>, ServiceError> {
            Ok(None)
        }
        async fn get_trickplay_tile_path(
            &self,
            _id: Uuid,
            _width: i32,
            _index: i32,
        ) -> Result<Option<String>, ServiceError> {
            Ok(None)
        }
    }

    fn provider(manager: &Arc<HeldManager>, behavior: TrickplayScanBehavior) -> ScanTrickplay {
        ScanTrickplay {
            manager: manager.clone(),
            behavior: Arc::new(move || behavior),
        }
    }

    #[tokio::test]
    async fn blocking_waits_and_full_refresh_alone_honors_regenerate() {
        let manager = Arc::new(HeldManager::default());
        let provider = provider(&manager, TrickplayScanBehavior::Blocking);
        assert!(provider.blocking());
        let id = Uuid::new_v4();
        let handle = tokio::spawn(async move {
            provider
                .refresh(
                    id,
                    &movie(),
                    Some(&[]),
                    &during_scan(),
                    &MetadataRefreshOptions {
                        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
                        regenerate_trickplay: true,
                        ..Default::default()
                    },
                    &ScanCancel::default(),
                )
                .await
        });
        manager.entered.notified().await;
        assert!(
            !handle.is_finished(),
            "Blocking waits until extraction completes"
        );
        assert_eq!(*manager.calls.lock().unwrap(), [(id, true, false, true)]);
        manager.release.notify_one();
        assert!(
            handle.await.unwrap(),
            "completed blocking provider succeeds"
        );
        assert!(manager.dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn nonblocking_and_unknown_timing_detach_by_id_and_keep_the_originating_cancellation() {
        for behavior in [
            TrickplayScanBehavior::NonBlocking,
            TrickplayScanBehavior::Unrecognized(7),
        ] {
            let manager = Arc::new(HeldManager::default());
            let provider = provider(&manager, behavior);
            assert!(!provider.blocking());
            let cancel = ScanCancel::default();
            let id = Uuid::new_v4();
            provider.spawn(
                id,
                during_scan(),
                &MetadataRefreshOptions {
                    regenerate_trickplay: true,
                    replace_all_metadata: true,
                    ..Default::default()
                },
                &cancel,
            );
            manager.entered.notified().await;
            assert_eq!(
                *manager.calls.lock().unwrap(),
                [(id, false, false, false)],
                "the detached refresh re-reads the saved row and streams by id"
            );
            assert!(!manager.dropped.load(Ordering::SeqCst));
            cancel.cancel();
            for _ in 0..100 {
                if manager.dropped.load(Ordering::SeqCst) {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(manager.dropped.load(Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn blocking_cancellation_drops_work_and_provider_failures_do_not_abort_the_refresh() {
        for fail in [false, true] {
            let manager = Arc::new(HeldManager {
                fail,
                ..Default::default()
            });
            let provider = provider(&manager, TrickplayScanBehavior::Blocking);
            let cancel = ScanCancel::default();
            let work_cancel = cancel.clone();
            let handle = tokio::spawn(async move {
                provider
                    .refresh(
                        Uuid::new_v4(),
                        &movie(),
                        None,
                        &during_scan(),
                        &MetadataRefreshOptions::default(),
                        &work_cancel,
                    )
                    .await
            });
            manager.entered.notified().await;
            if fail {
                manager.release.notify_one();
            } else {
                cancel.cancel();
            }
            assert!(
                handle.await.unwrap() == fail,
                "ordinary custom-provider errors permit the stamp; cancellation aborts the refresh"
            );
            assert!(manager.dropped.load(Ordering::SeqCst));
        }
    }
}
