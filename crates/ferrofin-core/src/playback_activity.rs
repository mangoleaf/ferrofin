//! The dashboard's playback activity entries ("{user} is playing {item} on
//! {device}"), written from the session layer's playback events.
//!
//! Port of `PlaybackStartLogger` / `PlaybackStopLogger`
//! (`Jellyfin.Server.Implementations/Events/Consumers/Session/`), the event
//! consumers of the `PlaybackStartEventArgs` / `PlaybackStopEventArgs` that
//! `SessionManager.OnPlaybackStart` / `OnPlaybackStopped` publish. Because they
//! hang off those events, every route that reports playback writes them: the
//! `/Sessions/Playing` reports and the obsolete `/PlayingItems/{itemId}` forms
//! alike.

use std::sync::Arc;

use ferrofin_model::activity::LogLevel;
use ferrofin_traits::activity::{ActivityLogCreate, ActivityManager};
use ferrofin_traits::library::LibraryManager;
use uuid::Uuid;

use crate::event_manager::{FerrofinEventManager, consumer_done};

/// Which of the two playback loggers is writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// C# `PlaybackStartLogger`.
    Start,
    /// C# `PlaybackStopLogger`.
    Stop,
}

/// The fields of a playback event payload the loggers read
/// (`session_manager::playback_event_payload`).
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PlaybackEvent {
    #[serde(default)]
    user_id: Option<Uuid>,
    #[serde(default)]
    user_name: Option<String>,
    #[serde(default)]
    device_name: Option<String>,
    #[serde(default)]
    item_id: Option<Uuid>,
}

/// Registers the two playback loggers on `bus`, for `PlaybackStart` and
/// `PlaybackStopped`. Their writes are awaited, as the session loggers' are,
/// so the entries land in the order the events are published.
pub fn subscribe(
    bus: &FerrofinEventManager,
    activity: &Arc<dyn ActivityManager>,
    library: &Arc<dyn LibraryManager>,
) {
    for (event, phase) in [
        ("PlaybackStart", Phase::Start),
        ("PlaybackStopped", Phase::Stop),
    ] {
        let activity = Arc::clone(activity);
        let library = Arc::clone(library);
        bus.subscribe(
            event,
            Arc::new(move |payload: &str| {
                let Ok(event) = serde_json::from_str::<PlaybackEvent>(payload) else {
                    return consumer_done();
                };
                let activity = Arc::clone(&activity);
                let library = Arc::clone(&library);
                Box::pin(async move {
                    record(activity.as_ref(), library.as_ref(), event, phase).await;
                    Ok(())
                })
            }),
        );
    }
}

/// Writes one playback entry, best-effort: a logging failure never fails the
/// playback report that raised the event.
async fn record(
    activity: &dyn ActivityManager,
    library: &dyn LibraryManager,
    event: PlaybackEvent,
    phase: Phase,
) {
    // `eventArgs.Users.Count == 0`: a user-less session writes nothing.
    let Some(user_name) = event.user_name.filter(|name| !name.is_empty()) else {
        return;
    };
    let item_id = event.item_id.filter(|id| !id.is_nil());
    let item = match item_id {
        Some(id) => match library.get_item_by_id(id).await {
            Ok(item) => item,
            Err(error) => {
                tracing::debug!(item_id = %id, %error, ?phase, "playback activity: item lookup failed");
                return;
            }
        },
        None => None,
    };
    // `eventArgs.MediaInfo is null`: upstream warns and writes nothing. A client
    // naming an item the server does not have is not an operator's problem, so
    // it is `debug` here (LOGGING.md §1).
    let Some(item) = item else {
        tracing::debug!(
            item_id = item_id.map(tracing::field::display),
            ?phase,
            "playback reported without a known item"
        );
        return;
    };
    // "Don't report theme song or local trailer playback."
    if is_theme_media(item.extra_type) {
        return;
    }
    let Some(type_) = notification_type(phase, item.media_type.as_deref()) else {
        // `GetPlaybackStoppedNotificationType` returned null: no entry at all.
        return;
    };
    let action = match phase {
        Phase::Start => "is playing",
        Phase::Stop => "has finished playing",
    };
    let name = format!(
        "{user_name} {action} {} on {}",
        item_name(
            item.name.as_deref(),
            item.series_name.as_deref(),
            item.artists.as_deref()
        ),
        event.device_name.unwrap_or_default()
    );
    let entry = ActivityLogCreate {
        name,
        type_: type_.to_owned(),
        user_id: event.user_id.filter(|id| !id.is_nil()),
        item_id,
        severity: LogLevel::Information,
        ..Default::default()
    };
    if let Err(error) = activity.create_entry(entry).await {
        tracing::debug!(%error, "playback activity entry not written");
    }
}

/// The activity-log `Type` for a playback event, from the item's media type.
///
/// Port of `PlaybackStartLogger.GetPlaybackNotificationType` /
/// `PlaybackStopLogger.GetPlaybackStoppedNotificationType`. `None` means "write
/// no entry": the stop logger returns `null` for anything that is neither audio
/// nor video.
fn notification_type(phase: Phase, media_type: Option<&str>) -> Option<&'static str> {
    let audio = media_type.is_some_and(|m| m.eq_ignore_ascii_case("Audio"));
    let video = media_type.is_some_and(|m| m.eq_ignore_ascii_case("Video"));
    match (phase, audio, video) {
        (Phase::Start, true, _) => Some("AudioPlayback"),
        (Phase::Start, _, true) => Some("VideoPlayback"),
        (Phase::Start, _, _) => Some("Playback"),
        (Phase::Stop, true, _) => Some("AudioPlaybackStopped"),
        (Phase::Stop, _, true) => Some("VideoPlaybackStopped"),
        (Phase::Stop, _, _) => None,
    }
}

/// The display name both playback loggers build (C# `GetItemName`): the item
/// name, prefixed with its series and then its first artist.
fn item_name(name: Option<&str>, series_name: Option<&str>, artists: Option<&str>) -> String {
    let mut out = name.unwrap_or_default().to_owned();
    if let Some(series) = series_name.filter(|s| !s.is_empty()) {
        out = format!("{series} - {out}");
    }
    // The stored column is the `|`-delimited multi-value form; C# takes
    // `item.Artists[0]`.
    if let Some(first) = artists
        .into_iter()
        .flat_map(|a| a.split('|'))
        .find(|a| !a.is_empty())
    {
        out = format!("{first} - {out}");
    }
    out
}

/// Whether the item is theme media, which both loggers skip.
///
/// `ExtraType` 8/9 are `ThemeSong`/`ThemeVideo` in the stored encoding (see
/// `crate::dto_service`).
fn is_theme_media(extra_type: Option<i32>) -> bool {
    matches!(extra_type, Some(8 | 9))
}

#[cfg(test)]
mod tests {
    use super::{Phase, is_theme_media, item_name, notification_type};

    /// C# `PlaybackStartLogger.GetPlaybackNotificationType` /
    /// `PlaybackStopLogger.GetPlaybackStoppedNotificationType`. The type was
    /// hardcoded to the Video pair, so an audio track's play landed in the
    /// dashboard feed as "VideoPlayback".
    #[test]
    fn playback_type_follows_the_items_media_type() {
        use Phase::{Start, Stop};
        assert_eq!(
            notification_type(Start, Some("Audio")),
            Some("AudioPlayback")
        );
        assert_eq!(
            notification_type(Start, Some("Video")),
            Some("VideoPlayback")
        );
        assert_eq!(
            notification_type(Stop, Some("Audio")),
            Some("AudioPlaybackStopped")
        );
        assert_eq!(
            notification_type(Stop, Some("Video")),
            Some("VideoPlaybackStopped")
        );
        // Neither audio nor video: the start logger falls back to "Playback"…
        assert_eq!(notification_type(Start, Some("Book")), Some("Playback"));
        assert_eq!(notification_type(Start, None), Some("Playback"));
        // …and the stop logger returns null, i.e. writes NO entry at all.
        assert_eq!(notification_type(Stop, Some("Book")), None);
        assert_eq!(notification_type(Stop, None), None);
    }

    /// C# `GetItemName`: series first, then the first artist, then the name.
    #[test]
    fn playback_item_name_prefixes_series_then_artist() {
        assert_eq!(item_name(Some("Track 01"), None, None), "Track 01");
        assert_eq!(
            item_name(Some("Pilot"), Some("Series 01"), None),
            "Series 01 - Pilot"
        );
        // The stored column is the `|`-delimited multi-value form; C# takes [0].
        assert_eq!(
            item_name(Some("Track 01"), None, Some("Artist A|Artist B")),
            "Artist A - Track 01"
        );
        assert_eq!(
            item_name(Some("Ep"), Some("Show"), Some("A")),
            "A - Show - Ep"
        );
        // Empty strings must not produce a bare separator.
        assert_eq!(item_name(Some("X"), Some(""), Some("")), "X");
        assert_eq!(item_name(None, None, None), "");
    }

    /// Both loggers bail on theme media ("Don't report theme song or local
    /// trailer playback"). 8/9 are ThemeSong/ThemeVideo in the stored encoding.
    #[test]
    fn theme_media_is_recognised_by_extra_type() {
        assert!(is_theme_media(Some(8)));
        assert!(is_theme_media(Some(9)));
        assert!(!is_theme_media(Some(1)));
        assert!(!is_theme_media(None));
    }
}
