//! Ported from `AudioBook/AudioBookResolverTests.cs`.

use ferrofin_naming::audiobook::{AudioBookFileInfo, AudioBookResolver};
use ferrofin_naming::common::NamingOptions;
use rstest::rstest;

#[rstest]
#[case(AudioBookFileInfo::new(
    "/server/AudioBooks/Larry Potter/Larry Potter.mp3",
    "mp3",
    None,
    None
))]
#[case(AudioBookFileInfo::new(
    "/server/AudioBooks/Berry Potter/Chapter 1 .ogg",
    "ogg",
    None,
    Some(1)
))]
#[case(AudioBookFileInfo::new(
    "/server/AudioBooks/Nerry Potter/Part 3 - Chapter 2.mp3",
    "mp3",
    Some(3),
    Some(2)
))]
fn resolve_valid_file_name_success(#[case] expected: AudioBookFileInfo) {
    let options = NamingOptions::new();
    let result = AudioBookResolver::new(&options).resolve(&expected.path);

    let result = result.expect("resolve should succeed");
    assert_eq!(result.path, expected.path);
    assert_eq!(result.container, expected.container);
    assert_eq!(result.chapter_number, expected.chapter_number);
    assert_eq!(result.part_number, expected.part_number);
}

#[test]
fn resolve_invalid_extension() {
    let options = NamingOptions::new();
    let result = AudioBookResolver::new(&options)
        .resolve("/server/AudioBooks/Larry Potter/Larry Potter.mp9");
    assert!(result.is_none());
}

#[test]
fn resolve_empty_file_name() {
    let options = NamingOptions::new();
    let result = AudioBookResolver::new(&options).resolve("");
    assert!(result.is_none());
}
