//! Cross-cutting item enums — port of the out-of-tree `Jellyfin.Data.Enums`.
//!
//! These live in a separate C# assembly upstream but are pulled into the model
//! crate here because the DTOs reference them. Video-range, media-type,
//! item-kind, collection-type, unrated-item and person-kind taxonomies.

use serde::Serialize;
use utoipa::ToSchema;

/// An enum representing video ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[repr(i32)]
pub enum VideoRange {
    /// Unknown video range.
    Unknown,
    /// SDR video range.
    #[serde(rename = "SDR")]
    Sdr,
    /// HDR video range.
    #[serde(rename = "HDR")]
    Hdr,
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    VideoRange, None, {
        Unknown => ("Unknown", 0),
        Sdr => ("SDR", 1),
        Hdr => ("HDR", 2),
    }
}

/// An enum representing types of video ranges.
///
/// This is a `[Flags]`-adjacent taxonomy in name only upstream; on the wire it
/// is a plain string enum (see the OpenAPI contract), so it is modeled as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[repr(i32)]
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
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    VideoRangeType, None, {
        Unknown => ("Unknown", 0),
        Sdr => ("SDR", 1),
        Hdr10 => ("HDR10", 2),
        Hlg => ("HLG", 3),
        Dovi => ("DOVI", 4),
        DoviWithHdr10 => ("DOVIWithHDR10", 5),
        DoviWithHlg => ("DOVIWithHLG", 6),
        DoviWithSdr => ("DOVIWithSDR", 7),
        DoviWithEl => ("DOVIWithEL", 8),
        DoviWithHdr10Plus => ("DOVIWithHDR10Plus", 9),
        DoviWithElhdr10Plus => ("DOVIWithELHDR10Plus", 10),
        DoviInvalid => ("DOVIInvalid", 11),
        Hdr10Plus => ("HDR10Plus", 12),
    }
}

/// Media types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[repr(i32)]
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
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    MediaType, None, {
        Unknown => ("Unknown", 0),
        Video => ("Video", 1),
        Audio => ("Audio", 2),
        Photo => ("Photo", 3),
        Book => ("Book", 4),
    }
}

/// Collection type.
///
/// Members are lowercase for backwards compatibility with the wire contract.
/// The server-internal virtual-folder variants (`tvshowseries`, `moviegenre`,
/// …, discriminants 101–115, marked `[OpenApiIgnoreEnum]` upstream) are not on
/// the wire and are intentionally omitted to keep the generated schema honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
#[allow(non_camel_case_types)]
#[repr(i32)]
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
    /// Upstream `tvshowseries` value.
    #[serde(rename = "tvshowseries")]
    Tvshowseries,

    /// Upstream `tvgenres` value.
    #[serde(rename = "tvgenres")]
    Tvgenres,

    /// Upstream `tvgenre` value.
    #[serde(rename = "tvgenre")]
    Tvgenre,

    /// Upstream `tvlatest` value.
    #[serde(rename = "tvlatest")]
    Tvlatest,

    /// Upstream `tvnextup` value.
    #[serde(rename = "tvnextup")]
    Tvnextup,

    /// Upstream `tvresume` value.
    #[serde(rename = "tvresume")]
    Tvresume,

    /// Upstream `tvfavoriteseries` value.
    #[serde(rename = "tvfavoriteseries")]
    Tvfavoriteseries,

    /// Upstream `tvfavoriteepisodes` value.
    #[serde(rename = "tvfavoriteepisodes")]
    Tvfavoriteepisodes,

    /// Upstream `movielatest` value.
    #[serde(rename = "movielatest")]
    Movielatest,

    /// Upstream `movieresume` value.
    #[serde(rename = "movieresume")]
    Movieresume,

    /// Upstream `moviemovies` value.
    #[serde(rename = "moviemovies")]
    Moviemovies,

    /// Upstream `moviecollection` value.
    #[serde(rename = "moviecollection")]
    Moviecollection,

    /// Upstream `moviefavorites` value.
    #[serde(rename = "moviefavorites")]
    Moviefavorites,

    /// Upstream `moviegenres` value.
    #[serde(rename = "moviegenres")]
    Moviegenres,

    /// Upstream `moviegenre` value.
    #[serde(rename = "moviegenre")]
    Moviegenre,

    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    CollectionType, None, {
        unknown => ("unknown", 0),
        movies => ("movies", 1),
        tvshows => ("tvshows", 2),
        music => ("music", 3),
        musicvideos => ("musicvideos", 4),
        trailers => ("trailers", 5),
        homevideos => ("homevideos", 6),
        boxsets => ("boxsets", 7),
        books => ("books", 8),
        photos => ("photos", 9),
        livetv => ("livetv", 10),
        playlists => ("playlists", 11),
        folders => ("folders", 12),
        Tvshowseries => ("tvshowseries", 101),
        Tvgenres => ("tvgenres", 102),
        Tvgenre => ("tvgenre", 103),
        Tvlatest => ("tvlatest", 104),
        Tvnextup => ("tvnextup", 105),
        Tvresume => ("tvresume", 106),
        Tvfavoriteseries => ("tvfavoriteseries", 107),
        Tvfavoriteepisodes => ("tvfavoriteepisodes", 108),
        Movielatest => ("movielatest", 109),
        Movieresume => ("movieresume", 110),
        Moviemovies => ("moviemovies", 111),
        Moviecollection => ("moviecollection", 112),
        Moviefavorites => ("moviefavorites", 113),
        Moviegenres => ("moviegenres", 114),
        Moviegenre => ("moviegenre", 115),
    }
}

/// An enum representing an unrated item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[repr(i32)]
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
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    UnratedItem, None, {
        Movie => ("Movie", 0),
        Trailer => ("Trailer", 1),
        Series => ("Series", 2),
        Music => ("Music", 3),
        Book => ("Book", 4),
        LiveTvChannel => ("LiveTvChannel", 5),
        LiveTvProgram => ("LiveTvProgram", 6),
        ChannelContent => ("ChannelContent", 7),
        Other => ("Other", 8),
    }
}

/// The person kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[repr(i32)]
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
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    PersonKind, None, {
        Unknown => ("Unknown", 0),
        Actor => ("Actor", 1),
        Director => ("Director", 2),
        Composer => ("Composer", 3),
        Writer => ("Writer", 4),
        GuestStar => ("GuestStar", 5),
        Producer => ("Producer", 6),
        Conductor => ("Conductor", 7),
        Lyricist => ("Lyricist", 8),
        Arranger => ("Arranger", 9),
        Engineer => ("Engineer", 10),
        Mixer => ("Mixer", 11),
        Remixer => ("Remixer", 12),
        Creator => ("Creator", 13),
        Artist => ("Artist", 14),
        AlbumArtist => ("AlbumArtist", 15),
        Author => ("Author", 16),
        Illustrator => ("Illustrator", 17),
        Penciller => ("Penciller", 18),
        Inker => ("Inker", 19),
        Colorist => ("Colorist", 20),
        Letterer => ("Letterer", 21),
        CoverArtist => ("CoverArtist", 22),
        Editor => ("Editor", 23),
        Translator => ("Translator", 24),
        Narrator => ("Narrator", 25),
    }
}

/// The base item kind (generated upstream from all `BaseItem` subclasses).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[repr(i32)]
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
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    BaseItemKind, None, {
        AggregateFolder => ("AggregateFolder", 0),
        Audio => ("Audio", 1),
        AudioBook => ("AudioBook", 2),
        BasePluginFolder => ("BasePluginFolder", 3),
        Book => ("Book", 4),
        BoxSet => ("BoxSet", 5),
        Channel => ("Channel", 6),
        ChannelFolderItem => ("ChannelFolderItem", 7),
        CollectionFolder => ("CollectionFolder", 8),
        Episode => ("Episode", 9),
        Folder => ("Folder", 10),
        Genre => ("Genre", 11),
        ManualPlaylistsFolder => ("ManualPlaylistsFolder", 12),
        Movie => ("Movie", 13),
        LiveTvChannel => ("LiveTvChannel", 14),
        LiveTvProgram => ("LiveTvProgram", 15),
        MusicAlbum => ("MusicAlbum", 16),
        MusicArtist => ("MusicArtist", 17),
        MusicGenre => ("MusicGenre", 18),
        MusicVideo => ("MusicVideo", 19),
        Person => ("Person", 20),
        Photo => ("Photo", 21),
        PhotoAlbum => ("PhotoAlbum", 22),
        Playlist => ("Playlist", 23),
        PlaylistsFolder => ("PlaylistsFolder", 24),
        Program => ("Program", 25),
        Recording => ("Recording", 26),
        Season => ("Season", 27),
        Series => ("Series", 28),
        Studio => ("Studio", 29),
        Trailer => ("Trailer", 30),
        TvChannel => ("TvChannel", 31),
        TvProgram => ("TvProgram", 32),
        UserRootFolder => ("UserRootFolder", 33),
        UserView => ("UserView", 34),
        Video => ("Video", 35),
        Year => ("Year", 36),
    }
}

/// The `(kind, stored-type-name)` pairs, copied from the C#
/// `ItemTypeLookup.BaseItemKindNames` dictionary. Kinds with no dedicated
/// `BaseItem` subclass in the C# table (e.g. `AudioBook`, `Program`) are omitted,
/// exactly as upstream omits them.
pub const BASE_ITEM_KIND_NAMES: &[(BaseItemKind, &str)] = &[
    (
        BaseItemKind::AggregateFolder,
        "MediaBrowser.Controller.Entities.AggregateFolder",
    ),
    (
        BaseItemKind::Audio,
        "MediaBrowser.Controller.Entities.Audio.Audio",
    ),
    (
        BaseItemKind::AudioBook,
        "MediaBrowser.Controller.Entities.AudioBook",
    ),
    (
        BaseItemKind::BasePluginFolder,
        "MediaBrowser.Controller.Entities.BasePluginFolder",
    ),
    (BaseItemKind::Book, "MediaBrowser.Controller.Entities.Book"),
    (
        BaseItemKind::BoxSet,
        "MediaBrowser.Controller.Entities.Movies.BoxSet",
    ),
    (
        BaseItemKind::Channel,
        "MediaBrowser.Controller.Channels.Channel",
    ),
    (
        BaseItemKind::CollectionFolder,
        "MediaBrowser.Controller.Entities.CollectionFolder",
    ),
    (
        BaseItemKind::Episode,
        "MediaBrowser.Controller.Entities.TV.Episode",
    ),
    (
        BaseItemKind::Folder,
        "MediaBrowser.Controller.Entities.Folder",
    ),
    (
        BaseItemKind::Genre,
        "MediaBrowser.Controller.Entities.Genre",
    ),
    (
        BaseItemKind::ManualPlaylistsFolder,
        "Emby.Server.Implementations.Playlists.ManualPlaylistsFolder",
    ),
    (
        BaseItemKind::Movie,
        "MediaBrowser.Controller.Entities.Movies.Movie",
    ),
    (
        BaseItemKind::LiveTvChannel,
        "MediaBrowser.Controller.LiveTv.LiveTvChannel",
    ),
    (
        BaseItemKind::LiveTvProgram,
        "MediaBrowser.Controller.LiveTv.LiveTvProgram",
    ),
    (
        BaseItemKind::MusicAlbum,
        "MediaBrowser.Controller.Entities.Audio.MusicAlbum",
    ),
    (
        BaseItemKind::MusicArtist,
        "MediaBrowser.Controller.Entities.Audio.MusicArtist",
    ),
    (
        BaseItemKind::MusicGenre,
        "MediaBrowser.Controller.Entities.Audio.MusicGenre",
    ),
    (
        BaseItemKind::MusicVideo,
        "MediaBrowser.Controller.Entities.MusicVideo",
    ),
    (
        BaseItemKind::Person,
        "MediaBrowser.Controller.Entities.Person",
    ),
    (
        BaseItemKind::Photo,
        "MediaBrowser.Controller.Entities.Photo",
    ),
    (
        BaseItemKind::PhotoAlbum,
        "MediaBrowser.Controller.Entities.PhotoAlbum",
    ),
    (
        BaseItemKind::Playlist,
        "MediaBrowser.Controller.Playlists.Playlist",
    ),
    (
        BaseItemKind::PlaylistsFolder,
        "Emby.Server.Implementations.Playlists.PlaylistsFolder",
    ),
    (
        BaseItemKind::Season,
        "MediaBrowser.Controller.Entities.TV.Season",
    ),
    (
        BaseItemKind::Series,
        "MediaBrowser.Controller.Entities.TV.Series",
    ),
    (
        BaseItemKind::Studio,
        "MediaBrowser.Controller.Entities.Studio",
    ),
    (
        BaseItemKind::Trailer,
        "MediaBrowser.Controller.Entities.Trailer",
    ),
    (
        BaseItemKind::TvChannel,
        "MediaBrowser.Controller.LiveTv.LiveTvChannel",
    ),
    (
        BaseItemKind::TvProgram,
        "MediaBrowser.Controller.LiveTv.LiveTvProgram",
    ),
    (
        BaseItemKind::UserRootFolder,
        "MediaBrowser.Controller.Entities.UserRootFolder",
    ),
    (
        BaseItemKind::UserView,
        "MediaBrowser.Controller.Entities.UserView",
    ),
    (
        BaseItemKind::Video,
        "MediaBrowser.Controller.Entities.Video",
    ),
    (BaseItemKind::Year, "MediaBrowser.Controller.Entities.Year"),
];

impl BaseItemKind {
    /// The stored `BaseItems.Type` name of this kind (the C# class's
    /// `typeof(T).FullName`), or [`None`] for a kind with no dedicated stored
    /// type (matching the C# dictionary's coverage).
    #[must_use]
    pub fn stored_type_name(self) -> Option<&'static str> {
        BASE_ITEM_KIND_NAMES
            .iter()
            .find(|(k, _)| *k == self)
            .map(|(_, name)| *name)
    }

    /// The inverse of [`stored_type_name`](Self::stored_type_name): the kind a
    /// stored `BaseItems.Type` name names, or [`None`] for an unrecognized one.
    #[must_use]
    pub fn from_stored_type_name(type_name: &str) -> Option<Self> {
        BASE_ITEM_KIND_NAMES
            .iter()
            .find(|(_, name)| *name == type_name)
            .map(|(kind, _)| *kind)
    }
}

/// Media streaming protocol.
///
/// Members are lowercase for backwards compatibility with the wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
#[allow(non_camel_case_types)]
#[repr(i32)]
pub enum MediaStreamProtocol {
    /// HTTP.
    #[default]
    http = 0,
    /// HTTP Live Streaming.
    hls = 1,
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    MediaStreamProtocol, Some(0), {
        http => ("http", 0),
        hls => ("hls", 1),
    }
}

#[cfg(test)]
mod stored_type_name_tests {
    use super::{BASE_ITEM_KIND_NAMES, BaseItemKind};

    /// Every stored name maps back to a kind stored under that name (a few
    /// kinds share one — `TvChannel`/`LiveTvChannel` — and the first listed
    /// wins, as the C# dictionary's reverse lookup does), and the match is
    /// exact: a name is not recognised by its last segment alone.
    #[test]
    fn stored_type_names_round_trip_exactly() {
        for (kind, name) in BASE_ITEM_KIND_NAMES {
            assert_eq!(kind.stored_type_name(), Some(*name));
            let back = BaseItemKind::from_stored_type_name(name).expect("known name");
            assert_eq!(back.stored_type_name(), Some(*name));
        }
        assert_eq!(
            BaseItemKind::from_stored_type_name("MediaBrowser.Controller.Entities.TV.Episode"),
            Some(BaseItemKind::Episode)
        );
        assert_eq!(BaseItemKind::from_stored_type_name("Episode"), None);
        assert_eq!(BaseItemKind::from_stored_type_name("Not.A.Kind"), None);
    }
}
