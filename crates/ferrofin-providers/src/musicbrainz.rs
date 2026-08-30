//! MusicBrainz metadata provider — a port of Jellyfin's core `MusicBrainz`
//! provider (`MediaBrowser.Providers/Plugins/MusicBrainz`, GUID
//! `8c95c4d2-e50c-4fb0-a4f3-6c06ff0f9a1a`).
//!
//! Resolves the `MusicBrainz*` ids for artists and albums from the MB ws/2 web
//! service (`?fmt=json`). The provider prefers ids already on the item (read
//! from embedded tags during the scan) and only queries the API when they are
//! missing — the faithful precedence:
//!
//! - **artist**: embedded `MusicBrainzArtist` id → done; else search by name.
//! - **album**: `MusicBrainzAlbum` (release) → `MusicBrainzReleaseGroup` → search
//!   by `"album" AND arid:{artist-mbid}` (or `AND artist:"{name}"`); backfill the
//!   missing half of the pair via a lookup.
//!
//! MusicBrainz requires **≤1 request/second** and a descriptive `User-Agent`;
//! both are enforced here. Keyless.

use std::time::Duration;

use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// The default MusicBrainz web-service base.
pub const DEFAULT_BASE_URL: &str = "https://musicbrainz.org";
/// The minimum interval between requests to the official server (MB policy),
/// and the `RateLimit` default the settings page ships
/// (`PluginConfiguration.DefaultRateLimit`). The plugin's setter refuses to go
/// below it while musicbrainz.org itself is selected — see
/// [`MusicBrainzConfig::normalized`](crate::plugin_config::MusicBrainzConfig::normalized).
const MIN_INTERVAL: Duration = Duration::from_secs(1);

/// A resolved album identity: the release id and/or its release-group id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlbumIds {
    /// The `MusicBrainzAlbum` (release) id, if resolved.
    pub release_id: Option<String>,
    /// The `MusicBrainzReleaseGroup` id, if resolved.
    pub release_group_id: Option<String>,
}

impl AlbumIds {
    /// Whether at least one id was resolved.
    #[must_use]
    pub fn is_some(&self) -> bool {
        self.release_id.is_some() || self.release_group_id.is_some()
    }
}

// ---- wire DTOs (MB ws/2 JSON) ---------------------------------------------

#[derive(Debug, Deserialize)]
struct ArtistSearch {
    #[serde(default)]
    artists: Vec<Entity>,
}

#[derive(Debug, Deserialize)]
struct Entity {
    id: String,
    /// The matched entity's own name — needed by the artist provider's
    /// `ReplaceArtistName` setting, which renames the item to it.
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReleaseSearch {
    #[serde(default)]
    releases: Vec<Release>,
}

#[derive(Debug, Deserialize)]
struct Release {
    id: String,
    #[serde(rename = "release-group", default)]
    group: Option<Entity>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    date: Option<String>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCreditWire>,
}

/// One `artist-credit` entry on a release: the credited name plus the artist
/// it points at (`inc=artists` / the search's embedded credit).
#[derive(Debug, Deserialize)]
struct ArtistCreditWire {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    artist: Option<ArtistCreditArtist>,
}

#[derive(Debug, Deserialize)]
struct ArtistCreditArtist {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ArtistLookup {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "life-span", default)]
    span: Option<LifeSpan>,
}

/// `/ws/2/artist?query=` with the full per-hit shape (id + name + life-span).
#[derive(Debug, Deserialize)]
struct ArtistSearchFull {
    #[serde(default)]
    artists: Vec<ArtistLookup>,
}

#[derive(Debug, Default, Deserialize)]
struct LifeSpan {
    #[serde(default)]
    begin: Option<String>,
    #[serde(default)]
    end: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReleaseGroupLookup {
    #[serde(default)]
    releases: Vec<Entity>,
}

/// A MusicBrainz date, which may specify only a year or a year and month.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartialDate {
    /// The year.
    pub year: i32,
    /// The month (1 when the source gave none).
    pub month: u32,
    /// The day (1 when the source gave none).
    pub day: u32,
}

impl PartialDate {
    /// The date as a UTC instant at midnight.
    #[must_use]
    pub fn to_utc(self) -> Option<chrono::DateTime<chrono::Utc>> {
        use chrono::TimeZone as _;
        let date = chrono::NaiveDate::from_ymd_opt(self.year, self.month, self.day)?;
        Some(chrono::Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?))
    }
}

/// One release's metadata beyond its ids.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleaseDetails {
    /// The release title.
    pub name: Option<String>,
    /// The release date.
    pub premiere_date: Option<PartialDate>,
    /// The release year.
    pub production_year: Option<i32>,
}

/// One artist credit on a release — the "Identify" result's `Artists` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleaseArtistCredit {
    /// The credited artist name.
    pub name: Option<String>,
    /// The `MusicBrainzArtist` id behind the credit, when MB supplied it.
    pub artist_id: Option<String>,
}

/// One release as a search/lookup hit — the fields
/// `MusicBrainzAlbumProvider.GetReleaseResult` reads off an `IRelease`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleaseHit {
    /// The `MusicBrainzAlbum` (release) id.
    pub id: String,
    /// The release title.
    pub title: Option<String>,
    /// The release date (`Date?.Year` / `NearestDate`).
    pub date: Option<PartialDate>,
    /// The `MusicBrainzReleaseGroup` id, when supplied.
    pub release_group_id: Option<String>,
    /// The artist credits in order; the first is the album artist.
    pub artist_credits: Vec<ReleaseArtistCredit>,
}

/// One artist as a search/lookup hit — the fields
/// `MusicBrainzArtistProvider.GetResultFromResponse` reads off an `IArtist`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtistHit {
    /// The `MusicBrainzArtist` id.
    pub id: String,
    /// The artist name as MusicBrainz spells it.
    pub name: Option<String>,
    /// The life-span begin date (`LifeSpan?.Begin`).
    pub begin: Option<PartialDate>,
}

impl From<Release> for ReleaseHit {
    fn from(r: Release) -> Self {
        Self {
            id: r.id,
            title: non_empty(r.title),
            date: r.date.as_deref().and_then(parse_partial_date),
            release_group_id: r.group.map(|g| g.id),
            artist_credits: r
                .artist_credit
                .into_iter()
                .map(|c| {
                    let (artist_id, artist_name) = c
                        .artist
                        .map_or((None, None), |a| (non_empty(a.id), non_empty(a.name)));
                    ReleaseArtistCredit {
                        // The credited name (`ArtistCredit.Name`), falling back
                        // to the artist's canonical name.
                        name: non_empty(c.name).or(artist_name),
                        artist_id,
                    }
                })
                .collect(),
        }
    }
}

/// One artist's metadata beyond its id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtistDetails {
    /// The artist name as MusicBrainz spells it.
    pub name: Option<String>,
    /// The life-span begin date (a band's formation).
    pub premiere_date: Option<PartialDate>,
    /// The life-span end date (a band's break-up), which the artist NFO saver
    /// writes as `<disbanded>`.
    pub end_date: Option<PartialDate>,
}

/// Parses a MusicBrainz partial date (`YYYY`, `YYYY-MM`, `YYYY-MM-DD`).
fn parse_partial_date(value: &str) -> Option<PartialDate> {
    let mut parts = value.trim().split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month = parts.next().and_then(|m| m.parse().ok()).unwrap_or(1);
    let day = parts.next().and_then(|d| d.parse().ok()).unwrap_or(1);
    (1..=12).contains(&month).then_some(())?;
    (1..=31).contains(&day).then_some(())?;
    Some(PartialDate { year, month, day })
}

/// A trimmed, non-empty string.
fn non_empty(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// An artist search hit: the MusicBrainz id and the name MusicBrainz has for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistNameMatch {
    /// The `MusicBrainzArtist` id.
    pub id: String,
    /// MusicBrainz's own name for the artist, when it published one.
    pub name: Option<String>,
}

/// A MusicBrainz client. Cheap to clone semantics via `Arc` at the call site;
/// serializes requests behind a ≥1s throttle.
pub struct MusicBrainzClient {
    http: reqwest::Client,
    /// The operator's explicit server override (`FERROFIN_MUSICBRAINZ_BASE_URL`
    /// / config `musicbrainz_base_url`). `None` when the operator set nothing,
    /// which is when the dashboard's `Server` governs.
    configured_base_url: Option<String>,
    /// The MusicBrainz plugin's dashboard settings (`Server`, `RateLimit`,
    /// `ReplaceArtistName`).
    plugin: crate::plugin_config::ConfigSource,
    user_agent: String,
    /// Timestamp of the last request, for the rate limit.
    last_request: Mutex<Option<Instant>>,
}

impl std::fmt::Debug for MusicBrainzClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MusicBrainzClient")
            .field("configured_base_url", &self.configured_base_url)
            .field("plugin", &self.plugin)
            .finish_non_exhaustive()
    }
}

impl MusicBrainzClient {
    /// A client against `base_url` (empty → [`DEFAULT_BASE_URL`]); `version` is
    /// stamped into the required descriptive `User-Agent`.
    #[must_use]
    pub fn new(base_url: &str, version: &str) -> Self {
        let base = base_url.trim_end_matches('/');
        Self {
            http: reqwest::Client::new(),
            configured_base_url: (!base.is_empty()).then(|| base.to_owned()),
            plugin: crate::plugin_config::ConfigSource::new(),
            // MB requires contact info; the project URL satisfies the policy.
            user_agent: format!("Ferrofin/{version} ( https://github.com/ )"),
            last_request: Mutex::new(None),
        }
    }

    /// Binds the plugin manager, making the MusicBrainz settings page's
    /// `Server` / `RateLimit` / `ReplaceArtistName` live. Called once by the
    /// composition root.
    pub fn attach_plugin_manager(
        &self,
        plugins: std::sync::Arc<dyn ferrofin_traits::plugins::PluginManager>,
    ) {
        self.plugin.attach(plugins);
    }

    /// The plugin's settings for this call, C#-normalized.
    ///
    /// `MusicBrainzArtistProvider.ReloadConfig` re-points the shared `Query`
    /// at `Configuration.Server` whenever the settings page is saved; reading
    /// per call is the same observable behaviour without an event bus. An
    /// operator who set `FERROFIN_MUSICBRAINZ_BASE_URL` still wins over the
    /// dashboard (an explicit env/file setting is the more specific
    /// instruction, and that knob shipped before this plugin had an identity).
    pub(crate) async fn settings(&self) -> crate::plugin_config::MusicBrainzConfig {
        let mut cfg: crate::plugin_config::MusicBrainzConfig = self
            .plugin
            .load(crate::builtin_plugins::MUSICBRAINZ.id)
            .await;
        if let Some(base) = &self.configured_base_url {
            cfg.server.clone_from(base);
        }
        cfg.normalized()
    }

    /// Blocks until at least `interval` has elapsed since the previous request,
    /// then records now — the ≤1 req/sec throttle, whose period is the
    /// plugin's `RateLimit` (seconds between requests).
    async fn throttle(&self, interval: std::time::Duration) {
        let mut last = self.last_request.lock().await;
        if let Some(prev) = *last
            && let Some(wait) = interval.checked_sub(prev.elapsed())
        {
            tokio::time::sleep(wait).await;
        }
        *last = Some(Instant::now());
    }

    /// `RateLimit` seconds as a `Duration`.
    ///
    /// A negative or non-finite value (only reachable by hand-editing
    /// `config.json` — the settings page writes a number input) would panic
    /// `Duration::from_secs_f64`, so it falls back to [`MIN_INTERVAL`], the
    /// published MusicBrainz policy floor. Falling back to *zero* there would
    /// turn a typo into an unthrottled crawl of a public service.
    fn interval(rate_limit: f64) -> Duration {
        if rate_limit.is_finite() && rate_limit > 0.0 {
            Duration::from_secs_f64(rate_limit)
        } else {
            MIN_INTERVAL
        }
    }

    /// GETs `path?{query}&fmt=json` as an authenticated (User-Agent) MB call,
    /// returning the parsed body or `None` on any failure. Throttled.
    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Option<T> {
        let cfg = self.settings().await;
        self.throttle(Self::interval(cfg.rate_limit)).await;
        let mut q: Vec<(&str, String)> = vec![("fmt", "json".to_owned())];
        q.extend(query.iter().cloned());
        let resp = self
            .http
            .get(format!("{}{path}", cfg.server))
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .query(&q)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json().await.ok()
    }

    /// Resolves an artist's `MusicBrainzArtist` id by name, or `None`. Port of
    /// `MusicBrainzArtistProvider`'s search. Uses `artist:"…"` / `artistaccent`
    /// lucene the same way the C# builds it.
    pub async fn search_artist(&self, name: &str) -> Option<String> {
        Some(self.search_artist_match(name).await?.id)
    }

    /// The first artist search hit, id **and** name.
    ///
    /// The name is what `MusicBrainzArtistProvider.cs:146` writes back over the
    /// item's when `ReplaceArtistName` is on
    /// (`result.Item.Name = singleResult.Name`); [`replace_artist_name`] says
    /// whether it should be.
    ///
    /// [`replace_artist_name`]: Self::replace_artist_name
    pub async fn search_artist_match(&self, name: &str) -> Option<ArtistNameMatch> {
        let query = artist_query(name);
        let result: ArtistSearch = self.get("/ws/2/artist", &[("query", query)]).await?;
        let hit = result.artists.into_iter().next()?;
        Some(ArtistNameMatch {
            id: hit.id,
            name: non_empty(hit.name),
        })
    }

    /// Whether the MusicBrainz settings page's `ReplaceArtistName` is on.
    pub async fn replace_artist_name(&self) -> bool {
        self.settings().await.replace_artist_name
    }

    /// Resolves an album's ids from its name + the album artist (by MB id when
    /// known, else by name). Port of the `MusicBrainzAlbumProvider` search:
    /// `"album" AND arid:{artist}` / `AND artist:"{name}"`.
    pub async fn search_release(
        &self,
        album: &str,
        artist_mbid: Option<&str>,
        artist_name: Option<&str>,
    ) -> AlbumIds {
        let query = release_query(album, artist_mbid, artist_name);
        let Some(result): Option<ReleaseSearch> =
            self.get("/ws/2/release", &[("query", query)]).await
        else {
            return AlbumIds::default();
        };
        result
            .releases
            .into_iter()
            .next()
            .map_or_else(AlbumIds::default, |r| AlbumIds {
                release_id: Some(r.id),
                release_group_id: r.group.map(|g| g.id),
            })
    }

    /// Runs a raw lucene release search (`/ws/2/release?query=`) and returns
    /// every hit in MB's order — port of `Query.FindReleasesAsync` as the
    /// "Identify" flow drives it (the caller composes the query exactly as the
    /// C# provider does). Empty on any failure.
    pub async fn find_releases(&self, query: &str) -> Vec<ReleaseHit> {
        let Some(result): Option<ReleaseSearch> = self
            .get("/ws/2/release", &[("query", query.to_owned())])
            .await
        else {
            return Vec::new();
        };
        result.releases.into_iter().map(ReleaseHit::from).collect()
    }

    /// Looks up one release with its artists + release group
    /// (`inc=artists+release-groups`) — port of `Query.LookupReleaseAsync(id,
    /// Include.Artists | Include.ReleaseGroups)`. `None` on any failure.
    pub async fn lookup_release(&self, release_id: &str) -> Option<ReleaseHit> {
        let release: Release = self
            .get(
                &format!("/ws/2/release/{release_id}"),
                &[("inc", "artists+release-groups".to_owned())],
            )
            .await?;
        Some(ReleaseHit::from(release))
    }

    /// Looks up a release group's releases (`inc=releases`) and resolves each
    /// through [`lookup_release`](Self::lookup_release) — port of the
    /// `MusicBrainzAlbumProvider.GetReleaseGroupResultAsync` walk. Empty when
    /// the group is unknown.
    pub async fn release_group_releases(&self, release_group_id: &str) -> Vec<ReleaseHit> {
        let Some(group): Option<ReleaseGroupLookup> = self
            .get(
                &format!("/ws/2/release-group/{release_group_id}"),
                &[("inc", "releases".to_owned())],
            )
            .await
        else {
            return Vec::new();
        };
        let mut hits = Vec::with_capacity(group.releases.len());
        for release in group.releases {
            if let Some(hit) = self.lookup_release(&release.id).await {
                hits.push(hit);
            }
        }
        hits
    }

    /// Runs a raw lucene artist search (`/ws/2/artist?query=`) and returns
    /// every hit — port of `Query.FindArtistsAsync` for the "Identify" flow.
    /// Empty on any failure.
    pub async fn find_artists(&self, query: &str) -> Vec<ArtistHit> {
        let Some(result): Option<ArtistSearchFull> = self
            .get("/ws/2/artist", &[("query", query.to_owned())])
            .await
        else {
            return Vec::new();
        };
        result
            .artists
            .into_iter()
            .filter_map(|a| {
                Some(ArtistHit {
                    id: non_empty(a.id)?,
                    name: non_empty(a.name),
                    begin: a
                        .span
                        .and_then(|s| s.begin)
                        .as_deref()
                        .and_then(parse_partial_date),
                })
            })
            .collect()
    }

    /// Looks up one artist by id — port of `Query.LookupArtistAsync` as the
    /// "Identify" flow uses it. `None` on any failure.
    pub async fn lookup_artist(&self, artist_id: &str) -> Option<ArtistHit> {
        let artist: ArtistLookup = self.get(&format!("/ws/2/artist/{artist_id}"), &[]).await?;
        Some(ArtistHit {
            id: non_empty(artist.id).unwrap_or_else(|| artist_id.to_owned()),
            name: non_empty(artist.name),
            begin: artist
                .span
                .and_then(|s| s.begin)
                .as_deref()
                .and_then(parse_partial_date),
        })
    }

    /// Looks up a release to get its release-group id (`inc=release-groups`).
    pub async fn release_group_of(&self, release_id: &str) -> Option<String> {
        let result: Release = self
            .get(
                &format!("/ws/2/release/{release_id}"),
                &[("inc", "release-groups".to_owned())],
            )
            .await?;
        result.group.map(|g| g.id)
    }

    /// Looks up a release group to get its first release id (`inc=releases`).
    pub async fn first_release_of(&self, release_group_id: &str) -> Option<String> {
        let result: ReleaseGroupLookup = self
            .get(
                &format!("/ws/2/release-group/{release_group_id}"),
                &[("inc", "releases".to_owned())],
            )
            .await?;
        result.releases.into_iter().next().map(|r| r.id)
    }

    /// One release's own metadata — port of the fields
    /// `MusicBrainzAlbumProvider` writes onto a `MusicAlbum` beyond its ids.
    ///
    /// MusicBrainz dates may be `YYYY`, `YYYY-MM` or `YYYY-MM-DD`; the missing
    /// parts default to January 1st, as C#'s partial-date handling does.
    pub async fn release_details(&self, release_id: &str) -> Option<ReleaseDetails> {
        let release: Release = self
            .get(&format!("/ws/2/release/{release_id}"), &[])
            .await?;
        let date = release.date.as_deref().and_then(parse_partial_date);
        Some(ReleaseDetails {
            name: non_empty(release.title),
            premiere_date: date,
            production_year: date.map(|d| d.year),
        })
    }

    /// One artist's own metadata — port of the fields
    /// `MusicBrainzArtistProvider` writes onto a `MusicArtist`.
    pub async fn artist_details(&self, artist_id: &str) -> Option<ArtistDetails> {
        let artist: ArtistLookup = self.get(&format!("/ws/2/artist/{artist_id}"), &[]).await?;
        let life_span = artist.span.unwrap_or_default();
        Some(ArtistDetails {
            name: non_empty(artist.name),
            premiere_date: life_span.begin.as_deref().and_then(parse_partial_date),
            end_date: life_span.end.as_deref().and_then(parse_partial_date),
        })
    }

    /// The full album-id resolution with the faithful precedence: fill the
    /// missing half of a known pair via a lookup, else search by name. Returns
    /// whatever it could resolve (possibly the input unchanged).
    pub async fn resolve_album(
        &self,
        album: &str,
        mut ids: AlbumIds,
        artist_mbid: Option<&str>,
        artist_name: Option<&str>,
    ) -> AlbumIds {
        // release-group known, release missing → first release of the group.
        if ids.release_id.is_none()
            && let Some(rg) = ids.release_group_id.as_deref()
        {
            ids.release_id = self.first_release_of(rg).await;
        }
        // Neither known → search by name.
        if ids.release_id.is_none() && ids.release_group_id.is_none() {
            ids = self.search_release(album, artist_mbid, artist_name).await;
        }
        // release known, group missing → look it up.
        if ids.release_group_id.is_none()
            && let Some(rel) = ids.release_id.as_deref()
        {
            ids.release_group_id = self.release_group_of(rel).await;
        }
        ids
    }
}

/// The lucene query for an artist search. Diacritics route through
/// `artistaccent` (C# `MusicBrainzArtistProvider`), else a plain phrase.
fn artist_query(name: &str) -> String {
    let escaped = lucene_escape(name);
    if name.is_ascii() {
        format!("artist:\"{escaped}\"")
    } else {
        format!("artistaccent:\"{escaped}\"")
    }
}

/// The lucene query for a release search: `"album" AND arid:{mbid}` when the
/// artist id is known, else `"album" AND artist:"{name}"`, else just `"album"`.
fn release_query(album: &str, artist_mbid: Option<&str>, artist_name: Option<&str>) -> String {
    use std::fmt::Write as _;
    let mut q = format!("\"{}\"", lucene_escape(album));
    if let Some(arid) = artist_mbid.filter(|s| !s.is_empty()) {
        // arid is a raw MBID (UUID) — not lucene-escaped.
        let _ = write!(q, " AND arid:{arid}");
    } else if let Some(name) = artist_name.filter(|s| !s.is_empty()) {
        let _ = write!(q, " AND artist:\"{}\"", lucene_escape(name));
    }
    q
}

/// Escapes the lucene special characters MB's query parser reserves.
fn lucene_escape(input: &str) -> String {
    const SPECIAL: &[char] = &[
        '+', '-', '&', '|', '!', '(', ')', '{', '}', '[', ']', '^', '"', '~', '*', '?', ':', '\\',
        '/',
    ];
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if SPECIAL.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {

    #[test]
    fn partial_dates_default_the_missing_parts_to_the_first() {
        assert_eq!(
            super::parse_partial_date("1997"),
            Some(super::PartialDate {
                year: 1997,
                month: 1,
                day: 1
            })
        );
        assert_eq!(
            super::parse_partial_date("1997-06"),
            Some(super::PartialDate {
                year: 1997,
                month: 6,
                day: 1
            })
        );
        assert_eq!(
            super::parse_partial_date(" 1997-06-16 "),
            Some(super::PartialDate {
                year: 1997,
                month: 6,
                day: 16
            })
        );
        // Out-of-range parts are not a date.
        assert_eq!(super::parse_partial_date("1997-13"), None);
        assert_eq!(super::parse_partial_date("1997-06-40"), None);
        assert_eq!(super::parse_partial_date("not a date"), None);
    }

    #[test]
    fn a_partial_date_converts_to_midnight_utc() {
        let date = super::PartialDate {
            year: 1997,
            month: 6,
            day: 16,
        };
        assert_eq!(
            date.to_utc().map(|d| d.to_rfc3339()),
            Some("1997-06-16T00:00:00+00:00".to_owned())
        );
    }
    use super::*;

    #[test]
    fn artist_query_uses_accent_field_for_non_ascii() {
        assert_eq!(artist_query("Miles Davis"), "artist:\"Miles Davis\"");
        assert_eq!(artist_query("Björk"), "artistaccent:\"Björk\"");
    }

    #[test]
    fn release_query_prefers_arid_then_artist_name() {
        assert_eq!(
            release_query("Kind of Blue", Some("mbid-123"), Some("Miles Davis")),
            "\"Kind of Blue\" AND arid:mbid-123"
        );
        assert_eq!(
            release_query("Kind of Blue", None, Some("Miles Davis")),
            "\"Kind of Blue\" AND artist:\"Miles Davis\""
        );
        assert_eq!(
            release_query("Kind of Blue", None, None),
            "\"Kind of Blue\""
        );
        // Blank ids/names are ignored.
        assert_eq!(release_query("X", Some(""), Some("")), "\"X\"");
    }

    #[test]
    fn lucene_escape_escapes_reserved_chars() {
        assert_eq!(lucene_escape("AC/DC"), "AC\\/DC");
        assert_eq!(
            lucene_escape("Sign \"O\" the Times"),
            "Sign \\\"O\\\" the Times"
        );
        assert_eq!(lucene_escape("plain"), "plain");
    }

    #[test]
    fn artist_search_parses_first_id() {
        let s: ArtistSearch = serde_json::from_str(
            r#"{"artists":[{"id":"artist-mbid","name":"Miles Davis","score":100},
                          {"id":"other","name":"other"}]}"#,
        )
        .expect("artist search");
        assert_eq!(
            s.artists.first().map(|a| a.id.as_str()),
            Some("artist-mbid")
        );
    }

    #[test]
    fn release_search_parses_release_and_group() {
        let s: ReleaseSearch = serde_json::from_str(
            r#"{"releases":[{"id":"rel-1","release-group":{"id":"rg-1"}},
                           {"id":"rel-2"}]}"#,
        )
        .expect("release search");
        let first = s.releases.into_iter().next().unwrap();
        assert_eq!(first.id, "rel-1");
        assert_eq!(first.group.map(|g| g.id).as_deref(), Some("rg-1"));
    }

    #[test]
    fn album_ids_is_some() {
        assert!(!AlbumIds::default().is_some());
        assert!(
            AlbumIds {
                release_group_id: Some("x".into()),
                ..AlbumIds::default()
            }
            .is_some()
        );
    }

    #[tokio::test]
    async fn resolution_paths_over_mock_server() {
        use crate::mock_http::MockServer;
        let server = MockServer::start(vec![
            (
                "/ws/2/artist",
                r#"{"artists":[{"id":"artist-mbid","name":"Miles Davis"}]}"#.to_owned(),
            ),
            (
                "/ws/2/release-group/",
                r#"{"releases":[{"id":"rel-from-rg"}]}"#.to_owned(),
            ),
            (
                "/ws/2/release/",
                r#"{"id":"rel-1","release-group":{"id":"rg-looked-up"}}"#.to_owned(),
            ),
            (
                "/ws/2/release?",
                r#"{"releases":[{"id":"rel-searched","release-group":{"id":"rg-searched"}}]}"#
                    .to_owned(),
            ),
        ])
        .await;
        let c = MusicBrainzClient::new(&server.base_url, "test");

        assert_eq!(
            c.search_artist("Miles Davis").await.as_deref(),
            Some("artist-mbid")
        );

        // Neither id known → search by name populates both.
        let ids = c
            .resolve_album(
                "Kind of Blue",
                AlbumIds::default(),
                Some("artist-mbid"),
                None,
            )
            .await;
        assert_eq!(ids.release_id.as_deref(), Some("rel-searched"));
        assert_eq!(ids.release_group_id.as_deref(), Some("rg-searched"));

        // Release-group known, release missing → first_release_of fills the release.
        let ids = c
            .resolve_album(
                "X",
                AlbumIds {
                    release_group_id: Some("rg-x".to_owned()),
                    ..AlbumIds::default()
                },
                None,
                None,
            )
            .await;
        assert_eq!(ids.release_id.as_deref(), Some("rel-from-rg"));

        // Release known, group missing → release_group_of fills the group.
        let ids = c
            .resolve_album(
                "X",
                AlbumIds {
                    release_id: Some("rel-y".to_owned()),
                    ..AlbumIds::default()
                },
                None,
                None,
            )
            .await;
        assert_eq!(ids.release_group_id.as_deref(), Some("rg-looked-up"));
    }

    #[tokio::test]
    async fn identify_hits_carry_title_date_group_and_artist_credits() {
        use crate::mock_http::MockServer;
        let server = MockServer::start(vec![
            (
                "/ws/2/artist?",
                r#"{"artists":[{"id":"artist-mbid","name":"Miles Davis","life-span":{"begin":"1926-05-26"}},{"name":"no id"}]}"#.to_owned(),
            ),
            (
                "/ws/2/artist/",
                r#"{"id":"artist-mbid","name":"Miles Davis","life-span":{"begin":"1926"}}"#.to_owned(),
            ),
            (
                "/ws/2/release?",
                r#"{"releases":[{"id":"rel-1","title":"Kind of Blue","date":"1959-08-17","release-group":{"id":"rg-1"},"artist-credit":[{"name":"Miles Davis","artist":{"id":"artist-mbid","name":"Miles Davis"}},{"artist":{"id":"other","name":"Other"}}]}]}"#.to_owned(),
            ),
        ])
        .await;
        let c = MusicBrainzClient::new(&server.base_url, "test");

        let hits = c.find_releases("\"Kind of Blue\"").await;
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.id, "rel-1");
        assert_eq!(hit.title.as_deref(), Some("Kind of Blue"));
        assert_eq!(hit.date.map(|d| d.year), Some(1959));
        assert_eq!(hit.release_group_id.as_deref(), Some("rg-1"));
        assert_eq!(hit.artist_credits.len(), 2);
        assert_eq!(hit.artist_credits[0].name.as_deref(), Some("Miles Davis"));
        assert_eq!(
            hit.artist_credits[0].artist_id.as_deref(),
            Some("artist-mbid")
        );
        // A credit without its own name falls back to the artist's name.
        assert_eq!(hit.artist_credits[1].name.as_deref(), Some("Other"));

        let artists = c.find_artists("\"Miles Davis\"").await;
        // The id-less hit is dropped.
        assert_eq!(artists.len(), 1);
        assert_eq!(artists[0].id, "artist-mbid");
        assert_eq!(
            artists[0].begin.map(|d| (d.year, d.month, d.day)),
            Some((1926, 5, 26))
        );

        let artist = c.lookup_artist("artist-mbid").await.expect("lookup");
        assert_eq!(artist.name.as_deref(), Some("Miles Davis"));
        assert_eq!(artist.begin.map(|d| d.year), Some(1926));
    }

    #[tokio::test]
    #[ignore = "hits the live MusicBrainz API; run with --ignored"]
    async fn live_resolves_kind_of_blue() {
        let c = MusicBrainzClient::new("", "test");
        let artist = c.search_artist("Miles Davis").await;
        assert!(artist.is_some(), "expected an artist mbid");
        let ids = c
            .resolve_album(
                "Kind of Blue",
                AlbumIds::default(),
                artist.as_deref(),
                Some("Miles Davis"),
            )
            .await;
        assert!(
            ids.release_group_id.is_some(),
            "expected a release-group id"
        );
    }
}
