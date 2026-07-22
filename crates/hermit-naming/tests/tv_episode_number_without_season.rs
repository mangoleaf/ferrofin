//! Ported from `TV/EpisodeNumberWithoutSeasonTests.cs`.

use hermit_naming::common::NamingOptions;
use hermit_naming::tv::EpisodeResolver;
use rstest::rstest;

#[rstest]
#[case(8, "The Simpsons/The Simpsons.S25E08.Steal this episode.mp4")]
#[case(2, "The Simpsons/The Simpsons - 02 - Ep Name.avi")]
#[case(2, "The Simpsons/02.avi")]
#[case(2, "The Simpsons/02 - Ep Name.avi")]
#[case(2, "The Simpsons/02-Ep Name.avi")]
#[case(2, "The Simpsons/02.EpName.avi")]
#[case(2, "The Simpsons/The Simpsons - 02.avi")]
#[case(2, "The Simpsons/The Simpsons - 02 Ep Name.avi")]
#[case(7, "GJ Club (2013)/GJ Club - 07.mkv")]
#[case(317, "Case Closed (1996-2007)/Case Closed - 317.mkv")]
fn get_episode_number_from_file_test(#[case] episode_number: i32, #[case] path: &str) {
    let options = NamingOptions::new();
    let result = EpisodeResolver::new(&options).resolve_simple(path, false);
    assert_eq!(result.and_then(|r| r.episode_number), Some(episode_number));
}
