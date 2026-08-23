//! TheAudioDb metadata provider — a port of Jellyfin's core `AudioDb` provider
//! (`MediaBrowser.Providers/Plugins/AudioDb`, GUID
//! `a629c0da-fac5-4c7e-931a-7174223f14c8`).
//!
//! Supplies artist biography/genre + album metadata and artwork, looked up
//! **by MusicBrainz id** (so MusicBrainz must resolve first). Uses the plugin's
//! built-in free API key.
//!
//! - artist: `GET /artist-mb.php?i={musicbrainz_artist_id}`
//! - album:  `GET /album-mb.php?i={musicbrainz_release_group_id}`

use ferrofin_model::entities::ImageType;
use serde::Deserialize;

use crate::tmdb::TmdbImage;

/// TheAudioDb API base including the plugin's built-in free key (`195003`).
const API_BASE: &str = "https://www.theaudiodb.com/api/v1/json/195003";

/// Mapped artist metadata + artwork.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioDbArtist {
    /// The biography (English), if any.
    pub biography: Option<String>,
    /// Genre, if any.
    pub genre: Option<String>,
    /// Artwork (thumb→Primary, logo→Logo, banner→Banner, fanart→Backdrop).
    pub images: Vec<TmdbImage>,
}

/// Mapped album metadata + artwork.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioDbAlbum {
    /// The album description (English), if any.
    pub description: Option<String>,
    /// Release year, if any.
    pub year: Option<i32>,
    /// Genre, if any.
    pub genre: Option<String>,
    /// Artwork (thumb→Primary, CD art→Disc).
    pub images: Vec<TmdbImage>,
}

// ---- wire DTOs -------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ArtistResult {
    artists: Option<Vec<ArtistWire>>,
}

#[derive(Debug, Deserialize)]
struct ArtistWire {
    #[serde(rename = "strBiographyEN")]
    biography: Option<String>,
    #[serde(rename = "strGenre")]
    genre: Option<String>,
    #[serde(rename = "strArtistThumb")]
    thumb: Option<String>,
    #[serde(rename = "strArtistLogo")]
    logo: Option<String>,
    #[serde(rename = "strArtistBanner")]
    banner: Option<String>,
    #[serde(rename = "strArtistFanart")]
    fanart: Option<String>,
    #[serde(rename = "strArtistFanart2")]
    fanart2: Option<String>,
    #[serde(rename = "strArtistFanart3")]
    fanart3: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AlbumResult {
    album: Option<Vec<AlbumWire>>,
}

#[derive(Debug, Deserialize)]
struct AlbumWire {
    #[serde(rename = "strDescriptionEN")]
    description: Option<String>,
    #[serde(rename = "intYearReleased")]
    year: Option<String>,
    #[serde(rename = "strGenre")]
    genre: Option<String>,
    #[serde(rename = "strAlbumThumb")]
    thumb: Option<String>,
    #[serde(rename = "strAlbumCDart")]
    cd_art: Option<String>,
}

/// A TheAudioDb client. Cheap to clone (wraps a [`reqwest::Client`]).
#[derive(Debug, Clone)]
pub struct AudioDbClient {
    http: reqwest::Client,
    base_url: String,
}

impl Default for AudioDbClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioDbClient {
    /// A client using the built-in free key.
    #[must_use]
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: API_BASE.to_owned(),
        }
    }

    /// Points the client at `base_url` (a mock server) for tests.
    #[cfg(test)]
    pub(crate) fn with_base_url(base_url: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.to_owned(),
        }
    }

    async fn fetch<T: for<'de> Deserialize<'de>>(&self, path: &str, mb_id: &str) -> Option<T> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base_url))
            .query(&[("i", mb_id)])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json().await.ok()
    }

    /// Artist metadata + artwork by MusicBrainz artist id.
    pub async fn artist(&self, mb_artist_id: &str) -> Option<AudioDbArtist> {
        let result: ArtistResult = self.fetch("/artist-mb.php", mb_artist_id).await?;
        let wire = result.artists?.into_iter().next()?;
        let mut images = Vec::new();
        push_image(&mut images, wire.thumb, ImageType::Primary);
        push_image(&mut images, wire.logo, ImageType::Logo);
        push_image(&mut images, wire.banner, ImageType::Banner);
        push_image(&mut images, wire.fanart, ImageType::Backdrop);
        push_image(&mut images, wire.fanart2, ImageType::Backdrop);
        push_image(&mut images, wire.fanart3, ImageType::Backdrop);
        Some(AudioDbArtist {
            biography: non_blank(wire.biography),
            genre: non_blank(wire.genre),
            images,
        })
    }

    /// Album metadata + artwork by MusicBrainz release-group id.
    pub async fn album(&self, mb_release_group_id: &str) -> Option<AudioDbAlbum> {
        let result: AlbumResult = self.fetch("/album-mb.php", mb_release_group_id).await?;
        let wire = result.album?.into_iter().next()?;
        let mut images = Vec::new();
        push_image(&mut images, wire.thumb, ImageType::Primary);
        push_image(&mut images, wire.cd_art, ImageType::Disc);
        Some(AudioDbAlbum {
            description: non_blank(wire.description),
            year: wire.year.as_deref().and_then(|y| y.trim().parse().ok()),
            genre: non_blank(wire.genre),
            images,
        })
    }
}

/// Pushes an artwork URL as a [`TmdbImage`] of `image_type`, skipping blanks.
fn push_image(out: &mut Vec<TmdbImage>, url: Option<String>, image_type: ImageType) {
    if let Some(url) = url.filter(|u| !u.trim().is_empty()) {
        out.push(TmdbImage {
            image_type,
            url,
            width: None,
            height: None,
            community_rating: None,
            vote_count: None,
            language: None,
        });
    }
}

/// `Some(s)` when `s` is non-blank, else `None`.
fn non_blank(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artist_parses_bio_genre_and_images() {
        let r: ArtistResult = serde_json::from_str(
            r#"{"artists":[{
              "strArtist":"Miles Davis",
              "strBiographyEN":"A jazz trumpeter.",
              "strGenre":"Jazz",
              "strArtistThumb":"/thumb.jpg",
              "strArtistLogo":"",
              "strArtistFanart":"/fan1.jpg",
              "strArtistFanart2":"/fan2.jpg"
            }]}"#,
        )
        .expect("artist");
        let wire = r.artists.unwrap().into_iter().next().unwrap();
        let mut images = Vec::new();
        push_image(&mut images, wire.thumb, ImageType::Primary);
        push_image(&mut images, wire.logo, ImageType::Logo); // blank → skipped
        push_image(&mut images, wire.fanart, ImageType::Backdrop);
        push_image(&mut images, wire.fanart2, ImageType::Backdrop);
        assert_eq!(images.len(), 3);
        assert_eq!(images[0].image_type, ImageType::Primary);
        assert_eq!(images[1].image_type, ImageType::Backdrop);
        assert_eq!(
            non_blank(wire.biography).as_deref(),
            Some("A jazz trumpeter.")
        );
        assert_eq!(non_blank(wire.genre).as_deref(), Some("Jazz"));
    }

    #[test]
    fn album_parses_year_and_cd_art() {
        let r: AlbumResult = serde_json::from_str(
            r#"{"album":[{
              "strAlbum":"Kind of Blue",
              "intYearReleased":"1959",
              "strGenre":"Jazz",
              "strDescriptionEN":"Seminal.",
              "strAlbumThumb":"/cover.jpg",
              "strAlbumCDart":"/cd.png"
            }]}"#,
        )
        .expect("album");
        let wire = r.album.unwrap().into_iter().next().unwrap();
        assert_eq!(
            wire.year.as_deref().and_then(|y| y.parse::<i32>().ok()),
            Some(1959)
        );
        let mut images = Vec::new();
        push_image(&mut images, wire.thumb, ImageType::Primary);
        push_image(&mut images, wire.cd_art, ImageType::Disc);
        assert_eq!(images.len(), 2);
        assert_eq!(images[1].image_type, ImageType::Disc);
    }

    #[test]
    fn empty_result_is_none() {
        let r: ArtistResult = serde_json::from_str(r#"{"artists":null}"#).expect("null");
        assert!(r.artists.is_none());
    }

    #[tokio::test]
    async fn artist_and_album_over_mock_server() {
        use crate::mock_http::MockServer;
        let server = MockServer::start(vec![
            (
                "/artist-mb.php",
                r#"{"artists":[{"strArtist":"Miles Davis","strBiographyEN":"Trumpeter.",
                    "strGenre":"Jazz","strArtistThumb":"/t.jpg","strArtistFanart":"/f.jpg"}]}"#
                    .to_owned(),
            ),
            (
                "/album-mb.php",
                r#"{"album":[{"strAlbum":"Kind of Blue","intYearReleased":"1959","strGenre":"Jazz",
                    "strDescriptionEN":"Seminal.","strAlbumThumb":"/c.jpg","strAlbumCDart":"/cd.jpg"}]}"#
                    .to_owned(),
            ),
        ])
        .await;
        let c = AudioDbClient::with_base_url(&server.base_url);

        let a = c.artist("mb-artist").await.expect("artist");
        assert_eq!(a.biography.as_deref(), Some("Trumpeter."));
        assert_eq!(a.genre.as_deref(), Some("Jazz"));
        assert_eq!(a.images.len(), 2);

        let al = c.album("mb-rg").await.expect("album");
        assert_eq!(al.year, Some(1959));
        assert_eq!(al.description.as_deref(), Some("Seminal."));
        assert_eq!(al.images.len(), 2);

        // A different server returning empty → None.
        let empty = MockServer::always(r#"{"artists":null}"#).await;
        assert!(
            AudioDbClient::with_base_url(&empty.base_url)
                .artist("x")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    #[ignore = "hits the live TheAudioDb API; run with --ignored"]
    async fn live_miles_davis_artist() {
        // Miles Davis's MusicBrainz artist id.
        let a = AudioDbClient::new()
            .artist("561d854a-6a28-4aa7-8c99-323e6ce46c2a")
            .await;
        let a = a.expect("artist");
        assert!(a.biography.is_some());
        assert!(!a.images.is_empty());
    }
}
