//! OMDb (<https://www.omdbapi.com>) provider — port of
//! `MediaBrowser.Providers/Plugins/Omdb/{OmdbProvider,OmdbItemProvider,
//! OmdbEpisodeProvider,OmdbImageProvider}.cs`.
//!
//! OMDb supplies IMDb-sourced text (plot, genres, cast/crew, the certificate),
//! the IMDb community rating, the Rotten Tomatoes critic score TMDB has no data
//! for, and a poster. OMDb requires an API key (free at
//! <https://www.omdbapi.com/apikey.aspx>); the composition root supplies it from
//! config, and an empty key disables the provider entirely — every method
//! returns `None`, so the scan behaves exactly as it did before OMDb existed.
//!
//! # Accepted divergences
//!
//! - **No on-disk response cache.** C# writes each response under
//!   `{cache}/omdb/{imdbId}.json` and reuses it for a day. Ferrofin's scan
//!   already skips a row that has the fields OMDb would fill, so the same title
//!   is not re-fetched on a re-scan; a title OMDb has nothing for is the only
//!   repeat request.
//! - **`imdbVotes` is not persisted** — C# parses it and then leaves the
//!   assignment commented out, so nothing observable is lost.

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

/// The OMDb API base URL.
const API_BASE: &str = "https://www.omdbapi.com/";

/// The OMDb `Ratings` source name for the Rotten Tomatoes critic score.
const ROTTEN_TOMATOES: &str = "Rotten Tomatoes";

/// The sentinel OMDb returns for a field it has no value for.
const NOT_AVAILABLE: &str = "N/A";

/// An OMDb client. Cheap to clone (wraps a [`reqwest::Client`]).
#[derive(Debug, Clone)]
pub struct OmdbClient {
    http: reqwest::Client,
    api_key: SecretString,
    base_url: String,
}

impl OmdbClient {
    /// Builds a client with the given API key. An empty (or whitespace) key
    /// leaves the client [disabled](Self::is_enabled).
    #[must_use]
    pub fn new(api_key: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: SecretString::from(api_key.trim()),
            base_url: API_BASE.to_owned(),
        }
    }

    /// Points the client at `base_url` (a mock server) for tests.
    #[cfg(test)]
    fn with_base_url(mut self, base_url: &str) -> Self {
        self.base_url = base_url.to_owned();
        self
    }

    /// Whether an API key is configured (lookups are attempted).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.api_key.expose_secret().is_empty()
    }

    /// The full title record for a title/year, found by OMDb's exact-title
    /// lookup (`&t=`) — port of `OmdbItemProvider.GetSearchResultsInternal`'s
    /// non-search branch, which is how the metadata path resolves an IMDb id
    /// for an item that has none.
    pub async fn find_by_title(
        &self,
        kind: OmdbKind,
        name: &str,
        year: Option<i32>,
    ) -> Option<OmdbItem> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        let year = year.map(|y| y.to_string());
        let mut params = vec![
            ("t", name),
            ("type", kind.as_str()),
            ("plot", "short"),
            ("tomatoes", "true"),
            ("r", "json"),
        ];
        if let Some(year) = year.as_deref() {
            params.push(("y", year));
        }
        let item: OmdbItem = self.get(name, &params).await?;
        item.is_found().then_some(item)
    }

    /// The Identify-dialog candidates for a title — port of
    /// `OmdbItemProvider.GetSearchResultsInternal`.
    ///
    /// A known IMDb id short-circuits the whole search: C# sets
    /// `isSearch = false` and asks for `&i=<id>`, so an item the user already
    /// pinned resolves to exactly itself instead of a list of fuzzy title
    /// matches. `episode` narrows that lookup with `&Season=`/`&Episode=`,
    /// keyed by the SERIES' id rather than the episode's own.
    pub async fn search(
        &self,
        kind: OmdbKind,
        name: &str,
        year: Option<i32>,
        known: &OmdbSearchKey<'_>,
    ) -> Vec<OmdbSearchHit> {
        let season = known.season.map(|n| n.to_string());
        let episode = known.episode.map(|n| n.to_string());
        if let Some(imdb_id) = known.imdb_id.map(str::trim).filter(|id| !id.is_empty()) {
            let mut params = vec![("i", imdb_id), ("plot", "full"), ("r", "json")];
            if let Some(episode) = episode.as_deref() {
                params.push(("Episode", episode));
            }
            if let Some(season) = season.as_deref() {
                params.push(("Season", season));
            }
            // The id branch answers with ONE record, not a `Search` array.
            return self
                .get::<OmdbSearchHit>(imdb_id, &params)
                .await
                .filter(|hit| hit.imdb_id.is_some() || hit.title.is_some())
                .into_iter()
                .collect();
        }
        let name = name.trim();
        if name.is_empty() {
            return Vec::new();
        }
        let year = year.map(|y| y.to_string());
        let mut params = vec![
            ("plot", "full"),
            ("r", "json"),
            ("s", name),
            ("type", kind.as_str()),
        ];
        if let Some(year) = year.as_deref() {
            params.push(("y", year));
        }
        if let Some(episode) = episode.as_deref() {
            params.push(("Episode", episode));
        }
        if let Some(season) = season.as_deref() {
            params.push(("Season", season));
        }
        self.get::<OmdbSearchResults>(name, &params)
            .await
            .map(|r| r.search)
            .unwrap_or_default()
    }

    /// The full title record for an IMDb id, or `None` when the provider is
    /// disabled, the id is empty, the request fails, or OMDb has no such title.
    ///
    /// Port of `OmdbProvider.GetRootObject`: the id is normalized to the `tt`
    /// form and requested with `plot=short&tomatoes=true&r=json`, exactly as
    /// upstream does (the `tomatoes` flag is what populates `Ratings`).
    pub async fn item(&self, imdb_id: &str) -> Option<OmdbItem> {
        let imdb_id = normalize_imdb_id(imdb_id)?;
        let params = [
            ("i", imdb_id.as_str()),
            ("plot", "short"),
            ("tomatoes", "true"),
            ("r", "json"),
        ];
        let item: OmdbItem = self.get(&imdb_id, &params).await?;
        item.is_found().then_some(item)
    }

    /// One episode's record, resolved through the series' season listing —
    /// port of `OmdbProvider.FetchEpisodeData`.
    ///
    /// The season listing is matched by the episode's own IMDb id first (when
    /// known) and by episode number second, mirroring upstream's two passes.
    pub async fn episode(
        &self,
        series_imdb_id: &str,
        season: i32,
        episode: i32,
        episode_imdb_id: Option<&str>,
    ) -> Option<OmdbItem> {
        let series_id = normalize_imdb_id(series_imdb_id)?;
        let season_str = season.to_string();
        let params = [
            ("i", series_id.as_str()),
            ("season", season_str.as_str()),
            ("detail", "full"),
        ];
        let listing: OmdbSeason = self.get(&series_id, &params).await?;
        let by_id = episode_imdb_id
            .and_then(normalize_imdb_id)
            .and_then(|wanted| {
                listing.episodes.iter().find(|e| {
                    e.imdb_id
                        .as_deref()
                        .is_some_and(|id| id.eq_ignore_ascii_case(&wanted))
                })
            });
        by_id
            .or_else(|| listing.episodes.iter().find(|e| e.episode == Some(episode)))
            .cloned()
    }

    /// The Rotten Tomatoes critic rating (`0.0`–`100.0`) for an IMDb id.
    ///
    /// A thin wrapper over [`item`](Self::item) kept because the scan's
    /// rating-only backfill path reads nothing else.
    pub async fn critic_rating(&self, imdb_id: &str) -> Option<f32> {
        self.item(imdb_id).await?.rotten_tomatoes()
    }

    /// Issues one OMDb GET and deserializes the body, logging (never returning)
    /// transport and status failures.
    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        imdb_id: &str,
        params: &[(&str, &str)],
    ) -> Option<T> {
        if !self.is_enabled() {
            return None;
        }
        tracing::debug!(provider = "omdb", imdb_id, "omdb lookup");
        let mut request = self
            .http
            .get(&self.base_url)
            .query(&[("apikey", self.api_key.expose_secret())]);
        for (key, value) in params {
            request = request.query(&[(key, value)]);
        }
        let resp = match request.send().await {
            Ok(resp) => resp,
            // `without_url()` strips the query string — the URL carries the API key.
            Err(e) => {
                tracing::warn!(provider = "omdb", imdb_id, error = %e.without_url(), "omdb request failed");
                return None;
            }
        };
        if !resp.status().is_success() {
            tracing::warn!(provider = "omdb", imdb_id, status = %resp.status(), "omdb returned non-success");
            return None;
        }
        match resp.json::<T>().await {
            Ok(body) => Some(body),
            Err(e) => {
                tracing::warn!(provider = "omdb", imdb_id, error = %e.without_url(), "omdb body did not parse");
                None
            }
        }
    }
}

/// The OMDb `type=` values Jellyfin queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OmdbKind {
    /// `type=movie`.
    Movie,
    /// `type=series`.
    Series,
    /// `type=episode`.
    Episode,
}

impl OmdbKind {
    /// The wire value for the `type` query parameter.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Series => "series",
            Self::Episode => "episode",
        }
    }
}

/// The `&s=` search response.
#[derive(Debug, Default, Deserialize)]
struct OmdbSearchResults {
    #[serde(rename = "Search", default)]
    search: Vec<OmdbSearchHit>,
}

/// One `&s=` search result — the Identify dialog's row.
#[derive(Debug, Clone, Deserialize)]
pub struct OmdbSearchHit {
    /// `Title`.
    #[serde(rename = "Title", default, deserialize_with = "na_option")]
    pub title: Option<String>,
    /// `Year` — a bare year or a range.
    #[serde(rename = "Year", default, deserialize_with = "na_option")]
    pub year: Option<String>,
    /// `imdbID`.
    #[serde(rename = "imdbID", default, deserialize_with = "na_option")]
    pub imdb_id: Option<String>,
    /// `Poster` — an absolute image URL.
    #[serde(rename = "Poster", default, deserialize_with = "na_option")]
    pub poster: Option<String>,
    /// `Released` — a full release date, e.g. `"16 Jul 2010"`.
    #[serde(rename = "Released", default, deserialize_with = "na_option")]
    pub released: Option<String>,
}

impl OmdbSearchHit {
    /// The production year, reading the leading four characters of `Year`.
    #[must_use]
    pub fn production_year(&self) -> Option<i32> {
        self.year.as_deref()?.trim().get(..4)?.parse().ok()
    }

    /// `Released` parsed as a date — the search hit's `PremiereDate`.
    #[must_use]
    pub fn premiere_date(&self) -> Option<DateTime<Utc>> {
        parse_release_date(self.released.as_deref()?)
    }
}

/// Parses OMDb's `Released` field — `"16 Jul 2010"`, the one form the API
/// emits, with ISO accepted too since `DateTime.TryParse` takes it.
fn parse_release_date(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    let date = NaiveDate::parse_from_str(value, "%d %b %Y")
        .or_else(|_| NaiveDate::parse_from_str(value, "%Y-%m-%d"))
        .ok()?;
    Some(Utc.from_utc_datetime(&date.into()))
}

/// What an Identify request already knows about the item being searched for —
/// C#'s `ItemLookupInfo` reduced to the three fields OMDb's query uses.
#[derive(Debug, Clone, Copy, Default)]
pub struct OmdbSearchKey<'a> {
    /// The IMDb id already recorded for the item — for an episode, its SERIES'
    /// id, as `GetSearchResultsInternal` reads `SeriesProviderIds`.
    pub imdb_id: Option<&'a str>,
    /// `ParentIndexNumber`, for an episode.
    pub season: Option<i32>,
    /// `IndexNumber`, for an episode.
    pub episode: Option<i32>,
}

/// One season listing (`&season=N`) — port of `OmdbProvider.SeasonRootObject`.
#[derive(Debug, Default, Deserialize)]
struct OmdbSeason {
    #[serde(rename = "Episodes", default)]
    episodes: Vec<OmdbItem>,
}

/// An OMDb title record — port of `OmdbProvider.RootObject`, restricted to the
/// fields Jellyfin actually maps onto an item.
///
/// Every string field goes through [`na_option`], so OMDb's `"N/A"` sentinel
/// arrives as `None` (C# does this with `JsonOmdbNotAvailableStringConverter`).
#[derive(Debug, Default, Clone, Deserialize)]
pub struct OmdbItem {
    /// `Title`.
    #[serde(rename = "Title", default, deserialize_with = "na_option")]
    pub title: Option<String>,
    /// `Year` — a bare year, or a range like `"2008–2013"`.
    #[serde(rename = "Year", default, deserialize_with = "na_option")]
    pub year: Option<String>,
    /// `Rated` — the certificate (`PG-13`, `TV-MA`, …).
    #[serde(rename = "Rated", default, deserialize_with = "na_option")]
    pub rated: Option<String>,
    /// `Genre` — comma-separated.
    #[serde(rename = "Genre", default, deserialize_with = "na_option")]
    pub genre: Option<String>,
    /// `Director` — comma-separated.
    #[serde(rename = "Director", default, deserialize_with = "na_option")]
    pub director: Option<String>,
    /// `Writer` — comma-separated.
    #[serde(rename = "Writer", default, deserialize_with = "na_option")]
    pub writer: Option<String>,
    /// `Actors` — comma-separated.
    #[serde(rename = "Actors", default, deserialize_with = "na_option")]
    pub actors: Option<String>,
    /// `Plot` — the item's overview.
    #[serde(rename = "Plot", default, deserialize_with = "na_option")]
    pub plot: Option<String>,
    /// `Language` — comma-separated; the first entry is the original language.
    #[serde(rename = "Language", default, deserialize_with = "na_option")]
    pub language: Option<String>,
    /// `Poster` — an absolute image URL.
    #[serde(rename = "Poster", default, deserialize_with = "na_option")]
    pub poster: Option<String>,
    /// `imdbRating` — the community rating, `0.0`–`10.0`.
    #[serde(rename = "imdbRating", default, deserialize_with = "na_option")]
    pub imdb_rating: Option<String>,
    /// `imdbID`.
    #[serde(rename = "imdbID", default, deserialize_with = "na_option")]
    pub imdb_id: Option<String>,
    /// `Website` — the item's home page.
    #[serde(rename = "Website", default, deserialize_with = "na_option")]
    pub website: Option<String>,
    /// `Episode` — the episode number, present only in a season listing.
    #[serde(rename = "Episode", default, deserialize_with = "na_number")]
    pub episode: Option<i32>,
    /// `Ratings` — the per-source score list the RT score comes from.
    #[serde(rename = "Ratings", default)]
    ratings: Vec<OmdbRating>,
    /// `Response` — `"False"` when OMDb has no such title.
    #[serde(rename = "Response", default)]
    response: Option<String>,
}

impl OmdbItem {
    /// Whether OMDb actually found the title (`"Response": "True"`). A season
    /// listing's episodes carry no `Response`, which also counts as found.
    fn is_found(&self) -> bool {
        self.response
            .as_deref()
            .is_none_or(|r| !r.eq_ignore_ascii_case("False"))
    }

    /// The Rotten Tomatoes critic percentage as `0.0`–`100.0`.
    #[must_use]
    pub fn rotten_tomatoes(&self) -> Option<f32> {
        self.ratings
            .iter()
            .find(|r| r.source.eq_ignore_ascii_case(ROTTEN_TOMATOES))
            .and_then(|r| parse_percent(&r.value))
    }

    /// The IMDb community rating as `0.0`–`10.0` (C# `item.CommunityRating`).
    #[must_use]
    pub fn community_rating(&self) -> Option<f32> {
        self.imdb_rating
            .as_deref()?
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|v| (0.0..=10.0).contains(v))
    }

    /// The production year — port of `OmdbProvider.TryParseYear`, which reads
    /// the first four characters so a `"2008–2013"` range yields `2008`.
    #[must_use]
    pub fn production_year(&self) -> Option<i32> {
        let year = self.year.as_deref()?.trim();
        year.get(..4)?.parse::<i32>().ok()
    }

    /// The genre list, split and trimmed (C# `AddGenre` per entry).
    #[must_use]
    pub fn genres(&self) -> Vec<String> {
        split_list(self.genre.as_deref())
    }

    /// The original language — the first entry of the comma-separated
    /// `Language` field.
    #[must_use]
    pub fn original_language(&self) -> Option<String> {
        split_list(self.language.as_deref()).into_iter().next()
    }

    /// The credited people in C# order: the director, the writer, then each
    /// actor. Director/writer are added whole (upstream does not split them).
    #[must_use]
    pub fn people(&self) -> Vec<(String, OmdbPersonKind)> {
        let mut out = Vec::new();
        if let Some(director) = trimmed(self.director.as_deref()) {
            out.push((director, OmdbPersonKind::Director));
        }
        if let Some(writer) = trimmed(self.writer.as_deref()) {
            out.push((writer, OmdbPersonKind::Writer));
        }
        out.extend(
            split_list(self.actors.as_deref())
                .into_iter()
                .map(|actor| (actor, OmdbPersonKind::Actor)),
        );
        out
    }
}

/// The three credit kinds OMDb supplies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OmdbPersonKind {
    /// `Director`.
    Director,
    /// `Writer`.
    Writer,
    /// One entry of `Actors`.
    Actor,
}

/// One `Ratings` entry (`{"Source": "...", "Value": "..."}`).
#[derive(Debug, Clone, Deserialize)]
struct OmdbRating {
    #[serde(rename = "Source")]
    source: String,
    #[serde(rename = "Value")]
    value: String,
}

/// Deserializes a string field, mapping OMDb's `"N/A"` sentinel and the empty
/// string to `None` (C# `JsonOmdbNotAvailableStringConverter`).
fn na_option<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(raw.and_then(|v| trimmed(Some(v.as_str()))))
}

/// Deserializes a numeric field that may arrive as a number, a numeric string,
/// or `"N/A"` (C# `JsonOmdbNotAvailableInt32Converter`).
fn na_number<'de, D>(deserializer: D) -> Result<Option<i32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(
        match Option::<serde_json::Value>::deserialize(deserializer)? {
            Some(serde_json::Value::Number(n)) => n.as_i64().and_then(|n| i32::try_from(n).ok()),
            Some(serde_json::Value::String(s)) => s.trim().parse().ok(),
            _ => None,
        },
    )
}

/// A trimmed, non-empty, non-`N/A` string.
fn trimmed(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty() && !value.eq_ignore_ascii_case(NOT_AVAILABLE)).then(|| value.to_owned())
}

/// Splits one of OMDb's comma-separated lists, dropping empty entries.
fn split_list(value: Option<&str>) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|v| v.split(','))
        .filter_map(|entry| trimmed(Some(entry)))
        .collect()
}

/// Normalizes an IMDb id to the `tt…` form C# requires, or `None` when empty.
fn normalize_imdb_id(imdb_id: &str) -> Option<String> {
    let id = imdb_id.trim();
    if id.is_empty() {
        return None;
    }
    // `get` rather than a slice: a non-ASCII first character would make
    // `id[..2]` panic mid-codepoint.
    Some(
        if id.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("tt")) {
            id.to_owned()
        } else {
            format!("tt{id}")
        },
    )
}

/// Parses an OMDb percentage string (e.g. `"85%"`) into `0.0`–`100.0`.
fn parse_percent(value: &str) -> Option<f32> {
    value
        .trim()
        .trim_end_matches('%')
        .trim()
        .parse::<f32>()
        .ok()
        .filter(|v| (0.0..=100.0).contains(v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock_http::MockServer;

    /// A full OMDb movie body, shaped exactly as the API returns one.
    fn movie_body() -> String {
        r#"{
            "Title":"Inception","Year":"2010","Rated":"PG-13","Runtime":"148 min",
            "Genre":"Action, Adventure, Sci-Fi","Director":"Christopher Nolan",
            "Writer":"Christopher Nolan","Actors":"Leonardo DiCaprio, Joseph Gordon-Levitt, Elliot Page",
            "Plot":"A thief who steals corporate secrets.","Language":"English, Japanese, French",
            "Poster":"https://example.test/poster.jpg",
            "Ratings":[{"Source":"Internet Movie Database","Value":"8.8/10"},
                       {"Source":"Rotten Tomatoes","Value":"87%"}],
            "imdbRating":"8.8","imdbID":"tt1375666","Website":"N/A","Response":"True"
        }"#
        .to_owned()
    }

    #[test]
    fn parses_rotten_tomatoes_from_ratings() {
        let item: OmdbItem = serde_json::from_str(&movie_body()).expect("parse");
        assert_eq!(item.rotten_tomatoes(), Some(87.0));
    }

    #[test]
    fn no_rotten_tomatoes_entry_is_none() {
        let item: OmdbItem =
            serde_json::from_str(r#"{"Ratings":[{"Source":"Metacritic","Value":"73/100"}]}"#)
                .expect("parse");
        assert_eq!(item.rotten_tomatoes(), None);
    }

    #[test]
    fn percent_parsing_bounds() {
        assert_eq!(parse_percent("0%"), Some(0.0));
        assert_eq!(parse_percent("100%"), Some(100.0));
        assert_eq!(parse_percent(" 85 %"), Some(85.0)); // surrounding space is trimmed
        assert_eq!(parse_percent("4 2%"), None); // an internal space is not a number
        assert_eq!(parse_percent("101%"), None); // out of range
        assert_eq!(parse_percent("N/A"), None);
    }

    #[test]
    fn disabled_without_key() {
        assert!(!OmdbClient::new("").is_enabled());
        assert!(OmdbClient::new("abc123").is_enabled());
    }

    #[test]
    fn maps_every_field_jellyfin_reads() {
        let item: OmdbItem = serde_json::from_str(&movie_body()).expect("parse");
        assert_eq!(item.title.as_deref(), Some("Inception"));
        assert_eq!(item.production_year(), Some(2010));
        assert_eq!(item.rated.as_deref(), Some("PG-13"));
        assert_eq!(item.genres(), ["Action", "Adventure", "Sci-Fi"]);
        assert_eq!(item.community_rating(), Some(8.8));
        assert_eq!(item.original_language().as_deref(), Some("English"));
        assert_eq!(
            item.plot.as_deref(),
            Some("A thief who steals corporate secrets.")
        );
        assert_eq!(
            item.poster.as_deref(),
            Some("https://example.test/poster.jpg")
        );
        assert_eq!(item.imdb_id.as_deref(), Some("tt1375666"));
    }

    #[test]
    fn na_fields_become_none() {
        // OMDb answers "N/A" rather than omitting a field it has no value for.
        let item: OmdbItem = serde_json::from_str(
            r#"{"Plot":"N/A","Runtime":"N/A","Genre":"N/A","imdbRating":"N/A",
                "Website":"N/A","Poster":"N/A","Year":"N/A","Episode":"N/A"}"#,
        )
        .expect("parse");
        assert_eq!(item.plot, None);
        assert!(item.genres().is_empty());
        assert_eq!(item.community_rating(), None);
        assert_eq!(item.website, None);
        assert_eq!(item.poster, None);
        assert_eq!(item.production_year(), None);
        assert_eq!(item.episode, None);
    }

    #[test]
    fn a_year_range_yields_its_first_year() {
        // A series' Year is a range; C# reads the leading four characters.
        let item: OmdbItem = serde_json::from_str(r#"{"Year":"2008-2013"}"#).expect("parse");
        assert_eq!(item.production_year(), Some(2008));
    }

    #[test]
    fn people_are_director_then_writer_then_actors() {
        let item: OmdbItem = serde_json::from_str(&movie_body()).expect("parse");
        let people = item.people();
        assert_eq!(
            people[0],
            ("Christopher Nolan".to_owned(), OmdbPersonKind::Director)
        );
        assert_eq!(
            people[1],
            ("Christopher Nolan".to_owned(), OmdbPersonKind::Writer)
        );
        assert_eq!(people.len(), 5, "three actors follow the two crew credits");
        assert!(people[2..].iter().all(|(_, k)| *k == OmdbPersonKind::Actor));
        assert_eq!(people[4].0, "Elliot Page");
    }

    #[test]
    fn imdb_ids_are_normalized_to_the_tt_form() {
        assert_eq!(normalize_imdb_id("1375666").as_deref(), Some("tt1375666"));
        assert_eq!(normalize_imdb_id("tt1375666").as_deref(), Some("tt1375666"));
        assert_eq!(normalize_imdb_id("TT1375666").as_deref(), Some("TT1375666"));
        assert_eq!(normalize_imdb_id("  "), None);
    }

    #[tokio::test]
    async fn item_fetches_and_parses_over_a_mock_server() {
        let server = MockServer::start(vec![("/", movie_body())]).await;
        let client = OmdbClient::new("key").with_base_url(&server.base_url);
        let item = client.item("tt1375666").await.expect("a title");
        assert_eq!(item.title.as_deref(), Some("Inception"));
        assert_eq!(client.critic_rating("tt1375666").await, Some(87.0));
    }

    #[tokio::test]
    async fn a_not_found_response_is_none() {
        let server = MockServer::start(vec![(
            "/",
            r#"{"Response":"False","Error":"Incorrect IMDb ID."}"#.to_owned(),
        )])
        .await;
        let client = OmdbClient::new("key").with_base_url(&server.base_url);
        assert!(client.item("tt0000000").await.is_none());
    }

    #[tokio::test]
    async fn a_disabled_client_never_calls_out() {
        // No mock server: a request would fail, so reaching None proves the
        // key check short-circuits before the call.
        let client = OmdbClient::new("");
        assert!(client.item("tt1375666").await.is_none());
        assert!(client.episode("tt0903747", 1, 1, None).await.is_none());
        assert!(client.critic_rating("tt1375666").await.is_none());
    }

    /// A season listing with two episodes.
    fn season_body() -> String {
        r#"{"Title":"Breaking Bad","Season":"1","Episodes":[
            {"Title":"Pilot","Episode":1,"imdbID":"tt0959621","Plot":"A chemistry teacher."},
            {"Title":"Cat's in the Bag...","Episode":2,"imdbID":"tt1054724","Plot":"Cleanup."}
        ]}"#
        .to_owned()
    }

    #[tokio::test]
    async fn an_episode_resolves_by_number_from_the_season_listing() {
        let server = MockServer::start(vec![("/", season_body())]).await;
        let client = OmdbClient::new("key").with_base_url(&server.base_url);
        let ep = client.episode("tt0903747", 1, 2, None).await.expect("ep");
        assert_eq!(ep.title.as_deref(), Some("Cat's in the Bag..."));
    }

    #[tokio::test]
    async fn an_episodes_own_imdb_id_wins_over_its_number() {
        // C# matches by id first: a mis-numbered row still resolves correctly.
        let server = MockServer::start(vec![("/", season_body())]).await;
        let client = OmdbClient::new("key").with_base_url(&server.base_url);
        let ep = client
            .episode("tt0903747", 1, 99, Some("tt0959621"))
            .await
            .expect("ep");
        assert_eq!(ep.title.as_deref(), Some("Pilot"));
    }

    #[tokio::test]
    async fn find_by_title_returns_the_full_record() {
        let server = MockServer::start(vec![("/", movie_body())]).await;
        let client = OmdbClient::new("key").with_base_url(&server.base_url);
        let item = client
            .find_by_title(OmdbKind::Movie, "Inception", Some(2010))
            .await
            .expect("a title");
        assert_eq!(item.imdb_id.as_deref(), Some("tt1375666"));
    }

    #[tokio::test]
    async fn an_empty_name_is_never_looked_up() {
        let client = OmdbClient::new("key").with_base_url("http://127.0.0.1:1");
        assert!(
            client
                .find_by_title(OmdbKind::Movie, "  ", None)
                .await
                .is_none()
        );
        assert!(
            client
                .search(OmdbKind::Movie, "", None, &OmdbSearchKey::default())
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_known_id_resolves_to_itself_instead_of_a_title_search() {
        // C# `GetSearchResultsInternal` sets `isSearch = false` and queries
        // `&i=<id>` whenever the item already carries an IMDb id, so Identify
        // on a pinned item offers exactly that title — not fuzzy matches.
        let body = r#"{"Title":"Inception","Year":"2010","imdbID":"tt1375666",
            "Released":"16 Jul 2010","Response":"True"}"#;
        let server = MockServer::start(vec![("/", body.to_owned())]).await;
        let client = OmdbClient::new("key").with_base_url(&server.base_url);
        let hits = client
            .search(
                OmdbKind::Movie,
                // A deliberately wrong name: the id must win over it.
                "Not The Title",
                None,
                &OmdbSearchKey {
                    imdb_id: Some("tt1375666"),
                    ..OmdbSearchKey::default()
                },
            )
            .await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].imdb_id.as_deref(), Some("tt1375666"));
        assert_eq!(hits[0].title.as_deref(), Some("Inception"));

        // With no id and no name there is nothing to ask for.
        let empty = client
            .search(OmdbKind::Movie, "  ", None, &OmdbSearchKey::default())
            .await;
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn search_lists_identify_candidates() {
        let body = r#"{"Search":[
            {"Title":"Inception","Year":"2010","imdbID":"tt1375666","Poster":"https://example.test/p.jpg","Released":"16 Jul 2010"},
            {"Title":"Inception: The Cobol Job","Year":"2010","imdbID":"tt5295894","Poster":"N/A"}
        ],"totalResults":"2","Response":"True"}"#;
        let server = MockServer::start(vec![("/", body.to_owned())]).await;
        let client = OmdbClient::new("key").with_base_url(&server.base_url);
        let hits = client
            .search(
                OmdbKind::Movie,
                "Inception",
                None,
                &OmdbSearchKey::default(),
            )
            .await;
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].imdb_id.as_deref(), Some("tt1375666"));
        assert_eq!(hits[0].production_year(), Some(2010));
        assert_eq!(hits[1].poster, None, "N/A posters do not become a URL");
        // `Released` becomes the candidate's PremiereDate, as C#
        // `ResultToMetadataResult` sets it.
        assert_eq!(
            hits[0].premiere_date(),
            chrono::NaiveDate::from_ymd_opt(2010, 7, 16)
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|d| d.and_utc())
        );
        assert_eq!(hits[1].premiere_date(), None, "an absent Released is None");
    }

    #[tokio::test]
    async fn an_absent_episode_is_none() {
        let server = MockServer::start(vec![("/", season_body())]).await;
        let client = OmdbClient::new("key").with_base_url(&server.base_url);
        assert!(client.episode("tt0903747", 1, 42, None).await.is_none());
    }
}
