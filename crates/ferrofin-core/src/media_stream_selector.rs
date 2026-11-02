//! Port of `Emby.Server.Implementations.Library.MediaStreamSelector`.
//!
//! Pure functions for choosing default audio/subtitle streams based on user
//! preferences, matching the upstream C# logic exactly.

use ferrofin_model::configuration::SubtitlePlaybackMode;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaStream;

/// Picks the default audio stream index.
///
/// Port of `MediaStreamSelector.GetDefaultAudioStreamIndex`.
pub(crate) fn get_default_audio_stream_index(
    streams: &[MediaStream],
    preferred_languages: &[String],
    prefer_default_track: bool,
) -> Option<i32> {
    let mut sorted: Vec<&MediaStream> = streams
        .iter()
        .filter(|s| s.stream_type == MediaStreamType::Audio)
        .collect();
    sorted.sort_by(|a, b| {
        get_stream_score(b, preferred_languages).cmp(&get_stream_score(a, preferred_languages))
    });

    if prefer_default_track && let Some(default) = sorted.iter().find(|s| s.is_default) {
        return Some(default.index);
    }

    sorted.first().map(|s| s.index)
}

/// Picks the default subtitle stream index.
///
/// Port of `MediaStreamSelector.GetDefaultSubtitleStreamIndex`.
#[allow(clippy::too_many_lines)]
pub(crate) fn get_default_subtitle_stream_index(
    streams: &[MediaStream],
    preferred_languages: &[String],
    mode: SubtitlePlaybackMode,
    audio_track_language: Option<&str>,
) -> Option<i32> {
    if mode == SubtitlePlaybackMode::None {
        return None;
    }

    let mut sorted: Vec<&MediaStream> = streams
        .iter()
        .filter(|s| s.stream_type == MediaStreamType::Subtitle)
        .collect();

    sorted.sort_by(|a, b| {
        fn key(s: &MediaStream, prefs: &[String]) -> (bool, bool, bool, bool, bool, bool) {
            (
                s.is_external,
                s.is_default,
                !s.is_forced && matches_preferred_language(s.language.as_deref(), prefs),
                s.is_forced && matches_preferred_language(s.language.as_deref(), prefs),
                s.is_forced && is_language_undefined(s.language.as_deref()),
                s.is_forced,
            )
        }
        key(b, preferred_languages).cmp(&key(a, preferred_languages))
    });

    match mode {
        SubtitlePlaybackMode::Default => sorted
            .iter()
            .find(|s| s.is_external || s.is_default || s.is_forced)
            .map(|s| s.index),
        SubtitlePlaybackMode::Smart => {
            let audio_lang = audio_track_language.unwrap_or("");
            if preferred_languages
                .iter()
                .any(|p| p.eq_ignore_ascii_case(audio_lang))
            {
                behavior_only_forced(&sorted, preferred_languages)
                    .first()
                    .map(|s| s.index)
            } else {
                sorted
                    .iter()
                    .find(|s| {
                        matches_preferred_language(s.language.as_deref(), preferred_languages)
                    })
                    .map(|s| s.index)
            }
        }
        SubtitlePlaybackMode::Always => sorted
            .iter()
            .find(|s| {
                !s.is_forced
                    && matches_preferred_language(s.language.as_deref(), preferred_languages)
            })
            .map(|s| s.index)
            .or_else(|| {
                behavior_only_forced(&sorted, preferred_languages)
                    .first()
                    .map(|s| s.index)
            }),
        SubtitlePlaybackMode::OnlyForced => behavior_only_forced(&sorted, preferred_languages)
            .first()
            .map(|s| s.index),
        _ => None,
    }
}

/// Sets scores on subtitle streams for client-side ranking.
///
/// Port of `MediaStreamSelector.SetSubtitleStreamScores`.
pub(crate) fn set_subtitle_stream_scores(
    streams: &mut [MediaStream],
    preferred_languages: &[String],
    mode: SubtitlePlaybackMode,
    audio_track_language: Option<&str>,
) {
    if mode == SubtitlePlaybackMode::None {
        return;
    }

    let mut sorted: Vec<&MediaStream> = streams
        .iter()
        .filter(|s| s.stream_type == MediaStreamType::Subtitle)
        .collect();
    sorted.sort_by(|a, b| {
        get_stream_score(b, preferred_languages).cmp(&get_stream_score(a, preferred_languages))
    });

    let indices: Vec<i32> = match mode {
        SubtitlePlaybackMode::Default => sorted
            .iter()
            .filter(|s| s.is_external || s.is_default || s.is_forced)
            .map(|s| s.index)
            .collect(),
        SubtitlePlaybackMode::Smart => {
            let audio_lang = audio_track_language.unwrap_or("");
            if preferred_languages
                .iter()
                .any(|p| p.eq_ignore_ascii_case(audio_lang))
            {
                behavior_only_forced(&sorted, preferred_languages)
                    .iter()
                    .map(|s| s.index)
                    .collect()
            } else {
                sorted
                    .iter()
                    .filter(|s| {
                        matches_preferred_language(s.language.as_deref(), preferred_languages)
                    })
                    .map(|s| s.index)
                    .collect()
            }
        }
        SubtitlePlaybackMode::Always => {
            let full: Vec<i32> = sorted
                .iter()
                .filter(|s| {
                    !s.is_forced
                        && matches_preferred_language(s.language.as_deref(), preferred_languages)
                })
                .map(|s| s.index)
                .collect();
            if full.is_empty() {
                behavior_only_forced(&sorted, preferred_languages)
                    .iter()
                    .map(|s| s.index)
                    .collect()
            } else {
                full
            }
        }
        SubtitlePlaybackMode::OnlyForced => behavior_only_forced(&sorted, preferred_languages)
            .iter()
            .map(|s| s.index)
            .collect(),
        _ => Vec::new(),
    };

    for stream in streams.iter_mut() {
        if indices.contains(&stream.index) {
            stream.score = Some(get_stream_score(stream, preferred_languages));
        }
    }
}

fn matches_preferred_language(language: Option<&str>, preferred: &[String]) -> bool {
    preferred.is_empty()
        || language.is_some_and(|lang| preferred.iter().any(|p| p.eq_ignore_ascii_case(lang)))
}

fn is_language_undefined(language: Option<&str>) -> bool {
    match language {
        None | Some("") => true,
        Some(l) => {
            l.eq_ignore_ascii_case("und")
                || l.eq_ignore_ascii_case("unknown")
                || l.eq_ignore_ascii_case("undetermined")
                || l.eq_ignore_ascii_case("mul")
                || l.eq_ignore_ascii_case("zxx")
        }
    }
}

fn behavior_only_forced<'a>(
    sorted: &[&'a MediaStream],
    preferred: &[String],
) -> Vec<&'a MediaStream> {
    let mut result: Vec<&MediaStream> = sorted
        .iter()
        .filter(|s| {
            s.is_forced
                && (matches_preferred_language(s.language.as_deref(), preferred)
                    || is_language_undefined(s.language.as_deref()))
        })
        .copied()
        .collect();
    result.sort_by(|a, b| {
        let a_pref = matches_preferred_language(a.language.as_deref(), preferred);
        let b_pref = matches_preferred_language(b.language.as_deref(), preferred);
        let a_und = is_language_undefined(a.language.as_deref());
        let b_und = is_language_undefined(b.language.as_deref());
        (b_pref, b_und).cmp(&(a_pref, a_und))
    });
    result
}

/// Scores a stream for language-preference ranking.
///
/// Port of `MediaStreamSelector.GetStreamScore`.
pub(crate) fn get_stream_score(stream: &MediaStream, language_preferences: &[String]) -> i32 {
    let index = stream.language.as_deref().and_then(|lang| {
        language_preferences
            .iter()
            .position(|p| p.eq_ignore_ascii_case(lang))
    });
    let mut score = match index {
        Some(i) => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let s = 101 - i as i32;
            s
        }
        None => 1,
    };
    score = score * 10 + if stream.is_forced { 2 } else { 1 };
    score = score * 10 + if stream.is_default { 2 } else { 1 };
    score = score * 10
        + if stream.supports_external_stream {
            2
        } else {
            1
        };
    score = score * 10
        + if stream.is_text_subtitle_stream.unwrap_or(false) {
            2
        } else {
            1
        };
    score = score * 10 + if stream.is_external { 2 } else { 1 };
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(index: i32, language: &str, is_default: bool, is_original: bool) -> MediaStream {
        MediaStream {
            index,
            stream_type: MediaStreamType::Audio,
            language: Some(language.to_owned()),
            is_default,
            is_original,
            ..MediaStream::default()
        }
    }

    fn subtitle(
        index: i32,
        language: &str,
        is_default: bool,
        is_forced: bool,
        is_external: bool,
    ) -> MediaStream {
        MediaStream {
            index,
            stream_type: MediaStreamType::Subtitle,
            language: if language.is_empty() {
                None
            } else {
                Some(language.to_owned())
            },
            is_default,
            is_forced,
            is_external,
            ..MediaStream::default()
        }
    }

    fn video() -> MediaStream {
        MediaStream {
            index: 0,
            stream_type: MediaStreamType::Video,
            is_default: true,
            ..MediaStream::default()
        }
    }

    // -- GetDefaultAudioStreamIndex tests (from MediaStreamSelectorTests.cs) --

    #[test]
    fn empty_streams_returns_none_for_audio() {
        assert_eq!(get_default_audio_stream_index(&[], &[], true), None);
        assert_eq!(get_default_audio_stream_index(&[], &[], false), None);
    }

    use rstest::rstest;

    #[rstest]
    #[case(&[], false, 1)]
    #[case(&[], true, 1)]
    #[case(&["eng"], false, 2)]
    #[case(&["eng"], true, 1)]
    #[case(&["eng", "fre"], false, 2)]
    #[case(&["fre", "eng"], false, 1)]
    #[case(&["eng", "fre"], true, 1)]
    fn audio_preferred_language(
        #[case] prefs: &[&str],
        #[case] prefer_default: bool,
        #[case] expected: i32,
    ) {
        let streams = vec![
            video(),
            MediaStream {
                index: 1,
                stream_type: MediaStreamType::Audio,
                language: Some("fre".to_owned()),
                is_default: true,
                ..MediaStream::default()
            },
            MediaStream {
                index: 2,
                stream_type: MediaStreamType::Audio,
                language: Some("eng".to_owned()),
                is_default: false,
                ..MediaStream::default()
            },
        ];
        let prefs: Vec<String> = prefs.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(
            get_default_audio_stream_index(&streams, &prefs, prefer_default),
            Some(expected)
        );
    }

    // -- GetStreamScore tests (from MediaStreamSelectorTests.cs) --

    #[rstest]
    #[case(MediaStream::default(), 111_111)]
    #[case(MediaStream { language: Some("eng".to_owned()), ..MediaStream::default() }, 10_111_111)]
    #[case(MediaStream { language: Some("fre".to_owned()), ..MediaStream::default() }, 10_011_111)]
    #[case(MediaStream { is_forced: true, ..MediaStream::default() }, 121_111)]
    #[case(MediaStream { is_default: true, ..MediaStream::default() }, 112_111)]
    #[case(MediaStream { supports_external_stream: true, ..MediaStream::default() }, 111_211)]
    #[case(MediaStream { is_external: true, ..MediaStream::default() }, 111_112)]
    #[case(MediaStream {
        language: Some("eng".to_owned()),
        is_forced: true,
        is_default: true,
        supports_external_stream: true,
        is_external: true,
        ..MediaStream::default()
    }, 10_122_212)]
    fn stream_score(#[case] stream: MediaStream, #[case] expected: i32) {
        let prefs = vec!["eng".to_owned(), "fre".to_owned()];
        assert_eq!(get_stream_score(&stream, &prefs), expected);
    }

    // -- Subtitle selection tests --

    #[test]
    fn subtitle_mode_none_returns_none() {
        let streams = vec![subtitle(1, "eng", true, false, false)];
        assert_eq!(
            get_default_subtitle_stream_index(&streams, &[], SubtitlePlaybackMode::None, None),
            None
        );
    }

    #[test]
    fn subtitle_mode_default_picks_default_or_forced() {
        let streams = vec![
            video(),
            audio(1, "eng", true, false),
            subtitle(2, "eng", false, false, false),
            subtitle(3, "eng", true, false, false),
        ];
        assert_eq!(
            get_default_subtitle_stream_index(
                &streams,
                &["eng".to_owned()],
                SubtitlePlaybackMode::Default,
                Some("eng"),
            ),
            Some(3)
        );
    }

    #[test]
    fn subtitle_mode_smart_prefers_sub_when_audio_differs() {
        let streams = vec![
            audio(0, "jpn", true, false),
            subtitle(1, "eng", false, false, false),
        ];
        assert_eq!(
            get_default_subtitle_stream_index(
                &streams,
                &["eng".to_owned()],
                SubtitlePlaybackMode::Smart,
                Some("jpn"),
            ),
            Some(1)
        );
    }

    #[test]
    fn subtitle_mode_smart_falls_to_forced_when_audio_matches() {
        let streams = vec![
            audio(0, "eng", true, false),
            subtitle(1, "eng", false, false, false),
            subtitle(2, "eng", false, true, false),
        ];
        assert_eq!(
            get_default_subtitle_stream_index(
                &streams,
                &["eng".to_owned()],
                SubtitlePlaybackMode::Smart,
                Some("eng"),
            ),
            Some(2)
        );
    }

    #[test]
    fn subtitle_mode_only_forced_picks_forced_sub() {
        let streams = vec![
            audio(0, "eng", true, false),
            subtitle(1, "eng", false, false, false),
            subtitle(2, "eng", false, true, false),
        ];
        assert_eq!(
            get_default_subtitle_stream_index(
                &streams,
                &["eng".to_owned()],
                SubtitlePlaybackMode::OnlyForced,
                Some("eng"),
            ),
            Some(2)
        );
    }

    #[test]
    fn subtitle_mode_always_prefers_non_forced() {
        let streams = vec![
            audio(0, "eng", true, false),
            subtitle(1, "eng", false, true, false),
            subtitle(2, "eng", false, false, false),
        ];
        assert_eq!(
            get_default_subtitle_stream_index(
                &streams,
                &["eng".to_owned()],
                SubtitlePlaybackMode::Always,
                Some("eng"),
            ),
            Some(2)
        );
    }

    #[test]
    fn is_language_undefined_recognizes_placeholders() {
        assert!(is_language_undefined(None));
        assert!(is_language_undefined(Some("")));
        assert!(is_language_undefined(Some("und")));
        assert!(is_language_undefined(Some("UND")));
        assert!(is_language_undefined(Some("unknown")));
        assert!(is_language_undefined(Some("undetermined")));
        assert!(is_language_undefined(Some("mul")));
        assert!(is_language_undefined(Some("zxx")));
        assert!(!is_language_undefined(Some("eng")));
    }

    #[test]
    fn only_forced_picks_undefined_language_forced_subs() {
        let streams = [subtitle(1, "", false, true, false)];
        let refs: Vec<&MediaStream> = streams.iter().collect();
        let result = behavior_only_forced(&refs, &["eng".to_owned()]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].index, 1);
    }
}
