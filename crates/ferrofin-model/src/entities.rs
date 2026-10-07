//! Entity enums — port of `MediaBrowser.Model.Entities`.
//!
//! Pure enums (and `PersonType` string constants) describing item media
//! streams, image kinds, encoding options, and metadata locks. Serde casing
//! matches the Jellyfin JSON contract exactly (see
//! `contracts/jellyfin-openapi-10.11.8.json`): most are PascalCase, but the
//! encoder/tonemapping/collection options are lowercase for backwards
//! compatibility (C# used `#pragma warning disable SA1300`).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Enum `ImageType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[repr(i32)]
pub enum ImageType {
    /// The primary.
    #[default]
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
    /// The screenshot (obsolete; not serialized by the XML serializer upstream).
    Screenshot = 8,
    /// The menu.
    Menu = 9,
    /// The chapter image.
    Chapter = 10,
    /// The box rear.
    BoxRear = 11,
    /// The user profile image.
    Profile = 12,
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    ImageType, None, {
        Primary => ("Primary", 0),
        Art => ("Art", 1),
        Backdrop => ("Backdrop", 2),
        Banner => ("Banner", 3),
        Logo => ("Logo", 4),
        Thumb => ("Thumb", 5),
        Disc => ("Disc", 6),
        Box => ("Box", 7),
        Screenshot => ("Screenshot", 8),
        Menu => ("Menu", 9),
        Chapter => ("Chapter", 10),
        BoxRear => ("BoxRear", 11),
        Profile => ("Profile", 12),
    }
}

/// Enum `MediaStreamType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[repr(i32)]
pub enum MediaStreamType {
    /// The audio.
    #[default]
    Audio,
    /// The video.
    Video,
    /// The subtitle.
    Subtitle,
    /// The embedded image.
    EmbeddedImage,
    /// The data.
    Data,
    /// The lyric.
    Lyric,
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    MediaStreamType, None, {
        Audio => ("Audio", 0),
        Video => ("Video", 1),
        Subtitle => ("Subtitle", 2),
        EmbeddedImage => ("EmbeddedImage", 3),
        Data => ("Data", 4),
        Lyric => ("Lyric", 5),
    }
}

/// Enum `VideoType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[repr(i32)]
pub enum VideoType {
    /// The video file.
    VideoFile,
    /// The iso.
    Iso,
    /// The DVD.
    Dvd,
    /// The blu ray.
    BluRay,
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    VideoType, None, {
        VideoFile => ("VideoFile", 0),
        Iso => ("Iso", 1),
        Dvd => ("Dvd", 2),
        BluRay => ("BluRay", 3),
    }
}

/// Enum `Video3DFormat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[repr(i32)]
pub enum Video3DFormat {
    /// Half side-by-side.
    HalfSideBySide,
    /// Full side-by-side.
    FullSideBySide,
    /// Full top-and-bottom.
    FullTopAndBottom,
    /// Half top-and-bottom.
    HalfTopAndBottom,
    /// Multiview Video Coding.
    #[serde(rename = "MVC")]
    Mvc,
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    Video3DFormat, None, {
        HalfSideBySide => ("HalfSideBySide", 0),
        FullSideBySide => ("FullSideBySide", 1),
        FullTopAndBottom => ("FullTopAndBottom", 2),
        HalfTopAndBottom => ("HalfTopAndBottom", 3),
        Mvc => ("MVC", 4),
    }
}

/// Enum `IsoType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[repr(i32)]
pub enum IsoType {
    /// The DVD.
    Dvd,
    /// The blu ray.
    BluRay,
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    IsoType, None, {
        Dvd => ("Dvd", 0),
        BluRay => ("BluRay", 1),
    }
}

/// Enum `LocationType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[repr(i32)]
pub enum LocationType {
    /// The file system.
    FileSystem = 0,
    /// The remote.
    Remote = 1,
    /// The virtual.
    Virtual = 2,
    /// The offline.
    Offline = 3,
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    LocationType, None, {
        FileSystem => ("FileSystem", 0),
        Remote => ("Remote", 1),
        Virtual => ("Virtual", 2),
        Offline => ("Offline", 3),
    }
}

/// Enum `ExtraType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[repr(i32)]
pub enum ExtraType {
    /// Unknown extra type.
    Unknown = 0,
    /// Clip.
    Clip = 1,
    /// Trailer.
    Trailer = 2,
    /// Behind the scenes.
    BehindTheScenes = 3,
    /// Deleted scene.
    DeletedScene = 4,
    /// Interview.
    Interview = 5,
    /// Scene.
    Scene = 6,
    /// Sample.
    Sample = 7,
    /// Theme song.
    ThemeSong = 8,
    /// Theme video.
    ThemeVideo = 9,
    /// Featurette.
    Featurette = 10,
    /// Short.
    Short = 11,
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    ExtraType, None, {
        Unknown => ("Unknown", 0),
        Clip => ("Clip", 1),
        Trailer => ("Trailer", 2),
        BehindTheScenes => ("BehindTheScenes", 3),
        DeletedScene => ("DeletedScene", 4),
        Interview => ("Interview", 5),
        Scene => ("Scene", 6),
        Sample => ("Sample", 7),
        ThemeSong => ("ThemeSong", 8),
        ThemeVideo => ("ThemeVideo", 9),
        Featurette => ("Featurette", 10),
        Short => ("Short", 11),
    }
}

/// Enum `TrailerType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum TrailerType {
    /// Coming soon to theaters.
    ComingSoonToTheaters = 1,
    /// Coming soon to DVD.
    ComingSoonToDvd = 2,
    /// Coming soon to streaming.
    ComingSoonToStreaming = 3,
    /// Archive.
    Archive = 4,
    /// Local trailer.
    LocalTrailer = 5,
}

/// The status of a series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum SeriesStatus {
    /// The series is currently releasing.
    Continuing,
    /// The series has completed and is no longer being released.
    Ended,
    /// The series has not been released yet.
    Unreleased,
}

/// Enum `MetadataField` — fields that can be locked against automatic edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[repr(i32)]
pub enum MetadataField {
    /// The cast.
    Cast,
    /// The genres.
    Genres,
    /// The production locations.
    ProductionLocations,
    /// The studios.
    Studios,
    /// The tags.
    Tags,
    /// The name.
    Name,
    /// The overview.
    Overview,
    /// The runtime.
    Runtime,
    /// The official rating.
    OfficialRating,
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    MetadataField, None, {
        Cast => ("Cast", 0),
        Genres => ("Genres", 1),
        ProductionLocations => ("ProductionLocations", 2),
        Studios => ("Studios", 3),
        Tags => ("Tags", 4),
        Name => ("Name", 5),
        Overview => ("Overview", 6),
        Runtime => ("Runtime", 7),
        OfficialRating => ("OfficialRating", 8),
    }
}

impl MetadataField {
    /// Every member, in declaration (discriminant) order.
    pub const ALL: [Self; 9] = [
        Self::Cast,
        Self::Genres,
        Self::ProductionLocations,
        Self::Studios,
        Self::Tags,
        Self::Name,
        Self::Overview,
        Self::Runtime,
        Self::OfficialRating,
    ];
}

/// The collection type options (library kinds an admin can create).
///
/// Members are lowercase for backwards compatibility with the wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
#[allow(non_camel_case_types)]
pub enum CollectionTypeOptions {
    /// Movies.
    movies = 0,
    /// TV Shows.
    tvshows = 1,
    /// Music.
    music = 2,
    /// Music Videos.
    musicvideos = 3,
    /// Home Videos (and Photos).
    homevideos = 4,
    /// Box Sets.
    boxsets = 5,
    /// Books.
    books = 6,
    /// Mixed Movies and TV Shows.
    mixed = 7,
}

/// Enum containing deinterlace methods. Lowercase for wire compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
#[allow(non_camel_case_types)]
pub enum DeinterlaceMethod {
    /// YADIF.
    yadif = 0,
    /// BWDIF.
    bwdif = 1,
}

/// An algorithm to downmix surround sound to stereo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum DownMixStereoAlgorithms {
    /// No special algorithm.
    None = 0,
    /// Algorithm by Dave_750.
    Dave750 = 1,
    /// Nightmode Dialogue algorithm.
    NightmodeDialogue = 2,
    /// RFC7845 Section 5.1.1.5 defined algorithm.
    Rfc7845 = 3,
    /// AC-4 standard algorithm with its default gain values (ETSI TS 103 190 6.2.17).
    Ac4 = 4,
}

/// Enum containing encoder presets. Lowercase for wire compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
#[allow(non_camel_case_types)]
pub enum EncoderPreset {
    /// Auto preset.
    auto = 0,
    /// Placebo preset.
    placebo = 1,
    /// Veryslow preset.
    veryslow = 2,
    /// Slower preset.
    slower = 3,
    /// Slow preset.
    slow = 4,
    /// Medium preset.
    medium = 5,
    /// Fast preset.
    fast = 6,
    /// Faster preset.
    faster = 7,
    /// Veryfast preset.
    veryfast = 8,
    /// Superfast preset.
    superfast = 9,
    /// Ultrafast preset.
    ultrafast = 10,
}

/// Enum containing hardware acceleration types. Lowercase for wire compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
#[allow(non_camel_case_types)]
pub enum HardwareAccelerationType {
    /// Software acceleration.
    none = 0,
    /// AMD AMF.
    amf = 1,
    /// Intel Quick Sync Video.
    qsv = 2,
    /// NVIDIA NVENC.
    nvenc = 3,
    /// Video4Linux2 V4L2M2M.
    v4l2m2m = 4,
    /// Video Acceleration API (VAAPI).
    vaapi = 5,
    /// Video ToolBox.
    videotoolbox = 6,
    /// Rockchip Media Process Platform (RKMPP).
    rkmpp = 7,
}

/// Enum containing tonemapping algorithms. Lowercase for wire compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
#[allow(non_camel_case_types)]
pub enum TonemappingAlgorithm {
    /// None.
    none = 0,
    /// Clip.
    clip = 1,
    /// Linear.
    linear = 2,
    /// Gamma.
    gamma = 3,
    /// Reinhard.
    reinhard = 4,
    /// Hable.
    hable = 5,
    /// Mobius.
    mobius = 6,
    /// BT2390.
    bt2390 = 7,
}

/// Enum containing tonemapping modes. Lowercase for wire compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
#[allow(non_camel_case_types)]
pub enum TonemappingMode {
    /// Auto.
    auto = 0,
    /// Max.
    max = 1,
    /// RGB.
    rgb = 2,
    /// Lum.
    lum = 3,
    /// ITP.
    itp = 4,
}

/// Enum containing tonemapping ranges. Lowercase for wire compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
#[allow(non_camel_case_types)]
pub enum TonemappingRange {
    /// Auto.
    auto = 0,
    /// TV.
    tv = 1,
    /// PC.
    pc = 2,
}

/// Enum `UserDataSaveReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum UserDataSaveReason {
    /// The playback start.
    PlaybackStart = 1,
    /// The playback progress.
    PlaybackProgress = 2,
    /// The playback finished.
    PlaybackFinished = 3,
    /// The toggle played.
    TogglePlayed = 4,
    /// The update user rating.
    UpdateUserRating = 5,
    /// The import.
    Import = 6,
    /// API call updated item user data.
    UpdateUserData = 7,
}

/// Types of persons — string constants (C# `PersonType` static class).
///
/// These are the legacy string values; new code should prefer
/// [`crate::data::PersonKind`]. Kept as `&str` constants to match the C#
/// `public const string` members exactly.
pub mod person_type {
    /// A person whose profession is acting on the stage, in films, or on television.
    pub const ACTOR: &str = "Actor";
    /// A person who supervises the actors and other staff in a production.
    pub const DIRECTOR: &str = "Director";
    /// A person who writes music, especially as a professional occupation.
    pub const COMPOSER: &str = "Composer";
    /// A writer of a book, article, or document (or generic music writer).
    pub const WRITER: &str = "Writer";
    /// A well-known performer appearing without a regular role.
    pub const GUEST_STAR: &str = "GuestStar";
    /// A person responsible for the financial and managerial aspects of a production.
    pub const PRODUCER: &str = "Producer";
    /// A person who directs the performance of an orchestra or choir.
    pub const CONDUCTOR: &str = "Conductor";
    /// A person who writes the words to a song or musical.
    pub const LYRICIST: &str = "Lyricist";
    /// A person who adapts a musical composition for performance.
    pub const ARRANGER: &str = "Arranger";
    /// An audio engineer who performed a general engineering role.
    pub const ENGINEER: &str = "Engineer";
    /// An engineer who mixed a recorded track into a single piece of music.
    pub const MIXER: &str = "Mixer";
    /// A person who remixed a recording from one or more other tracks.
    pub const REMIXER: &str = "Remixer";
}
