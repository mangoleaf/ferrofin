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

use crate::rate_limit::CountedBody as _;
use crate::rate_limit::{LimitedRequest as _, RateLimiter};
use ferrofin_model::entities::ImageType;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

use crate::tmdb::TmdbImage;

/// TheAudioDb API base including the plugin's built-in free key (`195003`).
const API_BASE: &str = "https://www.theaudiodb.com/api/v1/json/195003";

/// Mapped artist metadata + artwork — what `AudioDbArtistProvider.
/// ProcessResult` (`AudioDbArtistProvider.cs:197-233`) reads. Its
/// `strWebsite` → `HomePageUrl` is not read: Ferrofin stores no home page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioDbArtist {
    /// Every non-blank biography TheAudioDB has, keyed by its field suffix:
    /// `""` for the unsuffixed English `strBiography`, `"EN"`, `"DE"`, ….
    /// [`Self::biography`] picks one for a language.
    pub biographies: BTreeMap<String, String>,
    /// Genre, if any.
    pub genre: Option<String>,
    /// Sub-genre (`strSubGenre`), if any: the artist's second genre.
    pub sub_genre: Option<String>,
    /// The year the artist formed (`intFormedYear`), if it parses.
    pub formed_year: Option<i32>,
    /// Country (`strCountry`), if any: its production location.
    pub country: Option<String>,
    /// TheAudioDB's own artist id (`idArtist`).
    pub audiodb_id: Option<String>,
    /// The artist's MusicBrainz id as TheAudioDB has it (`strMusicBrainzID`).
    pub musicbrainz_id: Option<String>,
    /// Artwork (thumb→Primary, logo→Logo, banner→Banner, fanart→Backdrop).
    pub images: Vec<TmdbImage>,
}

/// Mapped album metadata + artwork.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioDbAlbum {
    /// The album title AudioDB has (`strAlbum`), present **only** when the
    /// plugin's `ReplaceAlbumName` setting is on, so a `None` here means
    /// "leave the scanned name alone", not "AudioDB had no title". The scan
    /// renames the album with it.
    ///
    /// ACCEPTED DIVERGENCE (Ferrofin makes the documented option work;
    /// upstream's is a no-op): `AudioDbAlbumProvider.cs:93-96` sets
    /// `item.Album`, which no `MergeData` copies onto the album, so the
    /// setting has changed nothing upstream since 26c778eb16 — while its
    /// twin `ReplaceArtistName` sets the artist's `Name`.
    pub name: Option<String>,
    /// Every non-blank description TheAudioDB has, keyed by its field suffix
    /// as [`AudioDbArtist::biographies`]. [`Self::description`] picks one.
    pub descriptions: BTreeMap<String, String>,
    /// Release year, if any.
    pub year: Option<i32>,
    /// Genre, if any.
    pub genre: Option<String>,
    /// The album's artist (`strArtist`): its album artist.
    pub artist: Option<String>,
    /// TheAudioDB's own artist id (`idArtist`).
    pub audiodb_artist_id: Option<String>,
    /// TheAudioDB's own album id (`idAlbum`).
    pub audiodb_album_id: Option<String>,
    /// The album artist's MusicBrainz id as TheAudioDB has it
    /// (`strMusicBrainzArtistID`).
    pub musicbrainz_artist_id: Option<String>,
    /// The album's MusicBrainz release-group id as TheAudioDB has it
    /// (`strMusicBrainzID`).
    pub musicbrainz_release_group_id: Option<String>,
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
    #[serde(rename = "strGenre")]
    genre: Option<String>,
    #[serde(rename = "strSubGenre")]
    sub_genre: Option<String>,
    #[serde(rename = "intFormedYear")]
    formed_year: Option<String>,
    #[serde(rename = "strCountry")]
    country: Option<String>,
    #[serde(rename = "idArtist")]
    id: Option<String>,
    #[serde(rename = "strMusicBrainzID")]
    musicbrainz_id: Option<String>,
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
    /// Every other field, among them the `strBiography{…}` texts.
    #[serde(flatten)]
    rest: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct AlbumResult {
    album: Option<Vec<AlbumWire>>,
}

#[derive(Debug, Deserialize)]
struct AlbumWire {
    #[serde(rename = "strAlbum")]
    name: Option<String>,
    #[serde(rename = "intYearReleased")]
    year: Option<String>,
    #[serde(rename = "strGenre")]
    genre: Option<String>,
    #[serde(rename = "strArtist")]
    artist: Option<String>,
    #[serde(rename = "idArtist")]
    artist_id: Option<String>,
    #[serde(rename = "idAlbum")]
    album_id: Option<String>,
    #[serde(rename = "strMusicBrainzArtistID")]
    musicbrainz_artist_id: Option<String>,
    #[serde(rename = "strMusicBrainzID")]
    musicbrainz_id: Option<String>,
    #[serde(rename = "strAlbumThumb")]
    thumb: Option<String>,
    #[serde(rename = "strAlbumCDart")]
    cd_art: Option<String>,
    /// Every other field, among them the `strDescription{…}` texts.
    #[serde(flatten)]
    rest: BTreeMap<String, Value>,
}

/// A TheAudioDb client. Cheap to clone (wraps a [`reqwest::Client`]).
#[derive(Debug)]
pub struct AudioDbClient {
    http: reqwest::Client,
    limiter: RateLimiter,
    base_url: String,
    /// The AudioDB plugin's dashboard settings (`ReplaceAlbumName`).
    plugin: crate::plugin_config::ConfigSource,
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
            limiter: RateLimiter::new("audiodb"),
            base_url: API_BASE.to_owned(),
            plugin: crate::plugin_config::ConfigSource::new(),
        }
    }

    /// Binds the plugin manager, making the AudioDB settings page's
    /// `ReplaceAlbumName` live. Called once by the composition root.
    pub fn attach_plugin_manager(
        &self,
        plugins: std::sync::Arc<dyn ferrofin_traits::plugins::PluginManager>,
    ) {
        self.plugin.attach(plugins);
    }

    /// Points the client at `base_url` (a mock server in tests).
    #[must_use]
    pub fn with_base_url(base_url: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            limiter: RateLimiter::new("audiodb"),
            base_url: base_url.to_owned(),
            plugin: crate::plugin_config::ConfigSource::new(),
        }
    }

    async fn fetch<T: for<'de> Deserialize<'de>>(&self, path: &str, mb_id: &str) -> Option<T> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base_url))
            .query(&[("i", mb_id)])
            .send_limited(&self.limiter)
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.counted_json().await.ok()
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
            biographies: localized_texts(&wire.rest, "strBiography"),
            genre: non_blank(wire.genre),
            sub_genre: non_blank(wire.sub_genre),
            // `int.TryParse(result.intFormedYear, NumberStyles.Integer, …)`.
            formed_year: wire
                .formed_year
                .as_deref()
                .and_then(|y| y.trim().parse().ok()),
            country: non_blank(wire.country),
            audiodb_id: non_blank(wire.id),
            musicbrainz_id: non_blank(wire.musicbrainz_id),
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
        // `if (Plugin.Instance.Configuration.ReplaceAlbumName &&
        //     !string.IsNullOrWhiteSpace(result.strAlbum)) item.Album = result.strAlbum;`
        // — off by default; handed back as the album's name (see
        // `AudioDbAlbum::name`, an accepted divergence). The blank guard is
        // upstream's, not a convenience: AudioDB returns `""` for albums it
        // has no title for and wiping a scanned album's name with it would be
        // data loss.
        let cfg: crate::plugin_config::AudioDbConfig =
            self.plugin.load(crate::builtin_plugins::AUDIODB.id).await;
        Some(AudioDbAlbum {
            name: if cfg.replace_album_name {
                non_blank(wire.name)
            } else {
                None
            },
            descriptions: localized_texts(&wire.rest, "strDescription"),
            year: wire.year.as_deref().and_then(|y| y.trim().parse().ok()),
            genre: non_blank(wire.genre),
            artist: non_blank(wire.artist),
            audiodb_artist_id: non_blank(wire.artist_id),
            audiodb_album_id: non_blank(wire.album_id),
            musicbrainz_artist_id: non_blank(wire.musicbrainz_artist_id),
            musicbrainz_release_group_id: non_blank(wire.musicbrainz_id),
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

impl AudioDbArtist {
    /// The biography for an item in `language` and its `ResultLanguage`
    /// (`AudioDbArtistProvider.ProcessResult`/`GetBiography`): the text in the
    /// language's subtag when TheAudioDB has it, marked with that subtag, else
    /// the English text marked `"en"`; HTML stripped, `None` when blank.
    #[must_use]
    pub fn biography(&self, language: Option<&str>) -> (Option<String>, String) {
        localized(&self.biographies, language)
    }
}

impl AudioDbAlbum {
    /// The description for an item in `language` and its `ResultLanguage`
    /// (`AudioDbAlbumProvider.ProcessResult`/`GetDescription`): the text in the
    /// language's subtag when TheAudioDB has it, marked with that subtag, else
    /// the English text marked `"en"`; HTML stripped, `None` when blank.
    #[must_use]
    pub fn description(&self, language: Option<&str>) -> (Option<String>, String) {
        localized(&self.descriptions, language)
    }
}

/// The non-blank `{prefix}{suffix}` text fields of a TheAudioDB answer, keyed
/// by suffix (`""` for the unsuffixed English text, `"EN"`, `"DE"`, …).
fn localized_texts(rest: &BTreeMap<String, Value>, prefix: &str) -> BTreeMap<String, String> {
    rest.iter()
        .filter_map(|(key, value)| {
            let suffix = key.strip_prefix(prefix)?;
            let text = value.as_str().filter(|text| !text.trim().is_empty())?;
            Some((suffix.to_owned(), text.to_owned()))
        })
        .collect()
}

/// v12.2 `ProcessResult` with `GetBiography`/`GetDescription`
/// (jellyfin/jellyfin#17619): the text in the requested language's subtag
/// when TheAudioDB has it, with that subtag as the answer's `ResultLanguage`;
/// otherwise the English text (`…EN`, else the unsuffixed field), marked
/// `"en"` so it does not block a provider further down the list that can serve
/// the requested language. HTML is stripped (`StripHtml`). If the localized
/// text strips to nothing, the answer is no overview and there is no English
/// fallback. Upstream stores `""` in that case; `None` here has the same
/// effect, because the merge treats both as empty (`IsNullOrEmpty`).
fn localized(texts: &BTreeMap<String, String>, language: Option<&str>) -> (Option<String>, String) {
    // `MetadataLanguageUtils.GetLanguageSubtag`.
    let subtag = language
        .filter(|language| !language.is_empty())
        .map(|language| {
            language
                .split_once('-')
                .map_or(language, |(subtag, _)| subtag)
                .to_lowercase()
        });
    let suffix = match subtag.as_deref() {
        Some("de") => Some("DE"),
        Some("en") => Some("EN"),
        Some("es") => Some("ES"),
        Some("fr") => Some("FR"),
        Some("he") => Some("IL"),
        Some("hu") => Some("HU"),
        Some("it") => Some("IT"),
        Some("ja") => Some("JP"),
        Some("nl") => Some("NL"),
        Some("no" | "nb" | "nn") => Some("NO"),
        Some("pl") => Some("PL"),
        Some("pt") => Some("PT"),
        Some("ru") => Some("RU"),
        Some("sv") => Some("SE"),
        Some("zh") => Some("CN"),
        _ => None,
    };
    let stripped = |text: &String| {
        Some(ferrofin_common::extensions::strip_html(text)).filter(|text| !text.is_empty())
    };
    if let (Some(suffix), Some(subtag)) = (suffix, subtag)
        && let Some(text) = texts.get(suffix)
    {
        return (stripped(text), subtag);
    }
    (
        texts.get("EN").or_else(|| texts.get("")).and_then(stripped),
        "en".to_owned(),
    )
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
            localized_texts(&wire.rest, "strBiography")
                .get("EN")
                .map(String::as_str),
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
                    "strGenre":"Jazz","strSubGenre":"Bebop","intFormedYear":"1944",
                    "strCountry":"Alton, Illinois","idArtist":"111239",
                    "strMusicBrainzID":"561d854a-6a28-4aa7-8c99-323e6ce46c2a",
                    "strArtistThumb":"/t.jpg","strArtistFanart":"/f.jpg"}]}"#
                    .to_owned(),
            ),
            (
                "/album-mb.php",
                r#"{"album":[{"strAlbum":"Kind of Blue","intYearReleased":"1959","strGenre":"Jazz",
                    "strArtist":"Miles Davis","idArtist":"111239","idAlbum":"2109614",
                    "strMusicBrainzArtistID":"561d854a-6a28-4aa7-8c99-323e6ce46c2a",
                    "strMusicBrainzID":"8e8a594f-2175-3d0d-9f10-5ab6e5d3ec59",
                    "strDescriptionEN":"Seminal.","strAlbumThumb":"/c.jpg","strAlbumCDart":"/cd.jpg"}]}"#
                    .to_owned(),
            ),
        ])
        .await;
        let c = AudioDbClient::with_base_url(&server.base_url);

        let a = c.artist("mb-artist").await.expect("artist");
        assert_eq!(a.biography(None).0.as_deref(), Some("Trumpeter."));
        assert_eq!(a.genre.as_deref(), Some("Jazz"));
        assert_eq!(a.sub_genre.as_deref(), Some("Bebop"));
        assert_eq!(a.formed_year, Some(1944));
        assert_eq!(a.country.as_deref(), Some("Alton, Illinois"));
        assert_eq!(a.audiodb_id.as_deref(), Some("111239"));
        assert_eq!(
            a.musicbrainz_id.as_deref(),
            Some("561d854a-6a28-4aa7-8c99-323e6ce46c2a")
        );
        assert_eq!(a.images.len(), 2);

        let al = c.album("mb-rg").await.expect("album");
        assert_eq!(al.year, Some(1959));
        assert_eq!(al.description(None).0.as_deref(), Some("Seminal."));
        assert_eq!(al.artist.as_deref(), Some("Miles Davis"));
        assert_eq!(al.audiodb_artist_id.as_deref(), Some("111239"));
        assert_eq!(al.audiodb_album_id.as_deref(), Some("2109614"));
        assert_eq!(
            al.musicbrainz_artist_id.as_deref(),
            Some("561d854a-6a28-4aa7-8c99-323e6ce46c2a")
        );
        assert_eq!(
            al.musicbrainz_release_group_id.as_deref(),
            Some("8e8a594f-2175-3d0d-9f10-5ab6e5d3ec59")
        );
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

    /// TheAudioDB now returns its English text unsuffixed (`strBiography`,
    /// `strDescription`); the `…EN` keys are gone (jellyfin/jellyfin#16606,
    /// #17081). A live answer in that shape still yields the overview.
    #[tokio::test]
    async fn the_unsuffixed_english_text_is_the_overview() {
        use crate::mock_http::MockServer;
        let server = MockServer::start(vec![
            (
                "/artist-mb.php",
                r#"{"artists":[{"strArtist":"A","strBiography":"English bio.",
                    "strBiographyDE":null}]}"#
                    .to_owned(),
            ),
            (
                "/album-mb.php",
                r#"{"album":[{"strAlbum":"B","strDescription":"English text."}]}"#.to_owned(),
            ),
        ])
        .await;
        let c = AudioDbClient::with_base_url(&server.base_url);
        let artist = c.artist("mb-artist").await.expect("artist");
        assert_eq!(
            artist.biography(Some("en-US")),
            (Some("English bio.".to_owned()), "en".to_owned())
        );
        let album = c.album("mb-rg").await.expect("album");
        assert_eq!(
            album.description(None),
            (Some("English text.".to_owned()), "en".to_owned())
        );
    }

    /// A text per field TheAudioDB fills, the suffix as its text; `EN` and the
    /// unsuffixed English differ so the precedence between them shows.
    fn every_language() -> BTreeMap<String, String> {
        [
            "EN", "DE", "ES", "FR", "IL", "HU", "IT", "JP", "NL", "NO", "PL", "PT", "RU", "SE",
            "CN",
        ]
        .into_iter()
        .map(|suffix| (suffix.to_owned(), suffix.to_owned()))
        .chain([(String::new(), "<p>Unsuffixed</p>".to_owned())])
        .collect()
    }

    /// `GetBiography`'s language table (v12.2 `AudioDbArtistProvider.cs:256-275`,
    /// `GetDescription` alike): the subtag picks the field and is the answer's
    /// language; any other language gets the English text (`…EN` first),
    /// marked `en`.
    #[rstest::rstest]
    #[case(Some("de"), "DE", "de")]
    #[case(Some("en"), "EN", "en")]
    #[case(Some("es"), "ES", "es")]
    #[case(Some("fr"), "FR", "fr")]
    #[case(Some("he"), "IL", "he")]
    #[case(Some("hu"), "HU", "hu")]
    #[case(Some("it"), "IT", "it")]
    #[case(Some("ja"), "JP", "ja")]
    #[case(Some("nl"), "NL", "nl")]
    #[case(Some("no"), "NO", "no")]
    #[case(Some("nb"), "NO", "nb")]
    #[case(Some("nn"), "NO", "nn")]
    #[case(Some("pl"), "PL", "pl")]
    #[case(Some("pt"), "PT", "pt")]
    #[case(Some("ru"), "RU", "ru")]
    #[case(Some("sv"), "SE", "sv")]
    #[case(Some("zh"), "CN", "zh")]
    #[case(Some("de-DE"), "DE", "de")]
    #[case(Some("ZH-cn"), "CN", "zh")]
    #[case(Some("ko"), "EN", "en")]
    #[case(None, "EN", "en")]
    #[case(Some(""), "EN", "en")]
    fn the_text_follows_the_items_language(
        #[case] language: Option<&str>,
        #[case] text: &str,
        #[case] result_language: &str,
    ) {
        assert_eq!(
            localized(&every_language(), language),
            (Some(text.to_owned()), result_language.to_owned())
        );
    }

    /// The English fallback without `…EN` is the unsuffixed text, HTML
    /// stripped; a language TheAudioDB has no text in, `en` included, falls
    /// back to it. A localized text that strips to nothing is no overview,
    /// with no fallback, as upstream's `ProcessResult` (it stores `""`).
    #[test]
    fn without_en_the_unsuffixed_text_is_the_english_one() {
        let mut texts = every_language();
        texts.remove("EN");
        texts.remove("FR");
        texts.insert("DE".to_owned(), "<br/>".to_owned());
        let english = (Some("Unsuffixed".to_owned()), "en".to_owned());
        assert_eq!(localized(&texts, Some("en")), english);
        assert_eq!(localized(&texts, Some("fr-CA")), english);
        assert_eq!(localized(&texts, None), english);
        assert_eq!(localized(&texts, Some("de")), (None, "de".to_owned()));
    }

    #[tokio::test]
    #[ignore = "hits the live TheAudioDb API; run with --ignored"]
    async fn live_miles_davis_artist() {
        // Miles Davis's MusicBrainz artist id.
        let a = AudioDbClient::new()
            .artist("561d854a-6a28-4aa7-8c99-323e6ce46c2a")
            .await;
        let a = a.expect("artist");
        assert!(a.biography(None).0.is_some());
        assert!(!a.images.is_empty(), "{:?}", a.images);
    }
}

#[cfg(test)]
mod quota_tests {
    use super::*;
    use crate::mock_http::MockServer;

    #[tokio::test]
    async fn api_quota_blocks_its_own_client_without_blocking_another_provider() {
        let server = MockServer::with_headers(
            vec![("/", "{}".to_owned())],
            "X-RateLimit-Remaining: 0\r\nX-RateLimit-Reset-In: 120\r\n",
        )
        .await;
        let client = AudioDbClient::with_base_url(&server.base_url);
        let first: Option<serde_json::Value> = client.fetch("/artist-mb.php", "id").await;
        assert!(first.is_some());
        let second: Option<serde_json::Value> = client.fetch("/artist-mb.php", "id").await;
        assert!(second.is_none());
        // An independently constructed provider can use the same test origin.
        let other = RateLimiter::new("fanart");
        assert!(
            reqwest::Client::new()
                .get(&server.base_url)
                .send_limited(&other)
                .await
                .is_ok()
        );
    }
}
