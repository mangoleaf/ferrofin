# CI base image: the pinned Rust toolchain plus the tools every Rust CI job needs,
# baked in so lint/coverage don't reinstall them on each pipeline run. Built and
# pushed by the `ci-image` job to $CI_REGISTRY_IMAGE/ci:rust-1.97.1 whenever this
# file changes; lint/coverage use it as their `image:`. Keep this in step with
# rust-toolchain.toml; bump the tag (ci:rust-<version>) whenever the toolchain moves.
FROM rust:1.97.1-bookworm

# rustfmt + clippy (lint) and llvm-tools-preview (coverage instrumentation).
RUN rustup component add rustfmt clippy llvm-tools-preview

# cargo-nextest (test runner) + cargo-llvm-cov (coverage) as prebuilt binaries
# via cargo-binstall, so the image build stays fast.
RUN curl -L --proto '=https' --tlsv1.2 -sSf \
      https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash \
 && cargo binstall -y cargo-nextest cargo-llvm-cov \
 && rm -rf "$CARGO_HOME/registry"

# Smoke-check the tools exist at build time (fails the image build if any is missing).
RUN cargo nextest --version && cargo llvm-cov --version && rustfmt --version && cargo clippy --version
