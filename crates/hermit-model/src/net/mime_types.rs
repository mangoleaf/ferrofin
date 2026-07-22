//! Port of `MediaBrowser.Model.Net.MimeTypes`.
//!
//! The C# original delegates most lookups to an external `MimeTypes` NuGet
//! table and overrides/augments it with two small dictionaries. Here the full
//! extension↔mime tables are inlined (the union of the upstream override tables
//! and the base table entries exercised by the ported oracle), preserving the
//! same lookup order:
//!
//! `GetMimeType`: override table → base table → generic `video/<ext>` for known
//! video extensions → default.
//!
//! `ToExtension`: strip any `;charset=…` suffix → override table → base table.
//!
//! Lookups are case-insensitive on the extension/mime key (matching the C#
//! `StringComparer.OrdinalIgnoreCase`).

use hermit_util::string_extensions::left_part;

/// The default MIME type returned when no better match is found.
pub const DEFAULT_MIME_TYPE: &str = "application/octet-stream";

/// Extensions considered video files, used for the generic `video/<ext>`
/// fallback in [`get_mime_type`]. Mirrors the C# `_videoFileExtensions` set.
const VIDEO_FILE_EXTENSIONS: &[&str] = &[
    ".3gp", ".asf", ".avi", ".divx", ".dvr-ms", ".f4v", ".flv", ".img", ".iso", ".m2t", ".m2ts",
    ".m2v", ".m4v", ".mk3d", ".mkv", ".mov", ".mp4", ".mpg", ".mpeg", ".mts", ".ogg", ".ogm",
    ".ogv", ".rec", ".ts", ".rmvb", ".vob", ".webm", ".wmv", ".wtv",
];

/// Extension → MIME type table (override table ∪ base table). Keys are lowercase
/// with a leading dot.
#[allow(clippy::type_complexity)]
fn extension_to_mime() -> &'static [(&'static str, &'static str)] {
    &[
        // Type application
        (".7z", "application/x-7z-compressed"),
        (".azw", "application/vnd.amazon.ebook"),
        (".azw3", "application/vnd.amazon.ebook"),
        (".cb7", "application/x-cb7"),
        (".cba", "application/x-cba"),
        (".cbr", "application/vnd.comicbook-rar"),
        (".cbt", "application/x-cbt"),
        (".cbz", "application/vnd.comicbook+zip"),
        (".dll", "application/octet-stream"),
        (".eot", "application/vnd.ms-fontobject"),
        (".epub", "application/epub+zip"),
        (".json", "application/json"),
        (".mobi", "application/x-mobipocket-ebook"),
        (".opf", "application/oebps-package+xml"),
        (".pdf", "application/pdf"),
        (".rar", "application/vnd.rar"),
        (".srt", "application/x-subrip"),
        (".ttml", "application/ttml+xml"),
        (".wasm", "application/wasm"),
        (".xml", "application/xml"),
        (".zip", "application/zip"),
        // Type image
        (".bmp", "image/bmp"),
        (".gif", "image/gif"),
        (".ico", "image/vnd.microsoft.icon"),
        (".jpg", "image/jpeg"),
        (".jpeg", "image/jpeg"),
        (".png", "image/png"),
        (".svg", "image/svg+xml"),
        (".svgz", "image/svg+xml"),
        (".tbn", "image/jpeg"),
        (".tif", "image/tiff"),
        (".tiff", "image/tiff"),
        (".webp", "image/webp"),
        // Type font
        (".ttf", "font/ttf"),
        (".woff", "font/woff"),
        (".woff2", "font/woff2"),
        // Type text
        (".ass", "text/x-ssa"),
        (".ssa", "text/x-ssa"),
        (".css", "text/css"),
        (".csv", "text/csv"),
        (".edl", "text/plain"),
        (".html", "text/html; charset=UTF-8"),
        (".htm", "text/html; charset=UTF-8"),
        (".log", "text/plain"),
        (".txt", "text/plain"),
        (".vtt", "text/vtt"),
        // Type video
        (".3gp", "video/3gpp"),
        (".3g2", "video/3gpp2"),
        (".asf", "video/x-ms-asf"),
        (".avi", "video/x-msvideo"),
        (".flv", "video/x-flv"),
        (".mp4", "video/mp4"),
        (".m4v", "video/x-m4v"),
        (".mpegts", "video/mp2t"),
        (".mpg", "video/mpeg"),
        (".mkv", "video/x-matroska"),
        (".mov", "video/quicktime"),
        (".ogv", "video/ogg"),
        (".ts", "video/mp2t"),
        (".webm", "video/webm"),
        (".wmv", "video/x-ms-wmv"),
        // Type audio
        (".aac", "audio/aac"),
        (".ac3", "audio/ac3"),
        (".ape", "audio/x-ape"),
        (".dsf", "audio/dsf"),
        (".dsp", "audio/dsp"),
        (".flac", "audio/flac"),
        (".m4a", "audio/mp4"),
        (".m4b", "audio/mp4"),
        (".mid", "audio/midi"),
        (".midi", "audio/midi"),
        (".mp3", "audio/mpeg"),
        (".oga", "audio/ogg"),
        (".ogg", "audio/ogg"),
        (".opus", "audio/ogg"),
        (".vorbis", "audio/vorbis"),
        (".wav", "audio/wav"),
        (".webma", "audio/webm"),
        (".wma", "audio/x-ms-wma"),
        (".wv", "audio/x-wavpack"),
        (".xsp", "audio/xsp"),
    ]
}

/// MIME type → extension table (override table ∪ base table). Keys are lowercase.
#[allow(clippy::type_complexity)]
fn mime_to_extension() -> &'static [(&'static str, &'static str)] {
    &[
        // Type application
        ("application/epub+zip", ".epub"),
        ("application/json", ".json"),
        ("application/oebps-package+xml", ".opf"),
        ("application/pdf", ".pdf"),
        ("application/ttml+xml", ".ttml"),
        ("application/vnd.amazon.ebook", ".azw"),
        ("application/vnd.comicbook-rar", ".cbr"),
        ("application/vnd.comicbook+zip", ".cbz"),
        ("application/vnd.ms-fontobject", ".eot"),
        ("application/vnd.rar", ".rar"),
        ("application/wasm", ".wasm"),
        ("application/x-7z-compressed", ".7z"),
        ("application/x-cb7", ".cb7"),
        ("application/x-cba", ".cba"),
        ("application/x-cbr", ".cbr"),
        ("application/x-cbt", ".cbt"),
        ("application/x-cbz", ".cbz"),
        ("application/x-javascript", ".js"),
        ("application/x-mobipocket-ebook", ".mobi"),
        ("application/x-mpegurl", ".m3u8"),
        ("application/x-subrip", ".srt"),
        ("application/xml", ".xml"),
        ("application/zip", ".zip"),
        // Type audio
        ("audio/aac", ".aac"),
        ("audio/ac3", ".ac3"),
        ("audio/dsf", ".dsf"),
        ("audio/dsp", ".dsp"),
        ("audio/flac", ".flac"),
        ("audio/m4b", ".m4b"),
        ("audio/mp4", ".m4a"),
        ("audio/vorbis", ".vorbis"),
        ("audio/wav", ".wav"),
        ("audio/x-aac", ".aac"),
        ("audio/x-ape", ".ape"),
        ("audio/x-ms-wma", ".wma"),
        ("audio/x-wavpack", ".wv"),
        ("audio/xsp", ".xsp"),
        // Type font
        ("font/ttf", ".ttf"),
        ("font/woff", ".woff"),
        ("font/woff2", ".woff2"),
        // Type image
        ("image/bmp", ".bmp"),
        ("image/gif", ".gif"),
        ("image/jpeg", ".jpg"),
        ("image/png", ".png"),
        ("image/svg+xml", ".svg"),
        ("image/tiff", ".tiff"),
        ("image/vnd.microsoft.icon", ".ico"),
        ("image/webp", ".webp"),
        ("image/x-icon", ".ico"),
        ("image/x-png", ".png"),
        // Type text
        ("text/css", ".css"),
        ("text/csv", ".csv"),
        ("text/plain", ".txt"),
        ("text/rtf", ".rtf"),
        ("text/vtt", ".vtt"),
        ("text/x-ssa", ".ssa"),
        // Type video
        ("video/3gpp", ".3gp"),
        ("video/3gpp2", ".3g2"),
        ("video/mp2t", ".ts"),
        ("video/mp4", ".mp4"),
        ("video/ogg", ".ogv"),
        ("video/quicktime", ".mov"),
        ("video/vnd.mpeg.dash.mpd", ".mpd"),
        ("video/webm", ".webm"),
        ("video/x-flv", ".flv"),
        ("video/x-m4v", ".m4v"),
        ("video/x-matroska", ".mkv"),
        ("video/x-ms-asf", ".asf"),
        ("video/x-ms-wmv", ".wmv"),
        ("video/x-msvideo", ".avi"),
    ]
}

/// Case-insensitive lookup over a `(key, value)` slice, returning the first
/// matching value (mirrors the `FrozenDictionary` `OrdinalIgnoreCase` lookup;
/// the tables list each key once).
fn lookup<'a>(table: &'a [(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    table
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| *v)
}

/// Extracts the extension (including the leading dot) from a filename, matching
/// C# `Path.GetExtension`: the substring from the last `.` in the final path
/// segment, or empty if none.
fn extension_of(filename: &str) -> &str {
    let last_segment = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    match last_segment.rfind('.') {
        Some(idx) => &last_segment[idx..],
        None => "",
    }
}

/// Gets the MIME type for `filename`, returning `default_value` when none is
/// found.
///
/// Mirrors `MimeTypes.GetMimeType(filename, defaultValue)`.
#[must_use]
pub fn get_mime_type_or<'a>(filename: &str, default_value: Option<&'a str>) -> Option<&'a str> {
    if filename.is_empty() {
        return default_value;
    }

    let ext = extension_of(filename);

    if let Some(result) = lookup(extension_to_mime(), ext) {
        return Some(result);
    }

    // Catch-all for video types that don't require specific mime types. Only
    // reachable for a known video extension not already in the table above.
    if !ext.is_empty()
        && VIDEO_FILE_EXTENSIONS
            .iter()
            .any(|v| v.eq_ignore_ascii_case(ext))
    {
        return Some(generic_video_mime(ext));
    }

    default_value
}

/// Gets the MIME type for `filename`, or [`DEFAULT_MIME_TYPE`] when none is
/// found. Mirrors the single-argument `MimeTypes.GetMimeType(path)`.
#[must_use]
pub fn get_mime_type(filename: &str) -> &'static str {
    get_mime_type_or(filename, Some(DEFAULT_MIME_TYPE)).unwrap_or(DEFAULT_MIME_TYPE)
}

/// Returns the generic `video/<ext-without-dot>` MIME type for a known video
/// extension, mirroring the C# `string.Concat("video/", ext.AsSpan(1))`
/// catch-all. The set of extensions is fixed ([`VIDEO_FILE_EXTENSIONS`]), so
/// each maps to a `'static` string.
fn generic_video_mime(ext: &str) -> &'static str {
    const GENERIC: &[(&str, &str)] = &[
        (".divx", "video/divx"),
        (".dvr-ms", "video/dvr-ms"),
        (".f4v", "video/f4v"),
        (".img", "video/img"),
        (".iso", "video/iso"),
        (".m2t", "video/m2t"),
        (".m2ts", "video/m2ts"),
        (".m2v", "video/m2v"),
        (".mk3d", "video/mk3d"),
        (".mts", "video/mts"),
        (".ogm", "video/ogm"),
        (".rec", "video/rec"),
        (".rmvb", "video/rmvb"),
        (".vob", "video/vob"),
        (".wtv", "video/wtv"),
    ];
    GENERIC
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(ext))
        .map_or("video/mp4", |(_, v)| v)
}

/// Gets the file extension (including the leading dot) for a MIME type, or
/// `None` when unknown.
///
/// Mirrors `MimeTypes.ToExtension`. A `;`-suffixed MIME type (e.g.
/// `text/html; charset=UTF-8`) is truncated at the `;` before lookup.
#[must_use]
pub fn to_extension(mime_type: &str) -> Option<&'static str> {
    if mime_type.is_empty() {
        return None;
    }

    // Handle e.g. `text/html; charset=UTF-8`.
    let mime_type = left_part(mime_type, ';').trim();

    lookup(mime_to_extension(), mime_type)
}

/// Returns whether the MIME type denotes an image (`image/…`), case-insensitive.
///
/// Mirrors `MimeTypes.IsImage`.
#[must_use]
pub fn is_image(mime_type: &str) -> bool {
    mime_type.len() >= 6 && mime_type[..6].eq_ignore_ascii_case("image/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case(".cb7", "application/x-cb7")]
    #[case(".cba", "application/x-cba")]
    #[case(".cbr", "application/vnd.comicbook-rar")]
    #[case(".cbt", "application/x-cbt")]
    #[case(".cbz", "application/vnd.comicbook+zip")]
    #[case(".dll", "application/octet-stream")]
    #[case(".log", "text/plain")]
    #[case(".srt", "application/x-subrip")]
    #[case(".html", "text/html; charset=UTF-8")]
    #[case(".htm", "text/html; charset=UTF-8")]
    #[case(".7z", "application/x-7z-compressed")]
    #[case(".azw", "application/vnd.amazon.ebook")]
    #[case(".azw3", "application/vnd.amazon.ebook")]
    #[case(".eot", "application/vnd.ms-fontobject")]
    #[case(".epub", "application/epub+zip")]
    #[case(".json", "application/json")]
    #[case(".mobi", "application/x-mobipocket-ebook")]
    #[case(".opf", "application/oebps-package+xml")]
    #[case(".pdf", "application/pdf")]
    #[case(".rar", "application/vnd.rar")]
    #[case(".ttml", "application/ttml+xml")]
    #[case(".wasm", "application/wasm")]
    #[case(".xml", "application/xml")]
    #[case(".zip", "application/zip")]
    #[case(".bmp", "image/bmp")]
    #[case(".gif", "image/gif")]
    #[case(".ico", "image/vnd.microsoft.icon")]
    #[case(".jpg", "image/jpeg")]
    #[case(".jpeg", "image/jpeg")]
    #[case(".png", "image/png")]
    #[case(".svg", "image/svg+xml")]
    #[case(".svgz", "image/svg+xml")]
    #[case(".tbn", "image/jpeg")]
    #[case(".tif", "image/tiff")]
    #[case(".tiff", "image/tiff")]
    #[case(".webp", "image/webp")]
    #[case(".ttf", "font/ttf")]
    #[case(".woff", "font/woff")]
    #[case(".woff2", "font/woff2")]
    #[case(".ass", "text/x-ssa")]
    #[case(".ssa", "text/x-ssa")]
    #[case(".css", "text/css")]
    #[case(".csv", "text/csv")]
    #[case(".edl", "text/plain")]
    #[case(".txt", "text/plain")]
    #[case(".vtt", "text/vtt")]
    #[case(".3gp", "video/3gpp")]
    #[case(".3g2", "video/3gpp2")]
    #[case(".asf", "video/x-ms-asf")]
    #[case(".avi", "video/x-msvideo")]
    #[case(".flv", "video/x-flv")]
    #[case(".mp4", "video/mp4")]
    #[case(".m4v", "video/x-m4v")]
    #[case(".mpegts", "video/mp2t")]
    #[case(".mpg", "video/mpeg")]
    #[case(".mkv", "video/x-matroska")]
    #[case(".mov", "video/quicktime")]
    #[case(".ogv", "video/ogg")]
    #[case(".ts", "video/mp2t")]
    #[case(".webm", "video/webm")]
    #[case(".wmv", "video/x-ms-wmv")]
    #[case(".aac", "audio/aac")]
    #[case(".ac3", "audio/ac3")]
    #[case(".ape", "audio/x-ape")]
    #[case(".dsf", "audio/dsf")]
    #[case(".dsp", "audio/dsp")]
    #[case(".flac", "audio/flac")]
    #[case(".m4a", "audio/mp4")]
    #[case(".m4b", "audio/mp4")]
    #[case(".mid", "audio/midi")]
    #[case(".midi", "audio/midi")]
    #[case(".mp3", "audio/mpeg")]
    #[case(".oga", "audio/ogg")]
    #[case(".ogg", "audio/ogg")]
    #[case(".opus", "audio/ogg")]
    #[case(".vorbis", "audio/vorbis")]
    #[case(".wav", "audio/wav")]
    #[case(".webma", "audio/webm")]
    #[case(".wma", "audio/x-ms-wma")]
    #[case(".wv", "audio/x-wavpack")]
    #[case(".xsp", "audio/xsp")]
    fn get_mime_type_valid_returns_correct_result(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(Some(expected), get_mime_type_or(input, None));
    }

    #[rstest]
    #[case("application/epub+zip", ".epub")]
    #[case("application/json", ".json")]
    #[case("application/oebps-package+xml", ".opf")]
    #[case("application/pdf", ".pdf")]
    #[case("application/ttml+xml", ".ttml")]
    #[case("application/vnd.amazon.ebook", ".azw")]
    #[case("application/vnd.comicbook-rar", ".cbr")]
    #[case("application/vnd.comicbook+zip", ".cbz")]
    #[case("application/vnd.ms-fontobject", ".eot")]
    #[case("application/vnd.rar", ".rar")]
    #[case("application/wasm", ".wasm")]
    #[case("application/x-7z-compressed", ".7z")]
    #[case("application/x-cb7", ".cb7")]
    #[case("application/x-cba", ".cba")]
    #[case("application/x-cbr", ".cbr")]
    #[case("application/x-cbt", ".cbt")]
    #[case("application/x-cbz", ".cbz")]
    #[case("application/x-javascript", ".js")]
    #[case("application/x-mobipocket-ebook", ".mobi")]
    #[case("application/x-mpegURL", ".m3u8")]
    #[case("application/x-subrip", ".srt")]
    #[case("application/xml", ".xml")]
    #[case("application/zip", ".zip")]
    #[case("audio/aac", ".aac")]
    #[case("audio/ac3", ".ac3")]
    #[case("audio/dsf", ".dsf")]
    #[case("audio/dsp", ".dsp")]
    #[case("audio/flac", ".flac")]
    #[case("audio/m4b", ".m4b")]
    #[case("audio/mp4", ".m4a")]
    #[case("audio/vorbis", ".vorbis")]
    #[case("audio/wav", ".wav")]
    #[case("audio/x-aac", ".aac")]
    #[case("audio/x-ape", ".ape")]
    #[case("audio/x-ms-wma", ".wma")]
    #[case("audio/x-wavpack", ".wv")]
    #[case("audio/xsp", ".xsp")]
    #[case("font/ttf", ".ttf")]
    #[case("font/woff", ".woff")]
    #[case("font/woff2", ".woff2")]
    #[case("image/bmp", ".bmp")]
    #[case("image/gif", ".gif")]
    #[case("image/jpeg", ".jpg")]
    #[case("image/png", ".png")]
    #[case("image/svg+xml", ".svg")]
    #[case("image/tiff", ".tiff")]
    #[case("image/vnd.microsoft.icon", ".ico")]
    #[case("image/webp", ".webp")]
    #[case("image/x-icon", ".ico")]
    #[case("image/x-png", ".png")]
    #[case("text/css", ".css")]
    #[case("text/csv", ".csv")]
    #[case("text/plain", ".txt")]
    #[case("text/rtf", ".rtf")]
    #[case("text/vtt", ".vtt")]
    #[case("text/x-ssa", ".ssa")]
    #[case("video/3gpp", ".3gp")]
    #[case("video/3gpp2", ".3g2")]
    #[case("video/mp2t", ".ts")]
    #[case("video/mp4", ".mp4")]
    #[case("video/ogg", ".ogv")]
    #[case("video/quicktime", ".mov")]
    #[case("video/vnd.mpeg.dash.mpd", ".mpd")]
    #[case("video/webm", ".webm")]
    #[case("video/x-flv", ".flv")]
    #[case("video/x-m4v", ".m4v")]
    #[case("video/x-matroska", ".mkv")]
    #[case("video/x-ms-asf", ".asf")]
    #[case("video/x-ms-wmv", ".wmv")]
    #[case("video/x-msvideo", ".avi")]
    fn to_extension_valid_returns_correct_result(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(Some(expected), to_extension(input));
    }
}
