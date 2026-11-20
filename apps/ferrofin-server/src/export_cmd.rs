//! `ferrofin-server export-jellyfin`: copies a stopped data directory and a
//! consistent snapshot of its database into a new directory, leaving the
//! source untouched, and writes Ferrofin's configuration over the copy as the
//! XML Jellyfin reads. It is the way back to Jellyfin once the export is
//! complete; until then the subcommand is hidden (see [`crate::config::Command`]).
//!
//! The copy is [`ferrofin_db::export::export_data_dir`] and the configuration
//! is [`ferrofin_core::config_export`]; this file holds the subcommand's
//! arguments and its report, so the phases that extend the export add their
//! flags here.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use ferrofin_core::config_export::{self, ConfigExport};

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
/// The configuration is read first, so a directory it cannot be read from (a
/// directory Ferrofin never served has none) is refused before anything is
/// written, and written after the copy, over the copies of Jellyfin's stale
/// files.
///
/// # Errors
/// Returns the export's error, with the source and output named, when the
/// configuration cannot be read or written, a file cannot be copied, the
/// database snapshot fails, or the source changed while it was exported.
pub async fn run(config: &Config, args: &ExportJellyfinArgs) -> anyhow::Result<()> {
    // The export runs without the server's logging, which would create
    // `{data}/log` in the source, so the warnings the configuration reads
    // give (a document that does not parse exports its defaults) go to stderr.
    // sqlx is left out: it warns about any statement over a second, which the
    // snapshot's `VACUUM INTO` of a real library always is.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::new("warn,sqlx=off"))
        // Colour only to a terminal, and never under `NO_COLOR`, which an
        // explicit `with_ansi` would otherwise override.
        .with_ansi(
            std::io::IsTerminal::is_terminal(&std::io::stderr())
                && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
        )
        .without_time()
        .with_target(false)
        .try_init();
    let source = &config.data_dir;
    let configuration = read_configuration(config).await.with_context(|| {
        format!(
            "failed to export {}: its configuration cannot be read; nothing was written to {}",
            source.display(),
            args.out.display()
        )
    })?;
    let incomplete = || {
        format!(
            "failed to export {} into {}; anything this export wrote into {} is incomplete and should be deleted",
            source.display(),
            args.out.display(),
            args.out.display()
        )
    };
    let summary = ferrofin_db::export::export_data_dir(source, &config.database_path(), &args.out)
        .await
        .with_context(incomplete)?;
    write_configuration(&configuration, &args.out)
        .await
        .with_context(incomplete)?;
    println!(
        "Exported {} into {}: the database snapshot at {} and {} other files ({} bytes), then {} configuration files for Jellyfin over them. The source is unchanged.",
        source.display(),
        args.out.display(),
        summary.database.display(),
        summary.files,
        summary.bytes,
        configuration.files.len()
    );
    for setting in &configuration.not_exported {
        println!(
            "Not exported: {} in {} ({})",
            setting.setting, setting.file, setting.reason
        );
    }
    Ok(())
}

/// Reads Ferrofin's configuration as the files Jellyfin reads: the six
/// configuration documents, the server id and each library's `options.xml`.
///
/// It is read from the directories the server uses, so a library's media
/// paths under them take Jellyfin's `%AppDataPath%` and `%MetadataPath%` forms.
async fn read_configuration(config: &Config) -> anyhow::Result<ConfigExport> {
    let mut export = config_export::export_configuration(&config.config_dir)?;
    let libraries =
        config_export::export_library_options(&config.data_dir, &config.config_dir).await?;
    export.files.extend(libraries.files);
    export.not_exported.extend(libraries.not_exported);
    Ok(export)
}

/// Writes `export`'s files under `out`. Each goes through a `.partial` file and
/// a rename, which also replaces a copy the source left read-only. A directory
/// the source left read-only (the copy keeps its mode) cannot take one, and the
/// export fails, naming it.
async fn write_configuration(export: &ConfigExport, out: &Path) -> anyhow::Result<()> {
    for file in &export.files {
        let path = out.join(&file.path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        let mut partial = path.clone().into_os_string();
        partial.push(".partial");
        tokio::fs::write(&partial, &file.contents)
            .await
            .with_context(|| format!("could not write {}", Path::new(&partial).display()))?;
        tokio::fs::rename(&partial, &path)
            .await
            .with_context(|| format!("could not replace {}", path.display()))?;
    }
    Ok(())
}
