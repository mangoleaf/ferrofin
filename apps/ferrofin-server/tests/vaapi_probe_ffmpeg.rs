//! The VAAPI device probes against the **real** ffmpeg and a real render node.
//!
//! The unit tests in `vaapi_probe` drive a shell stub, so they pin the parsing
//! but say nothing about whether the command line we send ffmpeg is one ffmpeg
//! accepts, or whether the strings we match for are the strings it prints.
//! Those are exactly the two things that cannot be checked without running it.
//!
//! Gated behind `FERROFIN_FFMPEG_TESTS=1` and skipped when there is no ffmpeg
//! or no `/dev/dri` render node, matching the other `*_ffmpeg` tests:
//!
//! ```text
//! FERROFIN_FFMPEG_TESTS=1 cargo test -p ferrofin-server --test vaapi_probe_ffmpeg
//! ```

use std::path::PathBuf;

use ferrofin_mediaencoding::encoding_helper::hw::{FfmpegCapabilities, Platform};
use ferrofin_model::configuration::EncodingOptions;
use ferrofin_model::entities::HardwareAccelerationType;
use ferrofin_server::vaapi_probe::VaapiProber;

/// The first render node present, if any.
fn render_node() -> Option<String> {
    ["/dev/dri/renderD128", "/dev/dri/renderD129"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(str::to_owned)
}

/// `ffmpeg` if it is on `PATH`, resolved the way the other ffmpeg-gated tests
/// do it — by running it, not by looking it up.
fn ffmpeg() -> Option<PathBuf> {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
        .then(|| PathBuf::from("ffmpeg"))
}

#[tokio::test]
async fn the_probe_runs_against_a_real_render_node() {
    if std::env::var("FERROFIN_FFMPEG_TESTS").is_err() {
        eprintln!("skipping: set FERROFIN_FFMPEG_TESTS=1 to run");
        return;
    }
    let (Some(ffmpeg), Some(node)) = (ffmpeg(), render_node()) else {
        eprintln!("skipping: needs ffmpeg and a /dev/dri render node");
        return;
    };

    let base = FfmpegCapabilities::builder()
        .platform(Platform::Linux)
        .hwaccels(["vaapi"])
        .build();
    let prober = VaapiProber::new(ffmpeg, base);
    let caps = prober
        .capabilities(&EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::vaapi,
            vaapi_device: Some(node.clone()),
            ..EncodingOptions::default()
        })
        .await;

    // At most one vendor may be reported. Two would mean the driver names are
    // matching each other's substrings rather than the line ffmpeg prints —
    // which a stub can never tell us, because a stub prints what we chose.
    let vendors = [
        caps.is_vaapi_device_amd(),
        caps.is_vaapi_device_intel_ihd(),
        caps.is_vaapi_device_intel_i965(),
    ];
    assert!(
        vendors.iter().filter(|v| **v).count() <= 1,
        "{node}: more than one VAAPI vendor reported: {vendors:?}"
    );

    // The DMA-BUF set is a superset of nothing in particular, but a device that
    // reports interop must also report the modifier extension — the four
    // dma-buf extensions are only useful alongside it, and a device claiming
    // the reverse would mean the two extension lists got crossed.
    if caps.vaapi_vulkan_drm_interop() {
        assert!(
            caps.vaapi_vulkan_drm_modifier(),
            "{node}: DMA-BUF interop without the DRM format modifier extension"
        );
    }

    eprintln!(
        "{node}: amd={} ihd={} i965={} vulkan_modifier={} vulkan_interop={}",
        caps.is_vaapi_device_amd(),
        caps.is_vaapi_device_intel_ihd(),
        caps.is_vaapi_device_intel_i965(),
        caps.vaapi_vulkan_drm_modifier(),
        caps.vaapi_vulkan_drm_interop(),
    );
}

#[tokio::test]
async fn a_nonexistent_render_node_probes_clean_rather_than_hanging() {
    if std::env::var("FERROFIN_FFMPEG_TESTS").is_err() {
        eprintln!("skipping: set FERROFIN_FFMPEG_TESTS=1 to run");
        return;
    }
    let Some(ffmpeg) = ffmpeg() else {
        eprintln!("skipping: needs ffmpeg");
        return;
    };
    // A misconfigured dashboard path is the common failure, and it must leave
    // every flag clear rather than error out of the plan.
    let prober = VaapiProber::new(
        ffmpeg,
        FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["vaapi"])
            .build(),
    );
    let caps = prober
        .capabilities(&EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::vaapi,
            vaapi_device: Some("/dev/dri/renderD999".to_owned()),
            ..EncodingOptions::default()
        })
        .await;
    assert!(!caps.is_vaapi_device_amd());
    assert!(!caps.is_vaapi_device_intel_ihd());
    assert!(!caps.is_vaapi_device_intel_i965());
    assert!(!caps.vaapi_vulkan_drm_modifier());
    assert!(!caps.vaapi_vulkan_drm_interop());
}
