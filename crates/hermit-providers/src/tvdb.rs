//! TheTVDB (thetvdb.com) remote metadata + image provider — a port of the
//! Jellyfin `Tvdb` plugin (`jellyfin-plugin-tvdb`, GUID
//! `a677c0da-fac5-4cde-941a-7134223f14c8`).
//!
//! Talks to the **TVDB v4 REST API** (`https://api4.thetvdb.com/v4`) with a
//! built-in project API key (like TMDB, so TV metadata works with zero config)
//! plus an optional subscriber PIN. It resolves and maps series, seasons,
//! episodes, and people, and lists series artwork.
//!
//! Auth: `POST /login` with `{ apikey, pin }` returns a bearer token; the token
//! is cached and re-fetched on demand. Every other call sends
//! `Authorization: Bearer <token>`.
//!
//! Faithful port notes (the C# is the oracle):
//! - Translations: v4 returns a base `name`/`overview` plus per-language
//!   translations; this port uses the base fields (English-default), matching
//!   `ReturnOriginalLanguageOrDefault` when no translation is requested. The
//!   language-fallback chain is a documented follow-up.
//! - Episode ordering: TVDB has multiple orderings (`official` = aired, `dvd`,
//!   `absolute`, …). The episode lookup takes a `season_type`, defaulting to
//!   `official`; season 0 is specials.

use std::sync::Mutex;

use hermit_model::entities::ImageType;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use crate::tmdb::{RemoteImage, TmdbImage};

/// The TVDB v4 REST base.
const API_BASE: &str = "https://api4.thetvdb.com/v4";
/// Jellyfin's built-in TVDB project API key (`PluginConfiguration.ProjectApiKey`),
/// so TV metadata works with no user configuration.
const PROJECT_API_KEY: &str = "7f7eed88-2530-4f84-8ee7-f154471b8f87";
/// The default episode ordering — TVDB "official" (aired) order.
pub const DEFAULT_SEASON_TYPE: &str = "official";

/// A credited person from TVDB (`Character`), mapped toward a Hermit person row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TvdbPerson {
    /// The actor/crew member's name.
    pub name: String,
    /// The Jellyfin person type (`Actor`, `Director`, `Writer`, `GuestStar`).
    pub person_type: String,
    /// The character/role name, if any.
    pub role: Option<String>,
    /// The profile image URL, if any.
    pub image_url: Option<String>,
}

/// A TVDB search candidate (`/search`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TvdbSearchHit {
    /// The numeric TVDB id.
    pub tvdb_id: i64,
    /// The series name.
    pub name: String,
    /// The first-aired year, if parseable.
    pub year: Option<i32>,
    /// A poster/primary image URL, if any.
    pub image_url: Option<String>,
    /// The overview, if any.
    pub overview: Option<String>,
}

/// Mapped series metadata (`/series/{id}/extended`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TvdbSeriesDetails {
    /// The TVDB numeric id.
    pub tvdb_id: i64,
    /// The (base-language) name.
    pub name: Option<String>,
    /// The overview.
    pub overview: Option<String>,
    /// First aired date (`YYYY-MM-DD`).
    pub premiere_date: Option<String>,
    /// Production year, derived from `firstAired`.
    pub production_year: Option<i32>,
    /// End date when the series has ended (`lastAired`), else `None`.
    pub end_date: Option<String>,
    /// The content rating for the requested country (USA fallback).
    pub official_rating: Option<String>,
    /// Genres.
    pub genres: Vec<String>,
    /// Networks/studios (`latestNetwork`/`originalNetwork`).
    pub studios: Vec<String>,
    /// Average runtime in minutes.
    pub runtime_minutes: Option<i32>,
    /// The series status name (e.g. `Continuing`, `Ended`).
    pub status: Option<String>,
    /// The URL slug (`TvdbSlug`).
    pub slug: Option<String>,
    /// Cross-provider ids resolved from `remoteIds`.
    pub imdb_id: Option<String>,
    /// The TMDB id, if TVDB carries one.
    pub tmdb_id: Option<String>,
    /// The Zap2It id, if present.
    pub zap2it_id: Option<String>,
    /// Cast + key crew.
    pub people: Vec<TvdbPerson>,
    /// All artwork, mapped to Hermit image types (rich, for the "Choose Image"
    /// listing). Use [`download_images`](TvdbSeriesDetails::download_images) for
    /// the type+URL pairs the scanner downloads.
    pub images: Vec<TmdbImage>,
}

impl TvdbSeriesDetails {
    /// The artwork as plain type+URL [`RemoteImage`] pairs for the scan-download
    /// path.
    #[must_use]
    pub fn download_images(&self) -> Vec<RemoteImage> {
        self.images
            .iter()
            .map(|i| RemoteImage {
                image_type: i.image_type,
                url: i.url.clone(),
            })
            .collect()
    }
}

/// Mapped episode metadata (`/episodes/{id}/extended`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TvdbEpisodeDetails {
    /// The episode name.
    pub name: Option<String>,
    /// The overview.
    pub overview: Option<String>,
    /// Aired date (`YYYY-MM-DD`).
    pub aired: Option<String>,
    /// Production year, derived from `aired`.
    pub production_year: Option<i32>,
    /// The still/primary image URL, if any.
    pub image_url: Option<String>,
    /// Cast + crew credited on the episode.
    pub people: Vec<TvdbPerson>,
}

/// Mapped season metadata (`/seasons/{id}/extended`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TvdbSeasonDetails {
    /// The season name, if any.
    pub name: Option<String>,
    /// The overview, if any.
    pub overview: Option<String>,
    /// The poster/primary image URL, if any.
    pub image_url: Option<String>,
}

/// Mapped person metadata (`/people/{id}/extended`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TvdbPersonDetails {
    /// The biography, if any (base language).
    pub biography: Option<String>,
    /// The birth date (`YYYY-MM-DD`).
    pub birth: Option<String>,
    /// The death date (`YYYY-MM-DD`).
    pub death: Option<String>,
    /// The birthplace.
    pub birthplace: Option<String>,
}

// ---------------------------------------------------------------------------
// Wire DTOs (TVDB v4 JSON). Field names are camelCase; `/search` uses a few
// snake_case aliases, handled with serde `alias`.
// ---------------------------------------------------------------------------

/// The `{ status, data }` envelope every v4 response carries.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct LoginData {
    token: String,
}

#[derive(Debug, Deserialize)]
struct SearchItem {
    #[serde(alias = "tvdb_id")]
    tvdb_id: Option<String>,
    name: Option<String>,
    year: Option<String>,
    overview: Option<String>,
    image_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RemoteIdWire {
    id: Option<serde_json::Value>,
    #[serde(alias = "sourceName")]
    source_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NamedWire {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ContentRatingWire {
    name: Option<String>,
    country: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CharacterWire {
    #[serde(alias = "personName")]
    person_name: Option<String>,
    name: Option<String>,
    #[serde(alias = "personImgURL")]
    person_img_url: Option<String>,
    #[serde(alias = "peopleType")]
    people_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ArtworkWire {
    image: Option<String>,
    #[serde(rename = "type")]
    type_: Option<i64>,
    language: Option<String>,
    score: Option<f64>,
    width: Option<i32>,
    height: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct SeriesExtendedWire {
    name: Option<String>,
    slug: Option<String>,
    overview: Option<String>,
    #[serde(alias = "firstAired")]
    first_aired: Option<String>,
    #[serde(alias = "lastAired")]
    last_aired: Option<String>,
    #[serde(alias = "averageRuntime")]
    average_runtime: Option<i32>,
    status: Option<NamedWire>,
    genres: Option<Vec<NamedWire>>,
    #[serde(alias = "contentRatings")]
    content_ratings: Option<Vec<ContentRatingWire>>,
    #[serde(alias = "remoteIds")]
    remote_ids: Option<Vec<RemoteIdWire>>,
    #[serde(alias = "latestNetwork")]
    latest_network: Option<NamedWire>,
    #[serde(alias = "originalNetwork")]
    original_network: Option<NamedWire>,
    characters: Option<Vec<CharacterWire>>,
    artworks: Option<Vec<ArtworkWire>>,
}

#[derive(Debug, Deserialize)]
struct EpisodeExtendedWire {
    name: Option<String>,
    overview: Option<String>,
    aired: Option<String>,
    image: Option<String>,
    characters: Option<Vec<CharacterWire>>,
}

#[derive(Debug, Deserialize)]
struct SeasonExtendedWire {
    name: Option<String>,
    overview: Option<String>,
    image: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PersonExtendedWire {
    biography: Option<String>,
    #[serde(alias = "birthDate")]
    birth: Option<String>,
    #[serde(alias = "deathDate")]
    death: Option<String>,
    #[serde(alias = "birthPlace")]
    birthplace: Option<String>,
}

/// A TVDB v4 client. Cheap to clone (wraps a [`reqwest::Client`]); caches the
/// bearer token.
pub struct TvdbClient {
    http: reqwest::Client,
    api_key: SecretString,
    pin: Option<String>,
    token: Mutex<Option<String>>,
}

impl std::fmt::Debug for TvdbClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TvdbClient")
            .field("has_pin", &self.pin.is_some())
            .field("has_token", &self.token.lock().is_ok_and(|t| t.is_some()))
            .finish_non_exhaustive()
    }
}

impl Default for TvdbClient {
    fn default() -> Self {
        Self::new()
    }
}

impl TvdbClient {
    /// A client using the built-in project API key and no subscriber PIN.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config("", "")
    }

    /// A client with an optional user API key (empty → built-in project key) and
    /// an optional subscriber PIN (empty → none).
    #[must_use]
    pub fn with_config(api_key: &str, pin: &str) -> Self {
        let key = if api_key.is_empty() {
            PROJECT_API_KEY
        } else {
            api_key
        };
        Self {
            http: reqwest::Client::new(),
            api_key: SecretString::from(key.to_owned()),
            pin: (!pin.is_empty()).then(|| pin.to_owned()),
            token: Mutex::new(None),
        }
    }

    /// The cached bearer token, logging in on first use. Returns `None` if login
    /// fails (the caller then yields no data — best-effort, like the plugin).
    async fn token(&self) -> Option<String> {
        if let Ok(guard) = self.token.lock()
            && let Some(token) = guard.as_ref()
        {
            return Some(token.clone());
        }
        let mut body = serde_json::Map::new();
        body.insert(
            "apikey".to_owned(),
            self.api_key.expose_secret().to_owned().into(),
        );
        if let Some(pin) = &self.pin {
            body.insert("pin".to_owned(), pin.clone().into());
        }
        let resp = self
            .http
            .post(format!("{API_BASE}/login"))
            .json(&body)
            .send()
            .await
            .ok()?;
        let env: Envelope<LoginData> = resp.json().await.ok()?;
        let token = env.data?.token;
        if let Ok(mut guard) = self.token.lock() {
            *guard = Some(token.clone());
        }
        Some(token)
    }

    /// GETs `path` (with optional query pairs) as an authenticated v4 call and
    /// returns the parsed `data` payload, or `None` on any failure.
    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Option<T> {
        let token = self.token().await?;
        let resp = self
            .http
            .get(format!("{API_BASE}{path}"))
            .bearer_auth(token)
            .query(query)
            .send()
            .await
            .ok()?;
        let env: Envelope<T> = resp.json().await.ok()?;
        env.data
    }

    /// Searches TVDB for a series by name (optionally narrowed by year),
    /// returning ranked candidates. Port of `FindSeries`.
    pub async fn search(&self, name: &str, year: Option<i32>) -> Vec<TvdbSearchHit> {
        let mut query = vec![
            ("query", name.to_owned()),
            ("type", "series".to_owned()),
            ("limit", "10".to_owned()),
        ];
        if let Some(y) = year {
            query.push(("year", y.to_string()));
        }
        let items: Vec<SearchItem> = self.get("/search", &query).await.unwrap_or_default();
        items
            .into_iter()
            .filter_map(|it| {
                let tvdb_id = it.tvdb_id.as_deref()?.parse::<i64>().ok()?;
                let name = it.name?;
                Some(TvdbSearchHit {
                    tvdb_id,
                    name,
                    year: it.year.as_deref().and_then(|y| y.parse::<i32>().ok()),
                    image_url: it.image_url,
                    overview: it.overview,
                })
            })
            .collect()
    }

    /// Fetches and maps full series metadata + artwork for `country_code`
    /// (three-letter ISO, for the content-rating pick). Port of
    /// `FetchSeriesMetadata`.
    pub async fn series_details(
        &self,
        tvdb_id: i64,
        country_code: &str,
    ) -> Option<TvdbSeriesDetails> {
        let wire: SeriesExtendedWire = self
            .get(
                &format!("/series/{tvdb_id}/extended"),
                &[("meta", "translations".to_owned())],
            )
            .await?;
        Some(map_series(tvdb_id, wire, country_code))
    }

    /// Fetches and maps a single episode's metadata. Port of the episode arm of
    /// `TvdbEpisodeProvider`.
    pub async fn episode_details(&self, episode_id: i64) -> Option<TvdbEpisodeDetails> {
        let wire: EpisodeExtendedWire = self
            .get(
                &format!("/episodes/{episode_id}/extended"),
                &[("meta", "translations".to_owned())],
            )
            .await?;
        let production_year = wire.aired.as_deref().and_then(year_of);
        Some(TvdbEpisodeDetails {
            name: wire.name,
            overview: wire.overview,
            aired: wire.aired,
            production_year,
            image_url: wire.image,
            people: map_characters(wire.characters),
        })
    }

    /// Resolves the TVDB episode id for a series' `season`/`number` in the given
    /// ordering (`season_type`, default `official`), then fetches its metadata.
    /// Port of `GetEpisodeTvdbId` (the SxE arm).
    pub async fn episode_by_number(
        &self,
        series_id: i64,
        season_type: &str,
        season: i32,
        number: i32,
    ) -> Option<TvdbEpisodeDetails> {
        #[derive(Deserialize)]
        struct EpisodeRef {
            id: Option<i64>,
            #[serde(alias = "seasonNumber")]
            season_number: Option<i32>,
            number: Option<i32>,
        }
        #[derive(Deserialize)]
        struct EpisodesPayload {
            episodes: Option<Vec<EpisodeRef>>,
        }
        let payload: EpisodesPayload = self
            .get(
                &format!("/series/{series_id}/episodes/{season_type}"),
                &[
                    ("season", season.to_string()),
                    ("episodeNumber", number.to_string()),
                ],
            )
            .await?;
        let id = payload
            .episodes?
            .into_iter()
            .find(|e| e.season_number == Some(season) && e.number == Some(number))
            .and_then(|e| e.id)?;
        self.episode_details(id).await
    }

    /// Fetches and maps a season's metadata. Port of `TvdbSeasonProvider`.
    pub async fn season_details(&self, season_id: i64) -> Option<TvdbSeasonDetails> {
        let wire: SeasonExtendedWire = self
            .get(
                &format!("/seasons/{season_id}/extended"),
                &[("meta", "translations".to_owned())],
            )
            .await?;
        Some(TvdbSeasonDetails {
            name: wire.name,
            overview: wire.overview,
            image_url: wire.image,
        })
    }

    /// Fetches and maps a person's biography. Port of `TvdbPersonProvider`.
    pub async fn person_details(&self, person_id: i64) -> Option<TvdbPersonDetails> {
        let wire: PersonExtendedWire = self
            .get(
                &format!("/people/{person_id}/extended"),
                &[("meta", "translations".to_owned())],
            )
            .await?;
        Some(TvdbPersonDetails {
            biography: wire.biography,
            birth: wire.birth,
            death: wire.death,
            birthplace: wire.birthplace,
        })
    }

    /// Downloads an image by absolute URL, returning its bytes.
    pub async fn download(&self, url: &str) -> Option<Vec<u8>> {
        let resp = self.http.get(url).send().await.ok()?;
        resp.bytes().await.ok().map(|b| b.to_vec())
    }
}

/// The four-digit year in a `YYYY-MM-DD` (or `YYYY…`) date string.
fn year_of(date: &str) -> Option<i32> {
    date.get(0..4).and_then(|y| y.parse::<i32>().ok())
}

/// Maps a TVDB artwork `type` id to a Hermit [`ImageType`], mirroring the
/// plugin's `ArtworkType.GetImageType()` for series artwork. The v4 numeric
/// types: 1 banner, 2 poster, 3 background, 22 clearlogo, 23 clearart.
fn artwork_image_type(type_id: Option<i64>) -> Option<ImageType> {
    match type_id? {
        2 => Some(ImageType::Primary),
        3 => Some(ImageType::Backdrop),
        1 => Some(ImageType::Banner),
        22 => Some(ImageType::Logo),
        23 => Some(ImageType::Art),
        _ => None,
    }
}

/// Maps the `characters` array to [`TvdbPerson`]s (empty name skipped). The
/// `peopleType` names align with Jellyfin's person kinds; default to `Actor`.
fn map_characters(characters: Option<Vec<CharacterWire>>) -> Vec<TvdbPerson> {
    characters
        .unwrap_or_default()
        .into_iter()
        .filter_map(|c| {
            let name = c.person_name.filter(|n| !n.trim().is_empty())?;
            let person_type = match c.people_type.as_deref() {
                Some("Director") => "Director",
                Some("Writer") => "Writer",
                Some("Guest Star" | "Guest") => "GuestStar",
                _ => "Actor",
            }
            .to_owned();
            Some(TvdbPerson {
                name,
                person_type,
                role: c.name.filter(|r| !r.is_empty()),
                image_url: c.person_img_url.filter(|u| !u.is_empty()),
            })
        })
        .collect()
}

/// Maps the raw series wire record to [`TvdbSeriesDetails`], resolving the
/// content rating (country match then USA fallback), networks, remote ids, and
/// artwork. Pure — the oracle for the mapping tests.
fn map_series(tvdb_id: i64, wire: SeriesExtendedWire, country_code: &str) -> TvdbSeriesDetails {
    let production_year = wire.first_aired.as_deref().and_then(year_of);
    // `lastAired` is the end date only for series that have ended.
    let end_date = wire
        .status
        .as_ref()
        .and_then(|s| s.name.as_deref())
        .is_some_and(|s| s.eq_ignore_ascii_case("Ended"))
        .then(|| wire.last_aired.clone())
        .flatten();

    let ratings = wire.content_ratings.unwrap_or_default();
    let official_rating = ratings
        .iter()
        .find(|r| {
            r.country
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(country_code))
        })
        .or_else(|| {
            ratings.iter().find(|r| {
                r.country
                    .as_deref()
                    .is_some_and(|c| c.eq_ignore_ascii_case("usa"))
            })
        })
        .and_then(|r| r.name.clone());

    let studios = wire
        .latest_network
        .and_then(|n| n.name)
        .or_else(|| wire.original_network.and_then(|n| n.name))
        .into_iter()
        .collect();

    let remote_ids = wire.remote_ids.unwrap_or_default();
    let find_remote = |source: &str| -> Option<String> {
        remote_ids
            .iter()
            .find(|r| {
                r.source_name
                    .as_deref()
                    .is_some_and(|s| s.eq_ignore_ascii_case(source))
            })
            .and_then(|r| r.id.as_ref())
            .map(remote_id_to_string)
    };

    let images = wire
        .artworks
        .unwrap_or_default()
        .into_iter()
        .filter_map(|a| {
            let image_type = artwork_image_type(a.type_)?;
            let url = a.image?;
            Some(TmdbImage {
                image_type,
                url,
                language: a.language.filter(|l| !l.is_empty() && l != "null"),
                community_rating: a.score,
                width: a.width,
                height: a.height,
                vote_count: None,
            })
        })
        .collect();

    TvdbSeriesDetails {
        tvdb_id,
        name: wire.name,
        overview: wire.overview,
        premiere_date: wire.first_aired,
        production_year,
        end_date,
        official_rating,
        genres: wire
            .genres
            .unwrap_or_default()
            .into_iter()
            .filter_map(|g| g.name)
            .collect(),
        studios,
        runtime_minutes: wire.average_runtime,
        status: wire.status.and_then(|s| s.name),
        slug: wire.slug,
        imdb_id: find_remote("IMDB"),
        tmdb_id: find_remote("TheMovieDB.com"),
        zap2it_id: find_remote("Zap2It"),
        people: map_characters(wire.characters),
        images,
    }
}

/// A `remoteIds` id is sometimes a JSON string, sometimes a number.
fn remote_id_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series_fixture() -> SeriesExtendedWire {
        serde_json::from_str(
            r#"{
              "id": 121361,
              "name": "Game of Thrones",
              "slug": "game-of-thrones",
              "overview": "Seven noble families fight for control.",
              "firstAired": "2011-04-17",
              "lastAired": "2019-05-19",
              "averageRuntime": 60,
              "status": { "name": "Ended" },
              "genres": [ { "name": "Drama" }, { "name": "Fantasy" } ],
              "contentRatings": [
                { "name": "TV-MA", "country": "usa" },
                { "name": "18", "country": "gbr" }
              ],
              "remoteIds": [
                { "id": "tt0944947", "sourceName": "IMDB" },
                { "id": 1399, "sourceName": "TheMovieDB.com" }
              ],
              "latestNetwork": { "name": "HBO" },
              "characters": [
                { "personName": "Emilia Clarke", "name": "Daenerys", "personImgURL": "/e.jpg", "peopleType": "Actor" },
                { "personName": "", "name": "ignored" },
                { "personName": "David Benioff", "peopleType": "Writer" }
              ],
              "artworks": [
                { "image": "/poster.jpg", "type": 2, "language": "eng", "score": 9.0, "width": 680, "height": 1000 },
                { "image": "/bg.jpg", "type": 3, "language": null, "score": 5.0 },
                { "image": "/unknown.jpg", "type": 999 }
              ]
            }"#,
        )
        .expect("valid series fixture")
    }

    #[test]
    fn map_series_resolves_ratings_ids_networks_and_artwork() {
        let d = map_series(121_361, series_fixture(), "usa");
        assert_eq!(d.name.as_deref(), Some("Game of Thrones"));
        assert_eq!(d.production_year, Some(2011));
        // Ended → end date is populated from lastAired.
        assert_eq!(d.end_date.as_deref(), Some("2019-05-19"));
        assert_eq!(d.official_rating.as_deref(), Some("TV-MA"));
        assert_eq!(d.genres, vec!["Drama".to_owned(), "Fantasy".to_owned()]);
        assert_eq!(d.studios, vec!["HBO".to_owned()]);
        assert_eq!(d.runtime_minutes, Some(60));
        assert_eq!(d.status.as_deref(), Some("Ended"));
        assert_eq!(d.imdb_id.as_deref(), Some("tt0944947"));
        assert_eq!(d.tmdb_id.as_deref(), Some("1399"));
        // Blank-name character dropped; writer kept with the right kind.
        assert_eq!(d.people.len(), 2);
        assert_eq!(d.people[0].name, "Emilia Clarke");
        assert_eq!(d.people[0].person_type, "Actor");
        assert_eq!(d.people[0].role.as_deref(), Some("Daenerys"));
        assert_eq!(d.people[1].person_type, "Writer");
        // Poster→Primary, background→Backdrop; the unknown type is dropped;
        // the "null" language becomes None.
        assert_eq!(d.images.len(), 2);
        assert_eq!(d.images[0].image_type, ImageType::Primary);
        assert_eq!(d.images[0].language.as_deref(), Some("eng"));
        assert_eq!(d.images[1].image_type, ImageType::Backdrop);
        assert_eq!(d.images[1].language, None);
    }

    #[test]
    fn map_series_prefers_country_rating_then_usa_fallback() {
        // gbr requested → gets the UK rating.
        let d = map_series(1, series_fixture(), "gbr");
        assert_eq!(d.official_rating.as_deref(), Some("18"));
        // A country with no entry → USA fallback.
        let d = map_series(1, series_fixture(), "fra");
        assert_eq!(d.official_rating.as_deref(), Some("TV-MA"));
    }

    #[test]
    fn continuing_series_has_no_end_date() {
        let mut wire = series_fixture();
        wire.status = Some(NamedWire {
            name: Some("Continuing".to_owned()),
        });
        let d = map_series(1, wire, "usa");
        assert_eq!(d.end_date, None);
    }

    #[test]
    fn artwork_type_map_matches_the_plugin() {
        assert_eq!(artwork_image_type(Some(2)), Some(ImageType::Primary));
        assert_eq!(artwork_image_type(Some(3)), Some(ImageType::Backdrop));
        assert_eq!(artwork_image_type(Some(1)), Some(ImageType::Banner));
        assert_eq!(artwork_image_type(Some(22)), Some(ImageType::Logo));
        assert_eq!(artwork_image_type(Some(23)), Some(ImageType::Art));
        assert_eq!(artwork_image_type(Some(999)), None);
        assert_eq!(artwork_image_type(None), None);
    }

    #[test]
    fn year_of_parses_leading_year() {
        assert_eq!(year_of("2011-04-17"), Some(2011));
        assert_eq!(year_of("bad"), None);
        assert_eq!(year_of(""), None);
    }

    #[test]
    fn search_items_parse_and_filter() {
        // Reuse the mapping via the wire type: a search item missing a tvdb_id or
        // name is dropped.
        let items: Vec<SearchItem> = serde_json::from_str(
            r#"[
              { "tvdb_id": "121361", "name": "Game of Thrones", "year": "2011", "image_url": "/p.jpg" },
              { "name": "No Id" },
              { "tvdb_id": "abc", "name": "Bad Id" }
            ]"#,
        )
        .expect("items");
        let hits: Vec<TvdbSearchHit> = items
            .into_iter()
            .filter_map(|it| {
                let tvdb_id = it.tvdb_id.as_deref()?.parse::<i64>().ok()?;
                Some(TvdbSearchHit {
                    tvdb_id,
                    name: it.name?,
                    year: it.year.as_deref().and_then(|y| y.parse().ok()),
                    image_url: it.image_url,
                    overview: it.overview,
                })
            })
            .collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tvdb_id, 121_361);
        assert_eq!(hits[0].year, Some(2011));
    }

    #[tokio::test]
    #[ignore = "hits the live TVDB API; run with --ignored"]
    async fn live_search_and_details_smoke() {
        let c = TvdbClient::new();
        let hits = c.search("Game of Thrones", None).await;
        assert!(
            hits.iter().any(|h| h.tvdb_id == 121_361),
            "expected GoT (121361) in {hits:?}"
        );
        let d = c.series_details(121_361, "usa").await.expect("details");
        assert_eq!(d.name.as_deref(), Some("Game of Thrones"));
        assert!(!d.images.is_empty(), "expected artwork");
    }

    #[test]
    fn default_and_debug() {
        let c = TvdbClient::default();
        assert!(c.pin.is_none());
        assert!(format!("{c:?}").contains("TvdbClient"));
        let c = TvdbClient::with_config("userkey", "1234");
        assert_eq!(c.pin.as_deref(), Some("1234"));
    }
}
