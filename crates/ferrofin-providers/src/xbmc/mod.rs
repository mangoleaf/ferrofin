//! XbmcMetadata NFO parsing — port of `MediaBrowser.XbmcMetadata.Parsers`.
//!
//! Reads Kodi/XBMC `.nfo` sidecar files into an [`item::NfoBaseItem`] wrapped in
//! a [`crate::container_types::MetadataResult`]. The parser core lives in
//! [`base_parser`]; the per-kind subclasses in [`parsers`]; the value/date/id
//! helpers in [`xml_ext`]; and the `XmlReader`-shaped cursor in [`xml_reader`].
//!
//! File I/O is kept out of the parse: callers pass the document *contents* to
//! [`fetch_movie`] / [`fetch_episode`] / … so tests read fixtures directly and
//! the un-mockable filesystem access does not enter the parity numbers. The one
//! filesystem dependency the parsers have (local-artwork resolution) is behind
//! the [`base_parser::DirectoryService`] trait.

pub mod base_parser;
pub mod config;
pub mod item;
pub mod parsers;
pub mod saver;
pub mod xml_ext;
pub mod xml_reader;

use crate::container_types::MetadataResult;

use base_parser::{BaseNfoParser, DirectoryService, ExternalIdSource, FetchError, FetchResult};
use config::NfoConfiguration;
use item::{NfoBaseItem, NfoItemKind};
use parsers::{
    EpisodeNfoParser, MovieNfoParser, SeasonNfoParser, SeriesNfoParser, SeriesNfoSeasonParser,
};

/// Validates the `Fetch` preconditions shared by every parser.
///
/// Port of the two `ArgumentException` guards: a null item (here the caller must
/// have provided one, so we only check the metadata path is non-empty).
fn check_fetch(metadata_file: &str) -> FetchResult {
    if metadata_file.is_empty() {
        return Err(FetchError::EmptyMetadataFile);
    }
    Ok(())
}

/// Parses a movie NFO document into `result` (`MovieNfoParser.Fetch`).
///
/// `metadata_file` is the source path (only its emptiness is validated, per C#);
/// `xml` is its contents.
///
/// # Errors
/// Returns [`FetchError::EmptyMetadataFile`] if `metadata_file` is empty.
pub fn fetch_movie(
    result: &mut MetadataResult<NfoBaseItem>,
    metadata_file: &str,
    xml: &str,
    config: &NfoConfiguration,
    external_ids: &dyn ExternalIdSource,
    directory_service: &dyn DirectoryService,
) -> FetchResult {
    check_fetch(metadata_file)?;
    let base = BaseNfoParser::new(config, external_ids, directory_service);
    base.fetch(&MovieNfoParser, result, xml);
    Ok(())
}

/// Parses an `album.nfo` or `artist.nfo` document into `result`.
///
/// C# routes both through `BaseNfoParser<T>` with no per-kind extensions
/// (`AlbumNfoProvider`/`ArtistNfoProvider` construct the base parser directly),
/// so this is the same read with no custom element switch.
///
/// # Errors
/// Returns [`FetchError::EmptyMetadataFile`] if `metadata_file` is empty.
pub fn fetch_music(
    result: &mut MetadataResult<NfoBaseItem>,
    metadata_file: &str,
    xml: &str,
    config: &NfoConfiguration,
    external_ids: &dyn ExternalIdSource,
    directory_service: &dyn DirectoryService,
) -> FetchResult {
    check_fetch(metadata_file)?;
    let base = BaseNfoParser::new(config, external_ids, directory_service);
    base.fetch(&crate::xbmc::parsers::PlainNfoParser, result, xml);
    Ok(())
}

/// Parses a series NFO document into `result` (`SeriesNfoParser.Fetch`).
///
/// # Errors
/// Returns [`FetchError::EmptyMetadataFile`] if `metadata_file` is empty.
pub fn fetch_series(
    result: &mut MetadataResult<NfoBaseItem>,
    metadata_file: &str,
    xml: &str,
    config: &NfoConfiguration,
    external_ids: &dyn ExternalIdSource,
    directory_service: &dyn DirectoryService,
) -> FetchResult {
    check_fetch(metadata_file)?;
    let base = BaseNfoParser::new(config, external_ids, directory_service);
    base.fetch(&SeriesNfoParser, result, xml);
    Ok(())
}

/// Parses a season NFO document into `result` (`SeasonNfoParser.Fetch`).
///
/// # Errors
/// Returns [`FetchError::EmptyMetadataFile`] if `metadata_file` is empty.
pub fn fetch_season(
    result: &mut MetadataResult<NfoBaseItem>,
    metadata_file: &str,
    xml: &str,
    config: &NfoConfiguration,
    external_ids: &dyn ExternalIdSource,
    directory_service: &dyn DirectoryService,
) -> FetchResult {
    check_fetch(metadata_file)?;
    let base = BaseNfoParser::new(config, external_ids, directory_service);
    base.fetch(&SeasonNfoParser, result, xml);
    Ok(())
}

/// Parses a series `<namedseason>` NFO into a season (`SeriesNfoSeasonParser.Fetch`).
///
/// # Errors
/// Returns [`FetchError::EmptyMetadataFile`] if `metadata_file` is empty.
pub fn fetch_series_season(
    result: &mut MetadataResult<NfoBaseItem>,
    metadata_file: &str,
    xml: &str,
    config: &NfoConfiguration,
    external_ids: &dyn ExternalIdSource,
    directory_service: &dyn DirectoryService,
) -> FetchResult {
    check_fetch(metadata_file)?;
    let base = BaseNfoParser::new(config, external_ids, directory_service);
    base.fetch(&SeriesNfoSeasonParser, result, xml);
    Ok(())
}

/// Parses an episode NFO document into `result` (`EpisodeNfoParser.Fetch`).
///
/// Uses the episode-specific multi-block merge rather than the base fetch.
///
/// # Errors
/// Returns [`FetchError::EmptyMetadataFile`] if `metadata_file` is empty.
pub fn fetch_episode(
    result: &mut MetadataResult<NfoBaseItem>,
    metadata_file: &str,
    xml: &str,
    config: &NfoConfiguration,
    external_ids: &dyn ExternalIdSource,
    directory_service: &dyn DirectoryService,
) -> FetchResult {
    check_fetch(metadata_file)?;
    let base = BaseNfoParser::new(config, external_ids, directory_service);
    EpisodeNfoParser.fetch(&base, result, xml);
    Ok(())
}

/// Creates an empty [`MetadataResult`] wrapping a fresh item of `kind`.
///
/// Convenience mirroring `new MetadataResult<T> { Item = new T() }`.
#[must_use]
pub fn new_result(kind: NfoItemKind) -> MetadataResult<NfoBaseItem> {
    MetadataResult::new(NfoBaseItem::new(kind))
}

/// A static [`ExternalIdSource`] backed by a fixed key list.
///
/// Mirrors the mocked `IProviderManager.GetExternalIdInfos` in the C# tests,
/// which returns a fixed set of [`ferrofin_model::providers::ExternalIdInfo`]s.
#[derive(Debug, Default, Clone)]
pub struct StaticExternalIds {
    keys: Vec<String>,
}

impl StaticExternalIds {
    /// Creates a source yielding exactly `keys` (e.g. `["Imdb"]`, `["Tmdb"]`).
    #[must_use]
    pub fn new(keys: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            keys: keys.into_iter().map(Into::into).collect(),
        }
    }
}

impl ExternalIdSource for StaticExternalIds {
    fn external_id_keys(&self) -> Vec<String> {
        self.keys.clone()
    }
}
