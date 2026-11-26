//! `ferrofin-server export-jellyfin` on a scratch data directory (gl-1548):
//! the binary copies the stopped directory, writes a database snapshot that
//! holds what the write-ahead log held, writes Ferrofin's configuration as the
//! XML Jellyfin reads (gl-1579), leaves the source byte for byte as it was,
//! and gives the same output on a second run. It refuses an export that would
//! be unsafe or meaningless (gl-1550), naming the cause, and a failed export
//! leaves no output. The exported database holds Jellyfin's schema and nothing
//! of Ferrofin's, and the loss report it writes and prints names every
//! Ferrofin table that held rows (gl-1553). Live TV and plugin settings are
//! written as Jellyfin keeps them, the Live TV items kept, and no secret
//! printed (gl-1556).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ferrofin_db::export::LOSS_REPORT;
use sqlx::ConnectOptions as _;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};

/// `path` with `suffix` appended to its file name.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Every file under `root`, relative to it, with its bytes.
fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut found = BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                found.insert(path.strip_prefix(root).unwrap().to_owned(), bytes);
            }
        }
    }
    found
}

/// The server id Ferrofin keeps in `{config}/system_id`.
const SERVER_ID: &str = "3f25fe64f745407b88841fa775eef31b";

/// A secret the export must never print.
const CERTIFICATE_PASSWORD: &str = "hunter2-secret";

/// A data directory in Jellyfin's layout as a killed server leaves it: the
/// database's three rows still in its write-ahead log, plus artwork,
/// Ferrofin's configuration (one setting changed in the main document, one in
/// a named one, and a certificate the export does not carry), a library with
/// one option changed, and the stale copies of Jellyfin's own files that an
/// adopted directory keeps (`system.xml` read-only).
async fn source(scratch: &Path) -> PathBuf {
    let source = scratch.join("source");
    let database = source.join("data/jellyfin.db");
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    let live = scratch.join("live.db");
    let mut conn = SqliteConnectOptions::new()
        .filename(&live)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .pragma("wal_autocheckpoint", "0")
        .connect()
        .await
        .unwrap();
    // Ferrofin's own bookkeeping: a Ferrofin database this build's start left,
    // at its last migration with a start of the server's build recorded, as
    // the export requires (gl-1910).
    for statement in [
        "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY)",
        r#"CREATE TABLE "FerrofinMeta" ("Key" TEXT PRIMARY KEY, "Value" TEXT NOT NULL)"#,
    ] {
        sqlx::query(statement).execute(&mut conn).await.unwrap();
    }
    sqlx::query("INSERT INTO _sqlx_migrations VALUES (?1)")
        .bind(ferrofin_db::schema_head())
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query(r#"INSERT INTO "FerrofinMeta" VALUES (?1, ?2)"#)
        .bind(ferrofin_db::STARTED_KEY)
        .bind(ferrofin_health::build_version())
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE Items (Name TEXT NOT NULL)")
        .execute(&mut conn)
        .await
        .unwrap();
    for name in ["one", "two", "three"] {
        sqlx::query("INSERT INTO Items (Name) VALUES (?)")
            .bind(name)
            .execute(&mut conn)
            .await
            .unwrap();
    }
    // Copied while the connection holds the log open, so the log is not folded in.
    std::fs::copy(&live, &database).unwrap();
    std::fs::copy(with_suffix(&live, "-wal"), with_suffix(&database, "-wal")).unwrap();
    sqlx::Connection::close(conn).await.unwrap();
    std::fs::create_dir_all(source.join("metadata/library/ab/ab12")).unwrap();
    std::fs::write(
        source.join("metadata/library/ab/ab12/poster.jpg"),
        b"jpeg bytes",
    )
    .unwrap();
    std::fs::create_dir_all(source.join("root/default/Movies")).unwrap();
    std::fs::write(
        source.join("root/default/Movies/movies.mblink"),
        b"/media/movies",
    )
    .unwrap();
    std::fs::write(
        source.join("root/default/Movies/options.json"),
        br#"{"SaveLocalMetadata": true, "PathInfos": [{"Path": "/media/movies"}]}"#,
    )
    .unwrap();
    std::fs::create_dir_all(source.join("config/users/named")).unwrap();
    std::fs::write(
        source.join("config/system.json"),
        br#"{"ServerName": "Living room", "IsStartupWizardCompleted": true}"#,
    )
    .unwrap();
    std::fs::write(source.join("config/system_id"), format!("{SERVER_ID}\n")).unwrap();
    std::fs::write(
        source.join("config/users/named/encoding.json"),
        br#"{"EncoderPreset": "slow"}"#,
    )
    .unwrap();
    std::fs::write(
        source.join("config/users/named/network.json"),
        format!(r#"{{"CertificatePath": "/certs/a.pfx", "CertificatePassword": "{CERTIFICATE_PASSWORD}"}}"#),
    )
    .unwrap();
    for (stale, contents) in [
        (
            "config/system.xml",
            "<ServerConfiguration><ServerName>Old</ServerName></ServerConfiguration>",
        ),
        ("data/device.txt", "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f"),
        ("root/default/Movies/options.xml", "<LibraryOptions />"),
    ] {
        std::fs::write(source.join(stale), contents).unwrap();
    }
    std::fs::set_permissions(
        source.join("config/system.xml"),
        std::os::unix::fs::PermissionsExt::from_mode(0o444),
    )
    .unwrap();
    source
}

/// Jellyfin's configuration as the export writes it, relative to the output.
const CONFIGURATION: [&str; 11] = [
    "config/system.xml",
    "config/encoding.xml",
    "config/branding.xml",
    "config/network.xml",
    "config/metadata.xml",
    "config/xbmcmetadata.xml",
    "data/device.txt",
    "root/default/Movies/options.xml",
    "config/livetv.xml",
    "data/livetv/timers.json",
    "data/livetv/seriestimers.json",
];

/// Runs `ferrofin-server export-jellyfin --data-dir <source> --out <out>` with
/// no `FERROFIN_*` variable from the test's environment.
fn export(source: &Path, out: &Path) -> Output {
    export_on(source, out, free_port())
}

/// A port nothing listens on: the default one may be taken on the machine the
/// tests run on, and the export checks it for this data directory's server.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// [`export`] with the server configured on `127.0.0.1:port`.
fn export_on(source: &Path, out: &Path, port: u16) -> Output {
    command(source, out, Some(port))
        .output()
        .expect("run ferrofin-server export-jellyfin")
}

/// The command [`export_on`] runs, for a test to add to; without a `port`,
/// the server's own settings pick it.
fn command(source: &Path, out: &Path, port: Option<u16>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferrofin-server"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("FERROFIN_") {
            command.env_remove(key);
        }
    }
    command.arg("--bind").arg("127.0.0.1");
    if let Some(port) = port {
        command.arg("--port").arg(port.to_string());
    }
    command
        .arg("export-jellyfin")
        .arg("--data-dir")
        .arg(source)
        .arg("--out")
        .arg(out);
    command
}

/// The `Name` of every row in the exported database's `Items`.
async fn exported_names(out: &Path) -> Vec<String> {
    let mut conn = SqliteConnectOptions::new()
        .filename(out.join("data/jellyfin.db"))
        .read_only(true)
        .connect()
        .await
        .unwrap();
    let names = sqlx::query_scalar("SELECT Name FROM Items ORDER BY rowid")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    sqlx::Connection::close(conn).await.unwrap();
    names
}

#[tokio::test]
async fn export_jellyfin_copies_a_stopped_data_directory_and_leaves_it_untouched() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    let before = files(&source);
    let (out, again) = (scratch.path().join("out"), scratch.path().join("again"));

    let run = export(&source, &out);

    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "{}\n{stdout}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(stdout.contains("The source is unchanged."), "{stdout}");
    // a database Ferrofin created has no Jellyfin history: it gets a fresh install's (gl-1552)
    assert!(
        stdout.contains(
            "the 105 migrations a fresh install of Jellyfin records, so Jellyfin runs none"
        ),
        "{stdout}"
    );
    assert_eq!(
        files(&source),
        before,
        "the source is left byte for byte as it was"
    );
    let mut copied = files(&out);
    // Every file but the database, which is the snapshot alone, and
    // Jellyfin's configuration, written from Ferrofin's.
    let snapshot = copied.remove(Path::new("data/jellyfin.db")).unwrap();
    let configuration: BTreeMap<_, _> = CONFIGURATION
        .iter()
        .map(|path| {
            let bytes = copied
                .remove(Path::new(path))
                .unwrap_or_else(|| panic!("{path} is written"));
            (*path, String::from_utf8(bytes).unwrap())
        })
        .collect();
    assert!(configuration["config/system.xml"].contains("<ServerName>Living room</ServerName>"));
    assert!(configuration["config/encoding.xml"].contains("<EncoderPreset>slow</EncoderPreset>"));
    assert!(
        configuration["root/default/Movies/options.xml"]
            .contains("<SaveLocalMetadata>true</SaveLocalMetadata>")
    );
    assert_eq!(configuration["data/device.txt"], SERVER_ID);
    // The loss report is written into the output and printed; the
    // certificate is reported by name, and its password nowhere.
    let report = String::from_utf8(copied.remove(Path::new(LOSS_REPORT)).unwrap()).unwrap();
    assert!(stdout.ends_with(&report), "{stdout}");
    assert!(
        report.contains("  CertificatePassword in config/network.xml: "),
        "{report}"
    );
    assert!(
        report.contains("  CertificatePath in config/network.xml: "),
        "{report}"
    );
    assert!(report.contains("  _sqlx_migrations (1 row): "), "{report}");
    assert!(
        report.ends_with("Backups and scheduled tasks:\n  None.\n"),
        "{report}"
    );
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(!stdout.contains(CERTIFICATE_PASSWORD) && !stderr.contains(CERTIFICATE_PASSWORD));
    assert!(!configuration["config/network.xml"].contains(CERTIFICATE_PASSWORD));
    // Every other file is a copy; the stale copies of Jellyfin's files were
    // replaced above.
    let mut expected = before.clone();
    expected.retain(|path, _| {
        !path.starts_with("data") && !CONFIGURATION.iter().any(|c| path == Path::new(c))
    });
    assert_eq!(copied, expected);
    assert_eq!(exported_names(&out).await, ["one", "two", "three"]);

    // B10: the same stopped source exports to the same database and the same
    // configuration.
    assert!(export(&source, &again).status.success());
    assert_eq!(
        std::fs::read(again.join("data/jellyfin.db")).unwrap(),
        snapshot
    );
    assert_eq!(
        std::fs::read_to_string(again.join(LOSS_REPORT)).unwrap(),
        report
    );
    for (path, contents) in &configuration {
        assert_eq!(
            &std::fs::read_to_string(again.join(path)).unwrap(),
            contents,
            "{path}"
        );
    }
    assert_eq!(files(&source), before);
}

/// A named document that does not parse is exported as its defaults, as the
/// server serves it, and the export says so on stderr.
#[tokio::test]
async fn export_jellyfin_warns_about_a_document_that_does_not_parse() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    std::fs::write(source.join("config/users/named/encoding.json"), "{not json").unwrap();
    let out = scratch.path().join("out");
    let run = export(&source, &out);

    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(run.status.success(), "{stderr}");
    assert!(stderr.contains("exporting its defaults"), "{stderr}");
    assert!(
        std::fs::read_to_string(out.join("config/encoding.xml"))
            .unwrap()
            .contains("<EncoderPreset>auto</EncoderPreset>")
    );
}

/// A directory Ferrofin never served has no configuration to give Jellyfin:
/// the export says so, before it writes anything, rather than writing
/// defaults.
#[tokio::test]
async fn export_jellyfin_refuses_a_directory_ferrofin_never_served() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    std::fs::remove_file(source.join("config/system.json")).unwrap();
    let out = scratch.path().join("out");
    let run = export(&source, &out);

    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(!run.status.success(), "{stderr}");
    assert!(stderr.contains("system.json"), "{stderr}");
    assert!(stderr.contains("nothing was written"), "{stderr}");
    assert!(!out.exists(), "the export wrote into {}", out.display());
}

/// The stderr of a refused export of `source` into `out`, which must exit
/// non-zero, leave `out` and its `.partial` absent (or as they were) and the
/// source byte for byte as it was.
fn refused(source: &Path, out: &Path, run: impl FnOnce() -> Output) -> String {
    let before = files(source);
    let partial = with_suffix(out, ".partial");
    let existed = (out.exists(), partial.exists());
    let run = run();
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    assert!(
        !run.status.success(),
        "{} was exported: {stderr}",
        out.display()
    );
    assert_eq!(files(source), before, "the source is left as it was");
    assert_eq!((out.exists(), partial.exists()), existed, "{stderr}");
    stderr
}

/// An output nested with the source, or the source itself, is refused: the
/// export never writes into what it reads.
#[tokio::test]
async fn export_jellyfin_refuses_an_output_nested_with_the_source() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    let inside = source.join("export");
    let stderr = refused(&source, &inside, || export(&source, &inside));
    assert!(stderr.contains("inside the source"), "{stderr}");
    let stderr = refused(&source, &source, || export(&source, &source));
    assert!(stderr.contains("is the source"), "{stderr}");
    let holding = scratch.path().join("missing/..");
    let stderr = refused(&source, &holding, || export(&source, &holding));
    assert!(stderr.contains("inside the output"), "{stderr}");
    // Inside a directory the source reaches through a link: the copy follows
    // links, so it would read what it writes.
    let volume = scratch.path().join("volume");
    std::fs::create_dir_all(volume.join("library")).unwrap();
    std::fs::write(volume.join("library/cover.jpg"), b"jpeg").unwrap();
    std::os::unix::fs::symlink(&volume, source.join("metadata/volume")).unwrap();
    let linked = volume.join("export");
    let stderr = refused(&source, &linked, || export(&source, &linked));
    assert!(stderr.contains("reaches through"), "{stderr}");
}

/// A non-empty output is refused, and so is an output beside which an
/// interrupted export left its `.partial`: it is reported, never reused.
#[tokio::test]
async fn export_jellyfin_refuses_a_used_output() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    let out = scratch.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("keep.txt"), "the operator's").unwrap();
    let stderr = refused(&source, &out, || export(&source, &out));
    assert!(stderr.contains("is not empty"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(out.join("keep.txt")).unwrap(),
        "the operator's"
    );

    let out = scratch.path().join("fresh");
    std::fs::create_dir_all(scratch.path().join("fresh.partial")).unwrap();
    let stderr = refused(&source, &out, || export(&source, &out));
    assert!(stderr.contains("did not finish"), "{stderr}");

    let dangling = scratch.path().join("dangling");
    std::os::unix::fs::symlink(scratch.path().join("nowhere"), &dangling).unwrap();
    let stderr = refused(&source, &dangling, || export(&source, &dangling));
    assert!(stderr.contains("does not exist"), "{stderr}");
}

/// The output is the directory `--out` names however it is spelled: with a
/// trailing `/`, as `.`, or as a link to an empty directory. The partial is
/// written beside the directory, never inside it, and renamed onto it.
#[tokio::test]
async fn export_jellyfin_writes_the_output_however_it_is_spelled() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    let exported = |out: &Path, run: Output| {
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(run.status.success(), "{}: {stderr}", out.display());
        assert!(out.join("data/jellyfin.db").is_file(), "{}", out.display());
        assert!(!out.join(".partial").exists(), "{}", out.display());
    };

    let new = scratch.path().join("new");
    let run = export(&source, &scratch.path().join("new/"));
    assert!(
        !String::from_utf8_lossy(&run.stdout).contains("replaced"),
        "a new output replaces nothing"
    );
    exported(&new, run);
    let empty = scratch.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    exported(&empty, export(&source, &scratch.path().join("empty/")));
    let here = scratch.path().join("here");
    std::fs::create_dir(&here).unwrap();
    let run = command(&source, Path::new("."), Some(free_port()))
        .current_dir(&here)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&run.stdout).contains("replaced the empty directory"),
        "{}",
        String::from_utf8_lossy(&run.stdout)
    );
    exported(&here, run);
    let target = scratch.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let link = scratch.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    exported(&target, export(&source, &link));
    assert!(link.is_symlink(), "the link is left a link");
    let leftovers: Vec<_> = std::fs::read_dir(scratch.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().ends_with(".partial"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// A database another process holds open (a running server) is refused.
#[tokio::test]
async fn export_jellyfin_refuses_a_database_in_use() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    let database = source.join("data/jellyfin.db");
    // Taken before the connection opens: closing any descriptor of a file
    // drops every POSIX lock this process holds on it, SQLite's included.
    let before = files(&source);
    // Opened in this process, so the export, another process, sees its lock.
    // SQLite opens the WAL index, and takes its lock, at the first read.
    let mut conn = SqliteConnectOptions::new()
        .filename(&database)
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await
        .unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM Items")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(rows, 3);
    let out = scratch.path().join("out");
    let run = export(&source, &out);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(!run.status.success(), "{stderr}");
    assert!(
        stderr.contains("another process holds the database"),
        "{stderr}"
    );
    assert!(!out.exists() && !with_suffix(&out, ".partial").exists());
    // Everything but the WAL index, which this test's connection wrote.
    let mut after = files(&source);
    sqlx::Connection::close(conn).await.unwrap();
    let mut before = before;
    for map in [&mut before, &mut after] {
        map.retain(|path, _| !path.to_string_lossy().ends_with("-shm"));
    }
    assert_eq!(after, before, "the source is left as it was");
}

/// A stand-in server on `listener` that answers one `GET {path}` with `body`
/// (anything else with a 302, as a server under a base URL does). Returns
/// whether anything connected within half a minute, so a probe that never
/// comes fails the test instead of hanging it.
fn answer(
    listener: std::net::TcpListener,
    path: &'static str,
    body: String,
) -> std::thread::JoinHandle<bool> {
    std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        let Some(mut stream) = accept_within(&listener) else {
            return false;
        };
        let mut request = [0_u8; 1024];
        let read = stream.read(&mut request).unwrap();
        let line = String::from_utf8_lossy(&request[..read]);
        if line.starts_with(&format!("GET {path} ")) {
            write!(
                stream,
                "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        } else {
            write!(stream, "HTTP/1.0 302 Found\r\nLocation: /web/\r\n\r\n").unwrap();
        }
        true
    })
}

/// The first connection to `listener` within half a minute, if any: a probe
/// that never comes fails its test instead of hanging it.
fn accept_within(listener: &std::net::TcpListener) -> Option<std::net::TcpStream> {
    listener.set_nonblocking(true).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                return Some(stream);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() > deadline {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("{e}"),
        }
    }
}

/// A server answering this data directory's id on its port, under its base
/// URL, is running on it and refused; another server on that port is not,
/// and the export goes on.
#[tokio::test]
async fn export_jellyfin_refuses_while_this_directorys_server_answers() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    for (id, base_url, path, refuse) in [
        (SERVER_ID, "", "/System/Info/Public", true),
        (SERVER_ID, "/jf/", "/jf/System/Info/Public", true),
        (
            "0123456789abcdef0123456789abcdef",
            "",
            "/System/Info/Public",
            false,
        ),
    ] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = answer(
            listener,
            path,
            format!(r#"{{"Id":"{id}","ServerName":"x"}}"#),
        );
        let out = scratch
            .path()
            .join(format!("out-{refuse}-{}", base_url.len()));
        let run = || {
            command(&source, &out, Some(port))
                .env("FERROFIN_BASE_URL", base_url)
                .output()
                .unwrap()
        };
        if refuse {
            let stderr = refused(&source, &out, run);
            assert!(stderr.contains("server is running"), "{base_url}: {stderr}");
        } else {
            let run = run();
            assert!(
                run.status.success(),
                "{}",
                String::from_utf8_lossy(&run.stderr)
            );
        }
        assert!(
            server.join().unwrap(),
            "{base_url}: the export probed the port"
        );
    }
}

/// With no flag or variable setting them, the probe looks where the server
/// would listen and mount: the port and base URL saved on the Networking page,
/// the base URL escaped as the server mounts it.
#[tokio::test]
async fn export_jellyfin_probes_the_networking_pages_port_and_base_url() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::fs::write(
        source.join("config/users/named/network.json"),
        format!(r#"{{"InternalHttpPort": {port}, "BaseUrl": "saved dir/"}}"#),
    )
    .unwrap();
    let server = answer(
        listener,
        "/saved%20dir/System/Info/Public",
        format!(r#"{{"Id":"{SERVER_ID}","ServerName":"x"}}"#),
    );
    let out = scratch.path().join("out");
    let stderr = refused(&source, &out, || {
        command(&source, &out, None).output().unwrap()
    });
    assert!(stderr.contains("server is running"), "{stderr}");
    assert!(server.join().unwrap(), "the export probed the saved port");
}

/// A source larger than the free space on the output's filesystem is refused
/// before anything is copied, with the shortfall.
#[tokio::test]
async fn export_jellyfin_refuses_too_little_free_space() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    let before = files(&source);
    // Sparse, so it takes no space: 1 PiB long where the filesystem allows
    // it, else 8 TiB (ext4's limit is 16), more than any test disk has free.
    let huge = source.join("metadata/huge.bin");
    let file = std::fs::File::create(&huge).unwrap();
    file.set_len(1 << 50)
        .or_else(|_| file.set_len(1 << 43))
        .unwrap();
    let length = file.metadata().unwrap().len();
    let out = scratch.path().join("out");
    let run = export(&source, &out);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(!run.status.success(), "{stderr}");
    assert!(stderr.contains("short"), "{stderr}");
    assert!(!out.exists() && !with_suffix(&out, ".partial").exists());
    // Not read whole: it is as long as it was, and the rest is as it was.
    assert_eq!(std::fs::metadata(&huge).unwrap().len(), length);
    std::fs::remove_file(&huge).unwrap();
    assert_eq!(files(&source), before, "the source is left as it was");
}

/// A server whose metadata directory its settings keep outside the data
/// directory, and whose cache directory is inside it, exports in the layout of
/// Jellyfin's docker image (gl-1557): the metadata directory it used becomes
/// the output's `metadata`, the data directory's own stale one and the cache
/// are left out and named in the loss report, and the preflight prints each
/// class's size before anything is copied. A metadata directory the settings
/// name that is not there is reported loudly.
#[tokio::test]
async fn export_jellyfin_writes_the_docker_layout_from_directories_kept_elsewhere() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    let meta = scratch.path().join("meta");
    std::fs::create_dir_all(meta.join("library/cd/cd34")).unwrap();
    std::fs::write(meta.join("library/cd/cd34/poster.jpg"), b"the server's").unwrap();
    let settings = |metadata: &Path| {
        std::fs::write(
            source.join("config/system.json"),
            format!(
                r#"{{"ServerName": "Living room", "IsStartupWizardCompleted": true, "MetadataPath": "{}"}}"#,
                metadata.display()
            ),
        )
        .unwrap();
    };
    settings(&meta);
    std::fs::create_dir_all(source.join("cache/images")).unwrap();
    std::fs::write(source.join("cache/images/resized.jpg"), b"cached").unwrap();
    let before = files(&source);
    let out = scratch.path().join("out");

    let run = command(&source, &out, Some(free_port()))
        .env("FERROFIN_CACHE_DIR", source.join("cache"))
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "{}\n{stdout}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(files(&source), before, "the source is left as it was");
    let copied = files(&out);
    assert_eq!(
        copied[Path::new("metadata/library/cd/cd34/poster.jpg")],
        b"the server's"
    );
    assert!(
        !copied
            .keys()
            .any(|path| path.starts_with("cache") || path.ends_with("ab12/poster.jpg"))
    );
    let preflight = stdout.find("Preflight: the export needs about ").unwrap();
    assert!(
        stdout[preflight..].contains("\n  artwork and other metadata: 0.1 MiB\n"),
        "{stdout}"
    );
    assert!(preflight < stdout.find("Exported ").unwrap(), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "{} was copied to {}, where Jellyfin reads it.",
            meta.display(),
            std::fs::canonicalize(&out)
                .unwrap()
                .join("metadata")
                .display()
        )),
        "{stdout}"
    );
    let report = std::fs::read_to_string(out.join(LOSS_REPORT)).unwrap();
    for line in [
        "  cache: left out of the copy; the server's cache, log or web client, which Jellyfin does not read.\n",
        "  metadata: left out of the copy; not the directory the server's settings name for it.\n",
    ] {
        assert!(report.contains(line), "{report}");
    }
    assert!(!report.contains("was not found"), "{report}");

    settings(&scratch.path().join("gone"));
    let again = scratch.path().join("again");
    let run = export(&source, &again);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    let warning = stdout
        .find("Warning: the metadata directory the server's settings name, ")
        .unwrap();
    assert!(
        warning < stdout.find("Exported ").unwrap(),
        "the preflight warns before the copy: {stdout}"
    );
    let report = std::fs::read_to_string(again.join(LOSS_REPORT)).unwrap();
    assert!(
        report.contains(
            "  The metadata directory the server's settings name was not found where the export ran"
        ),
        "{report}"
    );
}

/// A server that ran at `/data` (its database's root and library rows name
/// it), exported from another path: its stored paths are rewritten onto
/// `/config` from `/data`, a library's media path in its data directory is
/// stored as `%AppDataPath%`, reversed from `/data`, and both the output and
/// the loss report say the directories differ; the export is not refused
/// (gl-1557).
#[tokio::test]
async fn export_jellyfin_rewrites_from_where_the_server_ran() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    alter(
        &source,
        &[
            r#"CREATE TABLE "BaseItems" ("Type" TEXT NOT NULL, "Path" TEXT)"#,
            r#"INSERT INTO "BaseItems" VALUES
               ('MediaBrowser.Controller.Entities.AggregateFolder', '/data/root'),
               ('MediaBrowser.Controller.Entities.CollectionFolder', '/data/root/default/Collections')"#,
        ],
    )
    .await;
    std::fs::create_dir_all(source.join("root/default/Collections")).unwrap();
    std::fs::write(
        source.join("root/default/Collections/options.json"),
        br#"{"PathInfos": [{"Path": "/data/data/collections"}]}"#,
    )
    .unwrap();
    let out = scratch.path().join("out");

    let run = export(&source, &out);

    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "{}\n{stdout}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "The database's root and library rows name /data as the data directory the server ran at, not {}, the one exported",
            source.display()
        )),
        "{stdout}"
    );
    let report = std::fs::read_to_string(out.join(LOSS_REPORT)).unwrap();
    assert!(
        report.contains("  The database's root and library rows name another data directory than the one exported"),
        "{report}"
    );
    let options =
        std::fs::read_to_string(out.join("root/default/Collections/options.xml")).unwrap();
    assert!(
        options.contains("<Path>%AppDataPath%/collections</Path>"),
        "{options}"
    );
    let mut conn = SqliteConnectOptions::new()
        .filename(out.join("data/jellyfin.db"))
        .read_only(true)
        .connect()
        .await
        .unwrap();
    let paths: Vec<String> = sqlx::query_scalar(r#"SELECT "Path" FROM "BaseItems" ORDER BY rowid"#)
        .fetch_all(&mut conn)
        .await
        .unwrap();
    sqlx::Connection::close(conn).await.unwrap();
    assert_eq!(paths, ["/config/root", "/config/root/default/Collections"]);
}

/// Runs `statements` on `source`'s database, with foreign keys off so a test
/// can break them.
async fn alter(source: &Path, statements: &[&str]) {
    let mut conn = SqliteConnectOptions::new()
        .filename(source.join("data/jellyfin.db"))
        .foreign_keys(false)
        .connect()
        .await
        .unwrap();
    for statement in statements {
        sqlx::query(sqlx::AssertSqlSafe(*statement))
            .execute(&mut conn)
            .await
            .unwrap();
    }
    sqlx::Connection::close(conn).await.unwrap();
}

/// A database the export cannot vouch for is refused, and the export leaves
/// no output: what it had staged is removed. Not Ferrofin's, a broken foreign
/// key, a Jellyfin history from no release Ferrofin adopts, a damaged file,
/// a write-ahead log damaged before its end, and an adopted one without the
/// proof of the work of a code migration the history records as applied.
#[tokio::test]
async fn export_jellyfin_refuses_a_database_it_cannot_vouch_for() {
    let scratch = tempfile::tempdir().unwrap();
    let case = async |name: &str| {
        let dir = scratch.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        source(&dir).await
    };
    let expect = |source: &Path, cause: &str| {
        let out = source.with_file_name("out");
        let stderr = refused(source, &out, || export(source, &out));
        assert!(stderr.contains(cause), "{cause}: {stderr}");
        assert!(stderr.contains("partial output was removed"), "{stderr}");
    };

    let source = case("not-ferrofins").await;
    alter(&source, &["DROP TABLE _sqlx_migrations"]).await;
    expect(&source, "not a Ferrofin database");

    let source = case("foreign-key").await;
    alter(
        &source,
        &[
            "CREATE TABLE Parents (Id INTEGER PRIMARY KEY)",
            "CREATE TABLE Children (ParentId INTEGER REFERENCES Parents (Id))",
            "INSERT INTO Children VALUES (7)",
        ],
    )
    .await;
    expect(&source, "foreign-key check: 1 rows in Children");

    let source = case("future-history").await;
    alter(
        &source,
        &[
            r#"CREATE TABLE "__EFMigrationsHistory" ("MigrationId" TEXT PRIMARY KEY, "ProductVersion" TEXT)"#,
            r#"INSERT INTO "__EFMigrationsHistory" VALUES ('29990101000000_Future', '99.0')"#,
        ],
    )
    .await;
    expect(&source, "matches no Jellyfin release");

    // An index whose definition no longer matches its entries: SQLite reads
    // it without an error, and only the integrity check finds the mismatch.
    let source = case("integrity").await;
    alter(
        &source,
        &[
            "CREATE TABLE Pairs (A INTEGER, B INTEGER)",
            "CREATE INDEX IX_Pairs ON Pairs (A)",
            "INSERT INTO Pairs VALUES (1, 100), (2, 200), (3, 300)",
            "PRAGMA writable_schema = ON",
            "UPDATE sqlite_master SET sql = 'CREATE INDEX IX_Pairs ON Pairs (B)' WHERE name = 'IX_Pairs'",
        ],
    )
    .await;
    expect(&source, "fails its integrity check (");

    // Its header damaged once the log is folded in: while page 1 is in the
    // log, SQLite reads it from there, and the file's copy is not used.
    let source = case("damaged").await;
    alter(&source, &["PRAGMA wal_checkpoint(TRUNCATE)"]).await;
    let database = source.join("data/jellyfin.db");
    let mut bytes = std::fs::read(&database).unwrap();
    bytes[16..100].fill(0x5a);
    std::fs::write(&database, bytes).unwrap();
    expect(&source, "damaged");

    // The fixture's rows are still in its log, as a killed server leaves them.
    let source = case("log").await;
    let log = source.join("data/jellyfin.db-wal");
    let mut bytes = std::fs::read(&log).unwrap();
    bytes[32 + 24 + 100] ^= 0xff;
    std::fs::write(&log, bytes).unwrap();
    expect(&source, "write-ahead log is damaged at frame 1");

    // Adopted, and last started by a Ferrofin release before a boot repair:
    // no proof of the work of a routine the history records as applied
    // (gl-1902).
    let dir = scratch.path().join("no-proof");
    std::fs::create_dir(&dir).unwrap();
    let source = adopted_source(&dir).await;
    alter(
        &source,
        &[r#"DELETE FROM "FerrofinMeta" WHERE "Key" = 'rating_levels_v122'"#],
    )
    .await;
    expect(
        &source,
        "lacks the proof of that work for: MigrateRatingLevels (rating_levels_v122)",
    );
}

/// Whether permissions bind this process (they do not bind root): a file
/// made unreadable cannot be read.
fn permissions_bind(scratch: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    let probe = scratch.join("permission-probe");
    std::fs::write(&probe, b"x").unwrap();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o000)).unwrap();
    let binds = std::fs::read(&probe).is_err();
    std::fs::remove_file(&probe).unwrap();
    binds
}

/// A failed export leaves no output and the source as it was: an unreadable
/// source file fails the copy, and a read-only directory the copy keeps fails
/// the configuration written into it. The partial goes, read-only directories
/// and all, and so do the directories the export created above it.
#[tokio::test]
async fn export_jellyfin_leaves_no_output_when_it_fails() {
    use std::os::unix::fs::PermissionsExt as _;

    let scratch = tempfile::tempdir().unwrap();
    if !permissions_bind(scratch.path()) {
        // shortcut: as root no permission fails the export; the cleanup is
        // then untested here.
        return;
    }
    let source = source(scratch.path()).await;
    let before = files(&source);
    let failed = |changed: &Path, mode: u32, out: &Path| {
        let kept = std::fs::metadata(changed).unwrap().permissions();
        std::fs::set_permissions(changed, std::fs::Permissions::from_mode(mode)).unwrap();
        let run = export(&source, out);
        std::fs::set_permissions(changed, kept).unwrap();
        let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
        assert!(!run.status.success(), "{stderr}");
        assert!(stderr.contains("partial output was removed"), "{stderr}");
        assert!(
            !out.exists() && !with_suffix(out, ".partial").exists(),
            "{stderr}"
        );
        assert_eq!(files(&source), before, "the source is left as it was");
    };

    failed(
        &source.join("metadata/library/ab/ab12/poster.jpg"),
        0o000,
        &scratch.path().join("out"),
    );
    failed(
        &source.join("config"),
        0o555,
        &scratch.path().join("new/parents/out"),
    );
    assert!(
        !scratch.path().join("new").exists(),
        "the created parents go too"
    );
}

/// Something on the configured port that streams without end is read only
/// so far, and is not this data directory's server: the export goes on.
#[tokio::test]
async fn export_jellyfin_reads_a_bounded_answer_from_the_port() {
    let scratch = tempfile::tempdir().unwrap();
    let source = source(scratch.path()).await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let streamer = std::thread::spawn(move || {
        use std::io::Write as _;
        let mut stream = accept_within(&listener)?;
        let chunk = vec![b'x'; 64 * 1024];
        let mut sent = 0_usize;
        while stream.write_all(&chunk).is_ok() {
            sent += chunk.len();
        }
        Some(sent)
    });
    let out = scratch.path().join("out");
    let started = std::time::Instant::now();
    let run = export_on(&source, &out, port);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let sent = streamer
        .join()
        .unwrap()
        .expect("the export probed the port");
    // The answer limit and the kernel's buffers: a few MiB, where a read
    // bounded only by the deadline takes gigabytes.
    assert!(
        sent < 64 << 20,
        "the export stopped reading ({sent} bytes sent)"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(60));
}

/// The Jellyfin 10.11.8 schema the adoption tests seed a database from.
const JELLYFIN_10_11_8_SCHEMA: &str =
    include_str!("../../../crates/ferrofin-db/tests/data/jellyfin-10.11.8-schema.sql");

/// The Jellyfin schema an exported database must have: the 12.2 pin (12.3
/// and 12.4 add no schema migration).
const JELLYFIN_12_2_SCHEMA: &str =
    include_str!("../../../crates/ferrofin-db/tests/data/jellyfin-12.2-schema.sql");

/// Writes a backup archive at `path` whose manifest names `engine` as its
/// backup engine version.
fn archive(path: &Path, engine: &str) {
    use std::io::Write as _;

    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    zip.start_file("manifest.json", zip::write::SimpleFileOptions::default())
        .unwrap();
    write!(
        zip,
        r#"{{"Path":"x","DateCreated":"2026-11-01T00:00:00Z","ServerVersion":"10.11.8","BackupEngineVersion":"{engine}","Options":{{}}}}"#
    )
    .unwrap();
    zip.finish().unwrap();
}

/// [`source`] with its database replaced by a Jellyfin 10.11.8 database that
/// Ferrofin adopted and used: a user, two Intro Skipper segments and a
/// retained watch state, beside the rows the migrations themselves write. Its
/// backups directory holds an archive of Ferrofin's and one Jellyfin made
/// before adoption, and Ferrofin's scheduled-task files are there.
async fn adopted_source(scratch: &Path) -> PathBuf {
    let source = source(scratch).await;
    std::fs::create_dir_all(source.join("data/backups")).unwrap();
    archive(&source.join("data/backups/ferrofin-backup.zip"), "1.0.0");
    archive(&source.join("data/backups/jellyfin-backup.zip"), "0.2.0");
    std::fs::write(source.join("config/task_triggers.json"), "{}").unwrap();
    std::fs::write(source.join("task_results.json"), "{}").unwrap();
    let database = source.join("data/jellyfin.db");
    std::fs::remove_file(&database).unwrap();
    std::fs::remove_file(with_suffix(&database, "-wal")).unwrap();
    let generation = ferrofin_db::JELLYFIN_GENERATIONS
        .iter()
        .find(|g| g.name == "10.11.8")
        .unwrap();
    let mut conn = SqliteConnectOptions::new()
        .filename(&database)
        .create_if_missing(true)
        .connect()
        .await
        .unwrap();
    sqlx::raw_sql(JELLYFIN_10_11_8_SCHEMA)
        .execute(&mut conn)
        .await
        .unwrap();
    for id in generation.migration_ids {
        sqlx::query(r#"INSERT INTO "__EFMigrationsHistory" VALUES (?1, '10.11.8.0')"#)
            .bind(id)
            .execute(&mut conn)
            .await
            .unwrap();
    }
    sqlx::Connection::close(conn).await.unwrap();

    let db =
        ferrofin_db::Database::connect_sized(&format!("sqlite://{}", database.display()), Some(2))
            .await
            .unwrap();
    db.run_migrations().await.unwrap();
    let user = "5D1E2C3B-4A59-4687-9C8B-7A6F5E4D3C2B";
    for statement in [
        r#"INSERT INTO "Users" ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess", "HidePlayedInLatest",
            "InternalId", "InvalidLoginAttemptCount", "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack", "RememberAudioSelections",
            "RememberSubtitleSelections", "RowVersion", "SubtitleMode", "SyncPlayAccess",
            "Username", "NormalizedUsername")
           VALUES (?1, 'auth', 0, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 'reset', 1, 1, 1, 0, 0, 0, 'ann', 'ANN')"#,
        r#"INSERT INTO "FerrofinIntroSkipperSegments" ("ItemId", "Type", "Start", "End")
           VALUES ('a8b7c6d5-e4f3-4a2b-9c1d-0e1f2a3b4c5d', 0, 12.5, 88.0),
                  ('a8b7c6d5-e4f3-4a2b-9c1d-0e1f2a3b4c5d', 1, 1300.0, 1380.0)"#,
        r#"INSERT INTO "FerrofinUserDataRetentionSnapshots" ("ItemId", "UserId", "CustomDataKey",
            "IsFavorite", "PlayCount", "PlaybackPositionTicks", "Played", "RetentionDate")
           VALUES ('B1C2D3E4-F5A6-4B7C-8D9E-0F1A2B3C4D5E', ?1, 'tmdb-603', 1, 2, 0, 1,
                   '2026-11-01 00:00:00.0000000')"#,
        r#"INSERT INTO "FerrofinUserDataRetentionKeys" VALUES (1, 'identity', 'tmdb-603')"#,
        // What the boot repairs leave once they ran, which the export asks for
        // before it records their routines as applied (gl-1902).
        r#"INSERT OR REPLACE INTO "FerrofinMeta" ("Key", "Value") VALUES
            ('rehome_local_versions_v12', '1'), ('rederive_adopted_versions_v160', '1'),
            ('owner_id_relationships_v12', '1'),
            ('rating_levels_v122', '1'), ('missing_item_sweeps_v12', '1')"#,
        r#"UPDATE "FerrofinAdoption" SET "MembershipImportDone" = 1"#,
    ] {
        let query = sqlx::query(statement);
        // Only the user and the snapshot name the user.
        let query = if statement.contains("?1") {
            query.bind(user)
        } else {
            query
        };
        query.execute(db.writer()).await.unwrap();
    }
    db.record_started(ferrofin_health::build_version())
        .await
        .unwrap();
    db.pool().close().await;
    db.writer().close().await;
    source
}

/// The row count of every table of Ferrofin's in the database at `path`.
async fn ferrofin_tables(path: &Path) -> Vec<(String, i64)> {
    let mut conn = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .connect()
        .await
        .unwrap();
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' \
         AND (name GLOB 'Ferrofin*' OR name = '_sqlx_migrations') ORDER BY name",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    let mut tables = Vec::new();
    for name in names {
        let rows = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            r#"SELECT COUNT(*) FROM "{name}""#
        )))
        .fetch_one(&mut conn)
        .await
        .unwrap();
        tables.push((name, rows));
    }
    sqlx::Connection::close(conn).await.unwrap();
    tables
}

/// Every table (its columns) and index (its columns, an expression as
/// `<expr>`, and whether it is unique) of `conn`'s database, one line each,
/// SQLite's own objects and the partial indexes' `WHERE` aside: the schema
/// conformance test compares those.
async fn schema(conn: &mut sqlx::SqliteConnection) -> std::collections::BTreeSet<String> {
    let objects: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT type, name, tbl_name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let mut lines = std::collections::BTreeSet::new();
    for (kind, name, table) in objects {
        let detail: Vec<String> = match kind.as_str() {
            "table" => sqlx::query_as::<_, (String, String, i64, Option<String>, i64)>(
                "SELECT name, upper(type), \"notnull\", dflt_value, pk FROM pragma_table_info(?1)",
            )
            .bind(&name)
            .fetch_all(&mut *conn)
            .await
            .unwrap()
            .into_iter()
            .map(|column| format!("{column:?}"))
            .collect(),
            "index" => sqlx::query_scalar(
                "SELECT coalesce(name, '<expr>') FROM pragma_index_info(?1) \
                 UNION ALL SELECT 'unique=' || \"unique\" FROM pragma_index_list(?2) WHERE name = ?1",
            )
            .bind(&name)
            .bind(&table)
            .fetch_all(&mut *conn)
            .await
            .unwrap(),
            _ => Vec::new(),
        };
        lines.insert(format!("{kind} {name} on {table}: {detail:?}"));
    }
    lines
}

/// B7: the exported database of an adopted, used source holds Jellyfin's
/// schema and nothing of Ferrofin's, and the loss report names every table of
/// Ferrofin's that held rows, with its count.
#[tokio::test]
async fn export_jellyfin_reports_every_ferrofin_table_and_leaves_jellyfins_schema() {
    let scratch = tempfile::tempdir().unwrap();
    let source = adopted_source(scratch.path()).await;
    let held = ferrofin_tables(&source.join("data/jellyfin.db")).await;
    let out = scratch.path().join("out");

    let run = export(&source, &out);

    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "{}\n{stdout}",
        String::from_utf8_lossy(&run.stderr)
    );
    let report = std::fs::read_to_string(out.join(LOSS_REPORT)).unwrap();
    assert!(stdout.ends_with(&report), "{stdout}");
    for wanted in [
        "FerrofinIntroSkipperSegments",
        "FerrofinUserDataRetentionKeys",
        "FerrofinUserDataRetentionSnapshots",
        "FerrofinAdoption",
        "_sqlx_migrations",
    ] {
        assert!(
            held.iter().any(|(name, rows)| name == wanted && *rows > 0),
            "{wanted} holds rows in the source: {held:?}"
        );
    }
    for (name, rows) in held.iter().filter(|(_, rows)| *rows > 0) {
        let noun = if *rows == 1 { "row" } else { "rows" };
        assert!(
            report.contains(&format!("\n  {name} ({rows} {noun}): ")),
            "{name} held {rows} rows and is not in the report:\n{report}"
        );
    }
    assert!(
        report.contains(
            "(the database was adopted as Jellyfin 10.11.8):\n  DisableLegacyAuthorization: "
        ),
        "{report}"
    );
    assert!(
        report.contains(
            "Backups and scheduled tasks:\n  1 backup archive in data/backups made by Ferrofin, which Jellyfin cannot restore.\n  Scheduled tasks: "
        ),
        "{report}"
    );

    let mut exported = SqliteConnectOptions::new()
        .filename(out.join("data/jellyfin.db"))
        .read_only(true)
        .connect()
        .await
        .unwrap();
    let mut pinned = SqliteConnectOptions::new()
        .in_memory(true)
        .connect()
        .await
        .unwrap();
    sqlx::raw_sql(JELLYFIN_12_2_SCHEMA)
        .execute(&mut pinned)
        .await
        .unwrap();
    let (exported_schema, pinned_schema) = (schema(&mut exported).await, schema(&mut pinned).await);
    assert_eq!(
        exported_schema
            .difference(&pinned_schema)
            .collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "only in the export"
    );
    assert_eq!(
        pinned_schema
            .difference(&exported_schema)
            .collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "only in Jellyfin's schema"
    );
    let sequences: Vec<String> = sqlx::query_scalar("SELECT name FROM sqlite_sequence")
        .fetch_all(&mut exported)
        .await
        .unwrap();
    assert!(
        sequences.iter().all(|name| !name.starts_with("Ferrofin")),
        "{sequences:?}"
    );
    sqlx::Connection::close(exported).await.unwrap();
    sqlx::Connection::close(pinned).await.unwrap();
}

/// The LDAP-Auth bind password the export writes into the plugin's settings
/// and must never print.
const BIND_PASSWORD: &str = "bind-secret-1556";

/// An adopted data directory ([`adopted_source`]) where Ferrofin ran Live TV
/// (a tuner, a guide, a timer that recorded, a series timer, a channel item a
/// user made a favourite) and plugins: LDAP-Auth with a directory user's
/// link and its bind password, TMDb with a changed setting, a WASM plugin's
/// settings, and Jellyfin's own Intro Skipper settings and timer files from
/// before adoption, which Ferrofin never read.
async fn live_tv_source(scratch: &Path) -> PathBuf {
    let source = adopted_source(scratch).await;
    let user = "5D1E2C3B-4A59-4687-9C8B-7A6F5E4D3C2B";
    let channel = "714EE2D0-C390-F438-28ED-AFC33B39CC9A";
    let recording = source.join("data/livetv/recordings/Quiz Night/Round 1.ts");
    let statements = [
            format!(
                r#"INSERT INTO "BaseItems" ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked",
                    "IsMovie", "IsRepeat", "IsSeries", "IsVirtualItem", "Name")
                   VALUES ('{channel}', 'MediaBrowser.Controller.LiveTv.LiveTvChannel', 0, 0, 0, 0, 0, 0, 0,
                           'Test News')"#
            ),
            format!(
                r#"INSERT INTO "UserData" ("ItemId", "UserId", "CustomDataKey", "IsFavorite",
                    "PlayCount", "PlaybackPositionTicks", "Played")
                   VALUES ('{channel}', '{user}', 'channel', 1, 0, 0, 0)"#
            ),
            r#"INSERT INTO "FerrofinLiveTvTunerHosts" ("Id", "Url", "Type", "Data")
               VALUES ('t', 'http://tuner.lan/a.m3u', 'm3u',
                       '{"Id":"t","Url":"http://tuner.lan/a.m3u","Type":"m3u","FriendlyName":"Attic"}')"#.to_owned(),
            r#"INSERT INTO "FerrofinLiveTvListingProviders" ("Id", "Type", "Path", "Data")
               VALUES ('g', 'xmltv', 'http://tuner.lan/guide.xml',
                       '{"Id":"g","Type":"xmltv","Path":"http://tuner.lan/guide.xml","Password":"sd-secret"}')"#.to_owned(),
            format!(
                r#"INSERT INTO "FerrofinLiveTvTimers" ("Id", "ChannelId", "StartDate", "Status", "Data", "IsManual")
                   VALUES ('timer', '{channel}', '2026-11-22 20:00:00', 'Completed',
                           '{{"ExternalId":"ferrofin-timer","ExternalChannelId":"m3u_x","Name":"Quiz Night","Status":"Completed"}}', 1)"#
            ),
            format!(
                r#"INSERT INTO "FerrofinLiveTvRecordings" ("Id", "ChannelId", "TimerId", "StartDate",
                    "Status", "Path", "EpisodeTitle")
                   VALUES ('rec', '{channel}', 'timer', '2026-11-22 20:00:00', 'Completed', '{}', 'Round 1')"#,
                recording.display()
            ),
            r#"INSERT INTO "FerrofinLiveTvSeriesTimers" ("Id", "Data", "ExternalId", "SeriesId")
               VALUES ('series', '{"Name":"Quiz Night","RecordAnyTime":true}', 'ferrofin-series', 'quiz')"#.to_owned(),
            format!(
                r#"INSERT INTO "FerrofinExternalIdentities" ("UserId", "ProviderId", "Subject", "DateCreated")
                   VALUES ('{user}', 'Jellyfin.Plugin.LDAP_Auth.LdapAuthenticationProviderPlugin', 'ann',
                           '2026-11-21 00:00:00')"#
            ),
    ];
    alter(
        &source,
        &statements.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
    std::fs::create_dir_all(recording.parent().unwrap()).unwrap();
    std::fs::write(&recording, b"ts").unwrap();
    let plugins = source.join("config/plugins");
    for (id, settings) in [
        (
            "958aad66-3784-4d2a-b89a-a7b6fab6e25c",
            format!(r#"{{"LdapServer":"ldap.lan","LdapBindPassword":"{BIND_PASSWORD}"}}"#),
        ),
        (
            "b8715ed1-6c47-4528-9ad3-f72deb539cd4",
            r#"{"MaxCastMembers":9}"#.to_owned(),
        ),
        (
            "5f0e8d1c-0000-4000-8000-000000000001",
            r#"{"Anything":1}"#.to_owned(),
        ),
    ] {
        std::fs::create_dir_all(plugins.join(id)).unwrap();
        std::fs::write(plugins.join(id).join("config.json"), settings).unwrap();
    }
    std::fs::create_dir_all(source.join("plugins/configurations")).unwrap();
    std::fs::write(
        source.join("plugins/configurations/IntroSkipper.xml"),
        "<PluginConfiguration><MaxParallelism>4</MaxParallelism></PluginConfiguration>",
    )
    .unwrap();
    std::fs::create_dir_all(source.join("data/livetv")).unwrap();
    std::fs::write(
        source.join("data/livetv/timers.json"),
        r#"[{"Id":"jellyfin-timer","Name":"Before adoption"}]"#,
    )
    .unwrap();
    source
}

/// Live TV goes out as Jellyfin keeps it, with the channel item and its
/// favourite kept in the database; each plugin with a counterpart and
/// settings in Ferrofin gets its settings file, private, LDAP-Auth's with its
/// uid map; Jellyfin's own settings of a plugin Ferrofin never held settings
/// for stay as they were; and the source is left byte for byte as it was,
/// with no secret printed.
#[tokio::test]
async fn export_jellyfin_writes_live_tv_and_plugin_settings() {
    use std::os::unix::fs::PermissionsExt as _;

    let scratch = tempfile::tempdir().unwrap();
    let source = live_tv_source(scratch.path()).await;
    let before = files(&source);
    let out = scratch.path().join("out");

    let run = export(&source, &out);

    let (stdout, stderr) = (
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
    );
    assert!(run.status.success(), "{stderr}\n{stdout}");
    assert_eq!(
        files(&source),
        before,
        "the source is left byte for byte as it was"
    );
    for secret in [BIND_PASSWORD, "sd-secret"] {
        assert!(
            !stdout.contains(secret) && !stderr.contains(secret),
            "{secret}"
        );
    }
    let read = |path: &str| std::fs::read_to_string(out.join(path)).unwrap();
    let mode = |path: &str| {
        std::fs::metadata(out.join(path))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };

    let livetv = read("config/livetv.xml");
    assert!(
        livetv.contains("<Url>http://tuner.lan/a.m3u</Url>"),
        "{livetv}"
    );
    assert!(
        livetv.contains("<Password>sd-secret</Password>"),
        "{livetv}"
    );
    assert_eq!(mode("config/livetv.xml"), 0o600);
    let timers: serde_json::Value = serde_json::from_str(&read("data/livetv/timers.json")).unwrap();
    assert_eq!(timers.as_array().unwrap().len(), 1, "{timers}");
    assert_eq!(timers[0]["Id"], "ferrofin-timer");
    assert_eq!(timers[0]["IsManual"], true);
    assert_eq!(
        timers[0]["RecordingPath"],
        "/config/data/livetv/recordings/Quiz Night/Round 1.ts"
    );
    let series: serde_json::Value =
        serde_json::from_str(&read("data/livetv/seriestimers.json")).unwrap();
    assert_eq!(series[0]["Id"], "ferrofin-series");
    assert_eq!(series[0]["SeriesId"], "quiz");

    let ldap = read("plugins/configurations/LDAP-Auth.xml");
    assert!(ldap.contains("<LdapServer>ldap.lan</LdapServer>"), "{ldap}");
    assert!(
        ldap.contains(&format!(
            "<LdapBindPassword>{BIND_PASSWORD}</LdapBindPassword>"
        )),
        "{ldap}"
    );
    assert!(
        ldap.contains(
            "<LdapUser>\n      <LdapUid>ann</LdapUid>\n      <LinkedJellyfinUserId>5d1e2c3b-4a59-4687-9c8b-7a6f5e4d3c2b</LinkedJellyfinUserId>"
        ),
        "{ldap}"
    );
    assert_eq!(mode("plugins/configurations/LDAP-Auth.xml"), 0o600);
    assert!(
        read("plugins/configurations/Jellyfin.Plugin.Tmdb.xml")
            .contains("<MaxCastMembers>9</MaxCastMembers>")
    );
    assert_eq!(
        read("plugins/configurations/IntroSkipper.xml"),
        "<PluginConfiguration><MaxParallelism>4</MaxParallelism></PluginConfiguration>",
        "Jellyfin's own, which Ferrofin never read, is kept"
    );

    let report = read(LOSS_REPORT);
    for line in [
        "  Written for Jellyfin: 1 tuner and 1 guide provider in config/livetv.xml, 1 timer (1 completed recording among them) in data/livetv/timers.json, 1 series timer in data/livetv/seriestimers.json.\n",
        "  Kept in the database: 1 channel and 0 guide programmes, which Jellyfin reads as its own, with the users' data on them (1 favourite channel).",
        "  1 timer and 0 series timers that Jellyfin kept before adoption and Ferrofin does not hold are replaced by Ferrofin's.\n",
        "  Settings written for Jellyfin in plugins/configurations: TMDb, LDAP-Auth.\n",
        "  LDAP-Auth: 1 link from directory uids to users in its settings;",
        "  Kept as Jellyfin left them before adoption, as Ferrofin never held settings for them: the settings of Intro Skipper.\n",
        "  No Jellyfin counterpart, so their settings are not carried: a WASM plugin (5f0e8d1c-0000-4000-8000-000000000001).\n",
    ] {
        assert!(report.contains(line), "{line}\nnot in:\n{report}");
    }

    assert_eq!(
        channel_favourite(&out).await,
        1,
        "the channel and its favourite are kept"
    );
}

/// The favourite flag on the channel item of the database exported into `out`.
async fn channel_favourite(out: &Path) -> i64 {
    let mut exported = SqliteConnectOptions::new()
        .filename(out.join("data/jellyfin.db"))
        .read_only(true)
        .connect()
        .await
        .unwrap();
    let favourite: i64 = sqlx::query_scalar(
        r#"SELECT u."IsFavorite" FROM "UserData" u JOIN "BaseItems" b ON b."Id" = u."ItemId"
           WHERE b."Type" = 'MediaBrowser.Controller.LiveTv.LiveTvChannel'"#,
    )
    .fetch_one(&mut exported)
    .await
    .unwrap();
    sqlx::Connection::close(exported).await.unwrap();
    favourite
}
