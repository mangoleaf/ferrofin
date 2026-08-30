//! The two JSON documents an HDHomeRun device serves.
//!
//! Ports of `DiscoverResponse.cs` and `Channels.cs` (v10.11.8
//! `src/Jellyfin.LiveTv/TunerHosts/HdHomerun/`). Both are read with
//! `JsonBoolNumberConverter` in the options bag
//! (`HdHomerunHost` ctor, HdHomerunHost.cs:60-61), which is why the three
//! boolean lineup flags accept a number as well as a bool.

use serde::{Deserialize, Deserializer};

/// `GET {device}/discover.json`.
///
/// Port of `DiscoverResponse` (v10.11.8 DiscoverResponse.cs). Every field is
/// optional in practice: the HDHR3-US fixture omits `DeviceAuth` and
/// `LineupURL`, and the HDHR4 fallback constructs one with only `ModelNumber`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct DiscoverResponse {
    /// `DiscoverResponse.FriendlyName` — e.g. `"HDHomeRun PRIME"`.
    #[serde(rename = "FriendlyName")]
    pub friendly_name: Option<String>,
    /// `DiscoverResponse.ModelNumber` — e.g. `"HDHR3-CC"`.
    #[serde(rename = "ModelNumber")]
    pub model_number: Option<String>,
    /// `DiscoverResponse.FirmwareName`.
    #[serde(rename = "FirmwareName")]
    pub firmware_name: Option<String>,
    /// `DiscoverResponse.FirmwareVersion`.
    #[serde(rename = "FirmwareVersion")]
    pub firmware_version: Option<String>,
    /// `DiscoverResponse.DeviceID`.
    #[serde(rename = "DeviceID")]
    pub device_id: Option<String>,
    /// `DiscoverResponse.DeviceAuth`.
    #[serde(rename = "DeviceAuth")]
    pub device_auth: Option<String>,
    /// `DiscoverResponse.BaseURL`.
    #[serde(rename = "BaseURL")]
    pub base_url: Option<String>,
    /// `DiscoverResponse.LineupURL`.
    #[serde(rename = "LineupURL")]
    pub lineup_url: Option<String>,
    /// `DiscoverResponse.TunerCount`.
    #[serde(rename = "TunerCount")]
    pub tuner_count: i32,
}

impl DiscoverResponse {
    /// `DiscoverResponse.SupportsTranscoding` (v10.11.8 DiscoverResponse.cs:26-38):
    /// true exactly when the model number contains `"hdtc"`, case-insensitively.
    /// Only the HDHomeRun EXTEND (`HDTC-2US`) transcodes, so every other model
    /// is offered the native profile alone.
    #[must_use]
    pub fn supports_transcoding(&self) -> bool {
        self.model_number
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("hdtc")
    }
}

/// One entry of `GET {device}/lineup.json`.
///
/// Port of `Channels` (v10.11.8 Channels.cs). `Favorite`, `DRM` and `HD` are
/// `bool` upstream but arrive as NUMBERS from a real device
/// (`{"GuideNumber":"4.1","HD":1,"Favorite":1,…}`), which is exactly why
/// `JsonBoolNumberConverter` exists — its own doc comment reads "This is needed
/// for HDHomerun."
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct LineupChannel {
    /// `Channels.GuideNumber` — the channel number, e.g. `"4.1"`.
    #[serde(rename = "GuideNumber")]
    pub guide_number: Option<String>,
    /// `Channels.GuideName` — the display name.
    #[serde(rename = "GuideName")]
    pub guide_name: Option<String>,
    /// `Channels.VideoCodec`.
    #[serde(rename = "VideoCodec")]
    pub video_codec: Option<String>,
    /// `Channels.AudioCodec`.
    #[serde(rename = "AudioCodec")]
    pub audio_codec: Option<String>,
    /// `Channels.URL` — the stream URL, or a `hdhomerun://…` locator on a
    /// legacy device.
    #[serde(rename = "URL")]
    pub url: Option<String>,
    /// `Channels.Favorite`.
    #[serde(rename = "Favorite", deserialize_with = "de_bool_or_number")]
    pub favorite: bool,
    /// `Channels.DRM` — an encrypted channel, which the lineup drops.
    #[serde(rename = "DRM", deserialize_with = "de_bool_or_number")]
    pub drm: bool,
    /// `Channels.HD`.
    #[serde(rename = "HD", deserialize_with = "de_bool_or_number")]
    pub hd: bool,
}

/// Deserializes a flag that may arrive as a JSON bool or as a JSON number.
///
/// Port of `JsonBoolNumberConverter.Read` (v10.11.8
/// `src/Jellyfin.Extensions/Json/Converters/JsonBoolNumberConverter.cs`):
///
/// ```text
/// if (reader.TokenType == JsonTokenType.Number)
/// {
///     return Convert.ToBoolean(reader.GetInt32());
/// }
///
/// return reader.GetBoolean();
/// ```
///
/// `Convert.ToBoolean(int)` is `value != 0`, so `0` is false and any other
/// integer is true. Anything that is neither a number nor a bool throws
/// upstream, and errors here.
fn de_bool_or_number<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    struct BoolOrNumber;

    impl serde::de::Visitor<'_> for BoolOrNumber {
        type Value = bool;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a boolean or a number")
        }

        fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<bool, E> {
            Ok(value)
        }

        fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<bool, E> {
            // `Convert.ToBoolean(reader.GetInt32())`.
            Ok(value != 0)
        }

        fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<bool, E> {
            Ok(value != 0)
        }
    }

    deserializer.deserialize_any(BoolOrNumber)
}

#[cfg(test)]
mod tests {
    use super::{DiscoverResponse, LineupChannel};
    use rstest::rstest;

    #[rstest]
    // `ModelNumber.Contains("hdtc", OrdinalIgnoreCase)` — the EXTEND only.
    #[case(Some("HDTC-2US"), true)]
    #[case(Some("hdtc-2us"), true)]
    #[case(Some("HDHR3-CC"), false)]
    #[case(Some("HDHR3-US"), false)]
    #[case(Some("HDHR"), false)]
    #[case(None, false)]
    fn supports_transcoding_matches_the_model_substring(
        #[case] model: Option<&str>,
        #[case] expected: bool,
    ) {
        let response = DiscoverResponse {
            model_number: model.map(ToOwned::to_owned),
            ..DiscoverResponse::default()
        };
        assert_eq!(response.supports_transcoding(), expected);
    }

    #[rstest]
    // `JsonBoolNumberConverter`: numbers and bools both bind, 0 is false.
    #[case(r#"{"HD":1}"#, true)]
    #[case(r#"{"HD":0}"#, false)]
    #[case(r#"{"HD":true}"#, true)]
    #[case(r#"{"HD":false}"#, false)]
    #[case(r"{}", false)]
    fn hd_binds_from_a_number_or_a_bool(#[case] json: &str, #[case] expected: bool) {
        let channel: LineupChannel = serde_json::from_str(json).expect("binds");
        assert_eq!(channel.hd, expected);
    }

    #[test]
    fn a_flag_that_is_neither_a_number_nor_a_bool_is_rejected() {
        // `reader.GetBoolean()` throws on a string upstream.
        assert!(serde_json::from_str::<LineupChannel>(r#"{"HD":"yes"}"#).is_err());
    }

    #[test]
    fn the_lineup_entry_binds_every_upstream_field() {
        let channel: LineupChannel = serde_json::from_str(
            r#"{"GuideNumber":"4.1","GuideName":"WCMH-DT","VideoCodec":"MPEG2",
                "AudioCodec":"AC3","URL":"http://192.168.1.111:5004/auto/v4.1",
                "HD":1,"Favorite":1,"DRM":0}"#,
        )
        .expect("binds");
        assert_eq!(channel.guide_number.as_deref(), Some("4.1"));
        assert_eq!(channel.guide_name.as_deref(), Some("WCMH-DT"));
        assert_eq!(channel.video_codec.as_deref(), Some("MPEG2"));
        assert_eq!(channel.audio_codec.as_deref(), Some("AC3"));
        assert_eq!(
            channel.url.as_deref(),
            Some("http://192.168.1.111:5004/auto/v4.1")
        );
        assert!(channel.hd && channel.favorite && !channel.drm);
    }
}
