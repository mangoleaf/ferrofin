//! TheMovieDb (TMDB) remote artwork provider.
//!
//! A focused port of Jellyfin's `Tmdb` plugin (the image-acquisition slice):
//! matches an item by name/year against TMDB and returns its poster/backdrop
//! image URLs. Like Jellyfin, it uses a **built-in API key** ([`TmdbUtils.ApiKey`
//! upstream]) so artwork is fetched with zero configuration; a user-supplied key
//! overrides it.
//!
//! Only movies and TV series are matched here (the primary library artwork);
//! season/episode stills are a later extension. Metadata fields (overview,
//! genres, cast) are likewise deferred — this delivers the missing *artwork*.

use hermit_model::entities::ImageType;
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
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<SearchHit>,
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

/// One season's artwork from `/tv/{id}/season/{n}`: the season poster plus every
/// episode's still, keyed by episode number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeasonImages {
    /// The season poster URL, if any.
    pub poster: Option<String>,
    /// `episode_number` → still image URL.
    pub episode_stills: std::collections::HashMap<i32, String>,
}

#[derive(Debug, Deserialize)]
struct SeasonResponse {
    #[serde(default)]
    poster_path: Option<String>,
    #[serde(default)]
    episodes: Vec<SeasonEpisode>,
}

#[derive(Debug, Deserialize)]
struct SeasonEpisode {
    #[serde(default)]
    episode_number: i32,
    #[serde(default)]
    still_path: Option<String>,
}

/// A TMDB artwork client. Cheap to clone (wraps a [`reqwest::Client`]).
#[derive(Debug, Clone)]
pub struct TmdbClient {
    http: reqwest::Client,
    api_key: String,
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
            api_key: DEFAULT_API_KEY.to_owned(),
        }
    }

    /// A client using a user-supplied API key (empty falls back to the built-in).
    #[must_use]
    pub fn with_api_key(key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: if key.is_empty() {
                DEFAULT_API_KEY.to_owned()
            } else {
                key
            },
        }
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
            .get(format!("{API_BASE}/{path}"))
            .query(&[("api_key", self.api_key.as_str()), ("query", name)]);
        if let Some(y) = year {
            req = req.query(&[(year_param, y.to_string())]);
        }

        let Ok(resp) = req.send().await else {
            return Vec::new();
        };
        if !resp.status().is_success() {
            return Vec::new();
        }
        let Ok(parsed) = resp.json::<SearchResponse>().await else {
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
    /// episodes of the same series can be fetched with [`season_images`]. `None`
    /// on no match or any network/parse error.
    pub async fn series_match(&self, name: &str, year: Option<i32>) -> Option<SeriesMatch> {
        let mut req = self
            .http
            .get(format!("{API_BASE}/search/tv"))
            .query(&[("api_key", self.api_key.as_str()), ("query", name)]);
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

    /// Fetches one season's artwork (`/tv/{id}/season/{n}`): the season poster and
    /// every episode's still, in a single request. Empty on any failure.
    pub async fn season_images(&self, tmdb_id: i64, season_number: i32) -> SeasonImages {
        let url = format!("{API_BASE}/tv/{tmdb_id}/season/{season_number}");
        let Ok(resp) = self
            .http
            .get(url)
            .query(&[("api_key", self.api_key.as_str())])
            .send()
            .await
        else {
            return SeasonImages::default();
        };
        if !resp.status().is_success() {
            return SeasonImages::default();
        }
        let Ok(parsed) = resp.json::<SeasonResponse>().await else {
            return SeasonImages::default();
        };
        let mut images = SeasonImages {
            poster: parsed
                .poster_path
                .filter(|p| !p.is_empty())
                .map(|p| format!("{IMAGE_BASE}{p}")),
            episode_stills: std::collections::HashMap::new(),
        };
        for ep in parsed.episodes {
            if let Some(still) = ep.still_path.filter(|p| !p.is_empty()) {
                images
                    .episode_stills
                    .insert(ep.episode_number, format!("{IMAGE_BASE}{still}"));
            }
        }
        images
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_user_key_falls_back_to_builtin() {
        let c = TmdbClient::with_api_key(String::new());
        assert_eq!(c.api_key, DEFAULT_API_KEY);
        let c = TmdbClient::with_api_key("mykey".to_owned());
        assert_eq!(c.api_key, "mykey");
    }
}
