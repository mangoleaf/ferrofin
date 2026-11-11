//! `ferrofin-server --version` names the release the binary was built from: the
//! tag-derived `ferrofin_health::build_version()`, the value `/health/live`
//! reports as `build`, and not the workspace manifest's placeholder version
//! (gl-1497).

use std::process::Command;

#[test]
fn version_flag_prints_the_build_version_not_the_manifest_placeholder() {
    let out = Command::new(env!("CARGO_BIN_EXE_ferrofin-server"))
        .arg("--version")
        .output()
        .expect("run ferrofin-server --version");
    assert!(out.status.success(), "--version exited with {}", out.status);

    let stdout = String::from_utf8(out.stdout).expect("--version prints UTF-8");
    assert_eq!(
        stdout.trim_end(),
        format!("ferrofin-server {}", ferrofin_health::build_version())
    );
}
