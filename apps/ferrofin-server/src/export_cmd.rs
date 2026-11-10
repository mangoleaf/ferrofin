//! `ferrofin-server export-jellyfin`: copies a stopped data directory and a
//! consistent snapshot of its database into a new directory, leaving the
//! source untouched. It is the way back to Jellyfin once the export is
//! complete; until then the subcommand is hidden (see [`crate::config::Command`]).
//!
//! The export itself is [`ferrofin_db::export::export_data_dir`]; this file
//! holds the subcommand's arguments and its report, so the phases that extend
//! the export add their flags here.

use std::path::PathBuf;

use anyhow::Context as _;

use crate::config::Config;

/// The `export-jellyfin` subcommand's arguments. The source is the data
/// directory the server would use (`--data-dir`, `FERROFIN_DATA_DIR`,
/// `config.toml`, the default), and its database the one the server would open.
#[derive(Debug, Clone, clap::Args)]
pub struct ExportJellyfinArgs {
    /// The directory to write the export into.
    #[arg(long = "out", value_name = "DIR")]
    pub out: PathBuf,
}

/// Exports `config`'s data directory into `args.out` and prints what it wrote.
///
/// # Errors
/// Returns the export's error, with the source and output named, when a file
/// cannot be copied, the database snapshot fails, or the source changed while
/// it was exported.
pub async fn run(config: &Config, args: &ExportJellyfinArgs) -> anyhow::Result<()> {
    let source = &config.data_dir;
    let summary = ferrofin_db::export::export_data_dir(source, &config.database_path(), &args.out)
        .await
        .with_context(|| {
            format!(
                "failed to export {} into {}; anything this export wrote into {} is incomplete and should be deleted",
                source.display(),
                args.out.display(),
                args.out.display()
            )
        })?;
    println!(
        "Exported {} into {}: the database snapshot at {} and {} other files ({} bytes). The source is unchanged.",
        source.display(),
        args.out.display(),
        summary.database.display(),
        summary.files,
        summary.bytes
    );
    Ok(())
}
