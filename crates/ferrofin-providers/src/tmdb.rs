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

use crate::rate_limit::CountedBody as _;
use crate::rate_limit::{LimitedRequest as _, RateLimiter};
use ferrofin_model::entities::ImageType;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

/// The TMDB v3 REST base.
const API_BASE: &str = "https://api.themoviedb.org/3";

/// Maps the `ImageType` a `/images` entry was listed under to the TMDb
/// settings-page size that governs it.
///
/// `ConvertPostersToRemoteImageInfo` / `…Backdrops…` / `…Logos…` each pass their
/// own `Plugin.Instance.Configuration.*Size` into the shared `GetUrl`
/// (v10.11.8 `TmdbClientManager.cs:554-572`). A languaged backdrop is remapped
/// to `Thumb` by the caller AFTER the URL is built there, so it keeps the
/// backdrop size — hence `Thumb` maps to `Backdrop` here rather than to a size
/// of its own.
fn image_type_size(image_type: ImageType) -> crate::plugin_config::TmdbImageKind {
    use crate::plugin_config::TmdbImageKind;
    match image_type {
        ImageType::Backdrop | ImageType::Thumb => TmdbImageKind::Backdrop,
        ImageType::Logo => TmdbImageKind::Logo,
        _ => TmdbImageKind::Poster,
    }
}
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteImage {
    /// The image type (Primary/Backdrop/…).
    pub image_type: ImageType,
    /// The absolute CDN URL of the image.
    pub url: String,
    /// Provider-reported width; absent dimensions do not fail a minimum-width check.
    pub width: Option<i32>,
    /// Language used when preferring untagged backdrops.
    pub language: Option<String>,
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
fn search_hit_from(hit: SearchHit, cfg: &crate::plugin_config::TmdbConfig) -> TmdbSearchHit {
    let date = non_empty(hit.release_date.or(hit.first_air_date));
    TmdbSearchHit {
        tmdb_id: hit.id,
        name: hit.title.or(hit.name),
        year: year_from(date.as_deref()),
        premiere_date: date,
        poster_url: non_empty(hit.poster_path)
            .map(|p| cfg.image_url(crate::plugin_config::TmdbImageKind::Poster, &p)),
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

impl From<TmdbImage> for RemoteImage {
    fn from(image: TmdbImage) -> Self {
        Self {
            image_type: image.image_type,
            url: image.url,
            width: image.width,
            language: image.language,
        }
    }
}

/// Ranks rich images by the same language/rating/vote preference used by the
/// remote-image chooser, retaining dimensions for automatic acquisition.
#[must_use]
pub fn ordered_download_images(mut images: Vec<TmdbImage>, requested: &str) -> Vec<RemoteImage> {
    crate::provider_manager::order_tmdb_images(&mut images, requested);
    images.into_iter().map(RemoteImage::from).collect()
}

#[derive(Debug, Deserialize)]
struct ImagesResponse {
    #[serde(default)]
    posters: Vec<ImageEntry>,
    #[serde(default)]
    backdrops: Vec<ImageEntry>,
    #[serde(default)]
    logos: Vec<ImageEntry>,
    /// Episode stills (`/tv/{id}/season/{n}/episode/{m}/images`). C#
    /// `ConvertStillsToRemoteImageInfo` maps them to `Primary`, which is what
    /// an episode's poster slot actually holds.
    #[serde(default)]
    stills: Vec<ImageEntry>,
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

/// One season's metadata + artwork from `/tv/{id}/season/{n}` with its
/// `credits` and `external_ids` appended: everything `TmdbSeasonProvider`
/// maps for the season itself, its poster, and every episode's metadata, in
/// a single request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeasonDetails {
    /// The season's own TMDB id (`seasonResult.Id`), which
    /// `TmdbSeasonProvider` sets as the season's `Tmdb` provider id
    /// (`TmdbSeasonProvider.cs:81`).
    pub tmdb_id: Option<i64>,
    /// The season's display name (e.g. "Season 2"), if any.
    pub name: Option<String>,
    /// The season's synopsis, if any.
    pub overview: Option<String>,
    /// The season's air date (`YYYY-MM-DD`): its premiere date and year
    /// (`TmdbSeasonProvider.cs:72-73`).
    pub air_date: Option<String>,
    /// The season's TheTVDB id, from its `external_ids`
    /// (`TmdbSeasonProvider.cs:82`).
    pub tvdb_id: Option<String>,
    /// The season's credits — its cast in billing order, then the wanted
    /// crew — under the TMDb settings page's caps and profile filters
    /// (`TmdbSeasonProvider.cs:84-155`). Empty when TMDB credits nobody.
    pub people: Vec<TmdbPerson>,
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
    /// The title's original language, TMDB's ISO 639-1 `original_language`
    /// (`ja`, `en`, …) — what `BaseItems.OriginalLanguage` stores.
    pub original_language: Option<String>,
    /// Keyword tags attached to this movie or series.
    pub tags: Vec<String>,
    /// Movie production-country names.
    pub production_locations: Vec<String>,
    /// Series home page.
    pub home_page_url: Option<String>,
    /// Series last air date.
    pub end_date: Option<String>,
    /// The movie collection's TMDB id.
    pub collection_id: Option<i64>,
    /// The movie collection's name.
    pub collection_name: Option<String>,
    /// The series' TVRage id.
    pub tvrage_id: Option<String>,
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
    /// The series' TMDB status name (`Returning Series`, `Ended`, `Canceled`,
    /// …) — what `TmdbSeriesProvider` folds onto `Series.Status` through
    /// `TvParserHelpers.TryParseSeriesStatus`. Series only: a movie's
    /// release status (`Released`) is not carried.
    pub status: Option<String>,
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
    /// The credit's `SortOrder`: the cast member's billing `order`
    /// (`SortOrder = actor.Order`, `TmdbMovieProvider.cs:299`,
    /// `TmdbEpisodeProvider.cs:233,264`); `None` for crew, which upstream
    /// gives none.
    pub sort_order: Option<i32>,
    /// The person's profile-photo URL (headshot), when TMDB has one.
    pub profile_url: Option<String>,
}

/// TheMovieDb's credited people as persistable [`PeopleEntity`] rows, keeping
/// TMDB's person id (which keys the biography/headshot enrichment) and each
/// credit's `SortOrder`.
///
/// [`PeopleEntity`]: ferrofin_db::entities::base_items::PeopleEntity
#[must_use]
pub fn people_entities(
    people: &[TmdbPerson],
) -> Vec<ferrofin_db::entities::base_items::PeopleEntity> {
    people
        .iter()
        .map(|p| ferrofin_db::entities::base_items::PeopleEntity {
            id: ferrofin_db::store::guid_to_db(uuid::Uuid::new_v4()),
            name: p.name.clone(),
            person_type: Some(p.person_type.clone()),
            role: p.role.clone(),
            primary_image_url: p.profile_url.clone(),
            provider_id: Some(p.tmdb_id),
            sort_order: p.sort_order.map(i64::from),
        })
        .collect()
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
    homepage: Option<String>,
    last_air_date: Option<String>,
    episode_run_time: Option<Vec<i32>>,
    production_countries: Option<Vec<NamedEntry>>,
    keywords: Option<KeywordsResponse>,
    belongs_to_collection: Option<CollectionRef>,
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
    original_language: Option<String>,
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
    /// The series' production status (`Returning Series`, `Ended`,
    /// `Canceled`, `In Production`, …); movies carry a release status here
    /// (`Released`) that is ignored.
    #[serde(default)]
    status: Option<String>,
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

/// Episode fields only available on its detail endpoint.
#[derive(Debug)]
pub struct EpisodeExtras {
    /// IMDb, TVDB and TVRage identities for this episode.
    pub provider_ids: Vec<(String, String)>,
    /// Episode trailers.
    pub trailers: Vec<TmdbTrailer>,
    /// Credits, absent when the appended credits payload was unavailable.
    pub people: Option<Vec<TmdbPerson>>,
}

fn episode_people_from(
    credits: CreditsResponse,
    cfg: &crate::plugin_config::TmdbConfig,
) -> Vec<TmdbPerson> {
    let mut people = Vec::new();
    // `TmdbEpisodeProvider` applies `HideMissingCastMembers` +
    // `MaxCastMembers` to the cast and to the guest stars SEPARATELY (two
    // `.Take(config.MaxCastMembers)` loops, `:212-267`), so the cap is
    // per-list here too.
    let hide_cast = cfg.hide_missing_cast_members;
    let max_cast = cfg.max_cast_members;
    let mut push_cast = |entries: Vec<CastEntry>, person_type: &str| {
        for c in billed_cast(entries, hide_cast, max_cast) {
            if c.name.is_empty() {
                continue;
            }
            people.push(TmdbPerson {
                tmdb_id: c.id,
                name: c.name,
                person_type: person_type.to_owned(),
                role: c.character.filter(|r| !r.is_empty()),
                // `SortOrder = actor.Order` / `guest.Order`
                // (`TmdbEpisodeProvider.cs:233,264`).
                sort_order: Some(c.order),
                profile_url: c
                    .profile_path
                    .filter(|p| !p.is_empty())
                    .map(|p| cfg.image_url(crate::plugin_config::TmdbImageKind::Profile, &p)),
            });
        }
    };
    push_cast(credits.cast, "Actor");
    push_cast(credits.guest_stars, "GuestStar");
    for c in credits
        .crew
        .into_iter()
        .filter(|c| crew_person_type(c.job.as_deref()).is_some())
        .filter(|c| {
            !cfg.hide_missing_crew_members
                || c.profile_path.as_deref().is_some_and(|p| !p.is_empty())
        })
        .take(cfg.max_crew_members)
    {
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
            sort_order: None,
            profile_url: c
                .profile_path
                .filter(|p| !p.is_empty())
                .map(|p| cfg.image_url(crate::plugin_config::TmdbImageKind::Profile, &p)),
        });
    }
    people
}

/// TMDB `external_ids` — the IMDb id (RT lookup key for series) and the TVDB
/// id, which `TmdbSeriesProvider.MapTvShowToRemoteSearchResult` stamps onto an
/// Identify result alongside it.
// Field names mirror TMDB's external_ids payload.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Default, Deserialize)]
struct ExternalIds {
    tvrage_id: Option<i64>,
    #[serde(default)]
    imdb_id: Option<String>,
    #[serde(default)]
    tvdb_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct CollectionRef {
    id: Option<i64>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KeywordsResponse {
    // Movies use `keywords`; series use `results`.
    #[serde(default, alias = "results")]
    keywords: Vec<NamedEntry>,
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
    /// TMDB's billing order (TMDbLib's `Cast.Order`, an `int`: 0 when the
    /// payload leaves it out). Upstream orders the cast by it and stores it
    /// as the credit's `SortOrder`.
    #[serde(default)]
    order: i32,
}

/// `castQuery.OrderBy(a => a.Order)` after the settings page's
/// `HideMissingCastMembers` filter, then `.Take(MaxCastMembers)`
/// (`TmdbMovieProvider.cs:280-286`, `TmdbEpisodeProvider.cs:217-219,248-250`):
/// the entries a cast list keeps, in billing order (a stable sort, as LINQ's).
fn billed_cast(mut entries: Vec<CastEntry>, hide_missing: bool, max: usize) -> Vec<CastEntry> {
    entries.retain(|c| !hide_missing || c.profile_path.as_deref().is_some_and(|p| !p.is_empty()));
    entries.sort_by_key(|c| c.order);
    entries.truncate(max);
    entries
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

/// Maps a title's `credits` block to [`TmdbPerson`]s under the TMDb settings
/// page's cast/crew caps and profile filters.
///
/// Shared by the movie and series arms of [`TmdbClient::details`], which are one
/// request here and two providers upstream (`TmdbMovieProvider.GetMetadata` /
/// `TmdbSeriesProvider.GetPersons`) running the identical LINQ.
fn credits_to_people(
    credits: Option<CreditsResponse>,
    cfg: &crate::plugin_config::TmdbConfig,
) -> Vec<TmdbPerson> {
    let mut people = Vec::new();
    if let Some(credits) = credits {
        // Cast: the TMDb settings page's `HideMissingCastMembers` filter,
        // TMDB's billing order and the `MaxCastMembers` cap
        // (`TmdbMovieProvider.cs:280-286`); each credit keeps its own
        // `order` as its `SortOrder` (`SortOrder = actor.Order`, `:299`).
        for c in billed_cast(
            credits.cast,
            cfg.hide_missing_cast_members,
            cfg.max_cast_members,
        ) {
            if c.name.is_empty() {
                continue;
            }
            people.push(TmdbPerson {
                tmdb_id: c.id,
                name: c.name,
                person_type: "Actor".to_owned(),
                role: c.character.filter(|r| !r.is_empty()),
                sort_order: Some(c.order),
                profile_url: c
                    .profile_path
                    .filter(|p| !p.is_empty())
                    .map(|p| cfg.image_url(crate::plugin_config::TmdbImageKind::Profile, &p)),
            });
        }
        // Crew: only the roles Jellyfin surfaces on the detail page, then
        // the settings page's `HideMissingCrewMembers` filter and
        // `MaxCrewMembers` cap. Upstream applies both AFTER the
        // wanted-kind filter, so the cap counts kept crew, not raw credits.
        for c in credits
            .crew
            .into_iter()
            .filter(|c| crew_person_type(c.job.as_deref()).is_some())
            .filter(|c| {
                !cfg.hide_missing_crew_members
                    || c.profile_path.as_deref().is_some_and(|p| !p.is_empty())
            })
            .take(cfg.max_crew_members)
        {
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
                sort_order: None,
                profile_url: c
                    .profile_path
                    .filter(|p| !p.is_empty())
                    .map(|p| cfg.image_url(crate::plugin_config::TmdbImageKind::Profile, &p)),
            });
        }
    }
    people
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

/// Requested-country certification, with Jellyfin's US fallback and prefix.
fn certification(
    kind: TmdbKind,
    release_dates: Option<ReleaseDatesResults>,
    content_ratings: Option<ContentRatingResults>,
    country: Option<&str>,
) -> Option<String> {
    let country = country.filter(|c| !c.is_empty()).unwrap_or("US");
    let ratings: Vec<(String, String)> = match kind {
        TmdbKind::Movie => release_dates?
            .results
            .into_iter()
            .filter_map(|entry| {
                let rating = entry
                    .release_dates
                    .into_iter()
                    .find_map(|r| r.certification.filter(|c| !c.trim().is_empty()))?;
                Some((entry.iso_3166_1?, rating))
            })
            .collect(),
        TmdbKind::Series => content_ratings?
            .results
            .into_iter()
            .filter_map(|entry| {
                Some((
                    entry.iso_3166_1?,
                    entry.rating.filter(|r| !r.trim().is_empty())?,
                ))
            })
            .collect(),
    };
    if let Some((_, rating)) = ratings
        .iter()
        .find(|(code, _)| code.eq_ignore_ascii_case(country))
    {
        let prefix = if country.eq_ignore_ascii_case("US") {
            String::new()
        } else if country.eq_ignore_ascii_case("DE") {
            "FSK-".to_owned()
        } else {
            format!("{country}-")
        };
        return Some(format!("{prefix}{rating}"));
    }
    ratings
        .into_iter()
        .find(|(code, _)| code.eq_ignore_ascii_case("US"))
        .map(|(_, rating)| rating)
}

#[derive(Debug, Deserialize)]
struct SeasonResponse {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    air_date: Option<String>,
    #[serde(default)]
    poster_path: Option<String>,
    #[serde(default)]
    credits: Option<CreditsResponse>,
    #[serde(default)]
    external_ids: Option<ExternalIds>,
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
fn season_details_from(
    resp: SeasonResponse,
    cfg: &crate::plugin_config::TmdbConfig,
) -> SeasonDetails {
    let non_empty = |s: Option<String>| s.filter(|v| !v.is_empty());
    SeasonDetails {
        tmdb_id: resp.id.filter(|id| *id > 0),
        name: non_empty(resp.name),
        overview: non_empty(resp.overview),
        air_date: non_empty(resp.air_date),
        tvdb_id: resp
            .external_ids
            .and_then(|ids| ids.tvdb_id)
            .filter(|id| *id > 0)
            .map(|id| id.to_string()),
        // `TmdbSeasonProvider` runs the same cast and crew LINQ as the movie
        // and series providers (`:84-155`): billed cast under
        // `HideMissingCastMembers`/`MaxCastMembers`, then the wanted crew
        // under `HideMissingCrewMembers`/`MaxCrewMembers`.
        people: credits_to_people(resp.credits, cfg),
        poster: non_empty(resp.poster_path)
            .map(|p| cfg.image_url(crate::plugin_config::TmdbImageKind::Poster, &p)),
        episodes: resp
            .episodes
            .into_iter()
            .map(|ep| EpisodeDetails {
                tmdb_id: ep.id,
                episode_number: ep.episode_number,
                name: non_empty(ep.name),
                overview: non_empty(ep.overview),
                still_url: non_empty(ep.still_path)
                    .map(|p| cfg.image_url(crate::plugin_config::TmdbImageKind::Still, &p)),
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

/// `TmdbUtils.GetImageLanguagesParam` at pin4910aafa1a: the normalized preferred
/// language, `null`, and `en` as the final fallback —
/// comma separated (`"fr,null,en"`, `"en,null"`).
///
/// `TmdbMovieProvider` hands exactly this string to `FindByExternalIdAsync`'s
/// `language` argument (`TmdbMovieProvider.cs:96-101` and `:106-111` at v10.11.8), so
/// TMDB's `/find` sees `language=en,null` for a movie. `TmdbSeriesProvider.cs:73` passes
/// the bare `MetadataLanguage` instead. That asymmetry is upstream's, and it is ported
/// rather than smoothed over: `/find` is the branch an IMDb/TVDB Identify search takes,
/// and sending TMDB a different `language` than the oracle does is how the two servers
/// would drift apart on a localized title.
#[must_use]
pub fn image_languages_param(language: Option<&str>, country_code: Option<&str>) -> String {
    let preferred = normalize_language(language, country_code).unwrap_or_default();
    let mut languages = Vec::new();
    if !preferred.is_empty() {
        languages.push(preferred.clone());
    }
    languages.push("null".to_owned());
    // English is always the final fallback (and a blank preference is not "en", so it
    // still gets one — the C# yields `"null,en"` there, not `"null"`).
    if !preferred.eq_ignore_ascii_case("en") {
        languages.push("en".to_owned());
    }
    languages.join(",")
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
    #[serde(default)]
    width: Option<i32>,
    #[serde(default)]
    height: Option<i32>,
    #[serde(default)]
    iso_639_1: Option<String>,
    #[serde(default)]
    vote_average: Option<f64>,
    #[serde(default)]
    vote_count: Option<i32>,
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

/// A TMDB artwork client. Held behind an `Arc` at the call site.
#[derive(Debug)]
pub struct TmdbClient {
    http: reqwest::Client,
    limiter: RateLimiter,
    /// The key to use when the TMDb settings page names none: Jellyfin's
    /// built-in project key, or one the operator passed to
    /// [`with_api_key`](TmdbClient::with_api_key).
    api_key: SecretString,
    /// API root, overridable for tests ([`with_base_url`](TmdbClient::with_base_url)).
    base_url: String,
    /// The TMDb plugin's dashboard settings (`TmdbApiKey`, `IncludeAdult`, the
    /// cast/crew caps, the five image sizes).
    plugin: crate::plugin_config::ConfigSource,
    /// Image root replacing TMDb's CDN, for tests
    /// ([`with_image_root`](TmdbClient::with_image_root)).
    image_root: Option<String>,
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
            limiter: RateLimiter::new("tmdb"),
            api_key: SecretString::from(DEFAULT_API_KEY),
            base_url: API_BASE.to_owned(),
            plugin: crate::plugin_config::ConfigSource::new(),
            image_root: None,
        }
    }

    /// A client using a user-supplied API key (empty falls back to the built-in).
    #[must_use]
    pub fn with_api_key(key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            limiter: RateLimiter::new("tmdb"),
            api_key: SecretString::from(if key.is_empty() {
                DEFAULT_API_KEY.to_owned()
            } else {
                key
            }),
            base_url: API_BASE.to_owned(),
            plugin: crate::plugin_config::ConfigSource::new(),
            image_root: None,
        }
    }

    /// Binds the plugin manager, making the TMDb settings page live. Called
    /// once by the composition root.
    pub fn attach_plugin_manager(
        &self,
        plugins: std::sync::Arc<dyn ferrofin_traits::plugins::PluginManager>,
    ) {
        self.plugin.attach(plugins);
    }

    /// The TMDb plugin's settings for this call.
    ///
    /// Read per call, the way `Plugin.Instance.Configuration` is: an admin
    /// saving the settings page changes the next lookup, with no restart. The
    /// read is a few hundred bytes off disk in front of a TMDB round trip.
    pub(crate) async fn settings(&self) -> crate::plugin_config::TmdbConfig {
        let mut cfg: crate::plugin_config::TmdbConfig =
            self.plugin.load(crate::builtin_plugins::TMDB.id).await;
        cfg.image_root.clone_from(&self.image_root);
        cfg
    }

    /// The TMDb settings page's `ImportSeasonName`: whether
    /// `TmdbSeasonProvider` names a season after TMDB's season name
    /// (`TmdbSeasonProvider.cs:76-79`; off by default, so a season keeps
    /// the name its folder or the season-zero setting gives it).
    pub async fn import_season_name(&self) -> bool {
        self.settings().await.import_season_name
    }

    /// Points the artwork URLs at a different image root than TMDb's CDN
    /// (`https://image.tmdb.org/t/p`) — a mock server in tests; the size
    /// segment and the image path follow it.
    #[must_use]
    pub fn with_image_root(mut self, root: &str) -> Self {
        self.image_root = Some(root.trim_end_matches('/').to_owned());
        self
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
        self.images_for_language(kind, name, year, None).await
    }

    /// Finds title artwork using a resolved metadata language.
    pub async fn images_for_language(
        &self,
        kind: TmdbKind,
        name: &str,
        year: Option<i32>,
        language: Option<&str>,
    ) -> Vec<RemoteImage> {
        // The TMDb settings page governs the key, the adult filter, the
        // cast/crew caps and the image sizes; read per call, the way
        // `Plugin.Instance.Configuration` is (`TmdbClientManager`).
        let cfg = self.settings().await;
        let key = cfg.api_key(self.api_key.expose_secret());
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
            .query(&[("api_key", key), ("query", name)])
            // `include_adult` is `Plugin.Instance.Configuration.IncludeAdult`
            // on every TMDb search upstream (`TmdbClientManager.cs:396,424,467`);
            // the API's own default is `false`, which is also the plugin's.
            .query(&[("include_adult", cfg.include_adult.to_string())]);
        if let Some(y) = year {
            req = req.query(&[(year_param, y.to_string())]);
        }

        tracing::debug!(provider = "tmdb", query = name, ?year, "tmdb image search");
        let resp = match with_language(req, language)
            .send_limited(&self.limiter)
            .await
        {
            Ok(resp) => resp,
            // The limiter strips URLs from transport errors to protect API keys.
            Err(e) => {
                tracing::debug!(provider = "tmdb", error = %e, "tmdb request failed");
                return Vec::new();
            }
        };
        if !resp.status().is_success() {
            tracing::debug!(provider = "tmdb", status = %resp.status(), "tmdb returned non-success");
            return Vec::new();
        }
        let Ok(parsed) = resp.counted_json::<SearchResponse>().await else {
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
                url: cfg.image_url(crate::plugin_config::TmdbImageKind::Poster, &poster),
                ..Default::default()
            });
        }
        if let Some(backdrop) = hit.backdrop_path.filter(|p| !p.is_empty()) {
            images.push(RemoteImage {
                image_type: ImageType::Backdrop,
                url: cfg.image_url(crate::plugin_config::TmdbImageKind::Backdrop, &backdrop),
                ..Default::default()
            });
        }
        images
    }

    /// The poster and backdrop TMDB picks for the title `tmdb_id` names —
    /// `GET /movie/{id}` or `/tv/{id}`, their `poster_path`/`backdrop_path` —
    /// in the shape [`images_for`](Self::images_for) returns for a name
    /// search. What the scan's image pass fetches for an item whose TMDB id
    /// is known: upstream's `TmdbMovieImageProvider`/`TmdbSeriesImageProvider`
    /// look an item's artwork up by its id, never by its name, so an item
    /// identified as another title gets that title's artwork. Empty on a miss
    /// or any failure.
    pub async fn images_by_id(&self, kind: TmdbKind, tmdb_id: i64) -> Vec<RemoteImage> {
        self.images_by_id_for_language(kind, tmdb_id, None).await
    }

    /// Reads the title's preferred poster/backdrop in the metadata language.
    pub async fn images_by_id_for_language(
        &self,
        kind: TmdbKind,
        tmdb_id: i64,
        language: Option<&str>,
    ) -> Vec<RemoteImage> {
        let cfg = self.settings().await;
        let key = cfg.api_key(self.api_key.expose_secret());
        let path = match kind {
            TmdbKind::Movie => "movie",
            TmdbKind::Series => "tv",
        };
        let req = self
            .http
            .get(format!("{}/{path}/{tmdb_id}", self.base_url))
            .query(&[("api_key", key)]);
        let Ok(resp) = with_language(req, language)
            .send_limited(&self.limiter)
            .await
        else {
            return Vec::new();
        };
        if !resp.status().is_success() {
            return Vec::new();
        }
        let Ok(hit) = resp.counted_json::<SearchHit>().await else {
            return Vec::new();
        };
        let mut images = Vec::new();
        if let Some(poster) = hit.poster_path.filter(|p| !p.is_empty()) {
            images.push(RemoteImage {
                image_type: ImageType::Primary,
                url: cfg.image_url(crate::plugin_config::TmdbImageKind::Poster, &poster),
                ..Default::default()
            });
        }
        if let Some(backdrop) = hit.backdrop_path.filter(|p| !p.is_empty()) {
            images.push(RemoteImage {
                image_type: ImageType::Backdrop,
                url: cfg.image_url(crate::plugin_config::TmdbImageKind::Backdrop, &backdrop),
                ..Default::default()
            });
        }
        images
    }

    /// Fetches one season's metadata + artwork (`/tv/{id}/season/{n}`): the
    /// season's own fields, credits and external ids, its poster, and every
    /// episode's name/overview/still, in a single request. `None` on any
    /// failure.
    ///
    /// `TmdbClientManager.GetSeasonAsync` (`:238-262`) appends `credits`,
    /// `images`, `external_ids` and `videos` to this one request, which
    /// `TmdbSeasonProvider` (the credits and the TVDB id) and the season and
    /// episode image providers share. The scan reads the poster from the
    /// season's own `poster_path` and no season video, so only the two the
    /// season provider maps are appended.
    pub async fn season_details(&self, tmdb_id: i64, season_number: i32) -> Option<SeasonDetails> {
        self.season_details_for_language(tmdb_id, season_number, None)
            .await
    }

    /// Fetches a season and its episodes in the resolved metadata language.
    pub async fn season_details_for_language(
        &self,
        tmdb_id: i64,
        season_number: i32,
        language: Option<&str>,
    ) -> Option<SeasonDetails> {
        // The TMDb settings page governs the key, the adult filter, the
        // cast/crew caps and the image sizes; read per call, the way
        // `Plugin.Instance.Configuration` is (`TmdbClientManager`).
        let cfg = self.settings().await;
        let key = cfg.api_key(self.api_key.expose_secret());
        let url = format!("{}/tv/{tmdb_id}/season/{season_number}", self.base_url);
        let req = self.http.get(url).query(&[
            ("api_key", key),
            ("append_to_response", "credits,images,external_ids,videos"),
        ]);
        let req = req.query(&[(
            "include_image_language",
            image_languages_param(language, None),
        )]);
        let resp = with_language(req, language)
            .send_limited(&self.limiter)
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let parsed = resp.counted_json::<SeasonResponse>().await.ok()?;
        Some(season_details_from(parsed, &cfg))
    }

    /// Searches TMDB's collections by name (`/search/collection`) — port of
    /// `TmdbClientManager.SearchCollectionAsync`, the box-set half of the
    /// Identify flow. Empty on no match or any error.
    pub async fn search_collection(
        &self,
        name: &str,
        language: Option<&str>,
    ) -> Vec<TmdbCollectionHit> {
        // The TMDb settings page governs the key, the adult filter, the
        // cast/crew caps and the image sizes; read per call, the way
        // `Plugin.Instance.Configuration` is (`TmdbClientManager`).
        let cfg = self.settings().await;
        let key = cfg.api_key(self.api_key.expose_secret());
        let name = name.trim();
        if name.is_empty() {
            return Vec::new();
        }
        let req = self
            .http
            .get(format!("{}/search/collection", self.base_url))
            .query(&[("api_key", key), ("query", name)]);
        let Ok(resp) = with_language(req, language)
            .send_limited(&self.limiter)
            .await
        else {
            return Vec::new();
        };
        if !resp.status().is_success() {
            return Vec::new();
        }
        let Ok(parsed) = resp.counted_json::<CollectionSearchResponse>().await else {
            return Vec::new();
        };
        parsed
            .results
            .into_iter()
            .map(|hit| TmdbCollectionHit {
                tmdb_id: hit.id,
                name: hit.name.unwrap_or_default(),
                overview: non_empty(hit.overview),
                poster_url: non_empty(hit.poster_path)
                    .map(|p| cfg.image_url(crate::plugin_config::TmdbImageKind::Poster, &p)),
            })
            .collect()
    }

    /// One collection's details plus its artwork (`/collection/{id}` with
    /// `append_to_response=images`) — port of
    /// `TmdbClientManager.GetCollectionAsync`, which backs both
    /// `TmdbBoxSetProvider` and `TmdbBoxSetImageProvider`.
    pub async fn collection(&self, tmdb_id: i64, language: Option<&str>) -> Option<TmdbCollection> {
        // The TMDb settings page governs the key, the adult filter, the
        // cast/crew caps and the image sizes; read per call, the way
        // `Plugin.Instance.Configuration` is (`TmdbClientManager`).
        let cfg = self.settings().await;
        let parsed = self
            .collection_response(
                tmdb_id,
                language,
                Some(image_languages_param(language, None)),
                &cfg,
            )
            .await?;
        // Preserve the legacy metadata result's single default images. The
        // remote image provider uses `collection_images` instead, because the
        // pinned source converts only the appended candidate lists.
        let mut images = Vec::new();
        let mut push = |path: Option<String>, image_type: ImageType| {
            if let Some(path) = non_empty(path) {
                images.push(RemoteImage {
                    image_type,
                    url: cfg.image_url(image_type_size(image_type), &path),
                    ..Default::default()
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

    /// The collection image provider deliberately fetches all languages and
    /// returns the appended poster/backdrop lists, rather than the metadata
    /// endpoint's single default poster. Locale ranking is applied afterward.
    pub async fn collection_images(&self, tmdb_id: i64) -> Vec<TmdbImage> {
        let cfg = self.settings().await;
        let Some(parsed) = self.collection_response(tmdb_id, None, None, &cfg).await else {
            return Vec::new();
        };
        let cfg = &cfg;
        let map = |images: Vec<CollectionImage>, kind: ImageType| {
            images.into_iter().filter_map(move |image| {
                let size = image_type_size(kind);
                let scaled = cfg
                    .image_size(size)
                    .is_some_and(|size| !size.eq_ignore_ascii_case("original"));
                Some(TmdbImage {
                    image_type: kind,
                    url: cfg.image_url(size, &non_empty(image.file_path)?),
                    width: if scaled { None } else { image.width },
                    height: if scaled { None } else { image.height },
                    community_rating: image.vote_average,
                    vote_count: image.vote_count,
                    language: non_empty(image.iso_639_1),
                })
            })
        };
        map(parsed.images.posters, ImageType::Primary)
            .chain(map(parsed.images.backdrops, ImageType::Backdrop))
            .collect()
    }

    async fn collection_response(
        &self,
        tmdb_id: i64,
        language: Option<&str>,
        image_languages: Option<String>,
        cfg: &crate::plugin_config::TmdbConfig,
    ) -> Option<CollectionResponse> {
        let req = self
            .http
            .get(format!("{}/collection/{tmdb_id}", self.base_url))
            .query(&[
                ("api_key", cfg.api_key(self.api_key.expose_secret())),
                ("append_to_response", "images"),
            ]);
        let req = if let Some(image_languages) = image_languages {
            req.query(&[("include_image_language", image_languages)])
        } else {
            req
        };
        let resp = with_language(req, language)
            .send_limited(&self.limiter)
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.counted_json().await.ok()
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
        // The TMDb settings page governs the key, the adult filter, the
        // cast/crew caps and the image sizes; read per call, the way
        // `Plugin.Instance.Configuration` is (`TmdbClientManager`).
        let cfg = self.settings().await;
        let key = cfg.api_key(self.api_key.expose_secret());
        let (path, year_param) = match kind {
            TmdbKind::Movie => ("search/movie", "year"),
            TmdbKind::Series => ("search/tv", "first_air_date_year"),
        };
        let mut req = self
            .http
            .get(format!("{}/{path}", self.base_url))
            .query(&[("api_key", key), ("query", name)])
            // `include_adult` is `Plugin.Instance.Configuration.IncludeAdult`
            // on every TMDb search upstream (`TmdbClientManager.cs:396,424,467`);
            // the API's own default is `false`, which is also the plugin's.
            .query(&[("include_adult", cfg.include_adult.to_string())]);
        if let Some(y) = year {
            req = req.query(&[(year_param, y.to_string())]);
        }
        req = with_language(req, language);
        let Ok(resp) = req.send_limited(&self.limiter).await else {
            return Vec::new();
        };
        if !resp.status().is_success() {
            return Vec::new();
        }
        let Ok(parsed) = resp.counted_json::<SearchResponse>().await else {
            return Vec::new();
        };
        parsed
            .results
            .into_iter()
            .map(|hit| search_hit_from(hit, &cfg))
            .collect()
    }

    /// One page of TMDB's "similar titles" for a movie or series
    /// (`/movie|tv/{id}/similar`) — port of
    /// `TmdbClientManager.GetMovieSimilarPageAsync`/its TV twin.
    ///
    /// Returns the page's TMDB ids and the reported total page count, so the
    /// caller can walk the pages the way the C# provider does. Empty on any
    /// error.
    pub async fn similar_page(&self, kind: TmdbKind, tmdb_id: i64, page: i32) -> (Vec<i64>, i32) {
        // The TMDb settings page governs the key, the adult filter, the
        // cast/crew caps and the image sizes; read per call, the way
        // `Plugin.Instance.Configuration` is (`TmdbClientManager`).
        let cfg = self.settings().await;
        let key = cfg.api_key(self.api_key.expose_secret());
        let path = match kind {
            TmdbKind::Movie => "movie",
            TmdbKind::Series => "tv",
        };
        let Ok(resp) = self
            .http
            .get(format!("{}/{path}/{tmdb_id}/similar", self.base_url))
            .query(&[("api_key", key), ("page", &page.max(1).to_string())])
            .send_limited(&self.limiter)
            .await
        else {
            return (Vec::new(), 0);
        };
        if !resp.status().is_success() {
            return (Vec::new(), 0);
        }
        let Ok(parsed) = resp.counted_json::<SimilarResponse>().await else {
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
        // The TMDb settings page governs the key, the adult filter, the
        // cast/crew caps and the image sizes; read per call, the way
        // `Plugin.Instance.Configuration` is (`TmdbClientManager`).
        let cfg = self.settings().await;
        let key = cfg.api_key(self.api_key.expose_secret());
        let path = match kind {
            TmdbKind::Movie => "movie",
            TmdbKind::Series => "tv",
        };
        let Ok(resp) = self
            .http
            .get(format!("{}/{path}/{tmdb_id}/images", self.base_url))
            .query(&[("api_key", key)])
            .send_limited(&self.limiter)
            .await
        else {
            return Vec::new();
        };
        if !resp.status().is_success() {
            return Vec::new();
        }
        let Ok(parsed) = resp.counted_json::<ImagesResponse>().await else {
            return Vec::new();
        };
        // Borrowed, not moved: the closure is called once per image family and
        // each call's iterator is lazy, so it must not consume the config.
        let cfg = &cfg;
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
                    url: cfg.image_url(image_type_size(image_type), &path),
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

    /// Every automatic acquisition candidate, preserving dimensions instead
    /// of limiting a scan to the metadata response's single poster/backdrop.
    pub async fn image_candidates(
        &self,
        kind: TmdbKind,
        id: i64,
        language: &str,
    ) -> Vec<RemoteImage> {
        ordered_download_images(self.all_images(kind, id).await, language)
    }

    /// Every poster TMDB has for one season — `GET
    /// /tv/{series}/season/{n}/images`, the list behind
    /// `TmdbSeasonImageProvider.GetImages`.
    ///
    /// Keyed off the **series** id plus the season number: TMDB has no
    /// standalone season id, which is also why Ferrofin advertises no
    /// per-season `Tmdb` external-id field. Empty on any transport or decode
    /// failure — a "Choose Image" dialog with no candidates, never an error.
    pub async fn season_images(&self, series_tmdb_id: i64, season_number: i32) -> Vec<TmdbImage> {
        self.tv_images(format!(
            "{}/tv/{series_tmdb_id}/season/{season_number}/images",
            self.base_url
        ))
        .await
    }

    /// Every still TMDB has for one episode — `GET
    /// /tv/{series}/season/{n}/episode/{m}/images`, the list behind
    /// `TmdbEpisodeImageProvider.GetImages`.
    pub async fn episode_images(
        &self,
        series_tmdb_id: i64,
        season_number: i32,
        episode_number: i32,
    ) -> Vec<TmdbImage> {
        self.tv_images(format!(
            "{}/tv/{series_tmdb_id}/season/{season_number}/episode/{episode_number}/images",
            self.base_url
        ))
        .await
    }

    /// The shared body of [`season_images`](Self::season_images) and
    /// [`episode_images`](Self::episode_images): fetch one TMDB `images`
    /// document and take its posters and stills, both as `Primary`
    /// (C# `ConvertPostersToRemoteImageInfo` / `ConvertStillsToRemoteImageInfo`).
    async fn tv_images(&self, url: String) -> Vec<TmdbImage> {
        // The TMDb settings page governs the key, the adult filter, the
        // cast/crew caps and the image sizes; read per call, the way
        // `Plugin.Instance.Configuration` is (`TmdbClientManager`).
        let cfg = self.settings().await;
        let Ok(resp) = self
            .http
            .get(url)
            .query(&[("api_key", cfg.api_key(self.api_key.expose_secret()))])
            .send_limited(&self.limiter)
            .await
        else {
            return Vec::new();
        };
        if !resp.status().is_success() {
            return Vec::new();
        }
        let Ok(parsed) = resp.counted_json::<ImagesResponse>().await else {
            return Vec::new();
        };
        parsed
            .posters
            .into_iter()
            .chain(parsed.stills)
            .filter_map(|e| {
                let path = e.file_path.filter(|p| !p.is_empty())?;
                Some(TmdbImage {
                    image_type: ImageType::Primary,
                    // The settings-page size, not a hardcoded `original`: these
                    // are posters and stills, so they take the same size arm
                    // every other `Primary` image does.
                    url: cfg.image_url(image_type_size(ImageType::Primary), &path),
                    width: e.width,
                    height: e.height,
                    community_rating: e.vote_average,
                    vote_count: e.vote_count,
                    language: e.iso_639_1.filter(|l| !l.is_empty()),
                })
            })
            .collect()
    }

    /// Retrieves episode external ids, trailers and credits in one request.
    /// The season response supplies shared text/artwork; this replaces the
    /// separate credits request while retaining its failure semantics.
    ///
    /// The credits are `TmdbEpisodeProvider`'s: the episode's own `cast` (the
    /// regulars credited in THIS episode, in billing order), then its
    /// `guest_stars` (typed `GuestStar`), then the wanted `crew`. `people` is
    /// `None` when the answer carries no credits; the request's failure
    /// count records a failed one.
    pub async fn episode_extras(
        &self,
        series: i64,
        season: i32,
        episode: i32,
    ) -> Option<EpisodeExtras> {
        self.episode_extras_for_language(series, season, episode, None)
            .await
    }

    /// Episode credits and trailers in the resolved metadata language.
    pub async fn episode_extras_for_language(
        &self,
        series: i64,
        season: i32,
        episode: i32,
        language: Option<&str>,
    ) -> Option<EpisodeExtras> {
        #[derive(Deserialize)]
        struct Response {
            external_ids: Option<ExternalIds>,
            videos: Option<VideosResults>,
            credits: Option<CreditsResponse>,
        }
        let cfg = self.settings().await;
        let key = cfg.api_key(self.api_key.expose_secret());
        let req = self
            .http
            .get(format!(
                "{}/tv/{series}/season/{season}/episode/{episode}",
                self.base_url
            ))
            .query(&[
                ("api_key", key),
                ("append_to_response", "external_ids,videos,credits,images"),
            ]);
        let req = req.query(&[(
            "include_image_language",
            image_languages_param(language, None),
        )]);
        let resp = with_language(req, language)
            .send_limited(&self.limiter)
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let response: Response = resp.counted_json().await.ok()?;
        let ids = response.external_ids.unwrap_or_default();
        let mut provider_ids = Vec::new();
        for (key, value) in [
            ("Imdb", non_empty(ids.imdb_id)),
            (
                "Tvdb",
                ids.tvdb_id.filter(|id| *id > 0).map(|id| id.to_string()),
            ),
            (
                "TvRage",
                ids.tvrage_id.filter(|id| *id > 0).map(|id| id.to_string()),
            ),
        ] {
            if let Some(value) = value {
                provider_ids.push((key.to_owned(), value));
            }
        }
        Some(EpisodeExtras {
            provider_ids,
            trailers: youtube_trailers(response.videos),
            people: response.credits.map(|c| episode_people_from(c, &cfg)),
        })
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
        // The TMDb settings page governs the key, the adult filter, the
        // cast/crew caps and the image sizes; read per call, the way
        // `Plugin.Instance.Configuration` is (`TmdbClientManager`).
        let cfg = self.settings().await;
        let key = cfg.api_key(self.api_key.expose_secret());
        let external_id = external_id.trim();
        if external_id.is_empty() {
            return None;
        }
        let req = self
            .http
            .get(format!("{}/find/{external_id}", self.base_url))
            .query(&[("api_key", key), ("external_source", source)]);
        let resp = with_language(req, language)
            .send_limited(&self.limiter)
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let found = resp.counted_json::<FindResponse>().await.ok()?;
        let hits = match kind {
            TmdbKind::Movie => found.movie_results,
            TmdbKind::Series => found.tv_results,
        };
        // `Some(vec![])` is a real answer, not a failure: the C# providers read
        // `findResult?.MovieResults`/`TvResults` and, once that list exists, do
        // NOT fall back to a name search. `None` is reserved for a request that
        // never produced a payload.
        Some(
            hits.into_iter()
                .map(|hit| search_hit_from(hit, &cfg))
                .collect(),
        )
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
        self.details_for_locale(kind, tmdb_id, language, None).await
    }

    /// Fetches localized metadata and selects the requested country's rating,
    /// falling back to the US rating when no local certification is available.
    pub async fn details_for_locale(
        &self,
        kind: TmdbKind,
        tmdb_id: i64,
        language: Option<&str>,
        country: Option<&str>,
    ) -> Option<TmdbDetails> {
        // The TMDb settings page governs the key, the adult filter, the
        // cast/crew caps and the image sizes; read per call, the way
        // `Plugin.Instance.Configuration` is (`TmdbClientManager`).
        let cfg = self.settings().await;
        let key = cfg.api_key(self.api_key.expose_secret());
        let (path, append) = match kind {
            // The movie Identify branch needs `external_ids` too — `/movie/{id}`
            // carries `imdb_id` directly, so only the series arm asks for it.
            TmdbKind::Movie => ("movie", "credits,release_dates,images,videos,keywords"),
            TmdbKind::Series => (
                "tv",
                "credits,images,content_ratings,videos,external_ids,keywords",
            ),
        };
        let req = self
            .http
            .get(format!("{}/{path}/{tmdb_id}", self.base_url))
            .query(&[("api_key", key), ("append_to_response", append)]);
        let req = req.query(&[(
            "include_image_language",
            image_languages_param(language, country),
        )]);
        let resp = with_language(req, language)
            .send_limited(&self.limiter)
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let d = resp.counted_json::<DetailsResponse>().await.ok()?;

        Some(Self::details_from_response(d, kind, &cfg, country))
    }

    /// Convert the parsed title payload with the selected plugin settings and country.
    fn details_from_response(
        d: DetailsResponse,
        kind: TmdbKind,
        cfg: &crate::plugin_config::TmdbConfig,
        country: Option<&str>,
    ) -> TmdbDetails {
        let premiere = d
            .release_date
            .or(d.first_air_date)
            .filter(|s| !s.is_empty());
        let people = credits_to_people(d.credits, cfg);

        let trailers = youtube_trailers(d.videos);

        let original_title = d
            .original_title
            .or(d.original_name)
            .filter(|s| !s.is_empty());
        let external_ids = d.external_ids;
        TmdbDetails {
            name: d
                .title
                .or(d.name)
                .filter(|s| !s.is_empty())
                .or_else(|| original_title.clone()),
            original_title,
            original_language: non_empty(d.original_language),
            tags: d
                .keywords
                .into_iter()
                .flat_map(|k| k.keywords)
                .filter_map(|k| non_empty(k.name))
                .collect(),
            production_locations: d
                .production_countries
                .unwrap_or_default()
                .into_iter()
                .filter_map(|c| non_empty(c.name))
                .collect(),
            home_page_url: (kind == TmdbKind::Series)
                .then(|| non_empty(d.homepage))
                .flatten(),
            end_date: (kind == TmdbKind::Series)
                .then(|| non_empty(d.last_air_date))
                .flatten(),
            collection_id: d
                .belongs_to_collection
                .as_ref()
                .and_then(|c| c.id)
                .filter(|id| *id > 0),
            collection_name: d.belongs_to_collection.and_then(|c| non_empty(c.name)),
            tvrage_id: external_ids
                .as_ref()
                .and_then(|e| e.tvrage_id)
                .filter(|id| *id > 0)
                .map(|id| id.to_string()),
            overview: d.overview.filter(|s| !s.is_empty()),
            tagline: d.tagline.filter(|s| !s.is_empty()),
            genres: d.genres.into_iter().filter_map(|g| g.name).collect(),
            studios: resolve_studios(d.networks, d.production_companies),
            community_rating: d.vote_average.filter(|v| *v > 0.0),
            official_rating: certification(kind, d.release_dates, d.content_ratings, country),
            production_year: year_from(premiere.as_deref()),
            premiere_date: premiere,
            runtime_minutes: match kind {
                TmdbKind::Movie => d.runtime,
                TmdbKind::Series => d.episode_run_time.and_then(|r| r.into_iter().next()),
            }
            .filter(|m| *m > 0),
            people,
            trailers,
            imdb_id: d
                .imdb_id
                .or_else(|| external_ids.as_ref().and_then(|e| e.imdb_id.clone()))
                .filter(|s| !s.is_empty()),
            tvdb_id: external_ids
                .and_then(|e| e.tvdb_id)
                .map(|id| id.to_string()),
            poster_url: non_empty(d.poster_path)
                .map(|p| cfg.image_url(crate::plugin_config::TmdbImageKind::Poster, &p)),
            status: match kind {
                TmdbKind::Series => non_empty(d.status),
                TmdbKind::Movie => None,
            },
        }
    }

    /// Searches TMDB's people by name (`/search/person`) — port of
    /// `TmdbClientManager.SearchPersonAsync`, the "Identify" flow for a
    /// `Person`. Empty on no match or any error.
    pub async fn search_person(&self, name: &str) -> Vec<TmdbPersonHit> {
        // The TMDb settings page governs the key, the adult filter, the
        // cast/crew caps and the image sizes; read per call, the way
        // `Plugin.Instance.Configuration` is (`TmdbClientManager`).
        let cfg = self.settings().await;
        let key = cfg.api_key(self.api_key.expose_secret());
        let name = name.trim();
        if name.is_empty() {
            return Vec::new();
        }
        let Ok(resp) = self
            .http
            .get(format!("{}/search/person", self.base_url))
            .query(&[("api_key", key), ("query", name)])
            // `include_adult` is `Plugin.Instance.Configuration.IncludeAdult`
            // on every TMDb search upstream (`TmdbClientManager.cs:396,424,467`);
            // the API's own default is `false`, which is also the plugin's.
            .query(&[("include_adult", cfg.include_adult.to_string())])
            .send_limited(&self.limiter)
            .await
        else {
            return Vec::new();
        };
        if !resp.status().is_success() {
            return Vec::new();
        }
        let Ok(parsed) = resp.counted_json::<PersonSearchResponse>().await else {
            return Vec::new();
        };
        parsed
            .results
            .into_iter()
            .map(|hit| TmdbPersonHit {
                tmdb_id: hit.id,
                name: non_empty(hit.name),
                profile_url: non_empty(hit.profile_path)
                    .map(|p| cfg.image_url(crate::plugin_config::TmdbImageKind::Profile, &p)),
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
        // The TMDb settings page governs the key, the adult filter, the
        // cast/crew caps and the image sizes; read per call, the way
        // `Plugin.Instance.Configuration` is (`TmdbClientManager`).
        let cfg = self.settings().await;
        let p = self.person_response(tmdb_id, language, &cfg).await?;
        Some(TmdbPersonHit {
            tmdb_id: p.id.unwrap_or(tmdb_id),
            name: non_empty(p.name),
            // `Images.Profiles[0]` — the first profile image.
            profile_url: p
                .images
                .and_then(|i| i.profiles.into_iter().next())
                .and_then(|i| non_empty(i.file_path))
                .map(|path| cfg.image_url(crate::plugin_config::TmdbImageKind::Profile, &path)),
            biography: non_empty(p.biography),
            imdb_id: p.external_ids.and_then(|e| non_empty(e.imdb_id)),
        })
    }

    /// Every profile candidate, including the language, dimensions and vote
    /// information used by `TmdbPersonImageProvider`'s selection ordering.
    pub async fn person_images(&self, tmdb_id: i64, language: Option<&str>) -> Vec<TmdbImage> {
        let cfg = self.settings().await;
        let Some(person) = self.person_response(tmdb_id, language, &cfg).await else {
            return Vec::new();
        };
        let scaled = cfg
            .image_size(crate::plugin_config::TmdbImageKind::Profile)
            .is_some_and(|size| !size.eq_ignore_ascii_case("original"));
        person
            .images
            .into_iter()
            .flat_map(|images| images.profiles)
            .filter_map(|image| {
                Some(TmdbImage {
                    image_type: ImageType::Primary,
                    url: cfg.image_url(
                        crate::plugin_config::TmdbImageKind::Profile,
                        &non_empty(image.file_path)?,
                    ),
                    width: if scaled { None } else { image.width },
                    height: if scaled { None } else { image.height },
                    community_rating: image.vote_average,
                    vote_count: image.vote_count,
                    language: non_empty(image.iso_639_1),
                })
            })
            .collect()
    }

    async fn person_response(
        &self,
        tmdb_id: i64,
        language: Option<&str>,
        cfg: &crate::plugin_config::TmdbConfig,
    ) -> Option<PersonLookupResponse> {
        let req = self
            .http
            .get(format!("{}/person/{tmdb_id}", self.base_url))
            .query(&[
                ("api_key", cfg.api_key(self.api_key.expose_secret())),
                (
                    "append_to_response",
                    "images,external_ids,tv_credits,movie_credits",
                ),
            ]);
        let resp = with_language(req, language)
            .send_limited(&self.limiter)
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.counted_json().await.ok()
    }

    /// Fetches a person's biography via `/person/{id}`, or `None` on any
    /// network/parse error or when TMDB has no biographical text.
    pub async fn person_details(&self, tmdb_id: i64) -> Option<TmdbPersonDetails> {
        self.person_details_for_language(tmdb_id, None).await
    }

    /// Fetches the biography in the resolved item/server metadata language.
    /// The caller normalizes regional codes with the item's country, as
    /// `TmdbPersonProvider.GetMetadata` does before `GetPersonAsync`.
    pub async fn person_details_for_language(
        &self,
        tmdb_id: i64,
        language: Option<&str>,
    ) -> Option<TmdbPersonDetails> {
        // The TMDb settings page governs the key, the adult filter, the
        // cast/crew caps and the image sizes; read per call, the way
        // `Plugin.Instance.Configuration` is (`TmdbClientManager`).
        let cfg = self.settings().await;
        let key = cfg.api_key(self.api_key.expose_secret());
        let req = self
            .http
            .get(format!("{}/person/{tmdb_id}", self.base_url))
            .query(&[
                ("api_key", key),
                (
                    "append_to_response",
                    "images,external_ids,tv_credits,movie_credits",
                ),
            ]);
        let resp = with_language(req, language)
            .send_limited(&self.limiter)
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let p = resp.counted_json::<PersonDetailsResponse>().await.ok()?;
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

    /// Downloads an automatic-acquisition candidate with upstream retry/format
    /// semantics. A 403/404 skips its URL; other failures end this provider's
    /// current image-type pass.
    ///
    /// # Errors
    /// Returns whether acquisition should skip a missing/forbidden candidate
    /// or stop this provider's image-type pass after another download failure.
    pub async fn download_artwork(
        &self,
        url: &str,
    ) -> Result<crate::ArtworkDownload, crate::ArtworkDownloadFailure> {
        crate::image_download::download_artwork(&self.http, url).await
    }

    /// Downloads an image URL's bytes, or `None` on any failure.
    pub async fn download(&self, url: &str) -> Option<Vec<u8>> {
        let resp = crate::image_download::send(&self.http, url).await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.counted_bytes().await.ok()
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
    #[serde(default)]
    width: Option<i32>,
    #[serde(default)]
    height: Option<i32>,
    #[serde(default)]
    iso_639_1: Option<String>,
    #[serde(default)]
    vote_average: Option<f64>,
    #[serde(default)]
    vote_count: Option<i32>,
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

    /// A `200` whose body does not decode is a provider failure, not a
    /// miss: the scan must not stamp a refresh that only looked empty.
    #[tokio::test]
    async fn a_malformed_success_body_counts_as_a_failure() {
        let server = crate::mock_http::MockServer::start(vec![
            ("/season/1", r#"{"episodes": "not a list"}"#.to_owned()),
            ("/season/2", r#"{"episodes": []}"#.to_owned()),
        ])
        .await;
        let client = TmdbClient::new().with_base_url(&server.base_url);
        let (details, failures) =
            crate::rate_limit::count_request_failures(client.season_details(1399, 1)).await;
        assert!(details.is_none());
        assert_eq!(failures, 1);
        let (details, failures) =
            crate::rate_limit::count_request_failures(client.season_details(1399, 2)).await;
        assert!(details.is_some());
        assert_eq!(failures, 0);
    }

    #[tokio::test]
    async fn maps_collection_keywords_countries_and_series_fields() {
        let server = crate::mock_http::MockServer::start(vec![
            ("/movie/1", r#"{"belongs_to_collection":{"id":7,"name":"Collection"},"production_countries":[{"name":"Japan"}],"keywords":{"keywords":[{"name":"samurai"}]}}"#.into()),
            ("/tv/2?", r#"{"episode_run_time":[42,45],"last_air_date":"2020-01-02","homepage":"https://example.org/show","keywords":{"results":[{"name":"drama"}]},"external_ids":{"tvdb_id":3,"tvrage_id":4}}"#.into()),
            ("/episode/1", r#"{"external_ids":{"imdb_id":"tt123","tvdb_id":5,"tvrage_id":6},"videos":{"results":[{"site":"YouTube","type":"Trailer","key":"abc","name":"Trailer"}]},"credits":{"cast":[{"id":7,"name":"Actor"}]}}"#.into()),
        ]).await;
        let client = TmdbClient::new().with_base_url(&server.base_url);
        let movie = client.details(TmdbKind::Movie, 1, None).await.unwrap();
        assert_eq!(movie.collection_id, Some(7));
        assert_eq!(movie.collection_name.as_deref(), Some("Collection"));
        assert_eq!(movie.tags, ["samurai"]);
        assert_eq!(movie.production_locations, ["Japan"]);
        let series = client.details(TmdbKind::Series, 2, None).await.unwrap();
        assert_eq!(series.runtime_minutes, Some(42));
        assert_eq!(series.end_date.as_deref(), Some("2020-01-02"));
        assert_eq!(
            series.home_page_url.as_deref(),
            Some("https://example.org/show")
        );
        assert_eq!(series.tags, ["drama"]);
        assert_eq!(series.tvdb_id.as_deref(), Some("3"));
        assert_eq!(series.tvrage_id.as_deref(), Some("4"));
        let extras = client.episode_extras(2, 1, 1).await.unwrap();
        assert_eq!(
            extras.provider_ids,
            [
                ("Imdb".into(), "tt123".into()),
                ("Tvdb".into(), "5".into()),
                ("TvRage".into(), "6".into())
            ]
        );
        assert_eq!(
            extras.trailers[0].url,
            "https://www.youtube.com/watch?v=abc"
        );
        assert_eq!(extras.people.unwrap()[0].name, "Actor");
        // A successful but incomplete response must not clear stored credits.
        assert!(
            client
                .episode_extras(2, 1, 9)
                .await
                .unwrap()
                .people
                .is_none()
        );
    }

    #[tokio::test]
    async fn person_and_collection_candidates_keep_original_dimensions_case_insensitively() {
        let server = crate::mock_http::MockServer::always(r#"{"id":1,"images":{
            "profiles":[{"file_path":"/profile.jpg","width":900,"height":1200,"vote_average":8.2,"vote_count":3}],
            "posters":[{"file_path":"/poster.jpg","width":900,"height":1200,"vote_average":8.2,"vote_count":3}],
            "backdrops":[{"file_path":"/backdrop.jpg","width":1600,"height":900}]}}"#).await;
        for size in ["original", "ORIGINAL", "w185"] {
            let client = TmdbClient::new().with_base_url(&server.base_url);
            let config =
                serde_json::json!({"ProfileSize":size,"PosterSize":size,"BackdropSize":size});
            client.attach_plugin_manager(crate::plugin_config::tests_support::manager_with(
                crate::builtin_plugins::TMDB.id,
                serde_json::to_vec(&config).unwrap(),
            ));
            let profiles = client.person_images(1, Some("fr-CA")).await;
            let collections = client.collection_images(1).await;
            assert_eq!(profiles.len(), 1);
            assert_eq!(collections.len(), 2);
            let expected = if size.eq_ignore_ascii_case("original") {
                Some(900)
            } else {
                None
            };
            assert_eq!(profiles[0].width, expected, "profile {size}");
            assert_eq!(collections[0].width, expected, "collection {size}");
            let expected_height = expected.map(|_| 1200);
            assert_eq!(profiles[0].height, expected_height, "profile {size}");
            assert_eq!(collections[0].height, expected_height, "collection {size}");
            assert_eq!(profiles[0].community_rating, Some(8.2));
            assert_eq!(collections[0].community_rating, Some(8.2));
            assert_eq!(profiles[0].vote_count, Some(3));
            assert_eq!(collections[0].vote_count, Some(3));
        }
    }

    #[tokio::test]
    async fn collection_metadata_keeps_image_language_param_separate_from_all_language_artwork() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = seen.clone();
        let handle = tokio::spawn(async move {
            for _ in 0..7 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    let mut chunk = [0; 4096];
                    let length = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(length, 0);
                    bytes.extend_from_slice(&chunk[..length]);
                }
                let request = String::from_utf8_lossy(&bytes);
                let path = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_owned();
                captured.lock().unwrap().push(path);
                let body = r#"{"id":1,"name":"Collection","biography":"fixture","images":{"profiles":[{"file_path":"/p.jpg"}],"posters":[{"file_path":"/p.jpg"}]}}"#;
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        let client = TmdbClient::new().with_base_url(&format!("http://{address}"));
        assert!(client.collection(1, Some("fr-CA")).await.is_some());
        assert!(client.collection(1, None).await.is_some());
        assert_eq!(client.collection_images(1).await.len(), 1);
        assert_eq!(client.person_images(1, Some("fr-CA")).await.len(), 1);
        assert!(client.person_details_for_language(1, None).await.is_some());
        assert_eq!(client.person_images(1, None).await.len(), 1);
        assert!(client.person_lookup(1, Some("fr-CA")).await.is_some());
        handle.await.unwrap();
        let seen = seen.lock().unwrap();
        let values: Vec<_> = seen
            .iter()
            .map(|path| {
                reqwest::Url::parse(&format!("http://fixture{path}"))
                    .unwrap()
                    .query_pairs()
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect::<std::collections::HashMap<_, _>>()
            })
            .collect();
        assert_eq!(values[0].get("language").map(String::as_str), Some("fr-CA"));
        assert_eq!(
            values[0].get("include_image_language").map(String::as_str),
            Some("fr-CA,null,en")
        );
        assert!(!values[1].contains_key("language"));
        assert_eq!(
            values[1].get("include_image_language").map(String::as_str),
            Some("null,en")
        );
        assert!(!values[2].contains_key("language"));
        assert!(!values[2].contains_key("include_image_language"));
        assert_eq!(values[3].get("language").map(String::as_str), Some("fr-CA"));
        assert!(!values[3].contains_key("include_image_language"));
        assert!(!values[4].contains_key("language"));
        assert!(!values[4].contains_key("include_image_language"));
        assert!(!values[5].contains_key("language"));
        assert!(!values[5].contains_key("include_image_language"));
        assert_eq!(values[6].get("language").map(String::as_str), Some("fr-CA"));
        assert!(!values[6].contains_key("include_image_language"));
    }

    /// Source metadata clients append Images and use the same exact normalized
    /// fallback list, while direct artwork requests remain all-language.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // Four real request consumers and three locale phases.
    async fn metadata_title_season_episode_requests_include_pinned_image_languages() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            for _ in 0..12 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    let mut chunk = [0; 4096];
                    let length = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(length, 0);
                    bytes.extend_from_slice(&chunk[..length]);
                }
                let request = String::from_utf8_lossy(&bytes);
                let path = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_owned();
                let url = reqwest::Url::parse(&format!("http://fixture{path}")).unwrap();
                seen.push((
                    url.path().to_owned(),
                    url.query_pairs()
                        .map(|(key, value)| (key.into_owned(), value.into_owned()))
                        .collect::<std::collections::HashMap<_, _>>(),
                ));
                let body = r#"{"id":1,"name":"Fixture","title":"Fixture","episodes":[],"external_ids":{},"credits":{"cast":[],"crew":[]},"videos":{"results":[]}}"#;
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
            seen
        });
        let client = TmdbClient::new().with_base_url(&format!("http://{address}"));
        for language in [Some("fr-CA"), Some("es-AR"), None] {
            assert!(
                client
                    .details_for_locale(TmdbKind::Movie, 1, language, None)
                    .await
                    .is_some()
            );
            assert!(
                client
                    .details_for_locale(TmdbKind::Series, 1, language, None)
                    .await
                    .is_some()
            );
            assert!(
                client
                    .season_details_for_language(1, 2, language)
                    .await
                    .is_some()
            );
            assert!(
                client
                    .episode_extras_for_language(1, 2, 3, language)
                    .await
                    .is_some()
            );
        }
        let seen = handle.await.unwrap();
        for (phase, rows) in seen.as_chunks::<4>().0.iter().enumerate() {
            let (language, images) = [
                (Some("fr-CA"), "fr-CA,null,en"),
                (Some("es-AR"), "es-AR,null,en"),
                (None, "null,en"),
            ][phase];
            for ((path, query), expected_path) in rows.iter().zip([
                "/movie/1",
                "/tv/1",
                "/tv/1/season/2",
                "/tv/1/season/2/episode/3",
            ]) {
                assert_eq!(path, expected_path);
                assert_eq!(
                    query.get("language").map(String::as_str),
                    language,
                    "{path}: {query:?}"
                );
                assert_eq!(
                    query.get("include_image_language").map(String::as_str),
                    Some(images),
                    "{path}: {query:?}"
                );
                assert!(
                    query["append_to_response"]
                        .split(',')
                        .any(|method| method == "images"),
                    "{path}: {query:?}"
                );
            }
        }
    }

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

    /// `/movie/{id}` carries TMDB's `original_language`; an empty value is
    /// `None`, not an empty string, like every other detail field.
    #[tokio::test]
    async fn details_map_the_original_language() {
        let body = r#"{"id":346,"title":"Seven Samurai","original_title":"七人の侍",
            "original_language":"ja","overview":"A village hires samurai."}"#;
        let server = crate::mock_http::MockServer::start(vec![("/movie/", body.to_owned())]).await;
        let client = TmdbClient::new().with_base_url(&server.base_url);
        let details = client
            .details(TmdbKind::Movie, 346, None)
            .await
            .expect("details");
        assert_eq!(details.name.as_deref(), Some("Seven Samurai"));
        assert_eq!(details.original_title.as_deref(), Some("七人の侍"));
        assert_eq!(details.original_language.as_deref(), Some("ja"));

        let blank = r#"{"id":1,"title":"X","original_language":""}"#;
        let server = crate::mock_http::MockServer::start(vec![("/movie/", blank.to_owned())]).await;
        let client = TmdbClient::new().with_base_url(&server.base_url);
        let details = client
            .details(TmdbKind::Movie, 1, None)
            .await
            .expect("details");
        assert_eq!(details.original_language, None);
    }

    /// `credits.GuestStars.OrderBy(a => a.Order)` with `SortOrder =
    /// guest.Order` (`TmdbEpisodeProvider.cs:248-264`): the guest stars come
    /// back in billing order whatever the payload's order, each keeping
    /// TMDB's own `order` — not its position — as its sort order.
    #[tokio::test]
    async fn guest_stars_are_billed_by_their_tmdb_order() {
        use crate::mock_http::MockServer;
        let body = r#"{
          "cast": [],
          "guest_stars": [
            {"id": 11, "name": "Billed Third", "order": 530},
            {"id": 12, "name": "Billed First", "order": 510},
            {"id": 13, "name": "Billed Second", "order": 520}
          ],
          "crew": []
        }"#;
        let server =
            MockServer::start(vec![("/episode/", format!(r#"{{"credits":{body}}}"#))]).await;
        let client = TmdbClient::new().with_base_url(&server.base_url);
        let people = client
            .episode_extras(1399, 1, 1)
            .await
            .and_then(|extras| extras.people)
            .expect("credits fetched");
        let got: Vec<(&str, Option<i32>)> = people
            .iter()
            .map(|p| (p.name.as_str(), p.sort_order))
            .collect();
        assert_eq!(
            got,
            [
                ("Billed First", Some(510)),
                ("Billed Second", Some(520)),
                ("Billed Third", Some(530)),
            ]
        );
    }

    #[tokio::test]
    async fn episode_credits_map_cast_guests_and_crew() {
        use crate::mock_http::MockServer;

        let body = r#"{
          "cast": [
            {"id": 1, "name": "Regular One", "character": "Hero", "profile_path": "/r1.jpg", "order": 0},
            {"id": 2, "name": "Regular Two", "character": "Sidekick", "order": 1}
          ],
          "guest_stars": [
            {"id": 3, "name": "Guest Star", "character": "Villain", "profile_path": "/g.jpg", "order": 520}
          ],
          "crew": [
            {"id": 4, "name": "Ep Director", "job": "Director"},
            {"id": 5, "name": "Ep Writer", "job": "Screenplay"},
            {"id": 6, "name": "Best Boy", "job": "Best Boy"},
            {"id": 7, "name": "", "job": "Director"}
          ]
        }"#;
        let server =
            MockServer::start(vec![("/episode/", format!(r#"{{"credits":{body}}}"#))]).await;
        let client = TmdbClient::new().with_base_url(&server.base_url);

        let people = client
            .episode_extras(1399, 1, 1)
            .await
            .and_then(|extras| extras.people)
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
        // Each credit's own billing `order` is its sort order; headshots
        // are absolute URLs.
        assert_eq!(people[0].sort_order, Some(0));
        assert_eq!(people[1].sort_order, Some(1));
        assert_eq!(
            people[2].sort_order,
            Some(520),
            "the guest star's own order"
        );
        assert_eq!(people[3].sort_order, None, "crew has none");
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
            certification(TmdbKind::Movie, Some(rd), None, None).as_deref(),
            Some("R")
        );
        let cr = ContentRatingResults {
            results: vec![ContentRatingCountry {
                iso_3166_1: Some("US".to_owned()),
                rating: Some("TV-MA".to_owned()),
            }],
        };
        assert_eq!(
            certification(TmdbKind::Series, None, Some(cr), None).as_deref(),
            Some("TV-MA")
        );
        assert_eq!(certification(TmdbKind::Movie, None, None, None), None);
    }

    #[rstest::rstest]
    #[case("US", "R")]
    #[case("DE", "FSK-12")]
    #[case("de", "FSK-12")]
    #[case("GB", "GB-15")]
    #[case("FR", "R")]
    fn certifications_use_country_prefixes_and_us_fallback(
        #[case] country: &str,
        #[case] expected: &str,
    ) {
        let releases = serde_json::from_value(serde_json::json!({"results":[
            {"iso_3166_1":"DE","release_dates":[{"certification":""},{"certification":"12"}]},
            {"iso_3166_1":"US","release_dates":[{"certification":"R"}]},
            {"iso_3166_1":"GB","release_dates":[{"certification":"15"}]}
        ]}))
        .unwrap();
        assert_eq!(
            certification(TmdbKind::Movie, Some(releases), None, Some(country)).as_deref(),
            Some(expected)
        );
        let ratings = serde_json::from_value(serde_json::json!({"results":[
            {"iso_3166_1":"DE","rating":"12"},
            {"iso_3166_1":"US","rating":"R"},
            {"iso_3166_1":"GB","rating":"15"}
        ]}))
        .unwrap();
        assert_eq!(
            certification(TmdbKind::Series, None, Some(ratings), Some(country)).as_deref(),
            Some(expected)
        );
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
        let details = season_details_from(parsed, &crate::plugin_config::TmdbConfig::default());
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
