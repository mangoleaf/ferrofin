//! Ported from `Video/Format3DTests.cs`.

use ferrofin_naming::common::NamingOptions;
use ferrofin_naming::video::{format_3d_parser, video_resolver};

fn test(input: &str, is_3d: bool, format_3d: Option<&str>) {
    let options = NamingOptions::new();
    let result = format_3d_parser::parse(input, &options);

    assert_eq!(result.is_3d, is_3d, "is_3d mismatch for {input}");

    match format_3d {
        None => assert!(result.format_3d.is_none(), "expected no format for {input}"),
        Some(f) => assert!(
            result
                .format_3d
                .as_deref()
                .is_some_and(|r| r.eq_ignore_ascii_case(f)),
            "format mismatch for {input}: got {:?}",
            result.format_3d
        ),
    }
}

#[test]
fn test_kodi_format_3d() {
    test("Super movie.3d.mp4", false, None);
    test("Super movie.3d.hsbs.mp4", true, Some("hsbs"));
    test("Super movie.3d.sbs.mp4", true, Some("sbs"));
    test("Super movie.3d.htab.mp4", true, Some("htab"));
    test("Super movie.3d.tab.mp4", true, Some("tab"));
    test("Super movie 3d hsbs.mp4", true, Some("hsbs"));
}

#[test]
fn test_3d_name() {
    let options = NamingOptions::new();
    let result = video_resolver::resolve_file(
        Some("C:/Users/media/Desktop/Video Test/Movies/Oblivion/Oblivion.3d.hsbs.mkv"),
        &options,
        None,
    );

    assert_eq!(
        result.as_ref().and_then(|r| r.format_3d.as_deref()),
        Some("hsbs")
    );
    assert_eq!(result.as_ref().map(|r| r.name.as_str()), Some("Oblivion"));
}

#[test]
fn test_expanded_format_3d() {
    test("Super movie.3d.mp4", false, None);
    test("Super movie.3d.hsbs.mp4", true, Some("hsbs"));
    test("Super movie.3d.sbs.mp4", true, Some("sbs"));
    test("Super movie.3d.htab.mp4", true, Some("htab"));
    test("Super movie.3d.tab.mp4", true, Some("tab"));

    test("Super movie.hsbs.mp4", true, Some("hsbs"));
    test("Super movie.sbs.mp4", true, Some("sbs"));
    test("Super movie.htab.mp4", true, Some("htab"));
    test("Super movie.tab.mp4", true, Some("tab"));
    test("Super movie.sbs3d.mp4", true, Some("sbs3d"));
    test("Super movie.3d.mvc.mp4", true, Some("mvc"));

    test("Super movie [3d].mp4", false, None);
    test("Super movie [hsbs].mp4", true, Some("hsbs"));
    test("Super movie [fsbs].mp4", true, Some("fsbs"));
    test("Super movie [ftab].mp4", true, Some("ftab"));
    test("Super movie [htab].mp4", true, Some("htab"));
    test("Super movie [sbs3d].mp4", true, Some("sbs3d"));
}

#[test]
fn test_format_3d_at_end_of_path() {
    // Directory based media (eg. DVD or BluRay folder rips) have no file extension,
    // so the 3D tag can be the last token of the path.
    test("Super movie (2009) 3d hsbs", true, Some("hsbs"));
    test("Super movie (2009).3d.sbs", true, Some("sbs"));
    test("Super movie (2009) 3d htab", true, Some("htab"));
    test("Super movie (2009).hsbs", true, Some("hsbs"));
    test("Super movie (2009) 3d", false, None);
}

#[test]
fn test_resolve_directory_3d() {
    let options = NamingOptions::new();
    let result = video_resolver::resolve_directory(
        Some("/movies/Oblivion (2013) 3d hsbs"),
        &options,
        true,
        None,
    );

    assert!(result.as_ref().is_some_and(|r| r.is_3d));
    assert!(
        result
            .as_ref()
            .and_then(|r| r.format_3d.as_deref())
            .is_some_and(|f| f.eq_ignore_ascii_case("hsbs"))
    );
}

/// The final token of a delimiter-free tail is the whole remainder (upstream
/// 5be844e1b7), so a trailing junk character no longer makes a token match.
#[test]
fn trailing_token_is_read_whole() {
    test("Super movie.3d.hsbs", true, Some("hsbs"));
    test("Super movie.3d.hsbsx", false, None);
    test("Super movie.3d.hsbs.", true, Some("hsbs"));
    test("Super movie 3d hsbs", true, Some("hsbs"));
}

/// The token split is shared across every 3D rule, so a rule late in the table
/// must see exactly the tokens an early rule saw.
#[test]
fn every_rule_sees_the_same_tokens() {
    // `mvc` is the last rule in the table; `hsbs` is near the first.
    test("Super movie.3d.mvc.mp4", true, Some("mvc"));
    test("Super movie.3d.hsbs.mp4", true, Some("hsbs"));
    // A preceding-token rule ("3d" + "sbs") and a bare rule ("sbs") differ only
    // in the prefix they demand, over an identical token stream.
    test("Super movie.sbs.mp4", true, Some("sbs"));
    test("Super movie.3d.sbs.mp4", true, Some("sbs"));
}

/// Upstream compares each token with `StringComparison.OrdinalIgnoreCase`; the
/// allocation-free comparison that replaced `String::eq_ignore_ascii_case` has
/// to keep doing that, and nothing else in the ported cases uses a capital.
#[test]
fn token_match_is_case_insensitive() {
    test("Super movie.3D.HSBS.mp4", true, Some("hsbs"));
    test("Super movie.HTAB.mp4", true, Some("htab"));
    test("Super movie [SBS3D].mp4", true, Some("sbs3d"));
    // Non-ASCII is compared exactly, just as ordinal-ignore-case does.
    test("Super movie.HSB\u{df}.mp4", false, None);
}
