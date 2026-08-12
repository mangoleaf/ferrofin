//! Ported from `TV/AbsoluteEpisodeNumberTests.cs`.

use ferrofin_naming::common::NamingOptions;
use ferrofin_naming::tv::EpisodeResolver;
use rstest::rstest;

#[rstest]
#[case("The Simpsons/12.avi", 12)]
#[case("The Simpsons/The Simpsons 12.avi", 12)]
#[case("The Simpsons/The Simpsons 82.avi", 82)]
#[case("The Simpsons/The Simpsons 112.avi", 112)]
#[case("The Simpsons/Foo_ep_02.avi", 2)]
#[case("The Simpsons/The Simpsons 889.avi", 889)]
#[case("The Simpsons/The Simpsons 101.avi", 101)]
fn get_episode_number_from_file_test(#[case] path: &str, #[case] episode_number: i32) {
    let options = NamingOptions::new();
    let result = EpisodeResolver::new(&options).resolve(path, false, None, None, Some(true), true);
    assert_eq!(result.and_then(|r| r.episode_number), Some(episode_number));
}
