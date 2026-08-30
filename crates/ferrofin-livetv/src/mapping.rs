//! The tuner-channel ↔ listings-channel matcher.
//!
//! Port of `Jellyfin.LiveTv.Listings.EpgChannelData` plus the private helpers
//! `ListingsManager` matches a tuner's lineup against a listings provider's own
//! channel list with: `GetMappedChannel`, `GetEpgChannelFromTunerChannel`,
//! `GetTunerChannelMapping` and `IsListingProviderEnabledForTuner`.
//!
//! It lives apart from the manager because it is pure: the manager loads the
//! two lineups (tuner channels out of the channel cache, provider channels out
//! of the XMLTV document) and this module decides which pairs up with which.
//! That is also what makes the branch order — the load-bearing part — testable
//! against the C# case by case.

use std::collections::HashMap;

use ferrofin_model::dto::NameValuePair;
use ferrofin_model::live_tv::{ListingsProviderInfo, TunerChannelMapping};

/// The suffix Schedules Direct appends to a tuner's channel id, stripped
/// (along with the leading `I`) before the id is looked up in the guide.
///
/// Port of `ListingsManager.GetEpgChannelFromTunerChannel`'s literal.
const SCHEDULES_DIRECT_SUFFIX: &str = ".json.schedulesdirect.org";

/// One channel of a listings provider's own lineup.
///
/// Port of the `ChannelInfo` fields `XmlTvListingsProvider.GetChannels`
/// populates: the guide document's `<channel id>`, its `<display-name>`, and
/// the number — which for XMLTV falls back to the id, because that fallback is
/// what feeds the by-number match arm below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpgChannel {
    /// The guide's channel id.
    pub id: String,
    /// The guide's display name.
    pub name: String,
    /// The guide's channel number, or [`id`](Self::id) when it carries none.
    pub number: String,
}

/// One channel of a tuner host's lineup, as the matcher needs it.
///
/// Port of the `ChannelInfo` fields `M3uParser` fills in for an M3U tuner.
/// [`id`](Self::id) is the external channel id
/// (`m3u_{MD5(tuner url)}{MD5(stream url)}`, see [`m3u_channel_id`]) and
/// [`tuner_channel_id`](Self::tuner_channel_id) is the playlist's `tvg-id`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TunerChannel {
    /// The external channel id — `ChannelInfo.Id`.
    pub id: String,
    /// The playlist's `tvg-id`/`channel-id` — `ChannelInfo.TunerChannelId`.
    pub tuner_channel_id: String,
    /// The channel number.
    pub number: String,
    /// The display name.
    pub name: String,
    /// The id of the tuner host this channel came from.
    pub tuner_host_id: String,
}

/// The external channel id an M3U tuner mints for one stream URL.
///
/// Port of `M3UTunerHost.GetFullChannelIdPrefix` (`BaseTunerHost.ChannelIdPrefix`
/// = `"m3u_"`, plus `MD5(tuner.Url)`) followed by `M3uParser.Parse`'s
/// `channel.Id = channelIdPrefix + MD5(streamUrl)`. Both halves are
/// `BaseExtensions.GetMD5` — MD5 over the string's UTF-16LE bytes, rendered as
/// a .NET `Guid.ToString("N")` — which is exactly
/// [`ferrofin_common::extensions::get_md5`] in `simple` form.
#[must_use]
pub fn m3u_channel_id(tuner_url: &str, stream_url: &str) -> String {
    format!(
        "m3u_{}{}",
        ferrofin_common::extensions::get_md5(tuner_url).simple(),
        ferrofin_common::extensions::get_md5(stream_url).simple()
    )
}

/// Whether `info` takes its listings from the tuner host `tuner_host_id`.
///
/// Port of `ListingsManager.IsListingProviderEnabledForTuner`.
#[must_use]
pub fn is_listing_provider_enabled_for_tuner(
    info: &ListingsProviderInfo,
    tuner_host_id: &str,
) -> bool {
    info.enable_all_tuners
        || info
            .enabled_tuners
            .iter()
            .any(|t| t.eq_ignore_ascii_case(tuner_host_id))
}

/// Resolves `channel_id` through the provider's manual channel mappings.
///
/// Port of `ListingsManager.GetMappedChannel`: the first pair whose `Name`
/// matches (case-insensitively) contributes its `Value`; an unmapped id is
/// returned unchanged.
#[must_use]
pub fn mapped_channel<'a>(channel_id: &'a str, mappings: &'a [NameValuePair]) -> &'a str {
    mappings
        .iter()
        .find(|m| {
            m.name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(channel_id))
        })
        .and_then(|m| m.value.as_deref())
        .unwrap_or(channel_id)
}

/// A listings provider's lineup, indexed the three ways the matcher looks it up.
///
/// Port of `EpgChannelData`: three case-insensitive dictionaries keyed by id,
/// by number and by [normalized name](normalize_name). A later channel with the
/// same key overwrites an earlier one, as the C# dictionary assignment does.
#[derive(Debug, Clone, Default)]
// The three lookups ARE the type: `EpgChannelData`'s whole job is to answer
// "by id / by number / by name", and the prefix is what names each index.
#[allow(clippy::struct_field_names)]
pub struct EpgChannelData {
    /// Channels keyed by lower-cased id.
    by_id: HashMap<String, EpgChannel>,
    /// Channels keyed by lower-cased number.
    by_number: HashMap<String, EpgChannel>,
    /// Channels keyed by lower-cased [normalized name](normalize_name).
    by_name: HashMap<String, EpgChannel>,
}

impl EpgChannelData {
    /// Indexes a provider lineup.
    #[must_use]
    pub fn new(channels: &[EpgChannel]) -> Self {
        let mut data = Self::default();
        for channel in channels {
            data.by_id
                .insert(channel.id.to_ascii_lowercase(), channel.clone());
            if !channel.number.is_empty() {
                data.by_number
                    .insert(channel.number.to_ascii_lowercase(), channel.clone());
            }
            let normalized = normalize_name(&channel.name);
            if !normalized.trim().is_empty() {
                data.by_name
                    .insert(normalized.to_ascii_lowercase(), channel.clone());
            }
        }
        data
    }

    /// The channel with this id, if any.
    #[must_use]
    pub fn by_id(&self, id: &str) -> Option<&EpgChannel> {
        self.by_id.get(&id.to_ascii_lowercase())
    }

    /// The channel with this number, if any.
    #[must_use]
    pub fn by_number(&self, number: &str) -> Option<&EpgChannel> {
        self.by_number.get(&number.to_ascii_lowercase())
    }

    /// The channel with this already-[normalized](normalize_name) name, if any.
    #[must_use]
    pub fn by_name(&self, name: &str) -> Option<&EpgChannel> {
        self.by_name.get(&name.to_ascii_lowercase())
    }
}

/// Strips the characters a channel name may or may not carry before it is used
/// as a match key.
///
/// Port of `EpgChannelData.NormalizeName`: spaces and hyphens removed.
#[must_use]
pub fn normalize_name(value: &str) -> String {
    value.replace([' ', '-'], "")
}

/// The provider channel a tuner channel takes its listings from, or `None`.
///
/// Port of `ListingsManager.GetEpgChannelFromTunerChannel`. The branch order is
/// load-bearing — external id, then the playlist's `tvg-id` (with the Schedules
/// Direct suffix stripped), then the number, then the normalized name — and the
/// first three run their key through [`mapped_channel`] first, falling back to
/// the raw key when the mapping resolves to blank.
#[must_use]
pub fn epg_channel_for_tuner_channel<'a>(
    mappings: &[NameValuePair],
    tuner: &TunerChannel,
    epg: &'a EpgChannelData,
) -> Option<&'a EpgChannel> {
    if !tuner.id.trim().is_empty() {
        let mapped = mapped_channel(&tuner.id, mappings);
        let key = if mapped.trim().is_empty() {
            tuner.id.as_str()
        } else {
            mapped
        };
        if let Some(found) = epg.by_id(key) {
            return Some(found);
        }
    }

    if !tuner.tuner_channel_id.trim().is_empty() {
        let stripped = strip_schedules_direct(&tuner.tuner_channel_id);
        let mapped = mapped_channel(&stripped, mappings);
        let key = if mapped.trim().is_empty() {
            stripped.as_str()
        } else {
            mapped
        };
        if let Some(found) = epg.by_id(key) {
            return Some(found);
        }
    }

    if !tuner.number.trim().is_empty() {
        let mapped = mapped_channel(&tuner.number, mappings);
        let key = if mapped.trim().is_empty() {
            tuner.number.as_str()
        } else {
            mapped
        };
        if let Some(found) = epg.by_number(key) {
            return Some(found);
        }
    }

    if !tuner.name.trim().is_empty()
        && let Some(found) = epg.by_name(&normalize_name(&tuner.name))
    {
        return Some(found);
    }

    None
}

/// One row of `GET /LiveTv/ChannelMappingOptions`' `TunerChannels`.
///
/// Port of `ListingsManager.GetTunerChannelMapping`: the display name is
/// `"{Number} {Name}"` when the channel has a number, the id is the tuner
/// channel's external id, and the provider columns are filled in from
/// [`epg_channel_for_tuner_channel`] when it matched.
#[must_use]
pub fn tuner_channel_mapping(
    tuner: &TunerChannel,
    mappings: &[NameValuePair],
    epg: &EpgChannelData,
) -> TunerChannelMapping {
    let name = if tuner.number.trim().is_empty() {
        tuner.name.clone()
    } else {
        format!("{} {}", tuner.number, tuner.name)
    };
    let matched = epg_channel_for_tuner_channel(mappings, tuner, epg);
    TunerChannelMapping {
        name: Some(name),
        provider_channel_name: matched.map(|c| c.name.clone()),
        provider_channel_id: matched.map(|c| c.id.clone()),
        id: Some(tuner.id.clone()),
    }
}

/// Removes the Schedules Direct station-id decoration from a tuner channel id.
///
/// Port of the `.json.schedulesdirect.org` branch inside
/// `GetEpgChannelFromTunerChannel`: the suffix goes, then any leading `I`.
fn strip_schedules_direct(tuner_channel_id: &str) -> String {
    let lowered = tuner_channel_id.to_ascii_lowercase();
    if !lowered.contains(SCHEDULES_DIRECT_SUFFIX) {
        return tuner_channel_id.to_owned();
    }
    // `Replace(..., OrdinalIgnoreCase)` — every occurrence, whatever its case.
    let mut out = String::with_capacity(tuner_channel_id.len());
    let mut rest = tuner_channel_id;
    while let Some(at) = rest.to_ascii_lowercase().find(SCHEDULES_DIRECT_SUFFIX) {
        out.push_str(&rest[..at]);
        rest = &rest[at + SCHEDULES_DIRECT_SUFFIX.len()..];
    }
    out.push_str(rest);
    out.trim_start_matches('I').to_owned()
}

#[cfg(test)]
mod tests {
    use super::{
        EpgChannel, EpgChannelData, TunerChannel, epg_channel_for_tuner_channel,
        is_listing_provider_enabled_for_tuner, m3u_channel_id, mapped_channel, normalize_name,
        strip_schedules_direct, tuner_channel_mapping,
    };
    use ferrofin_model::dto::NameValuePair;
    use ferrofin_model::live_tv::ListingsProviderInfo;

    fn epg(id: &str, name: &str, number: &str) -> EpgChannel {
        EpgChannel {
            id: id.to_owned(),
            name: name.to_owned(),
            number: number.to_owned(),
        }
    }

    fn pair(name: &str, value: &str) -> NameValuePair {
        NameValuePair {
            name: Some(name.to_owned()),
            value: Some(value.to_owned()),
        }
    }

    #[test]
    fn m3u_channel_id_matches_the_jellyfin_oracle() {
        // Pinned against the live Jellyfin 10.11.8 lab, which returned exactly
        // this `Id` for the fixture's first channel.
        assert_eq!(
            m3u_channel_id(
                "/media/synth/livetv/channels.m3u",
                "http://livetv-source:8000/live.ts?ch=1"
            ),
            "m3u_5581ab8b17869acbac4cc454abc401683f2ed88fba54056b16c110c12038d26b"
        );
        assert_eq!(
            m3u_channel_id(
                "/media/synth/livetv/channels.m3u",
                "http://livetv-source:8000/live.ts?ch=2"
            ),
            "m3u_5581ab8b17869acbac4cc454abc40168446c98aa7b6e4352b95b322018c4eebf"
        );
    }

    #[test]
    fn normalize_name_strips_spaces_and_hyphens() {
        assert_eq!(normalize_name("BBC One - HD"), "BBCOneHD");
        assert_eq!(normalize_name(""), "");
    }

    #[test]
    fn mapped_channel_is_case_insensitive_and_falls_through() {
        let mappings = [pair("TunerA", "provider-a")];
        assert_eq!(mapped_channel("tunera", &mappings), "provider-a");
        assert_eq!(mapped_channel("other", &mappings), "other");
        assert_eq!(mapped_channel("tunera", &[]), "tunera");
    }

    #[test]
    fn strip_schedules_direct_removes_suffix_and_leading_i() {
        assert_eq!(
            strip_schedules_direct("I12345.json.schedulesdirect.org"),
            "12345"
        );
        assert_eq!(strip_schedules_direct("parity1"), "parity1");
    }

    #[test]
    fn match_ladder_prefers_the_external_id() {
        let data = EpgChannelData::new(&[epg("m3u_abc", "By Id", "9"), epg("parity1", "Tvg", "1")]);
        let tuner = TunerChannel {
            id: "m3u_abc".into(),
            tuner_channel_id: "parity1".into(),
            number: "1".into(),
            name: "Tvg".into(),
            tuner_host_id: "t".into(),
        };
        assert_eq!(
            epg_channel_for_tuner_channel(&[], &tuner, &data).map(|c| c.id.as_str()),
            Some("m3u_abc")
        );
    }

    #[test]
    fn match_ladder_falls_to_the_tuner_channel_id() {
        let data = EpgChannelData::new(&[epg("parity1", "parity1", "parity1")]);
        let tuner = TunerChannel {
            id: "m3u_unknown".into(),
            tuner_channel_id: "parity1".into(),
            number: "1".into(),
            name: "Parity One".into(),
            tuner_host_id: "t".into(),
        };
        assert_eq!(
            epg_channel_for_tuner_channel(&[], &tuner, &data).map(|c| c.id.as_str()),
            Some("parity1")
        );
    }

    #[test]
    fn match_ladder_falls_to_the_number_then_the_normalized_name() {
        let by_number = EpgChannelData::new(&[epg("x", "Nothing Alike", "7")]);
        let tuner = TunerChannel {
            id: "m3u_unknown".into(),
            tuner_channel_id: "no-such-tvg".into(),
            number: "7".into(),
            name: "Whatever".into(),
            tuner_host_id: "t".into(),
        };
        assert_eq!(
            epg_channel_for_tuner_channel(&[], &tuner, &by_number).map(|c| c.id.as_str()),
            Some("x")
        );

        let by_name = EpgChannelData::new(&[epg("y", "BBCOne", "99")]);
        let tuner = TunerChannel {
            id: "m3u_unknown".into(),
            tuner_channel_id: "no-such-tvg".into(),
            number: "3".into(),
            name: "BBC - One".into(),
            tuner_host_id: "t".into(),
        };
        assert_eq!(
            epg_channel_for_tuner_channel(&[], &tuner, &by_name).map(|c| c.id.as_str()),
            Some("y")
        );

        // Nothing matches on any arm.
        assert!(epg_channel_for_tuner_channel(&[], &tuner, &EpgChannelData::default()).is_none());
    }

    #[test]
    fn a_manual_mapping_moves_the_match() {
        let data = EpgChannelData::new(&[epg("parity1", "One", "1"), epg("parity2", "Two", "2")]);
        let tuner = TunerChannel {
            id: "m3u_one".into(),
            tuner_channel_id: "parity1".into(),
            number: "1".into(),
            name: "Parity One".into(),
            tuner_host_id: "t".into(),
        };
        // Unmapped: the tvg-id arm wins.
        assert_eq!(
            epg_channel_for_tuner_channel(&[], &tuner, &data).map(|c| c.id.as_str()),
            Some("parity1")
        );
        // Mapped on the external id: the FIRST arm now resolves, to parity2.
        let mappings = [pair("m3u_one", "parity2")];
        assert_eq!(
            epg_channel_for_tuner_channel(&mappings, &tuner, &data).map(|c| c.id.as_str()),
            Some("parity2")
        );
    }

    #[test]
    fn tuner_channel_mapping_prefixes_the_number_and_carries_the_match() {
        let data = EpgChannelData::new(&[epg("parity1", "parity1", "parity1")]);
        let tuner = TunerChannel {
            id: "m3u_one".into(),
            tuner_channel_id: "parity1".into(),
            number: "1".into(),
            name: "Parity One".into(),
            tuner_host_id: "t".into(),
        };
        let row = tuner_channel_mapping(&tuner, &[], &data);
        assert_eq!(row.name.as_deref(), Some("1 Parity One"));
        assert_eq!(row.id.as_deref(), Some("m3u_one"));
        assert_eq!(row.provider_channel_id.as_deref(), Some("parity1"));
        assert_eq!(row.provider_channel_name.as_deref(), Some("parity1"));

        // No number: the name is used bare. No match: the provider columns stay
        // absent (they are `skip_serializing_if = "Option::is_none"`).
        let bare = TunerChannel {
            number: String::new(),
            tuner_channel_id: "nope".into(),
            ..tuner.clone()
        };
        let row = tuner_channel_mapping(&bare, &[], &EpgChannelData::default());
        assert_eq!(row.name.as_deref(), Some("Parity One"));
        assert!(row.provider_channel_id.is_none());
        assert!(row.provider_channel_name.is_none());
    }

    #[test]
    fn tuner_enablement_honors_enable_all_and_the_list() {
        let all = ListingsProviderInfo {
            enable_all_tuners: true,
            ..ListingsProviderInfo::default()
        };
        assert!(is_listing_provider_enabled_for_tuner(&all, "anything"));

        let listed = ListingsProviderInfo {
            enable_all_tuners: false,
            enabled_tuners: vec!["ABC".to_owned()],
            ..ListingsProviderInfo::default()
        };
        assert!(is_listing_provider_enabled_for_tuner(&listed, "abc"));
        assert!(!is_listing_provider_enabled_for_tuner(&listed, "other"));
    }
}
