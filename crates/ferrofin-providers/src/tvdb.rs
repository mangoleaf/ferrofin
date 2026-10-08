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
//! - Translations: series and episodes keep their name/overview translations.
//!   The scanner selects the configured metadata language, excluding name
//!   aliases, under the plugin defaults: no fallback languages and no original
//!   language fallback. The separate plugin configuration page remains an open
//!   settings work item; it is not implemented by these library preferences.
//!   Seasons select their translated overview through `IsMatch` too.
//! - Episode ordering: TVDB has multiple orderings (`official` = aired, `dvd`,
//!   `absolute`, …). The episode lookup takes a `season_type`, defaulting to
//!   `official`; season 0 is specials.

use crate::rate_limit::CountedBody as _;
use crate::rate_limit::{LimitedRequest as _, RateLimiter};
use std::sync::Mutex;

use ferrofin_model::entities::ImageType;
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

/// How long a resolved TVDB id is reused unless configured otherwise
/// ([`TvdbClient::with_cache_duration`], the server's
/// `FERROFIN_TVDB_CACHE_HOURS`): the plugin's `CacheDurationInHours` default
/// (`PluginConfiguration.cs:14`, `TvdbClientManager.cs:61`), for which its
/// `GetEpisodeTvdbId` caches the episode id it found (`:585-683`) and its
/// `GetSeriesExtendedByIdAsync` the series record a season's id is read
/// from (`:251-271`).
pub const DEFAULT_CACHE_DURATION: std::time::Duration = std::time::Duration::from_hours(1);

/// `(series id, season type, season, episode)` → the episode's TVDB id and
/// when it was resolved.
type EpisodeIdCache = std::collections::HashMap<(i64, String, i32, i32), (std::time::Instant, i64)>;

/// Series id → the seasons its extended record lists and when it was read.
type SeasonIdCache = std::collections::HashMap<i64, (std::time::Instant, Vec<TvdbSeasonRef>)>;

/// One season a series' extended record lists (`SeasonBaseRecord`): what
/// `TvdbSeasonProvider` picks a season's id by.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TvdbSeasonRef {
    /// The season's TVDB id.
    id: i64,
    /// The season number in its ordering.
    number: i64,
    /// The ordering it belongs to (`Type.Type`: `official`, `dvd`,
    /// `absolute`, …).
    season_type: String,
}

/// A credited person from TVDB (`Character`), mapped toward a Ferrofin person row.
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
    /// Name translations returned by the extended-record endpoint.
    pub name_translations: Vec<TvdbTranslation>,
    /// Overview translations returned by the extended-record endpoint.
    pub overview_translations: Vec<TvdbTranslation>,
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
    /// The days the series airs (`airsDays`, the true flags in weekday order),
    /// which the TVDB plugin maps onto `Series.AirDays`.
    pub air_days: Vec<ferrofin_model::dto::DayOfWeek>,
    /// The air time (`airsTime`, e.g. `20:00`) — `Series.AirTime`.
    pub air_time: Option<String>,
    /// The URL slug (`TvdbSlug`).
    pub slug: Option<String>,
    /// Official TVDB list ids, separated by semicolons as in the plugin.
    pub collection_ids: Option<String>,
    /// Cross-provider ids resolved from `remoteIds`.
    pub imdb_id: Option<String>,
    /// The TMDB id, if TVDB carries one.
    pub tmdb_id: Option<String>,
    /// The Zap2It id, if present.
    pub zap2it_id: Option<String>,
    /// Cast + key crew.
    pub people: Vec<TvdbPerson>,
    /// All artwork, mapped to Ferrofin image types (rich, for the "Choose Image"
    /// listing). Use [`download_images`](TvdbSeriesDetails::download_images) for
    /// the type+URL pairs the scanner downloads.
    pub images: Vec<TmdbImage>,
}

impl TvdbSeriesDetails {
    /// Applies the metadata language with the plugin's default empty fallback
    /// list and `FallbackToOriginalLanguage=false`. Alias names are excluded.
    pub fn localize(&mut self, language: &str, three_letter_names: &[String]) {
        self.name = translated_text(&self.name_translations, language, three_letter_names, true);
        self.overview = translated_text(
            &self.overview_translations,
            language,
            three_letter_names,
            false,
        );
    }

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
    /// The episode's own TVDB id.
    pub tvdb_id: i64,
    /// The episode's IMDb id.
    pub imdb_id: Option<String>,
    /// Special's position in aired order.
    pub airs_before_episode: Option<i64>,
    /// Season after which a special airs.
    pub airs_after_season: Option<i64>,
    /// Season before which a special airs.
    pub airs_before_season: Option<i64>,
    /// The episode name.
    pub name: Option<String>,
    /// The overview.
    pub overview: Option<String>,
    /// Name translations returned by the extended-record endpoint.
    pub name_translations: Vec<TvdbTranslation>,
    /// Overview translations returned by the extended-record endpoint.
    pub overview_translations: Vec<TvdbTranslation>,
    /// Aired date (`YYYY-MM-DD`).
    pub aired: Option<String>,
    /// Production year, derived from `aired`.
    pub production_year: Option<i32>,
    /// The still/primary image URL, if any.
    pub image_url: Option<String>,
    /// Cast + crew credited on the episode.
    pub people: Vec<TvdbPerson>,
}

impl TvdbEpisodeDetails {
    /// Applies the metadata language with the plugin's default fallback rules.
    pub fn localize(&mut self, language: &str, three_letter_names: &[String]) {
        self.name = translated_text(&self.name_translations, language, three_letter_names, true);
        self.overview = translated_text(
            &self.overview_translations,
            language,
            three_letter_names,
            false,
        );
    }
}

fn translated_text(
    translations: &[TvdbTranslation],
    language: &str,
    three_letter_names: &[String],
    exclude_aliases: bool,
) -> Option<String> {
    translations
        .iter()
        .find(|translation| {
            (!exclude_aliases || !translation.is_alias)
                && translation_matches(&translation.language, language, three_letter_names)
        })
        .and_then(|translation| translation.text.clone())
}

/// Mapped season metadata (`/seasons/{id}/extended?meta=translations`, the
/// plugin's `CustomSeasonExtendedRecord`).
///
/// A season record carries no base-language overview: its text is in its
/// `translations` only, which is where `TvdbSeasonProvider` reads it from
/// ([`translated_overview`](Self::translated_overview)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TvdbSeasonDetails {
    /// The season's own TVDB id (`season.Id`), which the plugin sets as the
    /// season's `Tvdb` provider id (`SetTvdbId`, only when positive).
    pub tvdb_id: Option<i64>,
    /// The season's base-language name, if any.
    pub name: Option<String>,
    /// The poster/primary image URL, if any.
    pub image_url: Option<String>,
    /// The name translations (`translations.nameTranslations`).
    pub name_translations: Vec<TvdbTranslation>,
    /// The overview translations (`translations.overviewTranslations`).
    pub overview_translations: Vec<TvdbTranslation>,
}

/// One translation of a TVDB record's name or overview.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TvdbTranslation {
    /// TVDB's language code: ISO 639-2 (`eng`, `fra`), or one of its own
    /// (`zhtw`, `pt`, `por`).
    pub language: String,
    /// The translated text; `None` when the translation carries none.
    pub text: Option<String>,
    /// An alias rather than the record's name in that language (`isAlias`).
    pub is_alias: bool,
}

impl TvdbSeasonDetails {
    /// The season's overview in `language` —
    /// `TvdbSdkExtensions.GetTranslatedOverviewOrDefault` (`:89-101`): the
    /// overview of the first translation in that language
    /// ([`translation_matches`]), else of one in the plugin's
    /// `FallbackLanguages`. Ferrofin exposes no TheTVDB settings page, so
    /// those are the plugin's default — none — and a season TVDB has no
    /// translation for in `language` gets no overview, as upstream's does.
    #[must_use]
    pub fn translated_overview(
        &self,
        language: &str,
        three_letter_names: &[String],
    ) -> Option<String> {
        self.overview_translations
            .iter()
            .find(|t| translation_matches(&t.language, language, three_letter_names))
            .and_then(|t| t.text.clone())
    }
}

/// `TvdbCultureInfo.GetCultureInfo` (`TvdbCultureInfo.cs:30-44`) over the
/// server's culture table (`ILocalizationManager.GetCultures`, which the
/// plugin keeps): the ISO 639-2 codes (`ThreeLetterISOLanguageNames`) of the
/// first culture whose display name, name, ISO 639-2 codes or two-letter code
/// is `language` (any case); empty when none is.
#[must_use]
pub fn three_letter_language_names(
    cultures: &[ferrofin_model::globalization::CultureDto],
    language: &str,
) -> Vec<String> {
    cultures
        .iter()
        .find(|c| {
            language.eq_ignore_ascii_case(&c.display_name)
                || language.eq_ignore_ascii_case(&c.name)
                || c.three_letter_iso_language_names
                    .iter()
                    .any(|n| language.eq_ignore_ascii_case(n))
                || language.eq_ignore_ascii_case(&c.two_letter_iso_language_name)
        })
        .map(|c| c.three_letter_iso_language_names.clone())
        .unwrap_or_default()
}

/// `TvdbSdkExtensions.IsMatch` (`:103-127`): whether a TVDB translation's
/// `translation` language is the item's metadata `language`. TVDB's own codes
/// for Traditional Chinese, Brazilian and European Portuguese are matched by
/// name (`zh-TW` → `zhtw`, `pt-BR` → `pt`, `pt-PT` → `por`); any other
/// language matches the ISO 639-2 codes of its culture —
/// `three_letter_names`, the `ThreeLetterISOLanguageNames` the server's
/// culture table lists for `language` (`TvdbCultureInfo.GetCultureInfo`,
/// fed from `ILocalizationManager.GetCultures`). A blank `language` matches
/// nothing.
#[must_use]
pub fn translation_matches(
    translation: &str,
    language: &str,
    three_letter_names: &[String],
) -> bool {
    if language.trim().is_empty() {
        return false;
    }
    let mapped = match language.to_lowercase().as_str() {
        "zh-tw" => Some("zhtw"),
        "pt-br" => Some("pt"),
        "pt-pt" => Some("por"),
        _ => None,
    };
    match mapped {
        Some(mapped) => translation.eq_ignore_ascii_case(mapped),
        None => three_letter_names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(translation)),
    }
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
    translations: Option<TranslationsWire>,
    lists: Option<Vec<ListWire>>,
    seasons: Option<Vec<SeasonRefWire>>,
    name: Option<String>,
    slug: Option<String>,
    overview: Option<String>,
    #[serde(alias = "firstAired")]
    first_aired: Option<String>,
    #[serde(alias = "lastAired")]
    last_aired: Option<String>,
    #[serde(alias = "averageRuntime")]
    average_runtime: Option<i32>,
    #[serde(alias = "airsDays")]
    airs_days: Option<AirsDaysWire>,
    #[serde(alias = "airsTime")]
    airs_time: Option<String>,
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

/// TVDB v4's `airsDays` object: one flag per weekday — the wire shape is
/// seven booleans, so the bool-heavy-struct lint does not apply.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default, Deserialize)]
struct AirsDaysWire {
    #[serde(default)]
    sunday: bool,
    #[serde(default)]
    monday: bool,
    #[serde(default)]
    tuesday: bool,
    #[serde(default)]
    wednesday: bool,
    #[serde(default)]
    thursday: bool,
    #[serde(default)]
    friday: bool,
    #[serde(default)]
    saturday: bool,
}

impl AirsDaysWire {
    /// The flagged days in weekday order (Sunday first, as `DayOfWeek`).
    fn days(&self) -> Vec<ferrofin_model::dto::DayOfWeek> {
        use ferrofin_model::dto::DayOfWeek;
        [
            (self.sunday, DayOfWeek::Sunday),
            (self.monday, DayOfWeek::Monday),
            (self.tuesday, DayOfWeek::Tuesday),
            (self.wednesday, DayOfWeek::Wednesday),
            (self.thursday, DayOfWeek::Thursday),
            (self.friday, DayOfWeek::Friday),
            (self.saturday, DayOfWeek::Saturday),
        ]
        .into_iter()
        .filter_map(|(airs, day)| airs.then_some(day))
        .collect()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListWire {
    id: Option<i64>,
    #[serde(default)]
    is_official: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EpisodeExtendedWire {
    translations: Option<TranslationsWire>,
    remote_ids: Option<Vec<RemoteIdWire>>,
    airs_before_episode: Option<i64>,
    airs_after_season: Option<i64>,
    airs_before_season: Option<i64>,
    name: Option<String>,
    overview: Option<String>,
    aired: Option<String>,
    image: Option<String>,
    characters: Option<Vec<CharacterWire>>,
}

/// A series record's `seasons` entry (`SeasonBaseRecord`).
#[derive(Debug, Deserialize)]
struct SeasonRefWire {
    id: Option<i64>,
    number: Option<i64>,
    #[serde(rename = "type")]
    season_type: Option<SeasonTypeWire>,
}

/// `SeasonType`: the ordering a season belongs to.
#[derive(Debug, Deserialize)]
struct SeasonTypeWire {
    #[serde(rename = "type")]
    kind: Option<String>,
}

impl SeasonRefWire {
    /// The season as the cache keeps it; `None` without an id, a number or
    /// an ordering.
    fn into_ref(self) -> Option<TvdbSeasonRef> {
        Some(TvdbSeasonRef {
            id: self.id.filter(|id| *id > 0)?,
            number: self.number?,
            season_type: self.season_type?.kind?,
        })
    }
}

/// The part of a series' extended record that lists its seasons — all the
/// short record (`?short=true`) the season provider asks for is read for.
#[derive(Debug, Deserialize)]
struct SeriesSeasonsWire {
    seasons: Option<Vec<SeasonRefWire>>,
}

#[derive(Debug, Deserialize)]
struct SeasonExtendedWire {
    id: Option<i64>,
    name: Option<String>,
    image: Option<String>,
    translations: Option<TranslationsWire>,
}

/// `TranslationExtended`, the `meta=translations` block.
#[derive(Debug, Deserialize)]
struct TranslationsWire {
    #[serde(alias = "nameTranslations")]
    name_translations: Option<Vec<TranslationWire>>,
    #[serde(alias = "overviewTranslations")]
    overview_translations: Option<Vec<TranslationWire>>,
}

/// One `Translation`.
#[derive(Debug, Deserialize)]
struct TranslationWire {
    language: Option<String>,
    name: Option<String>,
    overview: Option<String>,
    #[serde(alias = "isAlias")]
    is_alias: Option<bool>,
}

/// A translation list as [`TvdbTranslation`]s of the field `text` reads; an
/// entry with no language is dropped (nothing can match it).
fn translations_of(
    list: Option<Vec<TranslationWire>>,
    text: fn(TranslationWire) -> Option<String>,
) -> Vec<TvdbTranslation> {
    list.unwrap_or_default()
        .into_iter()
        .filter_map(|t| {
            let language = t.language.clone().filter(|l| !l.is_empty())?;
            let is_alias = t.is_alias.unwrap_or(false);
            Some(TvdbTranslation {
                language,
                text: text(t),
                is_alias,
            })
        })
        .collect()
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
    limiter: RateLimiter,
    api_key: SecretString,
    pin: Option<String>,
    token: Mutex<Option<String>>,
    base_url: String,
    /// The episode ids [`episode_by_number`](Self::episode_by_number)
    /// resolved, kept for `cache_duration` as the
    /// plugin keeps them, so a refresh repeated within that time asks for
    /// the episode record only.
    episode_ids: Mutex<EpisodeIdCache>,
    /// The seasons each series' extended record listed, kept for
    /// `cache_duration` as the plugin keeps the
    /// record ([`season_id`](Self::season_id)).
    season_ids: Mutex<SeasonIdCache>,
    /// How long both caches keep an entry: the plugin's
    /// `CacheDurationInHours` ([`DEFAULT_CACHE_DURATION`] unless set).
    cache_duration: std::time::Duration,
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
            limiter: RateLimiter::new("tvdb"),
            api_key: SecretString::from(key.to_owned()),
            pin: (!pin.is_empty()).then(|| pin.to_owned()),
            token: Mutex::new(None),
            base_url: API_BASE.to_owned(),
            episode_ids: Mutex::new(EpisodeIdCache::new()),
            season_ids: Mutex::new(SeasonIdCache::new()),
            cache_duration: DEFAULT_CACHE_DURATION,
        }
    }

    /// Sets how long resolved ids are reused — the plugin's
    /// `CacheDurationInHours` setting, which covers the episode ids and each
    /// series' season list.
    #[must_use]
    pub fn with_cache_duration(mut self, duration: std::time::Duration) -> Self {
        self.cache_duration = duration;
        self
    }

    /// Points the client at a different API root (a mock server in tests).
    ///
    /// `pub`, matching `TmdbClient::with_base_url`: while this was
    /// crate-private, every TVDB **hit** path in the library scan was
    /// structurally unreachable from `ferrofin-core`'s tests, so those branches
    /// could be deleted with the whole suite still green.
    #[must_use]
    pub fn with_base_url(mut self, base_url: &str) -> Self {
        base_url
            .trim_end_matches('/')
            .clone_into(&mut self.base_url);
        self
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
            .post(format!("{}/login", self.base_url))
            .json(&body)
            .send_limited(&self.limiter)
            .await
            .ok()?;
        let env: Envelope<LoginData> = resp.counted_json().await.ok()?;
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
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(token)
            .query(query)
            .send_limited(&self.limiter)
            .await
            .ok()?;
        let env: Envelope<T> = resp.counted_json().await.ok()?;
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

    /// Resolves an IMDb, Zap2It or TMDB id through TVDB's remote-id search.
    /// Only series results are eligible; unrelated movie/person hits are ignored.
    pub async fn series_by_remote_id(&self, remote_id: &str) -> Option<i64> {
        #[derive(Deserialize)]
        struct SeriesRef {
            id: Option<i64>,
        }
        #[derive(Deserialize)]
        struct Hit {
            series: Option<SeriesRef>,
        }
        let remote_id = remote_id.trim();
        if remote_id.is_empty() {
            return None;
        }
        // Provider ids are alphanumeric (Zap2It also uses punctuation).
        // Do not let malformed stored ids alter the URL path or query.
        if !remote_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        {
            return None;
        }
        let encoded = remote_id;
        let hits: Vec<Hit> = self
            .get(&format!("/search/remoteid/{encoded}"), &[])
            .await?;
        hits.into_iter()
            .find_map(|hit| hit.series.and_then(|s| s.id).filter(|id| *id > 0))
    }

    /// Fetches and maps full series metadata + artwork for `country_code`
    /// (three-letter ISO, for the content-rating pick). Port of
    /// `FetchSeriesMetadata`.
    pub async fn series_details(
        &self,
        tvdb_id: i64,
        country_code: &str,
    ) -> Option<TvdbSeriesDetails> {
        let mut wire: SeriesExtendedWire = self
            .get(
                &format!("/series/{tvdb_id}/extended"),
                &[("meta", "translations".to_owned())],
            )
            .await?;
        // The full record lists the same seasons as the short one the
        // season provider reads them from, so a season refreshed after its
        // series resolves its id with no request of its own.
        self.remember_seasons(tvdb_id, wire.seasons.take());
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
        let (names, overviews) = wire.translations.map_or((None, None), |t| {
            (t.name_translations, t.overview_translations)
        });
        Some(TvdbEpisodeDetails {
            tvdb_id: episode_id,
            imdb_id: wire
                .remote_ids
                .unwrap_or_default()
                .into_iter()
                .find(|id| {
                    id.source_name
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case("IMDB"))
                })
                .and_then(|id| id.id)
                .map(|id| remote_id_to_string(&id)),
            airs_before_episode: wire.airs_before_episode,
            airs_after_season: wire.airs_after_season,
            airs_before_season: wire.airs_before_season,
            name: wire.name,
            overview: wire.overview,
            name_translations: translations_of(names, |t| t.name),
            overview_translations: translations_of(overviews, |t| t.overview),
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
        let key = (series_id, season_type.to_owned(), season, number);
        let cached = self.episode_ids.lock().ok().and_then(|ids| {
            ids.get(&key)
                .filter(|(at, _)| at.elapsed() < self.cache_duration)
                .map(|(_, id)| *id)
        });
        let id = if let Some(id) = cached {
            id
        } else {
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
            // Only a found id is kept, as the plugin keeps it; the expired
            // entries go on every write, so the map holds at most one TTL's
            // worth of lookups.
            if let Ok(mut ids) = self.episode_ids.lock() {
                ids.retain(|_, (at, _)| at.elapsed() < self.cache_duration);
                ids.insert(key, (std::time::Instant::now(), id));
            }
            id
        };
        self.episode_details(id).await
    }

    /// The TVDB id of season `number` of series `series_id` in the
    /// `season_type` ordering — the identity step of `TvdbSeasonProvider.
    /// GetMetadata` (`:76-90`): the first season the series' extended record
    /// lists with that number and an ordering of that name (any case).
    ///
    /// The plugin reads the short record (`GetSeriesExtendedByIdAsync(…,
    /// small: true)`) and caches it for `CacheDurationInHours`
    /// (`TvdbClientManager.cs:251-271`); this keeps the record's season list
    /// for `cache_duration`, also when [`series_details`](Self::series_details)
    /// read the full record, which lists the same seasons. `None` when the
    /// series lists no such season, or the record could not be read (a
    /// failure, counted by the request).
    pub async fn season_id(&self, series_id: i64, season_type: &str, number: i32) -> Option<i64> {
        let pick = |seasons: &[TvdbSeasonRef]| {
            seasons
                .iter()
                .find(|s| {
                    s.number == i64::from(number) && s.season_type.eq_ignore_ascii_case(season_type)
                })
                .map(|s| s.id)
        };
        let cached = self.season_ids.lock().ok().and_then(|cache| {
            cache
                .get(&series_id)
                .filter(|(at, _)| at.elapsed() < self.cache_duration)
                .map(|(_, seasons)| pick(seasons))
        });
        if let Some(found) = cached {
            return found;
        }
        let wire: SeriesSeasonsWire = self
            .get(
                &format!("/series/{series_id}/extended"),
                &[("short", "true".to_owned())],
            )
            .await?;
        let seasons = self.remember_seasons(series_id, wire.seasons);
        pick(&seasons)
    }

    /// Keeps the seasons a series' extended record listed for
    /// `cache_duration` and returns them. The expired entries go on every
    /// write, so the map holds at most one cache duration's worth of series.
    fn remember_seasons(
        &self,
        series_id: i64,
        seasons: Option<Vec<SeasonRefWire>>,
    ) -> Vec<TvdbSeasonRef> {
        let seasons: Vec<TvdbSeasonRef> = seasons
            .unwrap_or_default()
            .into_iter()
            .filter_map(SeasonRefWire::into_ref)
            .collect();
        if let Ok(mut cache) = self.season_ids.lock() {
            cache.retain(|_, (at, _)| at.elapsed() < self.cache_duration);
            cache.insert(series_id, (std::time::Instant::now(), seasons.clone()));
        }
        seasons
    }

    /// Fetches and maps a season's record with its translations
    /// (`GetSeasonByIdAsync`, `/seasons/{id}/extended?meta=translations`,
    /// `TvdbClientManager.cs:309-327`).
    pub async fn season_details(&self, season_id: i64) -> Option<TvdbSeasonDetails> {
        let wire: SeasonExtendedWire = self
            .get(
                &format!("/seasons/{season_id}/extended"),
                &[("meta", "translations".to_owned())],
            )
            .await?;
        let (names, overviews) = wire.translations.map_or((None, None), |t| {
            (t.name_translations, t.overview_translations)
        });
        Some(TvdbSeasonDetails {
            tvdb_id: wire.id.filter(|id| *id > 0),
            name: wire.name,
            image_url: wire.image,
            name_translations: translations_of(names, |t| t.name),
            overview_translations: translations_of(overviews, |t| t.overview),
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
        let resp = crate::image_download::send(&self.http, url).await.ok()?;
        resp.counted_bytes().await.ok()
    }
}

/// The four-digit year in a `YYYY-MM-DD` (or `YYYY…`) date string.
fn year_of(date: &str) -> Option<i32> {
    date.get(0..4).and_then(|y| y.parse::<i32>().ok())
}

/// Maps a TVDB artwork `type` id to a Ferrofin [`ImageType`], mirroring the
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
    let (names, overviews) = wire.translations.map_or((None, None), |t| {
        (t.name_translations, t.overview_translations)
    });
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
        name_translations: translations_of(names, |t| t.name),
        overview_translations: translations_of(overviews, |t| t.overview),
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
        air_days: wire
            .airs_days
            .as_ref()
            .map(AirsDaysWire::days)
            .unwrap_or_default(),
        air_time: wire.airs_time.filter(|t| !t.is_empty()),
        slug: wire.slug,
        collection_ids: official_collection_ids(wire.lists),
        imdb_id: find_remote("IMDB"),
        tmdb_id: find_remote("TheMovieDB.com")
            .map(|id| id.split('-').next().unwrap_or_default().to_owned()),
        zap2it_id: find_remote("Zap2It"),
        people: map_characters(wire.characters),
        images,
    }
}

/// TVDB's official list ids use the plugin's trailing-semicolon format.
fn official_collection_ids(lists: Option<Vec<ListWire>>) -> Option<String> {
    use std::fmt::Write as _;
    let mut ids = String::new();
    for id in lists
        .unwrap_or_default()
        .into_iter()
        .filter(|list| list.is_official)
        .filter_map(|list| list.id.filter(|id| *id > 0))
    {
        let _ = write!(ids, "{id};");
    }
    (!ids.is_empty()).then_some(ids)
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

    #[test]
    fn series_and_episode_localization_exclude_aliases_and_do_not_fall_back_to_base_text() {
        let names = vec![
            TvdbTranslation {
                language: "deu".into(),
                text: Some("Alias".into()),
                is_alias: true,
            },
            TvdbTranslation {
                language: "deu".into(),
                text: Some("Deutsch".into()),
                is_alias: false,
            },
            TvdbTranslation {
                language: "eng".into(),
                text: Some("English".into()),
                is_alias: false,
            },
        ];
        let overviews = vec![TvdbTranslation {
            language: "deu".into(),
            text: Some("Beschreibung".into()),
            is_alias: false,
        }];
        let mut series = TvdbSeriesDetails {
            name: Some("Base name".into()),
            overview: Some("Base overview".into()),
            name_translations: names.clone(),
            overview_translations: overviews.clone(),
            ..Default::default()
        };
        let mut episode = TvdbEpisodeDetails {
            name: Some("Base episode".into()),
            overview: Some("Base synopsis".into()),
            name_translations: names,
            overview_translations: overviews,
            ..Default::default()
        };
        let codes = ["deu".to_owned(), "ger".to_owned()];
        series.localize("de", &codes);
        episode.localize("de", &codes);
        assert_eq!(series.name.as_deref(), Some("Deutsch"));
        assert_eq!(episode.name, series.name);
        assert_eq!(series.overview.as_deref(), Some("Beschreibung"));
        assert_eq!(episode.overview, series.overview);
        series.localize("fr", &["fra".into()]);
        episode.localize("fr", &["fra".into()]);
        assert_eq!((series.name, series.overview), (None, None));
        assert_eq!((episode.name, episode.overview), (None, None));
    }

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
              "airsDays": { "sunday": true, "monday": false, "friday": true },
              "airsTime": "21:00",
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

    #[tokio::test]
    async fn remote_ids_resolve_only_series_and_episode_fields_survive() {
        let server = crate::mock_http::MockServer::start(vec![
            ("/login", r#"{"data":{"token":"test"}}"#.into()),
            ("/search/remoteid/tt123", r#"{"data":[{"movie":{"id":1}},{"series":{"id":0}},{"series":{"id":42}}]}"#.into()),
            ("/search/remoteid/99", r#"{"data":[]}"#.into()),
            ("/episodes/8/extended", r#"{"data":{"name":"Special","airsBeforeEpisode":2,"airsBeforeSeason":1,"airsAfterSeason":0,"remoteIds":[{"sourceName":"IMDB","id":"tt456"}]}}"#.into()),
        ]).await;
        let client = TvdbClient::new().with_base_url(&server.base_url);
        assert_eq!(client.series_by_remote_id("tt123").await, Some(42));
        assert_eq!(client.series_by_remote_id("99").await, None);
        assert_eq!(client.series_by_remote_id("bad?query").await, None);
        assert_eq!(client.series_by_remote_id(" ").await, None);
        let ep = client.episode_details(8).await.unwrap();
        assert_eq!(ep.tvdb_id, 8);
        assert_eq!(ep.imdb_id.as_deref(), Some("tt456"));
        assert_eq!(ep.airs_before_episode, Some(2));
        assert_eq!(ep.airs_before_season, Some(1));
        assert_eq!(ep.airs_after_season, Some(0));
    }

    #[test]
    fn series_maps_official_lists_and_normalizes_tmdb_urls() {
        let wire = serde_json::from_str(r#"{"lists":[{"id":1,"isOfficial":true},{"id":2,"isOfficial":false},{"id":3,"isOfficial":true}],"remoteIds":[{"sourceName":"TheMovieDB.com","id":"1399-game-of-thrones"}]}"#).unwrap();
        let details = map_series(42, wire, "usa");
        assert_eq!(details.collection_ids.as_deref(), Some("1;3;"));
        assert_eq!(details.tmdb_id.as_deref(), Some("1399"));
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
        assert_eq!(
            d.air_days,
            [
                ferrofin_model::dto::DayOfWeek::Sunday,
                ferrofin_model::dto::DayOfWeek::Friday
            ]
        );
        assert_eq!(d.air_time.as_deref(), Some("21:00"));
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
    async fn client_methods_over_mock_server() {
        use crate::mock_http::MockServer;

        let series = r#"{"data":{"name":"Game of Thrones","slug":"got",
          "firstAired":"2011-04-17","lastAired":"2019-05-19","averageRuntime":60,
          "status":{"name":"Ended"},"genres":[{"name":"Drama"}],
          "contentRatings":[{"name":"TV-MA","country":"usa"}],
          "remoteIds":[{"id":"tt0944947","sourceName":"IMDB"},{"id":1399,"sourceName":"TheMovieDB.com"}],
          "latestNetwork":{"name":"HBO"},
          "characters":[{"personName":"Emilia Clarke","name":"Daenerys","peopleType":"Actor"}],
          "artworks":[{"image":"/p.jpg","type":2,"language":"eng"}]}}"#;
        let server = MockServer::start(vec![
            ("/login", r#"{"data":{"token":"tok"}}"#.to_owned()),
            (
                "/search",
                r#"{"data":[{"tvdb_id":"121361","name":"Game of Thrones","year":"2011","image_url":"/g.jpg"}]}"#.to_owned(),
            ),
            (
                "/episodes/official",
                r#"{"data":{"episodes":[{"id":9,"seasonNumber":1,"number":1}]}}"#.to_owned(),
            ),
            (
                "/episodes/",
                r#"{"data":{"name":"Winter Is Coming","overview":"Ned is summoned.","aired":"2011-04-17","image":"/s.jpg","characters":[{"personName":"Sean Bean","peopleType":"Actor"}]}}"#.to_owned(),
            ),
            ("/series/", series.to_owned()),
            (
                "/seasons/",
                r#"{"data":{"name":"Season 1","overview":"first","image":"/se.jpg"}}"#.to_owned(),
            ),
            (
                "/people/",
                r#"{"data":{"biography":"An actress.","birthDate":"1986-10-23","birthPlace":"London"}}"#.to_owned(),
            ),
            ("/img.jpg", "IMAGEBYTES".to_owned()),
        ])
        .await;
        let c = TvdbClient::new().with_base_url(&server.base_url);

        let hits = c.search("Game of Thrones", Some(2011)).await;
        assert_eq!(hits.first().map(|h| h.tvdb_id), Some(121_361));

        let d = c.series_details(121_361, "usa").await.expect("series");
        assert_eq!(d.name.as_deref(), Some("Game of Thrones"));
        assert_eq!(d.imdb_id.as_deref(), Some("tt0944947"));
        assert_eq!(d.tmdb_id.as_deref(), Some("1399"));
        assert_eq!(d.images.len(), 1);
        assert_eq!(d.people.len(), 1);

        let ep = c
            .episode_by_number(121_361, DEFAULT_SEASON_TYPE, 1, 1)
            .await
            .expect("episode");
        assert_eq!(ep.name.as_deref(), Some("Winter Is Coming"));
        assert_eq!(ep.image_url.as_deref(), Some("/s.jpg"));

        let season = c.season_details(1).await.expect("season");
        assert_eq!(season.name.as_deref(), Some("Season 1"));

        let person = c.person_details(1).await.expect("person");
        assert_eq!(person.biography.as_deref(), Some("An actress."));
        assert_eq!(person.birthplace.as_deref(), Some("London"));

        let bytes = c
            .download(&format!("{}/img.jpg", server.base_url))
            .await
            .expect("download");
        assert_eq!(bytes, b"IMAGEBYTES");
    }

    #[tokio::test]
    async fn login_failure_yields_no_data() {
        use crate::mock_http::MockServer;
        // No /login route → token() fails → every call returns None/empty.
        let server = MockServer::start(vec![("/search", "{}".to_owned())]).await;
        let c = TvdbClient::new().with_base_url(&server.base_url);
        assert!(c.search("x", None).await.is_empty());
        assert!(c.series_details(1, "usa").await.is_none());
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

    /// An episode's resolved id is reused within the plugin's cache
    /// duration: a second lookup of the same episode asks only for its
    /// record; another episode is looked up again.
    #[tokio::test]
    async fn an_episode_id_is_resolved_once_per_cache_duration() {
        use std::io::{Read as _, Write as _};
        let lookups = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&lookups);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let payload = if req.contains("/login") {
                    r#"{"data":{"token":"tok"}}"#
                } else if req.contains("/episodes/official") {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    r#"{"data":{"episodes":[{"id":9,"seasonNumber":1,"number":1},
                        {"id":10,"seasonNumber":1,"number":2}]}}"#
                } else {
                    r#"{"data":{"name":"An episode"}}"#
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
            }
        });
        let c = TvdbClient::new().with_base_url(&format!("http://{addr}"));
        for _ in 0..2 {
            assert!(
                c.episode_by_number(121_361, DEFAULT_SEASON_TYPE, 1, 1)
                    .await
                    .is_some()
            );
        }
        assert_eq!(lookups.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            c.episode_by_number(121_361, DEFAULT_SEASON_TYPE, 1, 2)
                .await
                .is_some()
        );
        assert_eq!(lookups.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// The configured cache duration (`FERROFIN_TVDB_CACHE_HOURS`, the
    /// plugin's `CacheDurationInHours`) bounds both caches: once it has
    /// passed, the episode id and the series' season list are read again.
    #[tokio::test]
    async fn the_cache_duration_bounds_both_caches() {
        use std::io::{Read as _, Write as _};
        let episode_lists = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let season_lists = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (episodes, seasons) = (
            std::sync::Arc::clone(&episode_lists),
            std::sync::Arc::clone(&season_lists),
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let payload = if req.contains("/login") {
                    r#"{"data":{"token":"tok"}}"#
                } else if req.contains("/episodes/official") {
                    episodes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    r#"{"data":{"episodes":[{"id":9,"seasonNumber":1,"number":1}]}}"#
                } else if req.contains("/series/5/extended") {
                    seasons.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    r#"{"data":{"seasons":[{"id":51,"number":1,"type":{"type":"official"}}]}}"#
                } else {
                    r#"{"data":{"name":"An episode"}}"#
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
            }
        });
        let c = TvdbClient::new()
            .with_base_url(&format!("http://{addr}"))
            .with_cache_duration(std::time::Duration::from_nanos(1));
        for _ in 0..2 {
            assert_eq!(c.season_id(5, DEFAULT_SEASON_TYPE, 1).await, Some(51));
            assert!(
                c.episode_by_number(121_361, DEFAULT_SEASON_TYPE, 1, 1)
                    .await
                    .is_some()
            );
        }
        assert_eq!(season_lists.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(episode_lists.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
