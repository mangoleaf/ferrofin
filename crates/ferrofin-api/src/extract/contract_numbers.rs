//! Every numeric member reachable from either vendored request-body contract.
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

/// Checks the real DTO against each numeric field in its contract inventory.
/// Comparing serialized native/quoted results also catches accepted but
/// incorrectly bound values. Handlers with intentionally narrower wire DTOs
/// still receive every contract field in both forms.
pub(crate) fn check_model<T: Serialize + DeserializeOwned>(schema: &str, baseline: T) -> usize {
    let inventory: Value = serde_json::from_str(include_str!(
        "../../tests/data/json-binding/body-inventory.json"
    ))
    .unwrap();
    let baseline = serde_json::to_value(baseline).unwrap();
    let mut checked = 0;
    for row in inventory["numeric_fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["schema"] == schema)
    {
        let field = row["field"].as_str().unwrap();
        let mut native = baseline.clone();
        native[field] = json!(1);
        let native: T = super::deserialize_mvc(&native.to_string())
            .unwrap_or_else(|e| panic!("{schema}.{field} native: {e}"));
        let native = serde_json::to_value(native).unwrap();
        let mut quoted = baseline.clone();
        quoted[field] = json!("1");
        let quoted: T = super::deserialize_mvc(&quoted.to_string())
            .unwrap_or_else(|e| panic!("{schema}.{field} quoted: {e}"));
        assert_eq!(
            serde_json::to_value(quoted).unwrap(),
            native,
            "{schema}.{field}"
        );
        checked += 1;
    }
    assert!(checked > 0, "{schema} must be in the request inventory");
    checked
}

#[test]
#[allow(clippy::too_many_lines)] // Keep the full contract inventory together.
fn every_model_body_numeric_field_accepts_quoted_numbers() {
    let mut checked = 0;
    checked += check_model(
        "AccessSchedule",
        ferrofin_model::users::AccessSchedule {
            id: 0,
            user_id: uuid::Uuid::nil(),
            day_of_week: ferrofin_model::users::DynamicDayOfWeek::Everyday,
            start_hour: 0.0,
            end_hour: 0.0,
        },
    );
    checked += check_model("AlbumInfo", ferrofin_model::providers::AlbumInfo::default());
    checked += check_model(
        "ArtistInfo",
        ferrofin_model::providers::ArtistInfo::default(),
    );
    checked += check_model("BaseItemDto", ferrofin_model::dto::BaseItemDto::default());
    checked += check_model("BookInfo", ferrofin_model::providers::BookInfo::default());
    checked += check_model(
        "BoxSetInfo",
        ferrofin_model::providers::BoxSetInfo::default(),
    );
    checked += check_model(
        "BufferRequestDto",
        ferrofin_model::sync_play::BufferRequestDto::default(),
    );
    checked += check_model(
        "ChapterInfo",
        ferrofin_model::entities_media::ChapterInfo::default(),
    );
    checked += check_model(
        "DeviceOptionsDto",
        ferrofin_model::dto::DeviceOptionsDto::default(),
    );
    checked += check_model(
        "DeviceProfile",
        ferrofin_model::dlna::DeviceProfile::default(),
    );
    checked += check_model(
        "DisplayPreferencesDto",
        ferrofin_model::dto::DisplayPreferencesDto::default(),
    );
    checked += check_model(
        "ImageOption",
        ferrofin_model::configuration::ImageOption::default(),
    );
    checked += check_model(
        "LibraryOptions",
        ferrofin_model::configuration::LibraryOptions::default(),
    );
    checked += check_model(
        "MediaAttachment",
        ferrofin_model::entities_media::MediaAttachment::default(),
    );
    checked += check_model(
        "MediaSegmentDto",
        ferrofin_model::media_segments::MediaSegmentDto::default(),
    );
    checked += check_model(
        "MediaSourceInfo",
        ferrofin_model::dto::MediaSourceInfo::default(),
    );
    checked += check_model(
        "MediaStream",
        ferrofin_model::entities_media::MediaStream::default(),
    );
    checked += check_model(
        "MessageCommand",
        ferrofin_model::session::MessageCommand::default(),
    );
    checked += check_model(
        "MovePlaylistItemRequestDto",
        ferrofin_model::sync_play::MovePlaylistItemRequestDto::default(),
    );
    checked += check_model("MovieInfo", ferrofin_model::providers::MovieInfo::default());
    checked += check_model(
        "MusicVideoInfo",
        ferrofin_model::providers::MusicVideoInfo::default(),
    );
    checked += check_model(
        "PersonLookupInfo",
        ferrofin_model::providers::PersonLookupInfo::default(),
    );
    checked += check_model(
        "PingRequestDto",
        ferrofin_model::sync_play::PingRequestDto::default(),
    );
    checked += check_model(
        "PlayRequestDto",
        ferrofin_model::sync_play::PlayRequestDto::default(),
    );
    checked += check_model(
        "PlaybackProgressInfo",
        ferrofin_model::session::PlaybackProgressInfo::default(),
    );
    checked += check_model(
        "PlaybackStartInfo",
        ferrofin_model::session::PlaybackStartInfo::default(),
    );
    checked += check_model(
        "PlaybackStopInfo",
        ferrofin_model::session::PlaybackStopInfo::default(),
    );
    checked += check_model(
        "ReadyRequestDto",
        ferrofin_model::sync_play::ReadyRequestDto::default(),
    );
    checked += check_model(
        "RemoteSearchResult",
        ferrofin_model::providers::RemoteSearchResult::default(),
    );
    checked += check_model(
        "SeekRequestDto",
        ferrofin_model::sync_play::SeekRequestDto::default(),
    );
    checked += check_model(
        "SeriesInfo",
        ferrofin_model::providers::SeriesInfo::default(),
    );
    checked += check_model(
        "SeriesTimerInfoDto",
        ferrofin_model::live_tv::SeriesTimerInfoDto::default(),
    );
    checked += check_model(
        "ServerConfiguration",
        ferrofin_model::configuration::ServerConfiguration::default(),
    );
    checked += check_model("SongInfo", ferrofin_model::providers::SongInfo::default());
    checked += check_model(
        "TaskTriggerInfo",
        ferrofin_model::tasks::TaskTriggerInfo::default(),
    );
    checked += check_model(
        "TimerInfoDto",
        ferrofin_model::live_tv::TimerInfoDto::default(),
    );
    checked += check_model(
        "TrailerInfo",
        ferrofin_model::providers::TrailerInfo::default(),
    );
    checked += check_model(
        "TranscodingProfile",
        ferrofin_model::dlna::TranscodingProfile::default(),
    );
    checked += check_model(
        "TrickplayInfoDto",
        ferrofin_model::dto::TrickplayInfoDto::default(),
    );
    checked += check_model(
        "TrickplayOptions",
        ferrofin_model::configuration::TrickplayOptions::default(),
    );
    checked += check_model(
        "TunerHostInfo",
        ferrofin_model::live_tv::TunerHostInfo::default(),
    );
    checked += check_model(
        "UpdateUserItemDataDto",
        ferrofin_model::dto::UpdateUserItemDataDto::default(),
    );
    checked += check_model("UserDto", ferrofin_model::dto::UserDto::default());
    checked += check_model(
        "UserItemDataDto",
        ferrofin_model::dto::UserItemDataDto {
            rating: None,
            played_percentage: None,
            unplayed_item_count: None,
            playback_position_ticks: 0,
            play_count: 0,
            is_favorite: false,
            likes: None,
            last_played_date: None,
            played: false,
            key: String::new(),
            item_id: uuid::Uuid::nil(),
        },
    );
    checked += check_model("UserPolicy", ferrofin_model::users::UserPolicy::default());
    // The remaining fifteen fields live in four private handler DTOs; their
    // adjacent tests use this same inventory and the same production binder.
    assert_eq!(checked, 207);
}
