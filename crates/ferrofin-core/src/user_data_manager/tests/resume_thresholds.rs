//! Every branch of the resume decision a play report makes:
//! `UserDataManager.UpdatePlayState` (v12.4 `UserDataManager.cs:455-529`) and
//! the `LastPlayedDate` stamp `SessionManager` adds around it. The expected
//! values are the C#'s, at the upstream defaults (`MinResumePct` 5,
//! `MaxResumePct` 90, `MinResumeDurationSeconds` 300, `MinAudiobookResume` and
//! `MaxAudiobookResume` 5 minutes) unless a case changes one.

use super::*;
use crate::test_support::seed_user;

const S: i64 = TICKS_PER_SECOND;
const MIN: i64 = 60 * S;
const HOUR: i64 = 60 * MIN;

/// The settings of a case that keeps every default.
fn defaults(_: &mut ServerConfiguration) {}

/// One stop report on a fresh row; `runtime` 0 stores none. `expected` is
/// (`UpdatePlayState`'s answer, `Played`, `PlaybackPositionTicks`).
#[rstest::rstest]
// Video: the percentage rules.
#[case::below_min_pct(BaseItemKind::Movie, None, HOUR, HOUR / 25, defaults, (false, false, 0))]
#[case::at_min_pct(BaseItemKind::Movie, None, HOUR, HOUR / 20, defaults, (false, false, HOUR / 20))]
#[case::at_max_pct(BaseItemKind::Movie, None, HOUR, HOUR * 9 / 10, defaults, (false, false, HOUR * 9 / 10))]
#[case::past_max_pct(BaseItemKind::Movie, None, HOUR, HOUR * 9 / 10 + 1, defaults, (true, true, 0))]
#[case::episode_past_max_pct(BaseItemKind::Episode, None, HOUR, HOUR * 95 / 100, defaults, (true, true, 0))]
#[case::zero_position(BaseItemKind::Movie, None, HOUR, 0, defaults, (false, false, 0))]
// Video: the last second, alone once MaxResumePct cannot fire.
#[case::last_second(BaseItemKind::Movie, None, HOUR, HOUR - S, |c: &mut ServerConfiguration| c.max_resume_pct = 100, (true, true, 0))]
#[case::before_last_second(BaseItemKind::Movie, None, HOUR, HOUR - S - 1, |c: &mut ServerConfiguration| c.max_resume_pct = 100, (false, false, HOUR - S - 1))]
// Video: MinResumeDurationSeconds.
#[case::shorter_than_min_duration(BaseItemKind::Movie, None, 299 * S, 150 * S, defaults, (true, true, 0))]
#[case::at_min_duration(BaseItemKind::Movie, None, 300 * S, 150 * S, defaults, (false, false, 150 * S))]
// A client may report any i64: `position * 100` must not overflow.
#[case::position_i64_max(BaseItemKind::Movie, None, HOUR, i64::MAX, defaults, (true, true, 0))]
// The percentages are C# decimals: an exact 29 % or 55 % is no float's
// 28.999999999999996 or 55.00000000000001.
#[case::exact_min_pct_decimal(BaseItemKind::Movie, None, HOUR, HOUR * 29 / 100, |c: &mut ServerConfiguration| c.min_resume_pct = 29, (false, false, HOUR * 29 / 100))]
#[case::exact_max_pct_decimal(BaseItemKind::Movie, None, HOUR, HOUR * 55 / 100, |c: &mut ServerConfiguration| c.max_resume_pct = 55, (false, false, HOUR * 55 / 100))]
// AudioBook: minutes in and minutes left, never percentages or duration.
#[case::audiobook_below_min_minutes(BaseItemKind::AudioBook, None, HOUR, 5 * MIN - 1, defaults, (false, false, 0))]
#[case::audiobook_at_min_minutes(BaseItemKind::AudioBook, None, HOUR, 5 * MIN, defaults, (false, false, 5 * MIN))]
#[case::audiobook_at_max_minutes_left(BaseItemKind::AudioBook, None, HOUR, HOUR - 5 * MIN, defaults, (false, false, HOUR - 5 * MIN))]
#[case::audiobook_inside_max_minutes_left(BaseItemKind::AudioBook, None, HOUR, HOUR - 5 * MIN + 1, defaults, (true, true, 0))]
#[case::audiobook_past_max_pct(BaseItemKind::AudioBook, None, 10 * HOUR, 10 * HOUR * 95 / 100, defaults, (false, false, 10 * HOUR * 95 / 100))]
#[case::audiobook_shorter_than_min_duration(BaseItemKind::AudioBook, None, 4 * MIN, 2 * MIN, |c: &mut ServerConfiguration| (c.min_audiobook_resume, c.max_audiobook_resume) = (1, 0), (false, false, 2 * MIN))]
#[case::audiobook_at_end(BaseItemKind::AudioBook, None, HOUR, HOUR, |c: &mut ServerConfiguration| c.max_audiobook_resume = 0, (true, true, 0))]
#[case::audiobook_before_end(BaseItemKind::AudioBook, None, HOUR, HOUR - 1, |c: &mut ServerConfiguration| c.max_audiobook_resume = 0, (false, false, HOUR - 1))]
// Book: no threshold applies while it has a runtime.
#[case::book_past_max_pct(BaseItemKind::Book, None, HOUR, HOUR * 95 / 100, defaults, (false, false, HOUR * 95 / 100))]
#[case::book_below_min_pct(BaseItemKind::Book, None, HOUR, HOUR / 100, defaults, (false, false, HOUR / 100))]
// No runtime: assumed played to the end, wherever the report says it stopped.
#[case::movie_no_runtime(BaseItemKind::Movie, None, 0, 10 * MIN, defaults, (true, true, 0))]
#[case::audiobook_no_runtime(BaseItemKind::AudioBook, None, 0, 10 * MIN, defaults, (true, true, 0))]
#[case::book_no_runtime(BaseItemKind::Book, None, 0, 10 * MIN, defaults, (true, true, 0))]
// SupportsPositionTicksResume false: played still counts, the position never.
#[case::song_midway(BaseItemKind::Audio, None, HOUR, HOUR / 2, defaults, (false, false, 0))]
#[case::song_past_max_pct(BaseItemKind::Audio, None, HOUR, HOUR * 95 / 100, defaults, (true, true, 0))]
#[case::trailer_extra_midway(BaseItemKind::Video, Some(2), HOUR, HOUR / 2, defaults, (false, false, 0))]
#[case::featurette_extra_midway(BaseItemKind::Video, Some(10), HOUR, HOUR / 2, defaults, (false, false, HOUR / 2))]
#[case::theme_video_extra_midway(BaseItemKind::Video, Some(9), HOUR, HOUR / 2, defaults, (false, false, 0))]
// SupportsPlayedStatus false: completion is answered but never stored.
#[case::photo_no_runtime(BaseItemKind::Photo, None, 0, 0, defaults, (true, false, 0))]
#[case::photo_past_max_pct(BaseItemKind::Photo, None, HOUR, HOUR * 95 / 100, defaults, (true, false, 0))]
#[tokio::test]
async fn play_report_resume_thresholds(
    #[case] kind: BaseItemKind,
    #[case] extra_type: Option<i32>,
    #[case] runtime: i64,
    #[case] position: i64,
    #[case] setting: fn(&mut ServerConfiguration),
    #[case] expected: (bool, bool, i64),
) {
    let db = test_db().await;
    let (user, item) = (Uuid::from_u128(1), Uuid::from_u128(2));
    seed_user(&db, user).await;
    seed_playable(&db, item, kind, runtime, extra_type).await;
    let mut config = default_server_configuration();
    setting(&mut config);
    let mgr = FerrofinUserDataManager::new(db, Arc::new(FixedConfig { config }));

    let completed = mgr
        .update_play_state_with_reason(
            user,
            item,
            Some(position),
            UserDataSaveReason::PlaybackFinished,
        )
        .await
        .expect("stop report");

    let row = mgr.read_row(item, user).await.expect("read").expect("row");
    assert_eq!(
        (completed, row.played, row.playback_position_ticks),
        expected
    );
    // `if (playedToCompletion || data.PlaybackPositionTicks > 0)` stamps it.
    assert_eq!(
        row.last_played_date.is_some(),
        completed || row.playback_position_ticks > 0
    );
}
