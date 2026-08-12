//! Ported from `Video/ExtraTests.cs`.

use ferrofin_model::entities::ExtraType;
use ferrofin_naming::common::{MediaType, NamingOptions};
use ferrofin_naming::video::{ExtraRule, ExtraRuleType, extra_rule_resolver};
use rstest::rstest;

fn test(input: &str, expected_type: Option<ExtraType>) {
    let options = NamingOptions::new();
    let extra_type = extra_rule_resolver::get_extra_info(input, &options, None).extra_type;
    assert_eq!(extra_type, expected_type, "for {input}");
}

fn test_with_library_root(input: &str, library_root: &str, expected_type: Option<ExtraType>) {
    let options = NamingOptions::new();
    let extra_type =
        extra_rule_resolver::get_extra_info(input, &options, Some(library_root)).extra_type;
    assert_eq!(extra_type, expected_type, "for {input}");
}

#[test]
fn test_kodi_extras() {
    test("trailer.mp4", Some(ExtraType::Trailer));
    test("300-trailer.mp4", Some(ExtraType::Trailer));
    test("300.trailer.mp4", Some(ExtraType::Trailer));
    test("300_trailer.mp4", Some(ExtraType::Trailer));
    test("300 - trailer.mp4", Some(ExtraType::Trailer));

    test("theme.mp3", Some(ExtraType::ThemeSong));
}

#[test]
fn test_expanded_extras() {
    test("trailer.mp4", Some(ExtraType::Trailer));
    test("trailer.mp3", None);
    test("300-trailer.mp4", Some(ExtraType::Trailer));
    test("stuff trailerthings.mkv", None);

    test("theme.mp3", Some(ExtraType::ThemeSong));
    test("theme.mkv", None);

    test("300-scene.mp4", Some(ExtraType::Scene));
    test("300-scene2.mp4", Some(ExtraType::Scene));
    test("300-clip.mp4", Some(ExtraType::Clip));

    test("300-deleted.mp4", Some(ExtraType::DeletedScene));
    test("300-deletedscene.mp4", Some(ExtraType::DeletedScene));
    test("300-interview.mp4", Some(ExtraType::Interview));
    test("300-behindthescenes.mp4", Some(ExtraType::BehindTheScenes));
    test("300-featurette.mp4", Some(ExtraType::Featurette));
    test("300-short.mp4", Some(ExtraType::Short));
    test("300-extra.mp4", Some(ExtraType::Unknown));
    test("300-other.mp4", Some(ExtraType::Unknown));
}

#[rstest]
#[case(ExtraType::ThemeSong, "theme-music")]
fn test_directories_audio_extras(#[case] typ: ExtraType, #[case] dir_name: &str) {
    test(&format!("{dir_name}/300.mp3"), Some(typ));
    test(&format!("300/{dir_name}/something.mp3"), Some(typ));
    test(
        &format!("/data/something/Movies/300/{dir_name}/whoknows.mp3"),
        Some(typ),
    );
}

#[rstest]
#[case(ExtraType::BehindTheScenes, "behind the scenes")]
#[case(ExtraType::DeletedScene, "deleted scenes")]
#[case(ExtraType::Interview, "interviews")]
#[case(ExtraType::Scene, "scenes")]
#[case(ExtraType::Sample, "samples")]
#[case(ExtraType::Short, "shorts")]
#[case(ExtraType::Trailer, "trailers")]
#[case(ExtraType::Featurette, "featurettes")]
#[case(ExtraType::Clip, "clips")]
#[case(ExtraType::ThemeVideo, "backdrops")]
#[case(ExtraType::Unknown, "extra")]
#[case(ExtraType::Unknown, "extras")]
#[case(ExtraType::Unknown, "other")]
fn test_directories_video_extras(#[case] typ: ExtraType, #[case] dir_name: &str) {
    test(&format!("{dir_name}/300.mp4"), Some(typ));
    test(&format!("300/{dir_name}/something.mkv"), Some(typ));
    test(
        &format!("/data/something/Movies/300/{dir_name}/whoknows.mp4"),
        Some(typ),
    );
}

#[rstest]
#[case("gibberish")]
#[case("not a scene")]
#[case("The Big Short")]
fn test_non_extra_directories(#[case] dir_name: &str) {
    test(&format!("{dir_name}/300.mp4"), None);
    test(&format!("300/{dir_name}/something.mkv"), None);
    test(
        &format!("/data/something/Movies/300/{dir_name}/whoknows.mp4"),
        None,
    );
    test(
        &format!("/data/something/Movies/{dir_name}/{dir_name}.mp4"),
        None,
    );
}

#[rstest]
#[case(ExtraType::ThemeSong, "theme-music")]
fn test_top_level_directories_with_audio_extra_names(
    #[case] typical_type: ExtraType,
    #[case] dir_name: &str,
) {
    let library_root = format!("/data/something/{dir_name}");
    test_with_library_root(&format!("{library_root}/300.mp3"), &library_root, None);
    test_with_library_root(
        &format!("{library_root}/300/{dir_name}/something.mp3"),
        &library_root,
        Some(typical_type),
    );
}

#[rstest]
#[case(ExtraType::Trailer, "trailers")]
#[case(ExtraType::ThemeVideo, "backdrops")]
#[case(ExtraType::BehindTheScenes, "behind the scenes")]
#[case(ExtraType::DeletedScene, "deleted scenes")]
#[case(ExtraType::Interview, "interviews")]
#[case(ExtraType::Scene, "scenes")]
#[case(ExtraType::Sample, "samples")]
#[case(ExtraType::Short, "shorts")]
#[case(ExtraType::Featurette, "featurettes")]
#[case(ExtraType::Unknown, "extras")]
#[case(ExtraType::Unknown, "extra")]
#[case(ExtraType::Unknown, "other")]
#[case(ExtraType::Clip, "clips")]
fn test_top_level_directories_with_video_extra_names(
    #[case] typical_type: ExtraType,
    #[case] dir_name: &str,
) {
    let library_root = format!("/data/something/{dir_name}");
    test_with_library_root(&format!("{library_root}/300.mp4"), &library_root, None);
    test_with_library_root(
        &format!("{library_root}/300/{dir_name}/something.mkv"),
        &library_root,
        Some(typical_type),
    );
}

#[test]
fn test_sample() {
    test("sample.mp4", Some(ExtraType::Sample));
    test("300-sample.mp4", Some(ExtraType::Sample));
    test("300.sample.mp4", Some(ExtraType::Sample));
    test("300_sample.mp4", Some(ExtraType::Sample));
    test("300 - sample.mp4", Some(ExtraType::Sample));
}

#[test]
fn test_suffix_part_of_title() {
    test("I Live In A Trailer.mp4", None);
    test("The DNA Sample.mp4", None);
}

#[test]
fn test_extra_info_invalid_rule_type() {
    let rule = ExtraRule::new(
        ExtraType::Unknown,
        ExtraRuleType::Regex,
        r"([eE]x(tra)?\.\w+)",
        MediaType::Video,
    );
    let mut options = NamingOptions::new();
    options.video_extra_rules = vec![rule.clone()];
    let res = extra_rule_resolver::get_extra_info("extra.mp4", &options, None);

    assert_eq!(res.rule, Some(rule));
}
