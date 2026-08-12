//! Ferrofin server binary entry point.
//!
//! Parses CLI flags, resolves the bootstrap [`Config`](ferrofin_server::config::Config),
//! and hands off to [`ferrofin_server::run`], which owns the whole boot-and-serve
//! sequence (port of `Jellyfin.Server`'s `Program.Main`). Keeping the binary thin
//! lets the composition root live in the library crate, where the First-Light
//! integration test can drive it. `anyhow` sits at this top level so any bring-up
//! failure surfaces as a non-zero exit with full context.

use clap::Parser as _;

use ferrofin_server::config::{Cli, Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::load(cli)?;
    ferrofin_server::run(config).await
}
