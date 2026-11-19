//! The plugin's segment editor service (`Manager/MediaSegmentEditorService.cs`):
//! how an edit from the plugin dashboard's segment editor reaches the
//! published `MediaSegments`.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::intro_skipper as intro_store;
use ferrofin_traits::media_segments::MediaSegmentManager;
use uuid::Uuid;

/// `MediaSegmentEditorService._itemLocks`: one lock per edited item, kept for
/// the process's lifetime.
// ponytail: never evicted, as upstream ("re-add eviction if touched item count
// becomes measurable") — one small entry per item ever edited by hand.
fn item_lock(item_id: Uuid) -> Arc<tokio::sync::Mutex<()>> {
    type Locks = HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>;
    static LOCKS: LazyLock<Mutex<Locks>> = LazyLock::new(Default::default);
    let mut locks = LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(locks.entry(item_id).or_default())
}

/// `MediaSegmentEditorService.CreateOrReplaceSegmentAsync`, under the item's
/// lock so two simultaneous edits publish one segment: a Commercial identical
/// to one already published is skipped; any other type replaces the item's
/// segments of that type (any provider); then the edit is published under the
/// plugin's provider id. An item that takes no segments (not a video or
/// audio) is left alone.
pub(super) async fn create_or_replace_segment(
    segments: &dyn MediaSegmentManager,
    segment: &MediaSegmentDto,
) -> Result<(), ServiceError> {
    // Jellyfin's `GetSupportedProviders(item)` is empty for anything but a Video or Audio.
    if !segments.is_type_supported(segment.item_id).await? {
        tracing::warn!(item_id = %segment.item_id, "intro skipper: the item is not a video or audio, or is gone; the edit is not published");
        return Ok(());
    }
    let lock = item_lock(segment.item_id);
    let _held = lock.lock().await;
    let existing = segments
        .get_segments(segment.item_id, Some(&[segment.type_]), false)
        .await?;
    if segment.type_ == MediaSegmentType::Commercial {
        if existing
            .iter()
            .any(|e| e.start_ticks == segment.start_ticks && e.end_ticks == segment.end_ticks)
        {
            return Ok(());
        }
    } else {
        for e in existing {
            // Upstream logs a failed delete and carries on with the others.
            if let Err(err) = segments.delete_segment(e.id).await {
                tracing::warn!(%err, segment_id = %e.id, "intro skipper: could not delete a replaced segment");
            }
        }
    }
    segments
        .create_segment(segment, &intro_store::provider_id())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::create_or_replace_segment;
    use async_trait::async_trait;
    use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::intro_skipper as intro_store;
    use ferrofin_traits::media_segments::{MediaSegmentManager, MediaSegmentProviderInfo};
    use rstest::rstest;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::Notify;
    use uuid::Uuid;

    /// Segments in memory, by provider; each read yields after taking its
    /// snapshot, as a real store awaits there, so a concurrent edit reads the
    /// same snapshot. `fail_deletes` refuses every delete; a read of `gate`'s
    /// item waits for its notification; `unsupported` items take no segments.
    #[derive(Default)]
    struct YieldingSegments {
        rows: Mutex<Vec<(String, MediaSegmentDto)>>,
        fail_deletes: bool,
        gate: Option<(Uuid, Arc<Notify>)>,
        unsupported: bool,
    }

    impl YieldingSegments {
        fn rows(&self) -> Vec<(String, MediaSegmentDto)> {
            self.rows.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl MediaSegmentManager for YieldingSegments {
        async fn is_type_supported(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
            Ok(!self.unsupported)
        }
        async fn create_segment(
            &self,
            segment: &MediaSegmentDto,
            provider: &str,
        ) -> Result<MediaSegmentDto, ServiceError> {
            let mut rows = self.rows.lock().expect("lock");
            let created = MediaSegmentDto {
                id: Uuid::from_u128(rows.len() as u128 + 1),
                ..segment.clone()
            };
            rows.push((provider.to_owned(), created.clone()));
            Ok(created)
        }
        async fn delete_segment(&self, segment_id: Uuid) -> Result<(), ServiceError> {
            if self.fail_deletes {
                return Err(ServiceError::backend("delete failed"));
            }
            self.rows
                .lock()
                .expect("lock")
                .retain(|(_, s)| s.id != segment_id);
            Ok(())
        }
        async fn delete_segments(&self, _item_id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn delete_provider_segments(
            &self,
            _item_id: Uuid,
            _provider_id: &str,
            _type_filter: Option<MediaSegmentType>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_segments(
            &self,
            item_id: Uuid,
            types: Option<&[MediaSegmentType]>,
            _by_provider: bool,
        ) -> Result<Vec<MediaSegmentDto>, ServiceError> {
            if let Some((blocked, gate)) = &self.gate
                && *blocked == item_id
            {
                gate.notified().await;
            }
            let found = self
                .rows
                .lock()
                .expect("lock")
                .iter()
                .map(|(_, s)| s)
                .filter(|s| s.item_id == item_id && types.is_none_or(|t| t.contains(&s.type_)))
                .cloned()
                .collect();
            tokio::task::yield_now().await;
            Ok(found)
        }
        async fn has_segments(&self, _item_id: Uuid) -> Result<bool, ServiceError> {
            Ok(false)
        }
        async fn get_supported_providers(
            &self,
            _item_id: Uuid,
        ) -> Result<Vec<MediaSegmentProviderInfo>, ServiceError> {
            Ok(Vec::new())
        }
    }

    fn edit(
        item_id: Uuid,
        type_: MediaSegmentType,
        start_ticks: i64,
        end_ticks: i64,
    ) -> MediaSegmentDto {
        MediaSegmentDto {
            id: Uuid::nil(),
            item_id,
            type_,
            start_ticks,
            end_ticks,
        }
    }

    /// `MediaSegmentEditorService`'s per-item lock: two simultaneous edits of
    /// one item's Intro publish one segment, the later one.
    #[tokio::test]
    async fn simultaneous_edits_of_an_item_publish_one_segment() {
        let segments = Arc::new(YieldingSegments::default());
        let item = Uuid::from_u128(0x5d);
        let (first, second) = (
            edit(item, MediaSegmentType::Intro, 0, 10),
            edit(item, MediaSegmentType::Intro, 0, 20),
        );
        let (a, b) = tokio::join!(
            create_or_replace_segment(segments.as_ref(), &first),
            create_or_replace_segment(segments.as_ref(), &second),
        );
        a.expect("first");
        b.expect("second");
        let rows = segments.rows();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].1.end_ticks, 20);
    }

    /// `DoesNothing_WhenProviderNotFound`: Jellyfin offers no segment provider
    /// for an item that is not a video or audio (`GetSupportedProviders`), so
    /// such an edit is not published and replaces nothing.
    #[tokio::test]
    async fn an_item_that_takes_no_segments_is_left_alone() {
        let item = Uuid::new_v4();
        let segments = YieldingSegments {
            unsupported: true,
            ..YieldingSegments::default()
        };
        segments
            .create_segment(&edit(item, MediaSegmentType::Intro, 0, 5), "Other")
            .await
            .expect("seed");
        create_or_replace_segment(&segments, &edit(item, MediaSegmentType::Intro, 10, 20))
            .await
            .expect("edit");
        let rows = segments.rows();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].0, "Other", "{rows:?}");
    }

    /// `AllowsConcurrentCallsForDifferentItems`: the lock is per item, so an
    /// edit parked inside one item's lock does not hold up another item's.
    #[tokio::test]
    async fn an_edit_of_another_item_does_not_wait() {
        let (parked, other) = (Uuid::new_v4(), Uuid::new_v4());
        let gate = Arc::new(Notify::new());
        let segments = YieldingSegments {
            gate: Some((parked, Arc::clone(&gate))),
            ..YieldingSegments::default()
        };
        let first_edit = edit(parked, MediaSegmentType::Intro, 0, 10);
        let first = create_or_replace_segment(&segments, &first_edit);
        tokio::pin!(first);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut first)
                .await
                .is_err(),
            "the first edit parks inside its item's lock"
        );
        tokio::time::timeout(
            Duration::from_secs(5),
            create_or_replace_segment(&segments, &edit(other, MediaSegmentType::Intro, 0, 20)),
        )
        .await
        .expect("another item's edit does not wait for the parked one")
        .expect("other");
        gate.notify_one();
        first.await.expect("first");
        assert_eq!(segments.rows().len(), 2);
    }

    /// `TestMediaSegmentEditorService`: a non-Commercial edit replaces the
    /// item's segments of its type (`ReplacesExisting_ForNonCommercialType`,
    /// even when a delete fails: `StillCreates_WhenDeletingExistingFails`); a
    /// Commercial one is added unless identical
    /// (`SkipsCreate_WhenIdenticalCommercialExists`,
    /// `CreatesCommercial_WhenNoIdenticalExists`). Other types are untouched
    /// and the edit is published under the plugin's provider id. Expected:
    /// the rows of the edit's type, as (published by the plugin, start, end).
    #[rstest]
    #[case::replaces_its_type(MediaSegmentType::Intro, (10, 20), false, &[(true, 10, 20)])]
    #[case::still_creates_when_a_delete_fails(
        MediaSegmentType::Intro,
        (10, 20),
        true,
        &[(false, 0, 5), (false, 5, 9), (true, 10, 20)]
    )]
    #[case::skips_an_identical_commercial(
        MediaSegmentType::Commercial,
        (0, 5),
        false,
        &[(false, 0, 5), (false, 5, 9)]
    )]
    #[case::adds_another_commercial(
        MediaSegmentType::Commercial,
        (30, 40),
        false,
        &[(false, 0, 5), (false, 5, 9), (true, 30, 40)]
    )]
    #[tokio::test]
    async fn edits_follow_create_or_replace_segment_async(
        #[case] type_: MediaSegmentType,
        #[case] (start, end): (i64, i64),
        #[case] fail_deletes: bool,
        #[case] expected: &[(bool, i64, i64)],
    ) {
        let item = Uuid::new_v4();
        let segments = YieldingSegments {
            fail_deletes,
            ..YieldingSegments::default()
        };
        for (kind, from, to) in [
            (type_, 0, 5),
            (type_, 5, 9),
            (MediaSegmentType::Outro, 0, 5),
        ] {
            segments
                .create_segment(&edit(item, kind, from, to), "Other")
                .await
                .expect("seed");
        }
        create_or_replace_segment(&segments, &edit(item, type_, start, end))
            .await
            .expect("edit");
        let rows = segments.rows();
        let mut of_type: Vec<(bool, i64, i64)> = rows
            .iter()
            .filter(|(_, s)| s.type_ == type_)
            .map(|(provider, s)| {
                (
                    *provider == intro_store::provider_id(),
                    s.start_ticks,
                    s.end_ticks,
                )
            })
            .collect();
        of_type.sort_unstable_by_key(|&(_, from, to)| (from, to));
        assert_eq!(of_type, expected, "{rows:?}");
        assert!(
            rows.iter().any(|(_, s)| s.type_ == MediaSegmentType::Outro),
            "{rows:?}"
        );
    }
}
