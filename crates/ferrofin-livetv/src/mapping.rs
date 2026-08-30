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

/// Compares two strings the way .NET's `StringComparison.OrdinalIgnoreCase` does.
///
/// Every `string.Equals(…, StringComparison.OrdinalIgnoreCase)` in
/// `ListingsManager` runs through here. .NET folds ORDINALLY, not ASCII-only: it
/// maps each char through the invariant simple uppercase table and compares the
/// results, so `"kanal ö"` and `"KANAL Ö"` are equal there.
/// `str::eq_ignore_ascii_case` leaves every non-ASCII char alone, which matches
/// on Jellyfin and misses here for any channel name, tvg-id or mapping key
/// outside ASCII.
#[must_use]
pub fn eq_ordinal_ignore_case(a: &str, b: &str) -> bool {
    a.chars()
        .map(ordinal_upper)
        .eq(b.chars().map(ordinal_upper))
}

/// The folded lookup key of a
/// `Dictionary<string, ChannelInfo>(StringComparer.OrdinalIgnoreCase)`.
///
/// Same folding as [`eq_ordinal_ignore_case`], materialized — `EpgChannelData`'s
/// three indices are ordinal-ignore-case dictionaries, so both the insert and the
/// lookup key go through this.
#[must_use]
pub fn ordinal_ignore_case_key(value: &str) -> String {
    value.chars().map(ordinal_upper).collect()
}

/// One char through .NET's invariant SIMPLE uppercase mapping.
///
/// `char.ToUpperInvariant` is a 1:1 table — it never expands one char into
/// several, so `ß` stays `ß`. Rust's `char::to_uppercase` is the FULL mapping
/// (`ß` → `SS`), so a multi-char expansion is discarded and the original char
/// kept: that is what keeps this ordinal rather than linguistic.
fn ordinal_upper(c: char) -> char {
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(one), None) => one,
        _ => c,
    }
}

/// Where `needle` occurs in `haystack`, folded ordinally, as a `[start, end)`
/// byte range of `haystack`.
///
/// Backs the `Replace(…, StringComparison.OrdinalIgnoreCase)` inside
/// `GetEpgChannelFromTunerChannel`. The range is in HAYSTACK bytes, not folded
/// ones, because folding is per char and can change a char's byte length
/// (`ſ` → `S`) — an offset taken in the folded string would not map back.
fn find_ordinal_ignore_case(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    let folded_needle = ordinal_ignore_case_key(needle);
    if folded_needle.is_empty() {
        return Some((0, 0));
    }
    for (at, _) in haystack.char_indices() {
        let mut folded = String::new();
        let mut end = at;
        for (offset, c) in haystack[at..].char_indices() {
            folded.push(ordinal_upper(c));
            end = at + offset + c.len_utf8();
            if folded.len() >= folded_needle.len() {
                break;
            }
        }
        if folded == folded_needle {
            return Some((at, end));
        }
    }
    None
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
            .any(|t| eq_ordinal_ignore_case(t, tuner_host_id))
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
                .is_some_and(|n| eq_ordinal_ignore_case(n, channel_id))
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
    /// Channels keyed by the ordinally folded id.
    by_id: HashMap<String, EpgChannel>,
    /// Channels keyed by the ordinally folded number.
    by_number: HashMap<String, EpgChannel>,
    /// Channels keyed by the ordinally folded [normalized name](normalize_name).
    by_name: HashMap<String, EpgChannel>,
}

impl EpgChannelData {
    /// Indexes a provider lineup.
    #[must_use]
    pub fn new(channels: &[EpgChannel]) -> Self {
        let mut data = Self::default();
        for channel in channels {
            data.by_id
                .insert(ordinal_ignore_case_key(&channel.id), channel.clone());
            if !channel.number.is_empty() {
                data.by_number
                    .insert(ordinal_ignore_case_key(&channel.number), channel.clone());
            }
            let normalized = normalize_name(&channel.name);
            if !normalized.trim().is_empty() {
                data.by_name
                    .insert(ordinal_ignore_case_key(&normalized), channel.clone());
            }
        }
        data
    }

    /// The channel with this id, if any.
    #[must_use]
    pub fn by_id(&self, id: &str) -> Option<&EpgChannel> {
        self.by_id.get(&ordinal_ignore_case_key(id))
    }

    /// The channel with this number, if any.
    #[must_use]
    pub fn by_number(&self, number: &str) -> Option<&EpgChannel> {
        self.by_number.get(&ordinal_ignore_case_key(number))
    }

    /// The channel with this already-[normalized](normalize_name) name, if any.
    #[must_use]
    pub fn by_name(&self, name: &str) -> Option<&EpgChannel> {
        self.by_name.get(&ordinal_ignore_case_key(name))
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
    if find_ordinal_ignore_case(tuner_channel_id, SCHEDULES_DIRECT_SUFFIX).is_none() {
        return tuner_channel_id.to_owned();
    }
    // `Replace(..., OrdinalIgnoreCase)` — every occurrence, whatever its case.
    let mut out = String::with_capacity(tuner_channel_id.len());
    let mut rest = tuner_channel_id;
    while let Some((at, end)) = find_ordinal_ignore_case(rest, SCHEDULES_DIRECT_SUFFIX) {
        out.push_str(&rest[..at]);
        rest = &rest[end..];
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
    fn ordinal_folding_matches_dotnet_not_just_ascii() {
        use super::{eq_ordinal_ignore_case, ordinal_ignore_case_key};
        // `StringComparison.OrdinalIgnoreCase` folds the WHOLE ordinal range,
        // not the ASCII half: these pairs are equal on Jellyfin, and
        // `eq_ignore_ascii_case` misses every one of them.
        assert!(eq_ordinal_ignore_case("Kanal Ö", "kanal ö"));
        assert!(eq_ordinal_ignore_case("ČT sport", "čt SPORT"));
        assert!(eq_ordinal_ignore_case("ТВ Центр", "тв центр"));
        assert!(
            !"Kanal Ö".eq_ignore_ascii_case("kanal ö"),
            "the ASCII fold misses it"
        );
        // Still ordinal, never linguistic: `char.ToUpperInvariant` is a 1:1
        // table, so `ß` does not become `SS` and the two stay different.
        assert!(!eq_ordinal_ignore_case("straße", "STRASSE"));
        assert!(eq_ordinal_ignore_case("straße", "STRAßE"));
        // Different strings stay different, and the key agrees with the compare.
        assert!(!eq_ordinal_ignore_case("parity1", "parity2"));
        assert_eq!(ordinal_ignore_case_key("Ö1"), ordinal_ignore_case_key("ö1"));
        assert_ne!(ordinal_ignore_case_key("Ö1"), ordinal_ignore_case_key("O1"));
    }

    #[test]
    fn a_non_ascii_channel_name_and_id_still_match_the_guide() {
        // The regression this closes: an accented tuner channel matched on
        // Jellyfin and missed on Ferrofin, so its airings vanished from the
        // guide with no error anywhere.
        let epg = EpgChannelData::new(&[epg("ÖRF.at", "Kanal Ö", "1")]);
        assert_eq!(epg.by_id("örf.at").map(|c| c.id.as_str()), Some("ÖRF.at"));
        let tuner = TunerChannel {
            id: String::new(),
            tuner_channel_id: String::new(),
            number: String::new(),
            name: "kanal-ö".to_owned(),
            tuner_host_id: "t1".to_owned(),
        };
        assert_eq!(
            epg_channel_for_tuner_channel(&[], &tuner, &epg).map(|c| c.name.as_str()),
            Some("Kanal Ö"),
        );
        // …and so does a mapping keyed on a non-ASCII tuner channel id.
        let mapped = TunerChannel {
            id: "Ö-1".to_owned(),
            ..tuner.clone()
        };
        assert_eq!(
            epg_channel_for_tuner_channel(&[pair("ö-1", "örf.at")], &mapped, &epg)
                .map(|c| c.id.as_str()),
            Some("ÖRF.at"),
        );
        // Provider-to-tuner enablement is the same comparison.
        assert!(is_listing_provider_enabled_for_tuner(
            &ListingsProviderInfo {
                enable_all_tuners: false,
                enabled_tuners: vec!["TÜNER-1".to_owned()],
                ..ListingsProviderInfo::default()
            },
            "tüner-1",
        ));
    }

    #[test]
    fn the_schedules_direct_suffix_is_stripped_case_insensitively_around_non_ascii() {
        // `Replace(…, OrdinalIgnoreCase)` keeps the text around the match
        // verbatim — including chars whose folded form is a different byte
        // length, which is why the search reports haystack offsets.
        assert_eq!(strip_schedules_direct("IÖ1.JSON.SchedulesDirect.ORG"), "Ö1");
        assert_eq!(strip_schedules_direct("Ö1"), "Ö1");
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
