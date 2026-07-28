//! OMDb (<https://www.omdbapi.com>) provider — the Rotten Tomatoes critic-rating
//! source.
//!
//! TMDB has no Rotten Tomatoes data, so — exactly as Jellyfin's OMDb provider
//! does — the RT critic score comes from OMDb, keyed by a title's IMDb id. OMDb
//! requires an API key (free at <https://www.omdbapi.com/apikey.aspx>); the
//! composition root supplies it from config, and an empty key disables the
//! provider (RT ratings simply stay unpopulated).

use serde::Deserialize;

/// The OMDb API base URL.
const API_BASE: &str = "https://www.omdbapi.com/";

/// The OMDb `Ratings` source name for the Rotten Tomatoes critic score.
const ROTTEN_TOMATOES: &str = "Rotten Tomatoes";

/// An OMDb client. Cheap to clone (wraps a [`reqwest::Client`]).
#[derive(Debug, Clone)]
pub struct OmdbClient {
    http: reqwest::Client,
    api_key: String,
}

impl OmdbClient {
    /// Builds a client with the given API key. An empty (or whitespace) key
    /// leaves the client [disabled](Self::is_enabled).
    #[must_use]
    pub fn new(api_key: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.trim().to_owned(),
        }
    }

    /// Whether an API key is configured (RT lookups are attempted).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.api_key.is_empty()
    }

    /// Fetches the Rotten Tomatoes critic rating (`0.0`–`100.0`) for an IMDb id,
    /// or `None` when the provider is disabled, the id is empty, the request
    /// fails, or OMDb has no RT rating for the title.
    pub async fn critic_rating(&self, imdb_id: &str) -> Option<f32> {
        if !self.is_enabled() || imdb_id.is_empty() {
            return None;
        }
        let resp = self
            .http
            .get(API_BASE)
            .query(&[("apikey", self.api_key.as_str()), ("i", imdb_id)])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body = resp.json::<OmdbResponse>().await.ok()?;
        rotten_tomatoes_rating(&body)
    }
}

/// The subset of an OMDb title response Hermit reads.
#[derive(Debug, Default, Deserialize)]
struct OmdbResponse {
    #[serde(rename = "Ratings", default)]
    ratings: Vec<OmdbRating>,
}

/// One `Ratings` entry (`{"Source": "...", "Value": "..."}`).
#[derive(Debug, Deserialize)]
struct OmdbRating {
    #[serde(rename = "Source")]
    source: String,
    #[serde(rename = "Value")]
    value: String,
}

/// Extracts the Rotten Tomatoes critic percentage from an OMDb response, as a
/// `0.0`–`100.0` value.
fn rotten_tomatoes_rating(body: &OmdbResponse) -> Option<f32> {
    body.ratings
        .iter()
        .find(|r| r.source == ROTTEN_TOMATOES)
        .and_then(|r| parse_percent(&r.value))
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

    #[test]
    fn parses_rotten_tomatoes_from_ratings() {
        let body: OmdbResponse = serde_json::from_str(
            r#"{"Ratings":[
                {"Source":"Internet Movie Database","Value":"7.5/10"},
                {"Source":"Rotten Tomatoes","Value":"85%"},
                {"Source":"Metacritic","Value":"73/100"}
            ]}"#,
        )
        .expect("parse");
        assert_eq!(rotten_tomatoes_rating(&body), Some(85.0));
    }

    #[test]
    fn no_rotten_tomatoes_entry_is_none() {
        let body: OmdbResponse =
            serde_json::from_str(r#"{"Ratings":[{"Source":"Metacritic","Value":"73/100"}]}"#)
                .expect("parse");
        assert_eq!(rotten_tomatoes_rating(&body), None);
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
}
