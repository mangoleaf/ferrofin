//! Bakes a git-derived build identity into the crate so `/health/live` (and the
//! server's reported version) can never be a stale hardcoded value.
//!
//! Resolution order:
//! 1. `FERROFIN_GIT_DESCRIBE` — set by builds where `.git` is unavailable (the
//!    Docker image build excludes it via `.dockerignore`); the benchmark harness
//!    passes the host's `git describe` through as a build arg so the compiled
//!    binary carries the identity of the tree it was built from.
//! 2. `git describe` — local builds with a repo present.
//! 3. The crate version — source tarballs / shallow clones.
//!
//! `rerun-if-env-changed` makes a changed identity force a recompile, so a
//! binary produced through a warm cache still carries the *current* value — and
//! a binary that somehow wasn't rebuilt carries its old one, which is exactly
//! what lets the benchmark harness detect a stale binary before measuring it.

use std::process::Command;

fn main() {
    let version = std::env::var("FERROFIN_GIT_DESCRIBE")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["describe", "--tags", "--always", "--dirty", "--abbrev=12"])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION")));
    println!("cargo:rustc-env=FERROFIN_BUILD_VERSION={version}");
    println!("cargo:rerun-if-env-changed=FERROFIN_GIT_DESCRIBE");
    // Re-run when HEAD moves so the baked version tracks new commits/tags.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}
