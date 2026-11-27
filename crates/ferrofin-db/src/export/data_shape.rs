//! Each item's `BaseItems.Data`, written in the shape Jellyfin 12.4 serialises
//! it (gl-1630).
//!
//! Jellyfin saves an item by serialising the whole of it
//! (`Jellyfin.Server.Implementations/Item/BaseItemMapper.cs:266-268`): every
//! public property without `[JsonIgnore]`, the most derived class's first and
//! `BaseItem`'s last, each class's in declaration order, with its value or its
//! initial one, a null left out (`JsonDefaults.Options`). Ferrofin's writers
//! store only the keys they have a value for, in name order, so a returned
//! Jellyfin rewrites every such row on its first save of it, and re-probes a
//! video whose stored external files it finds missing. The kinds marked
//! `[RequiresSourceSerialisation]` (`Season`, `MusicAlbum`, `Person`, …) keep
//! no `Data` in Jellyfin; their rows, and those of kinds 12.4 does not have,
//! are left as they are.
//!
//! So each row of a serialised kind is written as Jellyfin holds it after its
//! own scan: a value only `Data` keeps from `Data`, one `BaseItemMapper.Map`
//! overlays from its column or table (`Size`, `Width`, `Height`,
//! `DateLastSaved`, `LinkedAlternateVersions`, `TrailerTypes`), and the probe's
//! lists of external files and its `HasLyrics` from the stored streams, which
//! Ferrofin's own probe keeps there rather than in `Data`. The text is
//! `System.Text.Json`'s: its escapes and its number format.

use std::collections::HashMap;
use std::fmt::Write as _;

use ferrofin_model::entities::MediaStreamType;
use serde_json::{Map, Value};
use sqlx::SqliteConnection;
use uuid::Uuid;

use super::layout::Layout;
use super::paths::has_table;
use crate::Result;

/// Where one serialised property's value comes from.
#[derive(Debug, Clone, Copy)]
enum Source {
    /// Only `Data` keeps it, written back as Jellyfin's deserialiser reads it;
    /// a row without it has the property's initial value.
    Data(Shape),
    /// `BaseItem.Size`, overlaid from its column; a null one left out.
    Size,
    /// `BaseItem.Width` or `Height`, overlaid from its column, none being 0
    /// (`GetValueOrDefault`).
    Dimension(Dimension),
    /// `BaseItem.DateLastSaved`, overlaid from its column.
    DateLastSaved,
    /// `BaseItem.IsHD`, computed: `Height >= 720` (`BaseItem.cs:420`).
    IsHd,
    /// A video's `SubtitleFiles` or `AudioFiles`: the paths of its external
    /// streams of that type, which the probe sets them to
    /// (`FFProbeVideoInfo.cs:575,595`).
    ExternalFiles(MediaStreamType),
    /// A track's `HasLyrics`: whether its streams hold a lyric
    /// (`AudioFileProber.cs:157`); a track never probed keeps what `Data` says.
    HasLyrics,
    /// A video's `LinkedAlternateVersions`, from the `LinkedChildren` table
    /// (`BaseItemMapper.cs:168-186`).
    LinkedAlternateVersions,
    /// A trailer's `TrailerTypes`, from `BaseItemTrailerTypes`
    /// (`BaseItemMapper.cs:163-166`).
    TrailerTypes,
    /// The root's `VirtualChildren`: the playlists folder
    /// `LibraryManager.CreateRootFolder` adds at every start, serialised as a
    /// `BaseItem`.
    VirtualChildren,
    /// A movie's `TmdbCollectionName`, which Ferrofin keeps as
    /// `CollectionName`, the name of the `[JsonIgnore]` property over it.
    CollectionName,
}

/// `Width` or `Height`.
#[derive(Debug, Clone, Copy)]
enum Dimension {
    Width,
    Height,
}

/// A property only `Data` keeps: how Jellyfin reads its value, and its value
/// when the row has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// A string, number or enum name; none is left out.
    Value,
    /// A non-nullable enum or string with this initial value.
    ValueOr(&'static str),
    /// A `bool` with this initial value.
    Bool(bool),
    /// A list with an empty one as its initial value.
    List,
    /// A non-nullable `Guid`, written in Jellyfin's `N` form; none is the
    /// empty one.
    Guid,
    /// A `Guid?`: none is left out, the empty one is written as null
    /// (`JsonNullableGuidConverter`).
    NullableGuid,
    /// A `Guid[]` an empty list initialises.
    Guids,
    /// A `Guid[]` nothing initialises: none is left out.
    NullableGuids,
    /// `IReadOnlyList<MediaUrl>`.
    MediaUrls,
    /// `IReadOnlyList<PlaylistUserPermissions>`.
    Shares,
}

/// One serialised property: its name and where its value comes from.
type Property = (&'static str, Source);

/// `BaseItem`'s serialised properties, last in every kind
/// (`MediaBrowser.Controller/Entities/BaseItem.cs`).
const BASE_ITEM: &[Property] = &[
    ("Size", Source::Size),
    ("Container", Source::Data(Shape::Value)),
    ("IsHD", Source::IsHd),
    ("IsShortcut", Source::Data(Shape::Bool(false))),
    ("ShortcutPath", Source::Data(Shape::Value)),
    ("Width", Source::Dimension(Dimension::Width)),
    ("Height", Source::Dimension(Dimension::Height)),
    ("DateLastSaved", Source::DateLastSaved),
    ("RemoteTrailers", Source::Data(Shape::MediaUrls)),
];

/// `Folder`'s.
const FOLDER: &[Property] = &[("IsRoot", Source::Data(Shape::Bool(false)))];

/// `UserRootFolder`'s `Folder` properties: its constructor sets `IsRoot`.
const USER_ROOT_FOLDER: &[Property] = &[("IsRoot", Source::Data(Shape::Bool(true)))];

/// `AggregateFolder`'s.
const AGGREGATE_FOLDER: &[Property] = &[
    ("VirtualChildren", Source::VirtualChildren),
    ("PhysicalLocationsList", Source::Data(Shape::List)),
];

/// `CollectionFolder`'s.
const COLLECTION_FOLDER: &[Property] = &[
    ("PhysicalLocationsList", Source::Data(Shape::List)),
    ("PhysicalFolderIds", Source::Data(Shape::Guids)),
    ("CollectionType", Source::Data(Shape::Value)),
];

/// `UserView`'s.
const USER_VIEW: &[Property] = &[
    ("ViewType", Source::Data(Shape::Value)),
    ("DisplayParentId", Source::Data(Shape::Guid)),
    ("UserId", Source::Data(Shape::NullableGuid)),
];

/// `BoxSet`'s: its constructor sets `DisplayOrder`.
const BOX_SET: &[Property] = &[
    ("DisplayOrder", Source::Data(Shape::ValueOr("PremiereDate"))),
    ("LibraryFolderIds", Source::Data(Shape::NullableGuids)),
];

/// `Playlist`'s.
const PLAYLIST: &[Property] = &[
    ("OwnerUserId", Source::Data(Shape::Guid)),
    ("OpenAccess", Source::Data(Shape::Bool(false))),
    ("Shares", Source::Data(Shape::Shares)),
    ("PlaylistMediaType", Source::Data(Shape::ValueOr("Unknown"))),
];

/// `Series`'.
const SERIES: &[Property] = &[
    ("AirDays", Source::Data(Shape::List)),
    ("AirTime", Source::Data(Shape::Value)),
    ("DisplayOrder", Source::Data(Shape::Value)),
    ("Status", Source::Data(Shape::Value)),
];

/// `Video`'s.
const VIDEO: &[Property] = &[
    ("AdditionalParts", Source::Data(Shape::List)),
    ("LocalAlternateVersions", Source::Data(Shape::List)),
    ("LinkedAlternateVersions", Source::LinkedAlternateVersions),
    ("Timestamp", Source::Data(Shape::Value)),
    (
        "SubtitleFiles",
        Source::ExternalFiles(MediaStreamType::Subtitle),
    ),
    ("AudioFiles", Source::ExternalFiles(MediaStreamType::Audio)),
    ("HasSubtitles", Source::Data(Shape::Bool(false))),
    ("IsPlaceHolder", Source::Data(Shape::Bool(false))),
    ("DefaultVideoStreamIndex", Source::Data(Shape::Value)),
    ("VideoType", Source::Data(Shape::ValueOr("VideoFile"))),
    ("IsoType", Source::Data(Shape::Value)),
    ("Video3DFormat", Source::Data(Shape::Value)),
    ("AspectRatio", Source::Data(Shape::Value)),
];

/// `Movie`'s.
const MOVIE: &[Property] = &[("TmdbCollectionName", Source::CollectionName)];

/// `Episode`'s.
const EPISODE: &[Property] = &[
    ("AirsBeforeSeasonNumber", Source::Data(Shape::Value)),
    ("AirsAfterSeasonNumber", Source::Data(Shape::Value)),
    ("AirsBeforeEpisodeNumber", Source::Data(Shape::Value)),
    ("IndexNumberEnd", Source::Data(Shape::Value)),
];

/// `Trailer`'s.
const TRAILER: &[Property] = &[("TrailerTypes", Source::TrailerTypes)];

/// `Audio`'s.
const AUDIO: &[Property] = &[
    ("HasLyrics", Source::HasLyrics),
    ("LyricFiles", Source::Data(Shape::List)),
];

/// `Photo`'s.
const PHOTO: &[Property] = &[
    ("CameraMake", Source::Data(Shape::Value)),
    ("CameraModel", Source::Data(Shape::Value)),
    ("Software", Source::Data(Shape::Value)),
    ("ExposureTime", Source::Data(Shape::Value)),
    ("FocalLength", Source::Data(Shape::Value)),
    ("Orientation", Source::Data(Shape::Value)),
    ("Aperture", Source::Data(Shape::Value)),
    ("ShutterSpeed", Source::Data(Shape::Value)),
    ("Latitude", Source::Data(Shape::Value)),
    ("Longitude", Source::Data(Shape::Value)),
    ("Altitude", Source::Data(Shape::Value)),
    ("IsoSpeedRating", Source::Data(Shape::Value)),
];

/// `LiveTvChannel`'s.
const LIVE_TV_CHANNEL: &[Property] = &[
    ("Number", Source::Data(Shape::Value)),
    ("ChannelType", Source::Data(Shape::ValueOr("TV"))),
];

/// Every serialised kind Ferrofin stores, by its stored type, with its classes'
/// properties from the most derived class to `BaseItem`.
const KINDS: [(&str, &[&[Property]]); 17] = [
    (
        "MediaBrowser.Controller.Entities.AggregateFolder",
        &[AGGREGATE_FOLDER, FOLDER, BASE_ITEM],
    ),
    (
        "MediaBrowser.Controller.Entities.Folder",
        &[FOLDER, BASE_ITEM],
    ),
    (
        "MediaBrowser.Controller.Entities.UserRootFolder",
        &[USER_ROOT_FOLDER, BASE_ITEM],
    ),
    (
        "MediaBrowser.Controller.Entities.CollectionFolder",
        &[COLLECTION_FOLDER, FOLDER, BASE_ITEM],
    ),
    (
        "MediaBrowser.Controller.Entities.UserView",
        &[USER_VIEW, FOLDER, BASE_ITEM],
    ),
    (
        "MediaBrowser.Controller.Entities.Movies.BoxSet",
        &[BOX_SET, FOLDER, BASE_ITEM],
    ),
    (
        "MediaBrowser.Controller.Playlists.Playlist",
        &[PLAYLIST, FOLDER, BASE_ITEM],
    ),
    (
        "MediaBrowser.Controller.Entities.TV.Series",
        &[SERIES, FOLDER, BASE_ITEM],
    ),
    (
        "MediaBrowser.Controller.Channels.Channel",
        &[FOLDER, BASE_ITEM],
    ),
    (
        "MediaBrowser.Controller.Entities.Video",
        &[VIDEO, BASE_ITEM],
    ),
    (
        "MediaBrowser.Controller.Entities.Movies.Movie",
        &[MOVIE, VIDEO, BASE_ITEM],
    ),
    (
        "MediaBrowser.Controller.Entities.TV.Episode",
        &[EPISODE, VIDEO, BASE_ITEM],
    ),
    (
        "MediaBrowser.Controller.Entities.Trailer",
        &[TRAILER, VIDEO, BASE_ITEM],
    ),
    (
        "MediaBrowser.Controller.Entities.MusicVideo",
        &[VIDEO, BASE_ITEM],
    ),
    (
        "MediaBrowser.Controller.Entities.Audio.Audio",
        &[AUDIO, BASE_ITEM],
    ),
    (
        "MediaBrowser.Controller.Entities.Photo",
        &[PHOTO, BASE_ITEM],
    ),
    (
        "MediaBrowser.Controller.LiveTv.LiveTvChannel",
        &[LIVE_TV_CHANNEL, BASE_ITEM],
    ),
];

/// The playlists folder's stored type.
const PLAYLISTS_FOLDER_TYPE: &str = "Emby.Server.Implementations.Playlists.PlaylistsFolder";

/// The root's stored type.
const AGGREGATE_FOLDER_TYPE: &str = "MediaBrowser.Controller.Entities.AggregateFolder";

/// `TrailerType`'s names, by value from 1 (`MediaBrowser.Model/Entities/TrailerType.cs`).
const TRAILER_TYPES: [&str; 5] = [
    "ComingSoonToTheaters",
    "ComingSoonToDvd",
    "ComingSoonToStreaming",
    "Archive",
    "LocalTrailer",
];

/// `BaseItem.IsHD`'s threshold (`BaseItem.cs:420`).
const HD_HEIGHT: i64 = 720;

/// The `BaseItems` columns the step reads.
const ITEM_COLUMNS: [&str; 8] = [
    "Id",
    "Type",
    "Data",
    "Size",
    "Width",
    "Height",
    "DateLastSaved",
    "ParentId",
];

/// `LinkedChildType.LinkedAlternateVersion`, the stored `ChildType` of a
/// video's linked alternate version.
const LINKED_ALTERNATE_VERSION: i64 = 3;

/// The properties of the kind stored as `type_name`; `None` for a kind
/// Jellyfin keeps no `Data` for, or does not have.
fn properties(type_name: &str) -> Option<&'static [&'static [Property]]> {
    KINDS
        .iter()
        .find(|(name, _)| *name == type_name)
        .map(|(_, layers)| *layers)
}

/// The columns of one item row the shape reads.
#[derive(Debug, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
struct ItemRow {
    id: String,
    #[sqlx(rename = "Type")]
    kind: String,
    data: Option<String>,
    size: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
    date_last_saved: Option<String>,
}

/// What the stored streams say of one item.
#[derive(Debug, Default)]
struct Streams {
    /// The paths of its external subtitle streams, in stream order, each once.
    subtitles: Vec<String>,
    /// The same for its external audio streams.
    audio: Vec<String>,
    /// Whether any is a lyric.
    lyric: bool,
}

/// What the other tables hold of the items, keyed by their id in upper case.
#[derive(Debug, Default)]
struct Tables {
    streams: HashMap<String, Streams>,
    /// A video's linked alternate versions' ids, in `SortOrder`.
    versions: HashMap<String, Vec<String>>,
    /// A trailer's trailer types, in key order.
    trailer_types: HashMap<String, Vec<i64>>,
    /// The playlists folder the root holds at every start, if any.
    playlists_folder: Option<ItemRow>,
    /// `%AppDataPath%` and `%MetadataPath%` with their directories in the
    /// layout: the tokens a stream's stored path may hold.
    tokens: [(&'static str, String); 2],
}

/// Rewrites the `Data` of every item of a serialised kind in the staged
/// database to the shape Jellyfin 12.4 serialises it in, its stored paths read
/// in `layout`, and returns how many rows changed. A database whose
/// `BaseItems` lacks a column this reads (or the table) is left as it is, as
/// the other steps leave one.
pub(super) async fn rewrite(conn: &mut SqliteConnection, layout: &Layout) -> Result<u64> {
    let found: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('BaseItems') \
         WHERE name IN (SELECT value FROM json_each(?1))",
    )
    .bind(Value::from(ITEM_COLUMNS.to_vec()).to_string())
    .fetch_one(&mut *conn)
    .await?;
    if usize::try_from(found).ok() != Some(ITEM_COLUMNS.len()) {
        return Ok(0);
    }
    let tables = Tables::read(conn, layout).await?;
    let kinds = Value::from(KINDS.map(|(name, _)| name).to_vec()).to_string();
    let rows: Vec<ItemRow> = sqlx::query_as(
        r#"SELECT "Id", "Type", "Data", "Size", "Width", "Height", "DateLastSaved"
           FROM "BaseItems" WHERE "Type" IN (SELECT value FROM json_each(?1)) ORDER BY "Id""#,
    )
    .bind(kinds)
    .fetch_all(&mut *conn)
    .await?;
    let mut rewritten = 0;
    for row in &rows {
        let Some(layers) = properties(&row.kind) else {
            continue;
        };
        let shaped = serialise(row, layers, &tables);
        if row.data.as_deref() != Some(shaped.as_str()) {
            sqlx::query(r#"UPDATE "BaseItems" SET "Data" = ?1 WHERE "Id" = ?2"#)
                .bind(&shaped)
                .bind(&row.id)
                .execute(&mut *conn)
                .await?;
            rewritten += 1;
        }
    }
    Ok(rewritten)
}

impl Tables {
    async fn read(conn: &mut SqliteConnection, layout: &Layout) -> Result<Self> {
        let mut tables = Self {
            tokens: [
                ("%AppDataPath%", layout.data()),
                ("%MetadataPath%", layout.metadata()),
            ],
            ..Self::default()
        };
        if has_table(conn, "MediaStreamInfos").await? {
            let rows: Vec<(String, i64, Option<bool>, Option<String>)> = sqlx::query_as(
                r#"SELECT upper("ItemId"), "StreamType", "IsExternal", "Path"
                   FROM "MediaStreamInfos" ORDER BY "ItemId", "StreamIndex""#,
            )
            .fetch_all(&mut *conn)
            .await?;
            for (item, kind, external, path) in rows {
                let streams = tables.streams.entry(item).or_default();
                let kind = i32::try_from(kind).ok();
                streams.lyric |= kind == Some(MediaStreamType::Lyric.json_value());
                let list = if kind == Some(MediaStreamType::Subtitle.json_value()) {
                    &mut streams.subtitles
                } else if kind == Some(MediaStreamType::Audio.json_value()) {
                    &mut streams.audio
                } else {
                    continue;
                };
                if let (Some(true), Some(path)) = (external, path) {
                    let path = expand(&path, &tables.tokens);
                    if !list.contains(&path) {
                        list.push(path);
                    }
                }
            }
        }
        if has_table(conn, "LinkedChildren").await? {
            let rows: Vec<(String, String)> = sqlx::query_as(
                r#"SELECT upper("ParentId"), "ChildId" FROM "LinkedChildren"
                   WHERE "ChildType" = ?1 ORDER BY "ParentId", "SortOrder""#,
            )
            .bind(LINKED_ALTERNATE_VERSION)
            .fetch_all(&mut *conn)
            .await?;
            for (parent, child) in rows {
                tables.versions.entry(parent).or_default().push(child);
            }
        }
        if has_table(conn, "BaseItemTrailerTypes").await? {
            let rows: Vec<(String, i64)> = sqlx::query_as(
                r#"SELECT upper("ItemId"), "Id" FROM "BaseItemTrailerTypes"
                   ORDER BY "ItemId", "Id""#,
            )
            .fetch_all(&mut *conn)
            .await?;
            for (item, kind) in rows {
                tables.trailer_types.entry(item).or_default().push(kind);
            }
        }
        // The one under the root first, which `CreateRootFolder` finds by its
        // path; Jellyfin's database has only that one.
        tables.playlists_folder = sqlx::query_as(
            r#"SELECT "Id", "Type", NULL AS "Data", "Size", "Width", "Height", "DateLastSaved"
               FROM "BaseItems" WHERE "Type" = ?1
               ORDER BY "ParentId" IS NOT (SELECT "Id" FROM "BaseItems" WHERE "Type" = ?2
                                           ORDER BY "Id" LIMIT 1), "Id"
               LIMIT 1"#,
        )
        .bind(PLAYLISTS_FOLDER_TYPE)
        .bind(AGGREGATE_FOLDER_TYPE)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(tables)
    }
}

/// `path` with each virtual-path token replaced by its directory, as
/// `ApplicationHost.ExpandVirtualPath` does (ignoring case).
fn expand(path: &str, tokens: &[(&str, String)]) -> String {
    let mut out = path.to_owned();
    for (token, dir) in tokens {
        // Tokens are ASCII, so the lower-cased copy has the same byte offsets.
        while let Some(at) = out.to_ascii_lowercase().find(&token.to_ascii_lowercase()) {
            out.replace_range(at..at + token.len(), dir);
        }
    }
    out
}

/// The `Data` text Jellyfin serialises for `row`, a kind with the classes'
/// properties `layers`.
fn serialise(row: &ItemRow, layers: &[&[Property]], tables: &Tables) -> String {
    let data = match row.data.as_deref().map(serde_json::from_str::<Value>) {
        Some(Ok(Value::Object(data))) => data,
        // Jellyfin reads a blob it cannot deserialise as a new item's.
        _ => Map::new(),
    };
    let key = row.id.to_ascii_uppercase();
    let mut out = Json::default();
    out.open('{');
    for property in layers.iter().flat_map(|layer| layer.iter()) {
        write_property(&mut out, *property, row, &data, &key, tables);
    }
    out.close('}');
    out.text
}

/// Writes one property of `row` to `out`, or nothing for a null one.
fn write_property(
    out: &mut Json,
    (name, source): Property,
    row: &ItemRow,
    data: &Map<String, Value>,
    key: &str,
    tables: &Tables,
) {
    // A JSON null, which no writer stores (both leave a null out), reads as none.
    let stored = data.get(name).filter(|value| !value.is_null());
    match source {
        Source::Data(shape) => write_stored(out, name, shape, stored),
        Source::Size => {
            if let Some(size) = row.size {
                out.key(name);
                out.raw(&size.to_string());
            }
        }
        Source::Dimension(which) => {
            let value = match which {
                Dimension::Width => row.width,
                Dimension::Height => row.height,
            };
            out.key(name);
            out.raw(&value.unwrap_or(0).to_string());
        }
        Source::DateLastSaved => {
            out.key(name);
            out.string(&date_time(row.date_last_saved.as_deref()));
        }
        Source::IsHd => {
            out.key(name);
            out.raw(bool_text(row.height.unwrap_or(0) >= HD_HEIGHT));
        }
        Source::ExternalFiles(kind) => {
            let files = tables.streams.get(key).map_or(&[][..], |streams| {
                if kind == MediaStreamType::Subtitle {
                    streams.subtitles.as_slice()
                } else {
                    streams.audio.as_slice()
                }
            });
            out.key(name);
            out.open('[');
            for file in files {
                out.item();
                out.string(file);
            }
            out.close(']');
        }
        Source::HasLyrics => match tables.streams.get(key) {
            Some(streams) => {
                out.key(name);
                out.raw(bool_text(streams.lyric));
            }
            None => write_stored(out, name, Shape::Value, stored),
        },
        Source::LinkedAlternateVersions => {
            out.key(name);
            out.open('[');
            for child in tables.versions.get(key).into_iter().flatten() {
                out.item();
                out.open('{');
                out.key("Type");
                out.string("LinkedAlternateVersion");
                out.key("ItemId");
                write_guid(out, child, true);
                out.close('}');
            }
            out.close(']');
        }
        Source::TrailerTypes => {
            out.key(name);
            out.open('[');
            for kind in tables.trailer_types.get(key).into_iter().flatten() {
                out.item();
                match kind
                    .checked_sub(1)
                    .and_then(|i| usize::try_from(i).ok())
                    .and_then(|i| TRAILER_TYPES.get(i))
                {
                    Some(trailer_type) => out.string(trailer_type),
                    // An undefined value is written as its number.
                    None => out.raw(&kind.to_string()),
                }
            }
            out.close(']');
        }
        Source::VirtualChildren => {
            out.key(name);
            out.open('[');
            if let Some(folder) = &tables.playlists_folder {
                out.item();
                out.open('{');
                for property in BASE_ITEM {
                    write_property(out, *property, folder, &Map::new(), "", tables);
                }
                out.close('}');
            }
            out.close(']');
        }
        Source::CollectionName => {
            let name_stored = data
                .get("CollectionName")
                .filter(|value| !value.is_null())
                .or(stored);
            write_stored(out, name, Shape::Value, name_stored);
        }
    }
}

/// Writes the property `name` of shape `shape` that `Data` stores as `stored`.
fn write_stored(out: &mut Json, name: &str, shape: Shape, stored: Option<&Value>) {
    match (shape, stored) {
        (Shape::Value | Shape::NullableGuid | Shape::NullableGuids, None) => {}
        (Shape::ValueOr(initial), None) => {
            out.key(name);
            out.string(initial);
        }
        (Shape::Bool(initial), None) => {
            out.key(name);
            out.raw(bool_text(initial));
        }
        (Shape::List | Shape::Guids | Shape::MediaUrls | Shape::Shares, None) => {
            out.key(name);
            out.raw("[]");
        }
        (Shape::Guid, None) => {
            out.key(name);
            out.string(&Uuid::nil().simple().to_string());
        }
        (Shape::Guid | Shape::NullableGuid, Some(Value::String(id))) => {
            out.key(name);
            write_guid(out, id, shape == Shape::NullableGuid);
        }
        (Shape::Guids | Shape::NullableGuids, Some(Value::Array(ids))) => {
            out.key(name);
            out.open('[');
            for id in ids {
                out.item();
                match id {
                    Value::String(id) => write_guid(out, id, false),
                    other => out.value(other),
                }
            }
            out.close(']');
        }
        (Shape::MediaUrls, Some(Value::Array(urls))) => {
            out.key(name);
            write_objects(out, urls, &["Url", "Name"]);
        }
        (Shape::Shares, Some(Value::Array(shares))) => {
            out.key(name);
            write_objects(out, shares, &["UserId", "CanEdit"]);
        }
        (_, Some(value)) => {
            out.key(name);
            out.value(value);
        }
    }
}

/// Writes `objects`, a list of `MediaUrl` or `PlaylistUserPermissions`, with
/// the class's properties `names` in their order: a string or a missing one
/// as `MediaUrl`'s are (a null left out), `UserId` as a `Guid`, `CanEdit` as a
/// `bool` (false when missing).
fn write_objects(out: &mut Json, objects: &[Value], names: &[&str]) {
    out.open('[');
    for object in objects {
        out.item();
        let Value::Object(object) = object else {
            out.value(object);
            continue;
        };
        out.open('{');
        for name in names {
            match (*name, object.get(*name).filter(|value| !value.is_null())) {
                ("UserId", Some(Value::String(id))) => {
                    out.key(name);
                    write_guid(out, id, false);
                }
                ("UserId", None) => {
                    out.key(name);
                    out.string(&Uuid::nil().simple().to_string());
                }
                ("CanEdit", None) => {
                    out.key(name);
                    out.raw(bool_text(false));
                }
                (_, Some(value)) => {
                    out.key(name);
                    out.value(value);
                }
                (_, None) => {}
            }
        }
        out.close('}');
    }
    out.close(']');
}

/// Writes the `Guid` stored as `id` in Jellyfin's `N` form
/// (`JsonGuidConverter`), the empty one as null when the property is a `Guid?`
/// (`JsonNullableGuidConverter`); text that is no `Guid` as it is.
fn write_guid(out: &mut Json, id: &str, nullable: bool) {
    match Uuid::parse_str(id) {
        Ok(id) if nullable && id.is_nil() => out.raw("null"),
        Ok(id) => out.string(&id.simple().to_string()),
        Err(_) => out.string(id),
    }
}

/// `true` or `false`.
fn bool_text(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

/// The stored `DateLastSaved` (`yyyy-MM-dd HH:mm:ss.FFFFFFF`, none being
/// `DateTime.MinValue`) as `JsonDateTimeConverter` writes the UTC time Jellyfin
/// saves: with all seven digits of its fraction when its milliseconds are 0,
/// else `System.Text.Json`'s round-trip form, the fraction's trailing zeros
/// trimmed. Text in neither form is written as it is.
fn date_time(stored: Option<&str>) -> String {
    let stored = stored.unwrap_or("0001-01-01 00:00:00");
    let text = stored.trim_end_matches('Z');
    let Some((date, time)) = text.split_once([' ', 'T']) else {
        return stored.to_owned();
    };
    let (seconds, fraction) = time.split_once('.').unwrap_or((time, ""));
    if date.len() != 10
        || seconds.len() != 8
        || fraction.len() > 7
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return stored.to_owned();
    }
    let fraction = format!("{fraction:0<7}");
    if fraction.starts_with("000") {
        format!("{date}T{seconds}.{fraction}Z")
    } else {
        format!("{date}T{seconds}.{}Z", fraction.trim_end_matches('0'))
    }
}

/// A JSON text written as `System.Text.Json` writes it with Jellyfin's options:
/// no white space, the default encoder's escapes, .NET's number format.
#[derive(Debug, Default)]
struct Json {
    text: String,
    /// For each object or list open, whether it has a member yet.
    started: Vec<bool>,
}

impl Json {
    /// Opens an object or a list.
    fn open(&mut self, bracket: char) {
        self.text.push(bracket);
        self.started.push(false);
    }

    /// Closes the object or list opened last.
    fn close(&mut self, bracket: char) {
        self.text.push(bracket);
        self.started.pop();
    }

    /// Starts a member: the comma before any but the first.
    fn item(&mut self) {
        if let Some(started) = self.started.last_mut() {
            if *started {
                self.text.push(',');
            }
            *started = true;
        }
    }

    /// Starts an object's member `name`.
    fn key(&mut self, name: &str) {
        self.item();
        self.string(name);
        self.text.push(':');
    }

    /// Writes text already in JSON form.
    fn raw(&mut self, text: &str) {
        self.text.push_str(text);
    }

    /// Writes `text` as a JSON string with the default encoder's escapes: a
    /// short form for the backslash and the control characters that have one,
    /// `\uXXXX` (upper case, UTF-16) for the quote, every other control
    /// character, `&`, `'`, `+`, `<`, `>`, the backtick, DEL and everything
    /// beyond ASCII.
    fn string(&mut self, text: &str) {
        self.text.push('"');
        for c in text.chars() {
            match c {
                '\\' => self.text.push_str("\\\\"),
                '\n' => self.text.push_str("\\n"),
                '\r' => self.text.push_str("\\r"),
                '\t' => self.text.push_str("\\t"),
                '\u{8}' => self.text.push_str("\\b"),
                '\u{c}' => self.text.push_str("\\f"),
                ' '..='~' if !matches!(c, '"' | '&' | '\'' | '+' | '<' | '>' | '`') => {
                    self.text.push(c);
                }
                _ => {
                    let mut units = [0u16; 2];
                    for unit in c.encode_utf16(&mut units) {
                        let _ = write!(self.text, "\\u{unit:04X}");
                    }
                }
            }
        }
        self.text.push('"');
    }

    /// Writes a value as Jellyfin reads and writes it back: an object's
    /// members in the order `serde_json` holds them.
    fn value(&mut self, value: &Value) {
        match value {
            Value::Null => self.raw("null"),
            Value::Bool(value) => self.raw(bool_text(*value)),
            Value::Number(number) => {
                let text = match number.as_f64() {
                    Some(real) if !number.is_i64() && !number.is_u64() => dotnet_double(real),
                    _ => number.to_string(),
                };
                self.raw(&text);
            }
            Value::String(text) => self.string(text),
            Value::Array(values) => {
                self.open('[');
                for value in values {
                    self.item();
                    self.value(value);
                }
                self.close(']');
            }
            Value::Object(members) => {
                self.open('{');
                for (name, value) in members {
                    self.key(name);
                    self.value(value);
                }
                self.close('}');
            }
        }
    }
}

/// A `double` as .NET writes it: its shortest round-trip digits, in fixed
/// notation for a decimal exponent from -5 to 14 exclusive, else in scientific
/// notation with a signed exponent of at least two digits (`1E-05`, `1E+15`).
fn dotnet_double(value: f64) -> String {
    let scientific = format!("{value:e}");
    let Some((mantissa, exponent)) = scientific.split_once('e') else {
        return scientific;
    };
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if -5 < exponent && exponent < 15 {
        return value.to_string();
    }
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}E{sign}{:02}", exponent.abs())
}

#[cfg(test)]
mod tests {
    use sqlx::{Connection as _, SqliteConnection};

    use super::{Json, date_time, dotnet_double, rewrite};
    use crate::export::DOCKER;

    const MOVIE: &str = "MediaBrowser.Controller.Entities.Movies.Movie";

    /// A database with the columns and tables the step reads.
    async fn database() -> SqliteConnection {
        let mut conn = SqliteConnection::connect("sqlite::memory:").await.unwrap();
        for statement in [
            r#"CREATE TABLE "BaseItems" ("Id" TEXT PRIMARY KEY, "Type" TEXT NOT NULL, "Data" TEXT,
                 "Size" INTEGER, "Width" INTEGER, "Height" INTEGER, "DateLastSaved" TEXT, "ParentId" TEXT)"#,
            r#"CREATE TABLE "MediaStreamInfos" ("ItemId" TEXT, "StreamIndex" INTEGER, "StreamType" INTEGER,
                 "IsExternal" INTEGER, "Path" TEXT)"#,
            r#"CREATE TABLE "LinkedChildren" ("ParentId" TEXT, "SortOrder" INTEGER, "ChildId" TEXT,
                 "ChildType" INTEGER)"#,
            r#"CREATE TABLE "BaseItemTrailerTypes" ("ItemId" TEXT, "Id" INTEGER)"#,
        ] {
            sqlx::query(statement).execute(&mut conn).await.unwrap();
        }
        conn
    }

    /// An item: id, type, `Data`, size, width, height, `DateLastSaved`, parent.
    type Item<'a> = (
        &'a str,
        &'a str,
        Option<&'a str>,
        Option<i64>,
        i64,
        i64,
        Option<&'a str>,
        Option<&'a str>,
    );

    /// Inserts `row`.
    async fn item(conn: &mut SqliteConnection, row: Item<'_>) {
        let (id, kind, data, size, width, height, saved, parent) = row;
        sqlx::query(r#"INSERT INTO "BaseItems" VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#)
            .bind(id)
            .bind(kind)
            .bind(data)
            .bind(size)
            .bind(width)
            .bind(height)
            .bind(saved)
            .bind(parent)
            .execute(&mut *conn)
            .await
            .unwrap();
    }

    async fn run(conn: &mut SqliteConnection, statements: &[&str]) {
        for statement in statements {
            sqlx::query(sqlx::AssertSqlSafe(*statement))
                .execute(&mut *conn)
                .await
                .unwrap();
        }
    }

    async fn data(conn: &mut SqliteConnection, id: &str) -> Option<String> {
        sqlx::query_scalar(r#"SELECT "Data" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(id)
            .fetch_one(conn)
            .await
            .unwrap()
    }

    /// Rows Jellyfin 12.4 wrote itself, with their streams and links, as a
    /// synthetic library's scan left them: the step writes each back byte for
    /// byte, so it holds Jellyfin's key order, initial values, escapes and
    /// date format.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one table of Jellyfin's rows, then each checked
    async fn rows_jellyfin_wrote_are_written_back_as_they_are() {
        let mut conn = database().await;
        let rows = [
            (
                "1559654E-1FB0-5780-0C66-00CA97F14DBB",
                MOVIE,
                Some(61963),
                320,
                180,
                "2026-11-26 23:44:21.9908372",
                r#"{"AdditionalParts":[],"LocalAlternateVersions":[],"LinkedAlternateVersions":[],"SubtitleFiles":["/media/movies/Caf\u00E9 \u6771\u4EAC/Amber Harbor.eng.srt"],"AudioFiles":[],"HasSubtitles":true,"IsPlaceHolder":false,"DefaultVideoStreamIndex":1,"VideoType":"VideoFile","Size":61963,"Container":"mkv","IsHD":false,"IsShortcut":false,"Width":320,"Height":180,"DateLastSaved":"2026-11-26T23:44:21.9908372Z","RemoteTrailers":[]}"#,
            ),
            (
                "2CDF752D-A332-FFEF-AD0E-FBAFAD95C844",
                MOVIE,
                Some(61963),
                320,
                180,
                "2026-11-26 23:44:37.6540559",
                r#"{"AdditionalParts":[],"LocalAlternateVersions":[],"LinkedAlternateVersions":[{"Type":"LinkedAlternateVersion","ItemId":"8757f3c2fd8a63c59c79a5d5f7d212a5"}],"SubtitleFiles":["/media/movies/Saffron Gate/Saffron Gate.eng.srt"],"AudioFiles":[],"HasSubtitles":true,"IsPlaceHolder":false,"DefaultVideoStreamIndex":1,"VideoType":"VideoFile","Size":61963,"Container":"mkv","IsHD":false,"IsShortcut":false,"Width":320,"Height":180,"DateLastSaved":"2026-11-26T23:44:37.6540559Z","RemoteTrailers":[]}"#,
            ),
            (
                "5136F4F7-6903-1423-B8DF-4F11C37566A5",
                "MediaBrowser.Controller.Entities.Video",
                Some(61963),
                320,
                180,
                "2026-11-26 23:44:35.8421541",
                r#"{"AdditionalParts":[],"LocalAlternateVersions":[],"LinkedAlternateVersions":[],"SubtitleFiles":[],"AudioFiles":[],"HasSubtitles":false,"IsPlaceHolder":false,"DefaultVideoStreamIndex":0,"VideoType":"VideoFile","Size":61963,"Container":"mkv","IsHD":false,"IsShortcut":false,"Width":320,"Height":180,"DateLastSaved":"2026-11-26T23:44:35.8421541Z","RemoteTrailers":[]}"#,
            ),
            (
                "392E82BB-4E08-424F-F31A-8831DC171DCF",
                "MediaBrowser.Controller.Entities.Audio.Audio",
                Some(39099),
                0,
                0,
                "2026-11-26 23:44:21.3995707",
                r#"{"HasLyrics":false,"LyricFiles":[],"Size":39099,"Container":"flac","IsHD":false,"IsShortcut":false,"Width":0,"Height":0,"DateLastSaved":"2026-11-26T23:44:21.3995707Z","RemoteTrailers":[]}"#,
            ),
            (
                "74C07C9A-EFAC-68FF-191E-0AF08BC90766",
                "MediaBrowser.Controller.Entities.Photo",
                Some(1829),
                320,
                240,
                "2026-11-26 23:44:21.1743207",
                r#"{"Size":1829,"IsHD":false,"IsShortcut":false,"Width":320,"Height":240,"DateLastSaved":"2026-11-26T23:44:21.1743207Z","RemoteTrailers":[]}"#,
            ),
            (
                "29066861-7012-CF1A-0D72-E5881A5D1EB9",
                "MediaBrowser.Controller.Entities.Movies.BoxSet",
                None,
                0,
                0,
                "2026-11-26 23:44:35.4029559",
                r#"{"DisplayOrder":"PremiereDate","LibraryFolderIds":["becdaa9a51902a2f8ea07296dcf217e3"],"IsRoot":false,"IsHD":false,"IsShortcut":false,"Width":0,"Height":0,"DateLastSaved":"2026-11-26T23:44:35.4029559Z","RemoteTrailers":[]}"#,
            ),
            (
                "9D7AD6AF-E9AF-A2DA-B1A2-F6E00AD28FA6",
                "MediaBrowser.Controller.Entities.CollectionFolder",
                None,
                0,
                0,
                "2026-11-26 23:44:35.2925227",
                r#"{"PhysicalLocationsList":["/config/root/default/Collections","/config/data/collections"],"PhysicalFolderIds":["8679d10569ec12981200c4116da3e90b"],"CollectionType":"boxsets","IsRoot":false,"IsHD":false,"IsShortcut":false,"Width":0,"Height":0,"DateLastSaved":"2026-11-26T23:44:35.2925227Z","RemoteTrailers":[]}"#,
            ),
            (
                "1FA53912-B8C8-1F25-3226-0E6C21A0D50A",
                "MediaBrowser.Controller.Entities.Folder",
                None,
                0,
                0,
                "2026-11-26 23:44:16.4834827",
                r#"{"IsRoot":false,"IsHD":false,"IsShortcut":false,"Width":0,"Height":0,"DateLastSaved":"2026-11-26T23:44:16.4834827Z","RemoteTrailers":[]}"#,
            ),
            (
                "E9D5075A-555C-1CBC-394E-EC4CEF295274",
                "MediaBrowser.Controller.Entities.UserRootFolder",
                None,
                0,
                0,
                "2026-11-26 23:44:35.1146128",
                r#"{"IsRoot":true,"IsHD":false,"IsShortcut":false,"Width":0,"Height":0,"DateLastSaved":"2026-11-26T23:44:35.1146128Z","RemoteTrailers":[]}"#,
            ),
            (
                "A8CC4FC3-1F7E-1144-17B0-8BE274201BC2",
                "MediaBrowser.Controller.Entities.TV.Series",
                None,
                0,
                0,
                "2026-11-26 23:44:20.766344",
                r#"{"AirDays":[],"IsRoot":false,"IsHD":false,"IsShortcut":false,"Width":0,"Height":0,"DateLastSaved":"2026-11-26T23:44:20.766344Z","RemoteTrailers":[]}"#,
            ),
            (
                "3E10ABB5-877A-C747-6AE5-7CE9FA009F33",
                "MediaBrowser.Controller.Playlists.Playlist",
                None,
                0,
                0,
                "2026-11-26 23:44:34.9863952",
                r#"{"OwnerUserId":"ae68dc6da35b403e801f2e5f2e64fb4c","OpenAccess":false,"Shares":[{"UserId":"5a29ba7614704cfbbccc184771d432d9","CanEdit":false}],"PlaylistMediaType":"Audio","IsRoot":false,"IsHD":false,"IsShortcut":false,"Width":0,"Height":0,"DateLastSaved":"2026-11-26T23:44:34.9863952Z","RemoteTrailers":[]}"#,
            ),
            (
                "357A95EF-C2E0-A819-191A-F96CAFAF17A1",
                "MediaBrowser.Controller.Entities.TV.Episode",
                Some(61963),
                320,
                180,
                "2026-11-26 23:44:21.5127237",
                r#"{"AdditionalParts":[],"LocalAlternateVersions":[],"LinkedAlternateVersions":[],"SubtitleFiles":["/media/shows/Clockwork Garden/Season 01/Clockwork Garden S01E01.eng.srt"],"AudioFiles":[],"HasSubtitles":true,"IsPlaceHolder":false,"DefaultVideoStreamIndex":1,"VideoType":"VideoFile","Size":61963,"Container":"mkv","IsHD":false,"IsShortcut":false,"Width":320,"Height":180,"DateLastSaved":"2026-11-26T23:44:21.5127237Z","RemoteTrailers":[]}"#,
            ),
            (
                "549EEBC0-AC1B-5CB0-6D37-CF122B6785D0",
                "MediaBrowser.Controller.Entities.UserView",
                None,
                0,
                0,
                "2026-11-26 23:44:40.9865348",
                r#"{"ViewType":"playlists","DisplayParentId":"1071671e7bffa0532e930debee501d2e","UserId":"ae68dc6da35b403e801f2e5f2e64fb4c","IsRoot":false,"IsHD":false,"IsShortcut":false,"Width":0,"Height":0,"DateLastSaved":"2026-11-26T23:44:40.9865348Z","RemoteTrailers":[]}"#,
            ),
        ];
        for (id, kind, size, width, height, saved, blob) in rows {
            item(
                &mut conn,
                (id, kind, Some(blob), size, width, height, Some(saved), None),
            )
            .await;
        }
        run(&mut conn, &[
            r#"INSERT INTO "MediaStreamInfos" VALUES
                 ('1559654E-1FB0-5780-0C66-00CA97F14DBB', 0, 2, 1, '/media/movies/Café 東京/Amber Harbor.eng.srt'),
                 ('1559654E-1FB0-5780-0C66-00CA97F14DBB', 1, 1, 0, NULL),
                 ('1559654E-1FB0-5780-0C66-00CA97F14DBB', 2, 0, 0, NULL),
                 ('2CDF752D-A332-FFEF-AD0E-FBAFAD95C844', 0, 2, 1, '/media/movies/Saffron Gate/Saffron Gate.eng.srt'),
                 ('2CDF752D-A332-FFEF-AD0E-FBAFAD95C844', 1, 1, 0, NULL),
                 ('5136F4F7-6903-1423-B8DF-4F11C37566A5', 0, 1, 0, NULL),
                 ('392E82BB-4E08-424F-F31A-8831DC171DCF', 0, 0, 0, NULL),
                 ('357A95EF-C2E0-A819-191A-F96CAFAF17A1', 0, 2, 1, '/media/shows/Clockwork Garden/Season 01/Clockwork Garden S01E01.eng.srt'),
                 ('357A95EF-C2E0-A819-191A-F96CAFAF17A1', 1, 1, 0, NULL)"#,
            // a playlist's and a box set's members are not versions
            r#"INSERT INTO "LinkedChildren" VALUES
                 ('2CDF752D-A332-FFEF-AD0E-FBAFAD95C844', 0, '8757F3C2-FD8A-63C5-9C79-A5D5F7D212A5', 3),
                 ('29066861-7012-CF1A-0D72-E5881A5D1EB9', 0, '1559654E-1FB0-5780-0C66-00CA97F14DBB', 0),
                 ('3E10ABB5-877A-C747-6AE5-7CE9FA009F33', 0, '392E82BB-4E08-424F-F31A-8831DC171DCF', 0)"#,
        ])
        .await;

        assert_eq!(rewrite(&mut conn, &DOCKER).await.unwrap(), 0);
        for (id, .., blob) in rows {
            assert_eq!(data(&mut conn, id).await.as_deref(), Some(blob), "{id}");
        }
    }

    /// Rows as Ferrofin's writers store them take the shape Jellyfin 12.4
    /// gives the same items when it saves them (the round trip's row diff of
    /// a native database), with the values its own scan stores: the external
    /// files and the versions from their tables, the collection name under
    /// Jellyfin's key, keys 12.4 no longer reads gone. Kinds Jellyfin keeps no
    /// `Data` for, and kinds it does not have, are left as they are; a second
    /// pass changes nothing.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one table of rows, each with its expected text
    async fn rows_ferrofin_wrote_take_the_shape_jellyfin_saves_them_in() {
        let mut conn = database().await;
        let saved = Some("2026-11-27 14:48:01.903005");
        let tail = r#""IsHD":false,"IsShortcut":false,"Width":0,"Height":0,"DateLastSaved":"2026-11-27T14:48:01.903005Z","RemoteTrailers":[]"#;
        let video_tail = r#""Size":61963,"Container":"mkv","IsHD":false,"IsShortcut":false,"Width":320,"Height":180,"DateLastSaved":"2026-11-27T14:48:01.903005Z","RemoteTrailers":[]"#;
        let movie_in = r#"{"AdditionalParts":[],"Container":"mkv","DefaultVideoStreamIndex":1,"HasSubtitles":true,"LocalAlternateVersions":[],"VideoType":"VideoFile"}"#;
        let cases = [
            ("A1", MOVIE, Some(movie_in), Some(61963), 320, 180, None,
             Some(format!(r#"{{"AdditionalParts":[],"LocalAlternateVersions":[],"LinkedAlternateVersions":[{{"Type":"LinkedAlternateVersion","ItemId":"476be447f73aab89ee4e91d8a6c8cc5c"}}],"SubtitleFiles":["/media/movies/Saffron Gate/Saffron Gate.eng.srt"],"AudioFiles":["/media/movies/Saffron Gate/Saffron Gate.fr.mka"],"HasSubtitles":true,"IsPlaceHolder":false,"DefaultVideoStreamIndex":1,"VideoType":"VideoFile",{video_tail}}}"#))),
            // a stale list of another file's subtitle, no external stream; sorted keys; a collection
            ("A2", MOVIE, Some(r#"{"AdditionalParts":[],"CollectionName":"Saga","Container":"mkv","DefaultVideoStreamIndex":0,"ExtraIds":[],"HasSubtitles":false,"LinkedChildren":[],"SubtitleFiles":["/media/other.srt"],"VideoType":"VideoFile"}"#), Some(61963), 320, 180, None,
             Some(format!(r#"{{"TmdbCollectionName":"Saga","AdditionalParts":[],"LocalAlternateVersions":[],"LinkedAlternateVersions":[],"SubtitleFiles":[],"AudioFiles":[],"HasSubtitles":false,"IsPlaceHolder":false,"DefaultVideoStreamIndex":0,"VideoType":"VideoFile",{video_tail}}}"#))),
            ("A3", "MediaBrowser.Controller.Playlists.Playlist", Some(r#"{"OpenAccess":false,"OwnerUserId":"747E95ED-3A7B-47BC-8135-E597606D4F73","PlaylistMediaType":"Video","Shares":[{"CanEdit":true,"UserId":"24b5faf9115c4796be5345382afae25f"}]}"#), None, 0, 0, None,
             Some(format!(r#"{{"OwnerUserId":"747e95ed3a7b47bc8135e597606d4f73","OpenAccess":false,"Shares":[{{"UserId":"24b5faf9115c4796be5345382afae25f","CanEdit":true}}],"PlaylistMediaType":"Video","IsRoot":false,{tail}}}"#))),
            ("A4", "MediaBrowser.Controller.Entities.Movies.BoxSet", Some(r#"{"LibraryFolderIds":["becdaa9a51902a2f8ea07296dcf217e3"]}"#), None, 0, 0, None,
             Some(format!(r#"{{"DisplayOrder":"PremiereDate","LibraryFolderIds":["becdaa9a51902a2f8ea07296dcf217e3"],"IsRoot":false,{tail}}}"#))),
            ("A5", "MediaBrowser.Controller.Entities.Folder", None, None, 0, 0, None,
             Some(format!(r#"{{"IsRoot":false,{tail}}}"#))),
            ("A6", "MediaBrowser.Controller.Entities.UserRootFolder", None, None, 0, 0, None,
             Some(format!(r#"{{"IsRoot":true,{tail}}}"#))),
            ("A7", "MediaBrowser.Controller.Entities.TV.Series", Some(r#"{"AirDays":["Monday"],"Status":"Ended"}"#), None, 0, 0, None,
             Some(format!(r#"{{"AirDays":["Monday"],"Status":"Ended","IsRoot":false,{tail}}}"#))),
            ("A8", "MediaBrowser.Controller.Entities.Audio.Audio", Some(r#"{"Container":"flac","LyricFiles":["/m/a.lrc"]}"#), Some(39099), 0, 0, None,
             Some(r#"{"HasLyrics":true,"LyricFiles":["/m/a.lrc"],"Size":39099,"Container":"flac","IsHD":false,"IsShortcut":false,"Width":0,"Height":0,"DateLastSaved":"2026-11-27T14:48:01.903005Z","RemoteTrailers":[]}"#.to_owned())),
            ("A9", "MediaBrowser.Controller.Entities.AggregateFolder", Some(r#"{"IsRoot":true,"PhysicalLocationsList":["/config/root","/media/movies"]}"#), None, 0, 0, None,
             Some(format!(r#"{{"VirtualChildren":[{{"IsHD":false,"IsShortcut":false,"Width":0,"Height":0,"DateLastSaved":"2026-11-27T14:48:00.0000000Z","RemoteTrailers":[]}}],"PhysicalLocationsList":["/config/root","/media/movies"],"IsRoot":true,{tail}}}"#))),
            ("B1", "MediaBrowser.Controller.LiveTv.LiveTvChannel", Some(r#"{"Number":"101","ChannelType":"TV","IsHD":false,"IsShortcut":false,"Width":0,"Height":0,"RemoteTrailers":[]}"#), None, 0, 0, None,
             Some(format!(r#"{{"Number":"101","ChannelType":"TV",{tail}}}"#))),
            ("B2", "MediaBrowser.Controller.Entities.Trailer", None, None, 1920, 1080, None,
             Some(r#"{"TrailerTypes":["ComingSoonToDvd","LocalTrailer"],"AdditionalParts":[],"LocalAlternateVersions":[],"LinkedAlternateVersions":[],"SubtitleFiles":["/config/metadata/library/b2/b2.en.srt"],"AudioFiles":[],"HasSubtitles":false,"IsPlaceHolder":false,"VideoType":"VideoFile","IsHD":true,"IsShortcut":false,"Width":1920,"Height":1080,"DateLastSaved":"2026-11-27T14:48:01.903005Z","RemoteTrailers":[{"Url":"https://example.org/t","Name":"Teaser"}]}"#.to_owned())),
            ("B3", "MediaBrowser.Controller.Entities.Photo", Some(r#"{"Aperture":2.0,"CameraModel":"Café's \"Pro\" <1> & 2+ `x` 📷","ExposureTime":0.00001,"IsoSpeedRating":100,"Latitude":47.25,"Orientation":"TopLeft"}"#), Some(5), 0, 0, None,
             Some(r#"{"CameraModel":"Caf\u00E9\u0027s \u0022Pro\u0022 \u003C1\u003E \u0026 2\u002B \u0060x\u0060 \uD83D\uDCF7","ExposureTime":1E-05,"Orientation":"TopLeft","Aperture":2,"Latitude":47.25,"IsoSpeedRating":100,"Size":5,"IsHD":false,"IsShortcut":false,"Width":0,"Height":0,"DateLastSaved":"0001-01-01T00:00:00.0000000Z","RemoteTrailers":[]}"#.to_owned())),
            // left as they are
            ("C1", "MediaBrowser.Controller.Entities.TV.Season", Some(r#"{"Old":1}"#), None, 0, 0, None, Some(r#"{"Old":1}"#.to_owned())),
            ("C2", "MediaBrowser.Controller.Entities.Person", None, None, 0, 0, None, None),
            ("C3", "Some.Plugin.Item", Some(r#"{"Old":1}"#), None, 0, 0, None, Some(r#"{"Old":1}"#.to_owned())),
        ];
        for (id, kind, blob, size, width, height, parent, _) in &cases {
            let when = if *id == "B3" { None } else { saved };
            item(
                &mut conn,
                (*id, *kind, *blob, *size, *width, *height, when, *parent),
            )
            .await;
        }
        item(
            &mut conn,
            (
                "P1",
                super::PLAYLISTS_FOLDER_TYPE,
                None,
                None,
                0,
                0,
                Some("2026-11-27 14:48:00"),
                Some("A9"),
            ),
        )
        .await;
        run(&mut conn, &[
            r#"INSERT INTO "MediaStreamInfos" VALUES
                 ('A1', 0, 2, 1, '/media/movies/Saffron Gate/Saffron Gate.eng.srt'),
                 ('A1', 1, 0, 1, '/media/movies/Saffron Gate/Saffron Gate.fr.mka'),
                 ('A1', 2, 1, 0, NULL), ('A1', 3, 0, 0, NULL),
                 ('A2', 0, 1, 0, NULL),
                 ('A8', 0, 0, 0, NULL), ('A8', 1, 5, 1, '/m/a.lrc'),
                 ('B2', 0, 2, 1, '%MetadataPath%/library/b2/b2.en.srt'),
                 ('B2', 1, 2, 1, '%metadatapath%/library/b2/b2.en.srt')"#,
            r#"INSERT INTO "LinkedChildren" VALUES
                 ('A1', 0, '476BE447-F73A-AB89-EE4E-91D8A6C8CC5C', 3),
                 ('A1', 1, 'C75E3CA7-0BA7-8F0A-F14C-567BD08934BF', 2)"#,
            r#"INSERT INTO "BaseItemTrailerTypes" VALUES ('B2', 5), ('B2', 2)"#,
            r#"UPDATE "BaseItems" SET "Data" = '{"RemoteTrailers":[{"Name":"Teaser","Url":"https://example.org/t"}]}'
                 WHERE "Id" = 'B2'"#,
        ])
        .await;

        assert_eq!(rewrite(&mut conn, &DOCKER).await.unwrap(), 12);
        for (id, .., expected) in &cases {
            assert_eq!(data(&mut conn, id).await, *expected, "{id}");
        }
        assert_eq!(rewrite(&mut conn, &DOCKER).await.unwrap(), 0);
    }

    /// Rows of shapes no writer means to store: `Data` that is not JSON or not
    /// an object reads as a new item's (Jellyfin's deserialiser fails it), a
    /// JSON null as none, Ferrofin's `CollectionName` before a stale
    /// `TmdbCollectionName`, the nil `Guid` as Jellyfin writes it for a
    /// `Guid` (zeros) and a `Guid?` (null); ids in other cases still find
    /// their streams and links; and a database without the stream, link and
    /// trailer-type tables shapes its rows from `BaseItems` alone.
    #[tokio::test]
    async fn rows_of_odd_shapes_are_read_as_jellyfin_reads_them() {
        let tail = r#""IsHD":false,"IsShortcut":false,"Width":0,"Height":0,"DateLastSaved":"0001-01-01T00:00:00.0000000Z","RemoteTrailers":[]"#;
        let mut conn = database().await;
        let cases = [
            (
                "D1",
                "MediaBrowser.Controller.Entities.Folder",
                "not json",
                format!(r#"{{"IsRoot":false,{tail}}}"#),
            ),
            (
                "D2",
                "MediaBrowser.Controller.Entities.Folder",
                "[1]",
                format!(r#"{{"IsRoot":false,{tail}}}"#),
            ),
            (
                "D3",
                "MediaBrowser.Controller.Entities.Audio.Audio",
                r#"{"Container":null,"HasLyrics":true}"#,
                format!(r#"{{"HasLyrics":true,"LyricFiles":[],{tail}}}"#),
            ),
            (
                "D4",
                MOVIE,
                r#"{"TmdbCollectionName":"Old","CollectionName":"New"}"#,
                format!(
                    r#"{{"TmdbCollectionName":"New","AdditionalParts":[],"LocalAlternateVersions":[],"LinkedAlternateVersions":[{{"Type":"LinkedAlternateVersion","ItemId":"476be447f73aab89ee4e91d8a6c8cc5c"}}],"SubtitleFiles":["/m/d4.srt"],"AudioFiles":[],"HasSubtitles":false,"IsPlaceHolder":false,"VideoType":"VideoFile",{tail}}}"#
                ),
            ),
            (
                "D5",
                "MediaBrowser.Controller.Playlists.Playlist",
                r#"{"OwnerUserId":"00000000-0000-0000-0000-000000000000"}"#,
                format!(
                    r#"{{"OwnerUserId":"00000000000000000000000000000000","OpenAccess":false,"Shares":[],"PlaylistMediaType":"Unknown","IsRoot":false,{tail}}}"#
                ),
            ),
            (
                "D6",
                "MediaBrowser.Controller.Entities.UserView",
                r#"{"UserId":"00000000000000000000000000000000","ViewType":"movies"}"#,
                format!(
                    r#"{{"ViewType":"movies","DisplayParentId":"00000000000000000000000000000000","UserId":null,"IsRoot":false,{tail}}}"#
                ),
            ),
        ];
        for (id, kind, blob, _) in &cases {
            item(&mut conn, (*id, *kind, Some(*blob), None, 0, 0, None, None)).await;
        }
        run(&mut conn, &[
            r#"INSERT INTO "MediaStreamInfos" VALUES ('d4', 0, 2, 1, '/m/d4.srt')"#,
            r#"INSERT INTO "LinkedChildren" VALUES ('d4', 0, '476be447-f73a-ab89-ee4e-91d8a6c8cc5c', 3)"#,
        ])
        .await;
        assert_eq!(rewrite(&mut conn, &DOCKER).await.unwrap(), 6);
        for (id, .., expected) in &cases {
            assert_eq!(
                data(&mut conn, id).await.as_deref(),
                Some(expected.as_str()),
                "{id}"
            );
        }

        let mut bare = SqliteConnection::connect("sqlite::memory:").await.unwrap();
        run(&mut bare, &[
            r#"CREATE TABLE "BaseItems" ("Id" TEXT PRIMARY KEY, "Type" TEXT NOT NULL, "Data" TEXT,
                 "Size" INTEGER, "Width" INTEGER, "Height" INTEGER, "DateLastSaved" TEXT, "ParentId" TEXT)"#,
            r#"INSERT INTO "BaseItems" VALUES ('E1', 'MediaBrowser.Controller.Entities.Video', NULL, NULL, 0, 0, NULL, NULL)"#,
        ])
        .await;
        assert_eq!(rewrite(&mut bare, &DOCKER).await.unwrap(), 1);
        assert_eq!(
            data(&mut bare, "E1").await.as_deref(),
            Some(format!(r#"{{"AdditionalParts":[],"LocalAlternateVersions":[],"LinkedAlternateVersions":[],"SubtitleFiles":[],"AudioFiles":[],"HasSubtitles":false,"IsPlaceHolder":false,"VideoType":"VideoFile",{tail}}}"#).as_str())
        );
    }

    /// A database without the columns the step reads is left as it is.
    #[tokio::test]
    async fn a_database_without_the_columns_is_left_alone() {
        let mut conn = SqliteConnection::connect("sqlite::memory:").await.unwrap();
        assert_eq!(rewrite(&mut conn, &DOCKER).await.unwrap(), 0);
        run(&mut conn, &[
            r#"CREATE TABLE "BaseItems" ("Type" TEXT, "Path" TEXT, "Data" TEXT)"#,
            r#"INSERT INTO "BaseItems" VALUES ('MediaBrowser.Controller.Entities.Folder', '/x', NULL)"#,
        ])
        .await;
        assert_eq!(rewrite(&mut conn, &DOCKER).await.unwrap(), 0);
    }

    #[test]
    fn dates_are_written_as_jellyfins_converter_writes_them() {
        for (stored, written) in [
            (None, "0001-01-01T00:00:00.0000000Z"),
            (
                Some("2026-11-27 14:48:01.903005"),
                "2026-11-27T14:48:01.903005Z",
            ),
            (Some("2026-11-27 14:48:01.9"), "2026-11-27T14:48:01.9Z"),
            // milliseconds 0: all seven digits (JsonDateTimeConverter)
            (
                Some("2026-11-27 14:48:01.0001234"),
                "2026-11-27T14:48:01.0001234Z",
            ),
            (Some("2026-11-27 14:48:01"), "2026-11-27T14:48:01.0000000Z"),
            (Some("2026-11-27T14:48:01.5Z"), "2026-11-27T14:48:01.5Z"),
            (Some("not a date"), "not a date"),
        ] {
            assert_eq!(date_time(stored), written, "{stored:?}");
        }
    }

    #[test]
    fn doubles_are_written_as_dotnet_writes_them() {
        for (value, written) in [
            (2.0, "2"),
            (0.0, "0"),
            (-1.5, "-1.5"),
            (0.0001, "0.0001"),
            (0.00001, "1E-05"),
            (0.000_015, "1.5E-05"),
            (1e14, "100000000000000"),
            (1e15, "1E+15"),
            (1.234_567_8e20, "1.2345678E+20"),
        ] {
            assert_eq!(dotnet_double(value), written, "{value}");
        }
    }

    #[test]
    fn strings_take_the_default_encoders_escapes() {
        let mut json = Json::default();
        json.string("a\\b\n\r\t\u{8}\u{c}\u{1}\u{7f}é\"/");
        assert_eq!(json.text, r#""a\\b\n\r\t\b\f\u0001\u007F\u00E9\u0022/""#);
    }
}
