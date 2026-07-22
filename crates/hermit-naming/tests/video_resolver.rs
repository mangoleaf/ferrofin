//! Ported from `Video/VideoResolverTests.cs`.

use hermit_model::entities::ExtraType;
use hermit_naming::common::NamingOptions;
use hermit_naming::video::{VideoFileInfo, video_resolver};
use rstest::rstest;

#[allow(clippy::too_many_arguments)]
fn vfi(
    path: &str,
    container: &str,
    name: &str,
    year: Option<i32>,
    is_3d: bool,
    format_3d: Option<&str>,
    is_stub: bool,
    stub_type: Option<&str>,
    extra_type: Option<ExtraType>,
) -> VideoFileInfo {
    let mut v = VideoFileInfo::new(name, path);
    v.container = Some(container.to_string());
    v.year = year;
    v.is_3d = is_3d;
    v.format_3d = format_3d.map(str::to_string);
    v.is_stub = is_stub;
    v.stub_type = stub_type.map(str::to_string);
    v.extra_type = extra_type;
    v
}

#[allow(clippy::too_many_arguments)]
#[rstest]
#[case(vfi(
    "/server/Movies/7 Psychos.mkv/7 Psychos.mkv",
    "mkv",
    "7 Psychos",
    None,
    false,
    None,
    false,
    None,
    None
))]
#[case(vfi(
    "/server/Movies/3 days to kill (2005)/3 days to kill (2005).mkv",
    "mkv",
    "3 days to kill",
    Some(2005),
    false,
    None,
    false,
    None,
    None
))]
#[case(vfi(
    "/server/Movies/American Psycho/American.Psycho.mkv",
    "mkv",
    "American.Psycho",
    None,
    false,
    None,
    false,
    None,
    None
))]
#[case(vfi(
    "/server/Movies/brave (2007)/brave (2006).3d.sbs.mkv",
    "mkv",
    "brave",
    Some(2006),
    true,
    Some("sbs"),
    false,
    None,
    None
))]
#[case(vfi(
    "/server/Movies/300 (2007)/300 (2006).3d1.sbas.mkv",
    "mkv",
    "300",
    Some(2006),
    false,
    None,
    false,
    None,
    None
))]
#[case(vfi(
    "/server/Movies/300 (2007)/300 (2006).3d.sbs.mkv",
    "mkv",
    "300",
    Some(2006),
    true,
    Some("sbs"),
    false,
    None,
    None
))]
#[case(vfi(
    "/server/Movies/brave (2007)/brave (2006)-trailer.bluray.disc",
    "disc",
    "brave",
    Some(2006),
    false,
    None,
    true,
    Some("bluray"),
    None
))]
#[case(vfi(
    "/server/Movies/300 (2007)/300 (2006)-trailer.bluray.disc",
    "disc",
    "300",
    Some(2006),
    false,
    None,
    true,
    Some("bluray"),
    None
))]
#[case(vfi(
    "/server/Movies/Brave (2007)/Brave (2006).bluray.disc",
    "disc",
    "Brave",
    Some(2006),
    false,
    None,
    true,
    Some("bluray"),
    None
))]
#[case(vfi(
    "/server/Movies/300 (2007)/300 (2006).bluray.disc",
    "disc",
    "300",
    Some(2006),
    false,
    None,
    true,
    Some("bluray"),
    None
))]
#[case(vfi(
    "/server/Movies/300 (2007)/300 (2006)-trailer.mkv",
    "mkv",
    "300",
    Some(2006),
    false,
    None,
    false,
    None,
    Some(ExtraType::Trailer)
))]
#[case(vfi(
    "/server/Movies/Brave (2007)/Brave (2006)-trailer.mkv",
    "mkv",
    "Brave",
    Some(2006),
    false,
    None,
    false,
    None,
    Some(ExtraType::Trailer)
))]
#[case(vfi(
    "/server/Movies/300 (2007)/300 (2006).mkv",
    "mkv",
    "300",
    Some(2006),
    false,
    None,
    false,
    None,
    None
))]
#[case(vfi(
    "/server/Movies/Bad Boys (1995)/Bad Boys (1995).mkv",
    "mkv",
    "Bad Boys",
    Some(1995),
    false,
    None,
    false,
    None,
    None
))]
#[case(vfi(
    "/server/Movies/Brave (2007)/Brave (2006).mkv",
    "mkv",
    "Brave",
    Some(2006),
    false,
    None,
    false,
    None,
    None
))]
#[case(vfi(
    "/server/Movies/Rain Man 1988 REMASTERED 1080p BluRay x264 AAC - JEFF/Rain Man 1988 REMASTERED 1080p BluRay x264 AAC - JEFF.mp4",
    "mp4",
    "Rain Man",
    Some(1988),
    false,
    None,
    false,
    None,
    None
))]
fn resolve_file_valid_file_name_success(#[case] expected: VideoFileInfo) {
    let options = NamingOptions::new();
    let result = video_resolver::resolve_file(Some(&expected.path), &options, None);

    let result = result.expect("resolve should succeed");
    assert_eq!(result.path, expected.path);
    assert_eq!(result.container, expected.container);
    assert_eq!(result.name, expected.name);
    assert_eq!(result.year, expected.year);
    assert_eq!(result.extra_type, expected.extra_type);
    assert_eq!(result.format_3d, expected.format_3d);
    assert_eq!(result.is_3d, expected.is_3d);
    assert_eq!(result.is_stub, expected.is_stub);
    assert_eq!(result.stub_type, expected.stub_type);
    assert_eq!(result.is_directory, expected.is_directory);
    assert_eq!(
        result.file_name_without_extension(),
        expected.file_name_without_extension()
    );
    assert_eq!(result.to_string(), expected.to_string());
}

#[test]
fn resolve_file_empty_path() {
    let options = NamingOptions::new();
    let result = video_resolver::resolve_file(Some(""), &options, None);
    assert!(result.is_none());
}

#[test]
fn resolve_directory_test() {
    let options = NamingOptions::new();
    let paths = ["/Server/Iron Man", "Batman", ""];
    let results: Vec<_> = paths
        .iter()
        .map(|p| video_resolver::resolve_directory(Some(p), &options, true, None))
        .collect();

    assert_eq!(results.len(), 3);
    assert!(results[0].is_some());
    assert!(results[1].is_some());
    assert!(results[2].is_none());
    for result in &results {
        assert!(result.as_ref().is_none_or(|r| r.container.is_none()));
    }
}
