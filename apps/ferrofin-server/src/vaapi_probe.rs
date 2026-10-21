//! The VAAPI device probes, run lazily and cached per render node.
//!
//! Port of the `// Check the Vaapi device vendor` block of C#
//! `MediaEncoder.SetFFmpegPath` (10.11.z 236-248) and the two spawns it calls,
//! `EncoderValidator.CheckVaapiDeviceByDriverName` and
//! `CheckVulkanDrmDeviceByExtensionName`.
//!
//! **Why these are not in [`crate::bootstrap`] with every other probe.** They
//! need `EncodingOptions.vaapi_device`, which lives in the encoding
//! configuration store — unread at `discover_ffmpeg` time — and which the
//! dashboard can change while the server runs.
//!
//! Upstream probes **once, at startup**: `SetFFmpegPath` has exactly one
//! caller, in `ApplicationHost.RunStartupTasksAsync`, and the configuration
//! -updated handler beside it never touches the encoder. Changing the VAAPI
//! device in Jellyfin's dashboard therefore needs a server restart. Ferrofin
//! probes on first use of a given device path and caches by that path, so a
//! dashboard change takes effect without one. That is a deliberate
//! **improvement**, not parity — do not "restore" it to a boot-time probe.
//!
//! What comes back is not a bag of flags but a whole [`FfmpegCapabilities`]
//! with the five VAAPI fields filled in. The filter chains all read
//! capabilities, so overlaying there keeps one type flowing through them, and
//! caching the overlaid value means the base capability lists are cloned once
//! per distinct device path rather than once per job.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use ferrofin_mediaencoding::encoder::{
    EncoderValidator, VULKAN_EXTERNAL_MEMORY_DMA_BUF_EXTS, VULKAN_IMAGE_DRM_FMT_MODIFIER_EXTS,
};
use ferrofin_mediaencoding::encoding_helper::hw::FfmpegCapabilities;
use ferrofin_model::configuration::EncodingOptions;
use ferrofin_model::entities::HardwareAccelerationType;
use std::sync::Mutex;

/// The driver names `CheckVaapiDeviceByDriverName` looks for, in upstream's
/// order. The strings are matched against ffmpeg's stderr verbatim.
const AMD_DRIVER: &str = "Mesa Gallium driver";
/// The modern Intel media driver.
const INTEL_IHD_DRIVER: &str = "Intel iHD driver";
/// The legacy Intel media driver, still shipped for pre-Broadwell parts.
const INTEL_I965_DRIVER: &str = "Intel i965 driver";
/// The line ffmpeg prints once it has opened a VAAPI device. Its presence is
/// what separates "an unrecognised vendor" from "the device never opened".
const VAAPI_DRIVER_MARKER: &str = "VAAPI driver:";

/// Resolves VAAPI device capabilities on demand, caching by render-node path.
pub struct VaapiProber {
    ffmpeg: PathBuf,
    /// Behind an `Arc` because the guard-fail path — every non-VAAPI server,
    /// on every call — returns it untouched, and a real ffmpeg's capability
    /// lists run to a few thousand strings.
    base: Arc<FfmpegCapabilities>,
    /// Keyed by render-node path. A path probed once keeps its answer for the
    /// process's life; a *different* path probes afresh, which is what makes a
    /// dashboard change take effect without a restart.
    cache: Mutex<HashMap<String, Probed>>,
}

/// One cached probe answer.
struct Probed {
    caps: Arc<FfmpegCapabilities>,
    valid: Validity,
}

/// How long a cached probe answer stands.
#[derive(Clone, Copy)]
enum Validity {
    /// A conclusive answer, kept for the process's life.
    Forever,
    /// An inconclusive answer, kept while the node is in the [`node_state`]
    /// it was probed against.
    UntilNodeChanges(Option<(i64, i64)>),
}

impl Validity {
    fn holds(self, node: &str) -> bool {
        match self {
            Validity::Forever => true,
            Validity::UntilNodeChanges(probed_against) => probed_against == node_state(node),
        }
    }
}

/// What the VAAPI spawn found.
enum Answer {
    /// ffmpeg opened the device and named its driver.
    Driver,
    /// ffmpeg ran but opened no device.
    NoDevice,
    /// ffmpeg could not be spawned at all.
    SpawnFailed,
}

/// Whether the render node exists and when its inode last changed. `ctime`
/// moves on a chmod or chown and on re-creation, which are the fixes for an
/// unopenable node (a container device mapped later starts a new process).
fn node_state(node: &str) -> Option<(i64, i64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(node)
            .ok()
            .map(|meta| (meta.ctime(), meta.ctime_nsec()))
    }
    // Never probed off Linux (the platform guard in `capabilities`).
    #[cfg(not(unix))]
    {
        let _ = node;
        None
    }
}

impl VaapiProber {
    /// Creates a prober over the discovered ffmpeg and the boot capabilities.
    #[must_use]
    pub fn new(ffmpeg: PathBuf, base: FfmpegCapabilities) -> Self {
        Self {
            ffmpeg,
            base: Arc::new(base),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The capabilities to plan with, given the current encoding options.
    ///
    /// Returns the unmodified boot capabilities unless every one of upstream's
    /// four guards passes: Linux, ffmpeg has the `vaapi` hwaccel, the
    /// configured render node is non-empty, and VAAPI is the *selected*
    /// accelerator. The last is not an optimisation — probing a device the
    /// operator did not choose would spawn ffmpeg twice on a machine that
    /// never uses VAAPI.
    ///
    /// An empty render-node path is refused rather than defaulted, matching
    /// upstream: `-init_hw_device vaapi=va:` probes ffmpeg's *own* default
    /// device, which could report a driver for hardware nobody configured.
    pub async fn capabilities(&self, options: &EncodingOptions) -> Arc<FfmpegCapabilities> {
        let node = options.vaapi_device.as_deref().unwrap_or_default();
        if !self.base.platform().is_linux()
            || !self.base.supports_hwaccel("vaapi")
            || node.is_empty()
            || options.hardware_acceleration_type != HardwareAccelerationType::vaapi
        {
            return Arc::clone(&self.base);
        }

        let hit = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(node)
            .map(|hit| (Arc::clone(&hit.caps), hit.valid));
        // The node is stat'ed outside the lock.
        if let Some((caps, valid)) = hit
            && valid.holds(node)
        {
            return caps;
        }

        // The lock is deliberately NOT held across the probe. Holding it would
        // serialise every VAAPI lookup behind one ffmpeg spawn; two concurrent
        // first uses of the same path probing twice is idempotent and wasted
        // once.
        //
        // The node's state is read before the spawn, so a fix landing during
        // the probe still invalidates the answer.
        let state = node_state(node);
        let (probed, answer) = probe(&self.ffmpeg, &self.base, node).await;
        let probed = Arc::new(probed);
        // A probe that opened no device is cached only until the node changes.
        // The usual cause is a render node the server user cannot read yet,
        // fixed by a chmod or chown that moves the node's ctime, so that fix
        // still needs no restart. A cause that leaves the node alone (a VA
        // driver installed while the server runs) waits for a restart or a
        // device-path change; upstream needs a restart for every case. Not
        // caching it at all re-spawned both probes and re-logged the WARN on
        // every plan, i.e. on every HLS segment request. A spawn that failed
        // says nothing about the node and stays uncached.
        let valid = match answer {
            Answer::Driver => Some(Validity::Forever),
            Answer::NoDevice => Some(Validity::UntilNodeChanges(state)),
            Answer::SpawnFailed => None,
        };
        if let Some(valid) = valid {
            self.cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    node.to_owned(),
                    Probed {
                        caps: Arc::clone(&probed),
                        valid,
                    },
                );
        }
        probed
    }
}

/// Runs the five checks across two ffmpeg spawns and returns `base` with their
/// answers overlaid, plus what the VAAPI spawn found.
///
/// Only [`Answer::Driver`] — ffmpeg printed its `VAAPI driver:` line — is
/// durable. A spawn that opened nothing, or failed, tells us nothing lasting
/// about the hardware.
async fn probe(
    ffmpeg: &Path,
    base: &FfmpegCapabilities,
    node: &str,
) -> (FfmpegCapabilities, Answer) {
    // Five checks, two spawns: upstream spawns once per driver name and once
    // per extension list, but within each pair the command is byte-identical
    // (the name and the list are only consumed after the spawn), so identical
    // commands cannot disagree.
    let vaapi_device = format!("vaapi=va:{node}");
    let drm_device = format!("drm=dr:{node}");
    let vaapi_args = ["-init_hw_device", vaapi_device.as_str()];
    let vulkan_args = [
        "-init_hw_device",
        drm_device.as_str(),
        "-init_hw_device",
        "vulkan=vk@dr",
    ];
    let (vaapi_stderr, vulkan_stderr) = tokio::join!(
        stderr_of(ffmpeg, &vaapi_args),
        stderr_of(ffmpeg, &vulkan_args),
    );
    // ffmpeg names the driver it opened on this line; its absence means the
    // device was never reached.
    let answer = match &vaapi_stderr {
        None => Answer::SpawnFailed,
        Some(stderr) if stderr.contains(VAAPI_DRIVER_MARKER) => Answer::Driver,
        Some(_) => Answer::NoDevice,
    };
    let conclusive = matches!(answer, Answer::Driver);
    // A spawn failure makes every check answer `false`, the same outcome as
    // C#'s catch-and-log.
    let vaapi_stderr = vaapi_stderr.unwrap_or_default();
    let vulkan_stderr = vulkan_stderr.unwrap_or_default();

    let amd = EncoderValidator::check_vaapi_driver_internal(&vaapi_stderr, AMD_DRIVER);
    let intel_ihd = EncoderValidator::check_vaapi_driver_internal(&vaapi_stderr, INTEL_IHD_DRIVER);
    let intel_i965 =
        EncoderValidator::check_vaapi_driver_internal(&vaapi_stderr, INTEL_I965_DRIVER);
    let drm_modifier = EncoderValidator::check_vulkan_extensions_internal(
        &vulkan_stderr,
        &VULKAN_IMAGE_DRM_FMT_MODIFIER_EXTS,
    );
    let drm_interop = EncoderValidator::check_vulkan_extensions_internal(
        &vulkan_stderr,
        &VULKAN_EXTERNAL_MEMORY_DMA_BUF_EXTS,
    );

    if amd {
        tracing::info!(render_node = node, "VAAPI device is an AMD GPU");
    } else if intel_ihd {
        tracing::info!(render_node = node, "VAAPI device is an Intel GPU (iHD)");
    } else if intel_i965 {
        tracing::info!(render_node = node, "VAAPI device is an Intel GPU (i965)");
    } else if conclusive {
        // A driver was found, just not one of the three upstream knows. Real
        // hardware lands here — NVIDIA's VAAPI shim reports "VA-API NVDEC
        // driver" — so this is information, not a problem: the chains simply
        // take their most conservative branch.
        tracing::info!(
            render_node = node,
            "VAAPI device vendor not recognised; using the limited filter chain"
        );
    } else {
        // No driver line at all: the device was never opened. That is usually
        // fixable, and it is worth saying so, because the resulting
        // no-hardware answer is indistinguishable from slow hardware.
        tracing::warn!(
            render_node = node,
            "VAAPI device could not be opened; hardware transcoding will not be \
             used. Check the render node exists and is readable by the server."
        );
    }
    if drm_modifier {
        tracing::info!(
            render_node = node,
            "VAAPI device supports Vulkan DRM modifier"
        );
    }
    if drm_interop {
        tracing::info!(
            render_node = node,
            "VAAPI device supports Vulkan DRM interop"
        );
    }

    let caps = base
        .clone()
        .with_vaapi_driver(amd, intel_ihd, intel_i965)
        .with_vaapi_vulkan(drm_modifier, drm_interop);
    (caps, answer)
}

/// Runs ffmpeg with the verbose flags the probes need and returns its stderr.
///
/// Upstream reads stderr because `-init_hw_device` reports what it opened on
/// the log stream, not on stdout. A spawn failure yields `None`.
async fn stderr_of(ffmpeg: &Path, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new(ffmpeg)
        .args(["-v", "verbose", "-hide_banner"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await;
    match output {
        Ok(output) => Some(String::from_utf8_lossy(&output.stderr).into_owned()),
        Err(e) => {
            // C# logs this at error level; a probe that cannot even spawn is
            // not a routine miss, and at debug the vulkan half would be
            // completely silent.
            tracing::warn!(error = %e, "error probing the VAAPI render node");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_exec::stub_script;
    use ferrofin_mediaencoding::encoding_helper::hw::Platform;

    fn linux_vaapi_caps() -> FfmpegCapabilities {
        FfmpegCapabilities::builder()
            .platform(Platform::Linux)
            .hwaccels(["vaapi"])
            .build()
    }

    fn vaapi_options(node: Option<&str>) -> EncodingOptions {
        EncodingOptions {
            hardware_acceleration_type: HardwareAccelerationType::vaapi,
            vaapi_device: node.map(str::to_owned),
            ..EncodingOptions::default()
        }
    }

    /// A stub that echoes `text` on stderr and records its argv, standing in
    /// for ffmpeg. Recording the argv is the point: a stub that ignores its
    /// arguments cannot tell us the command we send ffmpeg is one ffmpeg
    /// understands, and every assertion about the parsed result stays green
    /// while the probe asks for a device that does not exist.
    fn fake_ffmpeg(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("fake-ffmpeg");
        // One file per spawn, named by pid: the two probes run concurrently,
        // so appending to a shared file interleaves them.
        let argv_dir = dir.join("argv");
        std::fs::create_dir_all(&argv_dir).unwrap();
        stub_script(
            &path,
            &format!(
                "printf '%s\\n' \"$@\" > {}/$$\ncat >&2 <<'EOF'\n{text}\nEOF\n",
                argv_dir.display()
            ),
        );
        path
    }

    /// The argument vectors the stub was invoked with, one per spawn.
    fn recorded_argv(dir: &Path) -> Vec<Vec<String>> {
        let Ok(entries) = std::fs::read_dir(dir.join("argv")) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .map(|run| run.lines().map(str::to_owned).collect())
            .collect()
    }

    #[tokio::test]
    async fn the_probe_sends_the_command_lines_upstream_sends() {
        // Derived from C# `EncoderValidator.CheckVaapiDeviceByDriverName` and
        // `CheckVulkanDrmDeviceByExtensionName`, which build
        // `"-v verbose -hide_banner -init_hw_device vaapi=va:" + renderNodePath`
        // and `... -init_hw_device drm=dr:<node> -init_hw_device vulkan=vk@dr`.
        //
        // `-v verbose` is load-bearing, not decoration: without it ffmpeg
        // prints no driver line at all and every flag would read false on
        // hardware that has them.
        let dir = tempfile::tempdir().unwrap();
        let ffmpeg = fake_ffmpeg(dir.path(), "VAAPI driver: Intel iHD driver");
        let prober = VaapiProber::new(ffmpeg, linux_vaapi_caps());
        let _ = prober
            .capabilities(&vaapi_options(Some("/dev/dri/renderD128")))
            .await;

        let mut runs = recorded_argv(dir.path());
        runs.sort();
        assert_eq!(
            runs,
            vec![
                vec![
                    "-v",
                    "verbose",
                    "-hide_banner",
                    "-init_hw_device",
                    "drm=dr:/dev/dri/renderD128",
                    "-init_hw_device",
                    "vulkan=vk@dr",
                ],
                vec![
                    "-v",
                    "verbose",
                    "-hide_banner",
                    "-init_hw_device",
                    "vaapi=va:/dev/dri/renderD128",
                ],
            ]
        );
    }

    #[tokio::test]
    async fn a_probe_reads_the_driver_name_out_of_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let ffmpeg = fake_ffmpeg(
            dir.path(),
            "[AVHWDeviceContext] Opened VA display via DRM device /dev/dri/renderD128.\n\
             [AVHWDeviceContext] Initialised VAAPI connection: version 1.20\n\
             [AVHWDeviceContext] VAAPI driver: Intel iHD driver for Intel(R) Gen Graphics - 24.1.0.\n\
             [AVHWDeviceContext] Driver not found in known nonstandard list, using standard behaviour.",
        );
        let prober = VaapiProber::new(ffmpeg, linux_vaapi_caps());
        let caps = prober
            .capabilities(&vaapi_options(Some("/dev/dri/renderD128")))
            .await;
        assert!(caps.is_vaapi_device_intel_ihd());
        assert!(!caps.is_vaapi_device_amd());
        assert!(!caps.is_vaapi_device_intel_i965());
    }

    #[tokio::test]
    async fn an_amd_device_is_recognised_by_its_mesa_driver() {
        let dir = tempfile::tempdir().unwrap();
        let ffmpeg = fake_ffmpeg(
            dir.path(),
            "[AVHWDeviceContext] VAAPI driver: Mesa Gallium driver 24.0.3 for AMD Radeon Graphics.",
        );
        let prober = VaapiProber::new(ffmpeg, linux_vaapi_caps());
        let caps = prober
            .capabilities(&vaapi_options(Some("/dev/dri/renderD128")))
            .await;
        assert!(caps.is_vaapi_device_amd());
        assert!(!caps.is_vaapi_device_intel_ihd());
    }

    #[tokio::test]
    async fn vulkan_extensions_must_all_be_present() {
        let dir = tempfile::tempdir().unwrap();
        // Only the modifier extension; the dma-buf set needs all four.
        let ffmpeg = fake_ffmpeg(
            dir.path(),
            "[AVHWDeviceContext] Supported device extensions: VK_EXT_image_drm_format_modifier",
        );
        let prober = VaapiProber::new(ffmpeg, linux_vaapi_caps());
        let caps = prober
            .capabilities(&vaapi_options(Some("/dev/dri/renderD128")))
            .await;
        assert!(caps.vaapi_vulkan_drm_modifier());
        assert!(!caps.vaapi_vulkan_drm_interop());
    }

    #[tokio::test]
    async fn an_unrecognised_device_leaves_every_flag_clear() {
        let dir = tempfile::tempdir().unwrap();
        let ffmpeg = fake_ffmpeg(dir.path(), "[AVHWDeviceContext] Failed to open display.");
        let prober = VaapiProber::new(ffmpeg, linux_vaapi_caps());
        let caps = prober
            .capabilities(&vaapi_options(Some("/dev/dri/renderD128")))
            .await;
        assert!(!caps.is_vaapi_device_amd());
        assert!(!caps.is_vaapi_device_intel_ihd());
        assert!(!caps.is_vaapi_device_intel_i965());
        assert!(!caps.vaapi_vulkan_drm_modifier());
        assert!(!caps.vaapi_vulkan_drm_interop());
    }

    #[tokio::test]
    async fn each_guard_skips_the_probe_entirely() {
        let dir = tempfile::tempdir().unwrap();
        // A stub that would report iHD if it ever ran.
        let ffmpeg = fake_ffmpeg(dir.path(), "VAAPI driver: Intel iHD driver");

        // No render node configured — probing would ask ffmpeg's default
        // device and could name hardware nobody chose.
        let prober = VaapiProber::new(ffmpeg.clone(), linux_vaapi_caps());
        assert!(
            !prober
                .capabilities(&vaapi_options(None))
                .await
                .is_vaapi_device_intel_ihd()
        );
        assert!(
            !prober
                .capabilities(&vaapi_options(Some("")))
                .await
                .is_vaapi_device_intel_ihd()
        );

        // A different accelerator is selected: five spawns for a device this
        // server will never use.
        let mut other = vaapi_options(Some("/dev/dri/renderD128"));
        other.hardware_acceleration_type = HardwareAccelerationType::nvenc;
        assert!(
            !prober
                .capabilities(&other)
                .await
                .is_vaapi_device_intel_ihd()
        );

        // ffmpeg has no vaapi hwaccel at all.
        let no_vaapi = VaapiProber::new(
            ffmpeg.clone(),
            FfmpegCapabilities::builder()
                .platform(Platform::Linux)
                .build(),
        );
        assert!(
            !no_vaapi
                .capabilities(&vaapi_options(Some("/dev/dri/renderD128")))
                .await
                .is_vaapi_device_intel_ihd()
        );

        // ...and a non-Linux host, where VAAPI does not exist.
        let not_linux = VaapiProber::new(
            ffmpeg,
            FfmpegCapabilities::builder()
                .platform(Platform::Windows)
                .hwaccels(["vaapi"])
                .build(),
        );
        assert!(
            !not_linux
                .capabilities(&vaapi_options(Some("/dev/dri/renderD128")))
                .await
                .is_vaapi_device_intel_ihd()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_probe_that_never_reached_the_device_is_retried_once_the_node_changes() {
        // The failure that matters: a render node the server user cannot read
        // yet. Pinning "no hardware" for the process lifetime would mean fixing
        // the permissions needs a restart — the very thing this cache avoids —
        // but re-probing on every plan spawned two ffmpegs and a WARN per HLS
        // segment request.
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let ffmpeg = fake_ffmpeg(
            dir.path(),
            "[AVHWDeviceContext] Failed to open the DRM device.",
        );
        let node = dir.path().join("renderD128");
        std::fs::write(&node, "").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o600)).unwrap();
        let prober = VaapiProber::new(ffmpeg, linux_vaapi_caps());
        let opts = vaapi_options(node.to_str());

        assert!(!prober.capabilities(&opts).await.is_vaapi_device_intel_ihd());
        let after_first = recorded_argv(dir.path()).len();
        assert_eq!(after_first, 2, "two spawns per probe");

        // An unchanged node is served the failure without another spawn.
        let _ = prober.capabilities(&opts).await;
        assert_eq!(
            recorded_argv(dir.path()).len(),
            after_first,
            "an unchanged node must not be probed again"
        );

        // The operator's fix (a chmod moves the node's ctime) re-probes. The
        // pause keeps the chmod off the creation's timestamp tick.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let before = node_state(node.to_str().unwrap());
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert_ne!(
            node_state(node.to_str().unwrap()),
            before,
            "the chmod moved ctime"
        );
        let _ = prober.capabilities(&opts).await;
        assert_eq!(
            recorded_argv(dir.path()).len(),
            after_first * 2,
            "a changed node must be probed again"
        );
    }

    #[tokio::test]
    async fn a_probe_that_could_not_spawn_is_not_cached() {
        // A failed spawn says nothing about the node, so nothing pins it.
        let dir = tempfile::tempdir().unwrap();
        let prober = VaapiProber::new(dir.path().join("no-such-ffmpeg"), linux_vaapi_caps());
        let _ = prober
            .capabilities(&vaapi_options(Some("/dev/dri/renderD128")))
            .await;
        assert!(prober.cache.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_missing_node_is_retried_once_it_appears() {
        let dir = tempfile::tempdir().unwrap();
        let ffmpeg = fake_ffmpeg(dir.path(), "No VA display found for device");
        let node = dir.path().join("renderD128");
        let prober = VaapiProber::new(ffmpeg, linux_vaapi_caps());
        let opts = vaapi_options(node.to_str());

        let _ = prober.capabilities(&opts).await;
        let _ = prober.capabilities(&opts).await;
        assert_eq!(
            recorded_argv(dir.path()).len(),
            2,
            "probed once while absent"
        );

        std::fs::write(&node, "").unwrap();
        let _ = prober.capabilities(&opts).await;
        assert_eq!(
            recorded_argv(dir.path()).len(),
            4,
            "probed again once present"
        );
    }

    #[tokio::test]
    async fn a_probed_path_is_not_probed_again_but_a_new_one_is() {
        let dir = tempfile::tempdir().unwrap();
        // Appends a marker per run, so the file's length counts the spawns.
        let counter = dir.path().join("runs");
        let ffmpeg = dir.path().join("counting-ffmpeg");
        stub_script(
            &ffmpeg,
            &format!(
                "echo x >> {}\necho 'VAAPI driver: Intel iHD driver' >&2\n",
                counter.display()
            ),
        );
        let runs = || std::fs::read_to_string(&counter).map_or(0, |s| s.lines().count());

        let prober = VaapiProber::new(ffmpeg, linux_vaapi_caps());
        let opts = vaapi_options(Some("/dev/dri/renderD128"));
        assert!(prober.capabilities(&opts).await.is_vaapi_device_intel_ihd());
        let after_first = runs();
        assert!(after_first > 0, "the probe should have spawned ffmpeg");

        // Same path again: served from the cache.
        assert!(prober.capabilities(&opts).await.is_vaapi_device_intel_ihd());
        assert_eq!(runs(), after_first, "a cached path must not respawn ffmpeg");

        // A dashboard change to a different node re-probes, without a restart.
        let moved = vaapi_options(Some("/dev/dri/renderD129"));
        assert!(
            prober
                .capabilities(&moved)
                .await
                .is_vaapi_device_intel_ihd()
        );
        assert!(runs() > after_first, "a new path must probe afresh");
    }
}
