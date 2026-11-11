//! `ferrofin-server export-jellyfin` on a scratch data directory (gl-1548):
//! the binary copies the stopped directory, writes a database snapshot that
//! holds what the write-ahead log held, writes Ferrofin's configuration as the
//! XML Jellyfin reads (gl-1579), leaves the source byte for byte as it was,
//! and gives the same output on a second run. It refuses an export that would
//! be unsafe or meaningless (gl-1550), naming the cause, and a failed export
//! leaves no output.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
    // Ferrofin's own bookkeeping table: what makes it a Ferrofin database.
    sqlx::query("CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY)")
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
const CONFIGURATION: [&str; 8] = [
    "config/system.xml",
    "config/encoding.xml",
    "config/branding.xml",
    "config/network.xml",
    "config/metadata.xml",
    "config/xbmcmetadata.xml",
    "data/device.txt",
    "root/default/Movies/options.xml",
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
    command(source, out, port)
        .output()
        .expect("run ferrofin-server export-jellyfin")
}

/// The command [`export_on`] runs, for a test to add to.
fn command(source: &Path, out: &Path, port: u16) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferrofin-server"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("FERROFIN_") {
            command.env_remove(key);
        }
    }
    command
        .arg("--bind")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
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
    // The certificate is reported by name, and its password nowhere.
    assert!(
        stdout.contains("Not exported: CertificatePassword in config/network.xml"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Not exported: CertificatePath in config/network.xml"),
        "{stdout}"
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
    let run = command(&source, Path::new("."), free_port())
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
            command(&source, &out, port)
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
/// and a write-ahead log damaged before its end.
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
