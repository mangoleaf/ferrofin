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

/// How long the check for a running server waits on a listener that accepted
/// the connection to answer `GET /System/Info/Public`. A local server answers
/// within milliseconds; something that accepts and never answers is not one.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// The most of the answer the check reads. A server's `/System/Info/Public`
/// is well under a kilobyte; something else streaming on the port is not
/// read without end.
const PROBE_ANSWER_LIMIT: usize = 64 * 1024;

/// Exports `config`'s data directory into `args.out` and prints what it wrote.
///
/// Before anything is written, the export refuses to run when completing it
/// would be unsafe or meaningless ([`ferrofin_db::export::refuse_unsafe`], a
/// server for this data directory answering on its port), and reads the
/// configuration, so a directory Ferrofin never served (it has none) is refused
/// too. The export is then written into `<out>.partial` and renamed to `out`
/// whole once complete; a failed one removes it, so it leaves no output.
///
/// # Errors
/// Returns the refusal, or the export's error with the source and output
/// named, when a file cannot be copied, the database snapshot fails or the
/// staged database fails its checks, the source changed while it was
/// exported, or the configuration cannot be written.
pub async fn run(config: &Config, args: &ExportJellyfinArgs) -> anyhow::Result<()> {
    log_warnings_to_stderr();
    let (source, out) = (&config.data_dir, &args.out);
    let nothing_written = || {
        format!(
            "failed to export {} into {}; nothing was written",
            source.display(),
            out.display()
        )
    };
    let target = ferrofin_db::export::refuse_unsafe(source, &config.database_path(), out)
        .await
        .with_context(nothing_written)?;
    let probed = config.clone();
    tokio::task::spawn_blocking(move || refuse_running_server(&probed))
        .await
        .context("the check for a running server did not finish")
        .and_then(|refused| refused)
        .with_context(nothing_written)?;
    let configuration = read_configuration(config)
        .await
        .context("its configuration cannot be read")
        .with_context(nothing_written)?;
    let replaced = target.exists();
    let partial = ferrofin_db::export::partial_output(&target);
    let created = create_partial(&partial)
        .await
        .with_context(nothing_written)?;
    let written = async {
        let summary =
            ferrofin_db::export::export_data_dir(source, &config.database_path(), &partial).await?;
        write_configuration(&configuration, &partial).await?;
        tokio::fs::rename(&partial, &target)
            .await
            .with_context(|| {
                format!(
                    "could not rename {} to {}",
                    partial.display(),
                    target.display()
                )
            })?;
        anyhow::Ok(summary)
    }
    .await;
    let summary = match written {
        Ok(summary) => summary,
        Err(error) => {
            let cleanup = partial.clone();
            let removed = match tokio::task::spawn_blocking(move || {
                remove_partial(&cleanup, created.as_deref())
            })
            .await
            {
                Ok(Ok(())) => "its partial output was removed".to_owned(),
                Ok(Err(e)) => format!(
                    "delete {} by hand: it could not be removed ({e})",
                    partial.display()
                ),
                Err(e) => format!(
                    "delete {} by hand: its removal did not finish ({e})",
                    partial.display()
                ),
            };
            return Err(error.context(format!(
                "failed to export {} into {}; {removed}",
                source.display(),
                out.display()
            )));
        }
    };
    println!(
        "Exported {} into {}: the database snapshot at {} and {} other files ({} bytes), then {} configuration files for Jellyfin over them. The source is unchanged.",
        source.display(),
        out.display(),
        target
            .join(ferrofin_db::export::EXPORTED_DATABASE)
            .display(),
        summary.files,
        summary.bytes,
        configuration.files.len()
    );
    if replaced {
        println!(
            "The export replaced the empty directory {} with a new directory that has the source's permissions; a shell inside it must change into it again to see the files.",
            target.display()
        );
    }
    for setting in &configuration.not_exported {
        println!(
            "Not exported: {} in {} ({})",
            setting.setting, setting.file, setting.reason
        );
    }
    Ok(())
}

/// Sends warnings to stderr. The export runs without the server's logging,
/// which would create `{data}/log` in the source, so the warnings the
/// configuration reads give (a document that does not parse exports its
/// defaults) go there. sqlx is left out: it warns about any statement over a
/// second, which the snapshot's `VACUUM INTO` of a real library always is.
fn log_warnings_to_stderr() {
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
}

/// Creates the export's working directory, private to its owner (the copy
/// gives it the source's mode once complete), and refuses one that is already
/// there: another export started writing it since the checks ran. Returns the
/// outermost directory above it that this created, for a failed export to
/// remove again.
async fn create_partial(partial: &Path) -> anyhow::Result<Option<PathBuf>> {
    let mut created = None;
    let mut ancestor = partial.parent();
    while let Some(dir) = ancestor.filter(|dir| !dir.as_os_str().is_empty() && !dir.exists()) {
        created = Some(dir.to_owned());
        ancestor = dir.parent();
    }
    if let Some(parent) = partial.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let mut builder = tokio::fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    match builder.create(partial).await {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => anyhow::bail!(
            "another export is writing {}; let it finish, or delete it once it has stopped",
            partial.display()
        ),
        created_dir => created_dir.with_context(|| {
            format!(
                "could not create {} (an export is written beside its output and renamed onto \
                 it, so the output's parent must be writable)",
                partial.display()
            )
        })?,
    }
    Ok(created)
}

/// Removes a failed export's `partial`, and the directories above it the
/// export created (`created` the outermost) while they are empty. The copy
/// keeps the source's modes, so a directory in it may be read-only; the
/// export made every one of them, and makes each writable to empty it.
fn remove_partial(partial: &Path, created: Option<&Path>) -> std::io::Result<()> {
    match std::fs::remove_dir_all(partial) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            make_writable(partial)?;
            std::fs::remove_dir_all(partial)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        removed => removed?,
    }
    if let Some(created) = created {
        let mut dir = partial.parent();
        while let Some(empty) = dir.filter(|dir| dir.starts_with(created)) {
            if std::fs::remove_dir(empty).is_err() {
                break;
            }
            dir = empty.parent();
        }
    }
    Ok(())
}

/// Gives the owner write and search permission on `dir` and every directory
/// under it, links not followed.
#[cfg(unix)]
fn make_writable(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        let mode = std::fs::symlink_metadata(&dir)?.permissions().mode();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode | 0o700))?;
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_writable(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Refuses an export while this data directory's server answers on the port
/// it is configured for: `GET {base_url}/System/Info/Public` there returns the
/// id in `{config}/system_id`. Another server on that port (a Jellyfin the
/// operator started already, say) is not this one, and is no reason to
/// refuse. The database lock check finds a server on this data directory
/// however it is reached; this one also finds it when the database is on a
/// filesystem that does not share locks.
fn refuse_running_server(config: &Config) -> anyhow::Result<()> {
    use std::io::{Read as _, Write as _};

    let Ok(id) = std::fs::read_to_string(config.config_dir.join("system_id")) else {
        return Ok(());
    };
    let id = id.trim();
    let host = match config.bind_addr {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => std::net::Ipv4Addr::LOCALHOST.into(),
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => std::net::Ipv6Addr::LOCALHOST.into(),
        ip => ip,
    };
    let address = std::net::SocketAddr::new(host, config.port);
    // A refused, failed or unanswered connection is no server at all.
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&address, PROBE_TIMEOUT) else {
        return Ok(());
    };
    let request = format!(
        "GET {}/System/Info/Public HTTP/1.0\r\nHost: localhost\r\n\r\n",
        config.base_url.trim_end_matches('/')
    );
    let sent = stream
        .set_write_timeout(Some(PROBE_TIMEOUT))
        .and_then(|()| stream.write_all(request.as_bytes()));
    if sent.is_err() {
        return Ok(());
    }
    // An HTTP/1.0 answer ends when the server closes; what arrived before the
    // deadline or the limit still counts.
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    let mut response = Vec::new();
    let mut chunk = [0_u8; 4096];
    while response.len() < PROBE_ANSWER_LIMIT {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => response.extend_from_slice(&chunk[..read]),
        }
    }
    let body = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(&[][..], |at| &response[at + 4..]);
    let answering = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|info| info.get("Id")?.as_str().map(str::to_owned));
    if answering.is_some_and(|answering| answering.eq_ignore_ascii_case(id)) {
        anyhow::bail!(
            "this data directory's server is running at http://{address} (it answers with its server id {id}); stop it first"
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
