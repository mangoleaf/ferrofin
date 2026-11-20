//! A series timer's showings across a change of their channel's external id.
//!
//! An airing's listing key ends in the external id of the channel it is bound
//! to (`ListingsManager.GetProgramsAsync`: `program.Id += "_" + channel.Id`),
//! and a series child's id is derived from that key. v1.5.2 maps an HDHomeRun
//! channel under `hdhr_{GuideNumber}` where earlier releases used the M3U
//! shape, so the first guide refresh after the upgrade gives every showing on
//! such a channel a new key, and every series child a new id (gl-1863).

use std::path::PathBuf;

use ferrofin_model::live_tv::{ListingsProviderInfo, RecordingStatus, TimerInfoDto};
use ferrofin_traits::stubs::LiveTvManager;
use uuid::Uuid;

use super::{FakeFetcher, airings_of, as_xmltv, defaults_for, guide_of, hdhr_device, hdhr_tuner};
use crate::manager::{
    DELETE_TIMER_SQL, FerrofinLiveTvManager, FiringClaim, internal_program_id, series_child_timer,
};

/// The id v1.5.2 maps the HDHomeRun's channel 4.2 under.
const HDHR_KEY: &str = "hdhr_4.2";

/// The id earlier releases mapped the same channel under:
/// `m3u_{MD5(tuner url)}{MD5(stream url)}`, whatever host minted the channel.
fn previous_key() -> String {
    crate::mapping::m3u_channel_id(
        "http://192.168.1.182",
        "http://192.168.1.111:5004/auto/v4.2",
    )
}

/// A manager whose HDHomeRun channel 4.2 takes `guide`'s `one.tv` listings,
/// with a series timer on its "Recurring" airings; returns the series timer's
/// id.
async fn hdhr_series(guide: String) -> (FerrofinLiveTvManager, String) {
    let mut sources = hdhr_device();
    sources.insert("http://guide/xmltv.xml".to_owned(), guide);
    let mgr = super::manager_with(FakeFetcher(sources)).await;
    // No capture starts here: a showing under way fires at once, and a fire
    // stands down once the server has stopped.
    mgr.stopped.send_replace(true);
    mgr.save_tuner_host(hdhr_tuner()).await.expect("tuner");
    let provider = mgr
        .save_listing_provider(
            as_xmltv(ListingsProviderInfo {
                path: Some("http://guide/xmltv.xml".to_owned()),
                ..ListingsProviderInfo::default()
            }),
            false,
            false,
        )
        .await
        .expect("provider")
        .id
        .expect("provider id");
    mgr.refresh_guide().await.expect("refresh");
    mgr.set_channel_mapping(&provider, HDHR_KEY, "one.tv")
        .await
        .expect("map");
    mgr.refresh_guide().await.expect("mapped refresh");
    let airing = airings_of(&mgr, "Recurring")
        .await
        .into_iter()
        .next()
        .expect("an airing");
    let series = mgr
        .create_series_timer(defaults_for(&mgr, &airing).await)
        .await
        .expect("series timer");
    (mgr, series)
}

/// The series timer's stored children, earliest first.
async fn children(mgr: &FerrofinLiveTvManager, series: &str) -> Vec<TimerInfoDto> {
    let mut timers: Vec<TimerInfoDto> = mgr
        .all_timers()
        .await
        .expect("timers")
        .into_iter()
        .filter(|t| t.series_timer_id.as_deref() == Some(series))
        .collect();
    timers.sort_by_key(|t| t.base.start_date);
    timers
}

/// `key` as an earlier release bound it: ending in [`previous_key`] instead
/// of [`HDHR_KEY`].
fn before_the_rekey(key: &str) -> String {
    format!(
        "{}{}",
        key.strip_suffix(HDHR_KEY).expect("bound to 4.2"),
        previous_key()
    )
}

/// Replaces `child`'s row with one stored for the same showing under the
/// listing key `old_key` gives, and so under the id derived from it. `edit`
/// leaves it as a person or a capture did.
async fn replace_child(
    mgr: &FerrofinLiveTvManager,
    series: &str,
    child: &TimerInfoDto,
    old_key: fn(&str) -> String,
    edit: impl FnOnce(&mut TimerInfoDto),
) -> TimerInfoDto {
    let stored = mgr
        .stored_series_timer(series)
        .await
        .expect("read")
        .expect("series timer");
    let program = child.base.program_id.as_deref().expect("programme");
    let mut airing = mgr
        .program_row(Uuid::parse_str(program).expect("guid"))
        .await
        .expect("read")
        .expect("airing");
    let old_key = old_key(airing.external_id.as_deref().expect("listing key"));
    airing.id = internal_program_id(&old_key).to_string();
    airing.external_id = Some(old_key);
    let mut old = series_child_timer(&airing, &stored, child.base.external_channel_id.clone());
    edit(&mut old);
    assert_ne!(old.base.id, child.base.id);

    let id = child.base.id.as_deref().expect("id");
    mgr.disarm_timer(id);
    mgr.delete_by_id(DELETE_TIMER_SQL, id)
        .await
        .expect("delete");
    mgr.persist_timer(&old).await.expect("plant");
    old
}

/// A showing a person cancelled by hand stays cancelled, and one they edited
/// keeps its edit: each child is carried to the id the pass now derives, with
/// its status, its `IsManual` flag and its own settings, as a child found by
/// id keeps them (`UpdateTimersForSeriesTimer`, v12.4
/// DefaultLiveTvService.cs:716-774). Without that, the refresh scheduled the
/// cancelled showing again under its new id.
#[tokio::test]
async fn a_rekeyed_channel_keeps_its_series_showings_as_they_were() {
    let (mgr, series) = hdhr_series(guide_of(&[
        ("one.tv", 60, 120, "Recurring", "Drama"),
        ("one.tv", 1500, 1560, "Recurring", "Drama"),
    ]))
    .await;
    let now = children(&mgr, &series).await;
    assert_eq!(now.len(), 2, "{now:?}");

    let cancelled = replace_child(&mgr, &series, &now[0], before_the_rekey, |_| {}).await;
    mgr.cancel_timer(cancelled.base.id.as_deref().expect("id"))
        .await
        .expect("cancel by hand");
    let edited = replace_child(&mgr, &series, &now[1], before_the_rekey, |t| {
        t.base.pre_padding_seconds = 300;
    })
    .await;
    assert_ne!(
        edited.base.pre_padding_seconds,
        now[1].base.pre_padding_seconds
    );

    mgr.refresh_guide().await.expect("refresh");

    let after = children(&mgr, &series).await;
    let ids: Vec<_> = after.iter().map(|t| t.base.id.clone()).collect();
    assert_eq!(
        ids,
        [now[0].base.id.clone(), now[1].base.id.clone()],
        "one child per showing, under the id derived from its new key"
    );
    assert_eq!(after[0].status, RecordingStatus::Cancelled, "{after:?}");
    assert!(
        crate::dvr_repository::timer_is_manual(&mgr.db, now[0].base.id.as_deref().expect("id"))
            .await
            .expect("flag"),
        "the cancel is still the person's"
    );
    // With the earliest showing off the table, the duplicate-showing sweep
    // leaves the next one to record.
    assert_eq!(after[1].status, RecordingStatus::New);
    assert_eq!(after[1].base.pre_padding_seconds, 300);
    for (child, was) in after.iter().zip(&now) {
        assert_eq!(child.base.external_program_id, was.base.external_program_id);
        assert_eq!(child.base.program_id, was.base.program_id);
    }
}

/// A showing under way, its child stored as an earlier release left it while
/// it records: the manager, the series timer's id, the child the pass now
/// derives for the showing, and the stored child's id.
async fn under_way() -> (FerrofinLiveTvManager, String, TimerInfoDto, String) {
    let (mgr, series) = hdhr_series(guide_of(&[("one.tv", -10, 50, "Recurring", "Drama")])).await;
    let now = children(&mgr, &series).await;
    assert_eq!(now.len(), 1, "{now:?}");
    let recording = replace_child(&mgr, &series, &now[0], before_the_rekey, |t| {
        t.status = RecordingStatus::InProgress;
    })
    .await;
    let recording_id = recording.base.id.expect("id");
    let derived = now.into_iter().next().expect("child");
    (mgr, series, derived, recording_id)
}

/// Two refreshes leave the showing one child, under `recording_id`, with the
/// airing's new key.
async fn assert_kept_across_refreshes(
    mgr: &FerrofinLiveTvManager,
    series: &str,
    derived: &TimerInfoDto,
    recording_id: &str,
) -> TimerInfoDto {
    let mut last = None;
    for pass in ["the first refresh", "a later refresh"] {
        mgr.refresh_guide().await.expect("refresh");
        let mut after = children(mgr, series).await;
        assert_eq!(after.len(), 1, "{pass}: {after:?}");
        let child = after.remove(0);
        assert_eq!(child.base.id.as_deref(), Some(recording_id), "{pass}");
        assert_eq!(
            child.base.external_program_id, derived.base.external_program_id,
            "{pass}"
        );
        last = Some(child);
    }
    last.expect("a pass")
}

/// A showing being recorded keeps the child whose capture is running: the
/// fire guards are keyed on its id (`FiringClaim`, the active recordings), so
/// a child under the new id would fire at once and capture the showing again.
/// It takes the airing's new key, so the refresh's timer pass keeps it
/// (`RefreshTimers` deletes a timer whose programme left the guide) and a
/// later refresh finds it by programme.
#[tokio::test]
async fn a_rekeyed_channel_does_not_capture_a_showing_under_way_again() {
    let (mgr, series, derived, recording_id) = under_way().await;
    mgr.active_recordings_lock().insert(
        recording_id.clone(),
        crate::dvr::ActiveRecording::new(recording_id.clone(), PathBuf::from("/recordings/r.ts")),
    );
    let child = assert_kept_across_refreshes(&mgr, &series, &derived, &recording_id).await;
    assert_eq!(child.status, RecordingStatus::InProgress);
}

/// The same while the fire still holds its claim and the capture is not
/// registered yet (the tuner is opening): the claim alone keeps the child's id.
#[tokio::test]
async fn a_rekeyed_channel_keeps_a_showing_whose_capture_is_starting() {
    let (mgr, series, derived, recording_id) = under_way().await;
    let _starting = FiringClaim::take(&mgr.firing, &recording_id).expect("claim");
    assert_kept_across_refreshes(&mgr, &series, &derived, &recording_id).await;
}

/// Only the same listing under another channel id is carried over. A stored
/// child at the same slot under another listing (the channel was mapped to
/// another guide channel) is left to upstream's pass, which schedules the
/// showing afresh, and a `Completed` child is left alone.
#[tokio::test]
async fn only_the_same_listing_under_another_channel_id_is_carried_over() {
    let (mgr, series) = hdhr_series(guide_of(&[
        ("one.tv", 60, 120, "Recurring", "Drama"),
        ("one.tv", 1500, 1560, "Recurring", "Drama"),
    ]))
    .await;
    let now = children(&mgr, &series).await;
    assert_eq!(now.len(), 2, "{now:?}");

    let remapped = replace_child(
        &mgr,
        &series,
        &now[0],
        |key| format!("two.tv{}", key.strip_prefix("one.tv").expect("one.tv")),
        |_| {},
    )
    .await;
    let remapped_id = remapped.base.id.clone().expect("id");
    mgr.cancel_timer(&remapped_id)
        .await
        .expect("cancel by hand");
    let completed = replace_child(&mgr, &series, &now[1], before_the_rekey, |t| {
        t.status = RecordingStatus::Completed;
    })
    .await;
    let completed_id = completed.base.id.clone().expect("id");

    mgr.refresh_guide().await.expect("refresh");

    let after = children(&mgr, &series).await;
    let first = after
        .iter()
        .find(|t| t.base.id == now[0].base.id)
        .expect("the first showing scheduled afresh");
    assert_eq!(first.status, RecordingStatus::New, "{after:?}");
    assert!(
        after
            .iter()
            .all(|t| t.base.id.as_deref() != Some(remapped_id.as_str())),
        "the other listing's child left the guide with its airing: {after:?}"
    );
    let kept = after
        .iter()
        .find(|t| t.base.id.as_deref() == Some(completed_id.as_str()))
        .expect("the completed child keeps its id");
    assert_eq!(kept.status, RecordingStatus::Completed);
    assert!(
        after.iter().any(|t| t.base.id == now[1].base.id),
        "{after:?}"
    );
}
