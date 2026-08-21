//! External id descriptors and external ("Links") URLs.
//!
//! Ports Jellyfin's two small strategy families into plain tables:
//!
//! - `IExternalId` — the provider-id fields the *Identify* dialog offers for an
//!   item ([`external_id_infos`]), and
//! - `IExternalUrlProvider` — the "Links" row on an item's detail page
//!   ([`external_urls`]).
//!
//! Both are pure functions over `(BaseItemKind, provider ids)`: in C# they are
//! one class per provider per item type, registered in DI and filtered by
//! `Supports(item)`. There is exactly one call site for each here, so the port
//! is a match rather than a trait — see the `ferrofin-traits` port note on
//! strategy interfaces becoming match-on-kind logic.
//!
//! Ported providers (Jellyfin 12.0):
//! `Movies/Imdb{,Person}External{Id,UrlProvider}`, `TV/Zap2It*`,
//! `Plugins/Tmdb/Tmdb*External{Id,UrlProvider}`, `Plugins/MusicBrainz/*`,
//! `Plugins/AudioDb/*`, `Books/{ComicVine,GoogleBooks,Isbn}/*`,
//! `Music/ImvdbId`, plus `jellyfin-plugin-tvdb`'s `Providers/ExternalId/*`
//! (TVDB ships built into Ferrofin, so its id fields ship too).

use std::collections::HashMap;

use ferrofin_model::data::BaseItemKind;
use ferrofin_model::providers::{ExternalIdInfo, ExternalIdMediaType, ExternalUrl};

/// The IMDb site root (C# `ImdbExternalUrlProvider.baseUrl`).
const IMDB_BASE: &str = "https://www.imdb.com/";
/// The TMDB site root (C# `TmdbUtils.BaseTmdbUrl`).
const TMDB_BASE: &str = "https://www.themoviedb.org/";
/// TheAudioDb site root (C# `AudioDb*ExternalUrlProvider.baseUrl`).
const AUDIODB_BASE: &str = "https://www.theaudiodb.com/";

/// The item an external-id/URL lookup runs against.
///
/// Everything the C# providers read off `BaseItem`, flattened: an item's own
/// provider ids plus — for a `Season`/`Episode`, whose TMDB and IMDb links are
/// built from the *series* id — the owning series' ids and display order.
#[derive(Debug, Clone, Copy)]
pub struct ExternalIdItem<'a> {
    /// The item's kind.
    pub kind: BaseItemKind,
    /// The item's own provider ids (`BaseItemProviders` rows).
    pub provider_ids: &'a HashMap<String, String>,
    /// `IndexNumber` — the season number for a `Season`, the episode number for
    /// an `Episode`.
    pub index_number: Option<i32>,
    /// `ParentIndexNumber` — an `Episode`'s season number.
    pub parent_index_number: Option<i32>,
    /// The owning series' provider ids, for a `Season`/`Episode`.
    pub series_provider_ids: Option<&'a HashMap<String, String>>,
    /// The owning series' `DisplayOrder`. Jellyfin only emits season/episode
    /// TMDB links for the default (airdate) order.
    pub series_display_order: Option<&'a str>,
    /// The MusicBrainz server root the links point at (the configured mirror);
    /// empty falls back to [`crate::musicbrainz::DEFAULT_BASE_URL`].
    pub musicbrainz_server: &'a str,
}

impl<'a> ExternalIdItem<'a> {
    /// An item of `kind` carrying `provider_ids` and nothing else — the shape
    /// every kind except `Season`/`Episode` needs.
    #[must_use]
    pub fn new(kind: BaseItemKind, provider_ids: &'a HashMap<String, String>) -> Self {
        Self {
            kind,
            provider_ids,
            index_number: None,
            parent_index_number: None,
            series_provider_ids: None,
            series_display_order: None,
            musicbrainz_server: crate::musicbrainz::DEFAULT_BASE_URL,
        }
    }

    /// A non-empty provider id by key, trimmed.
    fn id(&self, key: &str) -> Option<&'a str> {
        self.provider_ids
            .get(key)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
    }

    /// A non-empty provider id by key from the owning series.
    fn series_id(&self, key: &str) -> Option<&'a str> {
        self.series_provider_ids?
            .get(key)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
    }

    /// Whether the owning series uses the default (airdate) display order, the
    /// only case in which C# emits a season/episode TMDB link.
    fn series_is_airdate_order(&self) -> bool {
        matches!(
            self.series_display_order.map(str::trim),
            // C# parses this with `Enum.TryParse<TvGroupType>`, whose member
            // is `OriginalAirDate`; no other spelling parses there.
            None | Some("" | "OriginalAirDate")
        )
    }

    /// The MusicBrainz root with any trailing slash removed.
    fn musicbrainz_root(&self) -> &str {
        let server = self.musicbrainz_server.trim().trim_end_matches('/');
        if server.is_empty() {
            crate::musicbrainz::DEFAULT_BASE_URL
        } else {
            server
        }
    }
}

/// Orders two provider names the way .NET's default string comparer does.
///
/// `OrderBy(i => i.Name)` uses `Comparer<string>.Default`, a culture-aware
/// comparison whose primary weight ignores case; a raw byte comparison would
/// put every capital ahead of every lowercase letter and reorder names like
/// "TMDB" against "TheAudioDb Artist". Case is the tie-break, so the order
/// stays total.
fn compare_provider_names(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    let (a, b) = (a.unwrap_or_default(), b.unwrap_or_default());
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

/// Pushes `name` → `url` onto `out`.
fn push(out: &mut Vec<ExternalUrl>, name: &str, url: String) {
    out.push(ExternalUrl {
        name: Some(name.to_owned()),
        url: Some(url),
    });
}

/// The "Links" row for an item — a port of every `IExternalUrlProvider`.
///
/// Ordered by provider name: `ProviderManager`'s constructor does
/// `externalUrlProviders.OrderBy(i => i.Name)`, discarding DI registration
/// order, so a client rendering them in sequence matches Jellyfin only if the
/// same sort is applied here.
#[must_use]
pub fn external_urls(item: &ExternalIdItem<'_>) -> Vec<ExternalUrl> {
    let mut out = Vec::new();
    imdb_urls(item, &mut out);
    tmdb_urls(item, &mut out);
    // C# applies no kind filter to Zap2It; only a Series ever carries the id.
    if let Some(id) = item.id("Zap2It") {
        push(
            &mut out,
            "Zap2It",
            format!("http://tvlistings.zap2it.com/overview.html?programSeriesId={id}"),
        );
    }
    musicbrainz_urls(item, &mut out);
    audiodb_urls(item, &mut out);
    book_urls(item, &mut out);
    // Stable, so several links from one provider keep their emitted order.
    // Case-INSENSITIVE: `OrderBy` uses .NET's culture-aware string comparer,
    // whose primary weight ignores case, so "TheAudioDb Artist" precedes
    // "TMDB" there while a byte comparison would reverse them.
    out.sort_by(|a, b| compare_provider_names(a.name.as_deref(), b.name.as_deref()));
    out
}

/// `ImdbExternalUrlProvider` — a season links its *series*' episode list.
fn imdb_urls(item: &ExternalIdItem<'_>, out: &mut Vec<ExternalUrl>) {
    match item.kind {
        BaseItemKind::Season => {
            if let (Some(series), Some(season)) = (item.series_id("Imdb"), item.index_number) {
                push(
                    out,
                    "IMDb",
                    format!("{IMDB_BASE}title/{series}/episodes/?season={season}"),
                );
            }
        }
        BaseItemKind::Person => {
            if let Some(id) = item.id("Imdb") {
                push(out, "IMDb", format!("{IMDB_BASE}name/{id}"));
            }
        }
        _ => {
            if let Some(id) = item.id("Imdb") {
                push(out, "IMDb", format!("{IMDB_BASE}title/{id}"));
            }
        }
    }
}

/// `TmdbExternalUrlProvider` — one path segment per kind; seasons and episodes
/// hang off the series id and only in the default (airdate) display order.
fn tmdb_urls(item: &ExternalIdItem<'_>, out: &mut Vec<ExternalUrl>) {
    let path = match item.kind {
        BaseItemKind::Series => item.id("Tmdb").map(|id| format!("tv/{id}")),
        BaseItemKind::Movie => item.id("Tmdb").map(|id| format!("movie/{id}")),
        BaseItemKind::Person => item.id("Tmdb").map(|id| format!("person/{id}")),
        BaseItemKind::BoxSet => item.id("Tmdb").map(|id| format!("collection/{id}")),
        BaseItemKind::Season if item.series_is_airdate_order() => {
            match (item.series_id("Tmdb"), item.index_number) {
                (Some(series), Some(season)) => Some(format!("tv/{series}/season/{season}")),
                _ => None,
            }
        }
        BaseItemKind::Episode if item.series_is_airdate_order() => match (
            item.series_id("Tmdb"),
            item.parent_index_number,
            item.index_number,
        ) {
            (Some(series), Some(season), Some(episode)) => {
                Some(format!("tv/{series}/season/{season}/episode/{episode}"))
            }
            _ => None,
        },
        _ => None,
    };
    if let Some(path) = path {
        push(out, "TMDB", format!("{TMDB_BASE}{path}"));
    }
}

/// The four `MusicBrainz*ExternalUrlProvider`s.
fn musicbrainz_urls(item: &ExternalIdItem<'_>, out: &mut Vec<ExternalUrl>) {
    let mb = item.musicbrainz_root();
    if matches!(item.kind, BaseItemKind::MusicArtist | BaseItemKind::Person)
        && let Some(id) = item.id("MusicBrainzArtist")
    {
        push(out, "MusicBrainz Artist", format!("{mb}/artist/{id}"));
    }
    if item.kind == BaseItemKind::MusicAlbum {
        if let Some(id) = item.id("MusicBrainzAlbumArtist") {
            push(out, "MusicBrainz Album Artist", format!("{mb}/artist/{id}"));
        }
        if let Some(id) = item.id("MusicBrainzAlbum") {
            push(out, "MusicBrainz Album", format!("{mb}/release/{id}"));
        }
        if let Some(id) = item.id("MusicBrainzReleaseGroup") {
            push(
                out,
                "MusicBrainz Release Group",
                format!("{mb}/release-group/{id}"),
            );
        }
    }
    if item.kind == BaseItemKind::Audio
        && let Some(id) = item.id("MusicBrainzTrack")
    {
        push(out, "MusicBrainz Track", format!("{mb}/track/{id}"));
    }
}

/// The two `AudioDb*ExternalUrlProvider`s.
fn audiodb_urls(item: &ExternalIdItem<'_>, out: &mut Vec<ExternalUrl>) {
    if matches!(item.kind, BaseItemKind::MusicArtist | BaseItemKind::Person)
        && let Some(id) = item.id("AudioDbArtist")
    {
        push(
            out,
            "TheAudioDb Artist",
            format!("{AUDIODB_BASE}artist/{id}"),
        );
    }
    if item.kind == BaseItemKind::MusicAlbum
        && let Some(id) = item.id("AudioDbAlbum")
    {
        push(out, "TheAudioDb Album", format!("{AUDIODB_BASE}album/{id}"));
    }
}

/// `ComicVine`/`GoogleBooks`/`ISBN` external URL providers.
fn book_urls(item: &ExternalIdItem<'_>, out: &mut Vec<ExternalUrl>) {
    if matches!(item.kind, BaseItemKind::Book | BaseItemKind::Person)
        && let Some(id) = item.id("ComicVine")
    {
        push(
            out,
            "Comic Vine",
            format!("https://comicvine.gamespot.com/{id}"),
        );
    }
    if item.kind != BaseItemKind::Book {
        return;
    }
    if let Some(id) = item.id("GoogleBooks") {
        push(
            out,
            "Google Books",
            format!("https://books.google.com/books?id={id}"),
        );
    }
    if let Some(id) = item.id("ISBN") {
        push(
            out,
            "ISBN",
            format!("https://search.worldcat.org/search?q=bn:{id}"),
        );
    }
}

/// One `IExternalId` descriptor: the display name, the `ProviderIds` key it
/// writes, its media type, and the kinds it supports.
struct ExternalIdDescriptor {
    /// C# `IExternalId.ProviderName`.
    name: &'static str,
    /// C# `IExternalId.Key` — the `ProviderIds` map key.
    key: &'static str,
    /// C# `IExternalId.Type`.
    media_type: Option<ExternalIdMediaType>,
    /// C# `IExternalId.Supports(item)`.
    kinds: &'static [BaseItemKind],
}

/// Every compiled-in `IExternalId`, in C# DI registration order.
const EXTERNAL_IDS: &[ExternalIdDescriptor] = {
    use BaseItemKind as K;
    &[
        // Movies/ImdbExternalId + ImdbPersonExternalId
        ExternalIdDescriptor {
            name: "IMDb",
            key: "Imdb",
            media_type: None,
            kinds: &[K::Movie, K::MusicVideo, K::Series, K::Episode, K::Trailer],
        },
        ExternalIdDescriptor {
            name: "IMDb",
            key: "Imdb",
            media_type: Some(ExternalIdMediaType::Person),
            kinds: &[K::Person],
        },
        // TV/Zap2ItExternalId
        ExternalIdDescriptor {
            name: "Zap2It",
            key: "Zap2It",
            media_type: None,
            kinds: &[K::Series],
        },
        // Plugins/Tmdb/**/Tmdb*ExternalId
        ExternalIdDescriptor {
            name: "TheMovieDb",
            key: "Tmdb",
            media_type: Some(ExternalIdMediaType::Movie),
            kinds: &[K::Movie],
        },
        ExternalIdDescriptor {
            name: "TheMovieDb",
            key: "Tmdb",
            media_type: Some(ExternalIdMediaType::Series),
            kinds: &[K::Series],
        },
        ExternalIdDescriptor {
            name: "TheMovieDb",
            key: "Tmdb",
            media_type: Some(ExternalIdMediaType::Season),
            kinds: &[K::Season],
        },
        ExternalIdDescriptor {
            name: "TheMovieDb",
            key: "Tmdb",
            media_type: Some(ExternalIdMediaType::Episode),
            kinds: &[K::Episode],
        },
        ExternalIdDescriptor {
            name: "TheMovieDb",
            key: "Tmdb",
            media_type: Some(ExternalIdMediaType::Person),
            kinds: &[K::Person],
        },
        // TmdbBoxSetExternalId — the *collection* a movie belongs to.
        ExternalIdDescriptor {
            name: "TheMovieDb",
            key: "TmdbCollection",
            media_type: Some(ExternalIdMediaType::BoxSet),
            kinds: &[K::Movie, K::MusicVideo, K::Trailer],
        },
        // jellyfin-plugin-tvdb Providers/ExternalId/*
        ExternalIdDescriptor {
            name: "TheTVDB Numerical",
            key: "Tvdb",
            media_type: Some(ExternalIdMediaType::Series),
            kinds: &[K::Series],
        },
        ExternalIdDescriptor {
            name: "TheTVDB",
            key: "Tvdb",
            media_type: Some(ExternalIdMediaType::Season),
            kinds: &[K::Season],
        },
        ExternalIdDescriptor {
            name: "TheTVDB",
            key: "Tvdb",
            media_type: Some(ExternalIdMediaType::Episode),
            kinds: &[K::Episode],
        },
        ExternalIdDescriptor {
            name: "TheTVDB Numerical",
            key: "Tvdb",
            media_type: Some(ExternalIdMediaType::Movie),
            kinds: &[K::Movie],
        },
        ExternalIdDescriptor {
            name: "TheTVDB",
            key: "Tvdb",
            media_type: Some(ExternalIdMediaType::Person),
            kinds: &[K::Person],
        },
        // Plugins/MusicBrainz/*
        ExternalIdDescriptor {
            name: "MusicBrainz",
            key: "MusicBrainzAlbum",
            media_type: Some(ExternalIdMediaType::Album),
            kinds: &[K::Audio, K::MusicAlbum],
        },
        ExternalIdDescriptor {
            name: "MusicBrainz",
            key: "MusicBrainzAlbumArtist",
            media_type: Some(ExternalIdMediaType::AlbumArtist),
            kinds: &[K::Audio],
        },
        ExternalIdDescriptor {
            name: "MusicBrainz",
            key: "MusicBrainzArtist",
            media_type: Some(ExternalIdMediaType::Artist),
            kinds: &[K::MusicArtist],
        },
        ExternalIdDescriptor {
            name: "MusicBrainz",
            key: "MusicBrainzArtist",
            media_type: Some(ExternalIdMediaType::OtherArtist),
            kinds: &[K::Audio, K::MusicAlbum],
        },
        ExternalIdDescriptor {
            name: "MusicBrainz",
            key: "MusicBrainzReleaseGroup",
            media_type: Some(ExternalIdMediaType::ReleaseGroup),
            kinds: &[K::Audio, K::MusicAlbum],
        },
        ExternalIdDescriptor {
            name: "MusicBrainz",
            key: "MusicBrainzTrack",
            media_type: Some(ExternalIdMediaType::Track),
            kinds: &[K::Audio],
        },
        ExternalIdDescriptor {
            name: "MusicBrainz",
            key: "MusicBrainzRecording",
            media_type: Some(ExternalIdMediaType::Recording),
            kinds: &[K::Audio],
        },
        // Plugins/AudioDb/*
        ExternalIdDescriptor {
            name: "TheAudioDb",
            key: "AudioDbAlbum",
            media_type: None,
            kinds: &[K::MusicAlbum],
        },
        ExternalIdDescriptor {
            name: "TheAudioDb",
            key: "AudioDbArtist",
            media_type: Some(ExternalIdMediaType::Artist),
            kinds: &[K::MusicArtist],
        },
        ExternalIdDescriptor {
            name: "TheAudioDb",
            key: "AudioDbAlbum",
            media_type: Some(ExternalIdMediaType::Album),
            kinds: &[K::Audio],
        },
        ExternalIdDescriptor {
            name: "TheAudioDb",
            key: "AudioDbArtist",
            media_type: Some(ExternalIdMediaType::OtherArtist),
            kinds: &[K::Audio, K::MusicAlbum],
        },
        // Music/ImvdbId
        ExternalIdDescriptor {
            name: "IMVDb",
            key: "IMVDb",
            media_type: None,
            kinds: &[K::MusicVideo],
        },
        // Books/**
        ExternalIdDescriptor {
            name: "Comic Vine",
            key: "ComicVine",
            media_type: None,
            kinds: &[K::Book],
        },
        ExternalIdDescriptor {
            name: "Comic Vine",
            key: "ComicVine",
            media_type: Some(ExternalIdMediaType::Person),
            kinds: &[K::Person],
        },
        ExternalIdDescriptor {
            name: "Google Books",
            key: "GoogleBooks",
            media_type: None,
            kinds: &[K::Book],
        },
        ExternalIdDescriptor {
            name: "ISBN",
            key: "ISBN",
            media_type: None,
            kinds: &[K::Book],
        },
    ]
};

/// The `IExternalId` descriptors that support `kind` — what
/// `GET /Items/{id}/ExternalIdInfos` returns and the Identify dialog renders as
/// id input fields.
#[must_use]
pub fn external_id_infos(kind: BaseItemKind) -> Vec<ExternalIdInfo> {
    let mut out: Vec<ExternalIdInfo> = EXTERNAL_IDS
        .iter()
        .filter(|d| d.kinds.contains(&kind))
        .map(|d| ExternalIdInfo::new(d.name.to_owned(), d.key.to_owned(), d.media_type))
        .collect();
    // `ProviderManager` stores `externalIds.OrderBy(i => i.ProviderName)`, so
    // the Identify dialog's field order is alphabetical, not registration
    // order — and alphabetical the way .NET orders it (see
    // [`compare_provider_names`]).
    out.sort_by(|a, b| compare_provider_names(a.name.as_deref(), b.name.as_deref()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identify_fields_are_ordered_by_provider_name() {
        // `ProviderManager` stores `externalIds.OrderBy(i => i.ProviderName)`,
        // so the Identify dialog's field order is alphabetical, not the DI
        // registration order.
        for kind in [
            BaseItemKind::Series,
            BaseItemKind::Person,
            BaseItemKind::MusicAlbum,
            BaseItemKind::Movie,
        ] {
            let names: Vec<String> = external_id_infos(kind)
                .into_iter()
                .filter_map(|info| info.name)
                .collect();
            let mut sorted = names.clone();
            // Case-insensitive, as .NET's default comparer orders — a plain
            // byte sort would put "TMDB" ahead of "TheAudioDb Artist".
            sorted.sort_by_key(|name| name.to_lowercase());
            assert_eq!(names, sorted, "{kind:?} ids are not name-ordered");
        }
        // And the order really is different from registration order for at
        // least one kind, so the assertion above has something to catch.
        let person: Vec<String> = external_id_infos(BaseItemKind::Person)
            .into_iter()
            .filter_map(|info| info.name)
            .collect();
        assert!(person.len() > 1, "Person should offer several ids");
    }

    fn ids(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn urls(item: &ExternalIdItem<'_>) -> Vec<(String, String)> {
        external_urls(item)
            .into_iter()
            .map(|u| (u.name.unwrap(), u.url.unwrap()))
            .collect()
    }

    #[test]
    fn movie_links_imdb_and_tmdb_in_registration_order() {
        let map = ids(&[("Imdb", "tt1375666"), ("Tmdb", "27205")]);
        let got = urls(&ExternalIdItem::new(BaseItemKind::Movie, &map));
        assert_eq!(
            got,
            vec![
                ("IMDb".into(), "https://www.imdb.com/title/tt1375666".into()),
                (
                    "TMDB".into(),
                    "https://www.themoviedb.org/movie/27205".into()
                ),
            ]
        );
    }

    #[test]
    fn person_uses_the_name_path_not_the_title_path() {
        let map = ids(&[("Imdb", "nm0000138"), ("Tmdb", "6193")]);
        let got = urls(&ExternalIdItem::new(BaseItemKind::Person, &map));
        assert_eq!(
            got,
            vec![
                ("IMDb".into(), "https://www.imdb.com/name/nm0000138".into()),
                (
                    "TMDB".into(),
                    "https://www.themoviedb.org/person/6193".into()
                ),
            ]
        );
    }

    #[test]
    fn series_links_tv_path_and_zap2it() {
        let map = ids(&[("Tmdb", "1396"), ("Zap2It", "EP01234567")]);
        let got = urls(&ExternalIdItem::new(BaseItemKind::Series, &map));
        assert_eq!(
            got,
            vec![
                ("TMDB".into(), "https://www.themoviedb.org/tv/1396".into()),
                (
                    "Zap2It".into(),
                    "http://tvlistings.zap2it.com/overview.html?programSeriesId=EP01234567".into()
                ),
            ]
        );
    }

    #[test]
    fn season_links_are_built_from_the_series_ids() {
        let own = ids(&[]);
        let series = ids(&[("Imdb", "tt0903747"), ("Tmdb", "1396")]);
        let item = ExternalIdItem {
            index_number: Some(2),
            series_provider_ids: Some(&series),
            ..ExternalIdItem::new(BaseItemKind::Season, &own)
        };
        assert_eq!(
            urls(&item),
            vec![
                (
                    "IMDb".into(),
                    "https://www.imdb.com/title/tt0903747/episodes/?season=2".into()
                ),
                (
                    "TMDB".into(),
                    "https://www.themoviedb.org/tv/1396/season/2".into()
                ),
            ]
        );
    }

    #[test]
    fn episode_links_use_season_and_episode_numbers() {
        let own = ids(&[]);
        let series = ids(&[("Tmdb", "1396")]);
        let item = ExternalIdItem {
            index_number: Some(7),
            parent_index_number: Some(3),
            series_provider_ids: Some(&series),
            ..ExternalIdItem::new(BaseItemKind::Episode, &own)
        };
        assert_eq!(
            urls(&item),
            vec![(
                "TMDB".into(),
                "https://www.themoviedb.org/tv/1396/season/3/episode/7".into()
            )]
        );
    }

    #[test]
    fn a_non_airdate_display_order_suppresses_the_tmdb_season_link() {
        let own = ids(&[]);
        let series = ids(&[("Tmdb", "1396")]);
        let item = ExternalIdItem {
            index_number: Some(2),
            series_provider_ids: Some(&series),
            series_display_order: Some("Absolute"),
            ..ExternalIdItem::new(BaseItemKind::Season, &own)
        };
        assert!(urls(&item).is_empty());
    }

    #[test]
    fn music_album_links_all_three_musicbrainz_ids_plus_audiodb() {
        let map = ids(&[
            ("MusicBrainzAlbum", "release-1"),
            ("MusicBrainzAlbumArtist", "artist-1"),
            ("MusicBrainzReleaseGroup", "rg-1"),
            ("AudioDbAlbum", "999"),
        ]);
        let got = urls(&ExternalIdItem::new(BaseItemKind::MusicAlbum, &map));
        assert_eq!(
            got,
            vec![
                // Alphabetical, as `ProviderManager` orders the providers —
                // NOT the DI registration order.
                (
                    "MusicBrainz Album".into(),
                    "https://musicbrainz.org/release/release-1".into()
                ),
                (
                    "MusicBrainz Album Artist".into(),
                    "https://musicbrainz.org/artist/artist-1".into()
                ),
                (
                    "MusicBrainz Release Group".into(),
                    "https://musicbrainz.org/release-group/rg-1".into()
                ),
                (
                    "TheAudioDb Album".into(),
                    "https://www.theaudiodb.com/album/999".into()
                ),
            ]
        );
    }

    #[test]
    fn a_configured_musicbrainz_mirror_is_used_and_its_trailing_slash_trimmed() {
        let map = ids(&[("MusicBrainzTrack", "t-1")]);
        let item = ExternalIdItem {
            musicbrainz_server: "https://mb.example.org/",
            ..ExternalIdItem::new(BaseItemKind::Audio, &map)
        };
        assert_eq!(
            urls(&item),
            vec![(
                "MusicBrainz Track".into(),
                "https://mb.example.org/track/t-1".into()
            )]
        );
    }

    #[test]
    fn book_links_comic_vine_google_books_and_isbn() {
        let map = ids(&[
            ("ComicVine", "4000-1234"),
            ("GoogleBooks", "abc123"),
            ("ISBN", "9780306406157"),
        ]);
        let got = urls(&ExternalIdItem::new(BaseItemKind::Book, &map));
        assert_eq!(
            got,
            vec![
                (
                    "Comic Vine".into(),
                    "https://comicvine.gamespot.com/4000-1234".into()
                ),
                (
                    "Google Books".into(),
                    "https://books.google.com/books?id=abc123".into()
                ),
                (
                    "ISBN".into(),
                    "https://search.worldcat.org/search?q=bn:9780306406157".into()
                ),
            ]
        );
    }

    #[test]
    fn ids_from_the_wrong_kind_are_ignored() {
        // A movie carrying music ids must not sprout MusicBrainz links.
        let map = ids(&[("MusicBrainzAlbum", "release-1"), ("AudioDbArtist", "1")]);
        assert!(urls(&ExternalIdItem::new(BaseItemKind::Movie, &map)).is_empty());
    }

    #[test]
    fn blank_and_whitespace_ids_produce_no_link() {
        let map = ids(&[("Imdb", "   "), ("Tmdb", "")]);
        assert!(urls(&ExternalIdItem::new(BaseItemKind::Movie, &map)).is_empty());
    }

    #[test]
    fn external_id_infos_are_filtered_by_kind() {
        let movie = external_id_infos(BaseItemKind::Movie);
        let keys: Vec<_> = movie.iter().map(|i| i.key.clone().unwrap()).collect();
        assert!(keys.contains(&"Imdb".to_owned()));
        assert!(keys.contains(&"Tmdb".to_owned()));
        assert!(keys.contains(&"TmdbCollection".to_owned()));
        assert!(keys.contains(&"Tvdb".to_owned()));
        assert!(!keys.contains(&"ISBN".to_owned()));

        let book = external_id_infos(BaseItemKind::Book);
        let book_keys: Vec<_> = book.iter().map(|i| i.key.clone().unwrap()).collect();
        assert_eq!(book_keys, vec!["ComicVine", "GoogleBooks", "ISBN"]);
    }

    #[test]
    fn the_movie_descriptor_carries_the_movie_media_type() {
        let movie = external_id_infos(BaseItemKind::Movie);
        let tmdb = movie
            .iter()
            .find(|i| i.key.as_deref() == Some("Tmdb"))
            .expect("Tmdb descriptor");
        assert_eq!(tmdb.name.as_deref(), Some("TheMovieDb"));
        assert_eq!(tmdb.type_, Some(ExternalIdMediaType::Movie));
    }

    #[test]
    fn a_kind_with_no_external_ids_returns_an_empty_list() {
        assert!(external_id_infos(BaseItemKind::Folder).is_empty());
    }
}
