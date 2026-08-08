//! fanart.tv remote **image** provider — a port of the Jellyfin `Fanart` plugin
//! (`jellyfin-plugin-fanart`, GUID `170a157f-ac6c-437a-abdd-ca9c25cebd39`).
//!
//! Supplies high-quality artwork (posters, logos, clear-art, disc, banners,
//! backgrounds, thumbs) for movies and series. fanart.tv is keyed off ids other
//! providers already resolved:
//! - **movies** — TMDb id (preferred) or IMDb id;
//! - **series** — TVDb id.
//!
//! It ships on with the plugin's built-in API key; a user `personal_key`
//! (appended as `&client_key=`) raises rate limits and unlocks fresher art.
//!
//! Image ordering mirrors the plugin's `GetImages` sort exactly: widest first
//! (HD variants win), then by language (preferred > `en` > none > other), then
//! by community likes. The `music` (artist/album) leg is deferred until Hermit
//! stamps MusicBrainz ids on music items (see `brain/METADATA_PROVIDERS.md`).

use hermit_model::entities::ImageType;
use serde::Deserialize;

use crate::tmdb::TmdbImage;

/// The fanart.tv v3.2 web-service base; `{type}` is `movies`/`tv`/`music`.
const API_BASE: &str = "https://webservice.fanart.tv/v3.2";
/// The plugin's built-in API key (`Plugin.ApiKey`), so artwork works with no
/// configuration.
const API_KEY: &str = "184e1a2b1fe3b94935365411f919f638";

/// A fanart.tv image record — shared across movie/series arrays. `season` is
/// present only on series images (used to drop season-specific artwork).
#[derive(Debug, Clone, Deserialize)]
struct FanartImage {
    url: Option<String>,
    #[serde(alias = "lang")]
    language: Option<String>,
    likes: Option<String>,
    season: Option<String>,
}

/// The movie response (`/movies/{id}`).
#[derive(Debug, Default, Deserialize)]
struct MovieRoot {
    #[serde(default)]
    hdmovieclearart: Vec<FanartImage>,
    #[serde(default)]
    hdmovielogo: Vec<FanartImage>,
    #[serde(default)]
    moviedisc: Vec<FanartImage>,
    #[serde(default)]
    movieposter: Vec<FanartImage>,
    #[serde(default)]
    movielogo: Vec<FanartImage>,
    #[serde(default)]
    movieart: Vec<FanartImage>,
    #[serde(default)]
    moviethumb: Vec<FanartImage>,
    #[serde(default)]
    moviebanner: Vec<FanartImage>,
    #[serde(default)]
    moviebackground: Vec<FanartImage>,
}

/// The series response (`/tv/{id}`).
#[derive(Debug, Default, Deserialize)]
struct SeriesRoot {
    #[serde(default)]
    hdtvlogo: Vec<FanartImage>,
    #[serde(default)]
    hdclearart: Vec<FanartImage>,
    #[serde(default)]
    clearlogo: Vec<FanartImage>,
    #[serde(default)]
    clearart: Vec<FanartImage>,
    #[serde(default)]
    showbackground: Vec<FanartImage>,
    #[serde(default)]
    seasonthumb: Vec<FanartImage>,
    #[serde(default)]
    tvthumb: Vec<FanartImage>,
    #[serde(default)]
    tvbanner: Vec<FanartImage>,
    #[serde(default)]
    tvposter: Vec<FanartImage>,
}

/// The music (artist) response (`/music/{mbid}`).
#[derive(Debug, Default, Deserialize)]
struct MusicRoot {
    #[serde(default)]
    artistthumb: Vec<FanartImage>,
    #[serde(default)]
    artistbackground: Vec<FanartImage>,
    #[serde(default)]
    hdmusiclogo: Vec<FanartImage>,
    #[serde(default)]
    musiclogo: Vec<FanartImage>,
    #[serde(default)]
    hdmusicarts: Vec<FanartImage>,
    #[serde(default)]
    musicarts: Vec<FanartImage>,
    #[serde(default)]
    musicbanner: Vec<FanartImage>,
    #[serde(default)]
    albums: Vec<FanartAlbum>,
}

/// One album entry inside a music response, keyed by its release-group id.
#[derive(Debug, Default, Deserialize)]
struct FanartAlbum {
    #[serde(default)]
    release_group_id: Option<String>,
    #[serde(default)]
    albumcover: Vec<FanartImage>,
    #[serde(default)]
    cdart: Vec<FanartImage>,
}

/// A fanart.tv client. Cheap to clone (wraps a [`reqwest::Client`]).
#[derive(Debug, Clone)]
pub struct FanartClient {
    http: reqwest::Client,
    /// Optional user personal key (`client_key`), raising limits/freshness.
    personal_key: Option<String>,
    /// The preferred artwork language for the ordering (Hermit has no per-item
    /// metadata-language plumbing yet, so this is fixed at construction).
    // ponytail: fixed language — thread the library's metadata language through
    // if per-library artwork language selection is wanted.
    language: String,
    /// The web-service base (const in production; overridable in tests).
    base_url: String,
}

impl Default for FanartClient {
    fn default() -> Self {
        Self::new(None)
    }
}

impl FanartClient {
    /// A client with an optional personal `client_key` (empty → none); the
    /// preferred artwork language defaults to English.
    #[must_use]
    pub fn new(personal_key: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            personal_key: personal_key.filter(|k| !k.is_empty()),
            language: "en".to_owned(),
            base_url: API_BASE.to_owned(),
        }
    }

    /// Points the client at `base_url` (a mock server) for tests.
    #[cfg(test)]
    fn with_base_url(mut self, base_url: &str) -> Self {
        self.base_url = base_url.to_owned();
        self
    }

    /// Builds the `{type}/{id}` request URL with the api key (+ optional
    /// client_key).
    fn url(&self, media_type: &str, id: &str) -> String {
        let mut url = format!("{}/{media_type}/{id}?api_key={API_KEY}", self.base_url);
        if let Some(key) = &self.personal_key {
            url.push_str("&client_key=");
            url.push_str(key);
        }
        url
    }

    /// GETs and parses a fanart response, or `None` on any failure (best-effort).
    async fn fetch<T: for<'de> Deserialize<'de>>(&self, media_type: &str, id: &str) -> Option<T> {
        let resp = self.http.get(self.url(media_type, id)).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json().await.ok()
    }

    /// Movie artwork by TMDb (or IMDb) id, ranked. Port of `MovieProvider`.
    pub async fn movie_images(&self, id: &str) -> Vec<TmdbImage> {
        let Some(root): Option<MovieRoot> = self.fetch("movies", id).await else {
            return Vec::new();
        };
        let mut out = Vec::new();
        // (array, type, fallback width, height) — verbatim from `AddImages`.
        populate(&mut out, &root.hdmovieclearart, ImageType::Art, 1000, false);
        populate(&mut out, &root.hdmovielogo, ImageType::Logo, 800, false);
        populate(&mut out, &root.moviedisc, ImageType::Disc, 1000, false);
        populate(&mut out, &root.movieposter, ImageType::Primary, 1000, false);
        populate(&mut out, &root.movielogo, ImageType::Logo, 400, false);
        populate(&mut out, &root.movieart, ImageType::Art, 500, false);
        populate(&mut out, &root.moviethumb, ImageType::Thumb, 1000, false);
        populate(&mut out, &root.moviebanner, ImageType::Banner, 1000, false);
        populate(
            &mut out,
            &root.moviebackground,
            ImageType::Backdrop,
            1920,
            false,
        );
        self.sort(&mut out);
        out
    }

    /// Series artwork by TVDb id, ranked, with season-specific images dropped.
    /// Port of `SeriesProvider` (`allowSeasonAll` only for backgrounds).
    pub async fn series_images(&self, tvdb_id: &str) -> Vec<TmdbImage> {
        let Some(root): Option<SeriesRoot> = self.fetch("tv", tvdb_id).await else {
            return Vec::new();
        };
        let mut out = Vec::new();
        populate(&mut out, &root.hdtvlogo, ImageType::Logo, 800, false);
        populate(&mut out, &root.hdclearart, ImageType::Art, 1000, false);
        populate(&mut out, &root.clearlogo, ImageType::Logo, 400, false);
        populate(&mut out, &root.clearart, ImageType::Art, 500, false);
        populate(
            &mut out,
            &root.showbackground,
            ImageType::Backdrop,
            1920,
            true,
        );
        populate(&mut out, &root.seasonthumb, ImageType::Thumb, 500, false);
        populate(&mut out, &root.tvthumb, ImageType::Thumb, 1000, false);
        populate(&mut out, &root.tvbanner, ImageType::Banner, 1000, false);
        populate(&mut out, &root.tvposter, ImageType::Primary, 1000, false);
        self.sort(&mut out);
        out
    }

    /// Artist artwork by MusicBrainz artist id, ranked. Port of `ArtistProvider`.
    pub async fn artist_images(&self, mb_artist_id: &str) -> Vec<TmdbImage> {
        let Some(root): Option<MusicRoot> = self.fetch("music", mb_artist_id).await else {
            return Vec::new();
        };
        let mut out = Vec::new();
        populate(&mut out, &root.artistthumb, ImageType::Primary, 1000, false);
        populate(&mut out, &root.hdmusiclogo, ImageType::Logo, 800, false);
        populate(&mut out, &root.musiclogo, ImageType::Logo, 400, false);
        populate(&mut out, &root.hdmusicarts, ImageType::Art, 1000, false);
        populate(&mut out, &root.musicarts, ImageType::Art, 500, false);
        populate(&mut out, &root.musicbanner, ImageType::Banner, 1000, false);
        populate(
            &mut out,
            &root.artistbackground,
            ImageType::Backdrop,
            1920,
            false,
        );
        self.sort(&mut out);
        out
    }

    /// Album artwork by the album-artist's MusicBrainz id + the album's
    /// release-group id (fanart nests albums under the artist). Port of
    /// `AlbumProvider`: the matching album's `albumcover`→Primary, `cdart`→Disc.
    pub async fn album_images(
        &self,
        mb_album_artist_id: &str,
        release_group_id: &str,
    ) -> Vec<TmdbImage> {
        let Some(root): Option<MusicRoot> = self.fetch("music", mb_album_artist_id).await else {
            return Vec::new();
        };
        let Some(album) = root.albums.into_iter().find(|a| {
            a.release_group_id
                .as_deref()
                .is_some_and(|id| id.eq_ignore_ascii_case(release_group_id))
        }) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        populate(&mut out, &album.albumcover, ImageType::Primary, 1000, false);
        populate(&mut out, &album.cdart, ImageType::Disc, 1000, false);
        self.sort(&mut out);
        out
    }

    /// Sorts images widest-first, then by language rank, then by likes — the
    /// plugin's `GetImages` `OrderByDescending(...).ThenByDescending(...)` chain.
    fn sort(&self, images: &mut [TmdbImage]) {
        let is_en = self.language.eq_ignore_ascii_case("en");
        images.sort_by(|a, b| {
            b.width
                .unwrap_or(0)
                .cmp(&a.width.unwrap_or(0))
                .then_with(|| {
                    self.language_rank(b.language.as_deref(), is_en)
                        .cmp(&self.language_rank(a.language.as_deref(), is_en))
                })
                .then_with(|| {
                    b.community_rating
                        .unwrap_or(0.0)
                        .total_cmp(&a.community_rating.unwrap_or(0.0))
                })
        });
    }

    /// The language preference rank (higher sorts first): preferred = 3, `en` = 2
    /// (when the preferred language isn't itself English), no-language = 3 if the
    /// preferred language is English else 2, everything else 0. Verbatim port of
    /// the C# `ThenByDescending` selector.
    fn language_rank(&self, lang: Option<&str>, is_en: bool) -> i32 {
        let lang = lang.unwrap_or("");
        if self.language.eq_ignore_ascii_case(lang) {
            return 3;
        }
        if !is_en && lang.eq_ignore_ascii_case("en") {
            return 2;
        }
        if lang.is_empty() {
            return if is_en { 3 } else { 2 };
        }
        0
    }
}

/// Appends one fanart array as [`TmdbImage`]s: empty urls skipped, `"00"`/season
/// languages normalized, `likes` → community rating, the fixed per-type width as
/// the sort key. `allow_season_all` keeps `season == "all"` images (backgrounds);
/// otherwise any season-tagged image is dropped (`isSeasonValid`).
fn populate(
    out: &mut Vec<TmdbImage>,
    images: &[FanartImage],
    image_type: ImageType,
    width: i32,
    allow_season_all: bool,
) {
    for img in images {
        let Some(url) = img.url.as_deref().filter(|u| !u.is_empty()) else {
            continue;
        };
        // Season-specific artwork is dropped unless it's the "all"-season set of
        // an array that permits it (backgrounds).
        let season_valid = match img.season.as_deref() {
            None | Some("") => true,
            Some(s) => allow_season_all && s.eq_ignore_ascii_case("all"),
        };
        if !season_valid {
            continue;
        }
        let language = img
            .language
            .as_deref()
            .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("00"))
            .map(str::to_owned);
        let community_rating = img.likes.as_deref().and_then(|l| l.parse::<f64>().ok());
        out.push(TmdbImage {
            image_type,
            url: url.to_owned(),
            width: Some(width),
            height: None,
            community_rating,
            vote_count: None,
            language,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(lang: &str, likes: &str) -> FanartImage {
        FanartImage {
            url: Some(format!("/{lang}-{likes}.jpg")),
            language: Some(lang.to_owned()),
            likes: Some(likes.to_owned()),
            season: None,
        }
    }

    #[test]
    fn populate_maps_language_and_likes_and_skips_blank_urls() {
        let images = vec![
            img("en", "10"),
            img("00", "3"),
            FanartImage {
                url: Some(String::new()),
                ..img("de", "1")
            },
        ];
        let mut out = Vec::new();
        populate(&mut out, &images, ImageType::Primary, 1000, false);
        assert_eq!(out.len(), 2, "blank url dropped");
        assert_eq!(out[0].image_type, ImageType::Primary);
        assert_eq!(out[0].width, Some(1000));
        assert_eq!(out[0].language.as_deref(), Some("en"));
        assert_eq!(out[0].community_rating, Some(10.0));
        // "00" → no language.
        assert_eq!(out[1].language, None);
    }

    #[test]
    fn populate_drops_season_specific_unless_allowed() {
        let seasoned = FanartImage {
            season: Some("2".to_owned()),
            ..img("en", "5")
        };
        let all = FanartImage {
            season: Some("all".to_owned()),
            ..img("en", "5")
        };
        // Season "2" is always dropped.
        let mut out = Vec::new();
        populate(
            &mut out,
            std::slice::from_ref(&seasoned),
            ImageType::Thumb,
            500,
            false,
        );
        assert!(out.is_empty());
        // Season "all" kept only when allow_season_all.
        let mut out = Vec::new();
        populate(
            &mut out,
            std::slice::from_ref(&all),
            ImageType::Backdrop,
            1920,
            false,
        );
        assert!(out.is_empty());
        let mut out = Vec::new();
        populate(
            &mut out,
            std::slice::from_ref(&all),
            ImageType::Backdrop,
            1920,
            true,
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn sort_prefers_width_then_language_then_likes() {
        let client = FanartClient::new(None); // language = en
        let mut images = vec![
            TmdbImage {
                image_type: ImageType::Primary,
                url: "narrow-en".into(),
                width: Some(500),
                height: None,
                community_rating: Some(50.0),
                vote_count: None,
                language: Some("en".into()),
            },
            TmdbImage {
                image_type: ImageType::Primary,
                url: "wide-de".into(),
                width: Some(1000),
                height: None,
                community_rating: Some(1.0),
                vote_count: None,
                language: Some("de".into()),
            },
            TmdbImage {
                image_type: ImageType::Primary,
                url: "wide-en-low".into(),
                width: Some(1000),
                height: None,
                community_rating: Some(2.0),
                vote_count: None,
                language: Some("en".into()),
            },
            TmdbImage {
                image_type: ImageType::Primary,
                url: "wide-en-high".into(),
                width: Some(1000),
                height: None,
                community_rating: Some(9.0),
                vote_count: None,
                language: Some("en".into()),
            },
        ];
        client.sort(&mut images);
        let order: Vec<&str> = images.iter().map(|i| i.url.as_str()).collect();
        // Widest first; among 1000-wide, English (rank 3) beats German (0), and
        // higher likes wins the English tie; the 500-wide sorts last.
        assert_eq!(
            order,
            vec!["wide-en-high", "wide-en-low", "wide-de", "narrow-en"]
        );
    }

    #[test]
    fn language_rank_matches_the_plugin() {
        let en = FanartClient::new(None); // preferred = en
        assert_eq!(en.language_rank(Some("en"), true), 3); // preferred match
        assert_eq!(en.language_rank(None, true), 3); // no-lang, en-preferred
        assert_eq!(en.language_rank(Some("de"), true), 0);
        let mut de = FanartClient::new(None);
        de.language = "de".to_owned();
        assert_eq!(de.language_rank(Some("de"), false), 3); // preferred
        assert_eq!(de.language_rank(Some("en"), false), 2); // en second
        assert_eq!(de.language_rank(None, false), 2); // no-lang second
    }

    #[test]
    fn url_includes_client_key_only_when_set() {
        let c = FanartClient::new(None);
        assert_eq!(
            c.url("movies", "603"),
            format!("{API_BASE}/movies/603?api_key={API_KEY}")
        );
        let c = FanartClient::new(Some("mykey".to_owned()));
        assert!(c.url("tv", "121361").ends_with("&client_key=mykey"));
    }

    #[tokio::test]
    async fn image_legs_over_mock_server() {
        use crate::mock_http::MockServer;
        let server = MockServer::start(vec![
            (
                "/movies/",
                r#"{"movieposter":[{"id":"1","url":"/p.jpg","lang":"en","likes":"9"}],
                    "hdmovielogo":[{"id":"2","url":"/l.jpg","lang":"en","likes":"3"}]}"#
                    .to_owned(),
            ),
            (
                "/tv/",
                r#"{"tvposter":[{"id":"1","url":"/tp.jpg","lang":"en","likes":"5"}],
                    "seasonthumb":[{"id":"2","url":"/st.jpg","lang":"en","season":"2","likes":"1"}]}"#
                    .to_owned(),
            ),
            (
                "/music/",
                r#"{"artistthumb":[{"id":"1","url":"/at.jpg","lang":"en","likes":"7"}],
                    "hdmusiclogo":[{"id":"2","url":"/ml.jpg","lang":"en","likes":"2"}],
                    "albums":[{"release_group_id":"rg-1","albumcover":[{"id":"3","url":"/ac.jpg","likes":"4"}],
                               "cdart":[{"id":"4","url":"/cd.jpg","likes":"1"}]}]}"#
                    .to_owned(),
            ),
        ])
        .await;
        let c = FanartClient::new(None).with_base_url(&server.base_url);

        let movie = c.movie_images("603").await;
        assert!(movie.iter().any(|i| i.image_type == ImageType::Primary));
        assert!(movie.iter().any(|i| i.image_type == ImageType::Logo));

        // Series drops the season-specific thumb; keeps the poster.
        let series = c.series_images("121361").await;
        assert!(series.iter().any(|i| i.image_type == ImageType::Primary));
        assert!(!series.iter().any(|i| i.image_type == ImageType::Thumb));

        let artist = c.artist_images("mbid").await;
        assert!(artist.iter().any(|i| i.image_type == ImageType::Primary));
        assert!(artist.iter().any(|i| i.image_type == ImageType::Logo));

        // Album filtered by release-group id → cover + disc.
        let album = c.album_images("mbid", "rg-1").await;
        assert!(album.iter().any(|i| i.image_type == ImageType::Primary));
        assert!(album.iter().any(|i| i.image_type == ImageType::Disc));
        // A non-matching release group → nothing.
        assert!(c.album_images("mbid", "nope").await.is_empty());
    }

    #[tokio::test]
    #[ignore = "hits the live fanart.tv API; run with --ignored"]
    async fn live_movie_images_smoke() {
        // Fight Club (TMDb 550) reliably has fanart artwork.
        let images = FanartClient::new(None).movie_images("550").await;
        assert!(!images.is_empty(), "expected fanart artwork for tmdb 550");
        assert!(
            images.iter().any(|i| i.image_type == ImageType::Primary),
            "expected at least a poster"
        );
    }

    #[test]
    fn movie_root_parses_real_shape() {
        let root: MovieRoot = serde_json::from_str(
            r#"{
              "name": "Fight Club", "tmdb_id": "550", "imdb_id": "tt0137523",
              "movieposter": [ {"id":"1","url":"/p.jpg","lang":"en","likes":"12"} ],
              "moviebackground": [ {"id":"2","url":"/b.jpg","lang":"00","likes":"5"} ]
            }"#,
        )
        .expect("movie root");
        assert_eq!(root.movieposter.len(), 1);
        assert_eq!(root.moviebackground.len(), 1);
        assert!(root.moviethumb.is_empty());
    }
}
