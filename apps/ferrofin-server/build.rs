//! Bakes a git-derived default version into the binary so the reported version
//! is never a stale hardcoded value when it isn't injected.
//!
//! At runtime `SERVICE_VERSION` (set by CI from the release tag) takes precedence;
//! this only provides the DEFAULT for builds where it's unset (local dev). The
//! value is `git describe` — e.g. `v0.5.0-3-g<sha12>` (latest tag, commits since,
//! abbreviated HEAD sha, plus `-dirty` for uncommitted work) — falling back to the
//! crate version when git or tags are unavailable (source tarball, shallow clone,
//! or the Docker build where `.git` is excluded via `.dockerignore`).

use std::process::Command;

fn main() {
    let version = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty", "--abbrev=12"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION")));
    println!("cargo:rustc-env=FERROFIN_BUILD_VERSION={version}");
    // Re-run when HEAD moves so the baked version tracks new commits/tags.
    println!("cargo:rerun-if-changed=.git/HEAD");
}
