//! Library-selected audio tag interpretation from `AudioFileProber`.
//!
//! The encoder's normalizer supplies a general ffprobe interpretation, while
//! the audio prober uses the owning library's artist and delimiter settings.

use std::collections::{HashMap, HashSet};

use ferrofin_model::configuration::LibraryOptions;
use ferrofin_model::media_info::MediaInfo;
use ferrofin_providers::metadata_merge::is_valid_provider_id;
use ferrofin_util::string_extensions::{remove_diacritics, upper_invariant};

const INTERNAL_VALUE_SEPARATOR: char = '\u{001f}';

/// The library-specific tag values that the scanner stores on an audio item.
#[derive(Debug)]
pub(super) struct AudioTagValues {
    pub(super) artists: Vec<String>,
    pub(super) album_artists: Vec<String>,
    pub(super) genres: Vec<String>,
    pub(super) provider_ids: Vec<(String, String)>,
}

/// Interprets unsplit embedded tags using the library's current settings.
#[allow(clippy::too_many_lines)] // One AudioFileProber selection across its related tag fields.
pub(super) fn for_library(info: &MediaInfo, options: Option<&LibraryOptions>) -> AudioTagValues {
    let Some(raw) = info.raw_audio_tags.as_ref() else {
        // Encoders supplied through the seam may already provide interpreted
        // values without raw tags. Preserve their existing contract.
        return AudioTagValues {
            artists: info.artists.clone(),
            album_artists: if info.album_artists.is_empty() {
                info.artists.clone()
            } else {
                info.album_artists.clone()
            },
            genres: info.genres.clone(),
            provider_ids: info
                .provider_ids
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        };
    };
    let prefer_nonstandard = options.is_some_and(|o| o.prefer_nonstandard_artists_tag);
    let use_custom = options.is_some_and(|o| o.use_custom_tag_delimiters);
    let delimiters = options.map_or_else(Vec::new, |o| custom_delimiters(&o.custom_tag_delimiters));
    let whitelist = options.map_or(&[][..], |o| o.delimiter_whitelist.as_slice());
    let path = info.media_source.path.as_deref().unwrap_or_default();
    let selected = |standard: &[&str], nonstandard: &[&str]| {
        let preferred = prefer_nonstandard
            .then(|| read_tag(raw, nonstandard, path))
            .flatten();
        let tag = preferred.or_else(|| read_tag(raw, standard, path));
        let values: Vec<_> = tag
            .map(|tag| {
                tag.split(INTERNAL_VALUE_SEPARATOR)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if use_custom {
            values
                .iter()
                .flat_map(|value| split_custom(value, &delimiters, whitelist))
                .collect()
        } else {
            values
        }
    };
    let artists = selected(&["artist"], &["artists", "track.artists"]);
    let album_artists = selected(
        &["albumartist", "album artist", "album_artist"],
        &["albumartists", "track.albumartists"],
    );
    // Upstream applies this fallback after custom splitting, before dropping
    // blank names for storage. An explicit blank multivalue is not absence.
    let album_artists = if album_artists.is_empty() {
        artists.clone()
    } else {
        album_artists
    };
    let genres: Vec<String> = read_tag(raw, &["genre"], path)
        .map(|tag| {
            let values = tag.split(INTERNAL_VALUE_SEPARATOR);
            if use_custom {
                values
                    .flat_map(|value| split_custom(value, &delimiters, whitelist))
                    .collect()
            } else {
                values.map(str::to_owned).collect()
            }
        })
        .unwrap_or_default();
    let genres = distinct_case(
        genres
            .into_iter()
            .map(|genre: String| genre.trim().to_owned()),
    );
    let mut provider_ids = info.provider_ids.clone();
    for (provider, keys) in MUSICBRAINZ_TAGS {
        // A present-but-empty field blocks the alternate spelling, as the
        // upstream TryGetAdditionalField(...) || TryGetAdditionalField(...) does.
        if keys.iter().any(|key| raw.contains_key(*key)) {
            provider_ids.remove(*provider);
            if let Some(tag) = read_tag(raw, keys, path) {
                let id = if *provider == "MusicBrainzRecording" {
                    Some(tag.to_owned())
                } else {
                    let first = tag
                        .split(INTERNAL_VALUE_SEPARATOR)
                        .next()
                        .unwrap_or_default();
                    if use_custom {
                        split_custom(first, &delimiters, whitelist)
                            .into_iter()
                            .next()
                    } else {
                        Some(first.to_owned())
                    }
                };
                if let Some(id) = id.map(|id| id.trim().to_owned())
                    && is_valid_provider_id(provider, &id)
                {
                    provider_ids.insert((*provider).to_owned(), id);
                }
            }
        }
    }
    AudioTagValues {
        // BaseItemMapper stores these two arrays distinct by ordinal case.
        artists: distinct_case(artists),
        album_artists: distinct_case(album_artists),
        genres,
        provider_ids: provider_ids.into_iter().collect(),
    }
}

/// First present tag, sanitized as `AudioFileProber.GetSanitizedStringTag`.
fn read_tag<'a>(raw: &'a HashMap<String, String>, keys: &[&str], path: &str) -> Option<&'a str> {
    let value = keys.iter().find_map(|key| raw.get(*key))?;
    if value.is_empty() {
        return None;
    }
    if let Some((prefix, _)) = value.split_once('\0') {
        tracing::warn!(
            path,
            "audio tag contains a null character; discarding the remaining characters"
        );
        Some(prefix)
    } else {
        Some(value)
    }
}

/// `LibraryOptionsExtension.GetCustomTagDelimiters`: only one UTF-16 char,
/// ignoring empty/multiple-character entries and always including NUL.
fn custom_delimiters(values: &[String]) -> Vec<char> {
    let mut delimiters: Vec<_> = values
        .iter()
        .filter_map(|value| {
            let mut chars = value.chars();
            let character = chars.next()?;
            (chars.next().is_none() && character.len_utf16() == 1).then_some(character)
        })
        .collect();
    delimiters.push('\0');
    delimiters
}

/// The whitelist comes first and keeps its configured spelling. Only the
/// remaining pieces use `DistinctNames` (diacritics and ordinal case folded).
fn split_custom(value: &str, delimiters: &[char], whitelist: &[String]) -> Vec<String> {
    let mut names = Vec::new();
    let mut remaining = value.to_owned();
    for name in whitelist.iter().filter(|name| !name.trim().is_empty()) {
        let (without, found) = remove_ignore_case(&remaining, name);
        remaining = without;
        if found {
            names.push(name.clone());
        }
    }
    let mut seen = HashSet::new();
    names.extend(
        remaining
            .split(delimiters)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .filter(|name| seen.insert(upper_invariant(&remove_diacritics(name))))
            .map(str::to_owned),
    );
    names
}

/// Case-insensitive replacement using .NET's simple uppercase mapping, so
/// replacement boundaries remain valid for non-ASCII artist names.
fn remove_ignore_case(value: &str, name: &str) -> (String, bool) {
    let original: Vec<char> = value.chars().collect();
    let upper: Vec<char> = upper_invariant(value).chars().collect();
    let target: Vec<char> = upper_invariant(name).chars().collect();
    if target.is_empty() {
        return (value.to_owned(), false);
    }
    let mut result = String::with_capacity(value.len());
    let mut cursor = 0;
    let mut found = false;
    while cursor < upper.len() {
        if upper[cursor..].starts_with(&target) {
            cursor += target.len();
            found = true;
        } else {
            result.push(original[cursor]);
            cursor += 1;
        }
    }
    (result, found)
}

fn distinct_case(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .filter(|value| seen.insert(upper_invariant(value)))
        .collect()
}

/// Normal additional-field names followed by each spelling's MKA fallback.
const MUSICBRAINZ_TAGS: &[(&str, &[&str])] = &[
    (
        "MusicBrainzArtist",
        &[
            "musicbrainz_artistid",
            "track.musicbrainz_artistid",
            "musicbrainz artist id",
            "track.musicbrainz_artist_id",
        ],
    ),
    (
        "MusicBrainzAlbumArtist",
        &[
            "musicbrainz_albumartistid",
            "track.musicbrainz_albumartistid",
            "musicbrainz album artist id",
            "track.musicbrainz_album_artist_id",
        ],
    ),
    (
        "MusicBrainzAlbum",
        &[
            "musicbrainz_albumid",
            "track.musicbrainz_albumid",
            "musicbrainz album id",
            "track.musicbrainz_album_id",
        ],
    ),
    (
        "MusicBrainzReleaseGroup",
        &[
            "musicbrainz_releasegroupid",
            "track.musicbrainz_releasegroupid",
            "musicbrainz release group id",
            "track.musicbrainz_release_group_id",
        ],
    ),
    (
        "MusicBrainzTrack",
        &[
            "musicbrainz_releasetrackid",
            "track.musicbrainz_releasetrackid",
            "musicbrainz release track id",
            "track.musicbrainz_release_track_id",
        ],
    ),
    (
        "MusicBrainzRecording",
        &[
            "musicbrainz_trackid",
            "track.musicbrainz_trackid",
            "musicbrainz track id",
            "track.musicbrainz_track_id",
        ],
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST_ID: &str = "11111111-1111-4111-8111-111111111111";
    const SECOND_ID: &str = "22222222-2222-4222-8222-222222222222";

    fn info(tags: &[(&str, &str)]) -> MediaInfo {
        MediaInfo {
            raw_audio_tags: Some(
                tags.iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn default_uses_standard_tags_without_implicit_name_or_genre_splitting() {
        let info = info(&[
            ("artist", "Standard / Partner; Guest"),
            ("artists", "Nonstandard"),
            ("albumartist", "Album / Collective"),
            ("albumartists", "Nonstandard album"),
            ("genre", "Rock / Experimental; Dance"),
        ]);
        let tags = for_library(&info, None);
        assert_eq!(tags.artists, ["Standard / Partner; Guest"]);
        assert_eq!(tags.album_artists, ["Album / Collective"]);
        assert_eq!(tags.genres, ["Rock / Experimental; Dance"]);
        let tags = for_library(
            &info,
            Some(&LibraryOptions {
                prefer_nonstandard_artists_tag: true,
                ..Default::default()
            }),
        );
        assert_eq!(tags.artists, ["Nonstandard"]);
        assert_eq!(tags.album_artists, ["Nonstandard album"]);
    }

    #[test]
    fn preferred_tags_fall_back_only_when_absent_or_empty_before_sanitizing() {
        let options = LibraryOptions {
            prefer_nonstandard_artists_tag: true,
            ..Default::default()
        };
        let tags = for_library(
            &info(&[("artist", "Standard"), ("artists", "")]),
            Some(&options),
        );
        assert_eq!(tags.artists, ["Standard"]);
        let tags = for_library(
            &info(&[("artist", "Standard"), ("artists", "\0Hidden")]),
            Some(&options),
        );
        assert!(
            tags.artists.is_empty(),
            "present tag sanitizing to empty suppresses standard fallback"
        );
        let tags = for_library(
            &info(&[("artist", "Standard"), ("artists", " ")]),
            Some(&options),
        );
        assert!(tags.artists.is_empty());
        let tags = for_library(
            &info(&[
                ("track.artists", "First\u{001f}Second"),
                ("track.albumartists", "Album"),
            ]),
            Some(&options),
        );
        assert_eq!(tags.artists, ["First", "Second"]);
        assert_eq!(tags.album_artists, ["Album"]);
        assert!(
            for_library(&info(&[("artists", "Ignored")]), None)
                .artists
                .is_empty()
        );
    }

    #[test]
    fn custom_delimiters_and_whitelist_apply_to_artist_album_artist_and_genre() {
        let options = LibraryOptions {
            prefer_nonstandard_artists_tag: true,
            use_custom_tag_delimiters: true,
            custom_tag_delimiters: vec!["/".to_owned(), ";".to_owned()],
            delimiter_whitelist: vec!["ac/dc".to_owned(), "Éclair/Anne".to_owned(), " ".to_owned()],
            ..Default::default()
        };
        let info = info(&[
            (
                "artists",
                "Guest; AC/DC; ÉCLAIR/ANNE; Guest;GUEST; Béla; Bela",
            ),
            ("albumartists", "AC/DC; Album guest"),
            ("genre", "Noise/Pop; AC/DC;noise;POP"),
        ]);
        let tags = for_library(&info, Some(&options));
        assert_eq!(tags.artists, ["ac/dc", "Éclair/Anne", "Guest", "Béla"]);
        assert_eq!(tags.album_artists, ["ac/dc", "Album guest"]);
        assert_eq!(tags.genres, ["ac/dc", "Noise", "Pop"]);
        let tags = for_library(
            &info,
            Some(&LibraryOptions {
                delimiter_whitelist: vec![],
                ..options
            }),
        );
        assert_eq!(tags.album_artists, ["AC", "DC", "Album guest"]);
    }

    #[test]
    fn delimiter_strings_are_single_utf16_characters_and_empty_selection_still_uses_whitelist() {
        let options = LibraryOptions {
            use_custom_tag_delimiters: true,
            custom_tag_delimiters: vec![
                String::new(),
                ";;".to_owned(),
                "🙂".to_owned(),
                "、".to_owned(),
            ],
            ..Default::default()
        };
        let tags = for_library(
            &info(&[("artist", "First、Second🙂Third;;Fourth")]),
            Some(&options),
        );
        assert_eq!(tags.artists, ["First", "Second🙂Third;;Fourth"]);
        let tags = for_library(
            &info(&[("artist", "AC/DC & Partner")]),
            Some(&LibraryOptions {
                custom_tag_delimiters: vec![],
                delimiter_whitelist: vec!["AC/DC".to_owned()],
                ..options
            }),
        );
        assert_eq!(tags.artists, ["AC/DC", "& Partner"]);
    }

    #[test]
    fn multivalues_nulls_and_per_value_distinct_names_follow_audio_prober_order() {
        let tags = for_library(
            &info(&[
                ("artist", "First\u{001f}Second\u{001f}FIRST\0Discarded"),
                ("genre", " Rock \u{001f}rock\u{001f}Fusion"),
            ]),
            None,
        );
        assert_eq!(tags.artists, ["First", "Second"]);
        assert_eq!(tags.album_artists, tags.artists);
        assert_eq!(tags.genres, ["Rock", "Fusion"]);
        let tags = for_library(
            &info(&[
                ("artist", "Béla; Bela\u{001f}Bela"),
                ("albumartist", "\0Empty"),
            ]),
            Some(&LibraryOptions {
                use_custom_tag_delimiters: true,
                custom_tag_delimiters: vec![";".to_owned()],
                ..Default::default()
            }),
        );
        assert_eq!(
            tags.artists,
            ["Béla", "Bela"],
            "DistinctNames is scoped to each original tag value"
        );
        assert_eq!(
            tags.album_artists, tags.artists,
            "custom splitting makes sanitized-empty album artist absent"
        );
        let tags = for_library(
            &info(&[("artist", "Artist"), ("albumartist", "\0Empty")]),
            None,
        );
        assert!(
            tags.album_artists.is_empty(),
            "explicit empty multivalue suppresses fallback before filtering"
        );
    }

    #[test]
    fn musicbrainz_ids_split_only_when_configured_and_recording_stays_unsplit() {
        let composite = format!("{FIRST_ID};{SECOND_ID}");
        let mut input = info(&[
            ("musicbrainz_albumid", &composite),
            ("musicbrainz_trackid", &composite),
        ]);
        input
            .provider_ids
            .insert("MusicBrainzAlbum".to_owned(), FIRST_ID.to_owned());
        input
            .provider_ids
            .insert("MusicBrainzRecording".to_owned(), FIRST_ID.to_owned());
        input
            .provider_ids
            .insert("Other".to_owned(), "kept".to_owned());
        let ids: HashMap<_, _> = for_library(&input, None).provider_ids.into_iter().collect();
        assert_eq!(
            ids,
            HashMap::from([("Other".to_owned(), "kept".to_owned())])
        );
        let options = LibraryOptions {
            use_custom_tag_delimiters: true,
            custom_tag_delimiters: vec![";".to_owned()],
            ..Default::default()
        };
        let ids: HashMap<_, _> = for_library(&input, Some(&options))
            .provider_ids
            .into_iter()
            .collect();
        assert_eq!(
            ids.get("MusicBrainzAlbum").map(String::as_str),
            Some(FIRST_ID)
        );
        assert!(!ids.contains_key("MusicBrainzRecording"));

        let multivalue = format!("{FIRST_ID}\u{001f}{SECOND_ID}");
        let tags = for_library(
            &info(&[
                ("musicbrainz_artistid", &multivalue),
                ("musicbrainz_trackid", FIRST_ID),
            ]),
            None,
        );
        let ids: HashMap<_, _> = tags.provider_ids.into_iter().collect();
        assert_eq!(
            ids.get("MusicBrainzArtist").map(String::as_str),
            Some(FIRST_ID)
        );
        assert_eq!(
            ids.get("MusicBrainzRecording").map(String::as_str),
            Some(FIRST_ID)
        );
        let tags = for_library(
            &info(&[
                ("musicbrainz_artistid", FIRST_ID),
                ("musicbrainz_trackid", FIRST_ID),
            ]),
            Some(&LibraryOptions {
                custom_tag_delimiters: vec!["-".to_owned()],
                ..options
            }),
        );
        assert_eq!(
            tags.provider_ids,
            [("MusicBrainzRecording".to_owned(), FIRST_ID.to_owned())]
        );
    }

    #[test]
    fn musicbrainz_additional_field_priority_and_mka_fallback_are_source_spelled() {
        let input = info(&[
            ("track.musicbrainz_artistid", FIRST_ID),
            ("track.musicbrainz_album_artist_id", SECOND_ID),
            ("musicbrainz album id", FIRST_ID),
            ("musicbrainz_releasegroupid", ""),
            ("musicbrainz release group id", FIRST_ID),
            ("track.musicbrainz_release_track_id", SECOND_ID),
            ("track.musicbrainz_trackid", FIRST_ID),
        ]);
        let ids: HashMap<_, _> = for_library(&input, None).provider_ids.into_iter().collect();
        for provider in [
            "MusicBrainzArtist",
            "MusicBrainzAlbum",
            "MusicBrainzRecording",
        ] {
            assert_eq!(ids.get(provider).map(String::as_str), Some(FIRST_ID));
        }
        for provider in ["MusicBrainzAlbumArtist", "MusicBrainzTrack"] {
            assert_eq!(ids.get(provider).map(String::as_str), Some(SECOND_ID));
        }
        assert!(
            !ids.contains_key("MusicBrainzReleaseGroup"),
            "empty primary field blocks alternate spelling"
        );
    }

    #[test]
    fn encoder_seams_without_raw_tags_preserve_interpreted_metadata_and_album_fallback() {
        let input = MediaInfo {
            artists: vec!["Already split".to_owned(), "Partner".to_owned()],
            genres: vec!["Genre".to_owned()],
            provider_ids: HashMap::from([("Provider".to_owned(), "value".to_owned())]),
            ..Default::default()
        };
        let tags = for_library(
            &input,
            Some(&LibraryOptions {
                use_custom_tag_delimiters: true,
                ..Default::default()
            }),
        );
        assert_eq!(tags.artists, input.artists);
        assert_eq!(tags.album_artists, input.artists);
        assert_eq!(tags.genres, input.genres);
        assert_eq!(
            tags.provider_ids,
            [("Provider".to_owned(), "value".to_owned())]
        );
    }
}
