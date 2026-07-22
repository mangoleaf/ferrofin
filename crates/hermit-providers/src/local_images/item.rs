//! The library-item view the local-image providers scan against.
//!
//! Port of the `BaseItem` members and concrete-type tests the C# image
//! providers read (`MediaBrowser.LocalMetadata.Images`). As with the NFO
//! parsers' [`crate::xbmc::item`], the `BaseItem` hierarchy is server-side
//! library plumbing dropped from `hermit-model`, so the union of the fields the
//! providers touch lives on one [`ImageItem`] with an [`ImageItemKind`]
//! discriminant. The providers switch on the kind exactly where the C# code did
//! `item is MusicAlbum` / `is Series` / `is Episode` / `is Person`, etc.

/// Which `BaseItem` subclass an [`ImageItem`] stands in for.
///
/// Drives the primary-image filename table selection and the per-kind logo /
/// art / disc / banner / thumb / backdrop branches in `LocalImageProvider`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageItemKind {
    /// A generic item using the common filename table (`poster/folder/cover/default`).
    #[default]
    Generic,
    /// A movie (`Movie` — a `Video`; uses the video filename table).
    Movie,
    /// A plain video (`Video`; uses the video filename table).
    Video,
    /// A music video (`MusicVideo` — a `Video`).
    MusicVideo,
    /// A TV series (`Series`; uses the series filename table).
    Series,
    /// A TV season (`Season`; may pull images from the parent series folder).
    Season,
    /// A TV episode (`Episode`; handled by [`super::EpisodeLocalImageProvider`]).
    Episode,
    /// A song (`Audio`, exactly — not `AudioBook`; excluded from most image kinds).
    Audio,
    /// An audiobook (`AudioBook` — an `Audio`, but *not* excluded like a song).
    AudioBook,
    /// A music album (`MusicAlbum`; prefers the folder filename + `cdart` disc).
    MusicAlbum,
    /// A music artist (`MusicArtist`; uses the music filename table).
    MusicArtist,
    /// A photo (`Photo`; excluded from local images).
    Photo,
    /// A photo album (`PhotoAlbum`; uses the music filename table).
    PhotoAlbum,
    /// A person (`Person`; uses the person filename table).
    Person,
    /// A box set (`BoxSet`; a video-like disc holder).
    BoxSet,
    /// A collection folder (`CollectionFolder`; scanned across physical locations).
    CollectionFolder,
}

impl ImageItemKind {
    /// Whether this kind derives from `Video` (`item is Video`).
    ///
    /// True for `Video`, `Movie`, `MusicVideo` and `Episode`; matches the C#
    /// class hierarchy (`Episode : Video`).
    #[must_use]
    pub fn is_video(self) -> bool {
        matches!(
            self,
            Self::Video | Self::Movie | Self::MusicVideo | Self::Episode
        )
    }

    /// Whether this kind is exactly a song (`item.GetType() == typeof(Audio)`).
    ///
    /// `AudioBook` derives from `Audio` but is *not* a song by this exact-type
    /// test, so it returns `false` here — matching the C# `isSong` computation.
    #[must_use]
    pub fn is_song(self) -> bool {
        matches!(self, Self::Audio)
    }

    /// Whether this kind is an `Audio` (song *or* audiobook).
    ///
    /// Port of `item is Audio`.
    #[must_use]
    pub fn is_audio(self) -> bool {
        matches!(self, Self::Audio | Self::AudioBook)
    }
}

/// The library-item view a local-image provider scans against.
///
/// A structural port of the `BaseItem` members the image providers read. Fields
/// default to empty/`None`/`false` to match a freshly-constructed C# item.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImageItem {
    /// The concrete item kind this stands in for.
    pub kind: ImageItemKind,

    /// The item's display name (`Name`).
    pub name: String,

    /// The item's full path (`Path`), if any.
    pub path: Option<String>,

    /// The folder that contains the item (`ContainingFolderPath`).
    ///
    /// For a single-file item this is the parent directory; for a folder item
    /// it is the folder itself. The image providers scan this directory.
    pub containing_folder_path: Option<String>,

    /// The item's filename without extension (`FileNameWithoutExtension`).
    ///
    /// Upstream this is derived from `Path`; it is carried explicitly here so
    /// the providers can match `"{FileNameWithoutExtension}-poster"` style
    /// prefixes without re-deriving path semantics.
    pub file_name_without_extension: Option<String>,

    /// Whether the item shares its folder with other content (`IsInMixedFolder`).
    ///
    /// When `true`, un-prefixed image filenames (e.g. bare `poster.png`) are not
    /// matched, since they could belong to a sibling item.
    pub is_in_mixed_folder: bool,

    /// The season's index number (`Season.IndexNumber`), for [`ImageItemKind::Season`].
    pub index_number: Option<i32>,

    /// The physical locations of a [`ImageItemKind::CollectionFolder`]
    /// (`CollectionFolder.PhysicalLocations`).
    pub physical_locations: Vec<String>,
}

impl ImageItem {
    /// Creates an item of `kind` with all other fields at their defaults.
    #[must_use]
    pub fn new(kind: ImageItemKind) -> Self {
        Self {
            kind,
            ..Self::default()
        }
    }
}
