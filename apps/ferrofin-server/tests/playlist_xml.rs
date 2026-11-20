//! A playlist folder's `playlist.xml` over the real router, database,
//! playlist manager, provider manager and library scan:
//!
//! - `PlaylistXmlSaver` writes a playlist's file in the data directory on its
//!   creation and on every change the playlist manager makes (members,
//!   shares, name), with its owner, shares and media type;
//! - `PlaylistXmlProvider` reads it back in the playlist's refresh: its title,
//!   overview and shares by `PlaylistMetadataService.MergeData`'s rule, its
//!   members only for a playlist outside the data directory;
//! - a playlist folder in a media library is read by the scan, and written
//!   back only once its library saves into its media folders (the owner rule
//!   on media-library writes).
//! - the first boot of this version writes the file of every playlist in the
//!   playlists folder from the database, once.
use std::path::Path;
use std::time::{Duration, SystemTime};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_server::{config::Config, state::build_app_state};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;

async fn call(
    router: &axum::Router,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let auth = format!(
        r#"MediaBrowser Client="playlist-xml-test", Device="fixture", DeviceId="playlist-xml-test", Version="1", Token="{token}""#
    );
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, auth)
        .header(header::CONTENT_TYPE, "application/json");
    let request = match body {
        Some(body) => request.body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
}

/// The file once `ready` holds for it: the saves run in the request, the
/// queued refreshes after it.
async fn wait_for_file(path: &Path, ready: impl Fn(&str) -> bool) -> String {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(path)
            && ready(&text)
        {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "{} never got there: {:?}",
        path.display(),
        std::fs::read_to_string(path)
    );
}

/// Waits out the queued refreshes, the running one included.
async fn settle(providers: &dyn ferrofin_traits::providers::ProviderManager) {
    for _ in 0..600 {
        if !providers.is_processing_refresh_queue().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the refresh queue never went idle");
}

/// Scans every library and waits until none reports a scan in flight.
async fn scan(router: &axum::Router, token: &str) {
    let (status, body) = call(router, token, "POST", "/Library/Refresh", None).await;
    assert!(status.is_success(), "{status} {body}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    for _ in 0..600 {
        let (_, folders) = call(router, token, "GET", "/Library/VirtualFolders", None).await;
        if folders
            .as_array()
            .is_some_and(|folders| folders.iter().all(|f| f["RefreshStatus"] == "Idle"))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the libraries never went idle");
}

/// The `<Path>`s of the `<PlaylistItem>`s, in order.
fn member_paths(xml: &str) -> Vec<&str> {
    xml.split("<Path>")
        .skip(1)
        .filter_map(|rest| rest.split_once("</Path>").map(|(path, _)| path))
        .collect()
}

/// `path` written with `text`, its mtime a minute ahead: an edit the next
/// refresh sees as newer than the playlist's save.
fn edit(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(60))
        .unwrap();
}

/// A playlist's share rows, in share order.
async fn shares(db: &Database, playlist: Uuid) -> Vec<(Uuid, bool)> {
    let rows: Vec<(String, bool)> = sqlx::query_as(
        r#"SELECT "UserId", "CanEdit" FROM "FerrofinPlaylistShares"
           WHERE "PlaylistId" = ?1 ORDER BY rowid"#,
    )
    .bind(guid_to_db(playlist))
    .fetch_all(db.pool())
    .await
    .unwrap();
    rows.into_iter()
        .map(|(user, can_edit)| (Uuid::parse_str(&user).unwrap(), can_edit))
        .collect()
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one HTTP fixture walks the file through writes, reads and both kinds of folder"
)]
async fn a_playlists_xml_follows_its_changes_and_is_read_back() {
    let temp = tempfile::tempdir().unwrap();
    let config = Config {
        admin_user: "admin".to_owned(),
        admin_password: "playlist-xml-pw".to_owned(),
        ..Config::test_stub(temp.path())
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    let db = Database::connect(&config.database_url()).await.unwrap();
    db.run_migrations().await.unwrap();
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        encoder_app_path_display: None,
        ffmpeg: "ffmpeg".into(),
        ffprobe: "ffprobe".into(),
        capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
        chromaprint_muxer: false,
    };
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, &config, &ffmpeg, None, shutdown_tx)
        .await
        .unwrap();
    ferrofin_server::seed::seed_default_admin(wired.state.users.as_ref(), &config)
        .await
        .unwrap();
    let providers = wired.state.providers.clone();
    let router = ferrofin_api::create_router(wired.state.clone());
    let request = Request::builder()
        .method("POST")
        .uri("/Users/AuthenticateByName")
        .header(
            header::AUTHORIZATION,
            r#"MediaBrowser Client="playlist-xml-test", Device="fixture", DeviceId="playlist-xml-test", Version="1""#,
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"Username": "admin", "Pw": "playlist-xml-pw"}).to_string(),
        ))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let admin: Value = serde_json::from_slice(&bytes).unwrap();
    let token = admin["AccessToken"].as_str().unwrap().to_owned();
    let token = token.as_str();
    let user = Uuid::parse_str(admin["User"]["Id"].as_str().unwrap()).unwrap();
    let (status, friend) = call(
        &router,
        token,
        "POST",
        "/Users/New",
        Some(json!({"Name": "friend", "Password": "friend-pw"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{friend}");
    let friend = Uuid::parse_str(friend["Id"].as_str().unwrap()).unwrap();

    // A movies library that did not opt into saving into its folders, with
    // two movies and a playlist folder Jellyfin wrote.
    let media = temp.path().join("movies");
    let mix = media.join("Mix [playlist]");
    std::fs::create_dir_all(&mix).unwrap();
    for name in ["Alpha", "Beta"] {
        std::fs::write(media.join(format!("{name}.mkv")), b"").unwrap();
    }
    let (alpha_path, beta_path) = (
        media.join("Alpha.mkv").to_string_lossy().into_owned(),
        media.join("Beta.mkv").to_string_lossy().into_owned(),
    );
    let mix_xml = mix.join("playlist.xml");
    let jellyfin_mix = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n<Item>\n  \
         <Added>11/20/2026 10:00:00</Added>\n  <LockData>false</LockData>\n  \
         <Overview>Movie night</Overview>\n  <LocalTitle>The Mix</LocalTitle>\n  \
         <OwnerUserId>{}</OwnerUserId>\n  <PlaylistItems>\n    <PlaylistItem>\n      \
         <Path>{beta_path}</Path>\n    </PlaylistItem>\n  </PlaylistItems>\n  <Shares>\n    \
         <Share>\n      <UserId>{friend}</UserId>\n      <CanEdit>true</CanEdit>\n    </Share>\n  \
         </Shares>\n  <PlaylistMediaType>Video</PlaylistMediaType>\n</Item>\n",
        user.simple()
    );
    std::fs::write(&mix_xml, &jellyfin_mix).unwrap();
    let (status, body) = call(
        &router,
        token,
        "POST",
        "/Library/VirtualFolders?name=Movies&refreshLibrary=false",
        Some(
            json!({"LibraryOptions":{"PathInfos":[{"Path":media.to_str().unwrap()}],"EnableRealtimeMonitor":false,"EnableChapterImageExtraction":false,"EnableTrickplayImageExtraction":false,"TypeOptions":[]}}),
        ),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    scan(&router, token).await;
    let (_, found) = call(
        &router,
        token,
        "GET",
        &format!("/Items?recursive=true&includeItemTypes=Movie&sortBy=SortName&userId={user}"),
        None,
    )
    .await;
    let movies: Vec<String> = found["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(movies.len(), 2, "{found}");
    let (alpha, beta) = (movies[0].clone(), movies[1].clone());

    // ── A playlist folder in the media library: the scan reads its title,
    // overview, members and shares, and writes nothing back into the folder.
    let scanned: String = sqlx::query_scalar(
        r#"SELECT "Id" FROM "BaseItems" WHERE "Path" = ?1 AND "Type" LIKE '%.Playlist'"#,
    )
    .bind(mix.to_str().unwrap())
    .fetch_one(db.pool())
    .await
    .unwrap();
    let scanned = Uuid::parse_str(&scanned).unwrap();
    let (status, item) = call(
        &router,
        token,
        "GET",
        &format!("/Users/{user}/Items/{scanned}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{item}");
    assert_eq!(item["Name"], "The Mix");
    assert_eq!(item["Overview"], "Movie night");
    assert_eq!(shares(&db, scanned).await, [(friend, true)]);
    let (_, entries) = call(
        &router,
        token,
        "GET",
        &format!("/Playlists/{scanned}/Items?userId={user}"),
        None,
    )
    .await;
    assert_eq!(entries["Items"][0]["Id"], json!(beta), "{entries}");
    settle(providers.as_ref()).await;
    assert_eq!(
        std::fs::read_to_string(&mix_xml).unwrap(),
        jellyfin_mix,
        "a library that did not opt in keeps its folder as it was"
    );

    // ── A playlist in the data directory: written on its creation.
    let (status, created) = call(
        &router,
        token,
        "POST",
        "/Playlists",
        Some(json!({
            "Name": "Road Trip",
            "Ids": [alpha],
            "UserId": user,
            "MediaType": "Video",
            "Users": [{"UserId": friend, "CanEdit": false}],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let playlist = Uuid::parse_str(created["Id"].as_str().unwrap()).unwrap();
    let (_, item) = call(
        &router,
        token,
        "GET",
        &format!("/Users/{user}/Items/{playlist}"),
        None,
    )
    .await;
    let folder = item["Path"].as_str().unwrap().to_owned();
    assert!(folder.ends_with("/playlists/Road Trip"), "{folder}");
    let xml = Path::new(&folder).join("playlist.xml");
    let friend_share = |can_edit: bool| {
        format!(
            "<UserId>{}</UserId>\n      <CanEdit>{can_edit}</CanEdit>",
            friend.hyphenated()
        )
    };
    let text = wait_for_file(&xml, |text| member_paths(text) == [alpha_path.as_str()]).await;
    assert!(
        text.contains("<LocalTitle>Road Trip</LocalTitle>"),
        "{text}"
    );
    assert!(
        text.contains(&format!("<OwnerUserId>{}</OwnerUserId>", user.simple())),
        "{text}"
    );
    assert!(text.contains(&friend_share(false)), "{text}");
    assert!(
        text.contains("<PlaylistMediaType>Video</PlaylistMediaType>"),
        "{text}"
    );

    // Every playlist-manager change rewrites it: a share changed, removed,
    // a member added, a rename.
    let (status, body) = call(
        &router,
        token,
        "POST",
        &format!("/Playlists/{playlist}/Users/{friend}"),
        Some(json!({"CanEdit": true})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    wait_for_file(&xml, |text| text.contains(&friend_share(true))).await;
    let (status, body) = call(
        &router,
        token,
        "DELETE",
        &format!("/Playlists/{playlist}/Users/{friend}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    wait_for_file(&xml, |text| text.contains("<Shares />")).await;
    let (status, body) = call(
        &router,
        token,
        "POST",
        &format!("/Playlists/{playlist}/Items?ids={beta}&userId={user}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    wait_for_file(&xml, |text| {
        member_paths(text) == [alpha_path.as_str(), beta_path.as_str()]
    })
    .await;
    let (status, body) = call(
        &router,
        token,
        "POST",
        &format!("/Playlists/{playlist}"),
        Some(json!({"Name": "Road Trip 2"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    wait_for_file(&xml, |text| {
        text.contains("<LocalTitle>Road Trip 2</LocalTitle>")
    })
    .await;
    settle(providers.as_ref()).await;

    // ── Read back: a hand edit newer than the playlist's save names it,
    // describes it and shares it (the file wins on a Default refresh), but
    // in the data directory its member list is not taken.
    let edited = std::fs::read_to_string(&xml)
        .unwrap()
        .replace(
            "<LocalTitle>Road Trip 2</LocalTitle>",
            "<LocalTitle>Edited Trip</LocalTitle>\n  <Overview>By hand</Overview>",
        )
        .replace(
            "<Shares />",
            &format!(
                "<Shares>\n    <Share>\n      {}\n    </Share>\n  </Shares>",
                friend_share(false)
            ),
        );
    let edited = edited.replace(
        &format!("<Path>{beta_path}</Path>"),
        "<Path>/nowhere.mkv</Path>",
    );
    edit(&xml, &edited);
    providers
        .refresh_single_item(playlist, &MetadataRefreshOptions::default())
        .await
        .unwrap();
    let (_, item) = call(
        &router,
        token,
        "GET",
        &format!("/Users/{user}/Items/{playlist}"),
        None,
    )
    .await;
    assert_eq!(item["Name"], "Edited Trip");
    assert_eq!(item["Overview"], "By hand");
    let (_, users) = call(
        &router,
        token,
        "GET",
        &format!("/Playlists/{playlist}/Users"),
        None,
    )
    .await;
    assert_eq!(
        users,
        json!([{"UserId": friend.simple().to_string(), "CanEdit": false}])
    );
    let (_, entries) = call(
        &router,
        token,
        "GET",
        &format!("/Playlists/{playlist}/Items?userId={user}"),
        None,
    )
    .await;
    let ids: Vec<&str> = entries["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [alpha.as_str(), beta.as_str()],
        "the links stay the source of truth"
    );
    // The read is a `MetadataImport`, which `PlaylistXmlSaver` writes back.
    let text = wait_for_file(&xml, |text| {
        text.contains(&format!("<Path>{beta_path}</Path>"))
    })
    .await;
    assert!(text.contains("<Overview>By hand</Overview>"), "{text}");

    // Every share removed by hand: `<Shares />` on a Default refresh.
    edit(
        &xml,
        &std::fs::read_to_string(&xml).unwrap().replace(
            &format!(
                "<Shares>\n    <Share>\n      {}\n    </Share>\n  </Shares>",
                friend_share(false)
            ),
            "<Shares />",
        ),
    );
    providers
        .refresh_single_item(playlist, &MetadataRefreshOptions::default())
        .await
        .unwrap();
    assert_eq!(shares(&db, playlist).await, []);

    // "Replace all metadata" where the saver writes the file (the data
    // directory) does not read it: the database wins, and the file is
    // rewritten from it.
    let shared = std::fs::read_to_string(&xml).unwrap().replace(
        "<Shares />",
        &format!(
            "<Shares>\n    <Share>\n      {}\n    </Share>\n  </Shares>",
            friend_share(true)
        ),
    );
    edit(&xml, &shared);
    providers
        .refresh_single_item(
            playlist,
            &MetadataRefreshOptions {
                metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
                replace_all_metadata: true,
                ..MetadataRefreshOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(shares(&db, playlist).await, []);
    wait_for_file(&xml, |text| text.contains("<Shares />")).await;

    // A full refresh that keeps metadata merges: the file's shares first,
    // then the stored ones it does not name, so a share removed only in the
    // file stays.
    let (status, body) = call(
        &router,
        token,
        "POST",
        &format!("/Playlists/{playlist}/Users/{friend}"),
        Some(json!({"CanEdit": true})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let unshared = wait_for_file(&xml, |text| text.contains(&friend_share(true))).await;
    let other = Uuid::new_v4();
    edit(
        &xml,
        &unshared.replace(
            &friend_share(true),
            &format!(
                "<UserId>{}</UserId>\n      <CanEdit>false</CanEdit>",
                other.hyphenated()
            ),
        ),
    );
    let full = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        ..MetadataRefreshOptions::default()
    };
    providers
        .refresh_single_item(playlist, &full)
        .await
        .unwrap();
    assert_eq!(
        shares(&db, playlist).await,
        [(other, false), (friend, true)]
    );

    // A locked playlist reads nothing (`RefreshWithProviders` returns first).
    let (status, body) = call(
        &router,
        token,
        "POST",
        &format!("/Items/{playlist}"),
        Some(json!({"Name": "Edited Trip", "LockData": true})),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    let locked = wait_for_file(&xml, |text| text.contains("<LockData>true</LockData>")).await;
    edit(
        &xml,
        &locked.replace(
            "<LocalTitle>Edited Trip</LocalTitle>",
            "<LocalTitle>Ignored</LocalTitle>",
        ),
    );
    providers
        .refresh_single_item(playlist, &full)
        .await
        .unwrap();
    let (_, item) = call(
        &router,
        token,
        "GET",
        &format!("/Users/{user}/Items/{playlist}"),
        None,
    )
    .await;
    assert_eq!(item["Name"], "Edited Trip");

    // ── A library that saves into its folders from the start: the scan that
    // first finds a playlist folder reads its file before anything writes it,
    // then writes it back with what it read.
    let opted = temp.path().join("opted");
    std::fs::create_dir_all(opted.join("Opted [playlist]")).unwrap();
    let opted_xml = opted.join("Opted [playlist]/playlist.xml");
    std::fs::write(
        &opted_xml,
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<Item><LocalTitle>Opted Mix</LocalTitle>\
             <PlaylistItems><PlaylistItem><Path>{alpha_path}</Path></PlaylistItem></PlaylistItems>\
             <Shares><Share><UserId>{friend}</UserId><CanEdit>true</CanEdit></Share></Shares></Item>"
        ),
    )
    .unwrap();
    let (status, body) = call(
        &router,
        token,
        "POST",
        "/Library/VirtualFolders?name=Opted&collectionType=movies&refreshLibrary=false",
        Some(
            json!({"LibraryOptions":{"PathInfos":[{"Path":opted.to_str().unwrap()}],"SaveLocalMetadata":true,"EnableRealtimeMonitor":false,"EnableChapterImageExtraction":false,"EnableTrickplayImageExtraction":false,"TypeOptions":[]}}),
        ),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    scan(&router, token).await;
    let text = wait_for_file(&opted_xml, |text| text.contains("<OwnerUserId>")).await;
    assert!(
        text.contains("<LocalTitle>Opted Mix</LocalTitle>"),
        "{text}"
    );
    assert!(text.contains(&friend_share(true)), "{text}");
    assert_eq!(member_paths(&text), [alpha_path.as_str()]);

    // ── The media library opts in: a refresh that reads the folder's file
    // writes it back in Ferrofin's form, the owner nobody.
    let (_, folders) = call(&router, token, "GET", "/Library/VirtualFolders", None).await;
    let library = folders
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["Name"] == "Movies")
        .unwrap()
        .clone();
    let mut options = library["LibraryOptions"].clone();
    options["SaveLocalMetadata"] = json!(true);
    let (status, body) = call(
        &router,
        token,
        "POST",
        "/Library/VirtualFolders/LibraryOptions",
        Some(json!({"Id": library["ItemId"], "LibraryOptions": options})),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    edit(
        &mix_xml,
        &jellyfin_mix.replace("Movie night", "Movie night, again"),
    );
    scan(&router, token).await;
    let text = wait_for_file(&mix_xml, |text| {
        text.contains("<OwnerUserId>00000000000000000000000000000000</OwnerUserId>")
    })
    .await;
    assert!(
        text.contains("<Overview>Movie night, again</Overview>"),
        "{text}"
    );
    assert!(text.contains(&friend_share(true)), "{text}");
    assert_eq!(member_paths(&text), [beta_path.as_str()]);
}

/// Boots the app state on `db`, as a start of the server does.
async fn boot(db: &Database, config: &Config) {
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        encoder_app_path_display: None,
        ffmpeg: "ffmpeg".into(),
        ffprobe: "ffprobe".into(),
        capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
        chromaprint_muxer: false,
    };
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(db, config, &ffmpeg, None, shutdown_tx)
        .await
        .unwrap();
    drop(wired);
}

/// The first boot of this version writes the `playlist.xml` of each playlist
/// in the playlists folder from the database (`save_playlist_xml_once`): a
/// file Jellyfin last wrote before the database was adopted, with a share
/// removed since, would otherwise give that share back at the next full
/// refresh. A playlist folder in a media library is left alone; a file that
/// cannot be written makes the next boot run the repair again; once it has
/// run whole, it never runs again.
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one fixture boots three times: a failed write, the retry, the marker"
)]
async fn the_first_boot_writes_the_playlists_folders_files_from_the_database() {
    use ferrofin_db::entities::base_items::BaseItemEntity;
    use ferrofin_traits::persistence::ItemPersistenceService as _;
    let temp = tempfile::tempdir().unwrap();
    let config = Config::test_stub(temp.path());
    std::fs::create_dir_all(&config.data_dir).unwrap();
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    let db = Database::connect(&config.database_url()).await.unwrap();
    db.run_migrations().await.unwrap();
    let playlists = config.data_dir.join("data/playlists");
    let (playlist, broken, library, owner, kept, removed) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let stale = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<Item><LocalTitle>Old Mix</LocalTitle>\
         <Shares><Share><UserId>{removed}</UserId><CanEdit>true</CanEdit></Share></Shares></Item>"
    );
    // In the playlists folder, stored in the `%AppDataPath%` form an adopted
    // database holds.
    std::fs::create_dir_all(playlists.join("Old Mix")).unwrap();
    let xml = playlists.join("Old Mix/playlist.xml");
    std::fs::write(&xml, &stale).unwrap();
    // A folder path that is a regular file: its playlist.xml cannot be made.
    std::fs::write(playlists.join("Broken"), b"").unwrap();
    // A playlist folder in a media library: never written by the repair.
    let media = temp.path().join("music/Library Mix [playlist]");
    std::fs::create_dir_all(&media).unwrap();
    let media_xml = media.join("playlist.xml");
    std::fs::write(&media_xml, &stale).unwrap();
    let row = |id: Uuid, path: String| BaseItemEntity {
        id: guid_to_db(id),
        type_: "MediaBrowser.Controller.Playlists.Playlist".into(),
        name: Some("Old Mix".into()),
        is_folder: true,
        media_type: Some("Audio".into()),
        path: Some(path),
        data: Some(
            json!({
                "OwnerUserId": owner.simple().to_string(),
                "OpenAccess": false,
                "Shares": [{"UserId": kept.simple().to_string(), "CanEdit": false}],
                "PlaylistMediaType": "Audio",
            })
            .to_string(),
        ),
        ..Default::default()
    };
    ferrofin_core::FerrofinItemPersistenceService::new(db.clone())
        .save_items(&[
            row(playlist, "%AppDataPath%/playlists/Old Mix".to_owned()),
            row(broken, "%AppDataPath%/playlists/Broken".to_owned()),
            row(library, media.to_string_lossy().into_owned()),
        ])
        .await
        .unwrap();
    let marker = || async {
        sqlx::query_scalar::<_, String>(
            r#"SELECT "Value" FROM "FerrofinMeta" WHERE "Key" = 'playlist_xml_saved_v1'"#,
        )
        .fetch_optional(db.pool())
        .await
        .unwrap()
    };

    // First boot: the playlist's file follows the database; the broken one
    // leaves the repair owed.
    boot(&db, &config).await;
    let text = std::fs::read_to_string(&xml).unwrap();
    assert!(
        text.contains(&format!("<OwnerUserId>{}</OwnerUserId>", owner.simple())),
        "{text}"
    );
    assert!(
        text.contains(&format!("<UserId>{}</UserId>", kept.hyphenated())),
        "{text}"
    );
    assert!(!text.contains(&removed.hyphenated().to_string()), "{text}");
    assert!(
        text.contains("<PlaylistMediaType>Audio</PlaylistMediaType>"),
        "{text}"
    );
    // Stamped after the write, so the file reads as no change.
    let saved: String =
        sqlx::query_scalar(r#"SELECT "DateLastSaved" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(playlist))
            .fetch_one(db.pool())
            .await
            .unwrap();
    let saved = chrono::NaiveDateTime::parse_from_str(&saved, "%Y-%m-%d %H:%M:%S%.f")
        .unwrap()
        .and_utc();
    let written =
        chrono::DateTime::<chrono::Utc>::from(std::fs::metadata(&xml).unwrap().modified().unwrap());
    assert!(saved >= written, "{saved} < {written}");
    assert_eq!(std::fs::read_to_string(&media_xml).unwrap(), stale);
    assert_eq!(
        marker().await,
        None,
        "a failed write leaves the repair owed"
    );

    // Second boot, the obstacle gone: the repair runs again, whole.
    std::fs::remove_file(playlists.join("Broken")).unwrap();
    std::fs::create_dir(playlists.join("Broken")).unwrap();
    std::fs::remove_file(&xml).unwrap();
    boot(&db, &config).await;
    assert!(xml.is_file() && playlists.join("Broken/playlist.xml").is_file());
    assert_eq!(std::fs::read_to_string(&media_xml).unwrap(), stale);
    assert_eq!(marker().await.as_deref(), Some("1"));

    // Third boot: done, never again.
    std::fs::remove_file(&xml).unwrap();
    boot(&db, &config).await;
    assert!(!xml.exists(), "the repair runs once");
}
