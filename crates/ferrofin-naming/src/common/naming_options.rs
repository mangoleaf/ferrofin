//! Port of `Emby.Naming.Common.NamingOptions`.
//!
//! The regex and configuration tables here are copied **byte-for-byte** from
//! the C# source so the parsers behave identically to Jellyfin. Do not
//! "tidy" these strings.

use std::collections::HashMap;

use ferrofin_model::entities::ExtraType;

use crate::common::{EpisodeExpression, GuardedRegex, MediaType};
use crate::video::{ExtraRule, ExtraRuleType, FileStackRule, Format3DRule, StubTypeRule};

/// Big collection of naming options driving every parser in the crate.
///
/// Ported from Jellyfin's `NamingOptions`; the doc comment there notes it
/// "should be split and injected instead of passed everywhere", but we keep the
/// shape for parity.
#[derive(Debug, Clone)]
pub struct NamingOptions {
    /// Folder name to extra types mapping (case-insensitive keys, lowercased).
    pub all_extras_types_folder_names: HashMap<String, ExtraType>,
    /// List of audio file extensions.
    pub audio_file_extensions: Vec<String>,
    /// List of external media flag delimiters.
    pub media_flag_delimiters: Vec<char>,
    /// List of external media forced flags.
    pub media_forced_flags: Vec<String>,
    /// List of external media default flags.
    pub media_default_flags: Vec<String>,
    /// List of external media hearing impaired flags.
    pub media_hearing_impaired_flags: Vec<String>,
    /// List of album stacking prefixes.
    pub album_stacking_prefixes: Vec<String>,
    /// List of artist subfolders.
    pub artist_subfolders: Vec<String>,
    /// List of subtitle file extensions.
    pub subtitle_file_extensions: Vec<String>,
    /// List of lyric file extensions.
    pub lyric_file_extensions: Vec<String>,
    /// List of episode regular expressions.
    pub episode_expressions: Vec<EpisodeExpression>,
    /// List of video file extensions.
    pub video_file_extensions: Vec<String>,
    /// List of video stub file extensions.
    pub stub_file_extensions: Vec<String>,
    /// List of raw audiobook parts regular expression strings.
    pub audio_book_parts_expressions: Vec<String>,
    /// List of raw audiobook names regular expression strings.
    pub audio_book_names_expressions: Vec<String>,
    /// List of stub type rules.
    pub stub_types: Vec<StubTypeRule>,
    /// List of video flag delimiters.
    pub video_flag_delimiters: Vec<char>,
    /// List of 3D format rules.
    pub format_3d_rules: Vec<Format3DRule>,
    /// The file stacking rules.
    pub video_file_stacking_rules: Vec<FileStackRule>,
    /// List of raw clean-`DateTime` regular expression strings.
    pub clean_date_times: Vec<String>,
    /// List of raw clean-string regular expression strings.
    pub clean_strings: Vec<String>,
    /// List of multi-episode regular expressions.
    pub multiple_episode_expressions: Vec<EpisodeExpression>,
    /// List of extra rules for videos.
    pub video_extra_rules: Vec<ExtraRule>,
    /// Compiled audiobook parts regexes.
    pub audio_book_parts_regexes: Vec<GuardedRegex>,
    /// Compiled audiobook names regexes.
    pub audio_book_names_regexes: Vec<GuardedRegex>,
    /// Compiled clean-`DateTime` regexes.
    pub clean_date_time_regexes: Vec<GuardedRegex>,
    /// Compiled clean-string regexes.
    pub clean_string_regexes: Vec<GuardedRegex>,
}

impl Default for NamingOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl NamingOptions {
    /// Creates the default naming options, compiling the clean regexes.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn new() -> Self {
        let video_file_extensions = str_vec(&[
            ".001", ".3g2", ".3gp", ".amv", ".asf", ".asx", ".avi", ".bin", ".bivx", ".divx",
            ".dv", ".dvr-ms", ".f4v", ".fli", ".flv", ".ifo", ".img", ".iso", ".m2t", ".m2ts",
            ".m2v", ".m4v", ".mkv", ".mk3d", ".mov", ".mp4", ".mpe", ".mpeg", ".mpg", ".mts",
            ".mxf", ".nrg", ".nsv", ".nuv", ".ogg", ".ogm", ".ogv", ".pva", ".qt", ".rec", ".rm",
            ".rmvb", ".strm", ".svq3", ".tp", ".ts", ".ty", ".viv", ".vob", ".vp3", ".webm",
            ".wmv", ".wtv", ".xvid",
        ]);

        let video_flag_delimiters = vec!['(', ')', '-', '.', '_', '[', ']'];

        let stub_file_extensions = str_vec(&[".disc"]);

        let stub_types = vec![
            StubTypeRule::new("dvd", "dvd"),
            StubTypeRule::new("hddvd", "hddvd"),
            StubTypeRule::new("bluray", "bluray"),
            StubTypeRule::new("brrip", "bluray"),
            StubTypeRule::new("bd25", "bluray"),
            StubTypeRule::new("bd50", "bluray"),
            StubTypeRule::new("vhs", "vhs"),
            StubTypeRule::new("HDTV", "tv"),
            StubTypeRule::new("PDTV", "tv"),
            StubTypeRule::new("DSR", "tv"),
        ];

        let video_file_stacking_rules = vec![
            FileStackRule::new(
                r"^(?<filename>.*?)(?:(?<=[\]\)\}])|[ _.-]+)[\(\[]?(?<parttype>cd|dvd|part|pt|dis[ck])[ _.-]*(?<number>[0-9]+)[\)\]]?(?:\.[^.]+)?$",
                true,
            ),
            FileStackRule::new(
                r"^(?<filename>.*?)(?:(?<=[\]\)\}])|[ _.-]+)[\(\[]?(?<parttype>cd|dvd|part|pt|dis[ck])[ _.-]*(?<number>[a-d])[\)\]]?(?:\.[^.]+)?$",
                false,
            ),
        ];

        let clean_date_times = str_vec(&[
            r"(.+[^_\,\.\(\)\[\]\-])[_\.\(\)\[\]\-](19[0-9]{2}|20[0-9]{2})(?![0-9]+|\W[0-9]{2}\W[0-9]{2})([ _\,\.\(\)\[\]\-][^0-9]|).*(19[0-9]{2}|20[0-9]{2})*",
            r"(.+[^_\,\.\(\)\[\]\-])[ _\.\(\)\[\]\-]+(19[0-9]{2}|20[0-9]{2})(?![0-9]+|\W[0-9]{2}\W[0-9]{2})([ _\,\.\(\)\[\]\-][^0-9]|).*(19[0-9]{2}|20[0-9]{2})*",
        ]);

        let clean_strings = str_vec(&[
            r"^\s*(?<cleaned>.+?)[ _\,\.\(\)\[\]\-](3d|sbs|tab|hsbs|htab|mvc|HDR|HDC|UHD|UltraHD|4k|ac3|dts|custom|dc|divx|divx5|dsr|dsrip|dutch|dvd|dvdrip|dvdscr|dvdscreener|screener|dvdivx|cam|fragment|fs|hdtv|hdrip|hdtvrip|internal|limited|multi|subs|ntsc|ogg|ogm|pal|pdtv|proper|repack|rerip|retail|cd[1-9]|r5|bd5|bd|se|svcd|swedish|german|read.nfo|nfofix|unrated|ws|telesync|ts|telecine|tc|brrip|bdrip|480p|480i|576p|576i|720p|720i|1080p|1080i|2160p|hrhd|hrhdtv|hddvd|bluray|blu-ray|x264|x265|h264|h265|xvid|xvidvd|xxx|www.www|AAC|DTS)(?=[ _\,\.\(\)\[\]\-]|$)",
            r"^\s*(?<cleaned>.+?)((\s*\[[^\]]+\]\s*)+)(\.[^\s]+)?$",
            r"^\s*(?<cleaned>.+?)\WE[0-9]+(-|~)E?[0-9]+(\W|$)",
            r"^\s*\[[^\]]+\](?!\.\w+$)\s*(?<cleaned>.+)",
            r"^\s*(?<cleaned>.+?)\s+-\s+[0-9]+\s*$",
            r"^\s*(?<cleaned>.+?)(([-._ ](trailer|sample))|-(scene|clip|behindthescenes|deleted|deletedscene|featurette|short|interview|other|extra))$",
        ]);

        let subtitle_file_extensions = str_vec(&[
            ".ass", ".mks", ".sami", ".smi", ".srt", ".ssa", ".sub", ".sup", ".vtt",
        ]);

        let lyric_file_extensions = str_vec(&[".lrc", ".elrc", ".txt"]);

        let album_stacking_prefixes = str_vec(&[
            "cd",
            "digital media",
            "disc",
            "disk",
            "vol",
            "volume",
            "part",
            "act",
        ]);

        let artist_subfolders = str_vec(&[
            "albums",
            "broadcasts",
            "bootlegs",
            "compilations",
            "dj-mixes",
            "eps",
            "live",
            "mixtapes",
            "others",
            "remixes",
            "singles",
            "soundtracks",
            "spokenwords",
            "streets",
        ]);

        let audio_file_extensions = str_vec(&[
            ".669", ".3gp", ".aa", ".aac", ".aax", ".ac3", ".act", ".adp", ".adplug", ".adx",
            ".afc", ".amf", ".aif", ".aifc", ".aiff", ".alac", ".amr", ".ape", ".ast", ".au",
            ".awb", ".cda", ".cue", ".dmf", ".dsf", ".dsm", ".dsp", ".dts", ".dvf", ".eac3",
            ".ec3", ".far", ".flac", ".gdm", ".gsm", ".gym", ".hps", ".imf", ".it", ".m15", ".m4a",
            ".m4b", ".mac", ".med", ".mka", ".mmf", ".mod", ".mogg", ".mp2", ".mp3", ".mpa",
            ".mpc", ".mpp", ".mp+", ".msv", ".nmf", ".nsf", ".nsv", ".oga", ".ogg", ".okt",
            ".opus", ".pls", ".ra", ".rf64", ".rm", ".s3m", ".sfx", ".shn", ".sid", ".stm",
            ".strm", ".ult", ".uni", ".vox", ".wav", ".wma", ".wv", ".xm", ".xsp", ".ymf",
        ]);

        let media_flag_delimiters = vec!['.'];
        let media_forced_flags = str_vec(&["foreign", "forced"]);
        let media_default_flags = str_vec(&["default"]);
        let media_hearing_impaired_flags = str_vec(&["cc", "hi", "sdh"]);

        let episode_expressions = build_episode_expressions();

        let video_extra_rules = build_video_extra_rules();

        let all_extras_types_folder_names = video_extra_rules
            .iter()
            .filter(|r| r.rule_type == ExtraRuleType::DirectoryName)
            .map(|r| (r.token.to_lowercase(), r.extra_type))
            .collect();

        let format_3d_rules = vec![
            // Kodi rules:
            Format3DRule::new("hsbs", Some("3d".to_string())),
            Format3DRule::new("sbs", Some("3d".to_string())),
            Format3DRule::new("htab", Some("3d".to_string())),
            Format3DRule::new("tab", Some("3d".to_string())),
            // Media Browser rules:
            Format3DRule::new("fsbs", None),
            Format3DRule::new("hsbs", None),
            Format3DRule::new("sbs", None),
            Format3DRule::new("ftab", None),
            Format3DRule::new("htab", None),
            Format3DRule::new("tab", None),
            Format3DRule::new("sbs3d", None),
            Format3DRule::new("mvc", None),
        ];

        let audio_book_parts_expressions = str_vec(&[
            // Detect specified chapters, like CH 01
            r"ch(?:apter)?[\s_-]?(?<chapter>[0-9]+)",
            // Detect specified parts, like Part 02
            r"p(?:ar)?t[\s_-]?(?<part>[0-9]+)",
            // Chapter is often beginning of filename
            "^(?<chapter>[0-9]+)",
            // Part if often ending of filename
            "(?<!ch(?:apter) )(?<part>[0-9]+)$",
            // Sometimes named as 0001_005 (chapter_part)
            "(?<chapter>[0-9]+)_(?<part>[0-9]+)",
            // Some audiobooks are ripped from cd's, named by disk number.
            r"dis(?:c|k)[\s_-]?(?<chapter>[0-9]+)",
        ]);

        let audio_book_names_expressions = str_vec(&[
            // Detect year usually in brackets after name Batman (2020)
            r"^(?<name>.+?)\s*\(\s*(?<year>[0-9]{4})\s*\)\s*$",
            r"^\s*(?<name>[^ ].*?)\s*$",
        ]);

        let multiple_episode_expressions = build_multiple_episode_expressions();

        let audio_book_parts_regexes = audio_book_parts_expressions
            .iter()
            .map(|e| compile(e))
            .collect();
        let audio_book_names_regexes = audio_book_names_expressions
            .iter()
            .map(|e| compile(e))
            .collect();
        let clean_date_time_regexes = clean_date_times.iter().map(|e| compile(e)).collect();
        let clean_string_regexes = clean_strings.iter().map(|e| compile(e)).collect();

        Self {
            all_extras_types_folder_names,
            audio_file_extensions,
            media_flag_delimiters,
            media_forced_flags,
            media_default_flags,
            media_hearing_impaired_flags,
            album_stacking_prefixes,
            artist_subfolders,
            subtitle_file_extensions,
            lyric_file_extensions,
            episode_expressions,
            video_file_extensions,
            stub_file_extensions,
            audio_book_parts_expressions,
            audio_book_names_expressions,
            stub_types,
            video_flag_delimiters,
            format_3d_rules,
            video_file_stacking_rules,
            clean_date_times,
            clean_strings,
            multiple_episode_expressions,
            video_extra_rules,
            audio_book_parts_regexes,
            audio_book_names_regexes,
            clean_date_time_regexes,
            clean_string_regexes,
        }
    }

    /// Recompiles the raw clean-regex strings into compiled regexes.
    pub fn compile(&mut self) {
        self.audio_book_parts_regexes = self
            .audio_book_parts_expressions
            .iter()
            .map(|e| compile(e))
            .collect();
        self.audio_book_names_regexes = self
            .audio_book_names_expressions
            .iter()
            .map(|e| compile(e))
            .collect();
        self.clean_date_time_regexes = self.clean_date_times.iter().map(|e| compile(e)).collect();
        self.clean_string_regexes = self.clean_strings.iter().map(|e| compile(e)).collect();
    }
}

/// Compiles a raw regex string case-insensitively.
///
/// # Panics
///
/// Panics if `exp` is not a valid regex. Production strings are vendored and
/// valid; the empty-string test path is also valid.
fn compile(exp: &str) -> GuardedRegex {
    GuardedRegex::new(&format!("(?i){exp}")).expect("NamingOptions clean regex is valid")
}

fn str_vec(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

fn named(expr: &str) -> EpisodeExpression {
    let mut e = EpisodeExpression::new(expr, false);
    e.is_named = true;
    e
}

#[allow(clippy::too_many_lines)]
fn build_episode_expressions() -> Vec<EpisodeExpression> {
    let mut list = Vec::new();

    // *** Begin Kodi Standard Naming
    // foo.s01.e01, foo.s01_e01, S01E02 foo, S01 - E02
    // NOTE: the C# class `[][ ._-]` starts with a literal `]`. .NET treats a
    // leading `]` in a class as literal, but Rust's regex dialect does not, so
    // the `]` is escaped as `\]` here — same matching semantics, valid Rust.
    list.push(named(
        r".*(\\|\/)(?<seriesname>((?![Ss]([0-9]+)[\]\[ ._-]*[Ee]([0-9]+))[^\\\/])*)?[Ss](?<seasonnumber>[0-9]+)[\]\[ ._-]*[Ee](?<epnumber>[0-9]+)([^\\/]*)$",
    ));
    // foo.ep01, foo.EP_01
    list.push(EpisodeExpression::new(
        r"[\._ -]()[Ee][Pp]_?([0-9]+)([^\\/]*)$",
        false,
    ));
    // foo.E01., foo.e01.
    list.push(EpisodeExpression::new(
        r"[^\\/]*?()\.?[Ee]([0-9]+)\.([^\\/]*)$",
        false,
    ));
    {
        let mut e = EpisodeExpression::new(
            r"(?<year>[0-9]{4})[._ -](?<month>[0-9]{2})[._ -](?<day>[0-9]{2})",
            true,
        );
        e.date_time_formats = str_vec(&["yyyy.MM.dd", "yyyy-MM-dd", "yyyy_MM_dd", "yyyy MM dd"]);
        list.push(e);
    }
    {
        let mut e = EpisodeExpression::new(
            r"(?<day>[0-9]{2})[._ -](?<month>[0-9]{2})[._ -](?<year>[0-9]{4})",
            true,
        );
        e.date_time_formats = str_vec(&["dd.MM.yyyy", "dd-MM-yyyy", "dd_MM_yyyy", "dd MM yyyy"]);
        list.push(e);
    }
    // "Series Season X Episode X - Title.avi"
    list.push(named(
        r".*[\\\/]((?<seriesname>[^\\/]+?)\s)?[Ss](?:eason)?\s*(?<seasonnumber>[0-9]+)\s+[Ee](?:pisode)?\s*(?<epnumber>[0-9]+).*$",
    ));
    // "Foo Bar 889"
    list.push(named(
        r".*[\\\/](?![Ee]pisode)(?<seriesname>[\w\s]+?)\s(?<epnumber>[0-9]{1,4})(-(?<endingepnumber>[0-9]{2,4}))*[^\\\/x]*$",
    ));
    {
        let mut e = EpisodeExpression::new(
            r"[\\\/\._ \[\(-]([0-9]+)x([0-9]+(?:(?:[a-i]|\.[1-9])(?![0-9]))?)([^\\\/]*)$",
            false,
        );
        e.supports_absolute_episode_numbers = true;
        list.push(e);
    }
    // [bar] Foo - 1 [baz]
    list.push(named(
        r".*[\\\/]?.*?(\[.*?\])+.*?(?<seriesname>[-\w\s]+?)[\s_]*-[\s_]*(?<epnumber>[0-9]+).*$",
    ));
    // "Name - 101.mkv", anime absolute with hyphen
    list.push(named(
        r".*[\\\/](?<seriesname>[^\\\/]+?)[\s_]+-[\s_]+(?<epnumber>[0-9]+)[\s_]*(?:\[.*?\]|\(.*?\))*[\s_]*(?:\.\w+)?$",
    ));
    // /server/anything_102.mp4 etc.
    {
        let mut e = EpisodeExpression::new(
            r"[\\/._ -](?<seriesname>(?![0-9]+[0-9][0-9])([^\\\/_])*)[\\\/._ -](?<seasonnumber>[0-9]+)(?<epnumber>[0-9][0-9](?:(?:[a-i]|\.[1-9])(?![0-9]))?)([._ -][^\\\/]*)$",
            false,
        );
        e.is_optimistic = true;
        e.is_named = true;
        e.supports_absolute_episode_numbers = false;
        list.push(e);
    }
    {
        let mut e = EpisodeExpression::new(
            r"[\/._ -]p(?:ar)?t[_. -]()([ivx]+|[0-9]+)([._ -][^\/]*)$",
            false,
        );
        e.supports_absolute_episode_numbers = true;
        list.push(e);
    }
    // *** End Kodi Standard Naming

    // "Episode 16", "Episode 16 - Title"
    list.push(named(
        r"[Ee]pisode (?<epnumber>[0-9]+)(-(?<endingepnumber>[0-9]+))?[^\\\/]*$",
    ));
    list.push(named(
        r".*(\\|\/)[sS]?(?<seasonnumber>[0-9]+)[xX](?<epnumber>[0-9]+)[^\\\/]*$",
    ));
    list.push(named(
        r".*(\\|\/)[sS](?<seasonnumber>[0-9]+)[x,X]?[eE](?<epnumber>[0-9]+)[^\\\/]*$",
    ));
    list.push(named(
        r".*(\\|\/)(?<seriesname>((?![sS]?[0-9]{1,4}[xX][0-9]{1,3})[^\\\/])*)?([sS]?(?<seasonnumber>[0-9]{1,4})[xX](?<epnumber>[0-9]+))[^\\\/]*$",
    ));
    list.push(named(
        r".*(\\|\/)(?<seriesname>[^\\\/]*)[sS](?<seasonnumber>[0-9]{1,4})[xX\.]?[eE](?<epnumber>[0-9]+)[^\\\/]*$",
    ));
    // "01.avi"
    {
        let mut e = EpisodeExpression::new(
            r".*[\\\/](?<epnumber>[0-9]+)(-(?<endingepnumber>[0-9]+))*\.\w+$",
            false,
        );
        e.is_optimistic = true;
        e.is_named = true;
        list.push(e);
    }
    // "1-12 episode title"
    list.push(EpisodeExpression::new("([0-9]+)-([0-9]+)", false));
    // "01 - blah.avi", "01-blah.avi"
    {
        let mut e = EpisodeExpression::new(
            r".*(\\|\/)(?<epnumber>[0-9]{1,3})(-(?<endingepnumber>[0-9]{2,3}))*\s?-\s?[^\\\/]*$",
            false,
        );
        e.is_optimistic = true;
        e.is_named = true;
        list.push(e);
    }
    // "01.blah.avi"
    {
        let mut e = EpisodeExpression::new(
            r".*(\\|\/)(?<epnumber>[0-9]{1,3})(-(?<endingepnumber>[0-9]{2,3}))*\.[^\\\/]+$",
            false,
        );
        e.is_optimistic = true;
        e.is_named = true;
        list.push(e);
    }
    // "blah - 01.avi" etc.
    {
        let mut e = EpisodeExpression::new(
            r".*[\\\/][^\\\/]* - (?<epnumber>[0-9]{1,3})(-(?<endingepnumber>[0-9]{2,3}))*[^\\\/]*$",
            false,
        );
        e.is_optimistic = true;
        e.is_named = true;
        list.push(e);
    }
    // "01 episode title.avi"
    {
        let mut e = EpisodeExpression::new(
            r"[Ss]eason[\._ ](?<seasonnumber>[0-9]+)[\\\/](?<epnumber>[0-9]{1,3})([^\\\/]*)$",
            false,
        );
        e.is_optimistic = true;
        e.is_named = true;
        list.push(e);
    }
    // Series and season only: "the show/season 1", "the show/s01"
    list.push(named(
        r"(.*(\\|\/))*(?<seriesname>.+)\/[Ss](eason)?[\. _\-]*(?<seasonnumber>[0-9]+)",
    ));
    // Series and season only: "the show S01", "the show season 1"
    list.push(named(
        r"(.*(\\|\/))*(?<seriesname>.+)[\. _\-]+[sS](eason)?[\. _\-]*(?<seasonnumber>[0-9]+)",
    ));
    // Anime style
    // `[^[\]]` (an unescaped `[` inside a class) is valid in .NET but rejected
    // by Rust's regex dialect; escape it as `[^\[\]]` — same semantics.
    list.push(named(
        r"(?:\[(?:[^\]]+)\]\s*)?(?<seriesname>\[[^\]]+\]|[^\[\]]+)\s*\[(?<epnumber>[0-9]+)\]",
    ));

    list
}

fn build_multiple_episode_expressions() -> Vec<EpisodeExpression> {
    let patterns = [
        r".*(\\|\/)[sS]?(?<seasonnumber>[0-9]{1,4})[xX](?<epnumber>[0-9]{1,3})((-| - )[0-9]{1,4}[eExX](?<endingepnumber>[0-9]{1,3}))+[^\\\/]*$",
        r".*(\\|\/)[sS]?(?<seasonnumber>[0-9]{1,4})[xX](?<epnumber>[0-9]{1,3})((-| - )[0-9]{1,4}[xX][eE](?<endingepnumber>[0-9]{1,3}))+[^\\\/]*$",
        r".*(\\|\/)[sS]?(?<seasonnumber>[0-9]{1,4})[xX](?<epnumber>[0-9]{1,3})((-| - )?[xXeE](?<endingepnumber>[0-9]{1,3}))+[^\\\/]*$",
        r".*(\\|\/)[sS]?(?<seasonnumber>[0-9]{1,4})[xX](?<epnumber>[0-9]{1,3})(-[xE]?[eE]?(?<endingepnumber>[0-9]{1,3}))+[^\\\/]*$",
        r".*(\\|\/)(?<seriesname>((?![sS]?[0-9]{1,4}[xX][0-9]{1,3})[^\\\/])*)?([sS]?(?<seasonnumber>[0-9]{1,4})[xX](?<epnumber>[0-9]{1,3}))((-| - )[0-9]{1,4}[xXeE](?<endingepnumber>[0-9]{1,3}))+[^\\\/]*$",
        r".*(\\|\/)(?<seriesname>((?![sS]?[0-9]{1,4}[xX][0-9]{1,3})[^\\\/])*)?([sS]?(?<seasonnumber>[0-9]{1,4})[xX](?<epnumber>[0-9]{1,3}))((-| - )[0-9]{1,4}[xX][eE](?<endingepnumber>[0-9]{1,3}))+[^\\\/]*$",
        r".*(\\|\/)(?<seriesname>((?![sS]?[0-9]{1,4}[xX][0-9]{1,3})[^\\\/])*)?([sS]?(?<seasonnumber>[0-9]{1,4})[xX](?<epnumber>[0-9]{1,3}))((-| - )?[xXeE](?<endingepnumber>[0-9]{1,3}))+[^\\\/]*$",
        r".*(\\|\/)(?<seriesname>((?![sS]?[0-9]{1,4}[xX][0-9]{1,3})[^\\\/])*)?([sS]?(?<seasonnumber>[0-9]{1,4})[xX](?<epnumber>[0-9]{1,3}))(-[xX]?[eE]?(?<endingepnumber>[0-9]{1,3}))+[^\\\/]*$",
        r".*(\\|\/)(?<seriesname>[^\\\/]*)[sS](?<seasonnumber>[0-9]{1,4})[xX\.]?[eE](?<epnumber>[0-9]{1,3})((-| - )?[xXeE](?<endingepnumber>[0-9]{1,3}))+[^\\\/]*$",
        r".*(\\|\/)(?<seriesname>[^\\\/]*)[sS](?<seasonnumber>[0-9]{1,4})[xX\.]?[eE](?<epnumber>[0-9]{1,3})(-[xX]?[eE]?(?<endingepnumber>[0-9]{1,3}))+[^\\\/]*$",
    ];

    patterns.iter().map(|p| named(p)).collect()
}

fn build_video_extra_rules() -> Vec<ExtraRule> {
    use ExtraRuleType::{DirectoryName, Filename, Suffix};
    use ExtraType::{
        BehindTheScenes, Clip, DeletedScene, Featurette, Interview, Sample, Scene, Short,
        ThemeSong, ThemeVideo, Trailer, Unknown,
    };
    use MediaType::{Audio, Video};

    vec![
        ExtraRule::new(Trailer, DirectoryName, "trailers", Video),
        ExtraRule::new(ThemeVideo, DirectoryName, "backdrops", Video),
        ExtraRule::new(ThemeSong, DirectoryName, "theme-music", Audio),
        ExtraRule::new(BehindTheScenes, DirectoryName, "behind the scenes", Video),
        ExtraRule::new(DeletedScene, DirectoryName, "deleted scenes", Video),
        ExtraRule::new(Interview, DirectoryName, "interviews", Video),
        ExtraRule::new(Scene, DirectoryName, "scenes", Video),
        ExtraRule::new(Sample, DirectoryName, "samples", Video),
        ExtraRule::new(Short, DirectoryName, "shorts", Video),
        ExtraRule::new(Featurette, DirectoryName, "featurettes", Video),
        ExtraRule::new(Unknown, DirectoryName, "extras", Video),
        ExtraRule::new(Unknown, DirectoryName, "extra", Video),
        ExtraRule::new(Unknown, DirectoryName, "other", Video),
        ExtraRule::new(Clip, DirectoryName, "clips", Video),
        ExtraRule::new(Trailer, Filename, "trailer", Video),
        ExtraRule::new(Sample, Filename, "sample", Video),
        ExtraRule::new(ThemeSong, Filename, "theme", Audio),
        ExtraRule::new(Trailer, Suffix, "-trailer", Video),
        ExtraRule::new(Trailer, Suffix, ".trailer", Video),
        ExtraRule::new(Trailer, Suffix, "_trailer", Video),
        ExtraRule::new(Trailer, Suffix, "- trailer", Video),
        ExtraRule::new(Sample, Suffix, "-sample", Video),
        ExtraRule::new(Sample, Suffix, ".sample", Video),
        ExtraRule::new(Sample, Suffix, "_sample", Video),
        ExtraRule::new(Sample, Suffix, "- sample", Video),
        ExtraRule::new(Scene, Suffix, "-scene", Video),
        ExtraRule::new(Clip, Suffix, "-clip", Video),
        ExtraRule::new(Interview, Suffix, "-interview", Video),
        ExtraRule::new(BehindTheScenes, Suffix, "-behindthescenes", Video),
        ExtraRule::new(DeletedScene, Suffix, "-deleted", Video),
        ExtraRule::new(DeletedScene, Suffix, "-deletedscene", Video),
        ExtraRule::new(Featurette, Suffix, "-featurette", Video),
        ExtraRule::new(Short, Suffix, "-short", Video),
        ExtraRule::new(Unknown, Suffix, "-extra", Video),
        ExtraRule::new(Unknown, Suffix, "-other", Video),
    ]
}
