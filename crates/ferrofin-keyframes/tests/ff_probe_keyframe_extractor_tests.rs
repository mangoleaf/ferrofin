//! Transliteration of `FfProbeKeyframeExtractorTests` (xUnit) verbatim.
//!
//! `[Theory]` + `[InlineData]` → `#[rstest]` + `#[case]`.
//! `Assert.Equal` → `assert_eq!`. The JSON `*_result.json` fixtures are the oracle.

use std::fs::File;
use std::path::Path;

use ferrofin_keyframes::ff_probe::parse_stream;
use ferrofin_keyframes::keyframe_data::KeyframeData;
use rstest::rstest;

#[rstest]
#[case("keyframes.txt", "keyframes_result.json")]
#[case("keyframes_streamduration.txt", "keyframes_streamduration_result.json")]
fn parse_stream_valid_success(#[case] test_data_file_name: &str, #[case] result_file_name: &str) {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let test_data_path = base.join(test_data_file_name);
    let result_path = base.join(result_file_name);

    let result_file = File::open(&result_path).expect("open result fixture");
    let expected_result: KeyframeData =
        serde_json::from_reader(result_file).expect("deserialize KeyframeData");

    let file = File::open(&test_data_path).expect("open test data fixture");
    let result = parse_stream(file);

    assert_eq!(expected_result.total_duration, result.total_duration);
    assert_eq!(expected_result.keyframe_ticks, result.keyframe_ticks);
}
