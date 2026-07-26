//! [`HermitLyricManager`] — local lyric reading (`.lrc`/`.elrc`/`.txt`).
//!
//! Port of the local slice of `MediaBrowser.Providers.Lyric.LyricManager` +
//! `LrcLyricParser`/`TxtLyricParser`: an item's lyrics come from a sidecar file
//! next to its media (`song.flac` → `song.lrc`). Synced `.lrc`/`.elrc` files
//! yield timestamped [`LyricLine`]s (with metadata tags); plain `.txt` yields
//! unsynced lines. Uploads write a sidecar; deletes remove it.
//!
//! Remote lyric providers (LrcLib, etc.) are network + feature-gated, so
//! [`search_lyrics`](LyricManager::search_lyrics) stays empty and
//! [`download_lyrics`](LyricManager::download_lyrics) is rejected — faithful to a
//! server with no remote lyric plugin installed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use hermit_model::lyrics::{LyricDto, LyricLine, LyricMetadata, RemoteLyricInfoDto};
use hermit_model::providers::LyricProviderInfo;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::persistence::ItemRepository;
use hermit_traits::stubs::LyricManager;

/// The number of ffmpeg ticks (100 ns) per millisecond.
const TICKS_PER_MS: i64 = 10_000;

/// Sidecar extensions probed for an item's lyrics, in priority order: synced LRC
/// first, then plain text. Port of `LrcLyricParser`/`TxtLyricParser`
/// `SupportedMediaTypes`.
const LYRIC_EXTENSIONS: [&str; 3] = ["lrc", "elrc", "txt"];

/// The local lyric manager: reads/writes `.lrc`/`.elrc`/`.txt` sidecars next to
/// an item's media file.
#[derive(Clone, Default)]
pub struct HermitLyricManager {
    /// Resolves an item id to its media path (to locate the sidecar). Absent →
    /// the manager behaves as an empty stub (unit tests).
    items: Option<Arc<dyn ItemRepository>>,
}

impl std::fmt::Debug for HermitLyricManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitLyricManager")
            .field("has_item_store", &self.items.is_some())
            .finish()
    }
}

impl HermitLyricManager {
    /// Creates a lyric manager with no item store (empty stub).
    #[must_use]
    pub fn new() -> Self {
        Self { items: None }
    }

    /// Attaches the item store used to resolve an item's media path, enabling
    /// local sidecar reading/writing.
    #[must_use]
    pub fn with_items(mut self, items: Arc<dyn ItemRepository>) -> Self {
        self.items = Some(items);
        self
    }

    /// The media file path for `item_id`, or `None` when unknown / pathless.
    async fn item_path(&self, item_id: Uuid) -> Result<Option<PathBuf>, ServiceError> {
        let Some(items) = &self.items else {
            return Ok(None);
        };
        Ok(items
            .retrieve_item(item_id)
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

    async fn search_lyrics(&self, _item_id: Uuid) -> Result<Vec<RemoteLyricInfoDto>, ServiceError> {
        // No remote lyric providers are configured.
        Ok(Vec::new())
    }

    async fn download_lyrics(
        &self,
        _item_id: Uuid,
        _lyric_id: &str,
    ) -> Result<Option<LyricDto>, ServiceError> {
        Err(Self::no_remote())
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
        _item_id: Uuid,
    ) -> Result<Vec<LyricProviderInfo>, ServiceError> {
        // No remote providers; local sidecar reading needs none advertised.
        Ok(Vec::new())
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
    use super::{HermitLyricManager, parse_lrc, parse_txt};
    use hermit_traits::stubs::LyricManager;
    use uuid::Uuid;

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
