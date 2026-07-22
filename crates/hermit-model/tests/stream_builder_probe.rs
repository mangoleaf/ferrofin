//! Probe: verify all fixtures deserialize with the ported serde types.

use std::path::Path;

use hermit_model::dlna::DeviceProfile;
use hermit_model::dto::MediaSourceInfo;

fn data_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data"))
}

#[test]
fn all_device_profiles_deserialize() {
    for entry in std::fs::read_dir(data_dir()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap();
        if !name.starts_with("DeviceProfile-") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let _: DeviceProfile =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

#[test]
fn all_media_sources_deserialize() {
    for entry in std::fs::read_dir(data_dir()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap();
        if !name.starts_with("MediaSourceInfo-") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let _: MediaSourceInfo =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}
