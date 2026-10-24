//! What a playlist stores for the ids it is given — `PlaylistManager.
//! AddToPlaylistInternal` through `Playlist.GetPlaylistItems` — over the real
//! router, database and playlist manager: containers expand into their
//! tracks or episodes, unknown ids and kinds that cannot be added are dropped,
//! a create infers and persists the playlist's media type, everything a
//! create writes commits together, and a create or lookup needs a user.
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use ferrofin_db::{Database, entities::base_items::BaseItemEntity, store::guid_to_db};
use ferrofin_server::{config::Config, state::build_app_state};
use ferrofin_traits::persistence::ItemPersistenceService as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;

const PLAYLIST_TYPE: &str = "MediaBrowser.Controller.Playlists.Playlist";

async fn call(
    router: &axum::Router,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let auth = format!(
        r#"MediaBrowser Client="playlist-items-test", Device="fixture", DeviceId="playlist-items-test", Version="1", Token="{token}""#
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
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

/// Logs `user` in and returns the access token.
async fn login(router: &axum::Router, user: &str, password: &str) -> String {
    let request = Request::builder()
        .method("POST")
        .uri("/Users/AuthenticateByName")
        .header(
            header::AUTHORIZATION,
            r#"MediaBrowser Client="playlist-items-test", Device="fixture", DeviceId="playlist-items-test", Version="1""#,
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"Username": user, "Pw": password}).to_string(),
        ))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let auth: Value = serde_json::from_slice(&bytes).unwrap();
    auth["AccessToken"].as_str().unwrap().to_owned()
}

/// The server, logged in as its admin, with one library.
#[derive(Clone)]
struct Fixture {
    temp: std::sync::Arc<tempfile::TempDir>,
    db: Database,
    router: axum::Router,
    state: ferrofin_api::state::AppState,
    token: String,
    library: Uuid,
}

/// The server's configuration over `root`.
fn config(root: &std::path::Path) -> Config {
    Config {
        admin_user: "admin".to_owned(),
        admin_password: "playlist-pw".to_owned(),
        ..Config::test_stub(root)
    }
}

/// An ffmpeg that is never run: nothing here transcodes.
fn ffmpeg() -> ferrofin_server::bootstrap::FfmpegPaths {
    ferrofin_server::bootstrap::FfmpegPaths {
        encoder_app_path_display: None,
        ffmpeg: "ffmpeg".into(),
        ffprobe: "ffprobe".into(),
        capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
        chromaprint_muxer: false,
    }
}

async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let config = config(temp.path());
    std::fs::create_dir_all(&config.data_dir).unwrap();
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    let db = Database::connect(&config.database_url()).await.unwrap();
    db.run_migrations().await.unwrap();
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, &config, &ffmpeg(), None, shutdown_tx)
        .await
        .unwrap();
    ferrofin_server::seed::seed_default_admin(wired.state.users.as_ref(), &config)
        .await
        .unwrap();
    let router = ferrofin_api::create_router(wired.state.clone());
    let token = login(&router, "admin", "playlist-pw").await;

    let media = temp.path().join("media");
    std::fs::create_dir_all(&media).unwrap();
    let (status, body) = call(
        &router,
        &token,
        "POST",
        "/Library/VirtualFolders?name=Media&refreshLibrary=false",
        Some(
            json!({"LibraryOptions":{"PathInfos":[{"Path":media.to_str().unwrap()}],"EnableRealtimeMonitor":false,"EnableChapterImageExtraction":false,"EnableTrickplayImageExtraction":false,"TypeOptions":[]}}),
        ),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    let (_, folders) = call(&router, &token, "GET", "/Library/VirtualFolders", None).await;
    let library = folders
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["Name"] == "Media")
        .unwrap()["ItemId"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    Fixture {
        temp: std::sync::Arc::new(temp),
        db,
        router,
        state: wired.state,
        token,
        library,
    }
}

impl Fixture {
    /// The server started again over the same database and directories —
    /// everything a restart runs before it serves — and its router.
    async fn restart(&self) -> axum::Router {
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        let wired = build_app_state(
            &self.db,
            &config(self.temp.path()),
            &ffmpeg(),
            None,
            shutdown_tx,
        )
        .await
        .unwrap();
        ferrofin_api::create_router(wired.state)
    }

    /// Saves `row` under `ancestors` (nearest first) inside the library.
    async fn seed(&self, mut row: BaseItemEntity, ancestors: &[Uuid]) -> Uuid {
        let id = Uuid::new_v4();
        row.id = guid_to_db(id);
        row.top_parent_id = Some(guid_to_db(self.library));
        if row.parent_id.is_none() {
            row.parent_id = Some(guid_to_db(self.library));
        }
        self.state.library.create_items(&[row], None).await.unwrap();
        let mut chain = ancestors.to_vec();
        chain.push(self.library);
        ferrofin_core::FerrofinItemPersistenceService::new(self.db.clone())
            .set_ancestors(id, &chain)
            .await
            .unwrap();
        id
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        call(&self.router, &self.token, method, path, body).await
    }

    async fn login(&self, user: &str, password: &str) -> String {
        login(&self.router, user, password).await
    }

    /// The names of the entries stored for `playlist`, in playlist order,
    /// whoever may read them.
    async fn stored(&self, playlist: &str) -> Vec<String> {
        sqlx::query_scalar(
            r#"SELECT bi."Name" FROM "LinkedChildren" lc
               JOIN "BaseItems" bi ON bi."Id" = lc."ChildId"
               WHERE lc."ParentId" = ?1 ORDER BY lc."SortOrder""#,
        )
        .bind(guid_to_db(playlist.parse().unwrap()))
        .fetch_all(self.db.pool())
        .await
        .unwrap()
    }

    /// The playlist's stored `DateLastMediaAdded`.
    async fn media_added(&self, playlist: &str) -> Option<String> {
        sqlx::query_scalar(r#"SELECT "DateLastMediaAdded" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(playlist.parse().unwrap()))
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }

    /// The names `GET /Playlists/{id}/Items` lists, in playlist order.
    async fn entries(&self, playlist: &str) -> Vec<String> {
        let (status, body) = self
            .call("GET", &format!("/Playlists/{playlist}/Items"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["Items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["Name"].as_str().unwrap().to_owned())
            .collect()
    }

    async fn playlist_count(&self) -> i64 {
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Type" = ?1"#)
            .bind(PLAYLIST_TYPE)
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }

    async fn count(&self, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql)
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }
}

fn row(type_: &str, name: &str, sort_name: &str) -> BaseItemEntity {
    BaseItemEntity {
        type_: type_.to_owned(),
        name: Some(name.to_owned()),
        sort_name: Some(sort_name.to_owned()),
        ..BaseItemEntity::default()
    }
}

/// "Rock" tracks by "Band": the album "Record" (Zulu, then Alpha), the album
/// "Zed" (Beta) and, under the album artist "Various", the R-rated Omega on
/// "Compilation". Track order disagrees with name order, album order with
/// track order, and album-artist order with album order.
struct Music {
    artist: Uuid,
    album: Uuid,
    genre: Uuid,
}

async fn seed_music(fx: &Fixture) -> Music {
    let mut artist = row(
        "MediaBrowser.Controller.Entities.Audio.MusicArtist",
        "Band",
        "band",
    );
    artist.is_folder = true;
    let artist = fx.seed(artist, &[]).await;
    let mut albums = std::collections::HashMap::new();
    for (name, album_artist) in [
        ("Record", "Band"),
        ("Zed", "Band"),
        ("Compilation", "Various"),
    ] {
        let mut album = row(
            "MediaBrowser.Controller.Entities.Audio.MusicAlbum",
            name,
            &name.to_lowercase(),
        );
        album.is_folder = true;
        album.album_artists = Some(album_artist.to_owned());
        albums.insert(name, fx.seed(album, &[]).await);
    }
    for (album, album_artist, index, name) in [
        ("Record", "Band", 1, "Zulu"),
        ("Record", "Band", 2, "Alpha"),
        ("Zed", "Band", 1, "Beta"),
        ("Compilation", "Various", 9, "Omega"),
    ] {
        let mut track = row(
            "MediaBrowser.Controller.Entities.Audio.Audio",
            name,
            &format!("{index:04} - {}", name.to_lowercase()),
        );
        track.media_type = Some("Audio".to_owned());
        track.parent_id = Some(guid_to_db(albums[album]));
        track.album = Some(album.to_owned());
        track.index_number = Some(index);
        track.artists = Some("Band".to_owned());
        track.album_artists = Some(album_artist.to_owned());
        if name == "Omega" {
            track.official_rating = Some("R".to_owned());
            track.inherited_parental_rating_value = Some(17);
        }
        let track = fx.seed(track, &[albums[album]]).await;
        ferrofin_core::FerrofinItemPersistenceService::new(fx.db.clone())
            .save_item_values(
                track,
                &[
                    (0, "Band".to_owned()),
                    (1, album_artist.to_owned()),
                    (2, "Rock".to_owned()),
                ],
            )
            .await
            .unwrap();
    }
    // A music item's genre is materialised as a `MusicGenre` row.
    let genre: String = sqlx::query_scalar(
        r#"SELECT "Id" FROM "BaseItems" WHERE "Type" LIKE '%.MusicGenre' AND "Name" = 'Rock'"#,
    )
    .fetch_one(fx.db.pool())
    .await
    .unwrap();
    Music {
        artist,
        album: albums["Record"],
        genre: genre.parse().unwrap(),
    }
}

/// A series with one season of two episodes, whose order is not name order.
struct Show {
    series: Uuid,
    season: Uuid,
}

async fn seed_show(fx: &Fixture) -> Show {
    let mut series = row("MediaBrowser.Controller.Entities.TV.Series", "Show", "show");
    series.is_folder = true;
    series.presentation_unique_key = Some("show-key".to_owned());
    let series = fx.seed(series, &[]).await;
    let mut season = row(
        "MediaBrowser.Controller.Entities.TV.Season",
        "Season 1",
        "0001",
    );
    season.is_folder = true;
    season.parent_id = Some(guid_to_db(series));
    season.series_id = Some(guid_to_db(series));
    season.index_number = Some(1);
    season.series_presentation_unique_key = Some("show-key".to_owned());
    season.presentation_unique_key = Some("show-key-001".to_owned());
    let season = fx.seed(season, &[series]).await;
    for (index, name) in [(1, "Pilot"), (2, "Arrival")] {
        let mut episode = row(
            "MediaBrowser.Controller.Entities.TV.Episode",
            name,
            &format!("0001-{index:04} - {}", name.to_lowercase()),
        );
        episode.media_type = Some("Video".to_owned());
        episode.parent_id = Some(guid_to_db(season));
        episode.season_id = Some(guid_to_db(season));
        episode.series_id = Some(guid_to_db(series));
        episode.series_presentation_unique_key = Some("show-key".to_owned());
        episode.index_number = Some(index);
        episode.parent_index_number = Some(1);
        fx.seed(episode, &[season, series]).await;
    }
    Show { series, season }
}

/// `POST /Playlists` with an album: the album's tracks, in track order, and
/// an `Audio` playlist stamped in the `MediaType` column and in `Data`.
#[tokio::test]
async fn creating_a_playlist_from_an_album_stores_its_tracks() {
    let fx = fixture().await;
    let music = seed_music(&fx).await;

    let (status, created) = fx
        .call(
            "POST",
            "/Playlists",
            Some(json!({"Name": "From album", "Ids": [music.album]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["Id"].as_str().unwrap().to_owned();
    assert_eq!(fx.entries(&id).await, ["Zulu", "Alpha"]);

    let (media_type, data, added): (Option<String>, Option<String>, Option<String>) =
        sqlx::query_as(
            r#"SELECT "MediaType", "Data", "DateLastMediaAdded" FROM "BaseItems" WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(Uuid::parse_str(&id).unwrap()))
        .fetch_one(fx.db.pool())
        .await
        .unwrap();
    assert_eq!(media_type.as_deref(), Some("Audio"));
    let data: Value = serde_json::from_str(data.as_deref().unwrap()).unwrap();
    assert_eq!(data["PlaylistMediaType"], "Audio", "{data}");
    assert!(added.is_some(), "an add stamps DateLastMediaAdded");
}

/// gl-910: a data directory moved under an existing database leaves the
/// playlists folder's row at the old path, under the same id. The next start
/// saves it at the new path, as `CreateRootFolder` does, and a create goes
/// into it — where it used to insert the folder's id a second time and
/// answer 500.
#[tokio::test]
async fn a_create_after_the_data_directory_moved_goes_into_the_repathed_folder() {
    let fx = fixture().await;
    let (folder, current): (String, String) = sqlx::query_as(
        r#"SELECT "Id", "Path" FROM "BaseItems"
           WHERE "Type" = 'Emby.Server.Implementations.Playlists.PlaylistsFolder'"#,
    )
    .fetch_one(fx.db.pool())
    .await
    .unwrap();
    sqlx::query(r#"UPDATE "BaseItems" SET "Path" = '/moved-from/data/playlists' WHERE "Id" = ?1"#)
        .bind(&folder)
        .execute(fx.db.writer())
        .await
        .unwrap();

    let router = fx.restart().await;
    let (status, created) = call(
        &router,
        &fx.token,
        "POST",
        "/Playlists",
        Some(json!({"Name": "After the move"})),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{created}");
    let path: Option<String> =
        sqlx::query_scalar(r#"SELECT "Path" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(&folder)
            .fetch_one(fx.db.pool())
            .await
            .unwrap();
    assert_eq!(path, Some(current), "the start re-paths the folder");
    let parent: Option<String> =
        sqlx::query_scalar(r#"SELECT "ParentId" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(created["Id"].as_str().unwrap().parse().unwrap()))
            .fetch_one(fx.db.pool())
            .await
            .unwrap();
    assert_eq!(parent, Some(folder), "the playlist is in that folder");
}

/// An id naming no item: upstream's `CreatePlaylist` throws before anything
/// exists (400, no playlist left behind), and an add drops it.
#[tokio::test]
async fn an_unknown_id_fails_a_create_and_is_dropped_by_an_add() {
    let fx = fixture().await;
    let music = seed_music(&fx).await;
    let unknown = Uuid::new_v4();

    let (status, body) = fx
        .call(
            "POST",
            "/Playlists",
            Some(json!({"Name": "Broken", "Ids": [unknown]})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(fx.playlist_count().await, 0, "no orphan playlist is left");

    let (status, created) = fx
        .call("POST", "/Playlists", Some(json!({"Name": "Empty"})))
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["Id"].as_str().unwrap().to_owned();
    let (status, body) = fx
        .call(
            "POST",
            &format!("/Playlists/{id}/Items?ids={unknown},{}", music.album),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(fx.entries(&id).await, ["Zulu", "Alpha"]);
}

/// `POST /Playlists/{id}/Items` with an artist, a series, a season and a
/// photo: the artist's tracks, the series' and the season's episodes, each
/// in upstream's order; the photo cannot be added.
#[tokio::test]
async fn adding_containers_expands_them_and_drops_what_cannot_be_added() {
    let fx = fixture().await;
    let music = seed_music(&fx).await;
    let show = seed_show(&fx).await;
    let mut photo = row("MediaBrowser.Controller.Entities.Photo", "Snap", "snap");
    photo.media_type = Some("Photo".to_owned());
    let photo = fx.seed(photo, &[]).await;

    let (status, created) = fx
        .call(
            "POST",
            "/Playlists",
            Some(json!({"Name": "Mixed", "MediaType": "Video"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["Id"].as_str().unwrap().to_owned();
    let (status, body) = fx
        .call(
            "POST",
            &format!(
                "/Playlists/{id}/Items?ids={},{},{photo},{}",
                music.artist, show.series, show.season
            ),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        fx.entries(&id).await,
        [
            "Zulu", "Alpha", "Beta", "Omega", "Pilot", "Arrival", "Pilot", "Arrival"
        ]
    );
    let added = fx.media_added(&id).await;
    assert!(added.is_some(), "an add stamps DateLastMediaAdded");

    // Nothing that can be added: nothing is written.
    let (status, body) = fx
        .call("POST", &format!("/Playlists/{id}/Items?ids={photo}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(fx.entries(&id).await.len(), 8);
    assert_eq!(fx.media_added(&id).await, added);

    // An update's `Ids` replace the entries through the same expansion, and
    // stamp the add.
    sqlx::query(r#"UPDATE "BaseItems" SET "DateLastMediaAdded" = NULL WHERE "Id" = ?1"#)
        .bind(guid_to_db(id.parse().unwrap()))
        .execute(fx.db.writer())
        .await
        .unwrap();
    let (status, body) = fx
        .call(
            "POST",
            &format!("/Playlists/{id}"),
            Some(json!({"Ids": [music.genre, photo]})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(fx.entries(&id).await, ["Zulu", "Alpha", "Beta", "Omega"]);
    let replaced = fx.media_added(&id).await;
    assert!(
        replaced.is_some(),
        "an update that adds stamps DateLastMediaAdded"
    );

    // `Ids` that all drop still replace (`LinkedChildren = []`), and add
    // nothing to stamp. The playlist's own id names nothing once that clear
    // has run.
    let (status, body) = fx
        .call(
            "POST",
            &format!("/Playlists/{id}"),
            Some(json!({"Ids": [photo, id]})),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(fx.entries(&id).await.is_empty());
    assert_eq!(fx.media_added(&id).await, replaced);
}

/// What a user may not see is never added for them: an over-limit item they
/// name is dropped (`IsVisible(user)`), so is another user's private
/// playlist, and so is an over-limit track of an artist they name (the
/// expansion query is theirs).
#[tokio::test]
async fn an_add_keeps_to_what_its_user_may_see() {
    let fx = fixture().await;
    let music = seed_music(&fx).await;
    let mut movie = row(
        "MediaBrowser.Controller.Entities.Movies.Movie",
        "Heist",
        "heist",
    );
    movie.media_type = Some("Video".to_owned());
    movie.official_rating = Some("R".to_owned());
    movie.inherited_parental_rating_value = Some(17);
    let movie = fx.seed(movie, &[]).await;
    let (status, private) = fx
        .call(
            "POST",
            "/Playlists",
            // A body without `IsPublic` creates an open playlist (C#'s
            // `IsPublic = true`), so a private one names it.
            Some(json!({"Name": "Admin's", "Ids": [music.album], "IsPublic": false})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{private}");
    let private = private["Id"].as_str().unwrap().to_owned();

    let (status, kid) = fx
        .call(
            "POST",
            "/Users/New",
            Some(json!({"Name": "kid", "Password": "kid-pw"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{kid}");
    let mut policy = kid["Policy"].clone();
    policy["MaxParentalRating"] = json!(13);
    let (status, body) = fx
        .call(
            "POST",
            &format!("/Users/{}/Policy", kid["Id"].as_str().unwrap()),
            Some(policy),
        )
        .await;
    assert!(status.is_success(), "{status} {body}");
    let kid = Fixture {
        token: fx.login("kid", "kid-pw").await,
        ..fx.clone()
    };
    let (status, created) = kid
        .call("POST", "/Playlists", Some(json!({"Name": "Kid's"})))
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["Id"].as_str().unwrap().to_owned();
    let (status, body) = kid
        .call(
            "POST",
            &format!(
                "/Playlists/{id}/Items?ids={movie},{private},{}",
                music.artist
            ),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    // What was stored, not what the kid's read lets through.
    assert_eq!(fx.stored(&id).await, ["Zulu", "Alpha", "Beta"]);

    // The same ids from the unlimited owner of both: everything.
    let (status, body) = fx
        .call(
            "POST",
            &format!("/Playlists/{private}/Items?ids={movie},{}", music.artist),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        fx.stored(&private).await,
        ["Zulu", "Alpha", "Heist", "Zulu", "Alpha", "Beta", "Omega"]
    );
}

/// A create naming no media type takes it from the first item that answers:
/// a series answers with its first episode's.
#[tokio::test]
async fn a_created_playlist_takes_its_media_type_from_its_items() {
    let fx = fixture().await;
    let show = seed_show(&fx).await;
    let (status, created) = fx
        .call(
            "POST",
            "/Playlists",
            Some(json!({"Name": "From series", "Ids": [show.series]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["Id"].as_str().unwrap().to_owned();
    assert_eq!(fx.entries(&id).await, ["Pilot", "Arrival"]);
    let (status, item) = fx.call("GET", &format!("/Items/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{item}");
    assert_eq!(item["MediaType"], "Video", "{item}");
}

/// The obsolete query form names the media type too, in any case, and wins
/// over the body and over inference (`mediaType ?? body.MediaType`), as its
/// name wins over the body's. The body still carries the `Name` upstream's
/// `required` member demands.
#[tokio::test]
async fn the_query_form_names_the_media_type() {
    let fx = fixture().await;
    let music = seed_music(&fx).await;
    let (status, created) = fx
        .call(
            "POST",
            &format!("/Playlists?name=Typed&mediaType=video&ids={}", music.album),
            Some(json!({"Name": "From body", "MediaType": "Audio"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["Id"].as_str().unwrap().to_owned();
    let (status, item) = fx.call("GET", &format!("/Items/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{item}");
    assert_eq!(item["MediaType"], "Video", "{item}");
    assert_eq!(item["Name"], "Typed", "{item}");
}

/// A create keeps its media type and its shares side by side in `Data`,
/// which Jellyfin reads both back from.
#[tokio::test]
async fn a_create_keeps_its_media_type_and_shares_in_data() {
    let fx = fixture().await;
    let sharee = Uuid::new_v4();
    let (status, created) = fx
        .call(
            "POST",
            "/Playlists",
            Some(json!({
                "Name": "Shared",
                "MediaType": "Video",
                "Users": [{"UserId": sharee, "CanEdit": true}],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id: Uuid = created["Id"].as_str().unwrap().parse().unwrap();
    let data: String = sqlx::query_scalar(r#"SELECT "Data" FROM "BaseItems" WHERE "Id" = ?1"#)
        .bind(ferrofin_db::store::guid_to_db(id))
        .fetch_one(fx.db.pool())
        .await
        .unwrap();
    let data: Value = serde_json::from_str(&data).unwrap();
    assert_eq!(data["PlaylistMediaType"], "Video", "{data}");
    let share = &data["Shares"][0];
    assert_eq!(
        share["UserId"].as_str().unwrap().parse::<Uuid>().unwrap(),
        sharee,
        "{data}"
    );
    assert_eq!(share["CanEdit"], true, "{data}");
}

/// A create that fails part-way leaves nothing behind — no playlist row
/// without its owner row, which every caller would open as its owner.
#[tokio::test]
async fn a_create_that_fails_part_way_leaves_no_playlist() {
    let fx = fixture().await;
    sqlx::query(
        r#"CREATE TRIGGER "fail_playlist_owner" BEFORE INSERT ON "FerrofinPlaylists"
           BEGIN SELECT RAISE(ABORT, 'injected failure'); END"#,
    )
    .execute(fx.db.writer())
    .await
    .unwrap();

    let (status, body) = fx
        .call("POST", "/Playlists", Some(json!({"Name": "Doomed"})))
        .await;
    assert!(status.is_server_error(), "{status} {body}");
    assert_eq!(
        fx.playlist_count().await,
        0,
        "the BaseItems row rolled back"
    );
}

/// The same when the last write, the `PlaylistMediaType` key, fails: the
/// owner row, the shares and the entries roll back with the playlist row.
#[tokio::test]
async fn a_create_whose_last_write_fails_leaves_nothing() {
    let fx = fixture().await;
    let music = seed_music(&fx).await;
    sqlx::query(
        r#"CREATE TRIGGER "fail_playlist_data" BEFORE UPDATE OF "Data" ON "BaseItems"
           WHEN old."Type" = 'MediaBrowser.Controller.Playlists.Playlist'
             AND new."Data" LIKE '%PlaylistMediaType%'
           BEGIN SELECT RAISE(ABORT, 'injected failure'); END"#,
    )
    .execute(fx.db.writer())
    .await
    .unwrap();

    let (status, body) = fx
        .call(
            "POST",
            "/Playlists",
            Some(json!({
                "Name": "Doomed",
                "Ids": [music.album],
                "Users": [{"UserId": Uuid::new_v4(), "CanEdit": false}],
            })),
        )
        .await;
    assert!(status.is_server_error(), "{status} {body}");
    assert_eq!(fx.playlist_count().await, 0);
    assert_eq!(
        fx.count(r#"SELECT COUNT(*) FROM "FerrofinPlaylists""#)
            .await,
        0
    );
    assert_eq!(
        fx.count(r#"SELECT COUNT(*) FROM "FerrofinPlaylistShares""#)
            .await,
        0
    );
    assert_eq!(
        fx.count(r#"SELECT COUNT(*) FROM "LinkedChildren""#).await,
        0
    );
}

/// An API key that names no user: upstream's `CreatePlaylist` and
/// `GetPlaylistForUser` call `GetUserById(Guid.Empty)`, which throws
/// `ArgumentException` (400). No unowned playlist is created, and the
/// routes that look a playlist up for a user answer 400 too. Naming the
/// user makes them the owner, as before, and a remove — upstream's one
/// API-key exception — still goes through.
#[tokio::test]
async fn an_api_key_without_a_user_cannot_create_or_look_up_a_playlist() {
    let fx = fixture().await;
    let (status, _) = fx
        .call("POST", "/Auth/Keys?app=playlist-items-test", None)
        .await;
    assert!(status.is_success());
    let (_, keys) = fx.call("GET", "/Auth/Keys", None).await;
    let key = keys["Items"][0]["AccessToken"].as_str().unwrap().to_owned();

    let (status, body) = call(
        &fx.router,
        &key,
        "POST",
        "/Playlists",
        Some(json!({"Name": "Keyed"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("Guid can't be empty"), "{body}");
    assert_eq!(fx.playlist_count().await, 0, "no unowned playlist");

    let (_, me) = fx.call("GET", "/Users/Me", None).await;
    let admin = me["Id"].as_str().unwrap().to_owned();
    // With members, so the items read takes its joined path, not the
    // empty-playlist fallback.
    let music = seed_music(&fx).await;
    let (status, created) = call(
        &fx.router,
        &key,
        "POST",
        &format!("/Playlists?userId={admin}"),
        Some(json!({"Name": "Keyed", "Ids": [music.album]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["Id"].as_str().unwrap().to_owned();
    let owner: Option<String> = sqlx::query_scalar(
        r#"SELECT "OwnerUserId" FROM "FerrofinPlaylists" WHERE "PlaylistId" = ?1"#,
    )
    .bind(guid_to_db(id.parse().unwrap()))
    .fetch_one(fx.db.pool())
    .await
    .unwrap();
    assert_eq!(
        owner.and_then(|o| Uuid::parse_str(&o).ok()),
        Some(admin.parse().unwrap()),
        "the named user owns it"
    );

    for (method, path) in [
        ("GET", format!("/Playlists/{id}")),
        ("POST", format!("/Playlists/{id}")),
        ("GET", format!("/Playlists/{id}/Items")),
        ("POST", format!("/Playlists/{id}/Items?ids={id}")),
        ("POST", format!("/Playlists/{id}/Items/{id}/Move/0")),
        ("GET", format!("/Playlists/{id}/Users")),
        ("GET", format!("/Playlists/{id}/Users/{admin}")),
        ("POST", format!("/Playlists/{id}/Users/{admin}")),
        ("DELETE", format!("/Playlists/{id}/Users/{admin}")),
    ] {
        let (status, body) = call(&fx.router, &key, method, &path, Some(json!({}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {path}: {body}");
        assert!(body.to_string().contains("Guid can't be empty"), "{body}");
    }
    // Upstream's one API-key exception: a remove skips the user lookup.
    assert_eq!(fx.stored(&id).await, ["Zulu", "Alpha"]);
    let first: String = sqlx::query_scalar(
        r#"SELECT "ChildId" FROM "LinkedChildren" WHERE "ParentId" = ?1 ORDER BY "SortOrder" LIMIT 1"#,
    )
    .bind(guid_to_db(id.parse().unwrap()))
    .fetch_one(fx.db.pool())
    .await
    .unwrap();
    let entry = Uuid::parse_str(&first).unwrap().simple();
    let (status, body) = call(
        &fx.router,
        &key,
        "DELETE",
        &format!("/Playlists/{id}/Items?entryIds={entry}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(fx.stored(&id).await, ["Alpha"]);
}
