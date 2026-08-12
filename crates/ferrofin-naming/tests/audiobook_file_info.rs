//! Ported from `AudioBook/AudioBookFileInfoTests.cs`.

use ferrofin_naming::audiobook::AudioBookFileInfo;

fn empty() -> AudioBookFileInfo {
    AudioBookFileInfo::new(String::new(), String::new(), None, None)
}

#[test]
fn compare_to_same_success() {
    let info = empty();
    assert_eq!(info.cmp(&info), std::cmp::Ordering::Equal);
}

#[test]
fn compare_to_empty_success() {
    let info1 = empty();
    let info2 = empty();
    assert_eq!(info1.cmp(&info2), std::cmp::Ordering::Equal);
}
