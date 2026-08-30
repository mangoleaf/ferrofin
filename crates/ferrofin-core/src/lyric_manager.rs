//! [`FerrofinLyricManager`] — local lyric reading (`.lrc`/`.elrc`/`.txt`) plus
//! remote lyric search/download through registered [`LyricProvider`]s.
//!
//! Port of `MediaBrowser.Providers.Lyric.LyricManager` +
//! `LrcLyricParser`/`TxtLyricParser`.
//!
//! **Where a lyric lives.** Jellyfin's `LyricManager.TrySaveLyric` builds a
//! *list* of save paths: the media folder only when the item's library has
//! `LibraryOptions.SaveLyricsWithMedia` (which defaults to **false**), and the
//! item's internal metadata folder (`{metadata}/library/{id2}/{idN}`)
//! **always**; `TrySaveToFiles` then writes the first path that succeeds. That
//! is why an upload works on a read-only media mount — the normal deployment —
//! and it is what [`Self::save_lyric`] / [`Self::download_lyrics`] do here.
//! Reads and deletes look in both places, in the order
//! `MediaInfoResolver.GetExternalFiles` enumerates them (containing folder,
//! then internal metadata folder).
//!
//! **What a lyric parses to.** `LrcLyricParser` (priority Fourth) handles
//! `.lrc`/`.elrc`, then `TxtLyricParser` (Fifth) handles `.lrc`/`.elrc`/`.txt`
//! — so an `.lrc` with no usable timestamps falls through to the plain-text
//! parser. Neither parser ever populates `LyricDto.Metadata`: it is stamped
//! only from a remote provider's search result
//! (`LyricManager.InternalSearchProviderAsync`), so a locally parsed lyric
//! always serialises `"Metadata":{}`.
//!
//! `LrcLyricParser` does not parse LRC itself — it delegates to the `LrcParser`
//! NuGet package, which Jellyfin 10.11.8 ships as `LrcParser.dll`
//! `2025.0623.0+ae8e5a182e7841a8150e852bb8cd6a9e01be3548`. `parse_lrc` here is a
//! port of that library's `Lrc` decoder at that exact commit —
//! `LrcStartTimeUtils.SplitLyricAndTimeTag`, `LrcLyricParser.Decode` and
//! `LrcTimedTextUtils.TimedTextToObject` — because the two behaviours a
//! from-first-principles reading misses are both in there: the word-tag list is
//! seeded with the LINE's own start time (so text before the first `<mm:ss.xx>`
//! owes a cue at position 0), and a line with zero or more than one `[mm:ss.xx]`
//! start time is returned verbatim with its word tags NOT parsed at all.
//!
//! Remote lyrics mirror the C# manager over its `ILyricProvider[]` registry:
//! [`search_lyrics`](LyricManager::search_lyrics) builds a
//! [`LyricSearchRequest`] from the resolved item and queries the providers one
//! at a time until one has results (the `SearchAllProviders == false` default);
//! [`download_lyrics`](LyricManager::download_lyrics) routes the namespaced id
//! (`"{provider_id}_{provider_local_id}"`, provider id = MD5 of the lowercased
//! provider name — `LyricManager.GetProviderId`) back to its provider, saves
//! the fetched lyric, and returns the parsed [`LyricDto`]. With no providers
//! registered the manager behaves like a server without a lyric plugin: search
//! is empty, download misses.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::lyrics::{
    LyricDto, LyricLine, LyricLineCue, LyricMetadata, LyricSearchRequest, RemoteLyricInfoDto,
};
use ferrofin_model::providers::LyricProviderInfo;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::ItemRepository;
use ferrofin_traits::stubs::{LyricManager, LyricProvider};

use crate::item_type_lookup::kind_from_type_name;

/// The number of ffmpeg ticks (100 ns) per millisecond.
const TICKS_PER_MS: i64 = 10_000;

/// Sidecar extensions probed for an item's lyrics, in priority order: synced LRC
/// first, then plain text. Port of `NamingOptions.LyricFileExtensions`
/// (`.lrc`, `.elrc`, `.txt`).
const LYRIC_EXTENSIONS: [&str; 3] = ["lrc", "elrc", "txt"];

/// The lyric manager: reads/writes `.lrc`/`.elrc`/`.txt` sidecars for an item
/// (media folder and/or internal metadata folder, see the module docs), and
/// searches/downloads remote lyrics through the registered [`LyricProvider`]s.
#[derive(Clone, Default)]
pub struct FerrofinLyricManager {
    /// Resolves an item id to its media path (to locate the sidecar) and its
    /// name/artists/album/runtime (to build a provider search request). Absent
    /// → the manager behaves as an empty stub (unit tests).
    items: Option<Arc<dyn ItemRepository>>,
    /// The registered remote lyric providers. Empty → search returns nothing
    /// and download misses (a server with no lyric plugin).
    providers: Vec<Arc<dyn LyricProvider>>,
    /// Internal-metadata base (`{program-data}/metadata`). Uploaded and
    /// downloaded lyrics always land under `{metadata}/library/{id2}/{idN}`,
    /// which is what makes an upload work over a read-only media mount. Absent
    /// → only the media folder can be written (unit-test default).
    metadata_path: Option<PathBuf>,
    /// The virtual-folder seam used to resolve the item's library options, i.e.
    /// `LibraryOptions.SaveLyricsWithMedia`. Absent → treated as `false`, the
    /// upstream default: never write into the media folder unasked.
    virtual_folders: Option<Arc<dyn VirtualFolderManager>>,
}

impl std::fmt::Debug for FerrofinLyricManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinLyricManager")
            .field("has_item_store", &self.items.is_some())
            .field("providers", &self.providers.len())
            .field("metadata_path", &self.metadata_path)
            .finish_non_exhaustive()
    }
}

impl FerrofinLyricManager {
    /// Creates a lyric manager with no item store and no remote providers
    /// (empty stub).
    #[must_use]
    pub fn new() -> Self {
        Self {
            items: None,
            providers: Vec::new(),
            metadata_path: None,
            virtual_folders: None,
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

    /// Sets the internal-metadata base (`{program-data}/metadata`) that
    /// uploaded/downloaded lyrics are always written under — Jellyfin's
    /// `Audio.GetInternalMetadataPath()` target.
    #[must_use]
    pub fn with_metadata_path(mut self, metadata_path: impl Into<PathBuf>) -> Self {
        self.metadata_path = Some(metadata_path.into());
        self
    }

    /// Attaches the virtual-folder seam used to read the item's library
    /// `SaveLyricsWithMedia` option.
    #[must_use]
    pub fn with_virtual_folders(mut self, folders: Arc<dyn VirtualFolderManager>) -> Self {
        self.virtual_folders = Some(folders);
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

    /// The item's internal-metadata folder (`{metadata}/library/{id2}/{idN}`) —
    /// `BaseItem.GetInternalMetadataPath()`. `None` when no metadata base is
    /// configured.
    fn item_metadata_dir(&self, item_id: Uuid) -> Option<PathBuf> {
        let dashless = item_id.simple().to_string();
        Some(
            self.metadata_path
                .as_ref()?
                .join("library")
                .join(&dashless[..2])
                .join(&dashless),
        )
    }

    /// `LibraryOptions.SaveLyricsWithMedia` for the library that owns
    /// `media_path` — the flag that decides whether the media folder is a save
    /// target at all. Defaults to `false` (the upstream default) when no
    /// virtual-folder seam is attached or the lookup fails.
    async fn save_with_media(&self, media_path: &Path) -> bool {
        let Some(folders) = &self.virtual_folders else {
            return false;
        };
        let Ok(folders) = folders.get_virtual_folders().await else {
            return false;
        };
        folders
            .iter()
            .find(|f| f.locations.iter().any(|loc| media_path.starts_with(loc)))
            .and_then(|f| f.library_options.as_ref())
            .is_some_and(|o| o.save_lyrics_with_media)
    }

    /// The save-path list for a lyric, in `TrySaveLyric` order: the media folder
    /// only when the library opts in, then always the internal metadata folder.
    /// The file name is the media base name plus the lowercased lyric format.
    async fn save_targets(&self, item_id: Uuid, media_path: &Path, format: &str) -> Vec<PathBuf> {
        let name = sidecar_file_name(media_path, &normalise_format(format));
        let mut targets = Vec::new();
        if self.save_with_media(media_path).await
            && let Some(folder) = media_path.parent()
        {
            targets.push(folder.join(&name));
        }
        if let Some(dir) = self.item_metadata_dir(item_id) {
            targets.push(dir.join(&name));
        }
        targets
    }

    /// Writes `content` to the first path that accepts it — port of
    /// `TrySaveToFiles`, which returns after the first successful write and only
    /// throws when every candidate failed.
    fn try_save_to_files(targets: &[PathBuf], content: &str) -> Result<(), ServiceError> {
        let mut last_error: Option<std::io::Error> = None;
        for target in targets {
            let written = target
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(target, content));
            match written {
                Ok(()) => {
                    tracing::info!(path = %target.display(), "saved lyrics");
                    return Ok(());
                }
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.map_or_else(
            || ServiceError::backend("no lyric save path is configured"),
            |e| ServiceError::backend(format!("write lyrics: {e}")),
        ))
    }

    /// Every existing lyric sidecar for the item, in the order
    /// `MediaInfoResolver.GetExternalFiles` enumerates them: the media's
    /// containing folder first, then the internal metadata folder.
    fn lyric_files(&self, item_id: Uuid, media_path: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for ext in LYRIC_EXTENSIONS {
            let name = sidecar_file_name(media_path, ext);
            if let Some(folder) = media_path.parent() {
                out.push(folder.join(&name));
            }
            if let Some(dir) = self.item_metadata_dir(item_id) {
                out.push(dir.join(&name));
            }
        }
        out.retain(|p| p.is_file());
        out
    }

    /// Routes a namespaced lyric id (`"{provider_id}_{provider_local_id}"`,
    /// `GetProviderId` namespacing) back to its owning provider and fetches
    /// the raw lyric — port of `LyricManager.InternalGetRemoteLyricsAsync`.
    /// `None` when no registered provider owns the prefix (logged, as upstream
    /// `GetProvider` does) or the provider has no such lyric.
    async fn fetch_remote(
        &self,
        lyric_id: &str,
    ) -> Result<Option<ferrofin_traits::stubs::LyricResponse>, ServiceError> {
        // `id.Split('_', 2)`: the prefix before the first underscore is the
        // provider id, the remainder is the provider-local id.
        let (namespace, local_id) = lyric_id.split_once('_').unwrap_or((lyric_id, lyric_id));
        let Some(provider) = self
            .providers
            .iter()
            .find(|p| provider_id(p.name()) == namespace)
        else {
            tracing::warn!(lyric_id, "unknown lyric provider id");
            return Ok(None);
        };
        provider.get_lyrics(local_id).await
    }

    /// Runs one provider's search and maps its results into namespaced
    /// [`RemoteLyricInfoDto`]s. A provider that errors yields an empty list
    /// (logged) rather than failing the whole search, and a result whose lyric
    /// no parser accepts is skipped — port of
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
            .filter_map(|result| {
                // Parse the raw text and stamp the provider's metadata over the
                // parsed lyric (upstream `parsedLyrics.Metadata = result.Metadata`
                // — the only place a `LyricDto` ever gets metadata).
                let mut lyrics = parse_by_format(&result.lyrics.format, &result.lyrics.text)?;
                lyrics.metadata = result.metadata;
                Some(RemoteLyricInfoDto {
                    id: format!("{namespace}_{}", result.id),
                    provider_name: result.provider_name,
                    lyrics,
                })
            })
            .collect()
    }
}

/// The stable id of a provider: the MD5 of its lowercased name, hex-encoded.
/// Port of `LyricManager.GetProviderId` (`name.ToLowerInvariant().GetMD5()`,
/// `"N"` format) — [`get_md5`](ferrofin_common::extensions::get_md5) reproduces
/// the .NET UTF-16LE `GetMD5` extension byte-for-byte.
fn provider_id(name: &str) -> String {
    ferrofin_common::extensions::get_md5(&name.to_lowercase())
        .simple()
        .to_string()
}

/// A lyric format string (`"lrc"`, `".LRC"`, …) reduced to a bare lowercase
/// extension. Mirrors `format.ReplaceLineEndings("").ToLowerInvariant()` plus
/// the leading-dot tolerance `Path.GetExtension` gives the controller.
fn normalise_format(format: &str) -> String {
    format
        .trim()
        .trim_start_matches('.')
        .replace(['\r', '\n'], "")
        .to_ascii_lowercase()
}

/// `{media base name}.{ext}` — the sidecar file name Jellyfin saves and resolves
/// (`Path.GetFileNameWithoutExtension(audio.Path) + "." + format`).
fn sidecar_file_name(media_path: &Path, ext: &str) -> std::ffi::OsString {
    let mut name = media_path
        .file_stem()
        .map_or_else(std::ffi::OsString::new, std::ffi::OsStr::to_os_string);
    name.push(".");
    name.push(ext);
    name
}

/// Runs the parser chain over a named lyric file — port of
/// `LyricManager.InternalParseRemoteLyricsAsync`/`GetLyricsAsync`, which walk
/// `_lyricParsers` in priority order and take the first non-null result:
/// `LrcLyricParser` (`.lrc`/`.elrc`) then `TxtLyricParser`
/// (`.lrc`/`.elrc`/`.txt`). `None` when no parser claims the extension, which is
/// what turns an upload of `x.foo` into a `400`.
fn parse_by_name(name: &str, content: &str) -> Option<LyricDto> {
    let ext = Path::new(name)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(ext.as_str(), "lrc" | "elrc")
        && let Some(dto) = parse_lrc(content)
    {
        return Some(dto);
    }
    if matches!(ext.as_str(), "lrc" | "elrc" | "txt") {
        return Some(parse_txt(content));
    }
    None
}

/// The parser chain for a bare format string — `new LyricFile($"lyric.{format}")`.
fn parse_by_format(format: &str, text: &str) -> Option<LyricDto> {
    parse_by_name(&format!("lyric.{}", normalise_format(format)), text)
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

/// True for the item kinds Jellyfin's `GetItemById<Audio>` accepts: `Audio` and
/// its `AudioBook` subclass.
fn is_audio_kind(type_name: &str) -> bool {
    matches!(
        kind_from_type_name(type_name),
        Some(BaseItemKind::Audio | BaseItemKind::AudioBook)
    )
}

#[async_trait]
impl LyricManager for FerrofinLyricManager {
    async fn get_lyrics(&self, item_id: Uuid) -> Result<Option<LyricDto>, ServiceError> {
        let Some(path) = self.item_path(item_id).await? else {
            return Ok(None);
        };
        // `GetLyricsAsync`: walk the item's lyric files and return the first one
        // a parser claims.
        for file in self.lyric_files(item_id, &path) {
            let Ok(content) = std::fs::read_to_string(&file) else {
                continue;
            };
            let name = file.file_name().unwrap_or_default().to_string_lossy();
            if let Some(dto) = parse_by_name(&name, &content) {
                return Ok(Some(dto));
            }
        }
        Ok(None)
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
        let Some(path) = self.item_path(item_id).await? else {
            return Err(ServiceError::not_found("item has no media path for lyrics"));
        };

        // `DownloadLyricsAsync`: fetch, parse, and only then save. An unknown
        // provider prefix or a provider miss is a plain `null` → 404.
        let Some(response) = self.fetch_remote(lyric_id).await? else {
            tracing::debug!(lyric_id, "unable to download lyrics");
            return Ok(None);
        };
        let Some(dto) = parse_by_format(&response.format, &response.text) else {
            return Ok(None);
        };

        let targets = self.save_targets(item_id, &path, &response.format).await;
        Self::try_save_to_files(&targets, &response.text)?;
        Ok(Some(dto))
    }

    async fn get_remote_lyrics(&self, lyric_id: &str) -> Result<Option<LyricDto>, ServiceError> {
        // `LyricManager.GetRemoteLyricsAsync`: fetch + parse only — no item,
        // no sidecar. An unknown provider id or a provider miss is `None`.
        let Some(response) = self.fetch_remote(lyric_id).await? else {
            return Ok(None);
        };
        Ok(parse_by_format(&response.format, &response.text))
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
        // `SaveLyricAsync` parses FIRST and returns null — the controller's
        // `400` — when no parser claims the format. Nothing is written then.
        let Some(dto) = parse_by_format(format, lyrics) else {
            return Ok(None);
        };
        let targets = self.save_targets(item_id, &path, format).await;
        Self::try_save_to_files(&targets, lyrics)?;
        Ok(Some(dto))
    }

    async fn delete_lyrics(&self, item_id: Uuid) -> Result<(), ServiceError> {
        let Some(path) = self.item_path(item_id).await? else {
            return Ok(());
        };
        // `DeleteLyricsAsync` deletes each lyric file with no `catch`: a failed
        // unlink is an error, never a silent `204`.
        for file in self.lyric_files(item_id, &path) {
            match std::fs::remove_file(&file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(ServiceError::backend(format!("delete lyrics: {e}"))),
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
            .is_some_and(|i| is_audio_kind(&i.type_));
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

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

/// Splits on `\r\n` | `\r` | `\n` keeping every element, including the trailing
/// empty one a file ending in a newline produces — .NET
/// `Split(_lineBreakCharacters, StringSplitOptions.None)`. `str::lines()` is
/// **not** equivalent: it swallows that trailing element.
fn split_lines(content: &str) -> Vec<&str> {
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let (mut start, mut i) = (0usize, 0usize);
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                out.push(&content[start..i]);
                i += usize::from(bytes.get(i + 1) == Some(&b'\n')) + 1;
                start = i;
            }
            b'\n' => {
                out.push(&content[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    out.push(&content[start..]);
    out
}

/// Parses an LRC time-tag body (`mm:ss.ff` / `mm:ss.fff`) into milliseconds.
///
/// The grammar is exactly the one the `LrcParser` library accepts and no wider:
/// one or more minute digits, **exactly two** second digits, and a two- or
/// three-digit fraction (centiseconds / milliseconds). `[00:12]`, `[00:5.00]`,
/// `[00:12.3]` and `[00:12.1234]` are all rejected by Jellyfin (measured), and a
/// rejected tag stays in the lyric text verbatim rather than becoming a line.
fn parse_time_tag(inner: &str) -> Option<i64> {
    fn digits(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
    }
    let (min_s, rest) = inner.split_once(':')?;
    let (sec_s, fraction) = rest.split_once('.')?;
    if !digits(min_s) || sec_s.len() != 2 || !digits(sec_s) {
        return None;
    }
    if !matches!(fraction.len(), 2 | 3) || !digits(fraction) {
        return None;
    }
    let minutes: i64 = min_s.parse().ok()?;
    let seconds: i64 = sec_s.parse().ok()?;
    let parsed: i64 = fraction.parse().ok()?;
    // Two digits are centiseconds, three are milliseconds.
    let millis = if fraction.len() == 2 {
        parsed * 10
    } else {
        parsed
    };
    minutes
        .checked_mul(60)?
        .checked_add(seconds)?
        .checked_mul(1000)?
        .checked_add(millis)
}

/// One time tag matched inside a source line: its byte range and its value in
/// milliseconds.
struct TimeTag {
    /// Byte offset of the opening delimiter.
    start: usize,
    /// Byte offset one past the closing delimiter.
    end: usize,
    /// The tag's value in milliseconds.
    ms: i64,
}

/// Finds every valid time tag delimited by `open`/`close`, in source order.
///
/// Equivalent to the `LrcParser` library's `TimeTagUtils` regexes
/// (`\[(\d{1,}):(\d{2})\.(\d{2,3})\]` and its `<>` twin): a delimited run that
/// is not a valid time tag is left in the surrounding text verbatim, which is
/// why Jellyfin keeps `[ti:T]` and `<00:08>` inside the lyric (measured), and
/// why the scan restarts just past a rejected `[` so `[abc[00:05.00]` still
/// yields the inner timestamp — exactly what the regex engine does.
fn find_time_tags(line: &str, open: char, close: char) -> Vec<TimeTag> {
    let mut out = Vec::new();
    let mut base = 0usize;
    while let Some(rel) = line[base..].find(open) {
        let start = base + rel;
        let inner_from = start + open.len_utf8();
        let Some(rel_end) = line[inner_from..].find(close) else {
            break;
        };
        let inner_to = inner_from + rel_end;
        if let Some(ms) = parse_time_tag(&line[inner_from..inner_to]) {
            let end = inner_to + close.len_utf8();
            out.push(TimeTag { start, end, ms });
            base = end;
        } else {
            base = inner_from;
        }
    }
    out
}

/// Splits an LRC line into its `[mm:ss.xx]` line time tags and the remaining
/// text. Port of `LrcStartTimeUtils.SplitLyricAndTimeTag`: **every** line time
/// tag is collected wherever it occurs, all of them are removed from the text,
/// and what is left is `.Trim()`ed.
fn split_lyric_and_time_tag(line: &str) -> (Vec<i64>, String) {
    if line.trim().is_empty() {
        return (Vec::new(), String::new());
    }
    let tags = find_time_tags(line, '[', ']');
    let mut text = String::new();
    let mut prev = 0usize;
    for tag in &tags {
        text.push_str(&line[prev..tag.start]);
        prev = tag.end;
    }
    text.push_str(&line[prev..]);
    (tags.iter().map(|t| t.ms).collect(), text.trim().to_owned())
}

/// `LrcParser.Model.TextIndex`: a UTF-16 index into the rendered line plus the
/// `IndexState` saying whether the tag sits *before* that character (`Start`)
/// or *after* it (`End`).
#[derive(Clone, Copy, PartialEq, Eq)]
struct TextIndex {
    /// The UTF-16 index. `-1` is reachable — `lyricText.Length - 1` on an empty
    /// builder — and resolves to position 0.
    index: i32,
    /// True for `IndexState.End`.
    end_state: bool,
}

impl TextIndex {
    /// The character position `LrcLyricParser` slices at:
    /// `State == IndexState.End ? Index + 1 : Index`.
    fn position(self) -> i32 {
        if self.end_state {
            self.index.saturating_add(1)
        } else {
            self.index
        }
    }
}

/// The rendered text of one LRC line plus its ordered word time tags.
struct TimedText {
    /// The lyric text with every word time tag removed.
    text: String,
    /// `(index, milliseconds)` in `SortedDictionary` order — which is also
    /// insertion order here, because both the `Start` indices and the trailing
    /// `End` index only ever move forward.
    tags: Vec<(TextIndex, i64)>,
}

/// `SortedDictionary.TryAdd`: the FIRST value written at a key wins.
///
/// It matters — a line ending in a whitespace-only segment inserts the same
/// `(len - 1, End)` key twice (once from the blank-segment branch, once from
/// the no-remaining-text branch) and Jellyfin keeps the earlier time.
fn try_add_tag(tags: &mut Vec<(TextIndex, i64)>, index: TextIndex, ms: i64) {
    if !tags.iter().any(|(k, _)| *k == index) {
        tags.push((index, ms));
    }
}

/// Parses one LRC line's text for enhanced-LRC `<mm:ss.xx>` word time tags.
/// Faithful port of `LrcParser`'s `LrcTimedTextUtils.TimedTextToObject`
/// (v2025.0623.0, the build Jellyfin 10.11.8 ships as `LrcParser.dll`).
///
/// The detail that is easy to miss, and that a corpus of lines all starting
/// with a word tag cannot expose: `lastTimeTag` is seeded with the **line's own
/// start time**, so a line carrying text before its first word tag records a
/// tag at index 0 holding that line start — `[00:01.00]hello <00:02.00>there`
/// has cues for *both* "hello" and "there", not just "there".
///
/// Whitespace: each segment is `Trim()`ed, a single space is inserted when the
/// previous segment ended with whitespace or the next one starts with it, and
/// whitespace *inside* a segment survives verbatim.
fn timed_text_to_object(timed_text: &str, line_start_ms: i64) -> TimedText {
    if timed_text.trim().is_empty() {
        return TimedText {
            text: String::new(),
            tags: Vec::new(),
        };
    }
    let matches = find_time_tags(timed_text, '<', '>');
    if matches.is_empty() {
        // No word time tags: the line is returned exactly as it came in.
        return TimedText {
            text: timed_text.to_owned(),
            tags: Vec::new(),
        };
    }

    let mut text = String::new();
    // The C# tracks `lyricText.Length`, i.e. UTF-16 code units.
    let mut len16: i32 = 0;
    let mut tags: Vec<(TextIndex, i64)> = Vec::new();
    let mut last_ms = line_start_ms;
    let mut segment_start = 0usize;
    let mut insert_space = false;
    let mut last_tag_was_start = false;

    for m in &matches {
        let segment = &timed_text[segment_start..m.start];
        segment_start = m.end;

        if segment.trim().is_empty() {
            if last_tag_was_start {
                // The previous tag opened a run that this tag closes.
                try_add_tag(
                    &mut tags,
                    TextIndex {
                        index: len16 - 1,
                        end_state: true,
                    },
                    last_ms,
                );
                last_tag_was_start = false;
            }
            last_ms = m.ms;
            if !segment.is_empty() {
                insert_space = true;
            }
            continue;
        }

        if (segment.starts_with(char::is_whitespace) || insert_space) && len16 > 0 {
            text.push(' ');
            len16 += 1;
        }
        try_add_tag(
            &mut tags,
            TextIndex {
                index: len16,
                end_state: false,
            },
            last_ms,
        );
        last_tag_was_start = true;
        let core = segment.trim();
        text.push_str(core);
        len16 = len16.saturating_add(to_i32(core.encode_utf16().count()));
        last_ms = m.ms;
        insert_space = segment.ends_with(char::is_whitespace);
    }

    let remaining = &timed_text[segment_start..];
    if remaining.trim().is_empty() {
        try_add_tag(
            &mut tags,
            TextIndex {
                index: len16 - 1,
                end_state: true,
            },
            last_ms,
        );
    } else {
        if (remaining.starts_with(char::is_whitespace) || insert_space) && len16 > 0 {
            text.push(' ');
            len16 += 1;
        }
        try_add_tag(
            &mut tags,
            TextIndex {
                index: len16,
                end_state: false,
            },
            last_ms,
        );
        text.push_str(remaining.trim());
    }

    TimedText { text, tags }
}

/// True when the UTF-16 slice `[a, b)` trims to nothing (C#
/// `currentSlice.Trim().Length == 0`). Positions are clamped, because a tag at
/// `TextIndex(-1, End)` resolves to 0 and the final slice runs to the end.
fn slice_is_blank(units: &[u16], a: i32, b: i32) -> bool {
    let clamp = |v: i32| usize::try_from(v).unwrap_or(0).min(units.len());
    let (a, b) = (clamp(a), clamp(b));
    a >= b || String::from_utf16_lossy(&units[a..b]).trim().is_empty()
}

/// A character index narrowed to the `i32` the DTO carries; saturates rather
/// than wrapping (a lyric line long enough to overflow cannot occur).
fn to_i32(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// One decoded LRC line — `LrcParser.Model.Lyric` after
/// `LrcParser.PostProcess` has expanded a multi-start line into one entry per
/// start time.
struct LrcLine {
    /// The rendered lyric text.
    text: String,
    /// This entry's start time, in milliseconds.
    start_ms: i64,
    /// The line's word time tags (empty unless it had exactly one start time).
    tags: Vec<(TextIndex, i64)>,
}

/// Parses an LRC/ELRC document into a [`LyricDto`]. Port of
/// `LrcLyricParser.ParseLyrics` over the `LrcParser` library's decoder.
///
/// `Metadata` is **never** populated — upstream returns
/// `new LyricDto { Lyrics = lyricList }` and leaves `Metadata` at its empty
/// default even for a file carrying `[ar:]`/`[ti:]`/`[al:]` tags (those lines
/// carry no line time tag, so `StartTimes` is empty and they produce no lyric
/// at all). Returns `None` when no timestamped line was produced
/// (`sortedLyricData.Count == 0`), which is what makes an untimed `.lrc` fall
/// through to [`parse_txt`].
fn parse_lrc(content: &str) -> Option<LyricDto> {
    let mut lines: Vec<LrcLine> = Vec::new();
    for raw in split_lines(content) {
        // `LyricParser.Decode` drops whitespace-only lines before parsing.
        if raw.trim().is_empty() {
            continue;
        }
        let (start_times, raw_lyric) = split_lyric_and_time_tag(raw);
        // `LrcLyricParser.Decode`: with no start time, or with more than one,
        // the word time tags cannot be attributed to a start time, so the
        // library deliberately returns the line as-is and parses none of them
        // — `[00:01.00][00:05.00]a <00:10.00>b` keeps `<00:10.00>` in the text.
        let decoded = if start_times.len() == 1 {
            timed_text_to_object(&raw_lyric, start_times[0])
        } else {
            TimedText {
                text: raw_lyric,
                tags: Vec::new(),
            }
        };
        for start_ms in start_times {
            lines.push(LrcLine {
                text: decoded.text.clone(),
                start_ms,
                tags: decoded.tags.clone(),
            });
        }
    }
    if lines.is_empty() {
        return None;
    }

    // `OrderBy(x => x.StartTime)` — .NET's stable sort, as is `sort_by_key`.
    lines.sort_by_key(|l| l.start_ms);

    let mut out = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        let units: Vec<u16> = line.text.encode_utf16().collect();
        let mut cues = Vec::new();
        if let Some((last, pairs)) = line.tags.split_last() {
            for (cur, next) in pairs.iter().zip(line.tags.iter().skip(1)) {
                let (a, b) = (cur.0.position(), next.0.position());
                if slice_is_blank(&units, a, b) {
                    continue;
                }
                cues.push(LyricLineCue {
                    position: a,
                    end_position: b,
                    start: cur.1 * TICKS_PER_MS,
                    end: Some(next.1 * TICKS_PER_MS),
                });
            }
            // The last tag runs to the end of the line; its `end` is the next
            // SORTED line's start, or absent on the final line.
            let a = last.0.position();
            let end_of_text = to_i32(units.len());
            if !slice_is_blank(&units, a, end_of_text) {
                cues.push(LyricLineCue {
                    position: a,
                    end_position: end_of_text,
                    start: last.1 * TICKS_PER_MS,
                    end: lines.get(i + 1).map(|n| n.start_ms * TICKS_PER_MS),
                });
            }
        }
        out.push(LyricLine {
            text: line.text.clone(),
            start: Some(line.start_ms * TICKS_PER_MS),
            cues: Some(cues),
        });
    }

    Some(LyricDto {
        metadata: LyricMetadata::default(),
        lyrics: out,
    })
}

/// Parses a plain-text lyric file. Port of `TxtLyricParser.ParseLyrics`: every
/// split element becomes a line (blank lines **kept**, including the trailing
/// one), each `.Trim()`ed, with no start and no cues, and an empty `Metadata`.
fn parse_txt(content: &str) -> LyricDto {
    LyricDto {
        metadata: LyricMetadata::default(),
        lyrics: split_lines(content)
            .into_iter()
            .map(|line| LyricLine {
                text: line.trim().to_owned(),
                start: None,
                cues: None,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use ferrofin_db::Database;
    use ferrofin_db::store::guid_to_db;
    use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::entities::CollectionTypeOptions;
    use ferrofin_model::entities_media::VirtualFolderInfo;
    use ferrofin_model::lyrics::{LyricLineCue, LyricMetadata, LyricSearchRequest};
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::library::VirtualFolderManager;
    use ferrofin_traits::stubs::{LyricManager, LyricProvider, LyricResponse, RemoteLyricInfo};
    use uuid::Uuid;

    use super::{
        FerrofinLyricManager, parse_by_name, parse_lrc, parse_txt, provider_id, split_lines,
    };
    use crate::test_support;

    /// Jellyfin's own enhanced-LRC oracle
    /// (`tests/Jellyfin.Providers.Tests/Test Data/Lyrics/Fleetwood Mac - Rumors.elrc`),
    /// asserted against below exactly as `LrcLyricParserTests.ParseElrcCues` does.
    const RUMORS_ELRC: &str = include_str!("data/fleetwood-mac-rumors.elrc");

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

    /// A virtual-folder seam that reports one library over `root` with
    /// `SaveLyricsWithMedia` set as given — the only option the lyric manager
    /// reads. Every mutation is unreachable here.
    struct FakeFolders {
        root: String,
        save_with_media: bool,
    }

    #[async_trait]
    impl VirtualFolderManager for FakeFolders {
        async fn get_virtual_folders(&self) -> Result<Vec<VirtualFolderInfo>, ServiceError> {
            Ok(vec![VirtualFolderInfo {
                name: Some("Music".to_owned()),
                locations: vec![self.root.clone()],
                library_options: Some(LibraryOptions {
                    save_lyrics_with_media: self.save_with_media,
                    ..LibraryOptions::default()
                }),
                ..VirtualFolderInfo::default()
            }])
        }

        async fn add_virtual_folder(
            &self,
            _name: &str,
            _collection_type: Option<CollectionTypeOptions>,
            _options: &LibraryOptions,
        ) -> Result<(), ServiceError> {
            unreachable!()
        }

        async fn remove_virtual_folder(&self, _name: &str) -> Result<(), ServiceError> {
            unreachable!()
        }

        async fn rename_virtual_folder(&self, _n: &str, _new: &str) -> Result<(), ServiceError> {
            unreachable!()
        }

        async fn add_media_path(
            &self,
            _name: &str,
            _info: &MediaPathInfo,
        ) -> Result<(), ServiceError> {
            unreachable!()
        }

        async fn update_media_path(
            &self,
            _name: &str,
            _info: &MediaPathInfo,
        ) -> Result<(), ServiceError> {
            unreachable!()
        }

        async fn remove_media_path(&self, _name: &str, _path: &str) -> Result<(), ServiceError> {
            unreachable!()
        }

        async fn update_library_options(
            &self,
            _name: &str,
            _options: &LibraryOptions,
        ) -> Result<(), ServiceError> {
            unreachable!()
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
        .bind(guid_to_db(id))
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
    fn manager_over(db: &Database, providers: Vec<Arc<dyn LyricProvider>>) -> FerrofinLyricManager {
        use crate::item_repository::FerrofinItemRepository;
        use crate::item_type_lookup::ItemTypeLookup;
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        FerrofinLyricManager::new()
            .with_items(Arc::new(FerrofinItemRepository::new(db.clone(), lookup)))
            .with_providers(providers)
    }

    /// `{metadata}/library/{id2}/{idN}/{stem}.{ext}` — where a saved lyric lands.
    fn metadata_sidecar(meta: &std::path::Path, id: Uuid, stem: &str, ext: &str) -> PathBuf {
        let dashless = id.simple().to_string();
        meta.join("library")
            .join(&dashless[..2])
            .join(&dashless)
            .join(format!("{stem}.{ext}"))
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
    async fn download_saves_into_the_internal_metadata_folder() {
        let db = test_support::test_db().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let meta = tempfile::tempdir().expect("tempdir");
        let media = dir.path().join("song.flac");
        std::fs::write(&media, b"flac").expect("write media");
        let item_id = seed_audio(&db, &media.to_string_lossy()).await;

        let mut fake = FakeLyricProvider::new("Fake");
        fake.fetch_response = Some(LyricResponse {
            format: "lrc".to_owned(),
            text: "[00:17.12]I want to live".to_owned(),
        });
        let fake = Arc::new(fake);
        let mgr = manager_over(&db, vec![Arc::clone(&fake) as Arc<dyn LyricProvider>])
            .with_metadata_path(meta.path());

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
        // `SaveLyricsWithMedia` defaults to false: nothing is written next to
        // the media file (which is what makes a read-only mount work).
        assert!(!media.with_extension("lrc").exists());
        let saved = metadata_sidecar(meta.path(), item_id, "song", "lrc");
        assert_eq!(
            std::fs::read_to_string(&saved).expect("sidecar exists"),
            "[00:17.12]I want to live"
        );
        // A locally parsed lyric never carries metadata.
        assert_eq!(dto.metadata, LyricMetadata::default());
        assert_eq!(dto.lyrics[0].text, "I want to live");

        // The local read path now serves the saved lyric…
        let read_back = mgr.get_lyrics(item_id).await.expect("get").expect("some");
        assert_eq!(read_back, dto);
        // …and the delete removes it again.
        mgr.delete_lyrics(item_id).await.expect("delete");
        assert!(!saved.exists());
        assert!(mgr.get_lyrics(item_id).await.expect("get").is_none());
    }

    #[tokio::test]
    async fn save_lyrics_with_media_adds_the_media_folder_target() {
        let db = test_support::test_db().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let meta = tempfile::tempdir().expect("tempdir");
        let media = dir.path().join("song.flac");
        std::fs::write(&media, b"flac").expect("write media");
        let item_id = seed_audio(&db, &media.to_string_lossy()).await;

        let folders: Arc<dyn VirtualFolderManager> = Arc::new(FakeFolders {
            root: dir.path().to_string_lossy().into_owned(),
            save_with_media: true,
        });
        let mgr = manager_over(&db, Vec::new())
            .with_metadata_path(meta.path())
            .with_virtual_folders(folders);

        mgr.save_lyric(item_id, "lrc", "[00:01.00]hi")
            .await
            .expect("save")
            .expect("parsed");

        // The media folder is first in `TrySaveLyric`'s list and writable here,
        // so `TrySaveToFiles` stops there — nothing lands in metadata.
        assert_eq!(
            std::fs::read_to_string(media.with_extension("lrc")).expect("media sidecar"),
            "[00:01.00]hi"
        );
        assert!(!metadata_sidecar(meta.path(), item_id, "song", "lrc").exists());
    }

    #[tokio::test]
    async fn save_lyric_rejects_an_unparsable_format_without_writing() {
        let db = test_support::test_db().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let meta = tempfile::tempdir().expect("tempdir");
        let media = dir.path().join("song.flac");
        std::fs::write(&media, b"flac").expect("write media");
        let item_id = seed_audio(&db, &media.to_string_lossy()).await;
        let mgr = manager_over(&db, Vec::new()).with_metadata_path(meta.path());

        // `x.foo`: no parser claims it → null → the controller's 400, no file.
        assert!(
            mgr.save_lyric(item_id, "foo", "hello")
                .await
                .expect("runs")
                .is_none()
        );
        assert!(!metadata_sidecar(meta.path(), item_id, "song", "foo").exists());

        // A `.txt` upload is stored and read back through the plain parser.
        let dto = mgr
            .save_lyric(item_id, "txt", "alpha\n\nbeta\n")
            .await
            .expect("save")
            .expect("parsed");
        assert_eq!(
            dto.lyrics
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "", "beta", ""]
        );
        assert_eq!(mgr.get_lyrics(item_id).await.expect("get"), Some(dto));
    }

    #[tokio::test]
    async fn delete_reports_a_failed_unlink() {
        let db = test_support::test_db().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let meta = tempfile::tempdir().expect("tempdir");
        let media = dir.path().join("song.flac");
        std::fs::write(&media, b"flac").expect("write media");
        let sidecar = media.with_extension("lrc");
        std::fs::write(&sidecar, "[00:01.00]hi").expect("write sidecar");
        let item_id = seed_audio(&db, &media.to_string_lossy()).await;
        let mgr = manager_over(&db, Vec::new()).with_metadata_path(meta.path());

        // A read-only parent makes the unlink fail. Upstream
        // `DeleteLyricsAsync` has no `catch`, so this must surface as an error
        // rather than the silent `204` that used to leave the lyric in place.
        let mut perms = std::fs::metadata(dir.path()).expect("stat").permissions();
        let restore = perms.clone();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o555);
        std::fs::set_permissions(dir.path(), perms).expect("chmod");
        let denied = mgr.delete_lyrics(item_id).await;
        std::fs::set_permissions(dir.path(), restore).expect("restore");
        assert!(denied.is_err(), "a failed unlink must not report success");
        assert!(sidecar.is_file(), "the lyric is still there");

        // With the file gone there is nothing to delete, and that IS a success.
        std::fs::remove_file(&sidecar).expect("rm");
        mgr.delete_lyrics(item_id).await.expect("no-op delete");
    }

    #[tokio::test]
    async fn download_saves_plain_sidecar_as_txt() {
        let db = test_support::test_db().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let meta = tempfile::tempdir().expect("tempdir");
        let media = dir.path().join("song.mp3");
        std::fs::write(&media, b"mp3").expect("write media");
        let item_id = seed_audio(&db, &media.to_string_lossy()).await;

        let mut fake = FakeLyricProvider::new("Fake");
        fake.fetch_response = Some(LyricResponse {
            format: "txt".to_owned(),
            text: "I want to live\nSomewhere I belong".to_owned(),
        });
        let mgr = manager_over(&db, vec![Arc::new(fake) as Arc<dyn LyricProvider>])
            .with_metadata_path(meta.path());

        let lyric_id = format!("{}_42_plain", provider_id("Fake"));
        let dto = mgr
            .download_lyrics(item_id, &lyric_id)
            .await
            .expect("download")
            .expect("lyric saved");

        assert!(metadata_sidecar(meta.path(), item_id, "song", "txt").is_file());
        assert!(!metadata_sidecar(meta.path(), item_id, "song", "lrc").exists());
        assert_eq!(dto.metadata, LyricMetadata::default());
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
        // No providers at all is the same miss (`GetProvider` → null → 404),
        // not an error.
        assert!(
            manager_over(&db, Vec::new())
                .download_lyrics(item_id, "deadbeef_42_synced")
                .await
                .expect("runs")
                .is_none()
        );
    }

    #[tokio::test]
    async fn get_remote_lyrics_parses_without_item_or_sidecar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut fake = FakeLyricProvider::new("Fake");
        fake.fetch_response = Some(LyricResponse {
            format: "lrc".to_owned(),
            text: "[ar:Borislav Slavov]\n[00:17.12]I want to live".to_owned(),
        });
        let fake = Arc::new(fake);
        // No item store at all: the route needs none.
        let mgr = FerrofinLyricManager::new()
            .with_providers(vec![Arc::clone(&fake) as Arc<dyn LyricProvider>]);

        let lyric_id = format!("{}_42_synced", provider_id("Fake"));
        let dto = mgr
            .get_remote_lyrics(&lyric_id)
            .await
            .expect("fetch")
            .expect("lyric found");
        assert_eq!(
            fake.last_fetch_id.lock().expect("lock").as_deref(),
            Some("42_synced")
        );
        // The `[ar:]` tag is consumed but never surfaces as metadata.
        assert_eq!(dto.metadata, LyricMetadata::default());
        assert_eq!(dto.lyrics.len(), 1);
        assert_eq!(dto.lyrics[0].text, "I want to live");
        assert_eq!(dto.lyrics[0].start, Some(17_120 * 10_000));
        // Nothing was written anywhere.
        assert!(std::fs::read_dir(dir.path()).expect("dir").next().is_none());

        // Unknown provider prefix → `None` (not an error).
        assert!(
            mgr.get_remote_lyrics("deadbeef_42_synced")
                .await
                .expect("runs")
                .is_none()
        );
        // No providers at all → `None` as well (C# `GetProvider` miss).
        assert!(
            FerrofinLyricManager::new()
                .get_remote_lyrics(&lyric_id)
                .await
                .expect("runs")
                .is_none()
        );
    }

    #[tokio::test]
    async fn supported_providers_only_for_audio() {
        let db = test_support::test_db().await;
        let audio_id = seed_audio(&db, "/music/song.flac").await;
        let movie_id = Uuid::new_v4();
        test_support::seed_item(&db, movie_id, BaseItemKind::Movie).await;
        let audiobook_id = Uuid::new_v4();
        test_support::seed_item(&db, audiobook_id, BaseItemKind::AudioBook).await;

        let mgr = manager_over(
            &db,
            vec![Arc::new(FakeLyricProvider::new("LrcLib Lyrics")) as Arc<dyn LyricProvider>],
        );

        let infos = mgr.get_supported_providers(audio_id).await.expect("audio");
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "LrcLib Lyrics");
        assert_eq!(infos[0].id, provider_id("LrcLib Lyrics"));

        // `GetItemById<Audio>` also matches the `AudioBook` subclass…
        assert_eq!(
            mgr.get_supported_providers(audiobook_id)
                .await
                .expect("audiobook")
                .len(),
            1
        );
        // …but nothing else.
        assert!(
            mgr.get_supported_providers(movie_id)
                .await
                .expect("movie")
                .is_empty()
        );
    }

    // ---------------------------------------------------------------- parsers

    #[test]
    fn lrc_never_reports_metadata_and_always_carries_cues() {
        // Measured against Jellyfin 10.11.8: three metadata tags in, `{}` out,
        // and every LRC line serialises `"Cues": []`.
        let lrc = "[ar:Beatles]\n[ti:Hey Jude]\n[al:X]\n[00:12.34]First line\n[01:05.00]Second\n";
        let dto = parse_lrc(lrc).expect("timed lines");
        assert_eq!(dto.metadata, LyricMetadata::default());
        assert_eq!(dto.lyrics.len(), 2);
        assert_eq!(dto.lyrics[0].text, "First line");
        assert_eq!(dto.lyrics[0].start, Some(12_340 * 10_000));
        assert_eq!(dto.lyrics[0].cues.as_deref(), Some(&[][..]));
        assert_eq!(dto.lyrics[1].start, Some(65_000 * 10_000));
    }

    #[test]
    fn lrc_timestamp_grammar_matches_upstream() {
        // Only `mm:ss.ff` and `mm:ss.fff` are timestamps; everything else is
        // dropped as a non-timed line (measured on Jellyfin 10.11.8).
        let dto = parse_lrc(
            "[00:12.345]three\n[00:12.3]one\n[00:12]none\n[00:12.34]two\n[1:2]bare\n\
             [00:05.1234]four\n[-00:05.00]neg\n[aa:bb.cc]junk\n",
        )
        .expect("some timed lines");
        assert_eq!(
            dto.lyrics
                .iter()
                .map(|l| (l.text.as_str(), l.start))
                .collect::<Vec<_>>(),
            [("two", Some(123_400_000)), ("three", Some(123_450_000)),]
        );
        // Minutes may be 1..n digits and seconds may reach 60; both are upstream.
        let wide = parse_lrc("[100:05.00]big\n[00:60.00]sixty\n").expect("timed");
        assert_eq!(
            wide.lyrics
                .iter()
                .map(|l| (l.text.as_str(), l.start))
                .collect::<Vec<_>>(),
            [("sixty", Some(600_000_000)), ("big", Some(60_050_000_000))]
        );
    }

    #[test]
    fn lrc_keeps_repeated_timestamps_and_non_timestamp_brackets() {
        let dto =
            parse_lrc("[00:01.00][00:05.00]dup text\n[ti:T][00:08.00]tagthents\n").expect("timed");
        assert_eq!(
            dto.lyrics
                .iter()
                .map(|l| (l.text.as_str(), l.start))
                .collect::<Vec<_>>(),
            [
                ("dup text", Some(10_000_000)),
                ("dup text", Some(50_000_000)),
                ("[ti:T]tagthents", Some(80_000_000)),
            ]
        );
        // A plain line keeps its interior whitespace and is trimmed at the ends.
        let plain = parse_lrc("[00:10.00]   a    b\tc   \n").expect("timed");
        assert_eq!(plain.lyrics[0].text, "a    b\tc");
    }

    #[test]
    fn elrc_word_cues_match_the_measured_oracle() {
        // Oracle: Jellyfin 10.11.8's own response to this exact input.
        let dto =
            parse_lrc("[00:10.00]<00:10.00>Hello <00:10.50>world <00:11.00>again").expect("timed");
        let line = &dto.lyrics[0];
        assert_eq!(line.text, "Hello world again");
        assert_eq!(line.start, Some(100_000_000));
        let cues = line.cues.as_ref().expect("cues");
        assert_eq!(
            cues.iter()
                .map(|c| (c.position, c.end_position, c.start, c.end))
                .collect::<Vec<_>>(),
            [
                (0, 6, 100_000_000, Some(105_000_000)),
                (6, 12, 105_000_000, Some(110_000_000)),
                (12, 17, 110_000_000, None),
            ]
        );

        // The last cue of a non-final line ends at the NEXT line's start.
        let two = parse_lrc("[00:10.00]<00:10.00>aa <00:10.50>bb\n[00:20.00]<00:20.00>cc")
            .expect("timed");
        let first = two.lyrics[0].cues.as_ref().expect("cues");
        assert_eq!(first[1].end, Some(200_000_000));
        assert_eq!(two.lyrics[1].cues.as_ref().expect("cues")[0].end, None);
    }

    #[test]
    fn elrc_whitespace_and_index_states_match_the_measured_oracle() {
        // Each case is a Jellyfin 10.11.8 response captured verbatim.
        /// One measured oracle: the raw line, its rendered text, and the
        /// `(position, end_position)` of every cue Jellyfin emitted for it.
        type Case = (&'static str, &'static str, &'static [(i32, i32)]);
        let cases: [Case; 7] = [
            // whitespace touching a tag boundary collapses to one space…
            (
                "[00:10.00]<00:10.00>aa  <00:10.50>bb",
                "aa bb",
                &[(0, 3), (3, 5)],
            ),
            // …whitespace inside a segment does not.
            (
                "[00:05.00]<00:05.00>a    b  <00:06.00>  c    d",
                "a    b c    d",
                &[(0, 7), (7, 13)],
            ),
            // no whitespace at the boundary → no space is invented.
            (
                "[00:10.00]<00:10.00>aa<00:11.00>bb",
                "aabb",
                &[(0, 2), (2, 4)],
            ),
            // a whitespace-only segment leaves the tag at the END of the word
            // before it, and the next tag at the start of the word after.
            (
                "[00:10.00]<00:10.00>aa<00:10.50>  <00:11.00>bb",
                "aa bb",
                &[(0, 2), (3, 5)],
            ),
            // a tag with nothing after it sits just past the last word.
            ("[00:10.00]<00:10.00>abc<00:11.00>", "abc", &[(0, 3)]),
            // trailing whitespace before a final tag is trimmed away entirely.
            ("[00:05.00]<00:05.00>a <00:06.00>", "a", &[(0, 1)]),
            // positions are UTF-16 code units, as in the C#.
            (
                "[00:10.00]<00:10.00>日本 <00:11.00>語",
                "日本 語",
                &[(0, 3), (3, 4)],
            ),
        ];
        for (input, text, spans) in cases {
            let dto = parse_lrc(input).expect("timed");
            assert_eq!(dto.lyrics[0].text, text, "text for {input:?}");
            assert_eq!(
                dto.lyrics[0]
                    .cues
                    .as_ref()
                    .expect("cues")
                    .iter()
                    .map(|c| (c.position, c.end_position))
                    .collect::<Vec<_>>(),
                spans,
                "cue spans for {input:?}"
            );
        }
        // An all-whitespace line yields empty text and no cues at all.
        let blank = parse_lrc("[00:10.00]  <00:10.50>   ").expect("timed");
        assert_eq!(blank.lyrics[0].text, "");
        assert_eq!(blank.lyrics[0].cues.as_deref(), Some(&[][..]));
        // An invalid word tag stays in the text verbatim.
        let kept = parse_lrc("[00:05.00]<0:05.00>a<00:6.00>b<00:07.000>c<00:08>d").expect("timed");
        assert_eq!(kept.lyrics[0].text, "a<00:6.00>bc<00:08>d");
        assert_eq!(
            kept.lyrics[0].cues.as_ref().expect("cues")[0].end_position,
            11
        );
    }

    /// A measured oracle line: its text, its start in ticks, and one
    /// `(position, end_position, start, end)` per cue.
    type Line = (&'static str, i64, &'static [(i32, i32, i64, Option<i64>)]);

    /// Parses each document and asserts the whole line/cue shape against the
    /// captured Jellyfin response.
    fn assert_lrc(cases: &[(&str, &[Line])]) {
        for (input, expected) in cases {
            let dto = parse_lrc(input).expect("timed");
            let got: Vec<_> = dto
                .lyrics
                .iter()
                .map(|l| {
                    (
                        l.text.clone(),
                        l.start.expect("start"),
                        l.cues
                            .as_ref()
                            .expect("cues")
                            .iter()
                            .map(|c| (c.position, c.end_position, c.start, c.end))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();
            let want: Vec<_> = expected
                .iter()
                .map(|(t, s, c)| ((*t).to_owned(), *s, c.to_vec()))
                .collect();
            assert_eq!(got, want, "for {input:?}");
        }
    }

    /// The class of enhanced-LRC line that a corpus of "every line starts with
    /// a word tag" cannot expose: `LrcTimedTextUtils.TimedTextToObject` seeds
    /// `lastTimeTag` with the LINE's start time, so text sitting before the
    /// first `<mm:ss.xx>` tag gets a cue of its own at position 0. Ferrofin
    /// used to drop it. Every expectation here is Jellyfin 10.11.8's own
    /// response to that exact input, captured from the lab pair.
    #[test]
    fn leading_text_before_the_first_word_tag_keeps_its_cue() {
        let cases: [(&str, &[Line]); 4] = [
            // text, then a word tag: TWO cues, the first carrying the line start.
            (
                "[00:01.00]hello <00:02.00>there\n[00:03.00]end\n",
                &[
                    (
                        "hello there",
                        10_000_000,
                        &[
                            (0, 6, 10_000_000, Some(20_000_000)),
                            (6, 11, 20_000_000, Some(30_000_000)),
                        ],
                    ),
                    ("end", 30_000_000, &[]),
                ],
            ),
            // the split can fall mid-word.
            (
                "[00:01.00]he<00:01.50>llo\n[00:03.00]end\n",
                &[
                    (
                        "hello",
                        10_000_000,
                        &[
                            (0, 2, 10_000_000, Some(15_000_000)),
                            (2, 5, 15_000_000, Some(30_000_000)),
                        ],
                    ),
                    ("end", 30_000_000, &[]),
                ],
            ),
            // a word tag AT index 0 wins over the line start (`TryAdd` keeps the
            // first value written at a key, and the blank leading segment never
            // writes one).
            (
                "[00:01.00]<00:05.00>hello <00:06.00>there\n[00:09.00]end\n",
                &[
                    (
                        "hello there",
                        10_000_000,
                        &[
                            (0, 6, 50_000_000, Some(60_000_000)),
                            (6, 11, 60_000_000, Some(90_000_000)),
                        ],
                    ),
                    ("end", 90_000_000, &[]),
                ],
            ),
            // leading WHITESPACE is not leading text: still one cue.
            (
                "[00:01.00]   <00:02.00>there\n[00:09.00]end\n",
                &[
                    ("there", 10_000_000, &[(0, 5, 20_000_000, Some(90_000_000))]),
                    ("end", 90_000_000, &[]),
                ],
            ),
        ];
        assert_lrc(&cases);
    }

    /// The rest of the measured `TimedTextToObject` oracle: what the LINE start
    /// tag does when the line's last segment is empty or blank (it becomes an
    /// `IndexState.End` index, whose own trailing slice emits nothing), that
    /// tags sort by index and not by time, and that a line with no word tags
    /// gets no cue at all despite having a start. Jellyfin 10.11.8's own
    /// responses, captured from the lab pair.
    #[test]
    fn line_start_tags_close_and_sort_the_way_upstream_does() {
        let cases: [(&str, &[Line]); 4] = [
            // a tag with nothing after it closes the leading run as an End
            // index, so the ONE cue spans the whole line.
            (
                "[00:01.00]hello<00:02.00>\n[00:09.00]end\n",
                &[
                    ("hello", 10_000_000, &[(0, 5, 10_000_000, Some(20_000_000))]),
                    ("end", 90_000_000, &[]),
                ],
            ),
            // …and the same when the trailing segment is whitespace only.
            (
                "[00:01.00]hello <00:02.00>   <00:03.00>   \n[00:09.00]end\n",
                &[
                    ("hello", 10_000_000, &[(0, 5, 10_000_000, Some(20_000_000))]),
                    ("end", 90_000_000, &[]),
                ],
            ),
            // keys sort by INDEX, not by time: a word tag earlier than the line
            // start produces a cue whose end precedes its start. Jellyfin does
            // this; so do we.
            (
                "[00:05.00]hello <00:02.00>there\n[00:09.00]end\n",
                &[
                    (
                        "hello there",
                        50_000_000,
                        &[
                            (0, 6, 50_000_000, Some(20_000_000)),
                            (6, 11, 20_000_000, Some(90_000_000)),
                        ],
                    ),
                    ("end", 90_000_000, &[]),
                ],
            ),
            // no word tags at all ⇒ no cues, even though the line has a start.
            (
                "[00:01.00]just text\n[00:09.00]end\n",
                &[("just text", 10_000_000, &[]), ("end", 90_000_000, &[])],
            ),
        ];
        assert_lrc(&cases);
    }

    /// `LrcLyricParser.Decode` refuses to attribute word time tags when the
    /// line does not have exactly ONE start time — the library returns the line
    /// as-is, word tags and all, because `<00:10.00>` cannot belong to both
    /// `[00:01.00]` and `[00:05.00]`. Measured against Jellyfin 10.11.8, which
    /// answers `Text = "hello <00:10.00>there"` with no cues.
    #[test]
    fn a_line_with_two_start_times_keeps_its_word_tags_verbatim() {
        let dto =
            parse_lrc("[00:01.00][00:05.00]hello <00:10.00>there\n[00:20.00]end\n").expect("timed");
        let lines: Vec<_> = dto
            .lyrics
            .iter()
            .map(|l| (l.text.as_str(), l.start, l.cues.as_deref().map(<[_]>::len)))
            .collect();
        assert_eq!(
            lines,
            [
                ("hello <00:10.00>there", Some(10_000_000), Some(0)),
                ("hello <00:10.00>there", Some(50_000_000), Some(0)),
                ("end", Some(200_000_000), Some(0)),
            ]
        );

        // Line time tags are collected wherever they occur, not just at the
        // start — `SplitLyricAndTimeTag` runs the regex over the whole line —
        // so a second, mid-line `[..]` disables word parsing the same way.
        let mid = parse_lrc("[00:01.00]hello [00:05.00]world <00:10.00>x\n").expect("timed");
        assert_eq!(mid.lyrics[0].text, "hello world <00:10.00>x");
        assert_eq!(mid.lyrics.len(), 2);

        // With exactly one such tag the word tags ARE parsed, and the text is
        // the tag-stripped remainder.
        let single = parse_lrc("hello [00:01.00]world <00:10.00>x\n").expect("timed");
        assert_eq!(single.lyrics[0].text, "hello world x");
        assert_eq!(
            single.lyrics[0].cues.as_ref().expect("cues")[0],
            LyricLineCue {
                position: 0,
                end_position: 12,
                start: 10_000_000,
                end: Some(100_000_000),
            }
        );
    }

    /// Transliteration of upstream
    /// `Jellyfin.Providers.Tests/Lyrics/LrcLyricParserTests.ParseElrcCues`, over
    /// the same `Fleetwood Mac - Rumors.elrc` fixture — the C# asserts are the
    /// oracle, reproduced value for value.
    #[test]
    fn parse_elrc_cues() {
        let parsed = parse_lrc(RUMORS_ELRC).expect("parsed");
        assert_eq!(parsed.lyrics.len(), 31);

        let line1 = &parsed.lyrics[0];
        assert_eq!(line1.text, "Every night that goes between");
        let cues1 = line1.cues.as_ref().expect("cues");
        assert_eq!(cues1.len(), 5);
        assert_eq!(cues1[0].start, 68_400_000);
        assert_eq!(cues1[0].end, Some(72_000_000));
        assert_eq!(cues1[0].position, 0);
        assert_eq!(cues1[0].end_position, 5);
        assert_eq!(cues1[1].position, 6);
        assert_eq!(cues1[1].end_position, 11);
        assert_eq!(cues1[2].position, 12);

        let line5 = &parsed.lyrics[4];
        assert_eq!(line5.text, "Every night you do not come");
        let cues5 = line5.cues.as_ref().expect("cues");
        assert_eq!(cues5.len(), 6);
        assert_eq!(cues5[2].start, 375_200_000);
        assert_eq!(cues5[2].end, Some(377_300_000));

        let last = parsed.lyrics.last().expect("last line");
        assert_eq!(last.text, "I have always been a storm");
        let last_cues = last.cues.as_ref().expect("cues");
        assert_eq!(last_cues.len(), 6);
        assert_eq!(last_cues[last_cues.len() - 1].start, 2_358_000_000);
        assert_eq!(last_cues[last_cues.len() - 1].end_position, 26);
        assert_eq!(last_cues[last_cues.len() - 1].end, None);
    }

    #[test]
    fn txt_keeps_every_line_including_the_trailing_empty_one() {
        // Oracle: Jellyfin 10.11.8's response to this exact upload.
        let dto = parse_txt("Plain line one\n\n   indented line\nlast\n");
        assert_eq!(dto.metadata, LyricMetadata::default());
        assert_eq!(
            dto.lyrics
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            ["Plain line one", "", "indented line", "last", ""]
        );
        assert!(
            dto.lyrics
                .iter()
                .all(|l| l.start.is_none() && l.cues.is_none())
        );
        // `\r\n` and a bare `\r` are line breaks too.
        assert_eq!(split_lines("line1\r\nline2\r\n").len(), 3);
        assert_eq!(split_lines("a\rb"), ["a", "b"]);
    }

    #[test]
    fn untimed_lrc_falls_through_to_the_text_parser() {
        // `LrcLyricParser` returns null on zero timed lines, so `TxtLyricParser`
        // — which also accepts `.lrc`/`.elrc` — answers instead.
        assert!(parse_lrc("no timestamps here\nsecond untimed\n").is_none());
        let dto = parse_by_name("x.lrc", "no timestamps here\nsecond untimed\n").expect("parsed");
        assert_eq!(
            dto.lyrics
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            ["no timestamps here", "second untimed", ""]
        );
        // A metadata-only `.lrc` is untimed too.
        let meta_only = parse_by_name("x.lrc", "[ar:Artist]\n[ti:Title]\n").expect("parsed");
        assert_eq!(
            meta_only
                .lyrics
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            ["[ar:Artist]", "[ti:Title]", ""]
        );
        // `.txt` never runs the LRC parser…
        let txt = parse_by_name("x.txt", "[00:05.00]still text").expect("parsed");
        assert_eq!(txt.lyrics[0].text, "[00:05.00]still text");
        assert!(txt.lyrics[0].start.is_none());
        // …and an extension no parser claims yields nothing at all (→ 400).
        assert!(parse_by_name("x.foo", "hello").is_none());
        assert!(parse_by_name("noextension", "hello").is_none());
    }

    #[tokio::test]
    async fn no_item_store_reads_empty() {
        let mgr = FerrofinLyricManager::new();
        assert!(mgr.get_lyrics(Uuid::new_v4()).await.expect("get").is_none());
        assert!(
            mgr.search_lyrics(Uuid::new_v4())
                .await
                .expect("search")
                .is_empty()
        );
        // A pathless item cannot be written to or deleted from.
        assert!(mgr.save_lyric(Uuid::new_v4(), "lrc", "x").await.is_err());
        mgr.delete_lyrics(Uuid::new_v4()).await.expect("no-op");
    }
}
