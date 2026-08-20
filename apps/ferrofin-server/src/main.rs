//! Ferrofin server binary entry point.
//!
//! Parses CLI flags, resolves the bootstrap [`Config`](ferrofin_server::config::Config),
//! and hands off to [`ferrofin_server::run`], which owns the whole boot-and-serve
//! sequence (port of `Jellyfin.Server`'s `Program.Main`). Keeping the binary thin
//! lets the composition root live in the library crate, where the First-Light
//! integration test can drive it. `anyhow` sits at this top level so any bring-up
//! failure surfaces as a non-zero exit with full context.

// glibc malloc arena contention convoyed 64 threads (32 tokio + 32 sqlx-sqlite)
// into 2200% kernel-mode CPU at moderate request rates; jemalloc eliminates it.
//
// The dependency enables jemalloc's `background_threads` feature, and that is a
// memory fix rather than a speed one. jemalloc purges decayed dirty pages
// *opportunistically, on allocator calls*: with no background thread, a server
// that goes quiet after a burst never runs the purge, so anonymous memory stays
// at whatever the burst peaked at for the rest of the process's life. Measured
// over an identical 20,000-request burst followed by 90 s idle, anonymous
// memory fell by 0.2-0.7 MiB (0.1-0.2%) without the background thread and by
// 69-309 MiB (31-88%) with it. That ratchet is why a long-lived server's peak
// RSS drifts far above its working set. Runtime `MALLOC_CONF` still overrides.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use clap::Parser as _;

use ferrofin_server::config::{Cli, Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::load(cli)?;
    ferrofin_server::run(config).await
}
