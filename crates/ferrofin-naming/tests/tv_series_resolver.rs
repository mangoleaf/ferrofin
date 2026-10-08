//! Ported from `TV/SeriesResolverTests.cs`.

use ferrofin_naming::common::NamingOptions;
use ferrofin_naming::tv::series_resolver;
use rstest::rstest;

#[rstest]
#[case("The.Show.S01", "The Show")]
#[case("The.Show.S01.COMPLETE", "The Show")]
#[case("S.H.O.W.S01", "S.H.O.W")]
#[case("The.Show.P.I.S01", "The Show P.I")]
#[case("The_Show_Season_1", "The Show")]
#[case("/something/The_Show/Season 10", "The Show")]
#[case("The Show", "The Show")]
#[case("/some/path/The Show", "The Show")]
#[case("/some/path/The Show s02e10 720p hdtv", "The Show")]
#[case("/some/path/The Show s02e10 the episode 720p hdtv", "The Show")]
#[case("/some/path/1923 (2022)", "1923")]
// A dotted acronym keeps its dots when it follows words, whether they are space or dot separated
#[case(
    "/some/path/Marvel's Agents of S.H.I.E.L.D.",
    "Marvel's Agents of S.H.I.E.L.D."
)]
#[case("Marvel's.Agents.of.S.H.I.E.L.D.", "Marvel's Agents of S.H.I.E.L.D.")]
#[case("The.Show.S.H.O.W", "The Show S.H.O.W")]
#[case("/some/path/Dawson's Creek", "Dawson's Creek")]
#[case(
    "/media/Bunker.S03.1080p.PULSAR.WEB-DL.DDP5.1.Atmos.H.264-showWEB",
    "Bunker"
)]
#[case(
    "/media/Outer.Colony.S01.1080p.NOVA.WEB-DL.DDP5.1.H.264.HUN.ENG-QUASAR",
    "Outer Colony"
)]
fn series_resolver_resolve_test(#[case] path: &str, #[case] name: &str) {
    let options = NamingOptions::new();
    let res = series_resolver::resolve(&options, path);
    assert_eq!(res.name.as_deref(), Some(name));
}
