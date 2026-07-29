//! Build-time vendoring of extension web-dashboard assets.
//!
//! A Jellyfin plugin's settings page is web content the plugin ships (an HTML
//! shell plus a built JS/CSS app). Hermit compiles extensions in, so those
//! assets must be baked into `hermit-extensions` — and they must be the *real*
//! plugin's assets, built from its repo at a pinned revision, so the dashboard
//! renders identically to a Jellyfin-hosted install.
//!
//! Flow (per plugin):
//! 1. If the vendored copies under `assets/` are present (the committed,
//!    hermetic path — no network/node needed), copy them to `OUT_DIR` and stop.
//! 2. Otherwise — or when `HERMIT_REFRESH_PLUGIN_ASSETS` is set — clone the
//!    plugin repo at the pinned revision into `OUT_DIR`, run its web build
//!    (`npm install && npm run build`, needs node on `PATH`), copy the built
//!    assets to `OUT_DIR` for `include_bytes!`, and mirror them into `assets/`
//!    so the refresh can be committed.
//!
//! To bump the plugin: update the pinned rev below and rebuild with
//! `HERMIT_REFRESH_PLUGIN_ASSETS=1`, then commit the refreshed `assets/`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The Intro Skipper plugin repository.
const INTRO_SKIPPER_REPO: &str = "https://github.com/intro-skipper/intro-skipper.git";
/// The pinned revision the vendored assets are built from.
const INTRO_SKIPPER_REV: &str = "db09359a520dc91cf51336e508634513d1800fa8";
/// The built dashboard assets, relative to `IntroSkipper/Configuration/` in the
/// plugin repo: the page shell and the vite-built app it loads by name.
const INTRO_SKIPPER_ASSETS: &[&str] = &["configPage.html", "introskipper.js", "introskipper.css"];

/// The File Transformation plugin repository.
const FILE_TRANSFORMATION_REPO: &str =
    "https://github.com/IAmParadox27/jellyfin-plugin-file-transformation.git";
/// The pinned revision its vendored settings page is copied from.
const FILE_TRANSFORMATION_REV: &str = "f4f01c361343c63b51fe7a69c6ea0625c4ad1852";
/// Its dashboard assets — one static page, no web build step.
const FILE_TRANSFORMATION_ASSETS: &[&str] = &["config.html"];

fn main() {
    println!("cargo:rerun-if-env-changed=HERMIT_REFRESH_PLUGIN_ASSETS");
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let refresh = env::var_os("HERMIT_REFRESH_PLUGIN_ASSETS").is_some();

    vendor_plugin_assets(
        &manifest,
        &out_dir,
        refresh,
        "introskipper",
        INTRO_SKIPPER_ASSETS,
        build_intro_skipper_assets,
    );
    vendor_plugin_assets(
        &manifest,
        &out_dir,
        refresh,
        "filetransformation",
        FILE_TRANSFORMATION_ASSETS,
        build_file_transformation_assets,
    );
}

/// Stages one plugin's assets into `OUT_DIR/{name}` for `include_bytes!`: from
/// the committed `assets/{name}` copies when present (the hermetic path), else
/// by fetching + building via `build` (which must also mirror into `assets/`).
fn vendor_plugin_assets(
    manifest: &Path,
    out_dir: &Path,
    refresh: bool,
    name: &str,
    assets: &[&str],
    build: fn(&Path, &Path, &Path),
) {
    let vendored = manifest.join("assets").join(name);
    let staged = out_dir.join(name);
    fs::create_dir_all(&staged).expect("create staged asset dir");

    for asset in assets {
        println!("cargo:rerun-if-changed={}", vendored.join(asset).display());
    }

    let complete = assets.iter().all(|a| vendored.join(a).is_file());
    if complete && !refresh {
        for asset in assets {
            fs::copy(vendored.join(asset), staged.join(asset))
                .unwrap_or_else(|e| panic!("copy vendored asset {asset}: {e}"));
        }
        return;
    }

    build(out_dir, &staged, &vendored);
}

/// Clones the plugin repo at the pinned rev, runs its web build, and installs
/// the assets into both the staged (`OUT_DIR`) and vendored (`assets/`) dirs.
fn build_intro_skipper_assets(out_dir: &Path, staged: &Path, vendored: &Path) {
    let clone = out_dir.join("intro-skipper-src");
    if clone.exists() {
        fs::remove_dir_all(&clone).expect("clear stale plugin clone");
    }
    fs::create_dir_all(&clone).expect("create plugin clone dir");

    // Fetch exactly the pinned revision (GitHub allows fetch-by-sha).
    run("git", &["init", "-q"], &clone);
    run(
        "git",
        &[
            "fetch",
            "-q",
            "--depth",
            "1",
            INTRO_SKIPPER_REPO,
            INTRO_SKIPPER_REV,
        ],
        &clone,
    );
    run("git", &["checkout", "-q", "FETCH_HEAD"], &clone);

    // Build the dashboard app (vite writes into IntroSkipper/Configuration/).
    let web = clone.join("web");
    run("npm", &["install", "--no-fund", "--no-audit"], &web);
    run("npm", &["run", "build"], &web);

    let built = clone.join("IntroSkipper").join("Configuration");
    fs::create_dir_all(vendored).expect("create vendored asset dir");
    for asset in INTRO_SKIPPER_ASSETS {
        let src = built.join(asset);
        assert!(
            src.is_file(),
            "plugin web build did not produce {}",
            src.display()
        );
        fs::copy(&src, staged.join(asset))
            .unwrap_or_else(|e| panic!("stage built asset {asset}: {e}"));
        fs::copy(&src, vendored.join(asset))
            .unwrap_or_else(|e| panic!("vendor built asset {asset}: {e}"));
    }
}

/// Clones the File Transformation repo at the pinned rev and installs its
/// static settings page into the staged (`OUT_DIR`) and vendored (`assets/`)
/// dirs — a fetch + copy, since the page has no web build step.
fn build_file_transformation_assets(out_dir: &Path, staged: &Path, vendored: &Path) {
    let clone = out_dir.join("file-transformation-src");
    if clone.exists() {
        fs::remove_dir_all(&clone).expect("clear stale plugin clone");
    }
    fs::create_dir_all(&clone).expect("create plugin clone dir");

    run("git", &["init", "-q"], &clone);
    run(
        "git",
        &[
            "fetch",
            "-q",
            "--depth",
            "1",
            FILE_TRANSFORMATION_REPO,
            FILE_TRANSFORMATION_REV,
        ],
        &clone,
    );
    run("git", &["checkout", "-q", "FETCH_HEAD"], &clone);

    let built = clone
        .join("src")
        .join("Jellyfin.Plugin.FileTransformation")
        .join("Configuration");
    fs::create_dir_all(vendored).expect("create vendored asset dir");
    for asset in FILE_TRANSFORMATION_ASSETS {
        let src = built.join(asset);
        assert!(src.is_file(), "plugin repo is missing {}", src.display());
        fs::copy(&src, staged.join(asset))
            .unwrap_or_else(|e| panic!("stage plugin asset {asset}: {e}"));
        fs::copy(&src, vendored.join(asset))
            .unwrap_or_else(|e| panic!("vendor plugin asset {asset}: {e}"));
    }
}

/// Runs `program args…` in `dir`, panicking with a diagnosable message on
/// failure — a failed asset build must fail the crate build loudly, not bake in
/// stale or missing pages. (Requires `git`, `node`/`npm`, and network on the
/// *first* build or a forced refresh only; committed `assets/` keep normal
/// builds hermetic.)
fn run(program: &str, args: &[&str], dir: &Path) {
    let status = Command::new(program)
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "failed to spawn `{program}` (needed to build vendored plugin web assets; \
                 install it or restore the committed assets/ dir): {e}"
            )
        });
    assert!(
        status.success(),
        "`{program} {}` failed in {} (building vendored plugin web assets)",
        args.join(" "),
        dir.display()
    );
}
