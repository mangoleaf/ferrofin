//! Ported from `Video/Format3DTests.cs`.

use hermit_naming::common::NamingOptions;
use hermit_naming::video::{format_3d_parser, video_resolver};

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
