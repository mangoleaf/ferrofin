//! The XMLTV programme etag: port of `XmlTvProgramEtag` (v12.4
//! src/Jellyfin.LiveTv/Listings/XmlTvProgramEtag.cs).
//!
//! `XmlTvListingsProvider.GetProgramInfoWithEtag` stamps each airing with a
//! SHA-256 over a canonical dump of the `ProgramInfo` fields
//! `GuideManager.GetProgram` consumes, and `GetProgram` stores it as the
//! programme's `ProgramEtag` provider id. Jellyfin skips an airing whose etag
//! matches the stored one, so the value has to be the one Jellyfin computes,
//! byte for byte, for a database to go back to Jellyfin without every
//! programme looking changed.

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

/// `XmlTvProgramEtag.Prefix`: an etag that starts with it is XMLTV's, the
/// only kind `GuideManager.GetProgram` skips an unchanged airing for.
pub(crate) const PREFIX: &str = "xmltv-sha256-v1:";

/// The `ProgramInfo` fields the etag hashes, as
/// `XmlTvListingsProvider.GetProgramInfo` fills them for one airing (after
/// its movie reset). The fields the XMLTV provider never sets
/// (`OriginalAirDate`, `IsHD`, `Audio`, `CommunityRating`, `ImagePath`,
/// `LogoImageUrl` and both provider id dictionaries) are hashed as null or
/// empty, as they are upstream.
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)] // the upstream ProgramInfo flags
pub(crate) struct EtagInput<'a> {
    /// `Id`: `{channelId}_{start:O}`.
    pub id: Option<&'a str>,
    /// `ChannelId`: the guide's channel id.
    pub channel_id: &'a str,
    /// `Name`.
    pub name: Option<&'a str>,
    /// `OfficialRating`.
    pub official_rating: Option<&'a str>,
    /// `Overview`.
    pub overview: Option<&'a str>,
    /// `StartDate`.
    pub start: Option<DateTime<Utc>>,
    /// `EndDate`.
    pub end: Option<DateTime<Utc>>,
    /// `Genres`: the categories that are not blank, as written.
    pub genres: Vec<&'a str>,
    /// `IsRepeat`.
    pub is_repeat: bool,
    /// `EpisodeTitle`.
    pub episode_title: Option<&'a str>,
    /// `ImageUrl`.
    pub image_url: Option<&'a str>,
    /// `ThumbImageUrl`.
    pub thumb_image_url: Option<&'a str>,
    /// `BackdropImageUrl`.
    pub backdrop_image_url: Option<&'a str>,
    /// `IsMovie`.
    pub is_movie: bool,
    /// `IsSports`.
    pub is_sports: bool,
    /// `IsSeries`: the provider's own value (an episode number), not
    /// `GuideManager`'s widened one.
    pub is_series: bool,
    /// `IsLive`.
    pub is_live: bool,
    /// `IsNews`.
    pub is_news: bool,
    /// `IsKids`.
    pub is_kids: bool,
    /// `IsPremiere`.
    pub is_premiere: bool,
    /// `ProductionYear`.
    pub production_year: Option<i32>,
    /// `SeriesId`.
    pub series_id: Option<&'a str>,
    /// `ShowId`.
    pub show_id: Option<&'a str>,
    /// `SeasonNumber`.
    pub season_number: Option<i32>,
    /// `EpisodeNumber`.
    pub episode_number: Option<i32>,
}

/// `XmlTvProgramEtag.TryCreate`: the airing's etag, or `None` when it has no
/// id, channel or valid start/end (upstream logs that and treats the airing
/// as updated on every refresh).
pub(crate) fn xmltv_program_etag(info: &EtagInput<'_>) -> Option<String> {
    let id = info.id.filter(|id| !id.trim().is_empty())?;
    if info.channel_id.trim().is_empty() {
        return None;
    }
    let (start, end) = (info.start?, info.end?);
    if end <= start {
        return None;
    }
    // upstream's `new StringBuilder(1024)`; only a starting size.
    let mut b = String::with_capacity(1024);
    // Keep this list aligned with the ProgramInfo fields consumed by GuideManager.
    value(&mut b, "schema", Some("xmltv-programinfo-v1"));
    value(&mut b, "Id", Some(id));
    value(&mut b, "ChannelId", Some(info.channel_id));
    value(&mut b, "Name", info.name);
    value(&mut b, "OfficialRating", info.official_rating);
    value(&mut b, "Overview", info.overview);
    value(&mut b, "StartDate", Some(&round_trip(start)));
    value(&mut b, "EndDate", Some(&round_trip(end)));
    value(&mut b, "Genres.Count", Some(&info.genres.len().to_string()));
    for (i, genre) in info.genres.iter().enumerate() {
        value(&mut b, &format!("Genres[{i}]"), Some(genre));
    }
    value(&mut b, "OriginalAirDate", None);
    value(&mut b, "IsHD", None);
    value(&mut b, "Audio", None);
    value(&mut b, "CommunityRating", None);
    flag(&mut b, "IsRepeat", info.is_repeat);
    value(&mut b, "EpisodeTitle", info.episode_title);
    value(&mut b, "ImagePath", None);
    value(&mut b, "ImageUrl", info.image_url);
    value(&mut b, "ThumbImageUrl", info.thumb_image_url);
    value(&mut b, "LogoImageUrl", None);
    value(&mut b, "BackdropImageUrl", info.backdrop_image_url);
    flag(&mut b, "IsMovie", info.is_movie);
    flag(&mut b, "IsSports", info.is_sports);
    flag(&mut b, "IsSeries", info.is_series);
    flag(&mut b, "IsLive", info.is_live);
    flag(&mut b, "IsNews", info.is_news);
    flag(&mut b, "IsKids", info.is_kids);
    flag(&mut b, "IsPremiere", info.is_premiere);
    number(&mut b, "ProductionYear", info.production_year);
    value(&mut b, "SeriesId", info.series_id);
    value(&mut b, "ShowId", info.show_id);
    number(&mut b, "SeasonNumber", info.season_number);
    number(&mut b, "EpisodeNumber", info.episode_number);
    value(&mut b, "ProviderIds.Count", Some("0"));
    value(&mut b, "SeriesProviderIds.Count", Some("0"));
    let hash = Sha256::digest(b.as_bytes());
    // `Convert.ToHexString`: uppercase.
    let mut etag = String::from(PREFIX);
    for byte in hash {
        let _ = write!(etag, "{byte:02X}");
    }
    Some(etag)
}

/// `AppendValue(string?)`: `name|N|0|` for null, else `name|S|<length>|value`,
/// where the length counts UTF-16 code units, as .NET's `string.Length` does.
fn value(b: &mut String, name: &str, value: Option<&str>) {
    b.push_str(name);
    b.push('|');
    match value {
        None => b.push_str("N|0|"),
        Some(v) => {
            b.push_str("S|");
            b.push_str(&v.encode_utf16().count().to_string());
            b.push('|');
            b.push_str(v);
        }
    }
    b.push('\n');
}

/// `AppendValue(bool)`.
fn flag(b: &mut String, name: &str, set: bool) {
    value(b, name, Some(if set { "true" } else { "false" }));
}

/// `AppendValue(int?)`, invariant culture.
fn number(b: &mut String, name: &str, n: Option<i32>) {
    value(b, name, n.map(|n| n.to_string()).as_deref());
}

/// `FormatDateTime`: the UTC instant in .NET's round-trip (`O`) form,
/// `yyyy-MM-ddTHH:mm:ss.fffffffZ`.
fn round_trip(at: DateTime<Utc>) -> String {
    format!(
        "{}.{:07}Z",
        at.format("%Y-%m-%dT%H:%M:%S"),
        at.timestamp_subsec_nanos() / 100
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).expect("instant").to_utc()
    }

    fn input<'a>() -> EtagInput<'a> {
        EtagInput {
            id: Some("one.tv_2026-11-11T05:00:00.0000000+00:00"),
            channel_id: "one.tv",
            name: Some("Plain Show"),
            start: Some(at("2026-11-11T05:00:00Z")),
            end: Some(at("2026-11-11T06:00:00Z")),
            ..EtagInput::default()
        }
    }

    /// `TryCreate` refuses an airing without an id, a channel, or an end after
    /// its start.
    #[rstest::rstest]
    #[case::no_id(EtagInput { id: Some("  "), ..input() })]
    #[case::no_channel(EtagInput { channel_id: "", ..input() })]
    #[case::no_start(EtagInput { start: None, ..input() })]
    #[case::no_end(EtagInput { end: None, ..input() })]
    #[case::end_not_after_start(EtagInput { end: Some(at("2026-11-11T05:00:00Z")), ..input() })]
    fn an_airing_without_identity_or_span_has_no_etag(#[case] info: EtagInput<'_>) {
        assert_eq!(xmltv_program_etag(&info), None);
    }

    /// The length is .NET's: UTF-16 code units, so a character outside the
    /// Basic Multilingual Plane counts twice, and null differs from empty.
    #[test]
    fn values_are_framed_by_their_utf16_length() {
        let mut b = String::new();
        value(&mut b, "Name", Some("é🎬"));
        value(&mut b, "Overview", Some(""));
        value(&mut b, "Audio", None);
        assert_eq!(b, "Name|S|3|é🎬\nOverview|S|0|\nAudio|N|0|\n");
        assert_eq!(
            round_trip(at("2026-11-11T05:00:00.1234567Z")),
            "2026-11-11T05:00:00.1234567Z"
        );
    }
}
