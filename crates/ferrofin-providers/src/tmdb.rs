//! TheMovieDb (TMDB) remote artwork provider.
//!
//! A focused port of Jellyfin's `Tmdb` plugin (the image-acquisition slice):
//! matches an item by name/year against TMDB and returns its poster/backdrop
//! image URLs. Like Jellyfin, it uses a **built-in API key** ([`TmdbUtils.ApiKey`
//! upstream]) so artwork is fetched with zero configuration; a user-supplied key
//! overrides it.
//!
//! Movies and TV series are matched by name/year; [`details`](TmdbClient::details)
//! then fetches full metadata (overview, tagline, genres, studios, rating,
//! certification, premiere date, and cast + key crew) alongside the artwork.

use ferrofin_model::entities::ImageType;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

/// The TMDB v3 REST base.
const API_BASE: &str = "https://api.themoviedb.org/3";
/// The TMDB image CDN base; `original` is the largest available size.
const IMAGE_BASE: &str = "https://image.tmdb.org/t/p/original";
/// Jellyfin's built-in TMDB v3 API key, used when no user key is configured so
/// artwork works out of the box. Verbatim port of `TmdbUtils.ApiKey`.
const DEFAULT_API_KEY: &str = "4219e299c89411838049ab0dab19ebd5";

/// The kind of item to match against TMDB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmdbKind {
    /// A movie (`/search/movie`).
    Movie,
    /// A TV series (`/search/tv`).
    Series,
}

/// A remote image to download and persist: its [`ImageType`] and absolute URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteImage {
    /// The image type (Primary/Backdrop/…).
    pub image_type: ImageType,
    /// The absolute CDN URL of the image.
    pub url: String,
}

/// A TMDB search result carrying the id + image paths (movie: `title`, tv: `name`).
#[derive(Debug, Deserialize)]
struct SearchHit {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    poster_path: Option<String>,
    #[serde(default)]
    backdrop_path: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    release_date: Option<String>,
    #[serde(default)]
    first_air_date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<SearchHit>,
}

/// Appends TMDB's `language` query parameter when one was supplied.
fn with_language(req: reqwest::RequestBuilder, language: Option<&str>) -> reqwest::RequestBuilder {
    match language.filter(|l| !l.is_empty()) {
        Some(lang) => req.query(&[("language", lang)]),
        None => req,
    }
}

/// Maps one raw `/search/*` or `/find/*` row onto a [`TmdbSearchHit`]. Pure —
/// shared by the name search and the external-id lookup, whose payload rows are
/// the same `SearchMovie`/`SearchTv` shape upstream.
fn search_hit_from(hit: SearchHit) -> TmdbSearchHit {
    let date = non_empty(hit.release_date.or(hit.first_air_date));
    TmdbSearchHit {
        tmdb_id: hit.id,
        name: hit.title.or(hit.name),
        year: year_from(date.as_deref()),
        premiere_date: date,
        poster_url: non_empty(hit.poster_path).map(|p| format!("{IMAGE_BASE}{p}")),
        overview: non_empty(hit.overview),
    }
}

/// One candidate from a TMDB name search (the "Identify" flow).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmdbSearchHit {
    /// The TMDB id.
    pub tmdb_id: i64,
    /// The candidate's title/name.
    pub name: Option<String>,
    /// The release / first-air year.
    pub year: Option<i32>,
    /// The raw release / first-air date (`YYYY-MM-DD`), kept so the Identify
    /// flow can emit `PremiereDate` — the C# providers set
    /// `RemoteSearchResult.PremiereDate` from `ReleaseDate`/`FirstAirDate`, not
    /// just the year.
    pub premiere_date: Option<String>,
    /// The poster image URL (for the result thumbnail).
    pub poster_url: Option<String>,
    /// The plot overview.
    pub overview: Option<String>,
}

/// One image offered by TMDB for an item (the "Choose Image" flow).
#[derive(Debug, Clone, PartialEq)]
pub struct TmdbImage {
    /// Whether this is a poster (Primary) or backdrop.
    pub image_type: ImageType,
    /// The full-resolution image URL.
    pub url: String,
    /// The image width in pixels.
    pub width: Option<i32>,
    /// The image height in pixels.
    pub height: Option<i32>,
    /// The community rating (TMDB `vote_average`).
    pub community_rating: Option<f64>,
    /// The vote count backing the rating.
    pub vote_count: Option<i32>,
    /// The ISO-639-1 language of the image, if tagged.
    pub language: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ImagesResponse {
    #[serde(default)]
    posters: Vec<ImageEntry>,
    #[serde(default)]
    backdrops: Vec<ImageEntry>,
    #[serde(default)]
    logos: Vec<ImageEntry>,
}

#[derive(Debug, Deserialize)]
struct ImageEntry {
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    width: Option<i32>,
    #[serde(default)]
    height: Option<i32>,
    #[serde(default)]
    vote_average: Option<f64>,
    #[serde(default)]
    vote_count: Option<i32>,
    #[serde(default)]
    iso_639_1: Option<String>,
}

/// A matched TV series: its TMDB id (for season/episode follow-up) + poster and
/// backdrop images.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesMatch {
    /// The TMDB series id, used to fetch season/episode artwork.
    pub tmdb_id: i64,
    /// The series' poster/backdrop images.
    pub images: Vec<RemoteImage>,
}

/// One season's metadata + artwork from `/tv/{id}/season/{n}`: the season's
/// name/overview/poster and every episode's metadata, in a single request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeasonDetails {
    /// The season's display name (e.g. "Season 2"), if any.
    pub name: Option<String>,
    /// The season's synopsis, if any.
    pub overview: Option<String>,
    /// The season poster URL, if any.
    pub poster: Option<String>,
    /// The season's episodes, in TMDB order.
    pub episodes: Vec<EpisodeDetails>,
}

/// One episode's metadata within a [`SeasonDetails`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EpisodeDetails {
    /// The episode's own TMDB id, for the `Tmdb` provider id on its row.
    pub tmdb_id: Option<i64>,
    /// The episode number within the season.
    pub episode_number: i32,
    /// The episode title, if any.
    pub name: Option<String>,
    /// The episode synopsis, if any.
    pub overview: Option<String>,
    /// The episode still-frame URL, if any.
    pub still_url: Option<String>,
    /// The original air date (`YYYY-MM-DD`), if any.
    pub air_date: Option<String>,
    /// TMDB's user rating out of 10, if any — the episode's community rating.
    pub vote_average: Option<f32>,
}

/// Full title metadata from `/movie|tv/{id}` (the detail-page fields).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TmdbDetails {
    /// The localized title (`title` for movies, `name` for series) — the C#
    /// provider's `Name = movieResult.Title ?? movieResult.OriginalTitle`.
    pub name: Option<String>,
    /// The original-language title (`original_title` / `original_name`).
    pub original_title: Option<String>,
    /// Plot synopsis.
    pub overview: Option<String>,
    /// Marketing tagline.
    pub tagline: Option<String>,
    /// Genre names.
    pub genres: Vec<String>,
    /// Production-company (studio) names.
    pub studios: Vec<String>,
    /// Community rating (`vote_average`, 0–10), when non-zero.
    pub community_rating: Option<f64>,
    /// US content certification (e.g. `R`, `TV-MA`), when available.
    pub official_rating: Option<String>,
    /// Release/first-air year.
    pub production_year: Option<i32>,
    /// Release/first-air date (`YYYY-MM-DD`).
    pub premiere_date: Option<String>,
    /// Runtime in minutes, when known.
    pub runtime_minutes: Option<i32>,
    /// Cast (billing order) followed by key crew (director/writer/producer).
    pub people: Vec<TmdbPerson>,
    /// YouTube trailers/teasers for the title.
    pub trailers: Vec<TmdbTrailer>,
    /// The IMDb id (`ttNNNNNNN`), when known — the key for an OMDb Rotten
    /// Tomatoes lookup.
    pub imdb_id: Option<String>,
    /// The TVDB id, when TMDB's `external_ids` carry one (series only) — the
    /// third id `MapTvShowToRemoteSearchResult` stamps onto an Identify result.
    pub tvdb_id: Option<String>,
    /// The poster's absolute URL — `TmdbClientManager.GetPosterUrl(PosterPath)`
    /// as the by-id Identify branch uses it.
    pub poster_url: Option<String>,
}

/// One credited person from a title's `credits`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmdbPerson {
    /// The person's TMDB id — the key for a [`person_details`](TmdbClient::person_details)
    /// biography lookup.
    pub tmdb_id: i64,
    /// The person's name.
    pub name: String,
    /// Jellyfin person type: `Actor`, `Director`, `Writer`, `Producer`, …
    pub person_type: String,
    /// The credited role (character for cast, job for crew), when present.
    pub role: Option<String>,
    /// Display order (cast billing order; crew sort last).
    pub sort_order: i32,
    /// The person's profile-photo URL (headshot), when TMDB has one.
    pub profile_url: Option<String>,
}

/// A person's biographical detail, from TMDB `/person/{id}`.
#[derive(Debug, Clone, Default)]
pub struct TmdbPersonDetails {
    /// The biography text.
    pub biography: Option<String>,
    /// The birthday (`YYYY-MM-DD`).
    pub birthday: Option<String>,
    /// The date of death (`YYYY-MM-DD`), when applicable.
    pub deathday: Option<String>,
    /// The place of birth.
    pub place_of_birth: Option<String>,
}

/// One `/search/person` hit — the fields `TmdbPersonProvider.GetSearchResults`
/// maps into a `RemoteSearchResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmdbPersonHit {
    /// The TMDB person id.
    pub tmdb_id: i64,
    /// The person's name.
    pub name: Option<String>,
    /// The profile image URL (`GetProfileUrl(profile_path)`), when present.
    pub profile_url: Option<String>,
    /// The biography (only populated by a by-id lookup, like the C#
    /// `GetPersonAsync` branch).
    pub biography: Option<String>,
    /// The IMDb id from `external_ids` (by-id lookup only).
    pub imdb_id: Option<String>,
}

/// A trailer/video link for a title (name + URL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmdbTrailer {
    /// The video's display name.
    pub name: String,
    /// The absolute (YouTube) URL.
    pub url: String,
}

/// YouTube "Trailer"/"Teaser" videos from a details `videos` append become
/// `RemoteTrailers` (the C# `TmdbMovieProvider` / `TmdbSeriesProvider` rule).
fn youtube_trailers(videos: Option<VideosResults>) -> Vec<TmdbTrailer> {
    videos
        .map(|v| v.results)
        .unwrap_or_default()
        .into_iter()
        .filter(|v| {
            v.site.as_deref() == Some("YouTube")
                && matches!(v.type_.as_deref(), Some("Trailer" | "Teaser"))
        })
        .filter_map(|v| {
            let key = v.key.filter(|k| !k.is_empty())?;
            Some(TmdbTrailer {
                name: v
                    .name
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| "Trailer".to_owned()),
                url: format!("https://www.youtube.com/watch?v={key}"),
            })
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct DetailsResponse {
    /// Movie title.
    #[serde(default)]
    title: Option<String>,
    /// Series name.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    original_title: Option<String>,
    #[serde(default)]
    original_name: Option<String>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    tagline: Option<String>,
    #[serde(default)]
    genres: Vec<NamedEntry>,
    #[serde(default)]
    production_companies: Vec<NamedEntry>,
    /// TV broadcast networks (HBO, AMC, …); empty for movies. Jellyfin's series
    /// "Networks" browse is populated from these, not production companies.
    #[serde(default)]
    networks: Vec<NamedEntry>,
    #[serde(default)]
    vote_average: Option<f64>,
    #[serde(default)]
    runtime: Option<i32>,
    #[serde(default)]
    release_date: Option<String>,
    #[serde(default)]
    first_air_date: Option<String>,
    #[serde(default)]
    poster_path: Option<String>,
    #[serde(default)]
    credits: Option<CreditsResponse>,
    #[serde(default)]
    release_dates: Option<ReleaseDatesResults>,
    #[serde(default)]
    content_ratings: Option<ContentRatingResults>,
    #[serde(default)]
    videos: Option<VideosResults>,
    /// IMDb id — present directly on `/movie/{id}`.
    #[serde(default)]
    imdb_id: Option<String>,
    /// IMDb id for `/tv/{id}` (via `append_to_response=external_ids`).
    #[serde(default)]
    external_ids: Option<ExternalIds>,
}

/// TMDB `external_ids` — the IMDb id (RT lookup key for series) and the TVDB
/// id, which `TmdbSeriesProvider.MapTvShowToRemoteSearchResult` stamps onto an
/// Identify result alongside it.
#[derive(Debug, Default, Deserialize)]
struct ExternalIds {
    #[serde(default)]
    imdb_id: Option<String>,
    #[serde(default)]
    tvdb_id: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct VideosResults {
    #[serde(default)]
    results: Vec<VideoEntry>,
}

#[derive(Debug, Deserialize)]
struct VideoEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    site: Option<String>,
    #[serde(rename = "type", default)]
    type_: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NamedEntry {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreditsResponse {
    #[serde(default)]
    cast: Vec<CastEntry>,
    #[serde(default)]
    crew: Vec<CrewEntry>,
    /// Episode credits only: the people credited as guest stars on THIS
    /// episode (the series regulars come back in `cast`).
    #[serde(default)]
    guest_stars: Vec<CastEntry>,
}

#[derive(Debug, Deserialize)]
struct CastEntry {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    character: Option<String>,
    #[serde(default)]
    profile_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CrewEntry {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    job: Option<String>,
    #[serde(default)]
    profile_path: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ReleaseDatesResults {
    #[serde(default)]
    results: Vec<ReleaseDatesCountry>,
}

#[derive(Debug, Deserialize)]
struct ReleaseDatesCountry {
    #[serde(default)]
    iso_3166_1: Option<String>,
    #[serde(default)]
    release_dates: Vec<ReleaseDatesEntry>,
}

#[derive(Debug, Deserialize)]
struct ReleaseDatesEntry {
    #[serde(default)]
    certification: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ContentRatingResults {
    #[serde(default)]
    results: Vec<ContentRatingCountry>,
}

#[derive(Debug, Deserialize)]
struct ContentRatingCountry {
    #[serde(default)]
    iso_3166_1: Option<String>,
    #[serde(default)]
    rating: Option<String>,
}

/// Maps a TMDB crew `job` to the Jellyfin person type Ferrofin surfaces, or `None`
/// to skip the credit.
fn crew_person_type(job: Option<&str>) -> Option<&'static str> {
    match job? {
        "Director" => Some("Director"),
        "Writer" | "Screenplay" | "Author" | "Novel" => Some("Writer"),
        "Producer" | "Executive Producer" => Some("Producer"),
        _ => None,
    }
}

/// Extracts the US content certification from a movie's `release_dates` or a
/// series' `content_ratings`, preferring the US entry.
fn us_certification(
    kind: TmdbKind,
    release_dates: Option<ReleaseDatesResults>,
    content_ratings: Option<ContentRatingResults>,
) -> Option<String> {
    match kind {
        TmdbKind::Movie => {
            let results = release_dates?.results;
            let us = results
                .iter()
                .find(|c| c.iso_3166_1.as_deref() == Some("US"))?;
            us.release_dates
                .iter()
                .find_map(|r| r.certification.clone().filter(|c| !c.is_empty()))
        }
        TmdbKind::Series => content_ratings?
            .results
            .into_iter()
            .find(|c| c.iso_3166_1.as_deref() == Some("US"))
            .and_then(|c| c.rating.filter(|r| !r.is_empty())),
    }
}

#[derive(Debug, Deserialize)]
struct SeasonResponse {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    poster_path: Option<String>,
    #[serde(default)]
    episodes: Vec<SeasonEpisode>,
}

#[derive(Debug, Deserialize)]
struct SeasonEpisode {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    episode_number: i32,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    still_path: Option<String>,
    #[serde(default)]
    air_date: Option<String>,
    #[serde(default)]
    vote_average: Option<f32>,
}

/// Converts a raw season payload into [`SeasonDetails`], prefixing image paths
/// with the TMDB image base and dropping empty strings. Pure — unit-testable
/// without network.
fn season_details_from(resp: SeasonResponse) -> SeasonDetails {
    let non_empty = |s: Option<String>| s.filter(|v| !v.is_empty());
    SeasonDetails {
        name: non_empty(resp.name),
        overview: non_empty(resp.overview),
        poster: non_empty(resp.poster_path).map(|p| format!("{IMAGE_BASE}{p}")),
        episodes: resp
            .episodes
            .into_iter()
            .map(|ep| EpisodeDetails {
                tmdb_id: ep.id,
                episode_number: ep.episode_number,
                name: non_empty(ep.name),
                overview: non_empty(ep.overview),
                still_url: non_empty(ep.still_path).map(|p| format!("{IMAGE_BASE}{p}")),
                air_date: non_empty(ep.air_date),
                // TMDB reports 0 for "nobody has rated this", which is a
                // missing rating, not a rating of zero.
                vote_average: ep.vote_average.filter(|v| *v > 0.0),
            })
            .collect(),
    }
}

/// `None` for an absent or empty string — TMDB returns `""` as often as `null`.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

/// Normalizes a metadata language for TMDB's `language` parameter — a port of
/// `MediaBrowser.Providers.Plugins.Tmdb.TmdbUtils.NormalizeLanguage`.
///
/// `es-419` (Latin-American Spanish) becomes the closest regional variant TMDB
/// knows; the region half is upper-cased because TMDB's API requires it; and
/// Switzerland (`de-CH`/`fr-CH`/`it-CH`), which TMDB does not carry, degrades to
/// the bare language. Blank in, blank out.
#[must_use]
pub fn normalize_language(language: Option<&str>, country_code: Option<&str>) -> Option<String> {
    let language = language.filter(|l| !l.is_empty())?;
    let mut language = language.to_owned();
    if language.eq_ignore_ascii_case("es-419")
        && let Some(country) = country_code.filter(|c| !c.is_empty())
    {
        language = if country.eq_ignore_ascii_case("AR") {
            "es-AR".to_owned()
        } else {
            "es-MX".to_owned()
        };
    }
    let parts: Vec<&str> = language.split('-').collect();
    if parts.len() == 2 {
        if parts[1].eq_ignore_ascii_case("CH") {
            return Some(parts[0].to_owned());
        }
        return Some(format!("{}-{}", parts[0], parts[1].to_uppercase()));
    }
    Some(language)
}

/// One page of `/movie|tv/{id}/similar`.
#[derive(Debug, Deserialize)]
struct SimilarResponse {
    #[serde(default)]
    results: Vec<SimilarHit>,
    #[serde(default)]
    total_pages: i32,
}

#[derive(Debug, Deserialize)]
struct SimilarHit {
    #[serde(default)]
    id: i64,
}

/// One `/search/collection` result.
#[derive(Debug, Deserialize)]
struct CollectionSearchResponse {
    #[serde(default)]
    results: Vec<CollectionSearchHit>,
}

#[derive(Debug, Deserialize)]
struct CollectionSearchHit {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    poster_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CollectionResponse {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    poster_path: Option<String>,
    #[serde(default)]
    backdrop_path: Option<String>,
    #[serde(default)]
    images: CollectionImages,
}

#[derive(Debug, Default, Deserialize)]
struct CollectionImages {
    #[serde(default)]
    posters: Vec<CollectionImage>,
    #[serde(default)]
    backdrops: Vec<CollectionImage>,
}

#[derive(Debug, Deserialize)]
struct CollectionImage {
    #[serde(default)]
    file_path: Option<String>,
}

/// One TMDB collection search candidate (the box-set Identify row).
#[derive(Debug, Clone)]
pub struct TmdbCollectionHit {
    /// The collection's TMDB id.
    pub tmdb_id: i64,
    /// The collection's name.
    pub name: String,
    /// The collection's overview, when TMDB has one.
    pub overview: Option<String>,
    /// The collection poster's absolute URL.
    pub poster_url: Option<String>,
}

/// One TMDB collection's details plus its artwork.
#[derive(Debug, Clone)]
pub struct TmdbCollection {
    /// The collection's TMDB id.
    pub tmdb_id: i64,
    /// The collection's name.
    pub name: String,
    /// The collection's overview.
    pub overview: Option<String>,
    /// Poster/backdrop candidates, TMDB's own pick first.
    pub images: Vec<RemoteImage>,
}

/// A TMDB artwork client. Cheap to clone (wraps a [`reqwest::Client`]).
#[derive(Debug, Clone)]
pub struct TmdbClient {
    http: reqwest::Client,
    api_key: SecretString,
    /// API root, overridable for tests ([`with_base_url`](TmdbClient::with_base_url)).
    base_url: String,
}

impl Default for TmdbClient {
    fn default() -> Self {
        Self::new()
    }
}

impl TmdbClient {
    /// A client using Jellyfin's built-in API key (zero configuration).
    #[must_use]
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: SecretString::from(DEFAULT_API_KEY),
            base_url: API_BASE.to_owned(),
        }
    }

    /// A client using a user-supplied API key (empty falls back to the built-in).
    #[must_use]
    pub fn with_api_key(key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: SecretString::from(if key.is_empty() {
                DEFAULT_API_KEY.to_owned()
            } else {
                key
            }),
            base_url: API_BASE.to_owned(),
        }
    }

    /// Points the client at a different API root (a mock server in tests).
    #[must_use]
    pub fn with_base_url(mut self, base_url: &str) -> Self {
        base_url
            .trim_end_matches('/')
            .clone_into(&mut self.base_url);
        self
    }

    /// Matches `name`/`year` against TMDB and returns its poster (Primary) and
    /// backdrop image URLs — whichever the best match provides.
    ///
    /// Returns an empty vec on no match or any network/parse error (best-effort:
    /// artwork acquisition must never abort a scan).
    pub async fn images_for(
        &self,
        kind: TmdbKind,
        name: &str,
        year: Option<i32>,
    ) -> Vec<RemoteImage> {
        let path = match kind {
            TmdbKind::Movie => "search/movie",
            TmdbKind::Series => "search/tv",
        };
        // TV uses `first_air_date_year`; movies use `year`.
        let year_param = match kind {
            TmdbKind::Movie => "year",
            TmdbKind::Series => "first_air_date_year",
        };
        let mut req = self
            .http
            .get(format!("{}/{path}", self.base_url))
            .query(&[("api_key", self.api_key.expose_secret()), ("query", name)]);
        if let Some(y) = year {
            req = req.query(&[(year_param, y.to_string())]);
        }

        tracing::debug!(provider = "tmdb", query = name, ?year, "tmdb image search");
        let resp = match req.send().await {
            Ok(resp) => resp,
            // `without_url()` strips the query string — the URL carries the API key.
            Err(e) => {
                tracing::warn!(provider = "tmdb", error = %e.without_url(), "tmdb request failed");
                return Vec::new();
            }
        };
        if !resp.status().is_success() {
            tracing::warn!(provider = "tmdb", status = %resp.status(), "tmdb returned non-success");
            return Vec::new();
        }
        let Ok(parsed) = resp.json::<SearchResponse>().await else {
            tracing::warn!(provider = "tmdb", "tmdb response parse failed");
            return Vec::new();
        };
        let Some(hit) = parsed.results.into_iter().next() else {
            return Vec::new();
        };

        let mut images = Vec::new();
        if let Some(poster) = hit.poster_path.filter(|p| !p.is_empty()) {
            images.push(RemoteImage {
                image_type: ImageType::Primary,
                url: format!("{IMAGE_BASE}{poster}"),
            });
        }
        if let Some(backdrop) = hit.backdrop_path.filter(|p| !p.is_empty()) {
            images.push(RemoteImage {
                image_type: ImageType::Backdrop,
                url: format!("{IMAGE_BASE}{backdrop}"),
            });
        }
        images
    }

    /// Matches a TV series by name/year and returns its TMDB id + poster/backdrop.
    ///
    /// Unlike [`images_for`](Self::images_for) this keeps the id so seasons and
    /// episodes of the same series can be fetched with
    /// [`season_details`](Self::season_details). `None`
    /// on no match or any network/parse error.
    pub async fn series_match(&self, name: &str, year: Option<i32>) -> Option<SeriesMatch> {
        let mut req = self
            .http
            .get(format!("{}/search/tv", self.base_url))
            .query(&[("api_key", self.api_key.expose_secret()), ("query", name)]);
        if let Some(y) = year {
            req = req.query(&[("first_air_date_year", y.to_string())]);
        }
        let resp = req.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let hit = resp
            .json::<SearchResponse>()
            .await
            .ok()?
            .results
            .into_iter()
            .next()?;

        let mut images = Vec::new();
        if let Some(poster) = hit.poster_path.filter(|p| !p.is_empty()) {
            images.push(RemoteImage {
                image_type: ImageType::Primary,
                url: format!("{IMAGE_BASE}{poster}"),
            });
        }
        if let Some(backdrop) = hit.backdrop_path.filter(|p| !p.is_empty()) {
            images.push(RemoteImage {
                image_type: ImageType::Backdrop,
                url: format!("{IMAGE_BASE}{backdrop}"),
            });
        }
        Some(SeriesMatch {
            tmdb_id: hit.id,
            images,
        })
    }

    /// Fetches one season's metadata + artwork (`/tv/{id}/season/{n}`): the
    /// season name/overview/poster and every episode's name/overview/still, in a
    /// single request. `None` on any failure.
    pub async fn season_details(&self, tmdb_id: i64, season_number: i32) -> Option<SeasonDetails> {
        let url = format!("{}/tv/{tmdb_id}/season/{season_number}", self.base_url);
        let resp = self
            .http
            .get(url)
            .query(&[("api_key", self.api_key.expose_secret())])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let parsed = resp.json::<SeasonResponse>().await.ok()?;
        Some(season_details_from(parsed))
    }

    /// Searches TMDB's collections by name (`/search/collection`) — port of
    /// `TmdbClientManager.SearchCollectionAsync`, the box-set half of the
    /// Identify flow. Empty on no match or any error.
    pub async fn search_collection(
        &self,
        name: &str,
        language: Option<&str>,
    ) -> Vec<TmdbCollectionHit> {
        let name = name.trim();
        if name.is_empty() {
            return Vec::new();
        }
        let req = self
            .http
            .get(format!("{}/search/collection", self.base_url))
            .query(&[("api_key", self.api_key.expose_secret()), ("query", name)]);
        let Ok(resp) = with_language(req, language).send().await else {
            return Vec::new();
        };
        if !resp.status().is_success() {
            return Vec::new();
        }
        let Ok(parsed) = resp.json::<CollectionSearchResponse>().await else {
            return Vec::new();
        };
        parsed
            .results
            .into_iter()
            .map(|hit| TmdbCollectionHit {
                tmdb_id: hit.id,
                name: hit.name.unwrap_or_default(),
                overview: non_empty(hit.overview),
                poster_url: non_empty(hit.poster_path).map(|p| format!("{IMAGE_BASE}{p}")),
            })
            .collect()
    }

    /// One collection's details plus its artwork (`/collection/{id}` with
    /// `append_to_response=images`) — port of
    /// `TmdbClientManager.GetCollectionAsync`, which backs both
    /// `TmdbBoxSetProvider` and `TmdbBoxSetImageProvider`.
    pub async fn collection(&self, tmdb_id: i64, language: Option<&str>) -> Option<TmdbCollection> {
        let req = self
            .http
            .get(format!("{}/collection/{tmdb_id}", self.base_url))
            .query(&[
                ("api_key", self.api_key.expose_secret()),
                ("append_to_response", "images"),
            ]);
        let resp = with_language(req, language).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let parsed: CollectionResponse = resp.json().await.ok()?;
        // The single `poster_path`/`backdrop_path` come first (they are TMDB's
        // own pick), then the rest of the `images` lists — same order the C#
        // `ConvertPostersToRemoteImageInfo`/`ConvertBackdrops…` pair yields.
        let mut images = Vec::new();
        let mut push = |path: Option<String>, image_type: ImageType| {
            if let Some(path) = non_empty(path) {
                images.push(RemoteImage {
                    image_type,
                    url: format!("{IMAGE_BASE}{path}"),
                });
            }
        };
        push(parsed.poster_path, ImageType::Primary);
        push(parsed.backdrop_path, ImageType::Backdrop);
        for poster in parsed.images.posters {
            push(poster.file_path, ImageType::Primary);
        }
        for backdrop in parsed.images.backdrops {
            push(backdrop.file_path, ImageType::Backdrop);
        }
        Some(TmdbCollection {
            tmdb_id: parsed.id,
            name: parsed.name.unwrap_or_default(),
            overview: non_empty(parsed.overview),
            images,
        })
    }

    /// Searches TMDB by name/year and returns the candidate list (the "Identify"
    /// flow). `language` is TMDB's `language` parameter, already normalized by
    /// [`normalize_language`]. Empty on no match or any error.
    pub async fn search(
        &self,
        kind: TmdbKind,
        name: &str,
        year: Option<i32>,
        language: Option<&str>,
    ) -> Vec<TmdbSearchHit> {
        let (path, year_param) = match kind {
            TmdbKind::Movie => ("search/movie", "year"),
            TmdbKind::Series => ("search/tv", "first_air_date_year"),
        };
        let mut req = self
            .http
            .get(format!("{}/{path}", self.base_url))
            .query(&[("api_key", self.api_key.expose_secret()), ("query", name)]);
        if let Some(y) = year {
            req = req.query(&[(year_param, y.to_string())]);
        }
        req = with_language(req, language);
        let Ok(resp) = req.send().await else {
            return Vec::new();
        };
        if !resp.status().is_success() {
            return Vec::new();
        }
        let Ok(parsed) = resp.json::<SearchResponse>().await else {
            return Vec::new();
        };
        parsed.results.into_iter().map(search_hit_from).collect()
    }

    /// One page of TMDB's "similar titles" for a movie or series
    /// (`/movie|tv/{id}/similar`) — port of
    /// `TmdbClientManager.GetMovieSimilarPageAsync`/its TV twin.
    ///
    /// Returns the page's TMDB ids and the reported total page count, so the
    /// caller can walk the pages the way the C# provider does. Empty on any
    /// error.
    pub async fn similar_page(&self, kind: TmdbKind, tmdb_id: i64, page: i32) -> (Vec<i64>, i32) {
        let path = match kind {
            TmdbKind::Movie => "movie",
            TmdbKind::Series => "tv",
        };
        let Ok(resp) = self
            .http
            .get(format!("{}/{path}/{tmdb_id}/similar", self.base_url))
            .query(&[
                ("api_key", self.api_key.expose_secret()),
                ("page", &page.max(1).to_string()),
            ])
            .send()
            .await
        else {
            return (Vec::new(), 0);
        };
        if !resp.status().is_success() {
            return (Vec::new(), 0);
        }
        let Ok(parsed) = resp.json::<SimilarResponse>().await else {
            return (Vec::new(), 0);
        };
        (
            parsed.results.into_iter().map(|hit| hit.id).collect(),
            parsed.total_pages,
        )
    }

    /// Lists **all** poster (Primary), backdrop (Backdrop; languaged → Thumb)
    /// and logo (Logo) images TMDB has for a title (the "Choose Image" flow),
    /// via `/movie|tv/{id}/images` — the set `TmdbMovieImageProvider` /
    /// `TmdbSeriesImageProvider.GetImages` return. Empty on any error.
    pub async fn all_images(&self, kind: TmdbKind, tmdb_id: i64) -> Vec<TmdbImage> {
        let path = match kind {
            TmdbKind::Movie => "movie",
            TmdbKind::Series => "tv",
        };
        let Ok(resp) = self
            .http
            .get(format!("{}/{path}/{tmdb_id}/images", self.base_url))
            .query(&[("api_key", self.api_key.expose_secret())])
            .send()
            .await
        else {
            return Vec::new();
        };
        if !resp.status().is_success() {
            return Vec::new();
        }
        let Ok(parsed) = resp.json::<ImagesResponse>().await else {
            return Vec::new();
        };
        let map = |entries: Vec<ImageEntry>, image_type: ImageType| {
            entries.into_iter().filter_map(move |e| {
                let path = e.file_path.filter(|p| !p.is_empty())?;
                let language = e.iso_639_1.filter(|l| !l.is_empty());
                // A backdrop with a language carries text — C#
                // `ConvertToRemoteImageInfo` returns those as `Thumb`.
                let image_type = if image_type == ImageType::Backdrop && language.is_some() {
                    ImageType::Thumb
                } else {
                    image_type
                };
                Some(TmdbImage {
                    image_type,
                    url: format!("{IMAGE_BASE}{path}"),
                    width: e.width,
                    height: e.height,
                    community_rating: e.vote_average,
                    vote_count: e.vote_count,
                    language,
                })
            })
        };
        map(parsed.posters, ImageType::Primary)
            .chain(map(parsed.backdrops, ImageType::Backdrop))
            .chain(map(parsed.logos, ImageType::Logo))
            .collect()
    }

    /// Fetches ONE episode's credited people via
    /// `/tv/{id}/season/{season}/episode/{episode}/credits`.
    ///
    /// Port of `TmdbEpisodeProvider`'s credits handling: the episode's own
    /// `cast` (the regulars credited in THIS episode, in billing order), then
    /// its `guest_stars` (typed `GuestStar`), then the wanted `crew` — which is
    /// what fills an episode page's Cast & Crew upstream.
    ///
    /// `None` on any network/HTTP/parse failure, distinct from `Some(vec![])`
    /// for an episode TMDB genuinely credits nobody on. The caller persists
    /// credits by replacement, so conflating the two lets one 429 during a
    /// large scan delete an episode's stored cast.
    pub async fn episode_credits(
        &self,
        series_tmdb_id: i64,
        season: i32,
        episode: i32,
    ) -> Option<Vec<TmdbPerson>> {
        let url = format!(
            "{}/tv/{series_tmdb_id}/season/{season}/episode/{episode}/credits",
            self.base_url
        );
        let resp = self
            .http
            .get(url)
            .query(&[("api_key", self.api_key.expose_secret())])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let credits = resp.json::<CreditsResponse>().await.ok()?;
        let mut people = Vec::new();
        let mut push_cast = |entries: Vec<CastEntry>, person_type: &str| {
            for (order, c) in entries.into_iter().enumerate() {
                if c.name.is_empty() {
                    continue;
                }
                people.push(TmdbPerson {
                    tmdb_id: c.id,
                    name: c.name,
                    person_type: person_type.to_owned(),
                    role: c.character.filter(|r| !r.is_empty()),
                    sort_order: i32::try_from(order).unwrap_or(i32::MAX),
                    profile_url: c
                        .profile_path
                        .filter(|p| !p.is_empty())
                        .map(|p| format!("{IMAGE_BASE}{p}")),
                });
            }
        };
        push_cast(credits.cast, "Actor");
        push_cast(credits.guest_stars, "GuestStar");
        for c in credits.crew {
            let Some(person_type) = crew_person_type(c.job.as_deref()) else {
                continue;
            };
            if c.name.is_empty() {
                continue;
            }
            people.push(TmdbPerson {
                tmdb_id: c.id,
                name: c.name,
                person_type: person_type.to_owned(),
                role: c.job.filter(|r| !r.is_empty()),
                sort_order: i32::MAX,
                profile_url: c
                    .profile_path
                    .filter(|p| !p.is_empty())
                    .map(|p| format!("{IMAGE_BASE}{p}")),
            });
        }
        Some(people)
    }

    /// Resolves a TMDB id from an external id (`/find/{id}?external_source=`)
    /// — port of `TmdbClientManager.FindByExternalIdAsync`, which the movie/
    /// series providers use to honour an IMDb/TVDB id already on the item.
    /// `source` is TMDB's source name (`imdb_id` / `tvdb_id`). `None` on no
    /// match or any error.
    pub async fn find_by_external_id(
        &self,
        kind: TmdbKind,
        source: &str,
        external_id: &str,
        language: Option<&str>,
    ) -> Option<Vec<TmdbSearchHit>> {
        let external_id = external_id.trim();
        if external_id.is_empty() {
            return None;
        }
        let req = self
            .http
            .get(format!("{}/find/{external_id}", self.base_url))
            .query(&[
                ("api_key", self.api_key.expose_secret()),
                ("external_source", source),
            ]);
        let resp = with_language(req, language).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let found = resp.json::<FindResponse>().await.ok()?;
        let hits = match kind {
            TmdbKind::Movie => found.movie_results,
            TmdbKind::Series => found.tv_results,
        };
        // `Some(vec![])` is a real answer, not a failure: the C# providers read
        // `findResult?.MovieResults`/`TvResults` and, once that list exists, do
        // NOT fall back to a name search. `None` is reserved for a request that
        // never produced a payload.
        Some(hits.into_iter().map(search_hit_from).collect())
    }

    /// The single TMDB id an external id resolves to, or `None` — the shape the
    /// refresh path wants (`TmdbMovieProvider`/`TmdbSeriesProvider.GetMetadata`
    /// take `TvResults[0].Id`).
    pub async fn find_id_by_external_id(
        &self,
        kind: TmdbKind,
        source: &str,
        external_id: &str,
    ) -> Option<i64> {
        self.find_by_external_id(kind, source, external_id, None)
            .await?
            .into_iter()
            .next()
            .map(|hit| hit.tmdb_id)
    }

    /// Fetches full metadata for a title (overview, tagline, genres, studios,
    /// rating, certification, premiere date, runtime, and cast + key crew) via
    /// `/movie|tv/{id}?append_to_response=credits,release_dates|content_ratings`.
    /// `None` on any network/parse error.
    pub async fn details(
        &self,
        kind: TmdbKind,
        tmdb_id: i64,
        language: Option<&str>,
    ) -> Option<TmdbDetails> {
        let (path, append) = match kind {
            // The movie Identify branch needs `external_ids` too — `/movie/{id}`
            // carries `imdb_id` directly, so only the series arm asks for it.
            TmdbKind::Movie => ("movie", "credits,release_dates,videos"),
            TmdbKind::Series => ("tv", "credits,content_ratings,videos,external_ids"),
        };
        let req = self
            .http
            .get(format!("{}/{path}/{tmdb_id}", self.base_url))
            .query(&[
                ("api_key", self.api_key.expose_secret()),
                ("append_to_response", append),
            ]);
        let resp = with_language(req, language).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let d = resp.json::<DetailsResponse>().await.ok()?;

        let premiere = d
            .release_date
            .or(d.first_air_date)
            .filter(|s| !s.is_empty());
        let mut people = Vec::new();
        if let Some(credits) = d.credits {
            // Cast: keep TMDB's billing order.
            for (order, c) in credits.cast.into_iter().enumerate() {
                if c.name.is_empty() {
                    continue;
                }
                people.push(TmdbPerson {
                    tmdb_id: c.id,
                    name: c.name,
                    person_type: "Actor".to_owned(),
                    role: c.character.filter(|r| !r.is_empty()),
                    sort_order: i32::try_from(order).unwrap_or(i32::MAX),
                    profile_url: c
                        .profile_path
                        .filter(|p| !p.is_empty())
                        .map(|p| format!("{IMAGE_BASE}{p}")),
                });
            }
            // Crew: only the roles Jellyfin surfaces on the detail page.
            for c in credits.crew {
                let Some(person_type) = crew_person_type(c.job.as_deref()) else {
                    continue;
                };
                if c.name.is_empty() {
                    continue;
                }
                people.push(TmdbPerson {
                    tmdb_id: c.id,
                    name: c.name,
                    person_type: person_type.to_owned(),
                    role: c.job.filter(|r| !r.is_empty()),
                    sort_order: i32::MAX,
                    profile_url: c
                        .profile_path
                        .filter(|p| !p.is_empty())
                        .map(|p| format!("{IMAGE_BASE}{p}")),
                });
            }
        }

        let trailers = youtube_trailers(d.videos);

        let original_title = d
            .original_title
            .or(d.original_name)
            .filter(|s| !s.is_empty());
        let external_ids = d.external_ids;
        Some(TmdbDetails {
            name: d
                .title
                .or(d.name)
                .filter(|s| !s.is_empty())
                .or_else(|| original_title.clone()),
            original_title,
            overview: d.overview.filter(|s| !s.is_empty()),
            tagline: d.tagline.filter(|s| !s.is_empty()),
            genres: d.genres.into_iter().filter_map(|g| g.name).collect(),
            studios: resolve_studios(d.networks, d.production_companies),
            community_rating: d.vote_average.filter(|v| *v > 0.0),
            official_rating: us_certification(kind, d.release_dates, d.content_ratings),
            production_year: year_from(premiere.as_deref()),
            premiere_date: premiere,
            runtime_minutes: d.runtime.filter(|m| *m > 0),
            people,
            trailers,
            imdb_id: d
                .imdb_id
                .or_else(|| external_ids.as_ref().and_then(|e| e.imdb_id.clone()))
                .filter(|s| !s.is_empty()),
            tvdb_id: external_ids
                .and_then(|e| e.tvdb_id)
                .map(|id| id.to_string()),
            poster_url: non_empty(d.poster_path).map(|p| format!("{IMAGE_BASE}{p}")),
        })
    }

    /// Searches TMDB's people by name (`/search/person`) — port of
    /// `TmdbClientManager.SearchPersonAsync`, the "Identify" flow for a
    /// `Person`. Empty on no match or any error.
    pub async fn search_person(&self, name: &str) -> Vec<TmdbPersonHit> {
        let name = name.trim();
        if name.is_empty() {
            return Vec::new();
        }
        let Ok(resp) = self
            .http
            .get(format!("{}/search/person", self.base_url))
            .query(&[("api_key", self.api_key.expose_secret()), ("query", name)])
            .send()
            .await
        else {
            return Vec::new();
        };
        if !resp.status().is_success() {
            return Vec::new();
        }
        let Ok(parsed) = resp.json::<PersonSearchResponse>().await else {
            return Vec::new();
        };
        parsed
            .results
            .into_iter()
            .map(|hit| TmdbPersonHit {
                tmdb_id: hit.id,
                name: non_empty(hit.name),
                profile_url: non_empty(hit.profile_path).map(|p| format!("{IMAGE_BASE}{p}")),
                biography: None,
                imdb_id: None,
            })
            .collect()
    }

    /// Looks a person up by TMDB id with their images + external ids
    /// (`/person/{id}?append_to_response=images,external_ids`) — port of
    /// `TmdbClientManager.GetPersonAsync` as the "Identify" flow's
    /// already-identified branch uses it. `None` on any error.
    pub async fn person_lookup(
        &self,
        tmdb_id: i64,
        language: Option<&str>,
    ) -> Option<TmdbPersonHit> {
        let req = self
            .http
            .get(format!("{}/person/{tmdb_id}", self.base_url))
            .query(&[
                ("api_key", self.api_key.expose_secret()),
                ("append_to_response", "images,external_ids"),
            ]);
        let resp = with_language(req, language).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let p = resp.json::<PersonLookupResponse>().await.ok()?;
        Some(TmdbPersonHit {
            tmdb_id: p.id.unwrap_or(tmdb_id),
            name: non_empty(p.name),
            // `Images.Profiles[0]` — the first profile image.
            profile_url: p
                .images
                .and_then(|i| i.profiles.into_iter().next())
                .and_then(|i| non_empty(i.file_path))
                .map(|path| format!("{IMAGE_BASE}{path}")),
            biography: non_empty(p.biography),
            imdb_id: p.external_ids.and_then(|e| non_empty(e.imdb_id)),
        })
    }

    /// Fetches a person's biography via `/person/{id}`, or `None` on any
    /// network/parse error or when TMDB has no biographical text.
    pub async fn person_details(&self, tmdb_id: i64) -> Option<TmdbPersonDetails> {
        let resp = self
            .http
            .get(format!("{}/person/{tmdb_id}", self.base_url))
            .query(&[("api_key", self.api_key.expose_secret())])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let p = resp.json::<PersonDetailsResponse>().await.ok()?;
        let details = TmdbPersonDetails {
            biography: p.biography.filter(|s| !s.is_empty()),
            birthday: p.birthday.filter(|s| !s.is_empty()),
            deathday: p.deathday.filter(|s| !s.is_empty()),
            place_of_birth: p.place_of_birth.filter(|s| !s.is_empty()),
        };
        // Skip persons with nothing worth storing (keeps re-fetch cheap).
        if details.biography.is_none()
            && details.birthday.is_none()
            && details.place_of_birth.is_none()
        {
            return None;
        }
        Some(details)
    }

    /// Downloads an image URL's bytes, or `None` on any failure.
    pub async fn download(&self, url: &str) -> Option<Vec<u8>> {
        let resp = self.http.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.bytes().await.ok().map(|b| b.to_vec())
    }
}

/// `/find/{external_id}`: the matching movies and TV series. The rows are the
/// same `SearchMovie`/`SearchTv` shape `/search/*` returns, which is why the C#
/// providers map them through the same result builder.
#[derive(Debug, Default, Deserialize)]
struct FindResponse {
    #[serde(default)]
    movie_results: Vec<SearchHit>,
    #[serde(default)]
    tv_results: Vec<SearchHit>,
}

#[derive(Debug, Deserialize)]
struct PersonSearchResponse {
    #[serde(default)]
    results: Vec<PersonSearchHit>,
}

#[derive(Debug, Deserialize)]
struct PersonSearchHit {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    profile_path: Option<String>,
}

/// `/person/{id}` with `images` + `external_ids` appended — the Identify
/// by-id branch.
#[derive(Debug, Deserialize)]
struct PersonLookupResponse {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    biography: Option<String>,
    #[serde(default)]
    images: Option<PersonImages>,
    #[serde(default)]
    external_ids: Option<PersonExternalIds>,
}

#[derive(Debug, Deserialize)]
struct PersonImages {
    #[serde(default)]
    profiles: Vec<PersonProfileImage>,
}

#[derive(Debug, Deserialize)]
struct PersonProfileImage {
    #[serde(default)]
    file_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PersonExternalIds {
    #[serde(default)]
    imdb_id: Option<String>,
}

/// The subset of TMDB `/person/{id}` Ferrofin surfaces on the person page.
#[derive(Debug, Default, Deserialize)]
struct PersonDetailsResponse {
    #[serde(default)]
    biography: Option<String>,
    #[serde(default)]
    birthday: Option<String>,
    #[serde(default)]
    deathday: Option<String>,
    #[serde(default)]
    place_of_birth: Option<String>,
}

/// Resolves an item's studios: for series, its broadcast networks (Jellyfin's
/// "Networks" browse), falling back to production companies when TMDB lists no
/// networks. Movies carry no networks, so they keep their production companies.
fn resolve_studios(networks: Vec<NamedEntry>, companies: Vec<NamedEntry>) -> Vec<String> {
    let networks: Vec<String> = networks.into_iter().filter_map(|c| c.name).collect();
    if networks.is_empty() {
        companies.into_iter().filter_map(|c| c.name).collect()
    } else {
        networks
    }
}

/// The four-digit year from a TMDB `YYYY-MM-DD` date string.
fn year_from(date: Option<&str>) -> Option<i32> {
    date?.get(0..4)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn year_parsed_from_date_prefix() {
        assert_eq!(year_from(Some("2014-10-10")), Some(2014));
        assert_eq!(year_from(Some("")), None);
        assert_eq!(year_from(None), None);
    }

    // An episode page's Cast & Crew comes from the EPISODE's credits: the
    // regulars credited in it (billing order), then its guest stars (typed
    // GuestStar), then the wanted crew — the shape upstream's
    // TmdbEpisodeProvider produces.
    #[tokio::test]
    async fn collection_search_and_details_map_the_box_set_shape() {
        let search = r#"{"results":[
            {"id":2344,"name":"The Matrix Collection","poster_path":"/c.jpg","overview":"Neo."},
            {"id":9,"name":"Other","poster_path":null,"overview":""}
        ]}"#;
        let collection = r#"{"id":2344,"name":"The Matrix Collection","overview":"Neo.",
            "poster_path":"/pick.jpg","backdrop_path":"/back.jpg",
            "images":{"posters":[{"file_path":"/alt.jpg"}],
                      "backdrops":[{"file_path":"/alt-back.jpg"}]}}"#;
        let server = crate::mock_http::MockServer::start(vec![
            ("/search/collection", search.to_owned()),
            ("/collection/", collection.to_owned()),
        ])
        .await;
        let client = TmdbClient::new().with_base_url(&server.base_url);

        let hits = client.search_collection("Matrix", None).await;
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].tmdb_id, 2344);
        assert_eq!(
            hits[0].poster_url.as_deref(),
            Some("https://image.tmdb.org/t/p/original/c.jpg")
        );
        // An empty poster path / overview is `None`, not an empty string.
        assert_eq!(hits[1].poster_url, None);
        assert_eq!(hits[1].overview, None);

        let details = client.collection(2344, None).await.expect("collection");
        assert_eq!(details.name, "The Matrix Collection");
        assert_eq!(details.overview.as_deref(), Some("Neo."));
        // TMDB's own pick first, then the rest of each list.
        let urls: Vec<&str> = details.images.iter().map(|i| i.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://image.tmdb.org/t/p/original/pick.jpg",
                "https://image.tmdb.org/t/p/original/back.jpg",
                "https://image.tmdb.org/t/p/original/alt.jpg",
                "https://image.tmdb.org/t/p/original/alt-back.jpg",
            ]
        );
    }

    #[tokio::test]
    async fn person_search_and_lookup_map_the_identify_shape() {
        let server = crate::mock_http::MockServer::start(vec![
            (
                "/search/person",
                r#"{"results":[{"id":287,"name":"Brad Pitt","profile_path":"/bp.jpg"},{"id":1,"name":"No Photo","profile_path":null}]}"#.to_owned(),
            ),
            (
                "/person/287",
                r#"{"id":287,"name":"Brad Pitt","biography":"An actor.","images":{"profiles":[{"file_path":"/first.jpg"},{"file_path":"/second.jpg"}]},"external_ids":{"imdb_id":"nm0000093"}}"#.to_owned(),
            ),
        ])
        .await;
        let client = TmdbClient::new().with_base_url(&server.base_url);

        let hits = client.search_person("Brad Pitt").await;
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].tmdb_id, 287);
        assert_eq!(hits[0].name.as_deref(), Some("Brad Pitt"));
        assert_eq!(
            hits[0].profile_url.as_deref(),
            Some("https://image.tmdb.org/t/p/original/bp.jpg")
        );
        assert!(hits[1].profile_url.is_none());
        assert!(client.search_person("  ").await.is_empty());

        let person = client.person_lookup(287, None).await.expect("lookup");
        assert_eq!(person.name.as_deref(), Some("Brad Pitt"));
        assert_eq!(person.biography.as_deref(), Some("An actor."));
        assert_eq!(person.imdb_id.as_deref(), Some("nm0000093"));
        assert_eq!(
            person.profile_url.as_deref(),
            Some("https://image.tmdb.org/t/p/original/first.jpg")
        );
    }

    #[test]
    fn normalize_language_matches_tmdb_utils() {
        use super::normalize_language;
        // Blank in, blank out.
        assert_eq!(normalize_language(None, Some("US")), None);
        assert_eq!(normalize_language(Some(""), Some("US")), None);
        // A bare language is untouched.
        assert_eq!(
            normalize_language(Some("en"), Some("US")).as_deref(),
            Some("en")
        );
        // The region half is upper-cased — TMDB's API requires it.
        assert_eq!(
            normalize_language(Some("pt-br"), Some("BR")).as_deref(),
            Some("pt-BR")
        );
        // Switzerland is not a TMDB region: degrade to the bare language.
        assert_eq!(
            normalize_language(Some("de-CH"), Some("CH")).as_deref(),
            Some("de")
        );
        assert_eq!(
            normalize_language(Some("fr-ch"), None).as_deref(),
            Some("fr")
        );
        // es-419 maps to the closest regional variant TMDB knows.
        assert_eq!(
            normalize_language(Some("es-419"), Some("AR")).as_deref(),
            Some("es-AR")
        );
        assert_eq!(
            normalize_language(Some("es-419"), Some("MX")).as_deref(),
            Some("es-MX")
        );
        // …but only when a country code is supplied.
        assert_eq!(
            normalize_language(Some("es-419"), None).as_deref(),
            Some("es-419")
        );
    }

    #[tokio::test]
    async fn find_by_external_id_picks_the_kinds_result_list() {
        let server = crate::mock_http::MockServer::start(vec![(
            "/find/tt0133093",
            r#"{"movie_results":[{"id":603,"title":"The Matrix","release_date":"1999-03-31"}],"tv_results":[{"id":1}]}"#.to_owned(),
        )])
        .await;
        let client = TmdbClient::new().with_base_url(&server.base_url);
        let movie = client
            .find_by_external_id(TmdbKind::Movie, "imdb_id", "tt0133093", None)
            .await
            .expect("movie find answered");
        assert_eq!(
            movie
                .iter()
                .map(|hit| (
                    hit.tmdb_id,
                    hit.name.as_deref(),
                    hit.premiere_date.as_deref()
                ))
                .collect::<Vec<_>>(),
            vec![(603, Some("The Matrix"), Some("1999-03-31"))]
        );
        assert_eq!(
            client
                .find_id_by_external_id(TmdbKind::Series, "imdb_id", "tt0133093")
                .await,
            Some(1)
        );
        // An answered `/find` with no rows is `Some(vec![])`, not a failure —
        // the C# providers stop there rather than falling back to a name
        // search. A blank id makes no request at all.
        assert_eq!(
            client
                .find_by_external_id(TmdbKind::Movie, "imdb_id", "tt0", None)
                .await,
            Some(Vec::new())
        );
        assert!(
            client
                .find_by_external_id(TmdbKind::Movie, "imdb_id", " ", None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_empty_collection_search_term_makes_no_request() {
        let client = TmdbClient::new().with_base_url("http://127.0.0.1:1");
        assert!(client.search_collection("  ", None).await.is_empty());
    }

    #[tokio::test]
    async fn similar_pages_yield_their_ids_and_page_count() {
        let body = r#"{"page":1,"total_pages":3,"results":[{"id":603},{"id":604}]}"#;
        let server = crate::mock_http::MockServer::start(vec![("/similar", body.to_owned())]).await;
        let client = TmdbClient::new().with_base_url(&server.base_url);
        let (ids, total) = client.similar_page(TmdbKind::Movie, 27205, 1).await;
        assert_eq!(ids, [603, 604]);
        assert_eq!(total, 3);
        // The TV path is the same shape.
        let (ids, _) = client.similar_page(TmdbKind::Series, 1396, 1).await;
        assert_eq!(ids, [603, 604]);
    }

    #[tokio::test]
    async fn a_failed_similar_request_yields_no_ids() {
        // Nothing listening: the client must degrade, not error.
        let client = TmdbClient::new().with_base_url("http://127.0.0.1:1");
        assert_eq!(
            client.similar_page(TmdbKind::Movie, 1, 1).await,
            (vec![], 0)
        );
        assert!(client.collection(1, None).await.is_none());
    }

    #[tokio::test]
    async fn episode_credits_map_cast_guests_and_crew() {
        use crate::mock_http::MockServer;

        let body = r#"{
          "cast": [
            {"id": 1, "name": "Regular One", "character": "Hero", "profile_path": "/r1.jpg"},
            {"id": 2, "name": "Regular Two", "character": "Sidekick"}
          ],
          "guest_stars": [
            {"id": 3, "name": "Guest Star", "character": "Villain", "profile_path": "/g.jpg"}
          ],
          "crew": [
            {"id": 4, "name": "Ep Director", "job": "Director"},
            {"id": 5, "name": "Ep Writer", "job": "Screenplay"},
            {"id": 6, "name": "Best Boy", "job": "Best Boy"},
            {"id": 7, "name": "", "job": "Director"}
          ]
        }"#;
        let server = MockServer::start(vec![("/credits", body.to_owned())]).await;
        let client = TmdbClient::new().with_base_url(&server.base_url);

        let people = client
            .episode_credits(1399, 1, 1)
            .await
            .expect("credits fetched");
        let rows: Vec<(&str, &str, Option<&str>)> = people
            .iter()
            .map(|p| (p.name.as_str(), p.person_type.as_str(), p.role.as_deref()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("Regular One", "Actor", Some("Hero")),
                ("Regular Two", "Actor", Some("Sidekick")),
                ("Guest Star", "GuestStar", Some("Villain")),
                ("Ep Director", "Director", Some("Director")),
                ("Ep Writer", "Writer", Some("Screenplay")),
            ],
            "unwanted crew jobs and blank names are dropped"
        );
        // Billing order is preserved for cast, and headshots are absolute URLs.
        assert_eq!(people[0].sort_order, 0);
        assert_eq!(people[1].sort_order, 1);
        assert!(
            people[0]
                .profile_url
                .as_deref()
                .is_some_and(|u| u.ends_with("/r1.jpg"))
        );
        assert_eq!(people[1].profile_url, None);
    }

    #[test]
    fn crew_jobs_map_to_person_types() {
        assert_eq!(crew_person_type(Some("Director")), Some("Director"));
        assert_eq!(crew_person_type(Some("Screenplay")), Some("Writer"));
        assert_eq!(
            crew_person_type(Some("Executive Producer")),
            Some("Producer")
        );
        assert_eq!(crew_person_type(Some("Gaffer")), None);
        assert_eq!(crew_person_type(None), None);
    }

    #[test]
    fn us_certification_prefers_us_entry() {
        let rd = ReleaseDatesResults {
            results: vec![
                ReleaseDatesCountry {
                    iso_3166_1: Some("GB".to_owned()),
                    release_dates: vec![ReleaseDatesEntry {
                        certification: Some("15".to_owned()),
                    }],
                },
                ReleaseDatesCountry {
                    iso_3166_1: Some("US".to_owned()),
                    release_dates: vec![ReleaseDatesEntry {
                        certification: Some("R".to_owned()),
                    }],
                },
            ],
        };
        assert_eq!(
            us_certification(TmdbKind::Movie, Some(rd), None).as_deref(),
            Some("R")
        );
        let cr = ContentRatingResults {
            results: vec![ContentRatingCountry {
                iso_3166_1: Some("US".to_owned()),
                rating: Some("TV-MA".to_owned()),
            }],
        };
        assert_eq!(
            us_certification(TmdbKind::Series, None, Some(cr)).as_deref(),
            Some("TV-MA")
        );
        assert_eq!(us_certification(TmdbKind::Movie, None, None), None);
    }

    #[test]
    fn empty_user_key_falls_back_to_builtin() {
        let c = TmdbClient::with_api_key(String::new());
        assert_eq!(c.api_key.expose_secret(), DEFAULT_API_KEY);
        let c = TmdbClient::with_api_key("mykey".to_owned());
        assert_eq!(c.api_key.expose_secret(), "mykey");
    }

    #[test]
    fn season_response_converts_to_details() {
        let parsed: SeasonResponse = serde_json::from_str(
            r#"{
                "name": "Season 2",
                "overview": "The second season.",
                "poster_path": "/s2.jpg",
                "episodes": [
                    { "episode_number": 1, "name": "Hello", "overview": "Ep one.",
                      "still_path": "/e1.jpg" },
                    { "episode_number": 2, "name": "", "overview": null,
                      "still_path": null }
                ]
            }"#,
        )
        .expect("parse");
        let details = season_details_from(parsed);
        assert_eq!(details.name.as_deref(), Some("Season 2"));
        assert_eq!(details.overview.as_deref(), Some("The second season."));
        assert_eq!(
            details.poster.as_deref(),
            Some("https://image.tmdb.org/t/p/original/s2.jpg")
        );
        assert_eq!(details.episodes.len(), 2);
        let ep1 = &details.episodes[0];
        assert_eq!(ep1.episode_number, 1);
        assert_eq!(ep1.name.as_deref(), Some("Hello"));
        assert_eq!(ep1.overview.as_deref(), Some("Ep one."));
        assert_eq!(
            ep1.still_url.as_deref(),
            Some("https://image.tmdb.org/t/p/original/e1.jpg")
        );
        // Empty strings and nulls collapse to None.
        let ep2 = &details.episodes[1];
        assert_eq!(ep2.name, None);
        assert_eq!(ep2.overview, None);
        assert_eq!(ep2.still_url, None);
    }
}
