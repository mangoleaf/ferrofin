//! Ported from `Video/CleanStringTests.cs`.

use ferrofin_naming::common::NamingOptions;
use ferrofin_naming::video::video_resolver;
use rstest::rstest;

#[rstest]
#[case("Super movie 480p.mp4", "Super movie")]
#[case("Super movie Multi.mp4", "Super movie")]
#[case("Super movie 480p 2001.mp4", "Super movie")]
#[case("Super movie [480p].mp4", "Super movie")]
#[case("480 Super movie [tmdbid=12345].mp4", "480 Super movie")]
#[case(
    "Crouching.Tiger.Hidden.Dragon.4k.mkv",
    "Crouching.Tiger.Hidden.Dragon"
)]
#[case(
    "Crouching.Tiger.Hidden.Dragon.UltraHD.mkv",
    "Crouching.Tiger.Hidden.Dragon"
)]
#[case(
    "Crouching.Tiger.Hidden.Dragon.UHD.mkv",
    "Crouching.Tiger.Hidden.Dragon"
)]
#[case(
    "Crouching.Tiger.Hidden.Dragon.HDR.mkv",
    "Crouching.Tiger.Hidden.Dragon"
)]
#[case(
    "Crouching.Tiger.Hidden.Dragon.HDC.mkv",
    "Crouching.Tiger.Hidden.Dragon"
)]
#[case(
    "Crouching.Tiger.Hidden.Dragon-HDC.mkv",
    "Crouching.Tiger.Hidden.Dragon"
)]
#[case(
    "Crouching.Tiger.Hidden.Dragon.BDrip.mkv",
    "Crouching.Tiger.Hidden.Dragon"
)]
#[case(
    "Crouching.Tiger.Hidden.Dragon.BDrip-HDC.mkv",
    "Crouching.Tiger.Hidden.Dragon"
)]
#[case(
    "Crouching.Tiger.Hidden.Dragon.4K.UltraHD.HDR.BDrip-HDC.mkv",
    "Crouching.Tiger.Hidden.Dragon"
)]
#[case("[HorribleSubs] Made in Abyss - 13 [720p].mkv", "Made in Abyss")]
#[case(
    "[Tsundere] Kore wa Zombie Desu ka of the Dead [BDRip h264 1920x1080 FLAC]",
    "Kore wa Zombie Desu ka of the Dead"
)]
#[case(
    "[Erai-raws] Jujutsu Kaisen - 03 [720p][Multiple Subtitle].mkv",
    "Jujutsu Kaisen"
)]
#[case("[OCN] 애타는 로맨스 720p-NEXT", "애타는 로맨스")]
#[case("[tvN] 혼술남녀.E01-E16.720p-NEXT", "혼술남녀")]
#[case("[tvN] 연애말고 결혼 E01~E16 END HDTV.H264.720p-WITH", "연애말고 결혼")]
#[case(
    "2026年01月10日23時00分00秒-[新]TRIGUN　STARGAZE[字].mp4",
    "2026年01月10日23時00分00秒-[新]TRIGUN　STARGAZE"
)]
fn clean_string_test_needs_cleaning_success(#[case] input: &str, #[case] expected_name: &str) {
    let new_name = video_resolver::try_clean_string(Some(input), &NamingOptions::new());
    assert_eq!(new_name.as_deref(), Some(expected_name));
}

#[rstest]
#[case(None)]
#[case(Some(""))]
#[case(Some("Super movie(2009).mp4"))]
#[case(Some("[rec].mkv"))]
#[case(Some("American.Psycho.mkv"))]
#[case(Some("American Psycho.mkv"))]
#[case(Some("Run lola run (lola rennt) (2009).mp4"))]
#[case(Some("2026年01月05日00時55分00秒-[新]違国日記【ＡＮｉＭｉＤＮｉＧＨＴ！！！】＃１.mp4"))]
fn clean_string_test_doesnt_need_cleaning_false(#[case] input: Option<&str>) {
    let new_name = video_resolver::try_clean_string(input, &NamingOptions::new());
    assert!(new_name.is_none());
}
