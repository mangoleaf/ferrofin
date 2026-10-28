//! The Schedules Direct JSON API documents, ported from
//! `Jellyfin.LiveTv/Listings/SchedulesDirectDtos` (v12.2).
//!
//! Only the members the provider reads (and upstream's deserialization tests
//! assert) are carried; serde ignores the rest, as `System.Text.Json` ignores
//! members a DTO does not declare. Upstream reads them with
//! `JsonDefaults.Options`, whose tolerances are kept where a real response can
//! need them: a number may arrive as a string (`NumberHandling.AllowReadingFromString`),
//! a string member may arrive as a bare number or boolean (`JsonStringConverter`),
//! and an image list may arrive as something other than an array
//! (`ImageDataArrayConverter`).

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize};

/// The Schedules Direct error codes (`SdErrorCode.cs`, v12.2), as published in
/// the SD JSON API's error table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdErrorCode {
    /// 3000.
    ServiceOffline,
    /// 3001.
    ServiceBusy,
    /// 4001.
    AccountExpired,
    /// 4002.
    InvalidHash,
    /// 4003.
    InvalidUser,
    /// 4004.
    AccountTempLock,
    /// 4005.
    AccountLocked,
    /// 4006.
    TokenExpired,
    /// 4007.
    AppLocked,
    /// 4008.
    AccountInactive,
    /// 4009.
    MaxLoginAttempts,
    /// 4010.
    MaxIpAttempts,
    /// 4100.
    MaxScheduleRequests,
    /// 5000.
    ImageNotFound,
    /// 5002.
    MaxImageDownloads,
    /// 5003.
    MaxImageDownloadsTrial,
    /// 5004.
    MaxInvalidImages,
}

impl SdErrorCode {
    /// The code for `value`, when it is one the table defines
    /// (`Enum.IsDefined((SdErrorCode)parsedCode)`).
    #[must_use]
    pub fn from_code(value: i64) -> Option<Self> {
        Some(match value {
            3000 => Self::ServiceOffline,
            3001 => Self::ServiceBusy,
            4001 => Self::AccountExpired,
            4002 => Self::InvalidHash,
            4003 => Self::InvalidUser,
            4004 => Self::AccountTempLock,
            4005 => Self::AccountLocked,
            4006 => Self::TokenExpired,
            4007 => Self::AppLocked,
            4008 => Self::AccountInactive,
            4009 => Self::MaxLoginAttempts,
            4010 => Self::MaxIpAttempts,
            4100 => Self::MaxScheduleRequests,
            5000 => Self::ImageNotFound,
            5002 => Self::MaxImageDownloads,
            5003 => Self::MaxImageDownloadsTrial,
            5004 => Self::MaxInvalidImages,
            _ => return None,
        })
    }

    /// The numeric code.
    #[must_use]
    pub fn code(self) -> i64 {
        match self {
            Self::ServiceOffline => 3000,
            Self::ServiceBusy => 3001,
            Self::AccountExpired => 4001,
            Self::InvalidHash => 4002,
            Self::InvalidUser => 4003,
            Self::AccountTempLock => 4004,
            Self::AccountLocked => 4005,
            Self::TokenExpired => 4006,
            Self::AppLocked => 4007,
            Self::AccountInactive => 4008,
            Self::MaxLoginAttempts => 4009,
            Self::MaxIpAttempts => 4010,
            Self::MaxScheduleRequests => 4100,
            Self::ImageNotFound => 5000,
            Self::MaxImageDownloads => 5002,
            Self::MaxImageDownloadsTrial => 5003,
            Self::MaxInvalidImages => 5004,
        }
    }
}

/// `TokenDto`: the `/token` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TokenDto {
    /// `code`.
    #[serde(deserialize_with = "lenient_i64")]
    pub code: i64,
    /// `message`.
    #[serde(deserialize_with = "lenient_string")]
    pub message: Option<String>,
    /// `serverID`.
    #[serde(rename = "serverID", deserialize_with = "lenient_string")]
    pub server_id: Option<String>,
    /// `token`.
    #[serde(deserialize_with = "lenient_string")]
    pub token: Option<String>,
    /// `datetime`.
    #[serde(rename = "datetime", deserialize_with = "lenient_datetime")]
    pub token_timestamp: Option<DateTime<Utc>>,
    /// `response`.
    #[serde(deserialize_with = "lenient_string")]
    pub response: Option<String>,
}

/// `HeadendsDto`: one `/headends` entry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HeadendsDto {
    /// `headend`.
    #[serde(deserialize_with = "lenient_string")]
    pub headend: Option<String>,
    /// `transport`.
    #[serde(deserialize_with = "lenient_string")]
    pub transport: Option<String>,
    /// `location`.
    #[serde(deserialize_with = "lenient_string")]
    pub location: Option<String>,
    /// `lineups`.
    #[serde(deserialize_with = "list")]
    pub lineups: Vec<LineupDto>,
}

/// `LineupDto`: a lineup of a headend or of the account.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LineupDto {
    /// `lineup`: the lineup id.
    #[serde(deserialize_with = "lenient_string")]
    pub lineup: Option<String>,
    /// `name`.
    #[serde(deserialize_with = "lenient_string")]
    pub name: Option<String>,
    /// `transport`.
    #[serde(deserialize_with = "lenient_string")]
    pub transport: Option<String>,
    /// `location`.
    #[serde(deserialize_with = "lenient_string")]
    pub location: Option<String>,
    /// `uri`.
    #[serde(deserialize_with = "lenient_string")]
    pub uri: Option<String>,
    /// `isDeleted`.
    pub is_deleted: Option<bool>,
}

/// `LineupsDto`: the `/lineups` response (the account's lineups).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LineupsDto {
    /// `code`.
    #[serde(deserialize_with = "lenient_i64")]
    pub code: i64,
    /// `serverID`.
    #[serde(rename = "serverID", deserialize_with = "lenient_string")]
    pub server_id: Option<String>,
    /// `datetime`.
    #[serde(rename = "datetime", deserialize_with = "lenient_datetime")]
    pub lineup_timestamp: Option<DateTime<Utc>>,
    /// `lineups`.
    #[serde(deserialize_with = "list")]
    pub lineups: Vec<LineupDto>,
}

/// `ChannelDto`: the `/lineups/{id}` response, a lineup's channel map.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ChannelDto {
    /// `map`.
    #[serde(deserialize_with = "list")]
    pub map: Vec<MapDto>,
    /// `stations`.
    #[serde(deserialize_with = "list")]
    pub stations: Vec<StationDto>,
}

/// `MapDto`: one channel → station mapping.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MapDto {
    /// `stationID`.
    #[serde(rename = "stationID", deserialize_with = "lenient_string")]
    pub station_id: Option<String>,
    /// `channel`.
    #[serde(deserialize_with = "lenient_string")]
    pub channel: Option<String>,
    /// `providerCallsign`.
    #[serde(deserialize_with = "lenient_string")]
    pub provider_callsign: Option<String>,
    /// `logicalChannelNumber`.
    #[serde(deserialize_with = "lenient_string")]
    pub logical_channel_number: Option<String>,
    /// `atscMajor`.
    #[serde(deserialize_with = "lenient_i64")]
    pub atsc_major: i64,
    /// `atscMinor`.
    #[serde(deserialize_with = "lenient_i64")]
    pub atsc_minor: i64,
    /// `matchType`.
    #[serde(deserialize_with = "lenient_string")]
    pub match_type: Option<String>,
}

/// `StationDto`: one station of a lineup.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StationDto {
    /// `stationID`.
    #[serde(rename = "stationID", deserialize_with = "lenient_string")]
    pub station_id: Option<String>,
    /// `name`.
    #[serde(deserialize_with = "lenient_string")]
    pub name: Option<String>,
    /// `callsign`.
    #[serde(deserialize_with = "lenient_string")]
    pub callsign: Option<String>,
    /// `logo`.
    pub logo: Option<LogoDto>,
}

/// `LogoDto`: a station logo.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LogoDto {
    /// `URL`.
    #[serde(rename = "URL", deserialize_with = "lenient_string")]
    pub url: Option<String>,
}

/// `RequestScheduleForChannelDto`: one station of a `/schedules` request.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RequestScheduleForChannelDto {
    /// `stationID`.
    #[serde(rename = "stationID")]
    pub station_id: String,
    /// `date`: the `yyyy-MM-dd` days asked for.
    #[serde(rename = "date")]
    pub date: Vec<String>,
}

/// `DayDto`: one station-day of a `/schedules` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DayDto {
    /// `stationID`.
    #[serde(rename = "stationID", deserialize_with = "lenient_string")]
    pub station_id: Option<String>,
    /// `programs`.
    #[serde(deserialize_with = "list")]
    pub programs: Vec<ProgramDto>,
}

/// `ProgramDto`: one airing of a station-day.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProgramDto {
    /// `programID`.
    #[serde(rename = "programID", deserialize_with = "lenient_string")]
    pub program_id: Option<String>,
    /// `airDateTime`.
    #[serde(deserialize_with = "lenient_datetime")]
    pub air_date_time: Option<DateTime<Utc>>,
    /// `duration`, in seconds.
    #[serde(deserialize_with = "lenient_i64")]
    pub duration: i64,
    /// `md5`.
    #[serde(deserialize_with = "lenient_string")]
    pub md5: Option<String>,
    /// `audioProperties`.
    #[serde(deserialize_with = "list")]
    pub audio_properties: Vec<String>,
    /// `videoProperties`.
    #[serde(deserialize_with = "list")]
    pub video_properties: Vec<String>,
    /// `new` — present (true) on a first run, absent on a repeat.
    pub new: Option<bool>,
    /// `liveTapeDelay`.
    #[serde(deserialize_with = "lenient_string")]
    pub live_tape_delay: Option<String>,
    /// `premiere`.
    pub premiere: bool,
    /// `isPremiereOrFinale`.
    #[serde(deserialize_with = "lenient_string")]
    pub is_premiere_or_finale: Option<String>,
}

/// `ProgramDetailsDto`: one `/programs` entry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProgramDetailsDto {
    /// `audience`.
    #[serde(deserialize_with = "lenient_string")]
    pub audience: Option<String>,
    /// `programID`.
    #[serde(rename = "programID", deserialize_with = "lenient_string")]
    pub program_id: Option<String>,
    /// `titles`.
    #[serde(deserialize_with = "list")]
    pub titles: Vec<TitleDto>,
    /// `eventDetails`.
    pub event_details: Option<EventDetailsDto>,
    /// `descriptions`.
    pub descriptions: Option<DescriptionsProgramDto>,
    /// `originalAirDate` (a bare date).
    #[serde(deserialize_with = "lenient_datetime")]
    pub original_air_date: Option<DateTime<Utc>>,
    /// `genres`.
    #[serde(deserialize_with = "list")]
    pub genres: Vec<String>,
    /// `episodeTitle150`.
    #[serde(rename = "episodeTitle150", deserialize_with = "lenient_string")]
    pub episode_title150: Option<String>,
    /// `metadata`.
    #[serde(deserialize_with = "list")]
    pub metadata: Vec<MetadataProgramsDto>,
    /// `contentRating`.
    #[serde(deserialize_with = "list")]
    pub content_rating: Vec<ContentRatingDto>,
    /// `cast`.
    #[serde(deserialize_with = "list")]
    pub cast: Vec<CastDto>,
    /// `crew`.
    #[serde(deserialize_with = "list")]
    pub crew: Vec<CrewDto>,
    /// `entityType`.
    #[serde(deserialize_with = "lenient_string")]
    pub entity_type: Option<String>,
    /// `hasImageArtwork`.
    pub has_image_artwork: bool,
    /// `movie`.
    pub movie: Option<MovieDto>,
}

/// `TitleDto`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TitleDto {
    /// `title120`.
    #[serde(rename = "title120", deserialize_with = "lenient_string")]
    pub title120: Option<String>,
}

/// `EventDetailsDto`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EventDetailsDto {
    /// `subType`.
    #[serde(deserialize_with = "lenient_string")]
    pub sub_type: Option<String>,
}

/// `DescriptionsProgramDto`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DescriptionsProgramDto {
    /// `description100`.
    #[serde(rename = "description100", deserialize_with = "list")]
    pub description100: Vec<DescriptionDto>,
    /// `description1000`.
    #[serde(rename = "description1000", deserialize_with = "list")]
    pub description1000: Vec<DescriptionDto>,
}

/// `Description100Dto` / `Description1000Dto` (the same two members).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DescriptionDto {
    /// `descriptionLanguage`.
    #[serde(deserialize_with = "lenient_string")]
    pub description_language: Option<String>,
    /// `description`.
    #[serde(deserialize_with = "lenient_string")]
    pub description: Option<String>,
}

/// `MetadataProgramsDto`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MetadataProgramsDto {
    /// `Gracenote` (capitalised on the wire).
    #[serde(rename = "Gracenote")]
    pub gracenote: Option<GracenoteDto>,
}

/// `GracenoteDto`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GracenoteDto {
    /// `season`.
    #[serde(deserialize_with = "lenient_i64")]
    pub season: i64,
    /// `episode`.
    #[serde(deserialize_with = "lenient_i64")]
    pub episode: i64,
}

/// `ContentRatingDto`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ContentRatingDto {
    /// `body`.
    #[serde(deserialize_with = "lenient_string")]
    pub body: Option<String>,
    /// `code`.
    #[serde(deserialize_with = "lenient_string")]
    pub code: Option<String>,
}

/// `CastDto`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CastDto {
    /// `billingOrder`.
    #[serde(deserialize_with = "lenient_string")]
    pub billing_order: Option<String>,
    /// `role`.
    #[serde(deserialize_with = "lenient_string")]
    pub role: Option<String>,
    /// `nameId`.
    #[serde(deserialize_with = "lenient_string")]
    pub name_id: Option<String>,
    /// `personId`.
    #[serde(deserialize_with = "lenient_string")]
    pub person_id: Option<String>,
    /// `name`.
    #[serde(deserialize_with = "lenient_string")]
    pub name: Option<String>,
    /// `characterName`.
    #[serde(deserialize_with = "lenient_string")]
    pub character_name: Option<String>,
}

/// `CrewDto`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CrewDto {
    /// `billingOrder`.
    #[serde(deserialize_with = "lenient_string")]
    pub billing_order: Option<String>,
    /// `role`.
    #[serde(deserialize_with = "lenient_string")]
    pub role: Option<String>,
    /// `nameId`.
    #[serde(deserialize_with = "lenient_string")]
    pub name_id: Option<String>,
    /// `personId`.
    #[serde(deserialize_with = "lenient_string")]
    pub person_id: Option<String>,
    /// `name`.
    #[serde(deserialize_with = "lenient_string")]
    pub name: Option<String>,
}

/// `MovieDto`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MovieDto {
    /// `year`.
    #[serde(deserialize_with = "lenient_string")]
    pub year: Option<String>,
}

/// `ShowImagesDto`: one `/metadata/programs/` entry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ShowImagesDto {
    /// `programID`.
    #[serde(rename = "programID", deserialize_with = "lenient_string")]
    pub program_id: Option<String>,
    /// `code`: set on an entry SD could not serve (an image limit, say).
    #[serde(deserialize_with = "lenient_opt_i64")]
    pub code: Option<i64>,
    /// `message`.
    #[serde(deserialize_with = "lenient_string")]
    pub message: Option<String>,
    /// `data` — read through `ImageDataArrayConverter`: anything but an array
    /// is no images.
    #[serde(deserialize_with = "image_data")]
    pub data: Vec<ImageDataDto>,
}

/// `ImageDataDto`: one artwork entry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ImageDataDto {
    /// `width`.
    #[serde(deserialize_with = "lenient_string")]
    pub width: Option<String>,
    /// `height`.
    #[serde(deserialize_with = "lenient_string")]
    pub height: Option<String>,
    /// `uri`.
    #[serde(deserialize_with = "lenient_string")]
    pub uri: Option<String>,
    /// `size`.
    #[serde(deserialize_with = "lenient_string")]
    pub size: Option<String>,
    /// `aspect`.
    #[serde(deserialize_with = "lenient_string")]
    pub aspect: Option<String>,
    /// `category`.
    #[serde(deserialize_with = "lenient_string")]
    pub category: Option<String>,
    /// `text`: `"yes"` when the artwork carries the title.
    #[serde(deserialize_with = "lenient_string")]
    pub text: Option<String>,
    /// `primary`.
    #[serde(deserialize_with = "lenient_string")]
    pub primary: Option<String>,
    /// `tier`.
    #[serde(deserialize_with = "lenient_string")]
    pub tier: Option<String>,
}

/// A list member: `null` is the empty list the DTO's initializer gives it.
fn list<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

/// A string member, as `JsonStringConverter` reads it: a string, or the raw
/// text of a number or boolean; `null` is `None`.
fn lenient_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(text) => Some(text),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(flag) => Some(flag.to_string()),
        serde_json::Value::Null => None,
        other => {
            return Err(serde::de::Error::custom(format!(
                "expected a string, found {other}"
            )));
        }
    })
}

/// A number member that may arrive as a string
/// (`JsonNumberHandling.AllowReadingFromString`); `null` is `None`.
fn lenient_opt_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Number(number) => number
            .as_i64()
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom(format!("{number} is not an integer"))),
        serde_json::Value::String(text) => text
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| serde::de::Error::custom(format!("{text:?} is not an integer"))),
        serde_json::Value::Null => Ok(None),
        other => Err(serde::de::Error::custom(format!(
            "expected an integer, found {other}"
        ))),
    }
}

/// [`lenient_opt_i64`] for a non-nullable member, where `null` is `0`.
fn lenient_i64<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(lenient_opt_i64(deserializer)?.unwrap_or_default())
}

/// A `DateTime?` member: an ISO 8601 instant, or a bare `yyyy-MM-dd` date
/// (SD's `originalAirDate`), read as midnight. `null` is `None`.
fn lenient_datetime<'de, D>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(text) = Option::<String>::deserialize(deserializer)? else {
        return Ok(None);
    };
    parse_datetime(&text)
        .map(Some)
        .ok_or_else(|| serde::de::Error::custom(format!("{text:?} is not a date")))
}

/// Parses an SD timestamp: RFC 3339, or a bare date at midnight UTC.
pub(super) fn parse_datetime(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();
    if let Ok(instant) = DateTime::parse_from_rfc3339(text) {
        return Some(instant.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|midnight| midnight.and_utc())
}

/// `ImageDataArrayConverter.Read`: an array's non-null elements; anything else
/// (an object, a string, `null`) is skipped and read as no images.
fn image_data<'de, D>(deserializer: D) -> Result<Vec<ImageDataDto>, D::Error>
where
    D: Deserializer<'de>,
{
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Array(items) => items
            .into_iter()
            .filter(|item| !item.is_null())
            .map(|item| serde_json::from_value(item).map_err(serde::de::Error::custom))
            .collect(),
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    //! `SchedulesDirectDeserializeTests` (v12.2
    //! `tests/Jellyfin.LiveTv.Tests/SchedulesDirect/SchedulesDirectDeserializeTests.cs`),
    //! transliterated over the same `Test Data/SchedulesDirect` files, vendored
    //! byte-for-byte into `tests/data/schedules_direct/`. The C# assertions are
    //! the oracle.

    use chrono::{TimeZone, Utc};

    use super::{
        ChannelDto, DayDto, HeadendsDto, LineupsDto, ProgramDetailsDto,
        RequestScheduleForChannelDto, SdErrorCode, ShowImagesDto, TokenDto,
    };

    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/schedules_direct")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    #[test]
    fn deserialize_token_response_live_success() {
        let token: TokenDto =
            serde_json::from_slice(&fixture("token_live_response.json")).expect("token");
        assert_eq!(token.code, 0);
        assert_eq!(token.message.as_deref(), Some("OK"));
        assert_eq!(token.server_id.as_deref(), Some("AWS-SD-web.1"));
        assert_eq!(
            token.token_timestamp,
            Some(Utc.with_ymd_and_hms(2016, 8, 23, 13, 55, 25).unwrap())
        );
        assert_eq!(
            token.token.as_deref(),
            Some("f3fca79989cafe7dead71beefedc812b")
        );
    }

    #[test]
    fn deserialize_token_response_offline_success() {
        let token: TokenDto =
            serde_json::from_slice(&fixture("token_offline_response.json")).expect("token");
        assert_eq!(token.code, 3_000);
        assert_eq!(
            token.message.as_deref(),
            Some("Server offline for maintenance.")
        );
        assert_eq!(token.server_id.as_deref(), Some("20141201.web.1"));
        assert_eq!(
            token.token_timestamp,
            Some(Utc.with_ymd_and_hms(2015, 4, 23, 0, 3, 32).unwrap())
        );
        assert_eq!(
            token.token.as_deref(),
            Some("CAFEDEADBEEFCAFEDEADBEEFCAFEDEADBEEFCAFE")
        );
        assert_eq!(token.response.as_deref(), Some("SERVICE_OFFLINE"));
    }

    #[test]
    fn serialize_schedule_request_success() {
        let expected = String::from_utf8(fixture("schedules_request.json")).expect("utf-8");
        let request = [
            RequestScheduleForChannelDto {
                station_id: "20454".to_owned(),
                date: vec!["2015-03-13".to_owned(), "2015-03-17".to_owned()],
            },
            RequestScheduleForChannelDto {
                station_id: "10021".to_owned(),
                date: vec!["2015-03-12".to_owned(), "2015-03-13".to_owned()],
            },
        ];
        assert_eq!(
            serde_json::to_string(&request).expect("serialize"),
            expected.trim()
        );
    }

    #[test]
    fn deserialize_schedule_response_success() {
        let days: Vec<DayDto> =
            serde_json::from_slice(&fixture("schedules_response.json")).expect("days");
        assert_eq!(days.len(), 1);
        let day = &days[0];
        assert_eq!(day.station_id.as_deref(), Some("20454"));
        assert_eq!(day.programs.len(), 2);
        let first = &day.programs[0];
        assert_eq!(first.program_id.as_deref(), Some("SH005371070000"));
        assert_eq!(
            first.air_date_time,
            Some(Utc.with_ymd_and_hms(2015, 3, 3, 0, 0, 0).unwrap())
        );
        assert_eq!(first.duration, 1_800);
        assert_eq!(first.md5.as_deref(), Some("Sy8HEMBPcuiAx3FBukUhKQ"));
        assert_eq!(first.new, Some(true));
        assert_eq!(first.audio_properties, ["stereo", "cc"]);
        assert_eq!(first.video_properties, ["hdtv"]);
    }

    #[test]
    fn deserialize_program_response_success() {
        let programs: Vec<ProgramDetailsDto> =
            serde_json::from_slice(&fixture("programs_response.json")).expect("programs");
        assert_eq!(programs.len(), 2);
        let first = &programs[0];
        assert_eq!(first.program_id.as_deref(), Some("EP000000060003"));
        assert_eq!(first.titles.len(), 1);
        assert_eq!(first.titles[0].title120.as_deref(), Some("'Allo 'Allo!"));
        assert_eq!(
            first
                .event_details
                .as_ref()
                .and_then(|e| e.sub_type.as_deref()),
            Some("Series")
        );
        let descriptions = first.descriptions.as_ref().expect("descriptions");
        assert_eq!(
            descriptions.description1000[0]
                .description_language
                .as_deref(),
            Some("en")
        );
        assert_eq!(
            descriptions.description1000[0].description.as_deref(),
            Some("A disguised British Intelligence officer is sent to help the airmen.")
        );
        assert_eq!(
            first.original_air_date,
            Some(Utc.with_ymd_and_hms(1985, 11, 4, 0, 0, 0).unwrap())
        );
        assert_eq!(first.genres, ["Sitcom"]);
        assert_eq!(
            first.episode_title150.as_deref(),
            Some("The Poloceman Cometh")
        );
        let gracenote = first.metadata[0].gracenote.as_ref().expect("gracenote");
        assert_eq!((gracenote.season, gracenote.episode), (2, 3));
        assert_eq!(first.cast.len(), 13);
        assert_eq!(first.cast[0].person_id.as_deref(), Some("383774"));
        assert_eq!(first.cast[0].name_id.as_deref(), Some("392649"));
        assert_eq!(first.cast[0].name.as_deref(), Some("Gorden Kaye"));
        assert_eq!(first.cast[0].role.as_deref(), Some("Actor"));
        assert_eq!(first.cast[0].billing_order.as_deref(), Some("01"));
        assert_eq!(first.crew.len(), 3);
        assert_eq!(first.crew[0].person_id.as_deref(), Some("354407"));
        assert_eq!(first.crew[0].name_id.as_deref(), Some("363281"));
        assert_eq!(first.crew[0].name.as_deref(), Some("David Croft"));
        assert_eq!(first.crew[0].role.as_deref(), Some("Director"));
        assert_eq!(first.crew[0].billing_order.as_deref(), Some("01"));
    }

    #[test]
    fn deserialize_metadata_programs_response_success() {
        let images: Vec<ShowImagesDto> =
            serde_json::from_slice(&fixture("metadata_programs_response.json")).expect("images");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].program_id.as_deref(), Some("SH00712240"));
        assert_eq!(images[0].data.len(), 4);
        let first = &images[0].data[0];
        assert_eq!(first.width.as_deref(), Some("135"));
        assert_eq!(first.height.as_deref(), Some("180"));
        assert_eq!(first.uri.as_deref(), Some("assets/p282288_b_v2_aa.jpg"));
        assert_eq!(first.size.as_deref(), Some("Sm"));
        assert_eq!(first.aspect.as_deref(), Some("3x4"));
        assert_eq!(first.category.as_deref(), Some("Banner-L3"));
        assert_eq!(first.text.as_deref(), Some("yes"));
        assert_eq!(first.primary.as_deref(), Some("true"));
        assert_eq!(first.tier.as_deref(), Some("Series"));
    }

    #[test]
    fn deserialize_metadata_programs_image_limit_response_success() {
        let images: Vec<ShowImagesDto> =
            serde_json::from_slice(&fixture("metadata_programs_image_limit_response.json"))
                .expect("images");
        assert_eq!(images.len(), 2);
        // A normal result with image data and no error code.
        assert_eq!(images[0].program_id.as_deref(), Some("SH00712240"));
        assert_eq!(images[0].code, None);
        assert_eq!(images[0].data.len(), 1);
        // A per-entry trial image download limit error (SD code 5003).
        assert_eq!(images[1].program_id.as_deref(), Some("SH00712241"));
        assert_eq!(
            images[1].code,
            Some(SdErrorCode::MaxImageDownloadsTrial.code())
        );
        assert!(images[1].data.is_empty());
    }

    #[test]
    fn deserialize_headends_response_success() {
        let headends: Vec<HeadendsDto> =
            serde_json::from_slice(&fixture("headends_response.json")).expect("headends");
        assert_eq!(headends.len(), 8);
        assert_eq!(headends[0].headend.as_deref(), Some("CA00053"));
        assert_eq!(headends[0].transport.as_deref(), Some("Cable"));
        assert_eq!(headends[0].location.as_deref(), Some("Beverly Hills"));
        assert_eq!(headends[0].lineups.len(), 2);
        assert_eq!(
            headends[0].lineups[0].name.as_deref(),
            Some("Time Warner Cable - Cable")
        );
        assert_eq!(
            headends[0].lineups[0].lineup.as_deref(),
            Some("USA-CA00053-DEFAULT")
        );
        assert_eq!(
            headends[0].lineups[0].uri.as_deref(),
            Some("/20141201/lineups/USA-CA00053-DEFAULT")
        );
    }

    #[test]
    fn deserialize_lineups_response_success() {
        let lineups: LineupsDto =
            serde_json::from_slice(&fixture("lineups_response.json")).expect("lineups");
        assert_eq!(lineups.code, 0);
        assert_eq!(lineups.server_id.as_deref(), Some("20141201.web.1"));
        assert_eq!(
            lineups.lineup_timestamp,
            Some(Utc.with_ymd_and_hms(2015, 4, 17, 14, 22, 17).unwrap())
        );
        assert_eq!(lineups.lineups.len(), 5);
        let first = &lineups.lineups[0];
        assert_eq!(first.lineup.as_deref(), Some("GBR-0001317-DEFAULT"));
        assert_eq!(
            first.name.as_deref(),
            Some("Freeview - Carlton - LWT (Southeast)")
        );
        assert_eq!(first.transport.as_deref(), Some("DVB-T"));
        assert_eq!(first.location.as_deref(), Some("London"));
        assert_eq!(
            first.uri.as_deref(),
            Some("/20141201/lineups/GBR-0001317-DEFAULT")
        );
        assert_eq!(lineups.lineups[4].name.as_deref(), Some("DELETED LINEUP"));
        assert_eq!(lineups.lineups[4].is_deleted, Some(true));
    }

    #[test]
    fn deserialize_lineup_response_success() {
        let channel: ChannelDto =
            serde_json::from_slice(&fixture("lineup_response.json")).expect("lineup");
        assert_eq!(channel.map.len(), 2);
        let first = &channel.map[0];
        assert_eq!(first.station_id.as_deref(), Some("24326"));
        assert_eq!(first.channel.as_deref(), Some("001"));
        assert_eq!(first.provider_callsign.as_deref(), Some("BBC ONE South"));
        assert_eq!(first.logical_channel_number.as_deref(), Some("1"));
        assert_eq!(first.match_type.as_deref(), Some("providerCallsign"));
    }

    /// `JsonDefaults.Options`' tolerances: a number as a string, a string as a
    /// number, `null` lists, and an image `data` that is not an array
    /// (`ImageDataArrayConverter` skips it).
    #[test]
    fn the_json_defaults_tolerances_hold() {
        let program: super::ProgramDto = serde_json::from_str(
            r#"{"programID":12,"duration":"1800","audioProperties":null,"new":null}"#,
        )
        .expect("program");
        assert_eq!(program.program_id.as_deref(), Some("12"));
        assert_eq!(program.duration, 1_800);
        assert!(
            program.audio_properties.is_empty(),
            "{:?}",
            program.audio_properties
        );
        assert_eq!(program.new, None);

        let images: Vec<ShowImagesDto> = serde_json::from_str(
            r#"[{"programID":"SH1","code":"5002","data":{"error":"x"}},{"programID":"SH2","data":[null,{"uri":"a.jpg","width":135}]}]"#,
        )
        .expect("images");
        assert_eq!(images[0].code, Some(5002));
        assert!(images[0].data.is_empty());
        assert_eq!(images[1].data.len(), 1);
        assert_eq!(images[1].data[0].width.as_deref(), Some("135"));
    }

    /// Every code in the v12.2 table round-trips, and an undefined one is not
    /// an `SdErrorCode` (`Enum.IsDefined`). These are jellyfin/jellyfin#16920's
    /// corrected values: 4001 is `AccountExpired`, not `InvalidUser`.
    #[test]
    fn the_error_code_table_is_v12_2s() {
        for (code, expected) in [
            (3000, SdErrorCode::ServiceOffline),
            (3001, SdErrorCode::ServiceBusy),
            (4001, SdErrorCode::AccountExpired),
            (4002, SdErrorCode::InvalidHash),
            (4003, SdErrorCode::InvalidUser),
            (4004, SdErrorCode::AccountTempLock),
            (4005, SdErrorCode::AccountLocked),
            (4006, SdErrorCode::TokenExpired),
            (4007, SdErrorCode::AppLocked),
            (4008, SdErrorCode::AccountInactive),
            (4009, SdErrorCode::MaxLoginAttempts),
            (4010, SdErrorCode::MaxIpAttempts),
            (4100, SdErrorCode::MaxScheduleRequests),
            (5000, SdErrorCode::ImageNotFound),
            (5002, SdErrorCode::MaxImageDownloads),
            (5003, SdErrorCode::MaxImageDownloadsTrial),
            (5004, SdErrorCode::MaxInvalidImages),
        ] {
            assert_eq!(SdErrorCode::from_code(code), Some(expected));
            assert_eq!(expected.code(), code);
        }
        for undefined in [0, 2999, 4000, 4011, 5001, 5005] {
            assert_eq!(SdErrorCode::from_code(undefined), None, "{undefined}");
        }
    }
}
