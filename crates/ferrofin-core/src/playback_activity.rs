//! The dashboard's playback activity entries ("{user} is playing {item} on
//! {device}"), written from the session layer's playback events.
//!
//! Port of `PlaybackStartLogger` / `PlaybackStopLogger`
//! (`Jellyfin.Server.Implementations/Events/Consumers/Session/`), the event
//! consumers of the `PlaybackStartEventArgs` / `PlaybackStopEventArgs` that
//! `SessionManager.OnPlaybackStart` / `OnPlaybackStopped` publish. Because they
//! hang off those events, every route that reports playback writes them: the
//! `/Sessions/Playing` reports and the obsolete `/PlayingItems/{itemId}` forms
//! alike. As upstream's read `eventArgs.MediaInfo` and `eventArgs.Item`, they
//! write from the items the event carries and read nothing.

use std::sync::Arc;

use ferrofin_model::activity::LogLevel;
use ferrofin_traits::activity::{ActivityLogCreate, ActivityManager};
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
    /// `null` when the report neither carried an `Item` nor named one the
    /// library has.
    #[serde(default)]
    media_info: Option<MediaInfo>,
    /// `null` when the library has no item of that id.
    #[serde(default)]
    item: Option<LibraryItem>,
}

/// The item a playback event names (`session_manager::playback_media_info`,
/// `reported_media_info`): the members of C# `eventArgs.MediaInfo` the loggers
/// read.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct MediaInfo {
    name: Option<String>,
    series_name: Option<String>,
    artists: Vec<String>,
    media_type: Option<String>,
}

/// The library item a playback event names
/// (`session_manager::playback_library_item`): C# `eventArgs.Item`, its `Id`
/// and its `ExtraType` for `IsThemeMedia`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct LibraryItem {
    id: Option<Uuid>,
    extra_type: Option<String>,
}

/// Registers the two playback loggers on `bus`, for `PlaybackStart` and
/// `PlaybackStopped`. Their writes are awaited, as the session loggers' are,
/// so the entries land in the order the events are published.
pub fn subscribe(bus: &FerrofinEventManager, activity: &Arc<dyn ActivityManager>) {
    for (event, phase) in [
        ("PlaybackStart", Phase::Start),
        ("PlaybackStopped", Phase::Stop),
    ] {
        let activity = Arc::clone(activity);
        bus.subscribe(
            event,
            Arc::new(move |payload: &str| {
                let Ok(event) = serde_json::from_str::<PlaybackEvent>(payload) else {
                    return consumer_done();
                };
                let activity = Arc::clone(&activity);
                Box::pin(async move {
                    record(activity.as_ref(), event, phase).await;
                    Ok(())
                })
            }),
        );
    }
}

/// Writes one playback entry, best-effort: a logging failure never fails the
/// playback report that raised the event.
async fn record(activity: &dyn ActivityManager, event: PlaybackEvent, phase: Phase) {
    // `eventArgs.Users.Count == 0`: a user-less session writes nothing.
    let Some(user_name) = event.user_name.filter(|name| !name.is_empty()) else {
        return;
    };
    let item_id = event.item_id.filter(|id| !id.is_nil());
    // `eventArgs.MediaInfo is null`: upstream warns and writes nothing. A client
    // naming an item the server does not have is not an operator's problem, so
    // it is `debug` here (LOGGING.md §1).
    let Some(item) = event.media_info else {
        tracing::debug!(
            item_id = item_id.map(tracing::field::display),
            ?phase,
            "playback reported without a known item"
        );
        return;
    };
    // `eventArgs.Item is not null && eventArgs.Item.IsThemeMedia`: "Don't
    // report theme song or local trailer playback."
    if is_theme_media(
        event
            .item
            .as_ref()
            .and_then(|item| item.extra_type.as_deref()),
    ) {
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
            &item.artists
        ),
        event.device_name.unwrap_or_default()
    );
    let entry = ActivityLogCreate {
        name,
        type_: type_.to_owned(),
        user_id: event.user_id.filter(|id| !id.is_nil()),
        // `ItemId = eventArgs.Item?.Id`: none for an item the library lacks.
        item_id: event.item.and_then(|item| item.id),
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
fn item_name(name: Option<&str>, series_name: Option<&str>, artists: &[String]) -> String {
    let mut out = name.unwrap_or_default().to_owned();
    if let Some(series) = series_name.filter(|s| !s.is_empty()) {
        out = format!("{series} - {out}");
    }
    // C# `item.Artists[0]`.
    if let Some(first) = artists.iter().find(|a| !a.is_empty()) {
        out = format!("{first} - {out}");
    }
    out
}

/// Whether the item is theme media, which both loggers skip (C#
/// `BaseItem.IsThemeMedia`).
fn is_theme_media(extra_type: Option<&str>) -> bool {
    matches!(extra_type, Some("ThemeSong" | "ThemeVideo"))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use ferrofin_model::activity::ActivityLogEntry;
    use ferrofin_model::querying::QueryResult;
    use ferrofin_traits::activity::{ActivityLogCreate, ActivityLogQuery, ActivityManager};
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::events::EventManager as _;
    use uuid::Uuid;

    use super::{Phase, is_theme_media, item_name, notification_type, subscribe};
    use crate::event_manager::FerrofinEventManager;

    /// The entries written, as (name, type, item id).
    #[derive(Default)]
    struct Recorded(Mutex<Vec<(String, String, Option<Uuid>)>>);

    #[async_trait]
    impl ActivityManager for Recorded {
        async fn get_paged_result(
            &self,
            _query: &ActivityLogQuery,
        ) -> Result<QueryResult<ActivityLogEntry>, ServiceError> {
            unreachable!("the loggers only write")
        }
        async fn create_entry(&self, entry: ActivityLogCreate) -> Result<(), ServiceError> {
            self.0
                .lock()
                .unwrap()
                .push((entry.name, entry.type_, entry.item_id));
            Ok(())
        }
        async fn clean(&self, _before: DateTime<Utc>) -> Result<u64, ServiceError> {
            unreachable!("the loggers only write")
        }
    }

    /// The loggers write from the event's `MediaInfo`, as upstream's read
    /// `eventArgs.MediaInfo`: an audio item is named from its first artist and
    /// its series. The library item (`eventArgs.Item`) gives the entry its
    /// `ItemId`, none for an item the library does not have, and theme media
    /// writes nothing.
    #[tokio::test]
    async fn the_loggers_write_from_the_events_media_info() {
        let bus = FerrofinEventManager::new();
        let recorded = Arc::new(Recorded::default());
        subscribe(&bus, &(Arc::clone(&recorded) as Arc<dyn ActivityManager>));
        let library_item = Uuid::new_v4();
        let event = |item: serde_json::Value| {
            serde_json::json!({
                "UserId": Uuid::new_v4(), "UserName": "viewer", "DeviceName": "TV",
                "ItemId": Uuid::new_v4(),
                "MediaInfo": {
                    "Name": "Track", "SeriesName": "Show", "Artists": ["A", "B"],
                    "MediaType": "Audio",
                },
                "Item": item,
            })
            .to_string()
        };
        bus.publish(
            "PlaybackStart",
            &event(serde_json::json!({ "Id": library_item, "ExtraType": "ThemeSong" })),
        )
        .await
        .unwrap();
        bus.publish(
            "PlaybackStopped",
            &event(serde_json::json!({ "Id": library_item, "ExtraType": null })),
        )
        .await
        .unwrap();
        bus.publish("PlaybackStart", &event(serde_json::Value::Null))
            .await
            .unwrap();
        assert_eq!(
            *recorded.0.lock().unwrap(),
            [
                (
                    "viewer has finished playing A - Show - Track on TV".to_owned(),
                    "AudioPlaybackStopped".to_owned(),
                    Some(library_item)
                ),
                (
                    "viewer is playing A - Show - Track on TV".to_owned(),
                    "AudioPlayback".to_owned(),
                    None
                )
            ]
        );
    }

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
        let artists = |names: &[&str]| names.iter().map(|&n| n.to_owned()).collect::<Vec<_>>();
        assert_eq!(item_name(Some("Track 01"), None, &[]), "Track 01");
        assert_eq!(
            item_name(Some("Pilot"), Some("Series 01"), &[]),
            "Series 01 - Pilot"
        );
        // C# takes `Artists[0]`.
        assert_eq!(
            item_name(Some("Track 01"), None, &artists(&["Artist A", "Artist B"])),
            "Artist A - Track 01"
        );
        assert_eq!(
            item_name(Some("Ep"), Some("Show"), &artists(&["A"])),
            "A - Show - Ep"
        );
        // Empty strings must not produce a bare separator.
        assert_eq!(item_name(Some("X"), Some(""), &artists(&[""])), "X");
        assert_eq!(item_name(None, None, &[]), "");
    }

    /// Both loggers bail on theme media ("Don't report theme song or local
    /// trailer playback").
    #[test]
    fn theme_media_is_recognised_by_extra_type() {
        assert!(is_theme_media(Some("ThemeSong")));
        assert!(is_theme_media(Some("ThemeVideo")));
        assert!(!is_theme_media(Some("Clip")));
        assert!(!is_theme_media(None));
    }
}
