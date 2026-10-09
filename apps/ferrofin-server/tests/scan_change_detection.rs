//! The scan change-detection matrix, end to end over real HTTP
//! (`PLAN_SCAN_CHANGE_DETECTION` Phase V, layer V1): a library scan
//! reprocesses only what changed, unless the request asked for more.
//!
//! The test boots the real server — [`ferrofin_server::run`], the binary's
//! own boot, with `/metrics` enabled — on a temp data dir, over a small
//! generated library: movies (one with an NFO, one with a sidecar
//! subtitle, one with TMDb artwork and a cast member), a series, a watched
//! series and a music album. Movies have TMDb's image fetcher ticked, so the
//! rows also cover artwork downloads and person refreshes — the request
//! families besides metadata that grow with a library. Everything is
//! driven over loopback HTTP, as a client would. What each scan did is
//! measured four independent ways, and the rows assert on all of them:
//!
//! - the `/metrics` counters the owner watches in production
//!   (`ferrofin_library_scans_total{trigger,result}`,
//!   `ferrofin_library_scan_items_total{trigger,outcome}`,
//!   `ferrofin_media_probe_total`, `ferrofin_metadata_provider_requests_total`)
//!   — so the matrix doubles as a test of those metrics;
//! - probe and ffmpeg spawns: `ffprobe` is a stub script that logs every call
//!   and answers a minimal probe (the fake prober seam); `ffmpeg` is a stub
//!   that answers discovery with a version banner and logs every other run
//!   (a frame or image extraction), which the quiet rows assert never happens;
//! - provider requests: every provider a library scan reaches points at an
//!   in-process mock that logs each request line — TMDb's API and its image
//!   CDN, TheTVDB, fanart.tv and TheAudioDB through
//!   [`Config::provider_endpoints`] (including OMDb), MusicBrainz and the studio
//!   artwork repository through their own settings. Not redirected: LrcLib
//!   and ListenBrainz, which a scan never calls. OpenSubtitles also points at
//!   the mock to test scan-time downloads for `SubtitleDownloadLanguages`.
//!   The other image CDNs are reached only through URLs a provider answer
//!   names, which the mock controls;
//! - database writes: a trigger on every table counts the rows written, and
//!   records which item each `BaseItems` row and each row of a table keyed by
//!   `ItemId` (streams, ancestors, item values, images, people links…)
//!   belongs to, by path.
//!
//! The rows are the plan's table:
//!
//! | # | Scenario | Expect |
//! |---|---|---|
//! | 1 | first scan | all created; every ticked provider called, in order |
//! | 2 | rescan, nothing changed | 0 probes, 0 provider requests, 0 writes, all unchanged |
//! | 3 | touch one file's mtime, rescan | exactly that item: probe + providers + save |
//! | 4 | NFO newer than DateLastSaved + 1 min | that item's local metadata re-read only |
//! | 5 | webhook and watcher: add one file | 1 created; parent via decision; nothing else |
//! | 6 | webhook and watcher: delete one file | 1 removed; the watched season asks its season providers once |
//! | 7 | edit Overview (no LockData), rescan | edit survives; `LockData` false |
//! | 8 | "Search for missing metadata" | empty fields filled, edited Overview kept |
//! | 9 | "Replace all metadata", Overview unlocked | Overview replaced |
//! | 10 | lock Overview, "Replace all metadata" | Overview kept, other fields replaced |
//! | 11 | `LockData=true`, add `poster.png`, rescan | poster discovered; 0 provider requests |
//! | 12 | locked item saved by a rescan | `Data` (trailers) intact |
//! | 13 | episode with TMDB date/rating, rescans | fields intact; the season whose folder changed asks its season providers once |
//! | 14 | pending library refresh + webhook path | no full scan queued |
//! | 15 | movie refresh: TMDB 429 and missing ffprobe | text retained; images follow replacement flags |
//!
//! Row 14's queue state has no HTTP surface, so it is asserted through its
//! observable effect: the scans that ran, by trigger (`/metrics`), their
//! scopes (the `library_scan_pass` span of each "library scan pass
//! complete" line in the server's log file) and what they processed. To
//! hold a scan running while the others queue, the ffprobe stub waits on a
//! gate file the test opens.
//!
//! Row 4 needs an NFO written more than a minute after the item's last
//! save; instead of waiting that minute, the test moves the item's
//! `DateLastSaved` back — its only write outside HTTP. Rows 5 and 6 run
//! twice: through `POST /Library/Media/Updated` on the movies library, and
//! through the real inotify watcher on a library with real-time monitoring
//! on, with `LibraryMonitorDelay` at 1 s.
//!
//! Rows 6 (watcher) and 13 refresh a season because its folder changed (a
//! file left or joined it): upstream's `requiresRefresh`
//! (`BaseItem.RequiresRefresh`, `Folder.RequiresRefresh`) makes
//! `GetProviders` run every remote provider of the season
//! (`MetadataService.cs:655-658`), and the Shows and Live libraries tick
//! TheMovieDb alone for seasons. `TmdbSeasonProvider` then asks for the
//! season by its series' recorded `Tmdb` id: exactly one
//! `/tmdb/tv/{series}/season/1` request, which an episode refreshed in the
//! same scan shares (row 5). Every other row's 0 holds: an unchanged season
//! runs no provider.
//!
//! The pinned count is the request the refresh decision makes, not
//! upstream's network count. Upstream's `TmdbClientManager` keeps each season
//! response in memory for an hour (`season-{id}-s{n}-{lang}`,
//! `TmdbClientManager.cs:238-262`), so it would serve rows 6 and 13 from
//! memory a few seconds after row 1 fetched the same seasons, with 0 network
//! requests. Ferrofin's TMDB season cache lasts one scan, so each of those
//! rows asks once.
//!
//! One test function, one process: the metrics pipeline is process-global.

// The counters render as exact integers, so comparing their f64 samples
// exactly is correct.
#![allow(clippy::float_cmp)]

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferrofin_server::config::{Config, ProviderEndpoints};
use serde_json::{Value, json};

const CLIENT: &str =
    r#"MediaBrowser Client="scan-matrix", Device="test", DeviceId="scan-matrix", Version="1""#;

/// Ceiling for one HTTP request: a hang guard, not a latency assertion.
const REQUEST_TIMEOUT: Duration = Duration::from_mins(1);

/// How long the server gets to bind and boot (debug build, busy machine).
const STARTUP_DEADLINE: Duration = Duration::from_mins(2);

/// How long a scan may take before the test calls it hung. The scans here
/// cover a dozen files; this is a hang guard, not a latency assertion.
const SCAN_DEADLINE: Duration = Duration::from_mins(2);

/// How often the test polls for a scan or a file to appear.
const POLL: Duration = Duration::from_millis(50);

/// How often the ffprobe stub checks its gate (row 14) while it waits.
const GATE_POLL: Duration = Duration::from_millis(50);

/// MusicBrainz's `RateLimit` plugin setting for the mock, in seconds between
/// requests: the 1 s default (musicbrainz.org's published limit, which a
/// mirror may lift) would add seconds of idle waiting to the first scan.
const MUSICBRAINZ_RATE_LIMIT: f64 = 0.01;

/// `LibraryMonitorDelay`, the watcher/webhook settle window, in seconds: the
/// smallest non-zero value, so a file's burst of inotify events (create,
/// write) settles into one scan instead of racing into several.
const SETTLE_SECONDS: i64 = 1;

/// A 1×1 PNG, the local poster row 11 adds.
const POSTER_PNG: [u8; 69] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xdf, 0xc0, 0x00,
    0x00, 0x04, 0x01, 0x01, 0x80, 0xc5, 0x2a, 0x18, 0x5d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e,
    0x44, 0xae, 0x42, 0x60, 0x82,
];

// ---------------------------------------------------------------------------
// the provider stand-in
// ---------------------------------------------------------------------------

/// The movies the TMDB stand-in knows: folder title → TMDB id.
const MOVIES: [(&str, u32); 5] = [
    ("Alpha", 101),
    ("Beta", 102),
    ("Gamma", 103),
    ("Delta", 104),
    ("Epsilon", 105),
];

/// The series the TMDB stand-in knows: title → TMDB id.
const SHOWS: [(&str, u32); 2] = [("Harbor", 201), ("Lantern", 202)];

/// Harbor's TheTVDB id, which TMDB's `external_ids` carry for it — so the
/// TVDB provider, running after TheMovieDb, resolves Harbor by it without a
/// search (`MergeNewData`).
const HARBOR_TVDB: u32 = 301;

/// Every remote provider's stand-in on one port, each under its own path
/// prefix (`/tmdb`, `/image` for TMDb's image CDN, `/tvdb`, `/fanart`,
/// `/audiodb`, `/mb`, `/studios`), logging every request line.
struct Providers {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    tmdb_unavailable: Arc<AtomicBool>,
    subtitles_unavailable: Arc<AtomicBool>,
}

impl Providers {
    fn spawn() -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let tmdb_unavailable = Arc::new(AtomicBool::new(false));
        let unavailable = Arc::clone(&tmdb_unavailable);
        let subtitles_unavailable = Arc::new(AtomicBool::new(false));
        let subs_unavailable = Arc::clone(&subtitles_unavailable);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind provider mock");
        let addr = listener.local_addr().expect("mock addr");
        let log = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = req.lines().next().unwrap_or_default().to_owned();
                log.lock().expect("lock").push(line.clone());
                let target = line.split_whitespace().nth(1).unwrap_or_default();
                if target.starts_with("/tmdb/") && unavailable.load(Ordering::Relaxed) {
                    // Longer than the maximum configured provider wait (300 s), so
                    // the scan skips the shared cooldown without sleeping.
                    let _ = write!(
                        s,
                        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 3600\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                    );
                    continue;
                }
                if target.starts_with("/opensubtitles/") && subs_unavailable.load(Ordering::Relaxed)
                {
                    let _ = write!(
                        s,
                        "HTTP/1.1 400 Bad Request\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                    );
                    continue;
                }
                let (status, content_type, body) = if target == "/subtitle-file" {
                    (
                        "200 OK",
                        "text/plain",
                        b"1\n00:00:00,000 --> 00:00:01,000\nHello\n".to_vec(),
                    )
                } else if target.starts_with("/image/") {
                    ("200 OK", "image/png", POSTER_PNG.to_vec())
                } else {
                    match subtitle_answer(target, addr)
                        .or_else(|| parity_answer(target, addr))
                        .or_else(|| omdb_answer(target))
                        .or_else(|| answer(target))
                    {
                        Some(body) => ("200 OK", "application/json", body.into_bytes()),
                        None => ("404 Not Found", "application/json", b"{}".to_vec()),
                    }
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(&body);
            }
        });
        Self {
            base: format!("http://{addr}"),
            requests,
            tmdb_unavailable,
            subtitles_unavailable,
        }
    }

    /// Every request line so far.
    fn all(&self) -> Vec<String> {
        self.requests.lock().expect("lock").clone()
    }
}

fn subtitle_answer(target: &str, addr: std::net::SocketAddr) -> Option<String> {
    let path = target.split('?').next()?;
    Some(match path {
        "/opensubtitles/login" => json!({"token":"test-token"}).to_string(),
        "/opensubtitles/download" => {
            json!({"link":format!("http://{addr}/subtitle-file")}).to_string()
        }
        "/opensubtitles/subtitles" => {
            assert!(target.contains("moviehash="));
            assert!(target.contains("moviehash_match=only"));
            // A nonmatching candidate first must never win perfect-match mode.
            json!({"data":[
                {"attributes":{"moviehash_match":false,"files":[{"file_id":99}]}},
                {"attributes":{"moviehash_match":true,"files":[{"file_id":42}]}}
            ]})
            .to_string()
        }
        _ => return None,
    })
}

/// Distinct artwork URLs let the HTTP matrix prove which provider won.
fn parity_answer(target: &str, addr: std::net::SocketAddr) -> Option<String> {
    let path = target.split('?').next()?;
    let art = |file: &str| format!("http://{addr}/image/{file}.png");
    Some(match path {
        "/tmdb/movie/901" => json!({"id":901,"title":"Mapped movie","original_title":"Original movie","original_language":"ja",
            "overview":"Mapped overview", "videos":{"results":[{"site":"YouTube","type":"Trailer","key":"mapped","name":"Trailer"}]}, "production_countries":[{"name":"Japan"}],"keywords":{"keywords":[{"name":"mapped keyword"}]},
            "belongs_to_collection":{"id":99,"name":"Mapped collection"},"poster_path":"/mapped-tmdb.png","credits":{"cast":[],"crew":[]}}),
        "/tmdb/movie/901/images" => json!({"posters":[{"file_path":"/mapped-tmdb.png","width":640,"height":960}],"backdrops":[],"logos":[]}),
        "/fanart/movies/901" => json!({"movieposter":[{"url":art("mapped-fanart"),"lang":"en"}]}),
        "/tvdb/login" => json!({"data":{"token":"test"}}),
        "/tvdb/search/remoteid/tt123456" | "/tvdb/search/remoteid/777" => json!({"data":[{"series":{"id":42}}]}),
        "/tvdb/series/42/extended" => json!({"data":{"id":42,"name":"Mapped series","overview":"TVDB series overview",
            "translations":{"nameTranslations":[{"language":"eng","name":"Mapped series"}],"overviewTranslations":[{"language":"eng","overview":"TVDB series overview"}]},
            "averageRuntime":42,"slug":"mapped-series","lists":[{"id":9,"isOfficial":true}],
            "remoteIds":[{"sourceName":"IMDB","id":"tt123456"}],
            "artworks":[{"type":2,"image":art("mapped-tvdb")}]
        }}),
        "/tvdb/series/42/episodes/official" => json!({"data":{"episodes":[{"id":43,"seasonNumber":1,"number":1}]}}),
        "/tvdb/episodes/43/extended" => json!({"data":{"id":43,"name":"Mapped episode","overview":"TVDB episode overview",
            "translations":{"nameTranslations":[{"language":"eng","name":"Mapped episode"}],"overviewTranslations":[{"language":"eng","overview":"TVDB episode overview"}]},
            "airsBeforeEpisode":2,"airsBeforeSeason":1,"airsAfterSeason":0,
            "remoteIds":[{"sourceName":"IMDB","id":"tt123457"}],"characters":[]}}),
        "/fanart/tv/42" => json!({"tvposter":[{"url":art("mapped-series-fanart"),"lang":"en"}]}),
        _ => return None,
    }.to_string())
}

/// The request lines of `provider` (its path prefix) among `lines`.
fn of<'a>(lines: &'a [String], provider: &str) -> Vec<&'a String> {
    let prefix = format!(" /{provider}/");
    lines.iter().filter(|l| l.contains(&prefix)).collect()
}

/// A trailer, so the D2 backfill heuristic ("no remote trailers") never asks
/// again on its own.
fn trailer(title: &str) -> String {
    format!(
        r#"{{"results": [{{"site": "YouTube", "type": "Trailer", "key": "k{title}", "name": "{title} trailer"}}]}}"#
    )
}

/// OMDb's response also verifies the credential supplied by the real server.
fn omdb_answer(target: &str) -> Option<String> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path == "/omdb/" {
        assert!(
            query.split('&').any(|p| p == "apikey=2c9d9507"),
            "default OMDb key"
        );
        return Some(
            r#"{"Response":"True","Title":"Shared Key Movie","Year":"1999",
            "imdbID":"tt1234567","Plot":"Fetched with the shared key.","imdbRating":"8.0",
            "Ratings":[{"Source":"Rotten Tomatoes","Value":"87%"}]}"#
                .to_owned(),
        );
    }
    None
}

/// Movie detail and image-list replies are separate provider requests.
fn movie_answer(id: &str) -> Option<String> {
    if let Some(id) = id.strip_suffix("/images") {
        let (title, _) = MOVIES.iter().find(|(_, m)| m.to_string() == id)?;
        let posters = if *title == "Alpha" {
            json!([{"file_path":"/alpha-poster.png","width":640,"height":960}])
        } else {
            json!([])
        };
        return Some(json!({"posters":posters,"backdrops":[],"logos":[]}).to_string());
    }
    let (title, _) = MOVIES.iter().find(|(_, m)| m.to_string() == id)?;
    // Alpha alone has artwork and a cast member: the artwork and person
    // requests of every row are then Alpha's (or nobody's).
    let (poster, cast) = if *title == "Alpha" {
        (
            r#""poster_path": "/alpha-poster.png","#,
            r#"[{"id": 501, "name": "Ada Actor", "character": "Lead", "order": 0,
                     "known_for_department": "Acting", "profile_path": "/ada.png"}]"#,
        )
    } else {
        ("", "[]")
    };
    Some(format!(
        r#"{{"id": {id}, "title": "{title}", "overview": "About {title}.", "vote_average": 8.0,
                "release_date": "1999-03-30", {poster} "videos": {}, "credits": {{"cast": {cast}, "crew": []}},
                "release_dates": {{"results": []}}}}"#,
        trailer(title)
    ))
}

/// The stand-in's answer to `target` (path + query), `None` for a 404.
fn answer(target: &str) -> Option<String> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path == "/tmdb/search/movie" {
        let hits: Vec<String> = MOVIES
            .iter()
            .filter(|(title, _)| query.contains(title))
            .map(|(title, id)| format!(r#"{{"id": {id}, "title": "{title}"}}"#))
            .collect();
        return Some(format!(r#"{{"results": [{}]}}"#, hits.join(",")));
    }
    if path == "/tmdb/search/tv" {
        let hits: Vec<String> = SHOWS
            .iter()
            .filter(|(title, _)| query.contains(title))
            .map(|(title, id)| format!(r#"{{"id": {id}, "name": "{title}"}}"#))
            .collect();
        return Some(format!(r#"{{"results": [{}]}}"#, hits.join(",")));
    }
    if let Some(id) = path.strip_prefix("/tmdb/movie/") {
        return movie_answer(id);
    }
    if path == "/tmdb/person/501" {
        return Some(
            r#"{"id": 501, "name": "Ada Actor", "biography": "An actor.", "birthday": "1970-01-01",
                "place_of_birth": "Nowhere", "profile_path": "/ada.png",
                "images": {"profiles": [{"file_path": "/ada.png", "width": 1, "height": 1}]},
                "external_ids": {}}"#
                .to_owned(),
        );
    }
    if let Some(rest) = path.strip_prefix("/tmdb/tv/") {
        let mut parts = rest.split('/');
        let id = parts.next()?;
        let (title, _) = SHOWS.iter().find(|(_, s)| s.to_string() == id)?;
        if rest.ends_with("/images") {
            return Some(json!({"posters":[],"backdrops":[],"logos":[],"stills":[]}).to_string());
        }
        let episode = |n: u32| {
            format!(
                r#"{{"id": {n}, "episode_number": {n}, "season_number": 1, "name": "{title} episode {n}",
                    "overview": "Episode {n} of {title}.", "air_date": "2010-01-0{n}", "vote_average": 7.5, "credits": {{"cast":[],"crew":[]}}, "external_ids": {{"imdb_id":"tt900{n}","tvdb_id":900{n}}}, "videos": {{"results":[]}}}}"#
            )
        };
        let external_ids = if *title == "Harbor" {
            format!(r#"{{"tvdb_id": {HARBOR_TVDB}}}"#)
        } else {
            "{}".to_owned()
        };
        return match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (None, ..) => Some(format!(
                r#"{{"id": {id}, "name": "{title}", "overview": "About {title}.", "vote_average": 8.1,
                    "first_air_date": "2010-01-01", "videos": {}, "credits": {{"cast": [], "crew": []}},
                    "content_ratings": {{"results": []}}, "external_ids": {external_ids}}}"#,
                trailer(title)
            )),
            (Some("season"), Some("1"), None, _) => Some(format!(
                r#"{{"id": 1, "season_number": 1, "name": "Season 1", "overview": "The first season.",
                    "air_date": "2010-01-01", "episodes": [{}, {}, {}]}}"#,
                episode(1),
                episode(2),
                episode(3)
            )),
            (Some("season"), Some("1"), Some("episode"), Some(n)) => {
                let n: u32 = n.parse().ok()?;
                Some(episode(n))
            }
            _ => None,
        };
    }
    if let Some(rest) = path.strip_prefix("/tvdb/") {
        return tvdb(rest);
    }
    if path.starts_with("/mb/ws/2/") {
        return Some(musicbrainz(path));
    }
    if path == "/audiodb/album-mb.php" {
        return Some(
            r#"{"album":[{"strDescriptionEN":"Album text.","strGenre":"Jazz"}]}"#.to_owned(),
        );
    }
    if path == "/audiodb/artist-mb.php" {
        return Some(
            r#"{"artists":[{"strBiographyEN":"Artist bio.","strGenre":"Jazz"}]}"#.to_owned(),
        );
    }
    None
}

/// TheTVDB's answers (`rest` is the path under `/tvdb/`): a login token, and
/// Harbor alone — its record, crediting one character (TMDB's credits
/// nobody on it), and each episode's, with a different title and air date
/// than TMDB's, crediting nobody. A search finds nothing.
fn tvdb(rest: &str) -> Option<String> {
    if rest == "login" {
        return Some(r#"{"data":{"token":"matrix"}}"#.to_owned());
    }
    if rest == "search" {
        return Some(r#"{"data":[]}"#.to_owned());
    }
    if rest == format!("series/{HARBOR_TVDB}/extended") {
        return Some(format!(
            r#"{{"data":{{"id":{HARBOR_TVDB},"name":"Harbor","overview":"TVDB's Harbor.",
                "characters":[{{"personName":"Tess Tvdb","peopleType":"Actor","name":"Keeper"}}]}}}}"#
        ));
    }
    if rest == format!("series/{HARBOR_TVDB}/episodes/official") {
        return Some(
            r#"{"data":{"episodes":[{"id":3001,"seasonNumber":1,"number":1},
                {"id":3002,"seasonNumber":1,"number":2}]}}"#
                .to_owned(),
        );
    }
    let n = rest
        .strip_prefix("episodes/300")?
        .strip_suffix("/extended")?;
    Some(format!(
        r#"{{"data":{{"name":"Harbor on TVDB {n}","overview":"TVDB's episode {n}.",
            "aired":"2011-02-0{n}","characters":[]}}}}"#
    ))
}

/// MusicBrainz answers for one album by one artist.
fn musicbrainz(path: &str) -> String {
    const RELEASE: &str = "11111111-1111-4111-8111-111111111111";
    const GROUP: &str = "22222222-2222-4222-8222-222222222222";
    const ARTIST: &str = "33333333-3333-4333-8333-333333333333";
    let release = format!(
        r#"{{"id":"{RELEASE}","title":"Kind of Blue","date":"1959-08-17","release-group":{{"id":"{GROUP}"}},"artist-credit":[{{"name":"Miles Davis","artist":{{"id":"{ARTIST}","name":"Miles Davis"}}}}]}}"#
    );
    if path.starts_with("/mb/ws/2/release-group/") {
        format!(
            r#"{{"id":"{GROUP}","title":"Kind of Blue","first-release-date":"1959-08-17","releases":[{{"id":"{RELEASE}"}}],"artist-credit":[{{"name":"Miles Davis"}}]}}"#
        )
    } else if path.starts_with("/mb/ws/2/release/") {
        release
    } else if path == "/mb/ws/2/release" {
        format!(r#"{{"releases":[{release}]}}"#)
    } else if path.starts_with("/mb/ws/2/artist/") {
        format!(r#"{{"id":"{ARTIST}","name":"Miles Davis"}}"#)
    } else {
        format!(r#"{{"artists":[{{"id":"{ARTIST}","name":"Miles Davis"}}]}}"#)
    }
}

// ---------------------------------------------------------------------------
// ffmpeg / ffprobe stubs
// ---------------------------------------------------------------------------

/// The version banner both stubs print (ffmpeg discovery checks the tool
/// name and the library versions).
fn banner(tool: &str) -> String {
    format!(
        "{tool} version 6.1.1 Copyright (c) 2000-2023 the FFmpeg developers\n\
         libavutil      58. 29.100\nlibavcodec     60. 31.102\nlibavformat    60. 16.100\n\
         libavdevice    60.  3.100\nlibavfilter     9. 12.100\nlibswscale      7.  5.100\n\
         libswresample   4. 12.100\n"
    )
}

/// The files the stubs need, all under one directory.
struct Stubs {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    /// One line per probe: the ffprobe arguments.
    calls: PathBuf,
    /// One line per ffmpeg run other than `-version` (discovery's capability
    /// probes at boot, then any frame or image extraction a scan starts).
    ffmpeg_calls: PathBuf,
    /// While this file exists, a video probe waits (row 14).
    gate: PathBuf,
    /// Created by a video probe that found the gate closed.
    waiting: PathBuf,
}

impl Stubs {
    fn write(dir: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        let stubs = Self {
            ffmpeg: dir.join("ffmpeg"),
            ffprobe: dir.join("ffprobe"),
            calls: dir.join("ffprobe.calls"),
            ffmpeg_calls: dir.join("ffmpeg.calls"),
            gate: dir.join("probe.gate"),
            waiting: dir.join("probe.waiting"),
        };
        let ffmpeg = format!(
            "#!/bin/sh\n[ \"$1\" = -version ] || printf '%s\\n' \"$*\" >> '{}'\ncat <<'EOF'\n{}EOF\n",
            stubs.ffmpeg_calls.display(),
            banner("ffmpeg")
        );
        let video = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":640,"height":360},{"index":1,"codec_type":"audio","codec_name":"aac","channels":2}],"format":{"format_name":"matroska,webm","duration":"60.000000","size":"1024","bit_rate":"1000"}}"#;
        let subtitle = r#"{"streams":[{"index":0,"codec_type":"subtitle","codec_name":"subrip"}],"format":{"format_name":"srt"}}"#;
        let ffprobe = format!(
            r#"#!/bin/sh
if [ "$1" = "-version" ]; then
cat <<'EOF'
{banner}EOF
exit 0
fi
printf '%s\n' "$*" >> '{calls}'
case "$*" in
*.srt*)
cat <<'EOF'
{subtitle}
EOF
;;
*.flac*)
case "$*" in *"So What"*) title="So What"; track=1;; *) title="Blue in Green"; track=2;; esac
cat <<EOF
{{"streams":[{{"index":0,"codec_type":"audio","codec_name":"flac","channels":2,"sample_rate":"44100"}}],"format":{{"format_name":"flac","duration":"300.000000","size":"1024","bit_rate":"900000","tags":{{"title":"$title","track":"$track","album":"Kind of Blue","artist":"Miles Davis","album_artist":"Miles Davis"}}}}}}
EOF
;;
*)
if [ -e '{gate}' ]; then
  : > '{waiting}'
  n=0
  while [ -e '{gate}' ] && [ "$n" -lt {gate_polls} ]; do sleep {gate_sleep}; n=$((n + 1)); done
fi
cat <<'EOF'
{video}
EOF
;;
esac
"#,
            banner = banner("ffprobe"),
            calls = stubs.calls.display(),
            gate = stubs.gate.display(),
            waiting = stubs.waiting.display(),
            // The gate holds a probe at most as long as a scan may take.
            gate_polls = SCAN_DEADLINE.as_millis() / GATE_POLL.as_millis(),
            gate_sleep = GATE_POLL.as_secs_f64(),
        );
        for (path, script) in [(&stubs.ffmpeg, ffmpeg), (&stubs.ffprobe, ffprobe)] {
            std::fs::write(path, script).expect("write stub");
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
        }
        stubs
    }

    /// Every probe so far (its argument line).
    fn calls(&self) -> Vec<String> {
        lines(&self.calls)
    }

    /// Every ffmpeg run so far other than `-version` (its argument line).
    fn ffmpeg_calls(&self) -> Vec<String> {
        lines(&self.ffmpeg_calls)
    }
}

/// The lines of a stub's call log (none before its first call).
fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|s| s.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// the generated library
// ---------------------------------------------------------------------------

/// Writes `bytes` to `path`, creating its directory.
fn put(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, bytes).expect("write");
}

/// Moves `path`'s mtime `seconds` from now.
fn set_mtime(path: &Path, seconds: i64) {
    let now = std::time::SystemTime::now();
    let at = if seconds >= 0 {
        now + Duration::from_secs(seconds.unsigned_abs())
    } else {
        now - Duration::from_secs(seconds.unsigned_abs())
    };
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open")
        .set_modified(at)
        .expect("set mtime");
}

/// Gamma's NFO, with `plot`.
fn movie_nfo(plot: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<movie>\n  <title>Gamma</title>\n  <plot>{plot}</plot>\n</movie>\n"
    )
}

/// The media paths the rows touch.
struct Media {
    root: PathBuf,
    alpha: PathBuf,
    beta: PathBuf,
    gamma_nfo: PathBuf,
    delta: PathBuf,
    epsilon: PathBuf,
    harbor_e1: PathBuf,
    harbor_e2: PathBuf,
    lantern_e2: PathBuf,
}

impl Media {
    fn generate(root: &Path) -> Self {
        let movie = |title: &str| {
            root.join("movies")
                .join(format!("{title} (1999)"))
                .join(format!("{title} (1999).mkv"))
        };
        let episode = |show: &str, lib: &str, n: u32| {
            root.join(lib)
                .join(show)
                .join("Season 01")
                .join(format!("{show} - S01E0{n}.mkv"))
        };
        let media = Self {
            root: root.to_owned(),
            alpha: movie("Alpha"),
            beta: movie("Beta"),
            gamma_nfo: movie("Gamma").with_extension("nfo"),
            delta: movie("Delta"),
            epsilon: movie("Epsilon"),
            harbor_e1: episode("Harbor", "shows", 1),
            harbor_e2: episode("Harbor", "shows", 2),
            lantern_e2: episode("Lantern", "live", 2),
        };
        let clip = [0u8; 1024];
        put(&media.alpha, &clip);
        put(&media.beta, &clip);
        put(
            &media.beta.with_file_name("Beta (1999).en.srt"),
            b"1\n00:00:01,000 --> 00:00:02,000\nHi\n",
        );
        put(&movie("Gamma"), &clip);
        put(
            &media.gamma_nfo,
            movie_nfo("Gamma from its NFO.").as_bytes(),
        );
        put(&media.harbor_e1, &clip);
        put(&media.harbor_e2, &clip);
        put(&episode("Lantern", "live", 1), &clip);
        let album = root.join("music").join("Miles Davis").join("Kind of Blue");
        put(&album.join("01 - So What.flac"), &clip);
        put(&album.join("02 - Blue in Green.flac"), &clip);
        // The files the rows touch, an hour in the past, so touching one
        // stands out by more than the scan's 1 s mtime tolerance.
        for path in [
            &media.alpha,
            &media.beta,
            &media.harbor_e1,
            &media.harbor_e2,
        ] {
            set_mtime(path, -3_600);
        }
        media
    }

    fn library(&self, name: &str) -> String {
        self.root.join(name).to_string_lossy().into_owned()
    }

    /// `path`'s key in the write log: its path under the media root.
    fn key(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .expect("under the media root")
            .to_string_lossy()
            .into_owned()
    }
}

// ---------------------------------------------------------------------------
// /metrics
// ---------------------------------------------------------------------------

/// One `/metrics` scrape: each series (name + label set) and its value.
#[derive(Clone, Default)]
struct Scrape(Vec<(String, BTreeMap<String, String>, f64)>);

impl Scrape {
    fn parse(text: &str) -> Self {
        let mut series = Vec::new();
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let Some((head, value)) = line.rsplit_once(' ') else {
                continue;
            };
            let Ok(value) = value.parse::<f64>() else {
                continue;
            };
            let (name, labels) = match head.split_once('{') {
                Some((name, rest)) => (name, rest.trim_end_matches('}')),
                None => (head, ""),
            };
            let labels = labels
                .split("\",")
                .filter_map(|pair| {
                    let (k, v) = pair.split_once("=\"")?;
                    Some((k.to_owned(), v.trim_end_matches('"').to_owned()))
                })
                .collect();
            series.push((name.to_owned(), labels, value));
        }
        Self(series)
    }

    /// The sum of every `name` series carrying all of `labels`.
    fn sum(&self, name: &str, labels: &[(&str, &str)]) -> f64 {
        self.0
            .iter()
            .filter(|(n, l, _)| {
                n == name
                    && labels
                        .iter()
                        .all(|(k, v)| l.get(*k).map(String::as_str) == Some(*v))
            })
            .map(|(_, _, v)| v)
            .sum()
    }

    /// `name`'s values by the value of `label`, summed over the rest.
    fn by(&self, name: &str, label: &str) -> BTreeMap<String, f64> {
        let mut out = BTreeMap::new();
        for (n, l, v) in &self.0 {
            if n == name
                && let Some(key) = l.get(label)
            {
                *out.entry(key.clone()).or_insert(0.0) += v;
            }
        }
        out
    }
}

/// A count as the f64 the counters are compared in.
fn count(n: usize) -> f64 {
    f64::from(u32::try_from(n).expect("a small count"))
}

const SCANS: &str = "ferrofin_library_scans_total";
const ITEMS: &str = "ferrofin_library_scan_items_total";
const PROBES: &str = "ferrofin_media_probe_total";
const PROVIDER_REQUESTS: &str = "ferrofin_metadata_provider_requests_total";

// ---------------------------------------------------------------------------
// database writes
// ---------------------------------------------------------------------------

/// Counts every row written to every table and logs which item each written
/// row belongs to — a `BaseItems` row itself, or a row of a table keyed by
/// `ItemId` — through triggers installed over a second connection to the
/// server's database file.
struct Writes {
    db: ferrofin_db::Database,
    /// The media root, to name items by their path under it.
    media: PathBuf,
    /// `BaseItems.Id` (lowercase) → the item's key ([`Writes::key`]),
    /// remembered across calls so a deleted item keeps its name.
    keys: Mutex<BTreeMap<String, String>>,
}

impl Writes {
    async fn install(url: &str, media: &Path) -> Self {
        let db = ferrofin_db::Database::connect(url)
            .await
            .expect("second connection");
        sqlx::query(r#"CREATE TABLE "TestWrites" ("Tbl" TEXT PRIMARY KEY, "N" INTEGER NOT NULL)"#)
            .execute(db.writer())
            .await
            .expect("counter table");
        sqlx::query(
            r#"CREATE TABLE "TestItemWrites" ("Seq" INTEGER PRIMARY KEY, "Tbl" TEXT, "Op" TEXT,
                 "Id" TEXT, "Path" TEXT, "Name" TEXT, "Type" TEXT)"#,
        )
        .execute(db.writer())
        .await
        .expect("item log table");
        let tables: Vec<String> = sqlx::query_scalar(
            r#"SELECT "name" FROM sqlite_master WHERE "type" = 'table'
                 AND "name" NOT LIKE 'sqlite_%' AND "name" NOT LIKE 'Test%'"#,
        )
        .fetch_all(db.pool())
        .await
        .expect("tables");
        for table in tables {
            sqlx::query(r#"INSERT INTO "TestWrites" VALUES (?1, 0)"#)
                .bind(&table)
                .execute(db.writer())
                .await
                .expect("counter row");
            let keyed: i64 = sqlx::query_scalar(
                r#"SELECT COUNT(*) FROM pragma_table_info(?1) WHERE "name" = 'ItemId'"#,
            )
            .bind(&table)
            .fetch_one(db.pool())
            .await
            .expect("columns");
            for (event, row) in [("INSERT", "NEW"), ("UPDATE", "NEW"), ("DELETE", "OLD")] {
                let item_log = if table == "BaseItems" {
                    format!(
                        r#"INSERT INTO "TestItemWrites" ("Tbl", "Op", "Id", "Path", "Name", "Type")
                           VALUES ('{table}', '{event}', {row}."Id", {row}."Path", {row}."Name", {row}."Type");"#
                    )
                } else if keyed > 0 {
                    format!(
                        r#"INSERT INTO "TestItemWrites" ("Tbl", "Op", "Id")
                           VALUES ('{table}', '{event}', {row}."ItemId");"#
                    )
                } else {
                    String::new()
                };
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    r#"CREATE TRIGGER "TestWrites_{table}_{event}" AFTER {event} ON "{table}"
                       BEGIN UPDATE "TestWrites" SET "N" = "N" + 1 WHERE "Tbl" = '{table}'; {item_log} END"#
                )))
                .execute(db.writer())
                .await
                .expect("trigger");
            }
        }
        Self {
            db,
            media: media.to_owned(),
            keys: Mutex::new(BTreeMap::new()),
        }
    }

    /// How an item is named in the write log: its path under the media root
    /// (`live/Lantern/Season 01`), which tells two "Season 1" rows apart, or
    /// `Type:Name` for an item outside it (a person, a genre, a year).
    fn key(&self, path: Option<&str>, name: Option<&str>, kind: Option<&str>) -> String {
        let root = format!("{}/", self.media.display());
        match path.and_then(|p| p.strip_prefix(&root)) {
            Some(relative) => relative.to_owned(),
            None => format!(
                "{}:{}",
                kind.and_then(|t| t.rsplit('.').next()).unwrap_or("?"),
                name.unwrap_or_default()
            ),
        }
    }

    async fn reset(&self) {
        sqlx::query(r#"UPDATE "TestWrites" SET "N" = 0"#)
            .execute(self.db.writer())
            .await
            .expect("reset counts");
        sqlx::query(r#"DELETE FROM "TestItemWrites""#)
            .execute(self.db.writer())
            .await
            .expect("reset item log");
    }

    /// Asserts that no item data was written since the last reset: a write
    /// that landed after a row's counts were taken (a late asynchronous one)
    /// would otherwise be wiped unseen by the next row's reset.
    async fn assert_quiet(&self, context: &str) {
        let (tables, items) = self.take().await;
        let item_tables: BTreeMap<&String, &i64> = tables
            .iter()
            .filter(|(t, _)| !REQUEST_TABLES.contains(&t.as_str()))
            .collect();
        assert!(
            item_tables.is_empty() && items.is_empty(),
            "{context}: item rows written outside every row's window: {item_tables:?} {items:?}"
        );
    }

    /// Moves the `BaseItems` row named `name`'s `DateLastSaved` ten minutes
    /// into the past (in the server's own `datetime_to_db` format).
    async fn backdate_last_saved(&self, name: &str) {
        self.assert_quiet("before the backdate").await;
        let done = sqlx::query(
            r#"UPDATE "BaseItems"
               SET "DateLastSaved" = strftime('%Y-%m-%d %H:%M:%S', 'now', '-10 minutes') || '.0000000'
               WHERE "Name" = ?1"#,
        )
        .bind(name)
        .execute(self.db.writer())
        .await
        .expect("backdate");
        assert_eq!(done.rows_affected(), 1, "one row named {name}");
        self.reset().await;
    }

    /// Every column of the `BaseItems` row named `name`, as SQL literals.
    async fn row(&self, name: &str) -> BTreeMap<String, String> {
        let columns: Vec<String> =
            sqlx::query_scalar(r#"SELECT "name" FROM pragma_table_info('BaseItems')"#)
                .fetch_all(self.db.pool())
                .await
                .expect("columns");
        let select = columns
            .iter()
            .map(|c| format!(r#"quote("{c}")"#))
            .collect::<Vec<_>>()
            .join(", ");
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            r#"SELECT {select} FROM "BaseItems" WHERE "Name" = ?1"#
        )))
        .bind(name)
        .fetch_one(self.db.pool())
        .await
        .expect("row");
        columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let value: String = sqlx::Row::try_get(&row, i).expect("quoted column");
                (c.clone(), value)
            })
            .collect()
    }

    /// Rows written per table, and every item-attributed row written
    /// (table, operation, item key), since the last reset.
    async fn take(&self) -> (BTreeMap<String, i64>, Vec<(String, String, String)>) {
        type Logged = (
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        type Stored = (String, Option<String>, Option<String>, Option<String>);
        let tables: Vec<(String, i64)> =
            sqlx::query_as(r#"SELECT "Tbl", "N" FROM "TestWrites" WHERE "N" > 0"#)
                .fetch_all(self.db.pool())
                .await
                .expect("writes");
        let logged: Vec<Logged> = sqlx::query_as(
            r#"SELECT "Tbl", "Op", "Id", "Path", "Name", "Type" FROM "TestItemWrites" ORDER BY "Seq""#,
        )
        .fetch_all(self.db.pool())
        .await
        .expect("item writes");
        let stored: Vec<Stored> =
            sqlx::query_as(r#"SELECT "Id", "Path", "Name", "Type" FROM "BaseItems""#)
                .fetch_all(self.db.pool())
                .await
                .expect("items");
        let mut keys = self.keys.lock().expect("lock");
        for (id, path, name, kind) in stored {
            let key = self.key(path.as_deref(), name.as_deref(), kind.as_deref());
            keys.insert(id.to_lowercase(), key);
        }
        // A BaseItems row logs its own path (a deleted item's included).
        for (table, _, id, path, name, kind) in &logged {
            if table == "BaseItems" {
                let key = self.key(path.as_deref(), name.as_deref(), kind.as_deref());
                keys.insert(id.to_lowercase(), key);
            }
        }
        let items = logged
            .into_iter()
            .map(|(table, op, id, ..)| {
                let key = keys
                    .get(&id.to_lowercase())
                    .cloned()
                    .unwrap_or_else(|| format!("id:{id}"));
                (table, op, key)
            })
            .collect();
        (tables.into_iter().collect(), items)
    }
}

/// The item tables a new, removed or re-probed media file writes: its row,
/// its ancestors, its provider ids, its streams.
const NEW_FILE_TABLES: &[&str] = &[
    "AncestorIds",
    "BaseItemProviders",
    "BaseItems",
    "MediaStreamInfos",
];

/// The item tables a movie's provider pass may write: its row, provider ids,
/// streams (the probe), people links, and the image rows of its artwork and
/// of the people it credits. Never `Peoples`, `ItemValues`,
/// `LinkedChildren`…: the stand-in answers the same every time.
const PROVIDER_PASS_TABLES: &[&str] = &[
    "BaseItemImageInfos",
    "BaseItemProviders",
    "BaseItems",
    "MediaStreamInfos",
    "PeopleBaseItemMap",
];

/// The tables a request with no scan in it still writes: the session and
/// activity bookkeeping of the HTTP calls themselves. Every other table is
/// item data, which a quiet rescan must leave alone.
const REQUEST_TABLES: [&str; 3] = ["ActivityLogs", "Devices", "Users"];

// ---------------------------------------------------------------------------
// the server
// ---------------------------------------------------------------------------

/// A port nobody is listening on right now (a probe, not a reservation).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local addr")
        .port()
}

/// What one row observed between [`Harness::mark`] and [`Harness::settle`].
#[derive(Debug, Default)]
struct Seen {
    /// Completed scan passes by trigger.
    scans: BTreeMap<String, f64>,
    /// Scan passes that did not complete, by trigger.
    unfinished: BTreeMap<String, f64>,
    /// Items by outcome, by trigger.
    items: BTreeMap<(String, String), f64>,
    /// `ferrofin_media_probe_total` by result.
    probes: BTreeMap<String, f64>,
    /// The ffprobe stub's calls.
    spawns: Vec<String>,
    /// The ffmpeg stub's calls.
    ffmpeg: Vec<String>,
    /// `ferrofin_metadata_provider_requests_total` by provider (all results).
    provider_metric: BTreeMap<String, f64>,
    /// The provider mock's request lines.
    requests: Vec<String>,
    /// Rows written per table.
    writes: BTreeMap<String, i64>,
    /// Item-attributed rows written: (table, operation, item key).
    item_writes: Vec<(String, String, String)>,
}

impl Seen {
    /// Items with `outcome`, over every trigger.
    fn outcome(&self, outcome: &str) -> f64 {
        self.items
            .iter()
            .filter(|((_, o), _)| o == outcome)
            .map(|(_, v)| v)
            .sum()
    }

    /// Items with `outcome` under `trigger`.
    fn item(&self, trigger: &str, outcome: &str) -> f64 {
        self.items
            .get(&(trigger.to_owned(), outcome.to_owned()))
            .copied()
            .unwrap_or(0.0)
    }

    fn provider_total(&self) -> f64 {
        self.provider_metric.values().sum()
    }

    /// The item-data tables written (everything but [`REQUEST_TABLES`]).
    fn item_tables(&self) -> BTreeMap<&str, i64> {
        self.writes
            .iter()
            .filter(|(t, _)| !REQUEST_TABLES.contains(&t.as_str()))
            .map(|(t, n)| (t.as_str(), *n))
            .collect()
    }

    /// The distinct items any row was written for — its own `BaseItems` row
    /// or a row keyed by its `ItemId` — by key ([`Writes::key`]).
    fn written_items(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.item_writes.iter().map(|(_, _, k)| k.clone()).collect();
        keys.sort_unstable();
        keys.dedup();
        keys
    }

    /// Whether any probe's argument line names `file`.
    fn probed(&self, file: &str) -> bool {
        self.spawns.iter().any(|s| s.contains(file))
    }

    /// The row's one-line summary, printed for the report.
    fn summary(&self, row: &str) -> String {
        format!(
            "row {row:>3}: scans {:?} items created={} updated={} unchanged={} removed={} | \
             probes metric={} spawns={} ffmpeg={} | provider metric={} mock={} {:?} | items written {:?} tables {:?}",
            self.scans,
            self.outcome("created") + 0.0,
            self.outcome("updated") + 0.0,
            self.outcome("unchanged") + 0.0,
            self.outcome("removed") + 0.0,
            self.probes.get("ok").copied().unwrap_or(0.0),
            self.spawns.len(),
            self.ffmpeg.len(),
            self.provider_total() + 0.0,
            self.requests.len(),
            self.requests
                .iter()
                .map(|r| {
                    let target = r.split_whitespace().nth(1).unwrap_or_default();
                    target.split_once('?').map_or(target, |(path, _)| path)
                })
                .collect::<Vec<_>>(),
            self.written_items(),
            self.item_tables(),
        )
    }
}

/// A snapshot taken before a row's action.
struct Mark {
    scrape: Scrape,
    spawns: usize,
    ffmpeg: usize,
    requests: usize,
}

struct Harness {
    client: reqwest::Client,
    base: String,
    token: String,
    media: Media,
    stubs: Stubs,
    providers: Providers,
    writes: Writes,
    log_dir: PathBuf,
    /// Library name → CollectionFolder id.
    libraries: BTreeMap<String, String>,
    server: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
    _temp: tempfile::TempDir,
}

impl Harness {
    async fn boot() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let media = Media::generate(&temp.path().join("media"));
        let stubs = Stubs::write(temp.path());
        let providers = Providers::spawn();
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("client");
        let make_config = |port: u16| Config {
            server_name: "ferrofin-scan-matrix".to_owned(),
            port,
            enable_metrics: Some(true),
            ffmpeg_path: Some(stubs.ffmpeg.clone()),
            ffprobe_path: Some(stubs.ffprobe.clone()),
            musicbrainz_base_url: format!("{}/mb", providers.base),
            studios_repo_url: format!("{}/studios", providers.base),
            provider_endpoints: ProviderEndpoints {
                tmdb: Some(format!("{}/tmdb", providers.base)),
                tmdb_images: Some(format!("{}/image", providers.base)),
                omdb: Some(format!("{}/omdb/", providers.base)),
                opensubtitles: Some(format!("{}/opensubtitles", providers.base)),
                tvdb: Some(format!("{}/tvdb", providers.base)),
                fanart: Some(format!("{}/fanart", providers.base)),
                audiodb: Some(format!("{}/audiodb", providers.base)),
                lrclib: Some(format!("{}/lrclib", providers.base)),
            },
            // A passwordless administrator: no PBKDF2 round in a debug build.
            admin_password: String::new(),
            ..Config::test_stub(temp.path())
        };
        let (server, base, config) = Self::spawn(&client, make_config).await;
        let token = Self::login(&client, &base).await;
        let writes = Writes::install(&config.database_url(), &media.root).await;
        let mut harness = Self {
            client,
            base,
            token,
            media,
            stubs,
            providers,
            writes,
            log_dir: config.data_dir.join("log"),
            libraries: BTreeMap::new(),
            server: Some(server),
            _temp: temp,
        };
        harness.configure().await;
        harness
    }

    /// Starts [`ferrofin_server::run`] on a free port (another if a
    /// concurrent test took it first) and waits until it answers.
    async fn spawn(
        client: &reqwest::Client,
        make_config: impl Fn(u16) -> Config,
    ) -> (std::thread::JoinHandle<anyhow::Result<()>>, String, Config) {
        for _ in 0..5 {
            let config = make_config(free_port());
            let base = format!("http://127.0.0.1:{}", config.port);
            let run_config = config.clone();
            // `run`'s future is not `Send` enough for `tokio::spawn`; the
            // binary drives it from `main`, so give it its own runtime too.
            let server = std::thread::spawn(move || {
                tokio::runtime::Runtime::new()
                    .expect("server runtime")
                    .block_on(ferrofin_server::run(run_config))
            });
            let deadline = Instant::now() + STARTUP_DEADLINE;
            while Instant::now() < deadline {
                let name = async {
                    let info: Value = client
                        .get(format!("{base}/System/Info/Public"))
                        .send()
                        .await
                        .ok()?
                        .json()
                        .await
                        .ok()?;
                    info["ServerName"].as_str().map(str::to_owned)
                };
                if name.await.as_deref() == Some(config.server_name.as_str()) {
                    return (server, base, config);
                }
                if server.is_finished() {
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
            assert!(
                server.is_finished(),
                "server never became reachable on {base}"
            );
        }
        panic!("no free port survived long enough to bind");
    }

    async fn login(client: &reqwest::Client, base: &str) -> String {
        let body: Value = client
            .post(format!("{base}/Users/AuthenticateByName"))
            .header("Authorization", CLIENT)
            .json(&json!({"Username": "admin", "Pw": ""}))
            .send()
            .await
            .expect("auth request")
            .json()
            .await
            .expect("auth json");
        body["AccessToken"]
            .as_str()
            .unwrap_or_else(|| panic!("no access token: {body}"))
            .to_owned()
    }

    fn auth(&self) -> String {
        format!("{CLIENT}, Token=\"{}\"", self.token)
    }

    async fn get(&self, path: &str) -> Value {
        let response = self
            .client
            .get(format!("{}{path}", self.base))
            .header("Authorization", self.auth())
            .send()
            .await
            .expect("GET");
        assert!(
            response.status().is_success(),
            "GET {path}: {}",
            response.status()
        );
        response.json().await.expect("json")
    }

    async fn post(&self, path: &str, body: Option<&Value>) {
        let mut request = self
            .client
            .post(format!("{}{path}", self.base))
            .header("Authorization", self.auth());
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.expect("POST");
        let status = response.status();
        assert!(
            status.is_success(),
            "POST {path}: {status} {}",
            response.text().await.unwrap_or_default()
        );
    }

    async fn scrape(&self) -> Scrape {
        let response = self
            .client
            .get(format!("{}/metrics", self.base))
            .send()
            .await
            .expect("metrics");
        assert_eq!(response.status(), 200, "/metrics is enabled");
        Scrape::parse(&response.text().await.expect("metrics text"))
    }

    /// The settle window, MusicBrainz's pace, and the libraries: movies, a
    /// series, a watched series and music, each with only the remote
    /// fetchers the stand-in answers for ticked — TMDb's image fetcher for
    /// movies, every other image fetcher off. The series library runs
    /// TheTVDB after TheMovieDb for series and episodes, so the first scan
    /// shows every provider answering, one after the other.
    async fn configure(&mut self) {
        let mut config = self.get("/System/Configuration").await;
        config["LibraryMonitorDelay"] = json!(SETTLE_SECONDS);
        self.post("/System/Configuration", Some(&config)).await;
        // The MusicBrainz settings page's `RateLimit` (a mirror may go
        // below musicbrainz.org's 1 s; the mock is one).
        let musicbrainz = "8c95c4d2e50c4fb0a4f36c06ff0f9a1a";
        self.post(
            &format!("/Plugins/{musicbrainz}/Configuration"),
            Some(&json!({"RateLimit": MUSICBRAINZ_RATE_LIMIT})),
        )
        .await;

        let fetchers = |types: &[&str], names: &[&str], images: &[&str]| -> Value {
            types
                .iter()
                .map(|t| {
                    json!({"Type": t, "MetadataFetchers": names, "MetadataFetcherOrder": names,
                           "ImageFetchers": images, "ImageFetcherOrder": images})
                })
                .collect()
        };
        let tv = ["Series", "Season", "Episode"];
        let libraries = [
            (
                "Movies",
                "movies",
                "movies",
                false,
                fetchers(&["Movie"], &["TheMovieDb"], &["TheMovieDb"]),
            ),
            (
                "Shows",
                "tvshows",
                "shows",
                false,
                [
                    fetchers(&["Series", "Episode"], &["TheMovieDb", "TheTVDB"], &[]),
                    fetchers(&["Season"], &["TheMovieDb"], &[]),
                ]
                .into_iter()
                .flat_map(|v| v.as_array().cloned().unwrap_or_default())
                .collect(),
            ),
            (
                "Live",
                "tvshows",
                "live",
                true,
                fetchers(&tv, &["TheMovieDb"], &[]),
            ),
            (
                "Music",
                "music",
                "music",
                false,
                fetchers(
                    &["MusicArtist", "MusicAlbum", "Audio"],
                    &["MusicBrainz", "TheAudioDB"],
                    &[],
                ),
            ),
        ];
        for (name, kind, dir, realtime, type_options) in libraries {
            let body = json!({"LibraryOptions": {
                "PathInfos": [{"Path": self.media.library(dir)}],
                "EnableRealtimeMonitor": realtime,
                "TypeOptions": type_options,
            }});
            self.post(
                &format!(
                    "/Library/VirtualFolders?name={name}&collectionType={kind}&refreshLibrary=false"
                ),
                Some(&body),
            )
            .await;
        }
        let folders = self.get("/Library/VirtualFolders").await;
        for folder in folders.as_array().expect("folders") {
            self.libraries.insert(
                folder["Name"].as_str().expect("name").to_owned(),
                folder["ItemId"].as_str().expect("id").to_owned(),
            );
        }
        assert_eq!(self.libraries.len(), 4, "{folders}");
        self.await_watch(&self.media.library("live")).await;
        // The libraries' own rows were written; the rows start from here.
        self.writes.reset().await;
    }

    /// Waits until the watcher logs that it watches `root` (the Live
    /// library, rows 5 and 6), failing at once when it logs that it could
    /// not (inotify limits) — rather than timing out in row 5.
    async fn await_watch(&self, root: &str) {
        let deadline = Instant::now() + STARTUP_DEADLINE;
        loop {
            let log = self.log();
            let about_root =
                |needle: &str| log.lines().any(|l| l.contains(needle) && l.contains(root));
            assert!(
                !about_root("failed to watch library root")
                    && !log.contains("filesystem watcher unavailable"),
                "rows 5 and 6 need the inotify watcher on {root}, which the server could not start"
            );
            if about_root("watching library root for changes") {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the watcher never reported watching {root}"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    fn library(&self, name: &str) -> &str {
        self.libraries.get(name).expect("library")
    }

    /// Takes the "before" snapshot of a row. The write counters were
    /// cleared when the last row's were taken; anything since is a write no
    /// row accounts for.
    async fn mark(&self) -> Mark {
        self.writes.assert_quiet("before the row").await;
        self.writes.reset().await;
        Mark {
            scrape: self.scrape().await,
            spawns: self.stubs.calls().len(),
            ffmpeg: self.stubs.ffmpeg_calls().len(),
            requests: self.providers.all().len(),
        }
    }

    /// Waits until the scans `expect`ed (trigger → passes) have finished and
    /// the queue is idle, then reports what happened since `mark`.
    async fn settle(&self, mark: Mark, expect: &[(&str, f64)]) -> Seen {
        let deadline = Instant::now() + SCAN_DEADLINE;
        let now = loop {
            let now = self.scrape().await;
            let done = expect.iter().all(|(trigger, n)| {
                now.sum(SCANS, &[("trigger", trigger)])
                    - mark.scrape.sum(SCANS, &[("trigger", trigger)])
                    >= *n
            });
            if done && now.sum("ferrofin_library_scan_in_progress", &[]) == 0.0 {
                break now;
            }
            assert!(
                Instant::now() < deadline,
                "scans {expect:?} did not finish; scans now {:?}",
                now.by(SCANS, "trigger")
            );
            tokio::time::sleep(POLL).await;
        };
        let delta = |name: &str, labels: &[(&str, &str)]| {
            now.sum(name, labels) - mark.scrape.sum(name, labels)
        };
        let mut seen = Seen::default();
        for trigger in ["api", "schedule", "startup", "watcher", "webhook"] {
            let completed = delta(SCANS, &[("trigger", trigger), ("result", "completed")]);
            let all = delta(SCANS, &[("trigger", trigger)]);
            if completed > 0.0 {
                seen.scans.insert(trigger.to_owned(), completed);
            }
            if all > completed {
                seen.unfinished.insert(trigger.to_owned(), all - completed);
            }
            for outcome in ["created", "updated", "unchanged", "removed"] {
                let n = delta(ITEMS, &[("trigger", trigger), ("outcome", outcome)]);
                if n > 0.0 {
                    seen.items
                        .insert((trigger.to_owned(), outcome.to_owned()), n);
                }
            }
        }
        for result in ["ok", "failed", "cancelled"] {
            let n = delta(PROBES, &[("result", result)]);
            if n > 0.0 {
                seen.probes.insert(result.to_owned(), n);
            }
        }
        for (provider, n) in now.by(PROVIDER_REQUESTS, "provider") {
            let n = n - mark
                .scrape
                .sum(PROVIDER_REQUESTS, &[("provider", &provider)]);
            if n > 0.0 {
                seen.provider_metric.insert(provider, n);
            }
        }
        seen.spawns = self.stubs.calls().split_off(mark.spawns);
        seen.ffmpeg = self.stubs.ffmpeg_calls().split_off(mark.ffmpeg);
        seen.requests = self.providers.all().split_off(mark.requests);
        (seen.writes, seen.item_writes) = self.writes.take().await;
        self.writes.reset().await;
        assert!(
            seen.unfinished.is_empty(),
            "a scan did not complete: {seen:?}"
        );
        seen
    }

    /// Queues `POST /Library/Refresh` (every library, the dashboard's "Scan
    /// All Libraries") and waits for it.
    async fn rescan(&self, row: &str) -> Seen {
        let mark = self.mark().await;
        self.post("/Library/Refresh", None).await;
        let seen = self.settle(mark, &[("api", 1.0)]).await;
        eprintln!("{}", seen.summary(row));
        seen
    }

    /// `POST /Items/{id}/Refresh` with the dashboard's refresh-dialog query.
    async fn refresh(&self, id: &str, mode: &str, replace_all: bool) {
        self.post(
            &format!(
                "/Items/{id}/Refresh?MetadataRefreshMode={mode}&ImageRefreshMode={mode}\
                 &ReplaceAllMetadata={replace_all}&ReplaceAllImages=false&RegenerateTrickplay=false"
            ),
            None,
        )
        .await;
    }

    /// Every item under the libraries, by path.
    async fn items(&self) -> BTreeMap<String, Value> {
        let all = self
            .get("/Items?Recursive=true&Fields=Path,Overview,LockData,LockedFields,RemoteTrailers,ProviderIds")
            .await;
        all["Items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|i| Some((i["Path"].as_str()?.to_owned(), i.clone())))
            .collect()
    }

    /// The id of the item at `path`.
    async fn id(&self, path: &Path) -> String {
        let items = self.items().await;
        items
            .get(path.to_string_lossy().as_ref())
            .and_then(|i| i["Id"].as_str())
            .unwrap_or_else(|| panic!("no item at {}: {:?}", path.display(), items.keys()))
            .to_owned()
    }

    /// `GET /Items/{id}` — the full DTO the metadata editor edits.
    async fn item(&self, id: &str) -> Value {
        self.get(&format!("/Items/{id}")).await
    }

    /// The metadata editor's save: the item as `GET` returned it, with
    /// `edit` applied.
    async fn edit(&self, id: &str, edit: impl FnOnce(&mut Value)) {
        let mut dto = self.item(id).await;
        edit(&mut dto);
        self.writes.assert_quiet("before the edit").await;
        self.post(&format!("/Items/{id}"), Some(&dto)).await;
        self.writes.reset().await;
    }

    /// Reports `path` through `POST /Library/Media/Updated`, the *arr
    /// webhook.
    async fn webhook(&self, path: &Path, update: &str) {
        self.post(
            "/Library/Media/Updated",
            Some(&json!({"Updates": [{"Path": path.to_string_lossy(), "UpdateType": update}]})),
        )
        .await;
    }

    /// Every line of the server's log files.
    fn log(&self) -> String {
        let mut text = String::new();
        if let Ok(entries) = std::fs::read_dir(&self.log_dir) {
            let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            files.sort();
            for file in files {
                text.push_str(&std::fs::read_to_string(file).unwrap_or_default());
            }
        }
        text
    }

    /// The `library_scan_pass` span's `scope` on every "library scan pass
    /// complete" line, oldest first — once the log has caught up with
    /// `/metrics` (one such line per completed pass; the file writer is a
    /// FIFO, so the last pass's line being there means all are).
    async fn pass_scopes(&self) -> Vec<String> {
        let completed = self.scrape().await.sum(SCANS, &[("result", "completed")]);
        let deadline = Instant::now() + SCAN_DEADLINE;
        loop {
            let scopes: Vec<String> = self
                .log()
                .lines()
                .filter(|l| l.contains("library scan pass complete"))
                .filter_map(|l| {
                    let rest = &l[l.find("scope=")? + "scope=".len()..];
                    let end = rest.find([' ', '}', ':']).unwrap_or(rest.len());
                    Some(rest[..end].trim_matches('"').to_owned())
                })
                .collect();
            if count(scopes.len()) >= completed {
                return scopes;
            }
            assert!(
                Instant::now() < deadline,
                "the log has {} completed passes, /metrics {completed}",
                scopes.len()
            );
            tokio::time::sleep(POLL).await;
        }
    }

    /// Waits until the log has `needle` more often than `before` times.
    async fn await_log(&self, needle: &str, before: usize) {
        let deadline = Instant::now() + SCAN_DEADLINE;
        while self.log().matches(needle).count() <= before {
            assert!(Instant::now() < deadline, "the log never showed {needle:?}");
            tokio::time::sleep(POLL).await;
        }
    }

    async fn shutdown(mut self) {
        self.post("/System/Shutdown", None).await;
        if let Some(server) = self.server.take() {
            tokio::time::timeout(
                Duration::from_mins(1),
                tokio::task::spawn_blocking(move || server.join()),
            )
            .await
            .expect("run returns after shutdown")
            .expect("join task")
            .expect("server thread did not panic")
            .expect("run exits cleanly");
        }
    }
}

/// Asserts no provider was asked anything, on both counts.
fn assert_no_provider_request(seen: &Seen, row: &str) {
    assert_eq!(
        seen.provider_total(),
        0.0,
        "row {row}: provider metric {seen:?}"
    );
    assert!(
        seen.requests.is_empty(),
        "row {row}: provider requests {:?}",
        seen.requests
    );
}

/// Asserts the row's only provider request was TheMovieDb's season lookup
/// `season` (`/tmdb/tv/{series}/season/{n}`) — what a season refreshed for
/// its changed folder asks for in a library that ticks only TheMovieDb for
/// seasons (see the module docs, rows 6 and 13).
fn assert_only_the_season_request(seen: &Seen, season: &str, row: &str) {
    assert_eq!(
        seen.provider_metric,
        BTreeMap::from([("tmdb".to_owned(), 1.0)]),
        "row {row}: provider metric {seen:?}"
    );
    let paths: Vec<&str> = seen
        .requests
        .iter()
        .filter_map(|r| r.split_whitespace().nth(1))
        .map(|target| target.split('?').next().unwrap_or(target))
        .collect();
    assert_eq!(paths, [season], "row {row}: provider requests {seen:?}");
}

/// Asserts the row wrote exactly the item tables in `tables` (every one,
/// and none else), so a rewrite of `Peoples`, `ItemValues`,
/// `LinkedChildren`… can't hide behind the right set of items.
fn assert_tables(seen: &Seen, tables: &[&str], row: &str) {
    let written: Vec<&str> = seen.item_tables().into_keys().collect();
    let mut wanted = tables.to_vec();
    wanted.sort_unstable();
    assert_eq!(
        written, wanted,
        "row {row}: tables written {:?}",
        seen.writes
    );
}

/// Asserts the row wrote no item table outside `allowed`.
fn assert_tables_within(seen: &Seen, allowed: &[&str], row: &str) {
    let stray: Vec<&str> = seen
        .item_tables()
        .into_keys()
        .filter(|t| !allowed.contains(t))
        .collect();
    assert!(
        stray.is_empty(),
        "row {row}: stray tables {stray:?} in {:?}",
        seen.writes
    );
}

/// Asserts no ffmpeg ran (no frame or image extraction).
fn assert_no_ffmpeg(seen: &Seen, row: &str) {
    assert!(
        seen.ffmpeg.is_empty(),
        "row {row}: ffmpeg ran {:?}",
        seen.ffmpeg
    );
}

/// Asserts nothing was probed, on both counts.
fn assert_no_probe(seen: &Seen, row: &str) {
    assert!(
        seen.probes.is_empty(),
        "row {row}: probe metric {:?}",
        seen.probes
    );
    assert!(
        seen.spawns.is_empty(),
        "row {row}: ffprobe ran {:?}",
        seen.spawns
    );
}

/// The metric and the mock agree, provider by provider (the metric's
/// label is the mock's path prefix).
fn assert_provider_counts_agree(seen: &Seen, row: &str) {
    for (provider, n) in &seen.provider_metric {
        let prefix = if provider == "musicbrainz" {
            "mb"
        } else {
            provider
        };
        assert_eq!(
            count(of(&seen.requests, prefix).len()),
            *n,
            "row {row}: {provider} metric vs mock {:?}",
            seen.requests
        );
    }
    assert_eq!(
        count(seen.requests.len()),
        seen.provider_total(),
        "row {row}: every mock request is counted {:?} vs {:?}",
        seen.provider_metric,
        seen.requests
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scan_reprocesses_only_what_changed() {
    let h = Harness::boot().await;
    let started = Instant::now();
    rows(&h).await;
    subtitles_are_downloaded_during_scan(&h).await;
    provider_identity_fields_and_artwork(&h).await;
    movie_metadata_survives_provider_outage(&h).await;
    subtitle_probe_failure_is_quiet(&h).await;
    omdb_works_without_an_operator_key(&h).await;
    eprintln!("matrix rows took {:?}", started.elapsed());
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subtitle_downloads_survive_rescans_and_probe_failure() {
    let h = Harness::boot().await;
    subtitles_are_downloaded_during_scan(&h).await;
    std::fs::remove_file(&h.stubs.ffprobe).expect("make ffprobe unavailable");
    subtitle_probe_failure_is_quiet(&h).await;
    h.shutdown().await;
}

#[allow(clippy::too_many_lines)]
async fn rows(h: &Harness) {
    let m = &h.media;
    let alpha_key = m.key(&m.alpha);
    let beta_key = m.key(&m.beta);
    let gamma_key = m.key(&m.gamma_nfo.with_extension("mkv"));

    // ---- 1: the first scan creates everything and asks the providers ----
    let first = h.rescan("1").await;
    // Movies: 3. Harbor: series, season, 2 episodes. Lantern: series,
    // season, 1 episode. Music: artist, album, 2 tracks. And each of the
    // four library locations' `Folder` rows (owner decision D1).
    let all_items = 18.0;
    assert_eq!(first.outcome("created"), all_items, "row 1: {first:?}");
    for outcome in ["updated", "unchanged", "removed"] {
        assert_eq!(first.outcome(outcome), 0.0, "row 1: {outcome} {first:?}");
    }
    let media_files: usize = 3 + 2 + 1 + 2; // movies, Harbor, Lantern, tracks
    assert_eq!(
        first.probes.get("ok").copied(),
        Some(count(media_files)),
        "row 1: one probe per media file {first:?}"
    );
    // Beta's sidecar subtitle is probed too, but not counted on its own.
    assert_eq!(
        first.spawns.len(),
        media_files + 1,
        "row 1: {:?}",
        first.spawns
    );
    for provider in ["tmdb", "tvdb", "musicbrainz", "audiodb", "image"] {
        assert!(
            first.provider_metric.get(provider).copied().unwrap_or(0.0) > 0.0,
            "row 1: {provider} was asked {:?}",
            first.provider_metric
        );
    }
    assert_provider_counts_agree(&first, "1");
    for provider in ["fanart", "studios"] {
        assert!(
            of(&first.requests, provider).is_empty(),
            "row 1: {provider} is off"
        );
    }
    // Every provider the Shows library ticks runs, one after the other
    // (`ExecuteRemoteProviders`): TheTVDB after TheMovieDb, for Harbor and
    // each of its episodes. It resolves Harbor by the TVDB id TMDB's answer
    // carried, never by a search (`MergeNewData`); the Live library, TMDB
    // only, never reaches it.
    let the_tvdb = of(&first.requests, "tvdb");
    assert!(
        the_tvdb
            .iter()
            .any(|r| r.contains(&format!("/tvdb/series/{HARBOR_TVDB}/extended")))
            && the_tvdb
                .iter()
                .any(|r| r.contains("/tvdb/episodes/3001/extended"))
            && the_tvdb
                .iter()
                .any(|r| r.contains("/tvdb/episodes/3002/extended")),
        "row 1: TVDB answered for Harbor and its episodes {the_tvdb:?}"
    );
    assert!(
        the_tvdb.iter().all(|r| !r.contains("/tvdb/search")),
        "row 1: TVDB looked Harbor up by the id TMDB found {the_tvdb:?}"
    );
    // Alpha's artwork and its cast member: the person refreshed from TMDb,
    // and both images downloaded.
    let artwork = artwork_and_people(&first);
    for wanted in [
        "/image/original/alpha-poster.png",
        "/tmdb/person/501",
        "/image/original/ada.png",
    ] {
        assert!(
            artwork.iter().any(|r| r.contains(wanted)),
            "row 1: {wanted} was fetched {artwork:?}"
        );
    }
    // The first answer wins a field and a later one fills what is still
    // empty (`MergeData(result, temp, [], false, false)`): Harbor keeps
    // TMDB's overview, and TheMovieDb credited nobody on it, so TheTVDB's
    // character is its cast.
    let harbor_dir = m.harbor_e1.parent().and_then(Path::parent).expect("series");
    let harbor = h.item(&h.id(harbor_dir).await).await;
    assert_eq!(harbor["Overview"], "About Harbor.", "row 1: {harbor}");
    assert!(
        harbor["People"].as_array().is_some_and(|p| p.len() == 1
            && p[0]["Name"] == "Tess Tvdb"
            && p[0]["Role"] == "Keeper"),
        "row 1: TVDB's cast fills TMDB's empty one {}",
        harbor["People"]
    );
    let harbor_e1 = h.item(&h.id(&m.harbor_e1).await).await;
    assert_eq!(harbor_e1["Name"], "Harbor episode 1", "row 1: TMDB's title");
    assert!(
        harbor_e1["PremiereDate"]
            .as_str()
            .is_some_and(|d| d.starts_with("2010-01-01")),
        "row 1: TMDB's air date, not TVDB's {}",
        harbor_e1["PremiereDate"]
    );
    // The seasons take TheMovieDb's season answer (`TmdbSeasonProvider`:
    // overview, air date, the season's own Tmdb id; no name while the TMDb
    // settings' `ImportSeasonName` is off), from the one season response
    // the season, its episodes and its poster share: one request per season.
    let harbor_season = h
        .item(&h.id(m.harbor_e1.parent().expect("season")).await)
        .await;
    assert_eq!(
        harbor_season["Overview"], "The first season.",
        "row 1: {harbor_season}"
    );
    assert!(
        harbor_season["PremiereDate"]
            .as_str()
            .is_some_and(|d| d.starts_with("2010-01-01")),
        "row 1: the season's air date {}",
        harbor_season["PremiereDate"]
    );
    assert_eq!(
        harbor_season["ProductionYear"], 2010,
        "row 1: {harbor_season}"
    );
    assert_eq!(
        harbor_season["Name"], "Season 1",
        "row 1: the folder's name"
    );
    assert_eq!(
        harbor_season["ProviderIds"]["Tmdb"], "1",
        "row 1: {harbor_season}"
    );
    for series in ["201", "202"] {
        let season = format!("/tmdb/tv/{series}/season/1?");
        assert_eq!(
            first
                .requests
                .iter()
                .filter(|r| r.contains(&season))
                .count(),
            1,
            "row 1: one season request for series {series} {:?}",
            first.requests
        );
    }
    let alpha = h.id(&m.alpha).await;
    let beta = h.id(&m.beta).await;
    let gamma = h.id(&m.gamma_nfo.with_extension("mkv")).await;
    let dto = h.item(&alpha).await;
    assert!(
        dto["ImageTags"]["Primary"].is_string(),
        "row 1: Alpha's poster {}",
        dto["ImageTags"]
    );
    assert!(
        dto["People"]
            .as_array()
            .is_some_and(|p| p.iter().any(|p| p["Name"] == "Ada Actor")),
        "row 1: Alpha's cast {}",
        dto["People"]
    );
    assert!(
        first
            .written_items()
            .contains(&"Person:Ada Actor".to_owned()),
        "row 1: {first:?}"
    );

    // ---- 2: an unchanged rescan does nothing ----
    let quiet = h.rescan("2").await;
    assert_eq!(
        quiet.scans,
        BTreeMap::from([("api".to_owned(), 1.0)]),
        "row 2: {quiet:?}"
    );
    assert_eq!(
        quiet.outcome("unchanged"),
        all_items,
        "row 2: all unchanged {quiet:?}"
    );
    for outcome in ["created", "updated", "removed"] {
        assert_eq!(quiet.outcome(outcome), 0.0, "row 2: {outcome} {quiet:?}");
    }
    assert_no_probe(&quiet, "2");
    assert_no_ffmpeg(&quiet, "2");
    assert_no_provider_request(&quiet, "2");
    assert_no_artwork_or_people(&quiet, "2");
    assert!(
        quiet.item_tables().is_empty(),
        "row 2: no item row written {:?}",
        quiet.writes
    );

    // ---- 3: a touched file is probed, fetched and saved, alone ----
    set_mtime(&m.alpha, 0);
    let touched = h.rescan("3").await;
    assert_eq!(touched.outcome("updated"), 1.0, "row 3: {touched:?}");
    assert_eq!(
        touched.outcome("unchanged"),
        all_items - 1.0,
        "row 3: {touched:?}"
    );
    assert_eq!(
        touched.probes.get("ok").copied(),
        Some(1.0),
        "row 3: {touched:?}"
    );
    assert!(
        touched.probed("Alpha (1999).mkv") && touched.spawns.len() == 1,
        "row 3: {:?}",
        touched.spawns
    );
    assert_provider_counts_agree(&touched, "3");
    let tmdb = of(&touched.requests, "tmdb");
    assert!(
        !tmdb.is_empty() && tmdb.len() == touched.requests.len(),
        "row 3: the touched movie asks TMDB, and nothing else is asked {:?}",
        touched.requests
    );
    assert!(
        tmdb.iter().all(|r| r.contains("/movie/101")),
        "row 3: only about Alpha {tmdb:?}"
    );
    // Only Alpha — and its cast member: `enrich_people` re-saves the image
    // row of every person a provider pass credits, unchanged or not (an open
    // work item: an unchanged row should not be rewritten).
    assert_only_items(&touched, &[&alpha_key], &["Person:Ada Actor"], "3");
    assert_tables_within(&touched, PROVIDER_PASS_TABLES, "3");

    // ---- 4: an NFO newer than the last save re-reads only local metadata ----
    // Upstream's rule is `nfo mtime − DateLastSaved > 1 min`: an NFO edited
    // more than a minute after the item was last saved. The minute is
    // simulated by moving the item's last save ten minutes back (the only
    // write the test makes outside HTTP), so the rewritten NFO is due now
    // and the save it causes stamps the item current again.
    h.writes.backdate_last_saved("Gamma").await;
    put(&m.gamma_nfo, movie_nfo("Gamma, rewritten.").as_bytes());
    let nfo = h.rescan("4").await;
    assert_eq!(nfo.outcome("updated"), 1.0, "row 4: {nfo:?}");
    assert_eq!(nfo.outcome("unchanged"), all_items - 1.0, "row 4: {nfo:?}");
    assert_no_probe(&nfo, "4");
    assert_no_provider_request(&nfo, "4");
    assert_eq!(
        nfo.written_items(),
        std::slice::from_ref(&gamma_key),
        "row 4: {nfo:?}"
    );
    assert_tables(&nfo, &["BaseItems"], "4");
    assert_eq!(
        h.item(&gamma).await["Overview"],
        "Gamma, rewritten.",
        "row 4"
    );

    // ---- 5 (webhook): a new movie reported by an *arr ----
    let mark = h.mark().await;
    put(&m.delta, &[0u8; 1024]);
    h.webhook(&m.delta, "Created").await;
    let added = h.settle(mark, &[("webhook", 1.0)]).await;
    eprintln!("{}", added.summary("5w"));
    assert_eq!(
        added.scans,
        BTreeMap::from([("webhook".to_owned(), 1.0)]),
        "row 5: {added:?}"
    );
    // A movie's nearest existing item is its library location: the scan
    // validates the reported path, with the location's folder above it as
    // context (owner decision D1) — counted unchanged, nothing else.
    assert_eq!(added.item("webhook", "created"), 1.0, "row 5: {added:?}");
    assert_eq!(added.outcome("unchanged"), 1.0, "row 5: {added:?}");
    for outcome in ["updated", "removed"] {
        assert_eq!(added.outcome(outcome), 0.0, "row 5: {outcome} {added:?}");
    }
    assert_eq!(
        added.probes.get("ok").copied(),
        Some(1.0),
        "row 5: {added:?}"
    );
    assert!(
        added.probed("Delta (1999).mkv") && added.spawns.len() == 1,
        "row 5: {added:?}"
    );
    assert_provider_counts_agree(&added, "5");
    assert!(
        !added.requests.is_empty()
            && added
                .requests
                .iter()
                .all(|r| r.contains("/tmdb/") && (r.contains("Delta") || r.contains("/movie/104"))),
        "row 5: only about Delta {:?}",
        added.requests
    );
    assert!(
        added
            .requests
            .iter()
            .any(|r| r.contains("/tmdb/search/movie") && r.contains("Delta"))
            && added.requests.iter().any(|r| r.contains("/tmdb/movie/104")),
        "row 5: the new movie was looked up and fetched {:?}",
        added.requests
    );
    assert_eq!(added.written_items(), [m.key(&m.delta)], "row 5: {added:?}");
    assert_tables(&added, NEW_FILE_TABLES, "5");

    // ---- 6 (webhook): the movie deleted ----
    let mark = h.mark().await;
    std::fs::remove_file(&m.delta).expect("delete Delta");
    std::fs::remove_dir(m.delta.parent().expect("dir")).expect("delete Delta's folder");
    h.webhook(&m.delta, "Deleted").await;
    let removed = h.settle(mark, &[("webhook", 1.0)]).await;
    eprintln!("{}", removed.summary("6w"));
    assert_eq!(
        removed.scans,
        BTreeMap::from([("webhook".to_owned(), 1.0)]),
        "row 6: {removed:?}"
    );
    assert_eq!(
        removed.item("webhook", "removed"),
        1.0,
        "row 6: {removed:?}"
    );
    // The location's folder above the path is context (owner decision D1).
    assert_eq!(removed.outcome("unchanged"), 1.0, "row 6: {removed:?}");
    for outcome in ["created", "updated"] {
        assert_eq!(
            removed.outcome(outcome),
            0.0,
            "row 6: {outcome} {removed:?}"
        );
    }
    assert_no_probe(&removed, "6");
    assert_no_provider_request(&removed, "6");
    assert_eq!(
        removed.written_items(),
        [m.key(&m.delta)],
        "row 6: {removed:?}"
    );
    assert!(
        removed.item_writes.iter().all(|(_, op, _)| op == "DELETE"),
        "row 6: {removed:?}"
    );
    assert_tables(&removed, NEW_FILE_TABLES, "6");

    // ---- 5 (watcher): a new episode lands in a watched season ----
    let lantern_season = m.key(m.lantern_e2.parent().expect("season"));
    let lantern_series = m.key(
        m.lantern_e2
            .parent()
            .and_then(Path::parent)
            .expect("series"),
    );
    let lantern = h.writes.row("Lantern").await;
    let mark = h.mark().await;
    put(&m.lantern_e2, &[0u8; 1024]);
    let added = h.settle(mark, &[("watcher", 1.0)]).await;
    eprintln!("{}", added.summary("5"));
    assert_eq!(
        added.scans,
        BTreeMap::from([("watcher".to_owned(), 1.0)]),
        "row 5: {added:?}"
    );
    // The episode is created; its season is the nearest existing item and
    // goes through the decision (its folder's mtime moved: D3). Pinned
    // EpisodeResolver does not set IsInMixedFolder when a sibling appears;
    // the existing episode, series and location are otherwise unchanged.
    assert_eq!(added.item("watcher", "created"), 1.0, "row 5: {added:?}");
    assert_eq!(added.item("watcher", "updated"), 1.0, "row 5: {added:?}");
    assert_eq!(added.item("watcher", "unchanged"), 3.0, "row 5: {added:?}");
    assert_eq!(added.outcome("removed"), 0.0, "row 5: {added:?}");
    assert_eq!(
        added.probes.get("ok").copied(),
        Some(1.0),
        "row 5: {added:?}"
    );
    assert!(
        added.probed("Lantern - S01E02.mkv") && added.spawns.len() == 1,
        "row 5: {added:?}"
    );
    assert_provider_counts_agree(&added, "5");
    assert!(
        !added.requests.is_empty()
            && added
                .requests
                .iter()
                .all(|r| r.contains("/tmdb/tv/202/season/1")),
        "row 5: the new episode's season data only, nothing about the series {:?}",
        added.requests
    );
    let mut expected = vec![
        lantern_series.clone(),
        lantern_season.clone(),
        m.key(&m.lantern_e2),
    ];
    expected.sort_unstable();
    assert_eq!(added.written_items(), expected, "row 5: {added:?}");
    assert_tables(&added, NEW_FILE_TABLES, "5");
    assert_only_newest_media_date_moved(&lantern, &h.writes.row("Lantern").await, "5");
    let episode_two = h.id(&m.lantern_e2).await;

    // ---- 6 (watcher): the episode deleted ----
    let lantern = h.writes.row("Lantern").await;
    let mark = h.mark().await;
    std::fs::remove_file(&m.lantern_e2).expect("delete the episode");
    let removed = h.settle(mark, &[("watcher", 1.0)]).await;
    eprintln!("{}", removed.summary("6"));
    assert_eq!(
        removed.scans,
        BTreeMap::from([("watcher".to_owned(), 1.0)]),
        "row 6: {removed:?}"
    );
    assert_eq!(
        removed.item("watcher", "removed"),
        1.0,
        "row 6: {removed:?}"
    );
    // The season goes through the decision again (its folder changed):
    // `requiresRefresh` runs its one ticked season provider, TheMovieDb's,
    // which asks for the season once — nothing else is asked for. The
    // existing sibling keeps its resolver state and is not saved again.
    assert_eq!(
        removed.item("watcher", "updated"),
        1.0,
        "row 6: {removed:?}"
    );
    assert_eq!(removed.outcome("created"), 0.0, "row 6: {removed:?}");
    assert_no_probe(&removed, "6");
    assert_only_the_season_request(&removed, "/tmdb/tv/202/season/1", "6");
    assert_no_artwork_or_people(&removed, "6");
    assert_eq!(removed.written_items(), expected, "row 6: {removed:?}");
    assert_tables(&removed, NEW_FILE_TABLES, "6");
    assert_only_newest_media_date_moved(&lantern, &h.writes.row("Lantern").await, "6");
    assert!(
        !h.items()
            .await
            .values()
            .any(|i| i["Id"] == episode_two.as_str()),
        "row 6: the episode is gone"
    );

    // ---- 7: an editor save without LockData survives a rescan ----
    h.edit(&alpha, |dto| {
        dto["Overview"] = json!("Edited overview.");
        dto["CommunityRating"] = Value::Null;
    })
    .await;
    let edited = h.rescan("7").await;
    // The movies location's own directory changed in row 6 (Delta's folder
    // went), so its folder is saved; nothing else. (Plan step 10, D2, stores
    // a folder's `DateModified` as Jellyfin does — none — and this goes.)
    assert_eq!(
        edited.outcome("unchanged"),
        all_items - 1.0,
        "row 7: {edited:?}"
    );
    assert_eq!(edited.outcome("updated"), 1.0, "row 7: {edited:?}");
    assert_no_probe(&edited, "7");
    assert_no_provider_request(&edited, "7");
    assert_no_artwork_or_people(&edited, "7");
    assert_eq!(
        edited.written_items(),
        [m.key(
            m.delta
                .parent()
                .and_then(std::path::Path::parent)
                .expect("location")
        )],
        "row 7: {edited:?}"
    );
    assert_no_ffmpeg(&edited, "7");
    let dto = h.item(&alpha).await;
    assert_eq!(dto["Overview"], "Edited overview.", "row 7");
    assert_eq!(dto["LockData"], false, "row 7");
    assert!(
        dto["CommunityRating"].is_null(),
        "row 7: {}",
        dto["CommunityRating"]
    );

    let movies = h.library("Movies").to_owned();
    // A Movies refresh processes its three movies and nothing else.
    let movie_files = 3.0;
    let all_movies = {
        let mut keys = vec![alpha_key.clone(), beta_key.clone(), gamma_key.clone()];
        keys.sort_unstable();
        keys
    };

    // ---- 8: "Search for missing metadata" fills gaps, keeps the edit ----
    let mark = h.mark().await;
    h.refresh(&movies, "FullRefresh", false).await;
    let search = h.settle(mark, &[("api", 1.0)]).await;
    eprintln!("{}", search.summary("8"));
    assert_full_pass(&search, movie_files, &all_movies, "8");
    let dto = h.item(&alpha).await;
    assert_eq!(
        dto["Overview"], "Edited overview.",
        "row 8: the edit is kept"
    );
    assert_eq!(dto["CommunityRating"], 8.0, "row 8: the gap is filled");

    // ---- 9: "Replace all metadata" replaces the unlocked overview ----
    let mark = h.mark().await;
    h.refresh(&movies, "FullRefresh", true).await;
    let replace = h.settle(mark, &[("api", 1.0)]).await;
    eprintln!("{}", replace.summary("9"));
    assert_full_pass(&replace, movie_files, &all_movies, "9");
    assert_eq!(
        h.item(&alpha).await["Overview"],
        "About Alpha.",
        "row 9: replaced"
    );

    // ---- 10: a locked field survives "Replace all metadata" ----
    h.edit(&alpha, |dto| {
        dto["Overview"] = json!("Locked overview.");
        dto["CommunityRating"] = json!(1.0);
        dto["LockedFields"] = json!(["Overview"]);
    })
    .await;
    let mark = h.mark().await;
    h.refresh(&movies, "FullRefresh", true).await;
    let locked = h.settle(mark, &[("api", 1.0)]).await;
    eprintln!("{}", locked.summary("10"));
    assert_full_pass(&locked, movie_files, &all_movies, "10");
    let dto = h.item(&alpha).await;
    assert_eq!(
        dto["Overview"], "Locked overview.",
        "row 10: the locked field is kept"
    );
    assert_eq!(
        dto["CommunityRating"], 8.0,
        "row 10: the unlocked one is replaced"
    );
    assert_eq!(dto["LockedFields"], json!(["Overview"]), "row 10");
    assert_eq!(dto["LockData"], false, "row 10");

    // ---- 11: LockData refuses the providers, a local poster still lands ----
    // Beta's file is touched too: unlocked, that alone makes a scan ask TMDb
    // for its metadata and its missing artwork (as Alpha's did in row 3), so
    // the lock is what keeps this rescan from asking anything.
    let trailers = h.item(&beta).await["RemoteTrailers"].clone();
    assert!(
        trailers.as_array().is_some_and(|t| !t.is_empty()),
        "row 12 needs Beta's trailers: {trailers}"
    );
    h.edit(&beta, |dto| dto["LockData"] = json!(true)).await;
    assert!(
        h.item(&beta).await["ImageTags"]["Primary"].is_null(),
        "row 11: no poster yet"
    );
    let poster = m.beta.with_file_name("poster.png");
    put(&poster, &POSTER_PNG);
    set_mtime(&m.beta, 0);
    let locked = h.rescan("11").await;
    assert_eq!(locked.outcome("updated"), 1.0, "row 11: {locked:?}");
    assert_eq!(
        locked.outcome("unchanged"),
        all_items - 1.0,
        "row 11: {locked:?}"
    );
    // The probe is not a provider: a locked item's file is still probed.
    assert_eq!(
        locked.probes.get("ok").copied(),
        Some(1.0),
        "row 11: {locked:?}"
    );
    assert_no_provider_request(&locked, "11");
    assert_eq!(
        locked.written_items(),
        std::slice::from_ref(&beta_key),
        "row 11: {locked:?}"
    );
    // The new poster's image row, and the probe's streams.
    assert_tables(
        &locked,
        &["BaseItemImageInfos", "BaseItems", "MediaStreamInfos"],
        "11",
    );
    let dto = h.item(&beta).await;
    assert!(
        dto["ImageTags"]["Primary"].is_string(),
        "row 11: the poster {}",
        dto["ImageTags"]
    );
    let images = h.get(&format!("/Items/{beta}/Images")).await;
    assert!(
        images.as_array().is_some_and(|i| {
            i.iter().any(|i| {
                i["ImageType"] == "Primary" && i["Path"] == poster.to_string_lossy().as_ref()
            })
        }),
        "row 11: the Primary image is the local poster {images}"
    );
    assert_eq!(dto["LockData"], true, "row 11");

    // ---- 12: a locked item a rescan saves keeps its Data (trailers) ----
    // Two minutes ahead: row 11's touch was moments ago, within the 1 s
    // tolerance of "now".
    set_mtime(&m.beta, 120);
    let saved = h.rescan("12").await;
    assert_eq!(saved.outcome("updated"), 1.0, "row 12: {saved:?}");
    assert_eq!(
        saved.probes.get("ok").copied(),
        Some(1.0),
        "row 12: {saved:?}"
    );
    assert!(saved.probed("Beta (1999).mkv"), "row 12: {saved:?}");
    assert_no_provider_request(&saved, "12");
    assert_eq!(
        saved.written_items(),
        std::slice::from_ref(&beta_key),
        "row 12: {saved:?}"
    );
    assert_tables(&saved, &["BaseItems", "MediaStreamInfos"], "12");
    let dto = h.item(&beta).await;
    assert_eq!(dto["RemoteTrailers"], trailers, "row 12: trailers intact");
    assert_eq!(dto["LockData"], true, "row 12");

    // ---- 13: an episode keeps its TMDB date and rating ----
    let episode = h.id(&m.harbor_e1).await;
    let fields = |dto: &Value| {
        (
            dto["PremiereDate"].clone(),
            dto["ProductionYear"].clone(),
            dto["CommunityRating"].clone(),
        )
    };
    let before = fields(&h.item(&episode).await);
    assert!(
        before
            .0
            .as_str()
            .is_some_and(|d| d.starts_with("2010-01-01")),
        "row 13: {before:?}"
    );
    assert_eq!(before.1, 2010, "row 13: TMDB's year {before:?}");
    assert_eq!(before.2, 7.5, "row 13: TMDB's rating {before:?}");
    let quiet = h.rescan("13").await;
    assert_eq!(quiet.outcome("unchanged"), all_items, "row 13: {quiet:?}");
    assert_no_ffmpeg(&quiet, "13");
    assert!(quiet.item_tables().is_empty(), "row 13: {quiet:?}");
    assert_no_provider_request(&quiet, "13");
    assert_no_artwork_or_people(&quiet, "13");
    assert_eq!(
        fields(&h.item(&episode).await),
        before,
        "row 13: a quiet rescan"
    );
    // A new sidecar re-probes the episode and saves it without asking its
    // providers (only the probe's change monitor fired): the stored date and
    // rating must survive that save. Its season goes through the decision
    // for its changed folder: `requiresRefresh` runs every season provider
    // the library ticks — TheMovieDb's — which asks for the season by the
    // series' recorded Tmdb id, once. The series and the other episode are
    // unchanged and ask nothing.
    put(
        &m.harbor_e1.with_file_name("Harbor - S01E01.en.srt"),
        b"1\n00:00:01,000 --> 00:00:02,000\nHi\n",
    );
    let resaved = h.rescan("13s").await;
    assert_eq!(resaved.outcome("updated"), 2.0, "row 13: {resaved:?}");
    assert_eq!(
        resaved.probes.get("ok").copied(),
        Some(1.0),
        "row 13: {resaved:?}"
    );
    assert!(
        resaved.probed("Harbor - S01E01.mkv"),
        "row 13: re-probed {resaved:?}"
    );
    assert_only_the_season_request(&resaved, "/tmdb/tv/201/season/1", "13");
    assert_no_artwork_or_people(&resaved, "13");
    assert_eq!(
        resaved.written_items(),
        [
            m.key(m.harbor_e1.parent().expect("season")),
            m.key(&m.harbor_e1)
        ],
        "row 13: {resaved:?}"
    );
    assert_tables(&resaved, &["BaseItems", "MediaStreamInfos"], "13");
    assert_eq!(
        fields(&h.item(&episode).await),
        before,
        "row 13: after a save"
    );

    // ---- 14: a pending library refresh + a webhook path: no full scan ----
    let shows = h.library("Shows").to_owned();
    let settled = h
        .log()
        .matches("library changes settled; queueing scan")
        .count();
    set_mtime(&m.harbor_e2, 0);
    std::fs::write(&h.stubs.gate, b"").expect("close the gate");
    let passes_before = h.pass_scopes().await.len();
    let mark = h.mark().await;
    // The running scan: the Shows library, held inside its one probe.
    h.refresh(&shows, "Default", false).await;
    let deadline = Instant::now() + SCAN_DEADLINE;
    while !h.stubs.waiting.exists() {
        assert!(
            Instant::now() < deadline,
            "row 14: the Shows scan never probed"
        );
        tokio::time::sleep(POLL).await;
    }
    // Queued behind it: a Movies library refresh, then a webhook report of
    // a new movie in that library, dispatched from its settle window.
    h.refresh(&movies, "Default", false).await;
    put(&m.epsilon, &[0u8; 1024]);
    h.webhook(&m.epsilon, "Created").await;
    h.await_log("library changes settled; queueing scan", settled)
        .await;
    std::fs::remove_file(&h.stubs.gate).expect("open the gate");
    let queued = h.settle(mark, &[("api", 2.0), ("webhook", 1.0)]).await;
    eprintln!("{}", queued.summary("14"));
    assert_eq!(
        queued.scans,
        BTreeMap::from([("api".to_owned(), 2.0), ("webhook".to_owned(), 1.0)]),
        "row 14: the two library refreshes and the webhook's own pass {queued:?}"
    );
    let scopes = h.pass_scopes().await.split_off(passes_before);
    assert_eq!(
        scopes,
        ["library", "library", "changed"],
        "row 14: the Shows scan, the Movies scan it queued, then the webhook's path; no full scan"
    );
    // The Movies refresh found the new file: the path queued behind it,
    // never joined into a wider scan, then had nothing left to create.
    assert_eq!(queued.item("api", "created"), 1.0, "row 14: {queued:?}");
    assert_eq!(queued.item("webhook", "created"), 0.0, "row 14: {queued:?}");
    // The movie, with its location's folder as context (owner decision D1).
    assert_eq!(
        queued.item("webhook", "unchanged"),
        2.0,
        "row 14: {queued:?}"
    );
    // Shows (series, season, 2 episodes; the touched one updated; the
    // location's folder) and Movies (4 with the new one; the location's
    // folder, saved — its directory gained the new movie's: plan step 10,
    // D2, makes that quiet): 10 items, not the 19 of every library.
    assert_eq!(queued.item("api", "updated"), 2.0, "row 14: {queued:?}");
    assert_eq!(queued.item("api", "unchanged"), 7.0, "row 14: {queued:?}");
    assert_eq!(queued.outcome("removed"), 0.0, "row 14: {queued:?}");
    assert!(
        queued.probed("Harbor - S01E02.mkv") && queued.probed("Epsilon (1999).mkv"),
        "row 14: {queued:?}"
    );
    assert_eq!(
        queued.probes.get("ok").copied(),
        Some(2.0),
        "row 14: {queued:?}"
    );
    assert_provider_counts_agree(&queued, "14");
    assert_tables_within(&queued, NEW_FILE_TABLES, "14");
    assert!(
        h.items()
            .await
            .contains_key(m.epsilon.to_string_lossy().as_ref()),
        "row 14: the new movie is browsable"
    );
}

/// Issue #23: a full movie-library refresh must preserve existing metadata
/// when the provider is throttled, including when ffprobe cannot run. Explicit
/// image removal still follows Jellyfin. The library was populated above; this
/// tests the refresh, not database adoption.
async fn movie_metadata_survives_provider_outage(h: &Harness) {
    let alpha = h.id(&h.media.alpha).await;
    // Remove the field lock from row 10 so it cannot conceal data loss.
    h.edit(&alpha, |dto| {
        dto["LockedFields"] = json!([]);
        dto["LockData"] = json!(false);
    })
    .await;
    let before = h.item(&alpha).await;
    assert!(!before["People"].as_array().expect("cast").is_empty());
    assert!(before["ImageTags"]["Primary"].is_string());
    let name = before["Name"].as_str().expect("movie name");
    let stored = h.writes.row(name).await;
    assert_ne!(stored["DateLastRefreshed"], "NULL");

    // A fresh movie still gets remote metadata when probing fails.
    std::fs::remove_file(&h.stubs.ffprobe).expect("make ffprobe unavailable");
    let mark = h.mark().await;
    put(&h.media.delta, &[0u8; 1024]);
    h.webhook(&h.media.delta, "Created").await;
    let added = h.settle(mark, &[("webhook", 1.0)]).await;
    eprintln!("{}", added.summary("15a"));
    assert_eq!(added.outcome("created"), 1.0);
    assert!(added.probes.get("failed").copied().unwrap_or_default() > 0.0);
    let delta = h.id(&h.media.delta).await;
    let populated = h.item(&delta).await;
    assert_eq!(populated["Overview"], "About Delta.");
    assert_eq!(populated["ProviderIds"]["Tmdb"], "104");

    h.providers.tmdb_unavailable.store(true, Ordering::Relaxed);
    let mark = h.mark().await;
    h.refresh(h.library("Movies"), "FullRefresh", true).await;
    let seen = h.settle(mark, &[("api", 1.0)]).await;
    eprintln!("{}", seen.summary("15"));
    assert_eq!(of(&seen.requests, "tmdb").len(), 1, "shared cooldown");
    assert!(seen.probes.get("failed").copied().unwrap_or_default() > 0.0);
    let after = h.item(&alpha).await;
    for field in [
        "Name",
        "Overview",
        "CommunityRating",
        "ProductionYear",
        "PremiereDate",
        "RemoteTrailers",
        "ProviderIds",
        "People",
        "ImageTags",
        "RunTimeTicks",
    ] {
        assert_eq!(after[field], before[field], "outage must preserve {field}");
    }
    assert_eq!(
        h.writes.row(name).await["DateLastRefreshed"],
        stored["DateLastRefreshed"],
        "failed refresh must not be stamped successful"
    );
    let tag = after["ImageTags"]["Primary"].as_str().expect("poster tag");
    let image = h
        .client
        .get(format!("{}/Items/{alpha}/Images/Primary?tag={tag}", h.base))
        .header("X-Emby-Token", &h.token)
        .send()
        .await
        .expect("poster request");
    assert_eq!(image.status(), reqwest::StatusCode::OK);
    assert!(!image.bytes().await.expect("poster body").is_empty());

    // Jellyfin's ItemRefreshController sets RemoveOldMetadata when replacing
    // metadata. Combined with ReplaceAllImages, MetadataService calls
    // RemoveImages BEFORE fetching, so an outage leaves the cached poster gone.
    let mark = h.mark().await;
    h.post(
        &format!(
            "/Items/{}/Refresh?MetadataRefreshMode=FullRefresh&ImageRefreshMode=FullRefresh\
             &ReplaceAllMetadata=true&ReplaceAllImages=true&RegenerateTrickplay=false",
            h.library("Movies")
        ),
        None,
    )
    .await;
    let removed = h.settle(mark, &[("api", 1.0)]).await;
    eprintln!("{}", removed.summary("15b"));
    assert!(
        of(&removed.requests, "tmdb").is_empty(),
        "still cooling down"
    );
    let after_removal = h.item(&alpha).await;
    assert!(after_removal["ImageTags"]["Primary"].is_null());
    assert_eq!(after_removal["Overview"], before["Overview"]);
    assert_eq!(after_removal["People"], before["People"]);
}

/// Scan-time subtitle downloads: existing sidecars, quiet rescans, explicit
/// refreshes, deletion, disabled providers, and provider outages over real HTTP.
#[allow(clippy::too_many_lines)]
async fn subtitles_are_downloaded_during_scan(h: &Harness) {
    assert!(of(&h.providers.all(), "opensubtitles").is_empty());
    h.post(
        "/Plugins/4a3f8e216c944d17a2b80f5e9c3d7a10/Configuration",
        Some(&json!({"Username":"test", "Password":"test"})),
    )
    .await;
    let root = PathBuf::from(h.media.library("subtitles"));
    let movie = root.join("Subtitle Film (2020).mkv");
    put(&movie, &vec![0u8; 131_072]);
    let french = movie.with_extension("fra.srt");
    put(&french, b"existing French subtitle");
    let mut options = json!({
        "PathInfos":[{"Path":root}], "EnableRealtimeMonitor":false,
        "SubtitleDownloadLanguages":["eng", "fra", "ENG"],
        "RequirePerfectSubtitleMatch":true,
        "TypeOptions":[{"Type":"Movie", "MetadataFetchers":[], "ImageFetchers":[]}]
    });
    h.post(
        "/Library/VirtualFolders?name=Subtitles&collectionType=movies&refreshLibrary=false",
        Some(&json!({"LibraryOptions":options})),
    )
    .await;
    let folders = h.get("/Library/VirtualFolders").await;
    let library = folders
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["Name"] == "Subtitles")
        .unwrap()["ItemId"]
        .as_str()
        .unwrap();
    h.writes.reset().await;
    let mark = h.mark().await;
    h.refresh(library, "Default", false).await;
    let first = h.settle(mark, &[("api", 1.0)]).await;
    assert_eq!(
        of(&first.requests, "opensubtitles").len(),
        3,
        "search + login + download: {:?}",
        first.requests
    );
    assert!(first.requests.iter().any(|r| r.contains("languages=en")));
    assert!(!first.requests.iter().any(|r| r.contains("languages=fr")));
    let english = movie.with_extension("eng.srt");
    let content = std::fs::read(&english).expect("subtitle downloaded before scan completes");
    assert_eq!(std::fs::read(&french).unwrap(), b"existing French subtitle");
    let id = h.id(&movie).await;
    let streams = h.get(&format!("/Items/{id}?Fields=MediaStreams")).await;
    assert_eq!(
        streams["MediaStreams"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["Type"] == "Subtitle")
            .count(),
        2
    );

    for mode in ["Default", "FullRefresh"] {
        let mark = h.mark().await;
        h.refresh(library, mode, mode == "FullRefresh").await;
        let seen = h.settle(mark, &[("api", 1.0)]).await;
        assert!(of(&seen.requests, "opensubtitles").is_empty());
        assert_eq!(std::fs::read(&english).unwrap(), content);
        if mode == "Default" {
            assert!(
                seen.spawns.is_empty(),
                "unchanged scan must not probe: {seen:?}"
            );
            assert!(!seen.writes.contains_key("MediaStreamInfos"));
        }
    }
    // A removed sidecar is a file change: download just that language again.
    std::fs::remove_file(&english).unwrap();
    let mark = h.mark().await;
    h.refresh(library, "Default", false).await;
    let repaired = h.settle(mark, &[("api", 1.0)]).await;
    assert_eq!(of(&repaired.requests, "opensubtitles").len(), 3);
    assert_eq!(std::fs::read(&english).unwrap(), content);

    options["DisabledSubtitleFetchers"] = json!(["opensubtitles"]);
    options["SubtitleDownloadLanguages"] = json!(["spa"]);
    h.post(
        "/Library/VirtualFolders/LibraryOptions",
        Some(&json!({"Id":library,"LibraryOptions":options})),
    )
    .await;
    h.writes.reset().await;
    let mark = h.mark().await;
    h.refresh(library, "FullRefresh", false).await;
    let disabled = h.settle(mark, &[("api", 1.0)]).await;
    assert!(of(&disabled.requests, "opensubtitles").is_empty());

    options["DisabledSubtitleFetchers"] = json!([]);
    h.post(
        "/Library/VirtualFolders/LibraryOptions",
        Some(&json!({"Id":library,"LibraryOptions":options})),
    )
    .await;
    h.providers
        .subtitles_unavailable
        .store(true, Ordering::Relaxed);
    h.writes.reset().await;
    let mark = h.mark().await;
    h.refresh(library, "FullRefresh", false).await;
    let outage = h.settle(mark, &[("api", 1.0)]).await;
    assert_eq!(of(&outage.requests, "opensubtitles").len(), 1);
    assert!(
        outage.unfinished.is_empty(),
        "subtitle failure must not abort scan"
    );
    assert_eq!(std::fs::read(&english).unwrap(), content);
    assert!(!movie.with_extension("spa.srt").exists());
    h.providers
        .subtitles_unavailable
        .store(false, Ordering::Relaxed);
    // Failure does not cause every unchanged scan to retry the provider.
    let mark = h.mark().await;
    h.refresh(library, "Default", false).await;
    let quiet = h.settle(mark, &[("api", 1.0)]).await;
    assert!(of(&quiet.requests, "opensubtitles").is_empty());
    assert!(quiet.spawns.is_empty());
    for mode in ["ValidationOnly", "None"] {
        set_mtime(&movie, 5);
        let mark = h.mark().await;
        h.refresh(library, mode, false).await;
        let seen = h.settle(mark, &[("api", 1.0)]).await;
        assert!(
            of(&seen.requests, "opensubtitles").is_empty(),
            "{mode} must not download"
        );
    }
}

/// The outage scenario removed ffprobe. A forced refresh must keep existing
/// subtitles and avoid downloading when it cannot inspect embedded streams.
async fn subtitle_probe_failure_is_quiet(h: &Harness) {
    let movie = PathBuf::from(h.media.library("subtitles")).join("Subtitle Film (2020).mkv");
    let id = h.id(&movie).await;
    let before = std::fs::read(movie.with_extension("eng.srt")).unwrap();
    let mark = h.mark().await;
    h.refresh(&id, "FullRefresh", false).await;
    let seen = h.settle(mark, &[("api", 1.0)]).await;
    assert!(seen.probes.get("failed").copied().unwrap_or_default() > 0.0);
    assert!(of(&seen.requests, "opensubtitles").is_empty());
    assert_eq!(
        std::fs::read(movie.with_extension("eng.srt")).unwrap(),
        before
    );
}

/// The real composition root supplies the shared OMDb key with the operator
/// setting empty. Per-library disabling still prevents requests entirely.
async fn omdb_works_without_an_operator_key(h: &Harness) {
    assert!(
        of(&h.providers.all(), "omdb").is_empty(),
        "disabled in earlier libraries"
    );
    let movie = PathBuf::from(h.media.library("omdb")).join("Shared Key Movie (1999).mkv");
    put(&movie, &[0u8; 1024]);
    let mut options = json!({
        "PathInfos": [{"Path": h.media.library("omdb")}],
        "EnableRealtimeMonitor": false,
        "TypeOptions": [{"Type":"Movie", "MetadataFetchers":["The Open Movie Database"],
                         "ImageFetchers":[]}]
    });
    h.post(
        "/Library/VirtualFolders?name=OMDb&collectionType=movies&refreshLibrary=false",
        Some(&json!({"LibraryOptions": options})),
    )
    .await;
    let folders = h.get("/Library/VirtualFolders").await;
    let library = folders
        .as_array()
        .expect("folders")
        .iter()
        .find(|f| f["Name"] == "OMDb")
        .expect("OMDb library")["ItemId"]
        .as_str()
        .expect("library ID");
    h.writes.reset().await;
    let mark = h.mark().await;
    h.refresh(library, "FullRefresh", false).await;
    let seen = h.settle(mark, &[("api", 1.0)]).await;
    assert_eq!(of(&seen.requests, "omdb").len(), 1);
    assert!(of(&seen.requests, "tmdb").is_empty());
    let id = h.id(&movie).await;
    let dto = h.item(&id).await;
    assert_eq!(dto["Overview"], "Fetched with the shared key.");
    assert_eq!(dto["CriticRating"], 87.0);
    assert_eq!(dto["ProviderIds"]["Imdb"], "tt1234567");

    options["TypeOptions"][0]["MetadataFetchers"] = json!([]);
    h.post(
        "/Library/VirtualFolders/LibraryOptions",
        Some(&json!({
            "Id": library, "LibraryOptions": options
        })),
    )
    .await;
    h.writes.reset().await;
    let mark = h.mark().await;
    h.refresh(library, "FullRefresh", true).await;
    let disabled = h.settle(mark, &[("api", 1.0)]).await;
    assert!(of(&disabled.requests, "omdb").is_empty());
}

/// External identities, newly mapped fields, artwork order and quiet rescans
/// through the real server, its persistence layer and SQLite write triggers.
#[allow(clippy::too_many_lines)]
async fn provider_identity_fields_and_artwork(h: &Harness) {
    let movie_root = h.media.library("provider-movies");
    let movie = PathBuf::from(&movie_root).join("Unmatched filename.mkv");
    put(&movie, &[0; 1024]);
    put(
        &movie.with_extension("nfo"),
        b"<movie><tmdbid>901</tmdbid></movie>",
    );
    let mut options = json!({"PathInfos":[{"Path":movie_root}],"EnableRealtimeMonitor":false,
        "TypeOptions":[{"Type":"Movie","MetadataFetchers":["TheMovieDb"],"ImageFetchers":["TheMovieDb","FanArt"]}]});
    h.post(
        "/Library/VirtualFolders?name=ProviderMovies&collectionType=movies&refreshLibrary=false",
        Some(&json!({"LibraryOptions":options})),
    )
    .await;
    let folders = h.get("/Library/VirtualFolders").await;
    let library = folders
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["Name"] == "ProviderMovies")
        .unwrap()["ItemId"]
        .as_str()
        .unwrap();
    h.writes.reset().await;
    let mark = h.mark().await;
    h.refresh(library, "FullRefresh", false).await;
    let seen = h.settle(mark, &[("api", 1.0)]).await;
    assert!(
        seen.requests
            .iter()
            .any(|r| r.contains(" /image/") && r.contains("mapped-tmdb")),
        "{seen:?}"
    );
    assert!(
        !seen
            .requests
            .iter()
            .any(|r| r.contains("/image/mapped-fanart.png")),
        "{seen:?}"
    );
    let id = h.id(&movie).await;
    let dto = h.item(&id).await;
    assert_eq!(dto["Name"], "Mapped movie");
    assert_eq!(dto["OriginalTitle"], "Original movie");
    assert_eq!(dto["ProductionLocations"], json!(["Japan"]));
    assert_eq!(dto["Tags"], json!(["mapped keyword"]));
    assert_eq!(dto["ProviderIds"]["TmdbCollection"], "99");
    // A plain rescan neither redownloads artwork nor changes image rows.
    h.writes.reset().await;
    let mark = h.mark().await;
    h.refresh(library, "Default", false).await;
    let quiet = h.settle(mark, &[("api", 1.0)]).await;
    assert!(quiet.requests.is_empty(), "{quiet:?}");
    assert!(
        !quiet.writes.contains_key("BaseItemImageInfos"),
        "{quiet:?}"
    );
    // Movie providers default to TMDB before FanArt (pinned intrinsic order).
    // An explicit reversed library order overrides that on replacement.
    options["TypeOptions"][0]["ImageFetcherOrder"] = json!(["FanArt", "TheMovieDb"]);
    h.post(
        "/Library/VirtualFolders/LibraryOptions",
        Some(&json!({"Id":library,"LibraryOptions":options})),
    )
    .await;
    h.writes.reset().await;
    let mark = h.mark().await;
    h.post(&format!("/Items/{id}/Refresh?MetadataRefreshMode=FullRefresh&ImageRefreshMode=FullRefresh&ReplaceAllImages=true"),None).await;
    let reordered = h.settle(mark, &[("api", 1.0)]).await;
    assert!(
        reordered
            .requests
            .iter()
            .any(|r| r.contains("/image/mapped-fanart.png")),
        "{reordered:?}"
    );

    let series_root = h.media.library("provider-series");
    let series = PathBuf::from(&series_root).join("Unmatched series");
    let episode = series.join("Season 1/Unknown.S01E01.mkv");
    put(&episode, &[0; 1024]);
    put(
        &series.join("tvshow.nfo"),
        b"<tvshow><imdbid>tt123456</imdbid></tvshow>",
    );
    let options = json!({"PathInfos":[{"Path":series_root}],"EnableRealtimeMonitor":false,"TypeOptions":[
        {"Type":"Series","MetadataFetchers":["TheTVDB"],"ImageFetchers":["TheTVDB","FanArt"]},
        {"Type":"Season","MetadataFetchers":[],"ImageFetchers":[]},
        {"Type":"Episode","MetadataFetchers":["TheTVDB"],"ImageFetchers":[]}]});
    h.post(
        "/Library/VirtualFolders?name=ProviderSeries&collectionType=tvshows&refreshLibrary=false",
        Some(&json!({"LibraryOptions":options})),
    )
    .await;
    let folders = h.get("/Library/VirtualFolders").await;
    let library = folders
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["Name"] == "ProviderSeries")
        .unwrap()["ItemId"]
        .as_str()
        .unwrap();
    h.writes.reset().await;
    let mark = h.mark().await;
    h.refresh(library, "FullRefresh", false).await;
    let seen = h.settle(mark, &[("api", 1.0)]).await;
    assert!(
        seen.requests
            .iter()
            .any(|r| r.contains("/tvdb/search/remoteid/tt123456")),
        "{seen:?}"
    );
    assert!(
        !seen.requests.iter().any(|r| r.contains("/tvdb/search?")),
        "{seen:?}"
    );
    assert!(
        seen.requests
            .iter()
            .any(|r| r.contains("/image/mapped-series-fanart.png")),
        "{seen:?}"
    );
    let dto = h.item(&h.id(&series).await).await;
    assert_eq!(dto["Name"], "Mapped series");
    assert_eq!(dto["OriginalTitle"], "Mapped series");
    assert_eq!(dto["RunTimeTicks"], 25_200_000_000_i64);
    assert_eq!(dto["ProviderIds"]["Tvdb"], "42");
    assert_eq!(dto["ProviderIds"]["TvdbSlug"], "mapped-series");
    assert_eq!(dto["ProviderIds"]["TvdbCollection"], "9;");
    let dto = h.item(&h.id(&episode).await).await;
    assert_eq!(dto["OriginalTitle"], "Mapped episode");
    assert_eq!(dto["ProviderIds"]["Tvdb"], "43");
    assert_eq!(dto["ProviderIds"]["Imdb"], "tt123457");
    assert_eq!(dto["AirsBeforeEpisodeNumber"], 2);
    assert_eq!(dto["AirsBeforeSeasonNumber"], 1);
    assert_eq!(dto["AirsAfterSeasonNumber"], 0);
}

/// Asserts the row wrote rows for every item in `items` and for nothing
/// outside `items` and `also` (the items it may touch as well).
fn assert_only_items(seen: &Seen, items: &[&str], also: &[&str], row: &str) {
    let written = seen.written_items();
    assert!(
        items.iter().all(|k| written.iter().any(|w| w == k))
            && written
                .iter()
                .all(|w| items.contains(&w.as_str()) || also.contains(&w.as_str())),
        "row {row}: written {written:?}, expected {items:?} (+ at most {also:?})"
    );
}

/// The artwork downloads and person requests among a row's requests.
fn artwork_and_people(seen: &Seen) -> Vec<&String> {
    seen.requests
        .iter()
        .filter(|r| r.contains(" /image/") || r.contains("/tmdb/person/"))
        .collect()
}

/// Asserts no artwork was downloaded and no person refreshed.
fn assert_no_artwork_or_people(seen: &Seen, row: &str) {
    let fetched = artwork_and_people(seen);
    assert!(
        fetched.is_empty(),
        "row {row}: artwork or people fetched again {fetched:?}"
    );
    assert!(
        !seen.provider_metric.contains_key("image"),
        "row {row}: artwork downloads in the metric {:?}",
        seen.provider_metric
    );
}

/// A full pass over the Movies library: every movie re-probed, fetched by
/// its stored TMDB id (no search) and saved, with the location's folder;
/// no other library touched.
fn assert_full_pass(seen: &Seen, movies: f64, keys: &[String], row: &str) {
    assert_eq!(
        seen.scans,
        BTreeMap::from([("api".to_owned(), 1.0)]),
        "row {row}: {seen:?}"
    );
    assert_eq!(seen.outcome("updated"), movies + 1.0, "row {row}: {seen:?}");
    for outcome in ["created", "unchanged", "removed"] {
        assert_eq!(seen.outcome(outcome), 0.0, "row {row}: {outcome} {seen:?}");
    }
    assert_eq!(
        seen.probes.get("ok").copied(),
        Some(movies),
        "row {row}: re-probed {seen:?}"
    );
    assert_provider_counts_agree(seen, row);
    let tmdb = of(&seen.requests, "tmdb");
    assert!(
        tmdb.iter()
            .all(|r| r.contains("/tmdb/movie/") || r.contains("/tmdb/person/")),
        "row {row}: by stored id, never a search {tmdb:?}"
    );
    for id in [101, 102, 103] {
        assert!(
            tmdb.iter()
                .any(|r| r.contains(&format!("/tmdb/movie/{id}"))),
            "row {row}: movie {id} fetched {tmdb:?}"
        );
    }
    assert_tables_within(seen, PROVIDER_PASS_TABLES, row);
    assert!(
        !seen.item_writes.iter().any(|(table, _, item)| {
            table == "BaseItemImageInfos" && item.starts_with("Person:")
        }),
        "row {row}: identical provider results must not rewrite cast images: {:?}",
        seen.item_writes
    );
    let written = seen.written_items();
    assert!(
        keys.iter().all(|k| written.contains(k))
            && written
                .iter()
                .all(|w| w == "movies" || w.starts_with("movies/") || w.contains(':')),
        "row {row}: the three movies, the location (and their people, genres, years) {written:?}"
    );
}

/// The series above a watcher event is context: the only column of its row
/// that may move is `DateLastMediaAdded` (the owner-approved divergence of
/// `ScanTarget::Changed`), with the `DateLastSaved` every row update stamps
/// (`LibraryManager.UpdateItem`).
fn assert_only_newest_media_date_moved(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    row: &str,
) {
    let moved: Vec<&String> = before
        .keys()
        .filter(|column| before.get(*column) != after.get(*column))
        .collect();
    assert!(
        moved
            .iter()
            .all(|c| *c == "DateLastMediaAdded" || *c == "DateLastSaved"),
        "row {row}: the series' columns that moved: {moved:?}"
    );
}
