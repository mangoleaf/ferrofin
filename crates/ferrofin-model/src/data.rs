//! Cross-cutting item enums — port of the out-of-tree `Jellyfin.Data.Enums`.
//!
//! These live in a separate C# assembly upstream but are pulled into the model
//! crate here because the DTOs reference them. Video-range, media-type,
//! item-kind, collection-type, unrated-item and person-kind taxonomies.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// An enum representing video ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum VideoRange {
    /// Unknown video range.
    Unknown,
    /// SDR video range.
    #[serde(rename = "SDR")]
    Sdr,
    /// HDR video range.
    #[serde(rename = "HDR")]
    Hdr,
}

/// An enum representing types of video ranges.
///
/// This is a `[Flags]`-adjacent taxonomy in name only upstream; on the wire it
/// is a plain string enum (see the OpenAPI contract), so it is modeled as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum VideoRangeType {
    /// Unknown video range type.
    Unknown,
    /// SDR video range type (8-bit).
    #[serde(rename = "SDR")]
    Sdr,
    /// HDR10 video range type (10-bit).
    #[serde(rename = "HDR10")]
    Hdr10,
    /// HLG video range type (10-bit).
    #[serde(rename = "HLG")]
    Hlg,
    /// Dolby Vision video range type (10-bit encoded / 12-bit remapped).
    #[serde(rename = "DOVI")]
    Dovi,
    /// Dolby Vision with HDR10 fallback (10-bit).
    #[serde(rename = "DOVIWithHDR10")]
    DoviWithHdr10,
    /// Dolby Vision with HLG fallback (10-bit).
    #[serde(rename = "DOVIWithHLG")]
    DoviWithHlg,
    /// Dolby Vision with SDR fallback (8-bit / 10-bit).
    #[serde(rename = "DOVIWithSDR")]
    DoviWithSdr,
    /// Dolby Vision with Enhancement Layer (Profile 7).
    #[serde(rename = "DOVIWithEL")]
    DoviWithEl,
    /// Dolby Vision and HDR10+ metadata coexist.
    #[serde(rename = "DOVIWithHDR10Plus")]
    DoviWithHdr10Plus,
    /// Dolby Vision with Enhancement Layer (Profile 7) and HDR10+ metadata coexist.
    #[serde(rename = "DOVIWithELHDR10Plus")]
    DoviWithElhdr10Plus,
    /// Dolby Vision with invalid configuration (e.g. Profile 8 compat id 6).
    #[serde(rename = "DOVIInvalid")]
    DoviInvalid,
    /// HDR10+ video range type (10-bit to 16-bit).
    #[serde(rename = "HDR10Plus")]
    Hdr10Plus,
}

/// Media types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum MediaType {
    /// Unknown media type.
    #[default]
    Unknown = 0,
    /// Video media.
    Video = 1,
    /// Audio media.
    Audio = 2,
    /// Photo media.
    Photo = 3,
    /// Book media.
    Book = 4,
}

/// Collection type.
///
/// Members are lowercase for backwards compatibility with the wire contract.
/// The server-internal virtual-folder variants (`tvshowseries`, `moviegenre`,
/// …, discriminants 101–115, marked `[OpenApiIgnoreEnum]` upstream) are not on
/// the wire and are intentionally omitted to keep the generated schema honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
#[allow(non_camel_case_types)]
pub enum CollectionType {
    /// Unknown collection.
    unknown = 0,
    /// Movies collection.
    movies = 1,
    /// TV shows collection.
    tvshows = 2,
    /// Music collection.
    music = 3,
    /// Music videos collection.
    musicvideos = 4,
    /// Trailers collection.
    trailers = 5,
    /// Home videos collection.
    homevideos = 6,
    /// Box sets collection.
    boxsets = 7,
    /// Books collection.
    books = 8,
    /// Photos collection.
    photos = 9,
    /// Live TV collection.
    livetv = 10,
    /// Playlists collection.
    playlists = 11,
    /// Folders collection.
    folders = 12,
}

/// An enum representing an unrated item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum UnratedItem {
    /// A movie.
    Movie = 0,
    /// A trailer.
    Trailer = 1,
    /// A series.
    Series = 2,
    /// Music.
    Music = 3,
    /// A book.
    Book = 4,
    /// A live TV channel.
    LiveTvChannel = 5,
    /// A live TV program.
    LiveTvProgram = 6,
    /// Channel content.
    ChannelContent = 7,
    /// Another type, not covered by the other fields.
    Other = 8,
}

/// The person kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum PersonKind {
    /// An unknown person kind.
    Unknown,
    /// A person whose profession is acting.
    Actor,
    /// A person who supervises the actors and other staff.
    Director,
    /// A person who writes music.
    Composer,
    /// A writer of a book, article, or document (or generic music writer).
    Writer,
    /// A well-known performer appearing without a regular role.
    GuestStar,
    /// A person responsible for financial and managerial aspects of a production.
    Producer,
    /// A person who directs an orchestra or choir.
    Conductor,
    /// A person who writes the words to a song or musical.
    Lyricist,
    /// A person who adapts a musical composition for performance.
    Arranger,
    /// An audio engineer who performed a general engineering role.
    Engineer,
    /// An engineer who mixed a recorded track into a single piece of music.
    Mixer,
    /// A person who remixed a recording from other tracks.
    Remixer,
    /// A person who created the material.
    Creator,
    /// A person who was the artist.
    Artist,
    /// A person who was the album artist.
    AlbumArtist,
    /// A person who was the author.
    Author,
    /// A person who was the illustrator.
    Illustrator,
    /// A person responsible for drawing the art.
    Penciller,
    /// A person responsible for inking the pencil art.
    Inker,
    /// A person responsible for applying color to drawings.
    Colorist,
    /// A person responsible for drawing text and speech bubbles.
    Letterer,
    /// A person responsible for drawing the cover art.
    CoverArtist,
    /// A person contributing by revising or elucidating the content.
    Editor,
    /// A person who renders a text from one language into another.
    Translator,
    /// A person who narrates a book or other work.
    Narrator,
}

/// The base item kind (generated upstream from all `BaseItem` subclasses).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum BaseItemKind {
    /// Item is an aggregate folder.
    #[default]
    AggregateFolder,
    /// Item is audio.
    Audio,
    /// Item is an audio book.
    AudioBook,
    /// Item is a base plugin folder.
    BasePluginFolder,
    /// Item is a book.
    Book,
    /// Item is a box set.
    BoxSet,
    /// Item is a channel.
    Channel,
    /// Item is a channel folder item.
    ChannelFolderItem,
    /// Item is a collection folder.
    CollectionFolder,
    /// Item is an episode.
    Episode,
    /// Item is a folder.
    Folder,
    /// Item is a genre.
    Genre,
    /// Item is a manual playlists folder.
    ManualPlaylistsFolder,
    /// Item is a movie.
    Movie,
    /// Item is a live TV channel.
    LiveTvChannel,
    /// Item is a live TV program.
    LiveTvProgram,
    /// Item is a music album.
    MusicAlbum,
    /// Item is a music artist.
    MusicArtist,
    /// Item is a music genre.
    MusicGenre,
    /// Item is a music video.
    MusicVideo,
    /// Item is a person.
    Person,
    /// Item is a photo.
    Photo,
    /// Item is a photo album.
    PhotoAlbum,
    /// Item is a playlist.
    Playlist,
    /// Item is a playlists folder.
    PlaylistsFolder,
    /// Item is a program.
    Program,
    /// Item is a recording (manually added upstream).
    Recording,
    /// Item is a season.
    Season,
    /// Item is a series.
    Series,
    /// Item is a studio.
    Studio,
    /// Item is a trailer.
    Trailer,
    /// Item is a live TV channel (type overridden upstream).
    TvChannel,
    /// Item is a live TV program (type overridden upstream).
    TvProgram,
    /// Item is a user root folder.
    UserRootFolder,
    /// Item is a user view.
    UserView,
    /// Item is a video.
    Video,
    /// Item is a year.
    Year,
}

/// Media streaming protocol.
///
/// Members are lowercase for backwards compatibility with the wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
#[allow(non_camel_case_types)]
pub enum MediaStreamProtocol {
    /// HTTP.
    #[default]
    http = 0,
    /// HTTP Live Streaming.
    hls = 1,
}
