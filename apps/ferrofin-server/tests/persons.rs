//! `GET /Persons` paging against the real composition root.
//!
//! Jellyfin 12.2's `GetPeopleItems` sets `MustHaveItem`: a credit with no
//! by-name `Person` item is neither returned nor counted, so `TotalRecordCount`
//! is the number of people a client can actually page through. This boots the
//! server over a temp DB, scans a movie whose NFO credits five actors, deletes
//! two of their `Person` items, and checks over real HTTP that every page
//! reports the three that remain and the pages tile exactly that set.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "persons-pw";

const ACTORS: [&str; 5] = ["Ada", "Ben", "Cy", "Dee", "Eli"];

/// A booted server plus what the test drives it with.
struct Harness {
    wired: WiredApp,
    db: Database,
    /// The movies library's media directory.
    media_dir: String,
    _temp: tempfile::TempDir,
}

/// The NFO body of a movie crediting `actors`, with `extra` elements.
fn movie_nfo(title: &str, actors: &[&str], extra: &str) -> String {
    let actors: Vec<_> = actors
        .iter()
        .map(|name| format!("<actor><name>{name}</name><type>Actor</type></actor>"))
        .collect();
    format!(
        "<movie><title>{title}</title>{extra}{}</movie>",
        actors.concat()
    )
}

/// Boots the composition root over a temp DB and seeds the admin. Each
/// `(dir, nfo)` becomes a movie under the media directory: `dir/<name>.mkv`
/// beside a `movie.nfo` holding `nfo`.
async fn boot(movies: &[(&str, String)]) -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(temp.path().join("config")).expect("config dir");
    let config = Config {
        server_name: "ferrofin-persons".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    let media_dir = temp.path().join("media");
    for (dir, nfo) in movies {
        let movie = media_dir.join(dir);
        let name = movie.file_name().expect("movie name").to_owned();
        std::fs::create_dir_all(&movie).expect("movie dir");
        std::fs::write(movie.join(name).with_extension("mkv"), [0u8; 1024]).expect("movie file");
        std::fs::write(movie.join("movie.nfo"), nfo).expect("nfo");
    }

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
    let wired = build_app_state(&db, &config, &ffmpeg, None, shutdown_tx)
        .await
        .expect("wire app state");
    ferrofin_server::seed::seed_default_admin(wired.state.users.as_ref(), &config)
        .await
        .expect("seed admin");
    Harness {
        wired,
        db,
        media_dir: media_dir.to_string_lossy().into_owned(),
        _temp: temp,
    }
}

/// Sends `method uri` with the session token; returns status + JSON body.
async fn send(
    router: &axum::Router,
    auth_header: &str,
    method: &str,
    uri: &str,
) -> (StatusCode, serde_json::Value) {
    send_json(router, auth_header, method, uri, None).await
}

/// [`send`] with an optional JSON request body.
async fn send_json(
    router: &axum::Router,
    auth_header: &str,
    method: &str,
    uri: &str,
    json: Option<&serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, auth_header);
    let request = match json {
        Some(json) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json.to_string())),
        None => request.body(Body::empty()),
    };
    let response = router
        .clone()
        .oneshot(request.expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("valid JSON body")
    };
    (status, body)
}

/// Authenticates the seeded admin; returns the token-bearing auth header.
async fn authenticate(router: &axum::Router) -> String {
    login(router, ADMIN_USER, ADMIN_PASSWORD).await
}

/// Authenticates `user` on a device of its own; returns the token-bearing
/// auth header.
async fn login(router: &axum::Router, user: &str, password: &str) -> String {
    let client = format!(
        r#"MediaBrowser Client="Persons", Device="Test Rig", DeviceId="rig-{user}", Version="1.0.0""#
    );
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(header::AUTHORIZATION, &client)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "Username": user, "Pw": password }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK, "{user} authenticates");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let auth: serde_json::Value = serde_json::from_slice(&bytes).expect("auth JSON");
    let token = auth["AccessToken"].as_str().expect("token");
    format!(r#"{client}, Token="{token}""#)
}

/// The `Name`s of a `/Persons` page.
fn names(page: &serde_json::Value) -> Vec<String> {
    page["Items"]
        .as_array()
        .expect("Items")
        .iter()
        .map(|p| p["Name"].as_str().expect("Name").to_owned())
        .collect()
}

/// Percent-encodes a filesystem path for a query string.
fn urlencoding(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => {
                use std::fmt::Write as _;
                // Writing to a `String` cannot fail.
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// Adds a movies library over `path` without scanning it.
async fn add_library(router: &axum::Router, auth: &str, name: &str, path: &str) {
    let path = urlencoding(path);
    let (status, body) = send(
        router,
        auth,
        "POST",
        &format!("/Library/VirtualFolders?name={name}&collectionType=movies&paths={path}&refreshLibrary=false"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "library added: {body}");
}

/// Runs a library scan and waits for the whole of it, so nothing it writes
/// later can undo what a test does next.
async fn scan(router: &axum::Router, auth: &str) {
    let refresh_end = async || {
        let (_, tasks) = send(router, auth, "GET", "/ScheduledTasks").await;
        let task = tasks
            .as_array()
            .expect("tasks")
            .iter()
            .find(|task| task["Key"] == "RefreshLibrary")
            .expect("RefreshLibrary task")
            .clone();
        (task["State"] == "Idle", task["LastExecutionResult"].clone())
    };
    let (_, previous) = refresh_end().await;
    let (status, _) = send(router, auth, "POST", "/Library/Refresh").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "scan queued");
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        let (idle, result) = refresh_end().await;
        if idle && result["EndTimeUtc"] != previous["EndTimeUtc"] {
            assert_eq!(result["Status"], "Completed", "{result}");
            break;
        }
        assert!(Instant::now() < deadline, "scan did not finish: {result}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn persons_total_counts_only_people_with_an_item_and_pages_tile_them() {
    let harness = boot(&[(
        "Credits Film (2001)",
        movie_nfo("Credits Film", &ACTORS, ""),
    )])
    .await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let auth = authenticate(&router).await;

    add_library(&router, &auth, "Movies", &harness.media_dir).await;
    scan(&router, &auth).await;

    // The scan credits every actor and materializes each one's Person item.
    let (_, page) = send(&router, &auth, "GET", "/Persons").await;
    assert_eq!(page["TotalRecordCount"], ACTORS.len(), "{page}");
    assert_eq!(names(&page), ACTORS);

    // Two credits lose their by-name item: they stay credited on the movie but
    // can no longer be handed back as a Person.
    let deleted = sqlx::query(
        r#"DELETE FROM "BaseItems"
           WHERE "Type" = 'MediaBrowser.Controller.Entities.Person' AND "Name" IN ('Ben', 'Dee')"#,
    )
    .execute(harness.db.writer())
    .await
    .expect("delete person items");
    assert_eq!(deleted.rows_affected(), 2);

    // Every page reports the three returnable people, and the pages tile them.
    let mut tiled = Vec::new();
    for start in [0, 2, 4] {
        let (status, page) = send(
            &router,
            &auth,
            "GET",
            &format!("/Persons?startIndex={start}&limit=2"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["TotalRecordCount"], 3, "page at {start}: {page}");
        assert_eq!(page["StartIndex"], start, "{page}");
        tiled.extend(names(&page));
    }
    assert_eq!(tiled, ["Ada", "Cy", "Eli"]);

    let (_, all) = send(&router, &auth, "GET", "/Persons").await;
    assert_eq!(all["TotalRecordCount"], 3, "{all}");
    assert_eq!(names(&all), ["Ada", "Cy", "Eli"]);

    // Listing never re-creates the missing items: the filtered names never
    // reach the by-name resolver (12.2's `GetPerson` only reads).
    let recreated: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "BaseItems"
           WHERE "Type" = 'MediaBrowser.Controller.Entities.Person' AND "Name" IN ('Ben', 'Dee')"#,
    )
    .fetch_one(harness.db.pool())
    .await
    .expect("count person items");
    assert_eq!(recreated, 0);
}

/// Issue #61: Jellyfin 12.2 hands `GetPeopleItems` an `AccessFilter` for a
/// user with content restrictions (`PersonsController.BuildAccessFilter`), so
/// `/Persons` lists and counts only people credited on an item the user can
/// see. Ada and Sam are credited in the open library, Sam and Bob in the
/// closed one, Rae only on an R-rated film and Tia only on one tagged `gore`.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn persons_lists_and_counts_only_people_credited_where_the_user_can_look() {
    let harness = boot(&[
        (
            "open/Open Film (2001)",
            movie_nfo("Open Film", &["Ada", "Sam"], ""),
        ),
        (
            "open/Rated Film (2002)",
            movie_nfo("Rated Film", &["Rae"], "<mpaa>R</mpaa>"),
        ),
        (
            "open/Tagged Film (2003)",
            movie_nfo("Tagged Film", &["Tia"], "<tag>gore</tag>"),
        ),
        (
            "closed/Closed Film (2004)",
            movie_nfo("Closed Film", &["Bob", "Sam"], "<genre>Western</genre>"),
        ),
    ])
    .await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let admin = authenticate(&router).await;
    add_library(
        &router,
        &admin,
        "Open",
        &format!("{}/open", harness.media_dir),
    )
    .await;
    add_library(
        &router,
        &admin,
        "Closed",
        &format!("{}/closed", harness.media_dir),
    )
    .await;
    scan(&router, &admin).await;

    let (_, folders) = send(&router, &admin, "GET", "/Library/VirtualFolders").await;
    let open = folders
        .as_array()
        .expect("folders")
        .iter()
        .find(|folder| folder["Name"] == "Open")
        .expect("Open library")["ItemId"]
        .clone();
    let (status, created) = send_json(
        &router,
        &admin,
        "POST",
        "/Users/New",
        Some(&serde_json::json!({ "Name": "viewer", "Password": "viewer-pw" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let viewer_id = created["Id"].as_str().expect("viewer id").to_owned();
    let viewer = login(&router, "viewer", "viewer-pw").await;
    let unrestricted = created["Policy"].clone();
    let set_policy = async |policy: &serde_json::Value| {
        let (status, body) = send_json(
            &router,
            &admin,
            "POST",
            &format!("/Users/{viewer_id}/Policy"),
            Some(policy),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    };
    let persons = async |uri: &str| {
        let (status, page) = send(&router, &viewer, "GET", uri).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        page
    };

    let mut library = unrestricted.clone();
    library["EnableAllFolders"] = false.into();
    library["EnabledFolders"] = serde_json::json!([open]);
    let mut rating = unrestricted.clone();
    rating["MaxParentalRating"] = 13.into();
    let mut tag = unrestricted.clone();
    tag["BlockedTags"] = serde_json::json!(["gore"]);
    for (case, policy, expected) in [
        (
            "unrestricted",
            &unrestricted,
            &["Ada", "Bob", "Rae", "Sam", "Tia"][..],
        ),
        ("library access", &library, &["Ada", "Rae", "Sam", "Tia"]),
        ("parental rating", &rating, &["Ada", "Bob", "Sam", "Tia"]),
        ("blocked tag", &tag, &["Ada", "Bob", "Rae", "Sam"]),
    ] {
        set_policy(policy).await;
        let page = persons("/Persons").await;
        assert_eq!(names(&page), expected, "{case}: {page}");
        assert_eq!(page["TotalRecordCount"], expected.len(), "{case}: {page}");
        // A page counts the whole filtered set, not just itself.
        let page = persons("/Persons?limit=2").await;
        assert_eq!(names(&page), expected[..2], "{case}: {page}");
        assert_eq!(page["TotalRecordCount"], expected.len(), "{case}: {page}");
    }

    // The by-name rows an item query exempts from library filtering are held
    // to the same rule (`ApplyItemByNameAccessFiltering`), including for a
    // user whose only restriction is library access.
    set_policy(&library).await;
    let page = persons("/Items?recursive=true&includeItemTypes=Person").await;
    assert_eq!(names(&page), ["Ada", "Rae", "Sam", "Tia"], "{page}");
    for uri in ["/Items?recursive=true&includeItemTypes=Genre", "/Genres"] {
        let page = persons(uri).await;
        assert_eq!(page["TotalRecordCount"], 0, "{uri}: {page}");
    }
    let (_, page) = send(
        &router,
        &admin,
        "GET",
        "/Items?recursive=true&includeItemTypes=Genre",
    )
    .await;
    assert_eq!(names(&page), ["Western"], "{page}");

    // `GetPeopleItems` then drops the people that fail `IsVisible` themselves.
    // Ada's credits are on an untagged film, but her own item now carries the
    // blocked tag. The total is the repository's, counted before that check,
    // as upstream's is.
    let (_, ada) = send(&router, &admin, "GET", "/Persons/Ada").await;
    let ada = ada["Id"].as_str().expect("Ada's id");
    let (status, body) = send_json(
        &router,
        &admin,
        "POST",
        &format!("/Items/{ada}"),
        Some(&serde_json::json!({ "Name": "Ada", "Tags": ["gore"] })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    set_policy(&tag).await;
    let page = persons("/Persons").await;
    assert_eq!(names(&page), ["Bob", "Rae", "Sam"], "{page}");
    assert_eq!(page["TotalRecordCount"], 4, "{page}");
}
