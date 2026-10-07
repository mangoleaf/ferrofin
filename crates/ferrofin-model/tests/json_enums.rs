//! Every request enum is checked against Jellyfin 12.2 reflection, including
//! nonsequential values and values without a declared member.
use ferrofin_model::json::enums::JsonEnum;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

fn check<T: JsonEnum + Serialize + DeserializeOwned>(name: &str) {
    let fixture =
        include_str!("../../ferrofin-api/tests/data/json-binding/jellyfin-12.2-enums.jsonl");
    let upstream: Value = fixture
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|row| row["name"] == name)
        .unwrap();
    assert_eq!(upstream["underlying"], "Int32", "{name}");
    assert_eq!(
        T::members().len(),
        upstream["members"].as_object().unwrap().len(),
        "{name}"
    );
    for &(wire, number) in T::members() {
        assert_eq!(upstream["members"][wire], number, "{name}.{wire}");
        for input in [
            number.to_string(),
            json!(number.to_string()).to_string(),
            json!(wire).to_string(),
            json!(wire.to_lowercase()).to_string(),
            json!(format!(" {wire} ")).to_string(),
        ] {
            let value: T =
                serde_json::from_str(&input).unwrap_or_else(|e| panic!("{name} {input}: {e}"));
            assert_eq!(serde_json::to_value(value).unwrap(), wire, "{name} {input}");
        }
    }
    for number in [i32::MIN, i32::MAX, 12345] {
        let value: T = serde_json::from_str(&number.to_string()).unwrap();
        assert_eq!(serde_json::to_value(value).unwrap(), number, "{name}");
    }
    for input in [
        "true",
        "1.0",
        "1e0",
        "2147483648",
        r#""2147483648""#,
        r#""no-such-name""#,
        "{}",
        "[]",
    ] {
        assert!(serde_json::from_str::<T>(input).is_err(), "{name} {input}");
    }
    for input in ["null", r#""""#] {
        let value = serde_json::from_str::<T>(input);
        assert_eq!(
            value.is_ok(),
            !upstream["default_value"].is_null(),
            "{name} {input}"
        );
        if let Ok(value) = value {
            assert_eq!(
                serde_json::to_value(value).unwrap(),
                upstream["default_value"]
            );
        }
    }
}

#[test]
fn every_request_enum_matches_the_upstream_inventory() {
    check::<ferrofin_model::configuration::EmbeddedSubtitleOptions>("EmbeddedSubtitleOptions");
    check::<ferrofin_model::configuration::HlsAudioSeekStrategy>("HlsAudioSeekStrategy");
    check::<ferrofin_model::configuration::ImageSavingConvention>("ImageSavingConvention");
    check::<ferrofin_model::configuration::ProcessPriorityClass>("ProcessPriorityClass");
    check::<ferrofin_model::configuration::SubtitlePlaybackMode>("SubtitlePlaybackMode");
    check::<ferrofin_model::configuration::TrickplayScanBehavior>("TrickplayScanBehavior");
    check::<ferrofin_model::data::BaseItemKind>("BaseItemKind");
    check::<ferrofin_model::data::CollectionType>("CollectionType");
    check::<ferrofin_model::data::MediaStreamProtocol>("MediaStreamProtocol");
    check::<ferrofin_model::data::MediaType>("MediaType");
    check::<ferrofin_model::data::PersonKind>("PersonKind");
    check::<ferrofin_model::data::UnratedItem>("UnratedItem");
    check::<ferrofin_model::data::VideoRange>("VideoRange");
    check::<ferrofin_model::data::VideoRangeType>("VideoRangeType");
    check::<ferrofin_model::dlna::enums::CodecType>("CodecType");
    check::<ferrofin_model::dlna::enums::DlnaProfileType>("DlnaProfileType");
    check::<ferrofin_model::dlna::enums::EncodingContext>("EncodingContext");
    check::<ferrofin_model::dlna::enums::ProfileConditionType>("ProfileConditionType");
    check::<ferrofin_model::dlna::enums::ProfileConditionValue>("ProfileConditionValue");
    check::<ferrofin_model::dlna::enums::SubtitleDeliveryMethod>("SubtitleDeliveryMethod");
    check::<ferrofin_model::dlna::enums::TranscodeSeekInfo>("TranscodeSeekInfo");
    check::<ferrofin_model::drawing::ImageOrientation>("ImageOrientation");
    check::<ferrofin_model::drawing::ImageResolution>("ImageResolution");
    check::<ferrofin_model::dto::DayOfWeek>("DayOfWeek");
    check::<ferrofin_model::dto::MediaSourceType>("MediaSourceType");
    check::<ferrofin_model::dto::ScrollDirection>("ScrollDirection");
    check::<ferrofin_model::dto::SortOrder>("SortOrder");
    check::<ferrofin_model::entities::ExtraType>("ExtraType");
    check::<ferrofin_model::entities::ImageType>("ImageType");
    check::<ferrofin_model::entities::IsoType>("IsoType");
    check::<ferrofin_model::entities::LocationType>("LocationType");
    check::<ferrofin_model::entities::MediaStreamType>("MediaStreamType");
    check::<ferrofin_model::entities::MetadataField>("MetadataField");
    check::<ferrofin_model::entities::Video3DFormat>("Video3DFormat");
    check::<ferrofin_model::entities::VideoType>("VideoType");
    check::<ferrofin_model::entities_media::AudioSpatialFormat>("AudioSpatialFormat");
    check::<ferrofin_model::library::PlayAccess>("PlayAccess");
    check::<ferrofin_model::live_tv::ChannelType>("ChannelType");
    check::<ferrofin_model::live_tv::DayPattern>("DayPattern");
    check::<ferrofin_model::live_tv::ItemSortBy>("ItemSortBy");
    check::<ferrofin_model::live_tv::KeepUntil>("KeepUntil");
    check::<ferrofin_model::live_tv::ProgramAudio>("ProgramAudio");
    check::<ferrofin_model::live_tv::RecordingStatus>("RecordingStatus");
    check::<ferrofin_model::media_info::MediaProtocol>("MediaProtocol");
    check::<ferrofin_model::media_info::TransportStreamTimestamp>("TransportStreamTimestamp");
    check::<ferrofin_model::media_segments::MediaSegmentType>("MediaSegmentType");
    check::<ferrofin_model::querying::ItemFields>("ItemFields");
    check::<ferrofin_model::session::GeneralCommandType>("GeneralCommandType");
    check::<ferrofin_model::session::PlayMethod>("PlayMethod");
    check::<ferrofin_model::session::PlaybackOrder>("PlaybackOrder");
    check::<ferrofin_model::session::RepeatMode>("RepeatMode");
    check::<ferrofin_model::session::SessionMessageType>("SessionMessageType");
    check::<ferrofin_model::sync_play::GroupQueueMode>("GroupQueueMode");
    check::<ferrofin_model::sync_play::GroupRepeatMode>("GroupRepeatMode");
    check::<ferrofin_model::sync_play::GroupShuffleMode>("GroupShuffleMode");
    check::<ferrofin_model::tasks::TaskTriggerInfoType>("TaskTriggerInfoType");
    check::<ferrofin_model::users::DynamicDayOfWeek>("DynamicDayOfWeek");
    check::<ferrofin_model::users::SyncPlayUserAccessType>("SyncPlayUserAccessType");
}
