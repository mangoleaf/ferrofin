//! Database-local enums stored as `#[repr(i32)]` `INTEGER` columns.
//!
//! These enums are **specific to the persistence layer** and deliberately
//! do not live in `ferrofin-model` — they mirror Jellyfin's
//! `Jellyfin.Database.Implementations.Enums` / `Entities` enums, which are
//! storage concerns (column discriminants) rather than wire DTOs.
//!
//! Two closely related enums — [`ferrofin_model::entities::MetadataField`] and
//! [`ferrofin_model::entities::TrailerType`] — already exist in `ferrofin-model`
//! with the identical discriminants, so they are **reused** from there (see
//! [`metadata_field`] / [`trailer_type`] for the `i32` mapping helpers) rather
//! than redefined here.
//!
//! Each generated enum provides:
//! - `#[repr(i32)]` with explicit discriminants matching the C# source, and
//! - [`TryFrom<i32>`] (fallible read from an `INTEGER` column) plus an
//!   infallible `as i32` cast for writing.
//!
//! `TryFrom` yields [`DbError::InvalidEnumValue`] for unknown discriminants.

use crate::error::DbError;
use ferrofin_model::entities::{MetadataField, TrailerType};

/// Generates a `#[repr(i32)]` enum with a `TryFrom<i32>` impl that rejects
/// unknown discriminants with [`DbError::InvalidEnumValue`].
///
/// Writing back to the database is done with a plain `value as i32` cast.
macro_rules! db_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$vmeta:meta])*
                $variant:ident = $value:expr
            ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(i32)]
        $vis enum $name {
            $(
                $(#[$vmeta])*
                $variant = $value,
            )+
        }

        impl TryFrom<i32> for $name {
            type Error = DbError;

            fn try_from(value: i32) -> Result<Self, Self::Error> {
                match value {
                    $( $value => Ok(Self::$variant), )+
                    other => Err(DbError::InvalidEnumValue {
                        enum_name: stringify!($name),
                        value: other,
                    }),
                }
            }
        }

        impl From<$name> for i32 {
            fn from(value: $name) -> Self {
                value as Self
            }
        }
    };
}

db_enum! {
    /// A permission toggle stored on a `Permissions` row.
    pub enum PermissionKind {
        /// Whether the user is an administrator.
        IsAdministrator = 0,
        /// Whether the user is hidden.
        IsHidden = 1,
        /// Whether the user is disabled.
        IsDisabled = 2,
        /// Whether the user can control shared devices.
        EnableSharedDeviceControl = 3,
        /// Whether the user can access the server remotely.
        EnableRemoteAccess = 4,
        /// Whether the user can manage live tv.
        EnableLiveTvManagement = 5,
        /// Whether the user can access live tv.
        EnableLiveTvAccess = 6,
        /// Whether the user can play media.
        EnableMediaPlayback = 7,
        /// Whether the server should transcode audio for the user if requested.
        EnableAudioPlaybackTranscoding = 8,
        /// Whether the server should transcode video for the user if requested.
        EnableVideoPlaybackTranscoding = 9,
        /// Whether the user can delete content.
        EnableContentDeletion = 10,
        /// Whether the user can download content.
        EnableContentDownloading = 11,
        /// Whether to enable sync transcoding for the user.
        EnableSyncTranscoding = 12,
        /// Whether the user can do media conversion.
        EnableMediaConversion = 13,
        /// Whether the user has access to all devices.
        EnableAllDevices = 14,
        /// Whether the user has access to all channels.
        EnableAllChannels = 15,
        /// Whether the user has access to all folders.
        EnableAllFolders = 16,
        /// Whether to enable public sharing for the user.
        EnablePublicSharing = 17,
        /// Whether the user can remotely control other users.
        EnableRemoteControlOfOtherUsers = 18,
        /// Whether the user is permitted to do playback remuxing.
        EnablePlaybackRemuxing = 19,
        /// Whether the server should force transcoding on remote connections.
        ForceRemoteSourceTranscoding = 20,
        /// Whether the user can create, modify and delete collections.
        EnableCollectionManagement = 21,
        /// Whether the user can edit subtitles.
        EnableSubtitleManagement = 22,
        /// Whether the user can edit lyrics.
        EnableLyricManagement = 23,
    }
}

db_enum! {
    /// A list-valued preference stored on a `Preferences` row.
    pub enum PreferenceKind {
        /// A list of blocked tags.
        BlockedTags = 0,
        /// A list of blocked channels.
        BlockedChannels = 1,
        /// A list of blocked media folders.
        BlockedMediaFolders = 2,
        /// A list of enabled devices.
        EnabledDevices = 3,
        /// A list of enabled channels.
        EnabledChannels = 4,
        /// A list of enabled folders.
        EnabledFolders = 5,
        /// A list of folders to allow content deletion from.
        EnableContentDeletionFromFolders = 6,
        /// A list of latest items to exclude.
        LatestItemExcludes = 7,
        /// A list of media to exclude.
        MyMediaExcludes = 8,
        /// A list of grouped folders.
        GroupedFolders = 9,
        /// A list of unrated items to block.
        BlockUnratedItems = 10,
        /// A list of ordered views.
        OrderedViews = 11,
        /// A list of allowed tags.
        AllowedTags = 12,
    }
}

db_enum! {
    /// The type of a home-screen section (`HomeSection.Type`).
    pub enum HomeSectionType {
        /// None.
        None = 0,
        /// My Media.
        SmallLibraryTiles = 1,
        /// My Media Small.
        LibraryButtons = 2,
        /// Active Recordings.
        ActiveRecordings = 3,
        /// Continue Watching.
        Resume = 4,
        /// Continue Listening.
        ResumeAudio = 5,
        /// Latest Media.
        LatestMedia = 6,
        /// Next Up.
        NextUp = 7,
        /// Live TV.
        LiveTv = 8,
        /// Continue Reading.
        ResumeBook = 9,
    }
}

db_enum! {
    /// The view type for a library or collection
    /// (`ItemDisplayPreferences.ViewType`).
    pub enum ViewType {
        /// Shows albums.
        Albums = 0,
        /// Shows album artists.
        AlbumArtists = 1,
        /// Shows artists.
        Artists = 2,
        /// Shows channels.
        Channels = 3,
        /// Shows collections.
        Collections = 4,
        /// Shows episodes.
        Episodes = 5,
        /// Shows favorites.
        Favorites = 6,
        /// Shows genres.
        Genres = 7,
        /// Shows guide.
        Guide = 8,
        /// Shows movies.
        Movies = 9,
        /// Shows networks.
        Networks = 10,
        /// Shows playlists.
        Playlists = 11,
        /// Shows programs.
        Programs = 12,
        /// Shows recordings.
        Recordings = 13,
        /// Shows schedule.
        Schedule = 14,
        /// Shows series.
        Series = 15,
        /// Shows shows.
        Shows = 16,
        /// Shows songs.
        Songs = 17,
        /// Shows suggestions.
        Suggestions = 18,
        /// Shows trailers.
        Trailers = 19,
        /// Shows upcoming.
        Upcoming = 20,
        /// Shows authors.
        Authors = 21,
        /// Shows books.
        Books = 22,
        /// Shows folders.
        Folders = 23,
        /// Shows mixed media.
        Mixed = 24,
        /// Shows photos.
        Photos = 25,
        /// Shows photo albums.
        PhotoAlbums = 26,
        /// Shows series timers.
        SeriesTimers = 27,
        /// Shows studios.
        Studios = 28,
        /// Shows videos.
        Videos = 29,
    }
}

db_enum! {
    /// A type of indexing in a user's display preferences (`IndexBy`).
    pub enum IndexingKind {
        /// Index by the premiere date.
        PremiereDate = 0,
        /// Index by the production year.
        ProductionYear = 1,
        /// Index by the community rating.
        CommunityRating = 2,
    }
}

db_enum! {
    /// The Chromecast client version (`DisplayPreferences.ChromecastVersion`).
    pub enum ChromecastVersion {
        /// Stable Chromecast version.
        Stable = 0,
        /// Unstable Chromecast version.
        Unstable = 1,
    }
}

db_enum! {
    /// The kind of a linked child (`HermitLinkedChildren.ChildType`).
    pub enum LinkedChildType {
        /// Manually linked child.
        Manual = 0,
        /// Shortcut linked child.
        Shortcut = 1,
        /// Local alternate version (same item, different file path).
        LocalAlternateVersion = 2,
        /// Linked alternate version (different item ID).
        LinkedAlternateVersion = 3,
    }
}

db_enum! {
    /// The kind of an item value (`ItemValues.Type`).
    ///
    /// Note: discriminant `5` is intentionally unused upstream.
    pub enum ItemValueType {
        /// Artists.
        Artist = 0,
        /// Album artist.
        AlbumArtist = 1,
        /// Genre.
        Genre = 2,
        /// Studios.
        Studios = 3,
        /// Tags.
        Tags = 4,
        /// Inherited tags.
        InheritedTags = 6,
    }
}

db_enum! {
    /// The stream kind stored on a `MediaStreamInfos` row (`StreamType`).
    pub enum MediaStreamTypeEntity {
        /// The audio.
        Audio = 0,
        /// The video.
        Video = 1,
        /// The subtitle.
        Subtitle = 2,
        /// The embedded image.
        EmbeddedImage = 3,
        /// The data.
        Data = 4,
        /// The lyric.
        Lyric = 5,
    }
}

db_enum! {
    /// The image kind stored on a `BaseItemImageInfos` row (`ImageType`).
    pub enum ImageInfoImageType {
        /// The primary image.
        Primary = 0,
        /// The art.
        Art = 1,
        /// The backdrop.
        Backdrop = 2,
        /// The banner.
        Banner = 3,
        /// The logo.
        Logo = 4,
        /// The thumb.
        Thumb = 5,
        /// The disc.
        Disc = 6,
        /// The box.
        Box = 7,
        /// The screenshot (obsolete upstream).
        Screenshot = 8,
        /// The menu.
        Menu = 9,
        /// The chapter image.
        Chapter = 10,
        /// The box rear.
        BoxRear = 11,
        /// The user profile image.
        Profile = 12,
    }
}

db_enum! {
    /// The audio layout of a program (`ProgramAudio`).
    pub enum ProgramAudioEntity {
        /// Mono.
        Mono = 0,
        /// Stereo.
        Stereo = 1,
        /// Dolby.
        Dolby = 2,
        /// Dolby Digital.
        DolbyDigital = 3,
        /// THX.
        Thx = 4,
        /// Atmos.
        Atmos = 5,
    }
}

/// `i32` mapping helpers for [`ferrofin_model::entities::MetadataField`].
///
/// `MetadataField` is reused from `ferrofin-model` (identical discriminants);
/// these free functions provide the DB-column conversions the model type does
/// not carry itself. The discriminants are the C# declaration order (0-based).
pub mod metadata_field {
    use super::{DbError, MetadataField};

    /// Reads a [`MetadataField`] from a stored `INTEGER` discriminant.
    ///
    /// # Errors
    /// Returns [`DbError::InvalidEnumValue`] for an unknown discriminant.
    pub fn from_i32(value: i32) -> Result<MetadataField, DbError> {
        let field = match value {
            0 => MetadataField::Cast,
            1 => MetadataField::Genres,
            2 => MetadataField::ProductionLocations,
            3 => MetadataField::Studios,
            4 => MetadataField::Tags,
            5 => MetadataField::Name,
            6 => MetadataField::Overview,
            7 => MetadataField::Runtime,
            8 => MetadataField::OfficialRating,
            other => {
                return Err(DbError::InvalidEnumValue {
                    enum_name: "MetadataField",
                    value: other,
                });
            }
        };
        Ok(field)
    }

    /// The `INTEGER` discriminant for a [`MetadataField`].
    #[must_use]
    pub fn to_i32(field: MetadataField) -> i32 {
        match field {
            MetadataField::Cast => 0,
            MetadataField::Genres => 1,
            MetadataField::ProductionLocations => 2,
            MetadataField::Studios => 3,
            MetadataField::Tags => 4,
            MetadataField::Name => 5,
            MetadataField::Overview => 6,
            MetadataField::Runtime => 7,
            MetadataField::OfficialRating => 8,
        }
    }
}

/// `i32` mapping helpers for [`ferrofin_model::entities::TrailerType`].
///
/// `TrailerType` is reused from `ferrofin-model` (identical discriminants);
/// these free functions provide the DB-column conversions. Discriminants are
/// 1-based, matching the C# source.
pub mod trailer_type {
    use super::{DbError, TrailerType};

    /// Reads a [`TrailerType`] from a stored `INTEGER` discriminant.
    ///
    /// # Errors
    /// Returns [`DbError::InvalidEnumValue`] for an unknown discriminant.
    pub fn from_i32(value: i32) -> Result<TrailerType, DbError> {
        let kind = match value {
            1 => TrailerType::ComingSoonToTheaters,
            2 => TrailerType::ComingSoonToDvd,
            3 => TrailerType::ComingSoonToStreaming,
            4 => TrailerType::Archive,
            5 => TrailerType::LocalTrailer,
            other => {
                return Err(DbError::InvalidEnumValue {
                    enum_name: "TrailerType",
                    value: other,
                });
            }
        };
        Ok(kind)
    }

    /// The `INTEGER` discriminant for a [`TrailerType`].
    #[must_use]
    pub fn to_i32(kind: TrailerType) -> i32 {
        match kind {
            TrailerType::ComingSoonToTheaters => 1,
            TrailerType::ComingSoonToDvd => 2,
            TrailerType::ComingSoonToStreaming => 3,
            TrailerType::Archive => 4,
            TrailerType::LocalTrailer => 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_generated_enum() {
        for value in 0..=23 {
            let kind = PermissionKind::try_from(value).expect("valid discriminant");
            assert_eq!(i32::from(kind), value);
        }
    }

    #[test]
    fn item_value_type_skips_five() {
        assert!(ItemValueType::try_from(5).is_err());
        assert_eq!(
            ItemValueType::try_from(6).expect("valid"),
            ItemValueType::InheritedTags
        );
    }

    #[test]
    fn rejects_unknown_discriminant() {
        let err = ChromecastVersion::try_from(99).expect_err("out of range");
        assert!(matches!(
            err,
            DbError::InvalidEnumValue {
                enum_name: "ChromecastVersion",
                value: 99
            }
        ));
    }

    #[test]
    fn reused_metadata_field_round_trips() {
        for value in 0..=8 {
            let field = metadata_field::from_i32(value).expect("valid");
            assert_eq!(metadata_field::to_i32(field), value);
        }
        assert!(metadata_field::from_i32(9).is_err());
    }

    #[test]
    fn reused_trailer_type_round_trips() {
        for value in 1..=5 {
            let kind = trailer_type::from_i32(value).expect("valid");
            assert_eq!(trailer_type::to_i32(kind), value);
        }
        assert!(trailer_type::from_i32(0).is_err());
    }
}
