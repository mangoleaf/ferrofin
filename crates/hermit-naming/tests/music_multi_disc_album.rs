//! Ported from `Music/MultiDiscAlbumTests.cs`.

use hermit_naming::audio::AlbumParser;
use hermit_naming::common::NamingOptions;
use rstest::rstest;

#[rstest]
#[case("", false)]
#[case("C:/", false)]
#[case("/home/", false)]
#[case("blah blah", false)]
#[case("D:/music/weezer/03 Pinkerton", false)]
#[case("D:/music/michael jackson/Bad (2012 Remaster)", false)]
#[case("cd1", true)]
#[case("disc18", true)]
#[case("disk10", true)]
#[case("vol7", true)]
#[case("volume1", true)]
#[case("cd 1", true)]
#[case("disc 1", true)]
#[case("disk 1", true)]
#[case("disk", false)]
#[case("disk ·", false)]
#[case("disk a", false)]
#[case("disk volume", false)]
#[case("disc disc", false)]
#[case("disk disc 6", false)]
#[case("cd  - 1", true)]
#[case("disc- 1", true)]
#[case("disk - 1", true)]
#[case("Disc 01 (Hugo Wolf · 24 Lieder)", true)]
#[case("Disc 04 (Encores and Folk Songs)", true)]
#[case("Disc04 (Encores and Folk Songs)", true)]
#[case("Disc 04(Encores and Folk Songs)", true)]
#[case("Disc04(Encores and Folk Songs)", true)]
#[case("D:/Video/MBTestLibrary/VideoTest/music/.38 special/anth/Disc 2", true)]
#[case("[1985] Opportunities (Let's make lots of money) (1985)", false)]
#[case("Blah 04(Encores and Folk Songs)", false)]
fn album_parser_multidisc_path_identifies(#[case] path: &str, #[case] result: bool) {
    let options = NamingOptions::new();
    let parser = AlbumParser::new(&options);
    assert_eq!(parser.is_multi_part(path), result, "for {path}");
}
