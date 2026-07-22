//! Ported from `Video/StubTests.cs`.

use hermit_naming::common::NamingOptions;
use hermit_naming::video::{stub_resolver, video_resolver};

fn test(path: &str, is_stub: bool, stub_type: Option<&str>) {
    let options = NamingOptions::new();
    let result = stub_resolver::try_resolve_file(path, &options);

    assert_eq!(result.is_stub, is_stub, "is_stub mismatch for {path}");

    if is_stub {
        assert_eq!(
            result.stub_type.as_deref(),
            stub_type,
            "stub type for {path}"
        );
    } else {
        assert!(result.stub_type.is_none());
    }
}

#[test]
fn test_stubs() {
    test("video.mkv", false, None);
    test("video.disc", true, None);
    test("video.dvd.disc", true, Some("dvd"));
    test("video.hddvd.disc", true, Some("hddvd"));
    test("video.bluray.disc", true, Some("bluray"));
    test("video.brrip.disc", true, Some("bluray"));
    test("video.bd25.disc", true, Some("bluray"));
    test("video.bd50.disc", true, Some("bluray"));
    test("video.vhs.disc", true, Some("vhs"));
    test("video.hdtv.disc", true, Some("tv"));
    test("video.pdtv.disc", true, Some("tv"));
    test("video.dsr.disc", true, Some("tv"));
    test("", false, Some("tv"));
}

#[test]
fn test_stub_name() {
    let options = NamingOptions::new();
    let result = video_resolver::resolve_file(
        Some("C:/Users/media/Desktop/Video Test/Movies/Oblivion/Oblivion.dvd.disc"),
        &options,
        None,
    );

    assert_eq!(result.map(|r| r.name), Some("Oblivion".to_string()));
}
