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

use crate::rate_limit::RateLimiter;
use serde::Deserialize;

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

    /// Both ids as the lookups send them ([`lookup_id`]): canonical, a
    /// malformed one dropped.
    fn for_lookup(self) -> Self {
        Self {
            release_id: self
                .release_id
                .as_deref()
                .and_then(|id| lookup_id(id, "release")),
            release_group_id: self
                .release_group_id
                .as_deref()
                .and_then(|id| lookup_id(id, "release group")),
        }
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
    /// `inc=labels`.
    #[serde(rename = "label-info", default)]
    label_info: Vec<LabelInfoWire>,
    /// `inc=genres`; `None` when not included.
    #[serde(default)]
    genres: Option<Vec<VotedWire>>,
    /// `inc=tags`; `None` when not included.
    #[serde(default)]
    tags: Option<Vec<VotedWire>>,
}

/// One `label-info` entry on a release (`inc=labels`).
#[derive(Debug, Deserialize)]
struct LabelInfoWire {
    #[serde(default)]
    label: Option<LabelWire>,
}

#[derive(Debug, Deserialize)]
struct LabelWire {
    #[serde(default)]
    name: Option<String>,
}

/// A genre or tag with its vote count (`inc=genres` / `inc=tags`).
#[derive(Debug, Deserialize)]
struct VotedWire {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    count: i64,
}

/// An artist's area (`area.name`).
#[derive(Debug, Deserialize)]
struct AreaWire {
    #[serde(default)]
    name: Option<String>,
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
    #[serde(default)]
    area: Option<AreaWire>,
    #[serde(default)]
    country: Option<String>,
    /// `inc=genres`; `None` when not included.
    #[serde(default)]
    genres: Option<Vec<VotedWire>>,
    /// `inc=tags`; `None` when not included.
    #[serde(default)]
    tags: Option<Vec<VotedWire>>,
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
    /// The group's first release date: the original album's.
    #[serde(rename = "first-release-date", default)]
    first_release_date: Option<String>,
    /// `inc=artists`.
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCreditWire>,
    /// `inc=genres`; `None` when not included.
    #[serde(default)]
    genres: Option<Vec<VotedWire>>,
    /// `inc=tags`; `None` when not included.
    #[serde(default)]
    tags: Option<Vec<VotedWire>>,
}

/// The names of `voted`, most votes first (`OrderByDescending(VoteCount)`,
/// stable), blank ones dropped.
fn by_votes(voted: &[VotedWire]) -> Vec<String> {
    let mut ordered: Vec<&VotedWire> = voted.iter().collect();
    ordered.sort_by_key(|v| std::cmp::Reverse(v.count));
    ordered
        .into_iter()
        .filter_map(|v| non_empty(v.name.clone()))
        .collect()
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

/// What `MusicBrainzAlbumProvider.GetMetadata` answers for an album: the
/// ids it settled on and what `Populate` (`MusicBrainzAlbumProvider.cs:
/// 268-320`) reads off the looked-up release and release group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlbumDetails {
    /// The `MusicBrainzAlbum` (release) id.
    pub release_id: Option<String>,
    /// The `MusicBrainzReleaseGroup` id — the given one, else the looked-up
    /// release's.
    pub release_group_id: Option<String>,
    /// `releaseGroup?.FirstReleaseDate ?? release?.Date`.
    pub premiere_date: Option<PartialDate>,
    /// That date's year.
    pub production_year: Option<i32>,
    /// The credited artist names (`release?.ArtistCredit ??
    /// releaseGroup?.ArtistCredit`): the album artists.
    pub album_artists: Vec<String>,
    /// `releaseGroup?.Genres ?? release?.Genres`, most votes first.
    pub genres: Vec<String>,
    /// `releaseGroup?.Tags ?? release?.Tags`, most votes first.
    pub tags: Vec<String>,
    /// The release's label names, distinct ignoring case: its studios.
    pub studios: Vec<String>,
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
    /// The release date, as MusicBrainz answered it (`Date?.Year` /
    /// `Date?.NearestDate` in the C#).
    pub date: MbDate,
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
    /// The life-span begin date (`LifeSpan?.Begin`), as MusicBrainz answered
    /// it — see [`MbDate`].
    pub begin: MbDate,
}

impl From<Release> for ReleaseHit {
    fn from(r: Release) -> Self {
        Self {
            id: r.id,
            title: non_empty(r.title),
            date: MbDate::parse(r.date.as_deref()),
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

/// One artist's metadata beyond its id — what `MusicBrainzArtistProvider.
/// GetMetadata` (`MusicBrainzArtistProvider.cs:112-175`) reads off the
/// looked-up artist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtistDetails {
    /// The artist name as MusicBrainz spells it.
    pub name: Option<String>,
    /// The life-span begin date (a band's formation).
    pub premiere_date: Option<PartialDate>,
    /// The life-span end date (a band's break-up), which the artist NFO saver
    /// writes as `<disbanded>`.
    pub end_date: Option<PartialDate>,
    /// Its area's name, else its country: its production location.
    pub location: Option<String>,
    /// Its genres, most votes first.
    pub genres: Vec<String>,
    /// Its tags, most votes first.
    pub tags: Vec<String>,
}

/// The `DateTime.MinValue` a component-less MetaBrainz `PartialDate` reports
/// as its `NearestDate` — `0001-01-01T00:00:00Z`, which Jellyfin serialises as
/// `"0001-01-01T00:00:00.0000000Z"`.
pub const MIN_DATE: PartialDate = PartialDate {
    year: 1,
    month: 1,
    day: 1,
};

/// How MusicBrainz answered a date field, preserving the distinction C#
/// inherits from MetaBrainz: a MISSING key leaves `IRelease.Date` null (both
/// `Year` and `NearestDate` null), while a key carrying no usable components —
/// MusicBrainz writes `"date": ""` for a release whose date is unknown — still
/// constructs a `PartialDate`, whose `Year` is null but whose `NearestDate` is
/// `DateTime.MinValue`. Collapsing the two drops `PremiereDate` from the
/// Identify dialog for every dateless release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MbDate {
    /// MusicBrainz supplied no date at all.
    #[default]
    Absent,
    /// MusicBrainz supplied a date with no parseable components.
    Componentless,
    /// MusicBrainz supplied a usable date.
    Known(PartialDate),
}

impl MbDate {
    /// `Date?.Year` — `None` unless MusicBrainz gave a real year.
    #[must_use]
    pub fn year(self) -> Option<i32> {
        match self {
            Self::Known(date) => Some(date.year),
            _ => None,
        }
    }

    /// `Date?.NearestDate` — the parsed instant, or `DateTime.MinValue` for a
    /// component-less date, or `None` when there was no date at all.
    #[must_use]
    pub fn nearest(self) -> Option<PartialDate> {
        match self {
            Self::Absent => None,
            Self::Componentless => Some(MIN_DATE),
            Self::Known(date) => Some(date),
        }
    }

    /// Classifies the raw JSON value MusicBrainz returned for a date field.
    fn parse(value: Option<&str>) -> Self {
        match value {
            None => Self::Absent,
            Some(raw) => parse_partial_date(raw).map_or(Self::Componentless, Self::Known),
        }
    }
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
    /// Request pacing and server-directed cooldown, shared by all callers.
    limiter: RateLimiter,
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
            user_agent: format!("Ferrofin/{version} ( https://github.com/mangoleaf/ferrofin )"),
            limiter: RateLimiter::new("musicbrainz"),
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

    /// `RateLimit` seconds as a `Duration`.
    ///
    /// A negative or non-finite value (only reachable by hand-editing
    /// `config.json` — the settings page writes a number input) would panic
    /// `Duration::from_secs_f64`, so it falls back to [`MIN_INTERVAL`], the
    /// published MusicBrainz policy floor. Falling back to *zero* there would
    /// turn a typo into an unthrottled crawl of a public service.
    fn interval(rate_limit: f64) -> Duration {
        if rate_limit > 0.0 {
            Duration::try_from_secs_f64(rate_limit)
                .unwrap_or(MIN_INTERVAL)
                .min(Duration::from_secs(u64::from(u32::MAX)))
        } else {
            MIN_INTERVAL
        }
    }

    fn request_interval(server: &str, rate_limit: f64) -> Duration {
        let interval = Self::interval(rate_limit);
        let official = reqwest::Url::parse(server).is_ok_and(|url| {
            url.host_str()
                .is_some_and(|host| host == "musicbrainz.org" || host.ends_with(".musicbrainz.org"))
        });
        if official {
            interval.max(MIN_INTERVAL)
        } else {
            interval
        }
    }

    /// Resolves metadata through the shared provider limiter.
    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Option<T> {
        let cfg = self.settings().await;
        let request = self
            .http
            .get(format!("{}{path}", cfg.server))
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .query(&[("fmt", "json")])
            .query(query);
        self.limiter
            .get_json(request, Self::request_interval(&cfg.server, cfg.rate_limit))
            .await
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
    /// `"album" AND arid:{artist}` / `AND artist:"{name}"`. With neither
    /// there is no search and no match (`MusicBrainzAlbumProvider.cs:
    /// 196-212` searches only in those two branches).
    pub async fn search_release(
        &self,
        album: &str,
        artist_mbid: Option<&str>,
        artist_name: Option<&str>,
    ) -> AlbumIds {
        let Some(query) = release_query(album, artist_mbid, artist_name) else {
            return AlbumIds::default();
        };
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
        let release_id = lookup_id(release_id, "release")?;
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
        let Some(release_group_id) = lookup_id(release_group_id, "release group") else {
            return Vec::new();
        };
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
                    begin: MbDate::parse(a.span.and_then(|s| s.begin).as_deref()),
                })
            })
            .collect()
    }

    /// Looks up one artist by id — port of `Query.LookupArtistAsync` as the
    /// "Identify" flow uses it. `None` on any failure.
    pub async fn lookup_artist(&self, artist_id: &str) -> Option<ArtistHit> {
        let artist_id = lookup_id(artist_id, "artist")?;
        let artist: ArtistLookup = self.get(&format!("/ws/2/artist/{artist_id}"), &[]).await?;
        Some(ArtistHit {
            id: non_empty(artist.id).unwrap_or(artist_id),
            name: non_empty(artist.name),
            begin: MbDate::parse(artist.span.and_then(|s| s.begin).as_deref()),
        })
    }

    /// Looks up a release group to get its first release id (`inc=releases`).
    pub async fn first_release_of(&self, release_group_id: &str) -> Option<String> {
        let release_group_id = lookup_id(release_group_id, "release group")?;
        let result: ReleaseGroupLookup = self
            .get(
                &format!("/ws/2/release-group/{release_group_id}"),
                &[("inc", "releases".to_owned())],
            )
            .await?;
        result.releases.into_iter().next().map(|r| r.id)
    }

    /// The tail of `MusicBrainzAlbumProvider.GetMetadata` over resolved
    /// `ids` (`MusicBrainzAlbumProvider.cs:220-265`): the release looked up
    /// with its artists, release group, labels, genres and tags — whose
    /// group is the album's when none was known — then the release group
    /// with its artists, genres and tags, and `Populate` over both. `None`
    /// when neither id is known or neither lookup finds anything.
    pub async fn album_details(&self, ids: AlbumIds) -> Option<AlbumDetails> {
        let ids = ids.for_lookup();
        let release: Option<Release> = match ids.release_id.as_deref() {
            Some(id) => {
                self.get(
                    &format!("/ws/2/release/{id}"),
                    &[(
                        "inc",
                        "artists+release-groups+labels+genres+tags".to_owned(),
                    )],
                )
                .await
            }
            None => None,
        };
        let release_group_id = ids.release_group_id.or_else(|| {
            release
                .as_ref()
                .and_then(|r| r.group.as_ref().map(|g| g.id.clone()))
        });
        let group: Option<ReleaseGroupLookup> = match release_group_id.as_deref() {
            Some(id) => {
                self.get(
                    &format!("/ws/2/release-group/{id}"),
                    &[("inc", "artists+genres+tags".to_owned())],
                )
                .await
            }
            None => None,
        };
        if release.is_none() && group.is_none() {
            return None;
        }
        // `Populate`: "Prefer the release group (album-level) data, falling
        // back to the specific release".
        //
        // ACCEPTED DIVERGENCE (the owner's rule: Jellyfin's bugs are not
        // ported): a group whose first-release date is blank (`""`) falls
        // back to the release's date. Upstream's `??` takes the blank date,
        // which MetaBrainz still builds into a `PartialDate`, and stores
        // `PremiereDate` 0001-01-01 with no year
        // (`MusicBrainzAlbumProvider.cs:282-287`).
        let date = group
            .as_ref()
            .and_then(|g| g.first_release_date.as_deref())
            .and_then(parse_partial_date)
            .or_else(|| {
                release
                    .as_ref()
                    .and_then(|r| r.date.as_deref())
                    .and_then(parse_partial_date)
            });
        let credit = match (&release, &group) {
            (Some(release), _) => &release.artist_credit,
            (None, Some(group)) => &group.artist_credit,
            (None, None) => return None,
        };
        let voted = |of_group: Option<&Vec<VotedWire>>, of_release: Option<&Vec<VotedWire>>| {
            of_group
                .or(of_release)
                .map(|v| by_votes(v))
                .unwrap_or_default()
        };
        // `.Distinct(StringComparer.OrdinalIgnoreCase)`: folded as .NET's
        // invariant upper case, non-ASCII letters included.
        let mut studios: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for label in release.iter().flat_map(|r| &r.label_info) {
            if let Some(name) = non_empty(label.label.as_ref().and_then(|l| l.name.clone()))
                && seen.insert(ferrofin_util::string_extensions::upper_invariant(&name))
            {
                studios.push(name);
            }
        }
        Some(AlbumDetails {
            release_id: ids.release_id,
            release_group_id,
            premiere_date: date,
            production_year: date.map(|d| d.year),
            album_artists: credit
                .iter()
                .filter_map(|c| non_empty(c.name.clone()))
                .collect(),
            genres: voted(
                group.as_ref().and_then(|g| g.genres.as_ref()),
                release.as_ref().and_then(|r| r.genres.as_ref()),
            ),
            tags: voted(
                group.as_ref().and_then(|g| g.tags.as_ref()),
                release.as_ref().and_then(|r| r.tags.as_ref()),
            ),
            studios,
        })
    }

    /// One artist's own metadata — port of the fields
    /// `MusicBrainzArtistProvider.GetMetadata` writes onto a `MusicArtist`
    /// from `LookupArtistOrNullAsync(id, Include.Genres | Include.Tags)`.
    pub async fn artist_details(&self, artist_id: &str) -> Option<ArtistDetails> {
        let artist_id = lookup_id(artist_id, "artist")?;
        let artist: ArtistLookup = self
            .get(
                &format!("/ws/2/artist/{artist_id}"),
                &[("inc", "genres+tags".to_owned())],
            )
            .await?;
        let life_span = artist.span.unwrap_or_default();
        Some(ArtistDetails {
            name: non_empty(artist.name),
            // ACCEPTED DIVERGENCE (the owner's rule: Jellyfin's bugs are
            // not ported): a blank `life-span.begin` gives no date. Upstream
            // takes it (`artist.LifeSpan?.Begin is not null`) and stores
            // `PremiereDate` 0001-01-01 with no year
            // (`MusicBrainzArtistProvider.cs:147-151`).
            premiere_date: life_span.begin.as_deref().and_then(parse_partial_date),
            end_date: life_span.end.as_deref().and_then(parse_partial_date),
            // `string.IsNullOrWhiteSpace(artist.Area?.Name) ? artist.Country
            // : artist.Area!.Name`.
            location: non_empty(artist.area.and_then(|a| a.name))
                .or_else(|| non_empty(artist.country)),
            genres: artist.genres.as_deref().map(by_votes).unwrap_or_default(),
            tags: artist.tags.as_deref().map(by_votes).unwrap_or_default(),
        })
    }

    /// The album-id resolution of `MusicBrainzAlbumProvider.GetMetadata`
    /// (`MusicBrainzAlbumProvider.cs:176-215`): a known release group with
    /// no release takes the group's first release; still no release → a
    /// search by name and artist (only with an artist id or name), whose hit
    /// gives the release and, when it carries one, the release group.
    /// Returns whatever it could resolve (possibly the input unchanged); a
    /// release still missing its group gets it from
    /// [`album_details`](Self::album_details)' release lookup.
    pub async fn resolve_album(
        &self,
        album: &str,
        ids: AlbumIds,
        artist_mbid: Option<&str>,
        artist_name: Option<&str>,
    ) -> AlbumIds {
        // `ParseMusicBrainzId` on both ids first: a malformed one is no id.
        let mut ids = ids.for_lookup();
        // release-group known, release missing → first release of the group.
        if ids.release_id.is_none()
            && let Some(rg) = ids.release_group_id.as_deref()
        {
            ids.release_id = self.first_release_of(rg).await;
        }
        // Still no release (none known, or a group without releases) →
        // search; the hit's group replaces a known one.
        if ids.release_id.is_none() {
            let hit = self.search_release(album, artist_mbid, artist_name).await;
            if let Some(release) = hit.release_id {
                ids.release_id = Some(release);
                if hit.release_group_id.is_some() {
                    ids.release_group_id = hit.release_group_id;
                }
            }
        }
        ids
    }
}

/// `MusicBrainzQueryExtensions.ParseMusicBrainzId`
/// (`MusicBrainzQueryExtensions.cs:35-49`): the id a by-id lookup sends —
/// the GUID `id` names in any form `Guid.TryParse` reads
/// ([`parse_guid`](crate::metadata_merge::parse_guid)), written as
/// `Guid.ToString()` writes it (the lowercase `D` form). A braced or
/// `X`-form id in a URL path is a 400 from MusicBrainz — a failed request,
/// which would keep the item from ever being stamped as refreshed. `None`
/// for a blank or malformed id, which upstream ignores rather than sends.
fn lookup_id(id: &str, entity: &'static str) -> Option<String> {
    if id.trim().is_empty() {
        return None;
    }
    let parsed = crate::metadata_merge::parse_guid(id);
    if parsed.is_none() {
        tracing::debug!(entity, id, "ignoring malformed MusicBrainz id");
    }
    parsed.map(|guid| guid.hyphenated().to_string())
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
/// artist id is known, else `"album" AND artist:"{name}"`; `None` with
/// neither — upstream searches by album name alone never
/// (`MusicBrainzAlbumProvider.cs:196-212`).
fn release_query(
    album: &str,
    artist_mbid: Option<&str>,
    artist_name: Option<&str>,
) -> Option<String> {
    let album = lucene_escape(album);
    if let Some(arid) = artist_mbid.filter(|s| !s.is_empty()) {
        // arid is a raw MBID (UUID) — not lucene-escaped.
        Some(format!("\"{album}\" AND arid:{arid}"))
    } else {
        artist_name
            .filter(|s| !s.is_empty())
            .map(|name| format!("\"{album}\" AND artist:\"{}\"", lucene_escape(name)))
    }
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
    use super::*;
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

    #[test]
    fn official_host_interval_floor() {
        assert_eq!(MusicBrainzClient::interval(f64::MAX), MIN_INTERVAL);
        assert_eq!(MusicBrainzClient::interval(f64::NAN), MIN_INTERVAL);
        for url in [
            "https://musicbrainz.org",
            "http://MUSICBRAINZ.ORG:80/",
            "https://www.musicbrainz.org",
        ] {
            assert_eq!(MusicBrainzClient::request_interval(url, 0.01), MIN_INTERVAL);
        }
        assert_eq!(
            MusicBrainzClient::request_interval("http://localhost", 0.1),
            Duration::from_millis(100)
        );
    }

    #[test]
    fn artist_query_uses_accent_field_for_non_ascii() {
        assert_eq!(artist_query("Miles Davis"), "artist:\"Miles Davis\"");
        assert_eq!(artist_query("Björk"), "artistaccent:\"Björk\"");
    }

    #[test]
    fn release_query_prefers_arid_then_artist_name() {
        assert_eq!(
            release_query("Kind of Blue", Some("mbid-123"), Some("Miles Davis")).as_deref(),
            Some("\"Kind of Blue\" AND arid:mbid-123")
        );
        assert_eq!(
            release_query("Kind of Blue", None, Some("Miles Davis")).as_deref(),
            Some("\"Kind of Blue\" AND artist:\"Miles Davis\"")
        );
        // Neither: no query — upstream never searches by album name alone.
        assert_eq!(release_query("Kind of Blue", None, None), None);
        // Blank ids/names are ignored.
        assert_eq!(release_query("X", Some(""), Some("")), None);
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

    /// Lookup ids: the lookups send only what `Guid.TryParse` reads.
    const REL: &str = "b1f1a1b0-0000-4000-8000-000000000001";
    const RG: &str = "b1f1a1b0-0000-4000-8000-000000000002";
    const ARTIST: &str = "b1f1a1b0-0000-4000-8000-000000000003";
    const RG_X: &str = "b1f1a1b0-0000-4000-8000-000000000004";
    const REL_Y: &str = "b1f1a1b0-0000-4000-8000-000000000005";
    const RG_EMPTY: &str = "b1f1a1b0-0000-4000-8000-000000000006";

    /// `ParseMusicBrainzId` before every by-id lookup
    /// (`MusicBrainzQueryExtensions.cs:35-49`): a braced, parenthesised,
    /// upper-case or `X`-form id is sent as the canonical lowercase `D` form
    /// (a raw one is a 400, a failed request forever), and a malformed one
    /// is not sent at all.
    #[rstest::rstest]
    #[case::braced("{B1F1A1B0-0000-4000-8000-000000000001}")]
    #[case::parenthesised("(b1f1a1b0-0000-4000-8000-000000000001)")]
    #[case::n_form("B1F1A1B0000040008000000000000001")]
    #[case::x_form("{0xb1f1a1b0,0x0,0x4000,{0x80,0x0,0x0,0x0,0x0,0x0,0x0,0x1}}")]
    #[case::d_form(" b1f1a1b0-0000-4000-8000-000000000001 ")]
    #[tokio::test]
    async fn a_lookup_sends_the_canonical_mbid(#[case] id: &str) {
        use crate::mock_http::MockServer;
        let server = MockServer::start(vec![
            (
                "/ws/2/release/b1f1a1b0-0000-4000-8000-000000000001?",
                r#"{"id":"b1f1a1b0-0000-4000-8000-000000000001","title":"Kind of Blue"}"#
                    .to_owned(),
            ),
            (
                "/ws/2/release-group/b1f1a1b0-0000-4000-8000-000000000001?",
                r#"{"releases":[{"id":"b1f1a1b0-0000-4000-8000-000000000009"}]}"#.to_owned(),
            ),
            (
                "/ws/2/artist/b1f1a1b0-0000-4000-8000-000000000001",
                r#"{"id":"b1f1a1b0-0000-4000-8000-000000000001","name":"Miles Davis"}"#.to_owned(),
            ),
        ])
        .await;
        let c = MusicBrainzClient::new(&server.base_url, "test");
        let hit = c.lookup_release(id).await.expect("release");
        assert_eq!(hit.title.as_deref(), Some("Kind of Blue"));
        assert_eq!(
            c.first_release_of(id).await.as_deref(),
            Some("b1f1a1b0-0000-4000-8000-000000000009")
        );
        let artist = c.lookup_artist(id).await.expect("artist");
        assert_eq!(artist.name.as_deref(), Some("Miles Davis"));
        let details = c
            .album_details(AlbumIds {
                release_id: Some(id.to_owned()),
                ..AlbumIds::default()
            })
            .await
            .expect("album");
        assert_eq!(
            details.release_id.as_deref(),
            Some("b1f1a1b0-0000-4000-8000-000000000001")
        );
    }

    #[tokio::test]
    async fn a_malformed_mbid_is_never_looked_up() {
        use crate::mock_http::MockServer;
        let server = MockServer::always(r#"{"id":"x","title":"Should not be asked"}"#).await;
        let c = MusicBrainzClient::new(&server.base_url, "test");
        assert!(c.lookup_release("not-an-mbid").await.is_none());
        assert!(c.lookup_artist("{not-an-mbid}").await.is_none());
        assert!(c.artist_details("rel-1").await.is_none());
        assert!(c.first_release_of("").await.is_none());
        assert!(c.release_group_releases("rg").await.is_empty());
    }

    /// `MusicBrainzAlbumProvider.Populate` (`:268-320`) over a release and
    /// its group, both looked up with what it includes: the group's first
    /// release date over the release's own, the release's artist credits,
    /// the group's genres and tags by votes (the release's only without a
    /// group), the release's labels as studios, distinct ignoring case.
    #[tokio::test]
    async fn an_album_is_populated_from_its_release_and_release_group() {
        use crate::mock_http::MockServer;
        let server = MockServer::start(vec![
            (
                "/ws/2/release-group/",
                r#"{"id":"b1f1a1b0-0000-4000-8000-000000000002","first-release-date":"1959-08-17",
                    "artist-credit":[{"name":"Group Credit"}],
                    "genres":[{"name":"modal jazz","count":2},{"name":"jazz","count":9},{"name":" ","count":20}],
                    "tags":[{"name":"trumpet","count":1}]}"#
                    .to_owned(),
            ),
            (
                "/ws/2/release/",
                r#"{"id":"b1f1a1b0-0000-4000-8000-000000000001","date":"1997-03-25","release-group":{"id":"b1f1a1b0-0000-4000-8000-000000000002"},
                    "artist-credit":[{"name":"Miles Davis","artist":{"id":"a"}},{"name":""}],
                    "label-info":[{"label":{"name":"Columbia"}},{"label":{"name":"columbia"}},{"label":null},
                                  {"label":{"name":"Éditions"}},{"label":{"name":"éditions"}}],
                    "genres":[{"name":"release genre","count":50}],
                    "tags":[{"name":"release tag","count":50}]}"#
                    .to_owned(),
            ),
        ])
        .await;
        let c = MusicBrainzClient::new(&server.base_url, "test");
        let details = c
            .album_details(AlbumIds {
                release_id: Some(REL.to_owned()),
                ..AlbumIds::default()
            })
            .await
            .expect("details");
        assert_eq!(
            details.release_group_id.as_deref(),
            Some(RG),
            "the release's group, when none was known"
        );
        assert_eq!(
            details.production_year,
            Some(1959),
            "the group's first date"
        );
        assert_eq!(
            details.premiere_date,
            Some(super::PartialDate {
                year: 1959,
                month: 8,
                day: 17
            })
        );
        assert_eq!(
            details.album_artists,
            ["Miles Davis"],
            "the release's credits"
        );
        assert_eq!(
            details.genres,
            ["jazz", "modal jazz"],
            "the group's, by votes"
        );
        assert_eq!(details.tags, ["trumpet"]);
        assert_eq!(
            details.studios,
            ["Columbia", "Éditions"],
            "distinct ignoring case, non-ASCII too"
        );

        // No group: the release's own genres, tags, date and credits.
        let server = MockServer::start(vec![(
            "/ws/2/release/",
            r#"{"id":"b1f1a1b0-0000-4000-8000-000000000001","date":"1997-03-25","artist-credit":[{"name":"Miles Davis"}],
                "genres":[{"name":"jazz","count":1}],"tags":[{"name":"reissue","count":1}]}"#
                .to_owned(),
        )])
        .await;
        let c = MusicBrainzClient::new(&server.base_url, "test");
        let details = c
            .album_details(AlbumIds {
                release_id: Some(REL.to_owned()),
                ..AlbumIds::default()
            })
            .await
            .expect("details");
        assert_eq!(details.release_group_id, None);
        assert_eq!(details.production_year, Some(1997));
        assert_eq!(details.genres, ["jazz"]);
        assert_eq!(details.tags, ["reissue"]);
        assert!(
            c.album_details(AlbumIds::default()).await.is_none(),
            "nothing to look up"
        );
    }

    /// `MusicBrainzArtistProvider.GetMetadata`'s lookup: the life span, the
    /// area's name else the country as the location, genres and tags by
    /// votes.
    #[tokio::test]
    async fn an_artist_is_populated_from_its_lookup() {
        use crate::mock_http::MockServer;
        let server = MockServer::start(vec![(
            "/ws/2/artist/",
            r#"{"id":"a","name":"Miles Davis","country":"US","area":{"name":"United States"},
                "life-span":{"begin":"1926-05-26","end":"1991-09-28"},
                "genres":[{"name":"bebop","count":1},{"name":"jazz","count":7}],
                "tags":[{"name":"trumpeter","count":3}]}"#
                .to_owned(),
        )])
        .await;
        let c = MusicBrainzClient::new(&server.base_url, "test");
        let details = c.artist_details(ARTIST).await.expect("details");
        assert_eq!(details.location.as_deref(), Some("United States"));
        assert_eq!(details.genres, ["jazz", "bebop"]);
        assert_eq!(details.tags, ["trumpeter"]);
        assert_eq!(details.premiere_date.map(|d| d.year), Some(1926));

        let server = MockServer::start(vec![(
            "/ws/2/artist/",
            r#"{"id":"a","country":"GB","area":{"name":" "}}"#.to_owned(),
        )])
        .await;
        let c = MusicBrainzClient::new(&server.base_url, "test");
        let details = c.artist_details(ARTIST).await.expect("details");
        assert_eq!(
            details.location.as_deref(),
            Some("GB"),
            "the country, without an area"
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
                    release_group_id: Some(RG_X.to_owned()),
                    ..AlbumIds::default()
                },
                None,
                None,
            )
            .await;
        assert_eq!(ids.release_id.as_deref(), Some("rel-from-rg"));

        // Release known, group missing: resolution asks nothing more — the
        // album lookup's release (`inc=release-groups`) gives the group.
        let known = AlbumIds {
            release_id: Some(REL_Y.to_owned()),
            ..AlbumIds::default()
        };
        let ids = c.resolve_album("X", known.clone(), None, None).await;
        assert_eq!(ids, known);
        let details = c.album_details(ids).await.expect("details");
        assert_eq!(details.release_group_id.as_deref(), Some("rg-looked-up"));
    }

    /// `MusicBrainzAlbumProvider.GetMetadata`'s search rules
    /// (`MusicBrainzAlbumProvider.cs:186-215`): with neither an artist id
    /// nor an album artist there is no search at all — the album alone is
    /// never searched; a known release group without releases is still
    /// searched, and the hit's group replaces it.
    #[tokio::test]
    async fn an_album_is_searched_only_by_its_artist_and_whenever_it_has_no_release() {
        use crate::mock_http::MockServer;
        let server = MockServer::start(vec![
            ("/ws/2/release-group/", r#"{"releases":[]}"#.to_owned()),
            (
                "/ws/2/release/",
                r#"{"id":"rel-searched","release-group":{"id":"rg-searched"}}"#.to_owned(),
            ),
            (
                "/ws/2/release?",
                r#"{"releases":[{"id":"rel-searched","release-group":{"id":"rg-searched"}}]}"#
                    .to_owned(),
            ),
        ])
        .await;
        let c = MusicBrainzClient::new(&server.base_url, "test");

        // No artist to search by: no search, so no match (a search would
        // have answered `rel-searched`).
        let ids = c
            .resolve_album("Kind of Blue", AlbumIds::default(), None, None)
            .await;
        assert_eq!(ids, AlbumIds::default(), "no search by album name alone");

        // A release group with no releases, and an artist to search by: the
        // search runs, and its hit's group replaces the known one.
        let empty_group = AlbumIds {
            release_group_id: Some(RG_EMPTY.to_owned()),
            ..AlbumIds::default()
        };
        let ids = c
            .resolve_album(
                "Kind of Blue",
                empty_group.clone(),
                Some("artist-mbid"),
                None,
            )
            .await;
        assert_eq!(ids.release_id.as_deref(), Some("rel-searched"));
        assert_eq!(ids.release_group_id.as_deref(), Some("rg-searched"));
        let ids = c
            .resolve_album(
                "Kind of Blue",
                empty_group.clone(),
                None,
                Some("Miles Davis"),
            )
            .await;
        assert_eq!(ids.release_id.as_deref(), Some("rel-searched"));

        // The same group with nothing to search by keeps the group alone.
        let ids = c
            .resolve_album("Kind of Blue", empty_group.clone(), None, None)
            .await;
        assert_eq!(ids, empty_group);
    }

    #[tokio::test]
    async fn a_dateless_release_keeps_the_min_date_sentinel() {
        // MusicBrainz writes `"date": ""` for a release whose date is unknown.
        // MetaBrainz still builds a `PartialDate` from it, so C# emits
        // `PremiereDate = DateTime.MinValue` with NO `ProductionYear`
        // (`MusicBrainzAlbumProvider.GetReleaseResult`: `Date?.NearestDate` /
        // `Date?.Year`). Dropping the distinction loses `PremiereDate` from
        // every dateless Identify candidate.
        use crate::mock_http::MockServer;
        let server = MockServer::start(vec![
            (
                "/ws/2/release?",
                r#"{"releases":[{"id":"empty","title":"No Date","date":""},{"id":"absent","title":"No Key"}]}"#.to_owned(),
            ),
            (
                "/ws/2/artist?",
                r#"{"artists":[{"id":"a","name":"A","life-span":{"begin":""}}]}"#.to_owned(),
            ),
        ])
        .await;
        let c = MusicBrainzClient::new(&server.base_url, "test");

        let hits = c.find_releases("x").await;
        assert_eq!(hits[0].date, MbDate::Componentless);
        assert_eq!(hits[0].date.year(), None, "no ProductionYear");
        assert_eq!(
            hits[0].date.nearest(),
            Some(MIN_DATE),
            "PremiereDate = MinValue"
        );
        assert_eq!(
            MIN_DATE.to_utc().expect("min date").to_rfc3339(),
            "0001-01-01T00:00:00+00:00"
        );
        // No `date` key at all leaves BOTH null.
        assert_eq!(hits[1].date, MbDate::Absent);
        assert_eq!(hits[1].date.year(), None);
        assert_eq!(hits[1].date.nearest(), None);

        // The artist life-span takes the same split.
        let artists = c.find_artists("x").await;
        assert_eq!(artists[0].begin, MbDate::Componentless);
        assert_eq!(artists[0].begin.nearest(), Some(MIN_DATE));
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
        assert_eq!(hit.date.year(), Some(1959));
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
            artists[0].begin.nearest().map(|d| (d.year, d.month, d.day)),
            Some((1926, 5, 26))
        );

        let artist = c.lookup_artist(ARTIST).await.expect("lookup");
        assert_eq!(artist.name.as_deref(), Some("Miles Davis"));
        assert_eq!(artist.begin.year(), Some(1926));
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
