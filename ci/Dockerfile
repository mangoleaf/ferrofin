# CI base image: the pinned Rust toolchain plus the tools every Rust CI job needs,
# baked in so lint/coverage don't reinstall them on each pipeline run. Built and
# pushed by the `ci-image` job to $CI_REGISTRY_IMAGE/ci:rust-1.97.1 whenever this
# file changes; lint/coverage use it as their `image:`. Keep this in step with
# rust-toolchain.toml; bump the tag (ci:rust-<version>) whenever the toolchain moves.
FROM rust:1.97.1-bookworm

# rustfmt + clippy (lint) and llvm-tools-preview (coverage instrumentation).
RUN rustup component add rustfmt clippy llvm-tools-preview

# cargo-nextest (test runner) + cargo-llvm-cov (coverage). We deliberately avoid
# cargo-binstall / GitHub release downloads here: those 503 / rate-limit from CI
# runners. cargo-nextest ships an official (Cloudflare-backed) tarball at
# get.nexte.st; cargo-llvm-cov is built from crates.io. Both sidestep github.com.
RUN curl -LsSf --retry 5 --retry-all-errors --proto '=https' --tlsv1.2 \
      https://get.nexte.st/latest/linux \
    | tar zxf - -C "$CARGO_HOME/bin" \
 && cargo install --locked cargo-llvm-cov \
 && rm -rf "$CARGO_HOME/registry"

# Smoke-check the tools exist at build time (fails the image build if any is missing).
RUN cargo nextest --version && cargo llvm-cov --version && rustfmt --version && cargo clippy --version
