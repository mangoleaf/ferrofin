//! The statements a library scan runs for each episode it adds (gl-1084),
//! and for each one a re-scan finds unchanged (gl-1194).
//!
//! The scan refreshes its items one at a time and awaits every statement, so
//! what one item costs sets the scan's speed once the probes keep up. The
//! test scans the same series twice, over fresh servers, with a different
//! number of episodes, then scans each library again, and counts the extra
//! statements per extra episode: a statement the scan runs once, or once per
//! series or season, cancels out.
//!
//! The server is the real composition root over a temp data dir, and the
//! statements are the `sqlx::query` events recorded inside the scan's
//! `library_scan` span. That span is a root span, so the binary holds this one
//! test: a second scan running beside it would be recorded too.

use std::sync::Mutex;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use ferrofin_traits::library::ScanTrigger;
use tower::ServiceExt as _;
use tracing_subscriber::layer::{Context, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "scan-statements-pw";

const CLIENT_AUTH: &str = r#"MediaBrowser Client="Scan Statements", Device="Test Rig", DeviceId="rig-ss", Version="1.0.0""#;

/// The root span every library scan runs in (`LibraryManager`).
const SCAN_SPAN: &str = "library_scan";

/// The statements recorded inside [`SCAN_SPAN`], in order.
static STATEMENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Records the text of every `sqlx::query` event emitted inside a scan.
struct Statements;

impl<S> tracing_subscriber::Layer<S> for Statements
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        let in_scan = ctx
            .event_scope(event)
            .is_some_and(|mut scope| scope.any(|span| span.name() == SCAN_SPAN));
        if !in_scan {
            return;
        }
        let mut text = StatementText(String::new());
        event.record(&mut text);
        STATEMENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(text.0.split_whitespace().collect::<Vec<_>>().join(" "));
    }
}

/// sqlx's `db.statement` (the whole statement), else its `summary`.
struct StatementText(String);

impl tracing::field::Visit for StatementText {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.take(field, value);
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.take(field, &format!("{value:?}"));
    }
}

impl StatementText {
    fn take(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "db.statement" if !value.trim().is_empty() => value.clone_into(&mut self.0),
            "summary" if self.0.is_empty() => value.clone_into(&mut self.0),
            _ => {}
        }
    }
}

/// Installs [`Statements`] as the process's subscriber, once.
fn record_statements() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Statements))
            .expect("no other subscriber in this binary");
    });
}

/// Takes what was recorded so far.
fn take_statements() -> Vec<String> {
    std::mem::take(
        &mut *STATEMENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// An NFO crediting `people` as `kind`, without provider ids.
fn credits(people: &[String], kind: &str) -> String {
    use std::fmt::Write as _;
    let mut xml = String::new();
    for (order, name) in people.iter().enumerate() {
        let _ = writeln!(
            xml,
            "  <actor><name>{name}</name><role>Role {order}</role>\
             <type>{kind}</type><order>{order}</order></actor>"
        );
    }
    xml
}

/// Writes one series with one season of `episodes` episodes under `root`:
/// series and season NFOs, each episode a placeholder file (the probe fails on
/// it, as on a damaged file) with an NFO crediting two guest stars and giving
/// it two genres and a studio, the same for every episode (so each by-name row
/// is made once, whatever the episode count), and two tags of its own.
fn write_series(root: &std::path::Path, episodes: usize) {
    let series = root.join("Bench Show (2001)");
    let season = series.join("Season 01");
    std::fs::create_dir_all(&season).expect("season dir");
    let cast: Vec<String> = (0..3).map(|i| format!("Series Actor {i}")).collect();
    std::fs::write(
        series.join("tvshow.nfo"),
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<tvshow>\n  <title>Bench Show</title>\n  \
             <year>2001</year>\n  <plot>A show.</plot>\n{}</tvshow>\n",
            credits(&cast, "Actor")
        ),
    )
    .expect("tvshow.nfo");
    std::fs::write(
        season.join("season.nfo"),
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<season>\n  <title>Season 1</title>\n  \
         <seasonnumber>1</seasonnumber>\n</season>\n",
    )
    .expect("season.nfo");
    for episode in 1..=episodes {
        let base = format!("Bench Show (2001) - S01E{episode:02} - Episode {episode}");
        std::fs::write(season.join(format!("{base}.mkv")), [0u8; 1024]).expect("episode");
        let guests: Vec<String> = (0..2).map(|k| format!("Guest {episode}-{k}")).collect();
        std::fs::write(
            season.join(format!("{base}.nfo")),
            format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<episodedetails>\n  \
                 <title>Episode {episode}</title>\n  <season>1</season>\n  \
                 <episode>{episode}</episode>\n  <plot>Episode {episode}.</plot>\n  \
                 <genre>Drama</genre>\n  <genre>Comedy</genre>\n  \
                 <studio>Bench Studio</studio>\n  <tag>Tag {episode}-0</tag>\n  \
                 <tag>Tag {episode}-1</tag>\n{}\
                 </episodedetails>\n",
                credits(&guests, "GuestStar")
            ),
        )
        .expect("episode nfo");
    }
}

/// Sends `method uri` with `auth_header` and an optional JSON body; returns
/// status + JSON body.
async fn send(
    router: &axum::Router,
    auth_header: &str,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, auth_header);
    let body = match body {
        Some(json) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    let response = router
        .clone()
        .oneshot(request.body(body).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("valid JSON body")
    };
    (status, json)
}

/// Percent-encodes a path for a query string, keeping `/`.
fn urlencoding(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(char::from(byte));
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// The statements of a library's first scan and of a re-scan that finds it
/// unchanged.
struct Scans {
    first: Vec<String>,
    rescan: Vec<String>,
}

/// Boots a server over a temp data dir, adds a TV library holding one series
/// of `episodes` episodes (no remote fetchers: the scan must not wait on the
/// network), scans it, then scans it again; returns the statements each scan
/// ran.
async fn scan_statements(episodes: usize) -> Scans {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(temp.path().join("config")).expect("config dir");
    let config = Config {
        server_name: "ferrofin-scan-statements".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    let shows = temp.path().join("shows");
    write_series(&shows, episodes);
    std::fs::create_dir_all(&config.data_dir).expect("data dir");
    let db = Database::connect(&config.database_url())
        .await
        .expect("open db");
    db.run_migrations().await.expect("migrations");
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        encoder_app_path_display: None,
        ffmpeg: std::path::PathBuf::from("ffmpeg"),
        ffprobe: std::path::PathBuf::from("ffprobe"),
        capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
        chromaprint_muxer: false,
    };
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let wired: WiredApp = build_app_state(&db, &config, &ffmpeg, None, shutdown_tx)
        .await
        .expect("wire app state");
    ferrofin_server::seed::seed_default_admin(wired.state.users.as_ref(), &config)
        .await
        .expect("seed admin");
    let router = ferrofin_api::create_router(wired.state.clone());
    let (status, auth) = send(
        &router,
        CLIENT_AUTH,
        "POST",
        "/Users/AuthenticateByName",
        Some(serde_json::json!({ "Username": ADMIN_USER, "Pw": ADMIN_PASSWORD })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "admin authenticates: {auth}");
    let token = auth["AccessToken"].as_str().expect("token");
    let auth_header = format!("{CLIENT_AUTH}, Token=\"{token}\"");
    let no_fetchers =
        |kind: &str| serde_json::json!({"Type": kind, "MetadataFetchers": [], "ImageFetchers": []});
    let (status, body) = send(
        &router,
        &auth_header,
        "POST",
        &format!(
            "/Library/VirtualFolders?name=Shows&collectionType=tvshows&paths={}&refreshLibrary=false",
            urlencoding(&shows.to_string_lossy())
        ),
        Some(serde_json::json!({"LibraryOptions": {"TypeOptions": [
            no_fetchers("Series"), no_fetchers("Season"), no_fetchers("Episode")
        ]}})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "library added: {body}");
    take_statements();
    let mut scans = [Vec::new(), Vec::new()];
    for statements in &mut scans {
        assert!(
            wired
                .state
                .library
                .run_library_scan(ScanTrigger::Api, None)
                .await
                .expect("scan"),
            "the scan ran"
        );
        *statements = take_statements();
    }
    let [first, rescan] = scans;
    let (status, page) = send(
        &router,
        &auth_header,
        "GET",
        &format!(
            "/Items?userId={}&recursive=true&includeItemTypes=Episode&limit=0",
            auth["User"]["Id"].as_str().expect("user id")
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "episodes: {page}");
    assert_eq!(
        page["TotalRecordCount"].as_u64(),
        u64::try_from(episodes).ok(),
        "every episode was added: {page}"
    );
    Scans { first, rescan }
}

/// The statements of a scan of `few` episodes and of one of `many`, and how
/// many more episodes the second had.
struct PerEpisode {
    few: Vec<String>,
    many: Vec<String>,
    extra: usize,
}

impl PerEpisode {
    /// How many more statements matching `matches` the larger scan ran.
    fn more(&self, matches: impl Fn(&str) -> bool) -> usize {
        let count = |statements: &[String]| statements.iter().filter(|s| matches(s)).count();
        count(&self.many).saturating_sub(count(&self.few))
    }

    /// `per_episode` statements for each extra episode.
    fn each(&self, per_episode: usize) -> usize {
        per_episode * self.extra
    }

    /// Every statement, per extra episode, for the test's output.
    #[allow(clippy::cast_precision_loss)] // a few hundred statements
    fn total_per_episode(&self) -> f64 {
        self.more(|_| true) as f64 / self.extra as f64
    }

    /// The statements of the larger scan, one per line, for a failure message.
    fn listing(&self) -> String {
        self.many.join("\n")
    }
}

#[tokio::test]
async fn an_episode_reads_neither_the_library_listing_nor_its_row_twice() {
    record_statements();
    let few = 2;
    let many = 5;
    let (few_scans, many_scans) = (scan_statements(few).await, scan_statements(many).await);
    let scans = PerEpisode {
        few: few_scans.first,
        many: many_scans.first,
        extra: many - few,
    };
    let rescans = PerEpisode {
        few: few_scans.rescan,
        many: many_scans.rescan,
        extra: many - few,
    };
    let listing = scans.listing();
    println!("{} statements per episode", scans.total_per_episode());

    // The dashboard's full listing re-heals each library's CollectionFolder
    // row (`SELECT 1`) and reads its images. The scan runs it once, at its
    // start; a per-item lookup reads the libraries' snapshot instead, as
    // upstream's `GetLibraryOptions(item)` reads them from memory. Both texts
    // are shared with other callers (`item_exists`, `get_image_infos`): a
    // new per-episode one trips this too.
    let heal = |s: &str| s == r#"SELECT 1 FROM "BaseItems" WHERE "Id" = ?1"#;
    let library_images = |s: &str| {
        s == r#"SELECT * FROM "BaseItemImageInfos" WHERE "ItemId" = ?1 ORDER BY "ImageType", rowid"#
    };
    assert!(
        scans.many.iter().any(|s| heal(s)) && scans.many.iter().any(|s| library_images(s)),
        "the scan's own listing is recorded:\n{listing}"
    );
    assert_eq!(
        (scans.more(heal), scans.more(library_images)),
        (0, 0),
        "an episode lists no library:\n{listing}"
    );

    // The NFO saver reads the row it may save once: a scan's `MetadataImport`
    // never saves an episode's NFO (`IsEnabledFor`, checked first upstream).
    // A ratchet: a change that drops this read lowers it.
    assert_eq!(
        scans.more(|s| s == r#"SELECT * FROM "BaseItems" WHERE "Id" = ?1 AND "Id" <> ?2"#),
        scans.each(1),
        "the NFO saver reads an episode's row once:\n{listing}"
    );

    // The credited people's rows and locks serve a TMDB biography fetch,
    // which needs a TMDB id these guests do not have. Their credits are
    // written, so the people pass ran.
    //
    // An episode's cast is written a few statements at a time, not one or
    // more per guest (gl-1181): each read (the name match, the person items
    // by derived id, then, for the guests with none, the same person under
    // another id, and the biographies) is one statement up to 256 binds,
    // and each insert (the new `Peoples` rows, the new `Person` items, the
    // credits) one per power-of-two run of its rows. With two new guests,
    // each step is one.
    for (step, prefix) in [
        (
            "name match",
            r#"SELECT "Id", "Name", "PersonType" FROM "Peoples""#,
        ),
        ("new people", r#"INSERT INTO "Peoples""#),
        ("person items by id", PERSON_ITEMS_BY_ID),
        ("same-person match", SAME_PERSON_MATCH),
        ("biographies", r#"SELECT "Id", "Overview" FROM "BaseItems""#),
        (
            "new person items",
            r#"INSERT OR IGNORE INTO "BaseItems" ("Id","Type","Name","CleanName""#,
        ),
        ("credits", r#"INSERT INTO "PeopleBaseItemMap""#),
    ] {
        assert_eq!(
            scans.more(|s| s.starts_with(prefix)),
            scans.each(1),
            "an episode's {step} is one statement:\n{listing}"
        );
    }
    assert_eq!(
        scans.more(|s| {
            s.starts_with(
                r#"SELECT "ItemId", "Id" FROM "BaseItemMetadataFields" WHERE "ItemId" IN"#,
            )
        }),
        0,
        "an episode reads no person's locks:\n{listing}"
    );

    // The ancestor closure is one statement, whatever the depth.
    assert_eq!(
        scans.more(|s| s.starts_with(r#"INSERT OR IGNORE INTO "AncestorIds""#)),
        scans.each(1),
        "an episode writes its ancestors in one statement:\n{listing}"
    );

    // An episode's genres, studios and tags are written a few statements at a
    // time, not four per value (gl-1282): the id read is one statement up to
    // 128 values, and each insert one per power-of-two run of its rows. The
    // episode's 5 values are a run of 4 and one of 1, and so are their links;
    // its 3 by-name rows (the genres and the studio) a run of 2 and one of 1.
    for (step, needle, statements) in [
        ("value rows", r#"INSERT OR IGNORE INTO "ItemValues" ("#, 2),
        ("value ids", r#""ItemValueId" FROM "ItemValues" WHERE"#, 1),
        ("links", r#"INSERT OR IGNORE INTO "ItemValuesMap""#, 2),
        (
            "by-name rows",
            r#"INSERT INTO "BaseItems" ("Id","Type","Name","CleanName","SortName","PresentationUniqueKey","#,
            2,
        ),
    ] {
        assert_eq!(
            scans.more(|s| s.contains(needle)),
            scans.each(statements),
            "the statement count of an episode's {step}:\n{listing}"
        );
    }

    // The parental score reads the rows above an episode only when the item
    // scored before it did not hold them (gl-1252): every episode after a
    // season's first shares the chain that one read, on a re-scan too.
    let reach = |s: &str| s.contains("WITH RECURSIVE reach(");
    assert_eq!(
        (scans.more(reach), rescans.more(reach)),
        (0, 0),
        "an episode reads no parent chain the one before it held:\n{listing}"
    );

    a_rescanned_episode_reads_its_cast_in_batches(&rescans);
}

/// The read of a cast's `Person` items by the ids their names derive (gl-1079).
const PERSON_ITEMS_BY_ID: &str = r#"SELECT "Id" FROM "BaseItems" WHERE +"Type" = ?1 AND "Id" IN ("#;

/// The read of the rows sharing a clean name with the names no derived id
/// found: the same person under another id (gl-1079).
const SAME_PERSON_MATCH: &str = r#"SELECT "CleanName", "Name", "Id" FROM "BaseItems""#;

/// A re-scan finds every episode's cast unchanged (gl-443) and writes none of
/// it. The people it hands the biography fetch are read a few statements at a
/// time too (gl-1194), not two per guest: their items by derived id and the
/// biographies, each one statement up to 256 binds; every guest has an item
/// at its id by then, so the same-person match is never read.
fn a_rescanned_episode_reads_its_cast_in_batches(rescans: &PerEpisode) {
    let relisting = rescans.listing();
    println!(
        "{} statements per re-scanned episode",
        rescans.total_per_episode()
    );
    for (step, prefix) in [
        ("person items by id", PERSON_ITEMS_BY_ID),
        ("biographies", r#"SELECT "Id", "Overview" FROM "BaseItems""#),
    ] {
        assert_eq!(
            rescans.more(|s| s.starts_with(prefix)),
            rescans.each(1),
            "a re-scanned episode's {step} is one statement:\n{relisting}"
        );
    }
    assert_eq!(
        rescans.more(|s| s.starts_with(SAME_PERSON_MATCH)),
        0,
        "a re-scanned episode's guests are all found by id:\n{relisting}"
    );
    assert_eq!(
        rescans.more(|s| s == r#"SELECT "Overview" FROM "BaseItems" WHERE "Id" = ?1"#),
        0,
        "a re-scanned episode reads no guest's biography alone:\n{relisting}"
    );
    for (step, prefix) in [
        ("credit rewrite", r#"DELETE FROM "PeopleBaseItemMap""#),
        ("credits", r#"INSERT INTO "PeopleBaseItemMap""#),
        ("new people", r#"INSERT INTO "Peoples""#),
    ] {
        assert_eq!(
            rescans.more(|s| s.starts_with(prefix)),
            0,
            "a re-scanned episode writes no {step}:\n{relisting}"
        );
    }
}
