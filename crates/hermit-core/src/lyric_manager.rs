//! [`HermitLyricManager`] — local lyric reading (`.lrc`/`.elrc`/`.txt`) plus
//! remote lyric search/download through registered [`LyricProvider`]s.
//!
//! Port of `MediaBrowser.Providers.Lyric.LyricManager` +
//! `LrcLyricParser`/`TxtLyricParser`: an item's lyrics come from a sidecar file
//! next to its media (`song.flac` → `song.lrc`). Synced `.lrc`/`.elrc` files
//! yield timestamped [`LyricLine`]s (with metadata tags); plain `.txt` yields
//! unsynced lines. Uploads write a sidecar; deletes remove it.
//!
//! Remote lyrics mirror the C# manager over its `ILyricProvider[]` registry:
//! [`search_lyrics`](LyricManager::search_lyrics) builds a
//! [`LyricSearchRequest`] from the resolved item and queries the providers one
//! at a time until one has results (the `SearchAllProviders == false` default);
//! [`download_lyrics`](LyricManager::download_lyrics) routes the namespaced id
//! (`"{provider_id}_{provider_local_id}"`, provider id = MD5 of the lowercased
//! provider name — `LyricManager.GetProviderId`) back to its provider, saves
//! the fetched lyric as a sidecar in the media folder (synced → `.lrc`, plain
//! → `.txt` — `TrySaveLyric` with `SaveLyricsWithMedia`), and returns the
//! parsed [`LyricDto`]. With no providers registered the manager behaves like
//! a server without a lyric plugin: search is empty, download rejects.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_model::data::BaseItemKind;
use hermit_model::lyrics::{
    LyricDto, LyricLine, LyricMetadata, LyricSearchRequest, RemoteLyricInfoDto,
};
use hermit_model::providers::LyricProviderInfo;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::persistence::ItemRepository;
use hermit_traits::stubs::{LyricManager, LyricProvider};

use crate::item_type_lookup::kind_from_type_name;

/// The number of ffmpeg ticks (100 ns) per millisecond.
const TICKS_PER_MS: i64 = 10_000;

/// Sidecar extensions probed for an item's lyrics, in priority order: synced LRC
/// first, then plain text. Port of `LrcLyricParser`/`TxtLyricParser`
/// `SupportedMediaTypes`.
const LYRIC_EXTENSIONS: [&str; 3] = ["lrc", "elrc", "txt"];

/// The lyric manager: reads/writes `.lrc`/`.elrc`/`.txt` sidecars next to an
/// item's media file, and searches/downloads remote lyrics through the
/// registered [`LyricProvider`]s (LrcLib, …).
#[derive(Clone, Default)]
pub struct HermitLyricManager {
    /// Resolves an item id to its media path (to locate the sidecar) and its
    /// name/artists/album/runtime (to build a provider search request). Absent
    /// → the manager behaves as an empty stub (unit tests).
    items: Option<Arc<dyn ItemRepository>>,
    /// The registered remote lyric providers. Empty → search returns nothing
    /// and download rejects (a server with no lyric plugin).
    providers: Vec<Arc<dyn LyricProvider>>,
}

impl std::fmt::Debug for HermitLyricManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitLyricManager")
            .field("has_item_store", &self.items.is_some())
            .field("providers", &self.providers.len())
            .finish()
    }
}

impl HermitLyricManager {
    /// Creates a lyric manager with no item store and no remote providers
    /// (empty stub).
    #[must_use]
    pub fn new() -> Self {
        Self {
            items: None,
            providers: Vec::new(),
        }
    }

    /// Attaches the item store used to resolve an item's media path, enabling
    /// local sidecar reading/writing.
    #[must_use]
    pub fn with_items(mut self, items: Arc<dyn ItemRepository>) -> Self {
        self.items = Some(items);
        self
    }

    /// Registers the remote lyric providers searched/downloaded through.
    #[must_use]
    pub fn with_providers(mut self, providers: Vec<Arc<dyn LyricProvider>>) -> Self {
        self.providers = providers;
        self
    }

    /// The item row for `item_id`, or `None` when unknown / no store attached.
    async fn item(&self, item_id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        let Some(items) = &self.items else {
            return Ok(None);
        };
        items.retrieve_item(item_id).await
    }

    /// The media file path for `item_id`, or `None` when unknown / pathless.
    async fn item_path(&self, item_id: Uuid) -> Result<Option<PathBuf>, ServiceError> {
        Ok(self
            .item(item_id)
            .await?
            .and_then(|i| i.path)
            .filter(|p| !p.is_empty())
            .map(PathBuf::from))
    }

    /// The rejection returned when a remote lyric download is requested but no
    /// remote provider is configured.
    fn no_remote() -> ServiceError {
        ServiceError::invalid_input("no remote lyric provider is configured")
    }

    /// Runs one provider's search and maps its results into namespaced
    /// [`RemoteLyricInfoDto`]s. A provider that errors yields an empty list
    /// (logged) rather than failing the whole search — port of
    /// `LyricManager.InternalSearchProviderAsync`.
    async fn search_provider(
        &self,
        provider: &Arc<dyn LyricProvider>,
        request: &LyricSearchRequest,
    ) -> Vec<RemoteLyricInfoDto> {
        let results = match provider.search(request).await {
            Ok(results) => results,
            Err(e) => {
                tracing::error!(provider = provider.name(), error = %e, "lyric search failed");
                return Vec::new();
            }
        };
        let namespace = provider_id(provider.name());
        results
            .into_iter()
            .map(|result| {
                // Parse the raw text and stamp the provider's metadata over the
                // parsed tags (upstream `parsedLyrics.Metadata = result.Metadata`).
                let mut lyrics = parse_by_format(&result.lyrics.format, &result.lyrics.text);
                lyrics.metadata = result.metadata;
                RemoteLyricInfoDto {
                    id: format!("{namespace}_{}", result.id),
                    provider_name: result.provider_name,
                    lyrics,
                }
            })
            .collect()
    }
}

/// The stable id of a provider: the MD5 of its lowercased name, hex-encoded.
/// Port of `LyricManager.GetProviderId` (`name.ToLowerInvariant().GetMD5()`,
/// `"N"` format) — [`get_md5`](hermit_common::extensions::get_md5) reproduces
/// the .NET UTF-16LE `GetMD5` extension byte-for-byte.
fn provider_id(name: &str) -> String {
    hermit_common::extensions::get_md5(&name.to_lowercase())
        .simple()
        .to_string()
}

/// Parses raw lyric text by its format: `txt` → unsynced lines, anything else
/// (`lrc`/`elrc`) → the LRC parser.
fn parse_by_format(format: &str, text: &str) -> LyricDto {
    if format.eq_ignore_ascii_case("txt") {
        parse_txt(text)
    } else {
        parse_lrc(text)
    }
}

/// Builds the provider search request from the resolved item — the fields
/// `LyricManager.SearchLyricsAsync(Audio…)` copies out of the `Audio` domain
/// object (path, name, album, album artists, artists, runtime).
fn search_request_for(item: &BaseItemEntity) -> LyricSearchRequest {
    /// Splits a stored pipe-delimited multi-value column (Jellyfin joins
    /// `Artists`/`AlbumArtists` with `|`), `None` when empty.
    fn split_multi(stored: Option<&str>) -> Option<Vec<String>> {
        let values: Vec<String> = stored?
            .split('|')
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect();
        (!values.is_empty()).then_some(values)
    }

    LyricSearchRequest {
        media_path: item.path.clone().filter(|p| !p.is_empty()),
        song_name: item.name.clone(),
        album_name: item.album.clone(),
        album_artists_names: split_multi(item.album_artists.as_deref()),
        artist_names: split_multi(item.artists.as_deref()),
        duration: item.run_time_ticks,
        ..LyricSearchRequest::default()
    }
}

/// The sidecar lyric file for `media_path` (`song.flac` → `song.lrc`), scanning
/// [`LYRIC_EXTENSIONS`] in order; `None` when none exists.
fn sidecar_for(media_path: &Path) -> Option<PathBuf> {
    LYRIC_EXTENSIONS
        .iter()
        .map(|ext| media_path.with_extension(ext))
        .find(|p| p.is_file())
}

#[async_trait]
impl LyricManager for HermitLyricManager {
    async fn get_lyrics(&self, item_id: Uuid) -> Result<Option<LyricDto>, ServiceError> {
        let Some(path) = self.item_path(item_id).await? else {
            return Ok(None);
        };
        let Some(sidecar) = sidecar_for(&path) else {
            return Ok(None);
        };
        let Ok(content) = std::fs::read_to_string(&sidecar) else {
            return Ok(None);
        };
        let synced = sidecar
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| !e.eq_ignore_ascii_case("txt"));
        Ok(Some(if synced {
            parse_lrc(&content)
        } else {
            parse_txt(&content)
        }))
    }

    async fn search_lyrics(&self, item_id: Uuid) -> Result<Vec<RemoteLyricInfoDto>, ServiceError> {
        let Some(item) = self.item(item_id).await? else {
            return Ok(Vec::new());
        };
        let request = search_request_for(&item);
        // Query providers one at a time until one has results — the C#
        // `SearchAllProviders == false` default path.
        for provider in &self.providers {
            let results = self.search_provider(provider, &request).await;
            if !results.is_empty() {
                return Ok(results);
            }
        }
        Ok(Vec::new())
    }

    async fn download_lyrics(
        &self,
        item_id: Uuid,
        lyric_id: &str,
    ) -> Result<Option<LyricDto>, ServiceError> {
        if self.providers.is_empty() {
            return Err(Self::no_remote());
        }
        let Some(path) = self.item_path(item_id).await? else {
            return Err(ServiceError::not_found("item has no media path for lyrics"));
        };

        // The id is `"{provider_id}_{provider_local_id}"` (`GetProviderId`
        // namespacing); route it back to the owning provider.
        let (namespace, local_id) = lyric_id.split_once('_').unwrap_or((lyric_id, lyric_id));
        let Some(provider) = self
            .providers
            .iter()
            .find(|p| provider_id(p.name()) == namespace)
        else {
            tracing::warn!(lyric_id, "unknown lyric provider id");
            return Ok(None);
        };

        let Some(response) = provider.get_lyrics(local_id).await? else {
            tracing::debug!(lyric_id, "unable to download lyrics");
            return Ok(None);
        };

        // Save the sidecar into the media folder (upstream `TrySaveLyric` with
        // `SaveLyricsWithMedia`): media base name + the lyric format extension
        // (synced → `.lrc`, plain → `.txt`).
        let dest = path.with_extension(response.format.to_lowercase());
        std::fs::write(&dest, &response.text)
            .map_err(|e| ServiceError::backend(format!("write lyrics: {e}")))?;

        // Return the saved lyric through the local parse path.
        Ok(Some(parse_by_format(&response.format, &response.text)))
    }

    async fn save_lyric(
        &self,
        item_id: Uuid,
        format: &str,
        lyrics: &str,
    ) -> Result<Option<LyricDto>, ServiceError> {
        let Some(path) = self.item_path(item_id).await? else {
            return Err(ServiceError::not_found("item has no media path for lyrics"));
        };
        // Write the sidecar with the upload's format extension (default `lrc`).
        let ext = {
            let e = format.trim().trim_start_matches('.').to_ascii_lowercase();
            if LYRIC_EXTENSIONS.contains(&e.as_str()) {
                e
            } else {
                "lrc".to_owned()
            }
        };
        let dest = path.with_extension(&ext);
        std::fs::write(&dest, lyrics)
            .map_err(|e| ServiceError::backend(format!("write lyrics: {e}")))?;
        Ok(Some(if ext == "txt" {
            parse_txt(lyrics)
        } else {
            parse_lrc(lyrics)
        }))
    }

    async fn delete_lyrics(&self, item_id: Uuid) -> Result<(), ServiceError> {
        if let Some(path) = self.item_path(item_id).await? {
            for ext in LYRIC_EXTENSIONS {
                let _ = std::fs::remove_file(path.with_extension(ext));
            }
        }
        Ok(())
    }

    async fn get_supported_providers(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<LyricProviderInfo>, ServiceError> {
        // Only audio items have lyric providers (upstream `item is not Audio`).
        let is_audio = self
            .item(item_id)
            .await?
            .is_some_and(|i| kind_from_type_name(&i.type_) == Some(BaseItemKind::Audio));
        if !is_audio {
            return Ok(Vec::new());
        }
        Ok(self
            .providers
            .iter()
            .map(|p| LyricProviderInfo {
                name: p.name().to_owned(),
                id: provider_id(p.name()),
            })
            .collect())
    }
}

/// Parses an LRC/ELRC document into a [`LyricDto`] (metadata tags + timestamped
/// lines). Port of `LrcLyricParser.ParseLyrics`.
fn parse_lrc(content: &str) -> LyricDto {
    let mut metadata = LyricMetadata::default();
    let mut lines: Vec<LyricLine> = Vec::new();

    for raw in content.lines() {
        let mut rest = raw.trim();
        let mut starts: Vec<i64> = Vec::new();
        // Consume leading `[...]` tags: each is either a timestamp or metadata.
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(close) = stripped.find(']') else {
                break;
            };
            let inner = &stripped[..close];
            rest = stripped[close + 1..].trim_start();
            if let Some(ticks) = parse_lrc_timestamp(inner) {
                starts.push(ticks);
            } else if let Some((tag, value)) = inner.split_once(':') {
                apply_metadata_tag(&mut metadata, tag.trim(), value.trim());
            }
        }
        let text = rest.trim().to_owned();
        for start in &starts {
            lines.push(LyricLine {
                text: text.clone(),
                start: Some(*start),
                cues: None,
            });
        }
    }

    lines.sort_by_key(|l| l.start.unwrap_or(0));
    metadata.is_synced = Some(!lines.is_empty());
    LyricDto {
        metadata,
        lyrics: lines,
    }
}

/// Parses a plain-text lyric file: each non-empty line is an unsynced
/// [`LyricLine`]. Port of `TxtLyricParser.ParseLyrics`.
fn parse_txt(content: &str) -> LyricDto {
    let lyrics = content
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .map(|l| LyricLine {
            text: l.to_owned(),
            start: None,
            cues: None,
        })
        .collect();
    LyricDto {
        metadata: LyricMetadata {
            is_synced: Some(false),
            ..LyricMetadata::default()
        },
        lyrics,
    }
}

/// Parses an LRC timestamp `mm:ss`, `mm:ss.cc`, or `mm:ss.mmm` into ticks, or
/// `None` when `inner` is not a timestamp (e.g. a metadata tag).
fn parse_lrc_timestamp(inner: &str) -> Option<i64> {
    let (min_str, rest) = inner.split_once(':')?;
    let minutes: i64 = min_str.trim().parse().ok()?;
    let (sec_str, frac_str) = match rest.split_once('.') {
        Some((s, f)) => (s, Some(f)),
        None => (rest, None),
    };
    let seconds: i64 = sec_str.trim().parse().ok()?;
    let mut ms = (minutes * 60 + seconds) * 1000;
    if let Some(frac) = frac_str {
        let frac = frac.trim();
        let value: i64 = frac.parse().ok()?;
        ms += match frac.len() {
            1 => value * 100, // tenths
            2 => value * 10,  // centiseconds
            _ => value,       // milliseconds (3+ digits)
        };
    }
    Some(ms * TICKS_PER_MS)
}

/// Applies a recognised LRC metadata tag (`ar`/`al`/`ti`/…) to `metadata`.
fn apply_metadata_tag(metadata: &mut LyricMetadata, tag: &str, value: &str) {
    let value = value.to_owned();
    match tag.to_ascii_lowercase().as_str() {
        "ar" => metadata.artist = Some(value),
        "al" => metadata.album = Some(value),
        "ti" => metadata.title = Some(value),
        "au" => metadata.author = Some(value),
        "by" => metadata.by = Some(value),
        "re" | "creator" => metadata.creator = Some(value),
        "ve" | "version" => metadata.version = Some(value),
        "length" => metadata.length = parse_lrc_timestamp(&value),
        "offset" => {
            metadata.offset = value.trim().parse::<i64>().ok().map(|ms| ms * TICKS_PER_MS);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use hermit_db::Database;
    use hermit_model::data::BaseItemKind;
    use hermit_model::lyrics::{LyricMetadata, LyricSearchRequest};
    use hermit_traits::error::ServiceError;
    use hermit_traits::stubs::{LyricManager, LyricProvider, LyricResponse, RemoteLyricInfo};
    use uuid::Uuid;

    use super::{HermitLyricManager, parse_lrc, parse_txt, provider_id};
    use crate::test_support;

    /// A scripted in-memory [`LyricProvider`]: records the search request it
    /// received and replays canned search/fetch results.
    struct FakeLyricProvider {
        name: &'static str,
        search_results: Vec<RemoteLyricInfo>,
        fetch_response: Option<LyricResponse>,
        fail_search: bool,
        last_request: Mutex<Option<LyricSearchRequest>>,
        last_fetch_id: Mutex<Option<String>>,
    }

    impl FakeLyricProvider {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                search_results: Vec::new(),
                fetch_response: None,
                fail_search: false,
                last_request: Mutex::new(None),
                last_fetch_id: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl LyricProvider for FakeLyricProvider {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn search(
            &self,
            request: &LyricSearchRequest,
        ) -> Result<Vec<RemoteLyricInfo>, ServiceError> {
            *self.last_request.lock().expect("lock") = Some(request.clone());
            if self.fail_search {
                return Err(ServiceError::backend("provider down"));
            }
            Ok(self.search_results.clone())
        }

        async fn get_lyrics(
            &self,
            provider_local_id: &str,
        ) -> Result<Option<LyricResponse>, ServiceError> {
            *self.last_fetch_id.lock().expect("lock") = Some(provider_local_id.to_owned());
            Ok(self.fetch_response.clone())
        }
    }

    /// A synced provider-local search candidate named `id`.
    fn synced_result(id: &str, text: &str) -> RemoteLyricInfo {
        RemoteLyricInfo {
            id: id.to_owned(),
            provider_name: "Fake".to_owned(),
            metadata: LyricMetadata {
                artist: Some("Borislav Slavov".to_owned()),
                title: Some("I Want to Live".to_owned()),
                is_synced: Some(true),
                ..LyricMetadata::default()
            },
            lyrics: LyricResponse {
                format: "lrc".to_owned(),
                text: text.to_owned(),
            },
        }
    }

    /// Seeds an `Audio` row with the search-relevant columns and a media path.
    async fn seed_audio(db: &Database, path: &str) -> Uuid {
        let id = Uuid::new_v4();
        test_support::seed_named_item(db, id, BaseItemKind::Audio, "I Want to Live").await;
        sqlx::query(
            r#"UPDATE "BaseItems"
               SET "Path" = ?2, "Album" = ?3, "Artists" = ?4,
                   "AlbumArtists" = ?5, "RunTimeTicks" = ?6
               WHERE "Id" = ?1"#,
        )
        .bind(id.to_string())
        .bind(path)
        .bind("Baldur's Gate 3")
        .bind("Borislav Slavov|Extra Artist")
        .bind("Borislav Slavov")
        .bind(233 * 10_000_000_i64)
        .execute(db.writer())
        .await
        .expect("update audio row");
        id
    }

    /// A manager over a fresh in-memory item store plus the given providers.
    fn manager_over(db: &Database, providers: Vec<Arc<dyn LyricProvider>>) -> HermitLyricManager {
        use crate::item_repository::HermitItemRepository;
        use crate::item_type_lookup::ItemTypeLookup;
        let lookup: Arc<dyn hermit_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        HermitLyricManager::new()
            .with_items(Arc::new(HermitItemRepository::new(db.clone(), lookup)))
            .with_providers(providers)
    }

    #[tokio::test]
    async fn search_builds_request_and_namespaces_results() {
        let db = test_support::test_db().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let media = dir.path().join("song.flac");
        std::fs::write(&media, b"flac").expect("write media");
        let item_id = seed_audio(&db, &media.to_string_lossy()).await;

        let mut fake = FakeLyricProvider::new("Fake");
        fake.search_results = vec![synced_result("42_synced", "[00:01.00]Hello")];
        let fake = Arc::new(fake);
        let mgr = manager_over(&db, vec![Arc::clone(&fake) as Arc<dyn LyricProvider>]);

        let results = mgr.search_lyrics(item_id).await.expect("search");
        assert_eq!(results.len(), 1);
        // The id is namespaced with the md5 provider id.
        assert_eq!(results[0].id, format!("{}_42_synced", provider_id("Fake")));
        assert_eq!(results[0].provider_name, "Fake");
        // The lyric text is parsed and the provider metadata stamped over it.
        assert_eq!(results[0].lyrics.lyrics[0].text, "Hello");
        assert_eq!(results[0].lyrics.lyrics[0].start, Some(1_000 * 10_000));
        assert_eq!(
            results[0].lyrics.metadata.artist.as_deref(),
            Some("Borislav Slavov")
        );

        // The provider saw the item-derived request.
        let seen = fake
            .last_request
            .lock()
            .expect("lock")
            .clone()
            .expect("request");
        assert_eq!(seen.song_name.as_deref(), Some("I Want to Live"));
        assert_eq!(seen.album_name.as_deref(), Some("Baldur's Gate 3"));
        assert_eq!(
            seen.artist_names.as_deref(),
            Some(&["Borislav Slavov".to_owned(), "Extra Artist".to_owned()][..])
        );
        assert_eq!(
            seen.album_artists_names.as_deref(),
            Some(&["Borislav Slavov".to_owned()][..])
        );
        assert_eq!(seen.duration, Some(233 * 10_000_000));
        assert_eq!(seen.media_path.as_deref(), Some(&*media.to_string_lossy()));
    }

    #[tokio::test]
    async fn search_skips_failing_provider_and_falls_through() {
        let db = test_support::test_db().await;
        let item_id = seed_audio(&db, "/nonexistent/song.mp3").await;

        let mut broken = FakeLyricProvider::new("Broken");
        broken.fail_search = true;
        let mut second = FakeLyricProvider::new("Second");
        second.search_results = vec![synced_result("7_synced", "[00:02.00]Fallback")];

        let mgr = manager_over(
            &db,
            vec![
                Arc::new(broken) as Arc<dyn LyricProvider>,
                Arc::new(second) as Arc<dyn LyricProvider>,
            ],
        );

        let results = mgr.search_lyrics(item_id).await.expect("search");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, format!("{}_7_synced", provider_id("Second")));
    }

    #[tokio::test]
    async fn download_saves_synced_sidecar_as_lrc() {
        let db = test_support::test_db().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let media = dir.path().join("song.flac");
        std::fs::write(&media, b"flac").expect("write media");
        let item_id = seed_audio(&db, &media.to_string_lossy()).await;

        let mut fake = FakeLyricProvider::new("Fake");
        fake.fetch_response = Some(LyricResponse {
            format: "lrc".to_owned(),
            text: "[00:17.12]I want to live".to_owned(),
        });
        let fake = Arc::new(fake);
        let mgr = manager_over(&db, vec![Arc::clone(&fake) as Arc<dyn LyricProvider>]);

        let lyric_id = format!("{}_42_synced", provider_id("Fake"));
        let dto = mgr
            .download_lyrics(item_id, &lyric_id)
            .await
            .expect("download")
            .expect("lyric saved");

        // The provider got the local id with the namespace stripped.
        assert_eq!(
            fake.last_fetch_id.lock().expect("lock").as_deref(),
            Some("42_synced")
        );
        // Synced content lands next to the media file as `.lrc`.
        let sidecar = media.with_extension("lrc");
        assert_eq!(
            std::fs::read_to_string(&sidecar).expect("sidecar exists"),
            "[00:17.12]I want to live"
        );
        assert_eq!(dto.metadata.is_synced, Some(true));
        assert_eq!(dto.lyrics[0].text, "I want to live");

        // The local read path now serves the downloaded lyric.
        let read_back = mgr.get_lyrics(item_id).await.expect("get").expect("some");
        assert_eq!(read_back, dto);
    }

    #[tokio::test]
    async fn download_saves_plain_sidecar_as_txt() {
        let db = test_support::test_db().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let media = dir.path().join("song.mp3");
        std::fs::write(&media, b"mp3").expect("write media");
        let item_id = seed_audio(&db, &media.to_string_lossy()).await;

        let mut fake = FakeLyricProvider::new("Fake");
        fake.fetch_response = Some(LyricResponse {
            format: "txt".to_owned(),
            text: "I want to live\nSomewhere I belong".to_owned(),
        });
        let mgr = manager_over(&db, vec![Arc::new(fake) as Arc<dyn LyricProvider>]);

        let lyric_id = format!("{}_42_plain", provider_id("Fake"));
        let dto = mgr
            .download_lyrics(item_id, &lyric_id)
            .await
            .expect("download")
            .expect("lyric saved");

        assert!(media.with_extension("txt").is_file());
        assert!(!media.with_extension("lrc").exists());
        assert_eq!(dto.metadata.is_synced, Some(false));
        assert_eq!(dto.lyrics.len(), 2);
    }

    #[tokio::test]
    async fn download_unknown_provider_id_returns_none() {
        let db = test_support::test_db().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let media = dir.path().join("song.flac");
        std::fs::write(&media, b"flac").expect("write media");
        let item_id = seed_audio(&db, &media.to_string_lossy()).await;

        let mgr = manager_over(
            &db,
            vec![Arc::new(FakeLyricProvider::new("Fake")) as Arc<dyn LyricProvider>],
        );

        let dto = mgr
            .download_lyrics(item_id, "deadbeef_42_synced")
            .await
            .expect("download runs");
        assert!(dto.is_none());
    }

    #[tokio::test]
    async fn download_without_providers_is_rejected() {
        let mgr = HermitLyricManager::new();
        assert!(mgr.download_lyrics(Uuid::new_v4(), "x_y").await.is_err());
    }

    #[tokio::test]
    async fn supported_providers_only_for_audio() {
        let db = test_support::test_db().await;
        let audio_id = seed_audio(&db, "/music/song.flac").await;
        let movie_id = Uuid::new_v4();
        test_support::seed_item(&db, movie_id, BaseItemKind::Movie).await;

        let mgr = manager_over(
            &db,
            vec![Arc::new(FakeLyricProvider::new("LrcLib Lyrics")) as Arc<dyn LyricProvider>],
        );

        let infos = mgr.get_supported_providers(audio_id).await.expect("audio");
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "LrcLib Lyrics");
        assert_eq!(infos[0].id, provider_id("LrcLib Lyrics"));

        assert!(
            mgr.get_supported_providers(movie_id)
                .await
                .expect("movie")
                .is_empty()
        );
    }

    #[test]
    fn parses_synced_lrc_with_metadata() {
        let lrc = "[ar:Beatles]\n[ti:Hey Jude]\n[00:12.34]First line\n[01:05.00]Second line\n";
        let dto = parse_lrc(lrc);
        assert_eq!(dto.metadata.artist.as_deref(), Some("Beatles"));
        assert_eq!(dto.metadata.title.as_deref(), Some("Hey Jude"));
        assert_eq!(dto.metadata.is_synced, Some(true));
        assert_eq!(dto.lyrics.len(), 2);
        assert_eq!(dto.lyrics[0].text, "First line");
        // 12.34s → 12_340 ms → ticks.
        assert_eq!(dto.lyrics[0].start, Some(12_340 * 10_000));
        assert_eq!(dto.lyrics[1].start, Some(65_000 * 10_000));
    }

    #[test]
    fn parses_plain_txt_unsynced() {
        let dto = parse_txt("Line one\n\nLine two\n");
        assert_eq!(dto.metadata.is_synced, Some(false));
        assert_eq!(dto.lyrics.len(), 2);
        assert!(dto.lyrics[0].start.is_none());
        assert_eq!(dto.lyrics[1].text, "Line two");
    }

    #[tokio::test]
    async fn no_item_store_reads_empty() {
        let mgr = HermitLyricManager::new();
        assert!(mgr.get_lyrics(Uuid::new_v4()).await.expect("get").is_none());
        assert!(
            mgr.search_lyrics(Uuid::new_v4())
                .await
                .expect("search")
                .is_empty()
        );
    }
}
