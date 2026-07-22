//! Ported from `TV/SimpleEpisodeTests.cs`.

use hermit_naming::common::NamingOptions;
use hermit_naming::tv::EpisodeResolver;
use rstest::rstest;

fn extension(path: &str) -> &str {
    let name = match path.rfind(['/', '\\']) {
        Some(idx) => &path[idx + 1..],
        None => path,
    };
    match name.rfind('.') {
        Some(idx) => &name[idx..],
        None => "",
    }
}

#[rstest]
#[case("/server/anything_s01e02.mp4", "anything", Some(1), Some(2), None)]
#[case("/server/anything_s1e2.mp4", "anything", Some(1), Some(2), None)]
#[case("/server/anything_s01.e02.mp4", "anything", Some(1), Some(2), None)]
#[case("/server/anything_102.mp4", "anything", Some(1), Some(2), None)]
#[case("/server/anything_1x02.mp4", "anything", Some(1), Some(2), None)]
#[case(
    "/server/The Walking Dead 4x01.mp4",
    "The Walking Dead",
    Some(4),
    Some(1),
    None
)]
#[case(
    "/server/the_simpsons-s02e01_18536.mp4",
    "the_simpsons",
    Some(2),
    Some(1),
    None
)]
#[case("/server/Temp/S01E02 foo.mp4", "", Some(1), Some(2), None)]
#[case("Series/4x12 - The Woman.mp4", "", Some(4), Some(12), None)]
#[case(
    "Series/LA X, Pt. 1_s06e32.mp4",
    "LA X, Pt. 1",
    Some(6),
    Some(32),
    None
)]
#[case(
    "[Baz-Bar]Foo - [1080p][Multiple Subtitle]/[Baz-Bar] Foo - 05 [1080p][Multiple Subtitle].mkv",
    "Foo",
    None,
    Some(5),
    None
)]
#[case(
    "/Foo/The.Series.Name.S01E04.WEBRip.x264-Baz[Bar]/the.series.name.s01e04.webrip.x264-Baz[Bar].mkv",
    "The.Series.Name",
    Some(1),
    Some(4),
    None
)]
#[case(
    "Love.Death.and.Robots.S01.1080p.NF.WEB-DL.DDP5.1.x264-NTG/Love.Death.and.Robots.S01E01.Sonnies.Edge.1080p.NF.WEB-DL.DDP5.1.x264-NTG.mkv",
    "Love.Death.and.Robots",
    Some(1),
    Some(1),
    None
)]
#[case(
    "[YuiSubs] Tensura Nikki - Tensei Shitara Slime Datta Ken/[YuiSubs] Tensura Nikki - Tensei Shitara Slime Datta Ken - 12 (NVENC H.265 1080p).mkv",
    "Tensura Nikki - Tensei Shitara Slime Datta Ken",
    None,
    Some(12),
    None
)]
#[case(
    "[Baz-Bar]Foo - 01 - 12[1080p][Multiple Subtitle]/[Baz-Bar] Foo - 05 [1080p][Multiple Subtitle].mkv",
    "Foo",
    None,
    Some(5),
    None
)]
#[case("Series/4-12 - The Woman.mp4", "", Some(4), Some(12), Some(12))]
fn test_simple(
    #[case] path: &str,
    #[case] series_name: &str,
    #[case] season_number: Option<i32>,
    #[case] episode_number: Option<i32>,
    #[case] episode_end_number: Option<i32>,
) {
    let options = NamingOptions::new();
    let result = EpisodeResolver::new(&options).resolve_simple(path, false);

    let result = result.expect("resolve should succeed");
    assert_eq!(result.season_number, season_number);
    assert_eq!(result.episode_number, episode_number);
    assert!(
        result
            .series_name
            .as_deref()
            .unwrap_or("")
            .eq_ignore_ascii_case(series_name),
        "series name mismatch: got {:?}, want {series_name:?}",
        result.series_name
    );
    assert_eq!(result.path, path);
    assert_eq!(result.container.as_deref(), Some(&extension(path)[1..]));
    assert!(result.format_3d.is_none());
    assert!(!result.is_3d);
    assert!(!result.is_stub);
    assert!(result.stub_type.is_none());
    assert_eq!(result.ending_episode_number, episode_end_number);
    assert!(!result.is_by_date);
}
