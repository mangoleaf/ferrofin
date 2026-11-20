//! Port of `MediaBrowser.Model.Net.MimeTypes`.
//!
//! The C# original delegates most lookups to the `MimeTypes` NuGet package
//! (2.5.2, pinned in upstream's `Directory.Packages.props`) and overrides or
//! augments its table with two small dictionaries. Here the package's whole
//! extension → mime table is inlined verbatim (`base_type_map`), beside the
//! upstream override table (`MIME_TYPE_LOOKUP`). The mime → extension table
//! (`mime_to_extension`) is the union of the upstream override table and the
//! package entries `ToExtension` gives for the types the ported oracle
//! exercises, plus every `image/*` type of the package (`ToExtension`'s callers
//! here are the image paths). The lookup order is upstream's:
//!
//! `GetMimeType`: override table → package table → generic `video/<ext>` for
//! known video extensions → default.
//!
//! `ToExtension`: strip any `;charset=…` suffix → override table → package
//! table.
//!
//! Lookups are case-insensitive on the extension/mime key (matching the C#
//! `StringComparer.OrdinalIgnoreCase`).

mod base_type_map;

use ferrofin_util::string_extensions::left_part;

/// The default MIME type returned when no better match is found.
pub const DEFAULT_MIME_TYPE: &str = "application/octet-stream";

/// Extensions considered video files, used for the generic `video/<ext>`
/// fallback in [`get_mime_type`]. Mirrors the C# `_videoFileExtensions` set.
const VIDEO_FILE_EXTENSIONS: &[&str] = &[
    ".3gp", ".asf", ".avi", ".divx", ".dvr-ms", ".f4v", ".flv", ".img", ".iso", ".m2t", ".m2ts",
    ".m2v", ".m4v", ".mk3d", ".mkv", ".mov", ".mp4", ".mpg", ".mpeg", ".mts", ".ogg", ".ogm",
    ".ogv", ".rec", ".ts", ".rmvb", ".vob", ".webm", ".wmv", ".wtv",
];

/// Upstream's `_mimeTypeLookup`: extensions the package's table lacks, and
/// types that override it. Keys are lowercase with a leading dot.
const MIME_TYPE_LOOKUP: &[(&str, &str)] = &[
    // Type application
    (".azw3", "application/vnd.amazon.ebook"),
    (".cb7", "application/x-cb7"),
    (".cba", "application/x-cba"),
    (".cbr", "application/vnd.comicbook-rar"),
    (".cbt", "application/x-cbt"),
    (".cbz", "application/vnd.comicbook+zip"),
    // Type image
    (".tbn", "image/jpeg"),
    // Type text
    (".ass", "text/x-ssa"),
    (".ssa", "text/x-ssa"),
    (".edl", "text/plain"),
    (".html", "text/html; charset=UTF-8"),
    (".htm", "text/html; charset=UTF-8"),
    // Type video
    (".mpegts", "video/mp2t"),
    // Type audio
    (".aac", "audio/aac"),
    (".ac3", "audio/ac3"),
    (".ape", "audio/x-ape"),
    (".dsf", "audio/dsf"),
    (".dsp", "audio/dsp"),
    (".flac", "audio/flac"),
    (".m4b", "audio/mp4"),
    (".mp3", "audio/mpeg"),
    (".vorbis", "audio/vorbis"),
    (".webma", "audio/webm"),
    (".wv", "audio/x-wavpack"),
    (".xsp", "audio/xsp"),
];

/// MIME type → extension table (override table ∪ base table). Keys are lowercase.
// A data table, one entry per line, so its length is the table's.
#[allow(clippy::type_complexity, clippy::too_many_lines)]
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
        // Type image: every `image/*` type, so `ToExtension` knows exactly the
        // image types Jellyfin does (the upload routes' `Content-Type` guard,
        // `TryGetImageExtensionFromContentType`, depends on it). The override
        // entries (`image/jpeg`, `image/jpg`, `image/tiff`, `image/x-icon`,
        // `image/x-png`) win; every other type is the base table's
        // `GetMimeTypeExtensions(..).FirstOrDefault()`, the first extension the
        // `MimeTypes` 2.5.2 package lists for it.
        ("image/aces", ".exr"),
        ("image/apng", ".apng"),
        ("image/avci", ".avci"),
        ("image/avcs", ".avcs"),
        ("image/avif", ".avif"),
        ("image/bmp", ".bmp"),
        ("image/cgm", ".cgm"),
        ("image/dicom-rle", ".drle"),
        ("image/dpx", ".dpx"),
        ("image/emf", ".emf"),
        ("image/fits", ".fits"),
        ("image/g3fax", ".g3"),
        ("image/gif", ".gif"),
        ("image/heic", ".heic"),
        ("image/heic-sequence", ".heics"),
        ("image/heif", ".heif"),
        ("image/heif-sequence", ".heifs"),
        ("image/hej2k", ".hej2"),
        ("image/hsj2", ".hsj2"),
        ("image/ief", ".ief"),
        ("image/jls", ".jls"),
        ("image/jp2", ".jp2"),
        ("image/jpeg", ".jpg"),
        ("image/jpg", ".jpg"),
        ("image/jph", ".jph"),
        ("image/jphc", ".jhc"),
        ("image/jpm", ".jpgm"),
        ("image/jpx", ".jpf"),
        ("image/jxr", ".jxr"),
        ("image/jxra", ".jxra"),
        ("image/jxrs", ".jxrs"),
        ("image/jxs", ".jxs"),
        ("image/jxsc", ".jxsc"),
        ("image/jxsi", ".jxsi"),
        ("image/jxss", ".jxss"),
        ("image/ktx", ".ktx"),
        ("image/ktx2", ".ktx2"),
        ("image/png", ".png"),
        ("image/prs.btif", ".btf"),
        ("image/prs.pti", ".pti"),
        ("image/sgi", ".sgi"),
        ("image/svg+xml", ".svg"),
        ("image/t38", ".t38"),
        ("image/tiff", ".tiff"),
        ("image/tiff-fx", ".tfx"),
        ("image/vnd.adobe.photoshop", ".psd"),
        ("image/vnd.airzip.accelerator.azv", ".azv"),
        ("image/vnd.dece.graphic", ".uvg"),
        ("image/vnd.djvu", ".djv"),
        ("image/vnd.dwg", ".dwg"),
        ("image/vnd.dxf", ".dxf"),
        ("image/vnd.fastbidsheet", ".fbs"),
        ("image/vnd.fpx", ".fpx"),
        ("image/vnd.fst", ".fst"),
        ("image/vnd.fujixerox.edmics-mmr", ".mmr"),
        ("image/vnd.fujixerox.edmics-rlc", ".rlc"),
        ("image/vnd.microsoft.icon", ".ico"),
        ("image/vnd.ms-dds", ".dds"),
        ("image/vnd.ms-modi", ".mdi"),
        ("image/vnd.ms-photo", ".wdp"),
        ("image/vnd.net-fpx", ".npx"),
        ("image/vnd.pco.b16", ".b16"),
        ("image/vnd.tencent.tap", ".tap"),
        ("image/vnd.valve.source.texture", ".vtf"),
        ("image/vnd.wap.wbmp", ".wbmp"),
        ("image/vnd.xiff", ".xif"),
        ("image/vnd.zbrush.pcx", ".pcx"),
        ("image/webp", ".webp"),
        ("image/wmf", ".wmf"),
        ("image/x-3ds", ".3ds"),
        ("image/x-cmu-raster", ".ras"),
        ("image/x-cmx", ".cmx"),
        ("image/x-freehand", ".fh"),
        ("image/x-icon", ".ico"),
        ("image/x-jng", ".jng"),
        ("image/x-mrsid-image", ".sid"),
        ("image/x-pict", ".pct"),
        ("image/x-png", ".png"),
        ("image/x-portable-anymap", ".pnm"),
        ("image/x-portable-bitmap", ".pbm"),
        ("image/x-portable-graymap", ".pgm"),
        ("image/x-portable-pixmap", ".ppm"),
        ("image/x-rgb", ".rgb"),
        ("image/x-tga", ".tga"),
        ("image/x-xbitmap", ".xbm"),
        ("image/x-xpixmap", ".xpm"),
        ("image/x-xwindowdump", ".xwd"),
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

    if let Some(result) = lookup(MIME_TYPE_LOOKUP, ext).or_else(|| base_mime_type(ext)) {
        return Some(result);
    }

    // Catch-all for video types that don't require specific mime types. Only
    // reachable for a known video extension neither table above names.
    if !ext.is_empty()
        && VIDEO_FILE_EXTENSIONS
            .iter()
            .any(|v| v.eq_ignore_ascii_case(ext))
    {
        return Some(generic_video_mime(ext));
    }

    default_value
}

/// `Model.MimeTypes.TryGetMimeType`: the package's table, keyed by the
/// extension without its dot. Its keys are lowercase and sorted, so a binary
/// search over the lowercased extension is its case-insensitive lookup.
fn base_mime_type(ext: &str) -> Option<&'static str> {
    let key = ext.strip_prefix('.')?;
    base_type_map::TYPE_MAP
        .binary_search_by(|(k, _)| k.bytes().cmp(key.bytes().map(|b| b.to_ascii_lowercase())))
        .ok()
        .map(|i| base_type_map::TYPE_MAP[i].1)
}

/// Gets the MIME type for `filename`, or [`DEFAULT_MIME_TYPE`] when none is
/// found. Mirrors the single-argument `MimeTypes.GetMimeType(path)`.
#[must_use]
pub fn get_mime_type(filename: &str) -> &'static str {
    get_mime_type_or(filename, Some(DEFAULT_MIME_TYPE)).unwrap_or(DEFAULT_MIME_TYPE)
}

/// Returns the generic `video/<ext-without-dot>` MIME type for a known video
/// extension, mirroring the C# `string.Concat("video/", ext.AsSpan(1))`
/// catch-all, for the [`VIDEO_FILE_EXTENSIONS`] neither table names (the
/// others never reach it), so each maps to a `'static` string.
fn generic_video_mime(ext: &str) -> &'static str {
    const GENERIC: &[(&str, &str)] = &[
        (".divx", "video/divx"),
        (".dvr-ms", "video/dvr-ms"),
        (".m2t", "video/m2t"),
        (".m2ts", "video/m2ts"),
        (".ogm", "video/ogm"),
        (".rec", "video/rec"),
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
    // Base-table image extensions, the types uploads are now stored under.
    #[case(".apng", "image/apng")]
    #[case(".avif", "image/avif")]
    #[case(".dib", "image/bmp")]
    #[case(".heic", "image/heic")]
    #[case(".jpe", "image/jpeg")]
    #[case(".pct", "image/x-pict")]
    #[case(".psd", "image/vnd.adobe.photoshop")]
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
    // The `MimeTypes` 2.5.2 package's table, which `GetMimeType` asks after the
    // override table: its rows, case-insensitively.
    #[case(".aif", "audio/x-aiff")]
    #[case(".aifc", "audio/x-aiff")]
    #[case(".aiff", "audio/x-aiff")]
    #[case(".AIFF", "audio/x-aiff")]
    #[case(".amr", "audio/amr")]
    #[case(".au", "audio/basic")]
    #[case(".dts", "audio/vnd.dts")]
    #[case(".m3u8", "application/vnd.apple.mpegurl")]
    #[case(".m4p", "application/mp4")]
    #[case(".mka", "audio/x-matroska")]
    #[case(".mp2", "audio/mpeg")]
    #[case(".mpc", "application/vnd.mophun.certificate")]
    #[case(".otf", "font/otf")]
    #[case(".pfb", "application/x-font-type1")]
    #[case(".ra", "audio/x-realaudio")]
    #[case(".spx", "audio/ogg")]
    #[case(".ttc", "font/collection")]
    #[case(".weba", "audio/webm")]
    // A video extension the package names wins over the generic `video/<ext>`.
    #[case(".f4v", "video/x-f4v")]
    #[case(".iso", "application/octet-stream")]
    #[case(".m2v", "video/mpeg")]
    #[case(".mk3d", "video/x-matroska")]
    #[case(".mpeg", "video/mpeg")]
    #[case(".mts", "model/vnd.mts")]
    #[case(".rmvb", "application/vnd.rn-realmedia-vbr")]
    #[case(".vob", "video/x-ms-vob")]
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
    #[case("image/jpg", ".jpg")]
    #[case("image/png", ".png")]
    #[case("image/svg+xml", ".svg")]
    #[case("image/tiff", ".tiff")]
    #[case("image/vnd.microsoft.icon", ".ico")]
    #[case("image/webp", ".webp")]
    #[case("image/x-icon", ".ico")]
    #[case("image/x-png", ".png")]
    // Base-table fallback (`GetMimeTypeExtensions(..).FirstOrDefault()`): the
    // first extension the `MimeTypes` 2.5.2 table lists for the type.
    #[case("image/apng", ".apng")]
    #[case("image/avif", ".avif")]
    #[case("image/heic", ".heic")]
    #[case("image/jpm", ".jpgm")]
    #[case("image/jpx", ".jpf")]
    #[case("image/vnd.adobe.photoshop", ".psd")]
    #[case("image/x-pict", ".pct")]
    #[case("IMAGE/AVIF", ".avif")]
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

    #[test]
    fn get_mime_type_branch_cases() {
        // Empty filename returns the supplied default (or DEFAULT_MIME_TYPE).
        assert_eq!(get_mime_type_or("", Some("x/y")), Some("x/y"));
        assert_eq!(get_mime_type(""), DEFAULT_MIME_TYPE);

        // Unknown extension → default; the package has no `.eac3` row either.
        assert_eq!(get_mime_type("file.unknownext"), DEFAULT_MIME_TYPE);
        assert_eq!(get_mime_type_or("file.unknownext", None), None);
        assert_eq!(get_mime_type_or("track.eac3", None), None);
        assert_eq!(get_mime_type_or("file.", None), None);

        // The package's table is keyed on the file name's extension.
        assert_eq!(get_mime_type("/music/a.b/Song.AIFF"), "audio/x-aiff");

        // A known video extension in neither table falls back to the generic
        // `video/<ext>` mapping.
        assert_eq!(get_mime_type("clip.divx"), "video/divx");
        assert_eq!(get_mime_type("show.m2ts"), "video/m2ts");
    }

    /// Every video extension neither table names has its generic
    /// `video/<ext>` entry.
    #[test]
    fn generic_video_fallback_covers_every_unlisted_video_extension() {
        for ext in VIDEO_FILE_EXTENSIONS {
            if lookup(MIME_TYPE_LOOKUP, ext).is_none() && base_mime_type(ext).is_none() {
                assert_eq!(generic_video_mime(ext), format!("video/{}", &ext[1..]));
            }
        }
    }

    /// The binary search in `base_mime_type` needs the package's keys
    /// lowercase and strictly ascending.
    #[test]
    fn base_table_keys_are_lowercase_and_sorted() {
        let table = base_type_map::TYPE_MAP;
        // The package's own row count (`new Dictionary<string, string>(1180, …)`).
        assert_eq!(table.len(), 1180);
        assert!(table.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert!(
            table
                .iter()
                .all(|(k, _)| !k.bytes().any(|b| b.is_ascii_uppercase()))
        );
    }

    /// Every extension `to_extension` hands out for an image type is typed as an
    /// image again by `get_mime_type`, so a file stored under it is never served
    /// as `application/octet-stream`.
    #[test]
    fn image_extensions_are_typed_back_as_images() {
        for (mime_type, extension) in mime_to_extension() {
            if is_image(mime_type) {
                let typed = get_mime_type(extension);
                assert!(is_image(typed), "{mime_type} -> {extension} -> {typed}");
            }
        }
    }

    #[test]
    fn to_extension_and_is_image_branches() {
        // The `;charset` suffix is stripped before lookup.
        assert_eq!(
            to_extension("application/json; charset=UTF-8"),
            Some(".json")
        );
        // Empty and unknown mime types.
        assert_eq!(to_extension(""), None);
        assert_eq!(to_extension("application/does-not-exist"), None);

        assert!(is_image("image/png"));
        assert!(is_image("IMAGE/JPEG"));
        assert!(!is_image("video/mp4"));
        assert!(!is_image("img"));
    }
}
