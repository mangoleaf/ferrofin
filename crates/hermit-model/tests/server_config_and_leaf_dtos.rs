#![recursion_limit = "256"]
//! Wire-contract regression tests for the server-config + leaf-DTO port unit.
//!
//! No xUnit tests exist upstream for these namespaces, so these lock the JSON
//! contract (property casing, enum string values, and the polymorphic
//! `GroupUpdate` tagged enum) against the vendored OpenAPI spec
//! (`contracts/jellyfin-openapi-10.11.8.json`).

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use uuid::Uuid;

/// Assert an enum value serializes to exactly `"expected"` and round-trips.
fn assert_enum<T>(value: &T, expected: &str)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let s = serde_json::to_string(value).expect("serialize");
    assert_eq!(s, format!("\"{expected}\""), "enum wire value");
    let back: T = serde_json::from_str(&s).expect("deserialize");
    assert_eq!(&back, value, "round-trip");
}

/// Assert a struct serializes to exactly `expected` (a JSON value) and
/// round-trips.
#[allow(clippy::needless_pass_by_value)]
fn assert_json<T>(value: &T, expected: serde_json::Value)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let actual = serde_json::to_value(value).expect("serialize");
    assert_eq!(actual, expected, "serialized JSON");
    let back: T = serde_json::from_value(actual).expect("deserialize");
    assert_eq!(&back, value, "round-trip");
}

#[test]
fn enum_wire_values() {
    use hermit_model::configuration::{ProcessPriorityClass, SubtitlePlaybackMode};
    use hermit_model::live_tv::{ChannelType, KeepUntil, ProgramAudio};
    use hermit_model::plugins::PluginStatus;
    use hermit_model::sync_play::{GroupRepeatMode, GroupUpdateType};
    use hermit_model::tasks::TaskCompletionStatus;

    assert_enum(&ChannelType::Tv, "TV");
    assert_enum(&ChannelType::Radio, "Radio");
    assert_enum(&ProgramAudio::DolbyDigital, "DolbyDigital");
    assert_enum(&KeepUntil::UntilSpaceNeeded, "UntilSpaceNeeded");
    assert_enum(&PluginStatus::Superseded, "Superseded");
    assert_enum(&PluginStatus::Superceded, "Superceded");
    assert_enum(&TaskCompletionStatus::Aborted, "Aborted");
    assert_enum(&GroupRepeatMode::RepeatAll, "RepeatAll");
    assert_enum(&GroupUpdateType::LibraryAccessDenied, "LibraryAccessDenied");
    assert_enum(&ProcessPriorityClass::BelowNormal, "BelowNormal");
    assert_enum(&SubtitlePlaybackMode::OnlyForced, "OnlyForced");
}

#[test]
fn iso_field_casing_globalization() {
    use hermit_model::globalization::CountryInfo;

    let c = CountryInfo {
        name: "US".into(),
        display_name: "United States".into(),
        two_letter_iso_region_name: "US".into(),
        three_letter_iso_region_name: "USA".into(),
    };
    assert_json(
        &c,
        json!({
            "Name": "US",
            "DisplayName": "United States",
            "TwoLetterISORegionName": "US",
            "ThreeLetterISORegionName": "USA",
        }),
    );
}

#[test]
fn installation_info_id_is_named_guid() {
    use hermit_model::updates::InstallationInfo;

    let id = Uuid::nil();
    let info = InstallationInfo {
        id,
        name: Some("Plugin".into()),
        version: Some("1.0.0.0".into()),
        changelog: None,
        source_url: None,
        checksum: None,
        package_info: None,
    };
    let v = serde_json::to_value(&info).expect("serialize");
    // The Id property is renamed to "Guid" on the wire; unset options omitted.
    assert_eq!(v["Guid"], json!(id.to_string()));
    assert!(v.get("Id").is_none());
    assert!(v.get("Changelog").is_none());
}

#[test]
fn package_info_and_version_info_use_camel_case() {
    use hermit_model::updates::{PackageInfo, VersionInfo};

    let version = VersionInfo {
        version: "1.2.3".into(),
        version_number: None,
        changelog: Some("notes".into()),
        target_abi: Some("10.9.0.0".into()),
        source_url: None,
        checksum: None,
        timestamp: None,
        repository_name: "main".into(),
        repository_url: "https://repo".into(),
    };
    let pkg = PackageInfo {
        name: "Sample".into(),
        description: "desc".into(),
        overview: "ov".into(),
        owner: "me".into(),
        category: "General".into(),
        id: Uuid::nil(),
        versions: vec![version],
        image_url: None,
    };
    let v = serde_json::to_value(&pkg).expect("serialize");
    assert_eq!(v["name"], json!("Sample"));
    assert_eq!(v["guid"], json!(Uuid::nil().to_string()));
    assert_eq!(v["versions"][0]["repositoryName"], json!("main"));
    assert_eq!(v["versions"][0]["targetAbi"], json!("10.9.0.0"));
}

#[test]
fn server_config_special_casings() {
    use hermit_model::configuration::ServerConfiguration;

    // Round-trip the JSON shape with the tricky acronym/number casings.
    let raw = json!({
        "LogFileRetentionDays": 3,
        "IsStartupWizardCompleted": false,
        "EnableMetrics": false,
        "EnableNormalizedItemByNameIds": true,
        "IsPortAuthorized": false,
        "QuickConnectAvailable": true,
        "EnableCaseSensitiveItemIds": true,
        "DisableLiveTvChannelUserDataName": true,
        "MetadataPath": "",
        "PreferredMetadataLanguage": "en",
        "MetadataCountryCode": "US",
        "SortReplaceCharacters": [".", "+", "%"],
        "SortRemoveCharacters": [],
        "SortRemoveWords": [],
        "MinResumePct": 5,
        "MaxResumePct": 90,
        "MinResumeDurationSeconds": 300,
        "MinAudiobookResume": 5,
        "MaxAudiobookResume": 5,
        "InactiveSessionThreshold": 0,
        "LibraryMonitorDelay": 60,
        "LibraryUpdateDuration": 30,
        "CacheSize": 800,
        "ImageSavingConvention": "Legacy",
        "MetadataOptions": [],
        "SkipDeserializationForBasicTypes": true,
        "ServerName": "",
        "UICulture": "en-US",
        "SaveMetadataHidden": false,
        "ContentTypes": [],
        "RemoteClientBitrateLimit": 0,
        "EnableFolderView": false,
        "EnableGroupingMoviesIntoCollections": false,
        "EnableGroupingShowsIntoCollections": false,
        "DisplaySpecialsWithinSeasons": true,
        "CodecsUsed": [],
        "PluginRepositories": [],
        "EnableExternalContentInSuggestions": true,
        "ImageExtractionTimeoutMs": 0,
        "PathSubstitutions": [],
        "EnableSlowResponseWarning": true,
        "SlowResponseThresholdMs": 500,
        "CorsHosts": ["*"],
        "LibraryScanFanoutConcurrency": 0,
        "LibraryMetadataRefreshConcurrency": 0,
        "AllowClientLogUpload": true,
        "DummyChapterDuration": 0,
        "ChapterImageResolution": "MatchSource",
        "ParallelImageEncodingLimit": 0,
        "CastReceiverApplications": [],
        "TrickplayOptions": {
            "EnableHwAcceleration": false,
            "EnableHwEncoding": false,
            "EnableKeyFrameOnlyExtraction": false,
            "ScanBehavior": "NonBlocking",
            "ProcessPriority": "BelowNormal",
            "Interval": 10000,
            "WidthResolutions": [320],
            "TileWidth": 10,
            "TileHeight": 10,
            "Qscale": 4,
            "JpegQuality": 90,
            "ProcessThreads": 1
        },
        "EnableLegacyAuthorization": false
    });
    let cfg: ServerConfiguration =
        serde_json::from_value(raw.clone()).expect("deserialize ServerConfiguration");
    assert_eq!(cfg.ui_culture, "en-US");
    let back = serde_json::to_value(&cfg).expect("serialize");
    assert_eq!(back, raw);
}

#[test]
fn library_options_lufs_and_delimiters_casing() {
    use hermit_model::configuration::LibraryOptions;

    let opts = LibraryOptions::default();
    let v = serde_json::to_value(&opts).expect("serialize");
    assert_eq!(v["EnableLUFSScan"], json!(false));
    assert_eq!(v["CustomTagDelimiters"], json!(["/", "|", ";", "\\"]));
    assert_eq!(v["SeasonZeroDisplayName"], json!("Specials"));
    // Round-trips.
    let back: LibraryOptions = serde_json::from_value(v).expect("deserialize");
    assert_eq!(back, opts);
}

#[test]
fn encoding_options_number_casing_and_defaults() {
    use hermit_model::configuration::EncodingOptions;

    let opts = EncodingOptions::default();
    let v = serde_json::to_value(&opts).expect("serialize");
    // Numbers/acronyms keep their exact C# casing.
    assert_eq!(v["H264Crf"], json!(23));
    assert_eq!(v["H265Crf"], json!(28));
    assert_eq!(v["EnableDecodingColorDepth10Vp9"], json!(true));
    assert_eq!(v["AllowAv1Encoding"], json!(false));
    assert_eq!(v["VaapiDevice"], json!("/dev/dri/renderD128"));
    assert_eq!(v["DownMixStereoAlgorithm"], json!("None"));
    let back: EncodingOptions = serde_json::from_value(v).expect("deserialize");
    assert_eq!(back, opts);
}

#[test]
fn group_update_is_internally_tagged_on_type() {
    use hermit_model::sync_play::{GroupUpdate, UserJoinedUpdate};

    let gid = Uuid::nil();
    let update = GroupUpdate::UserJoined(UserJoinedUpdate {
        group_id: gid,
        data: "alice".into(),
    });
    let v = serde_json::to_value(&update).expect("serialize");
    assert_eq!(
        v,
        json!({
            "Type": "UserJoined",
            "GroupId": gid.to_string(),
            "Data": "alice",
        }),
    );
    // Deserialization picks the variant by the "Type" discriminator.
    let back: GroupUpdate = serde_json::from_value(v).expect("deserialize");
    assert_eq!(back, update);
}

#[test]
fn group_update_state_variant_carries_typed_data() {
    use hermit_model::sync_play::{
        GroupStateType, GroupStateUpdate, GroupUpdate, PlaybackRequestType, StateUpdate,
    };

    let gid = Uuid::nil();
    let update = GroupUpdate::StateUpdate(StateUpdate {
        group_id: gid,
        data: GroupStateUpdate {
            state: GroupStateType::Playing,
            reason: PlaybackRequestType::Unpause,
        },
    });
    let v = serde_json::to_value(&update).expect("serialize");
    assert_eq!(v["Type"], json!("StateUpdate"));
    assert_eq!(v["Data"]["State"], json!("Playing"));
    assert_eq!(v["Data"]["Reason"], json!("Unpause"));
    let back: GroupUpdate = serde_json::from_value(v).expect("deserialize");
    assert_eq!(back, update);
}

#[test]
fn media_segment_type_default_is_unknown() {
    use hermit_model::media_segments::MediaSegmentType;

    assert_enum(&MediaSegmentType::Unknown, "Unknown");
    assert_enum(&MediaSegmentType::Commercial, "Commercial");
    assert_eq!(MediaSegmentType::default(), MediaSegmentType::Unknown);
}

#[test]
fn activity_log_severity_uses_log_level_names() {
    use hermit_model::activity::LogLevel;

    assert_enum(&LogLevel::Information, "Information");
    assert_enum(&LogLevel::Critical, "Critical");
    assert_enum(&LogLevel::None, "None");
}
