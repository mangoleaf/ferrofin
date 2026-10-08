//! A video's version group and the media sources it lists.
//!
//! Jellyfin keeps a video's versions as relations of the `Video` object
//! (`Video.cs`): its local alternate versions (`LinkedChildren` type 2, the
//! files the resolver grouped beside it) and its linked alternate versions
//! (type 3, merged by hand), and every member that reads the group goes
//! through `GetAllItemsForMediaSources` (`Video.cs:845-885`): the media
//! sources, the version count, played-state propagation and the version a
//! media source id names.
//!
//! Ferrofin stores the same relations as row pointers: every version names
//! its primary in `PrimaryVersionId`, and a LOCAL version is also owned by
//! it (`OwnerId` = primary, no `ExtraType`, the scan's
//! `sync_local_versions` keeps the type-2 links and these pointers in
//! step). So the group is read from rows, batched: [`VersionRows::load`]
//! reads every row a set of queried items' groups can reach — one query
//! for the versions pointing at them, one more only when a queried item is
//! itself a version whose primary is not among them, and a third only when
//! a version merged into a group lists local versions of its own — and
//! [`VersionRows::items_for_media_sources`] composes one item's group from
//! them without another round trip.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::dto::{MediaSourceInfo, MediaSourceType};
use ferrofin_model::entities::VideoType;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::persistence::ItemRepository;
use uuid::Uuid;

/// The delimiters a version label is cut at (`BaseItem.VersionDelimiters`,
/// `BaseItem.cs:95`).
const VERSION_DELIMITERS: [char; 3] = ['-', '_', '.'];

/// The `Data` key holding a stacked video's other parts.
const ADDITIONAL_PARTS: &str = "AdditionalParts";

/// The `Data` key holding a primary's local alternate version paths.
const LOCAL_ALTERNATE_VERSIONS: &str = "LocalAlternateVersions";

fn parse_id(id: Option<&str>) -> Option<Uuid> {
    id.and_then(|s| Uuid::parse_str(s).ok())
}

fn row_id(row: &BaseItemEntity) -> Uuid {
    Uuid::parse_str(&row.id).unwrap_or_else(|_| Uuid::nil())
}

/// A path list stored in a video's `Data` (`Video.AdditionalParts`,
/// `Video.LocalAlternateVersions`); absent, `null` or not a list reads as
/// the empty array C# initialises it to.
fn data_paths(data: Option<&str>, key: &str) -> Vec<String> {
    crate::item_data::parse_data(data)
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A stacked video's other parts' paths (`Video.AdditionalParts`).
#[must_use]
pub fn additional_parts(data: Option<&str>) -> Vec<String> {
    data_paths(data, ADDITIONAL_PARTS)
}

/// How many other parts a stacked video has — the DTO's `PartCount` reads
/// it for every video on a page.
///
/// Every video the scan stores carries the key, nearly always as `[]`, so a
/// blob whose (first) `AdditionalParts` is absent, empty or `null` is read
/// off the text without a JSON parse; only a stacked video's is parsed.
#[must_use]
pub fn additional_part_count(data: Option<&str>) -> usize {
    let Some(text) = data else {
        return 0;
    };
    let Some(at) = text.find("\"AdditionalParts\"") else {
        return 0;
    };
    let value = text[at + "\"AdditionalParts\"".len()..]
        .trim_start()
        .strip_prefix(':')
        .map(str::trim_start)
        .unwrap_or_default();
    let empty = value.starts_with("null")
        || value
            .strip_prefix('[')
            .is_some_and(|rest| rest.trim_start().starts_with(']'));
    if empty {
        return 0;
    }
    crate::item_data::parse_data(data)
        .get(ADDITIONAL_PARTS)
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len)
}

/// Whether a video's `Data` names other parts or local alternate versions
/// — the videos it owns and copies its metadata to.
#[must_use]
pub fn owns_videos(data: Option<&str>) -> bool {
    additional_part_count(data) > 0 || !data_paths(data, LOCAL_ALTERNATE_VERSIONS).is_empty()
}

/// Whether `row` is one of `primary`'s LOCAL alternate versions — a file
/// the resolver grouped with it (`LibraryManager.ResolveAlternateVersion`
/// gives it `OwnerId` = `PrimaryVersionId` = the primary), as opposed to a
/// version merged onto it, which no item owns.
#[must_use]
pub fn is_local_version_of(row: &BaseItemEntity, primary: Uuid) -> bool {
    row.extra_type.is_none()
        && parse_id(row.owner_id.as_deref()) == Some(primary)
        && parse_id(row.primary_version_id.as_deref()) == Some(primary)
}

/// The two reads a version group needs, so each caller can serve them from
/// the handle it already holds.
#[async_trait]
pub trait VersionRowReader: Send + Sync {
    /// The rows with the given ids (any order; missing ids are skipped).
    ///
    /// # Errors
    ///
    /// [`ServiceError`] on a storage failure.
    async fn rows_by_id(&self, ids: &[Uuid]) -> Result<Vec<BaseItemEntity>, ServiceError>;

    /// The rows naming each id as their `PrimaryVersionId`, by that id.
    ///
    /// # Errors
    ///
    /// [`ServiceError`] on a storage failure.
    async fn rows_by_primary(
        &self,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<BaseItemEntity>>, ServiceError>;
}

/// [`VersionRowReader`] over an [`ItemRepository`].
pub struct RepositoryVersionReader<'a>(pub &'a dyn ItemRepository);

#[async_trait]
impl VersionRowReader for RepositoryVersionReader<'_> {
    async fn rows_by_id(&self, ids: &[Uuid]) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.0.retrieve_items(ids).await
    }

    async fn rows_by_primary(
        &self,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<BaseItemEntity>>, ServiceError> {
        self.0.get_items_by_primary_version_batch(ids).await
    }
}

/// [`VersionRowReader`] straight over the database.
pub struct DbVersionReader<'a>(pub &'a Database);

#[async_trait]
impl VersionRowReader for DbVersionReader<'_> {
    async fn rows_by_id(&self, ids: &[Uuid]) -> Result<Vec<BaseItemEntity>, ServiceError> {
        crate::item_repository::select_item_rows(self.0, r#""Id""#, ids).await
    }

    async fn rows_by_primary(
        &self,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<BaseItemEntity>>, ServiceError> {
        let mut map: HashMap<Uuid, Vec<BaseItemEntity>> = HashMap::new();
        for row in
            crate::item_repository::select_item_rows(self.0, r#""PrimaryVersionId""#, ids).await?
        {
            if let Some(primary) = parse_id(row.primary_version_id.as_deref())
                && primary != row_id(&row)
            {
                map.entry(primary).or_default().push(row);
            }
        }
        Ok(map)
    }
}

/// Every row the version groups of a set of queried items reach, read once.
#[derive(Debug, Default)]
pub struct VersionRows {
    /// The rows by id (a queried item another queried item's group reaches
    /// is copied in too).
    rows: HashMap<Uuid, BaseItemEntity>,
    /// The ids of the rows naming each id as their `PrimaryVersionId`, in
    /// read order.
    by_primary: HashMap<Uuid, Vec<Uuid>>,
    /// Each video with local versions: its `LocalAlternateVersions` paths,
    /// parsed once at load, which order them.
    local_order: HashMap<Uuid, Vec<String>>,
}

impl VersionRows {
    /// Reads the rows `queried`'s version groups reach:
    ///
    /// 1. the primary of each queried item that is a version (only when one
    ///    is, and only those not queried themselves);
    /// 2. the versions pointing at each queried item and each such primary;
    /// 3. the local versions of each version found that is merged rather
    ///    than local (`GetAllItemsForMediaSources` takes the local versions
    ///    of every grouped item) — only when such a version's `Data` lists
    ///    local versions.
    ///
    /// # Errors
    ///
    /// [`ServiceError`] when a read fails.
    pub async fn load(
        reader: &dyn VersionRowReader,
        queried: &[&BaseItemEntity],
    ) -> Result<Self, ServiceError> {
        let mut out = Self::default();
        if queried.is_empty() {
            return Ok(out);
        }
        let by_id: HashMap<Uuid, &BaseItemEntity> =
            queried.iter().map(|row| (row_id(row), *row)).collect();
        let mut keys: Vec<Uuid> = by_id.keys().copied().collect();
        let mut wanted: Vec<Uuid> = Vec::new();
        for row in queried {
            let Some(primary) = parse_id(row.primary_version_id.as_deref()) else {
                continue;
            };
            if primary == row_id(row) || out.rows.contains_key(&primary) {
                continue;
            }
            if let Some(stored) = by_id.get(&primary) {
                out.rows.insert(primary, (*stored).clone());
            } else if !wanted.contains(&primary) {
                wanted.push(primary);
            }
        }
        if !wanted.is_empty() {
            for row in reader.rows_by_id(&wanted).await? {
                let id = row_id(&row);
                keys.push(id);
                out.rows.insert(id, row);
            }
        }
        out.absorb(reader.rows_by_primary(&keys).await?, |_, _| true);
        let known: HashSet<Uuid> = keys.iter().copied().collect();
        let mut merged: Vec<Uuid> = out
            .by_primary
            .iter()
            .flat_map(|(primary, ids)| {
                ids.iter()
                    .filter(|id| {
                        out.rows.get(id).is_some_and(|row| {
                            !is_local_version_of(row, *primary)
                                && lists_local_versions(row.data.as_deref())
                        })
                    })
                    .copied()
                    .collect::<Vec<_>>()
            })
            .filter(|id| !known.contains(id))
            .collect();
        merged.sort_unstable();
        merged.dedup();
        if !merged.is_empty() {
            let found = reader.rows_by_primary(&merged).await?;
            out.absorb(found, is_local_version_of);
        }
        // The link order of each video's local versions, its `Data` read
        // once here rather than at every use.
        let owners: Vec<Uuid> = out
            .by_primary
            .iter()
            .filter(|(primary, ids)| {
                ids.iter().any(|id| {
                    out.rows
                        .get(id)
                        .is_some_and(|row| is_local_version_of(row, **primary))
                })
            })
            .map(|(primary, _)| *primary)
            .collect();
        for owner in owners {
            let data = by_id
                .get(&owner)
                .map(|row| row.data.as_deref())
                .or_else(|| out.rows.get(&owner).map(|row| row.data.as_deref()));
            if let Some(data) = data {
                out.local_order
                    .insert(owner, data_paths(data, LOCAL_ALTERNATE_VERSIONS));
            }
        }
        Ok(out)
    }

    fn absorb(
        &mut self,
        found: HashMap<Uuid, Vec<BaseItemEntity>>,
        keep: impl Fn(&BaseItemEntity, Uuid) -> bool,
    ) {
        for (primary, rows) in found {
            for row in rows {
                if !keep(&row, primary) {
                    continue;
                }
                let id = row_id(&row);
                let list = self.by_primary.entry(primary).or_default();
                if !list.contains(&id) {
                    list.push(id);
                }
                self.rows.entry(id).or_insert(row);
            }
        }
    }

    /// How many rows name `primary` as their `PrimaryVersionId` — its local
    /// and linked versions together (`Video.GetMediaSourceCount` minus one).
    #[must_use]
    pub fn version_count(&self, primary: Uuid) -> usize {
        self.by_primary.get(&primary).map_or(0, Vec::len)
    }

    /// The ids of every row read (each of them some queried item's version
    /// or primary).
    pub fn ids(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.rows.keys().copied()
    }

    fn row<'a>(&'a self, id: Uuid, queried: &'a BaseItemEntity) -> Option<&'a BaseItemEntity> {
        if id == row_id(queried) {
            Some(queried)
        } else {
            self.rows.get(&id)
        }
    }

    fn versions_of<'a>(
        &'a self,
        video: &'a BaseItemEntity,
        queried: &'a BaseItemEntity,
    ) -> impl Iterator<Item = &'a BaseItemEntity> + 'a {
        self.by_primary
            .get(&row_id(video))
            .into_iter()
            .flatten()
            .filter_map(move |id| self.row(*id, queried))
    }

    /// `video`'s local alternate versions — `LibraryManager.
    /// GetLocalAlternateVersionIds` (`LibraryManager.cs:2297-2308`): its
    /// type-2 links, ordered as the scan writes them, which is the order of
    /// its `LocalAlternateVersions` paths.
    fn local_alternates<'a>(
        &'a self,
        video: &'a BaseItemEntity,
        queried: &'a BaseItemEntity,
    ) -> Vec<&'a BaseItemEntity> {
        let id = row_id(video);
        let mut locals: Vec<&BaseItemEntity> = self
            .versions_of(video, queried)
            .filter(|row| is_local_version_of(row, id))
            .collect();
        let Some(order) = self.local_order.get(&id).filter(|_| locals.len() > 1) else {
            return locals;
        };
        locals.sort_by_key(|row| {
            row.path
                .as_deref()
                .and_then(|path| order.iter().position(|p| p == path))
                .unwrap_or(usize::MAX)
        });
        locals
    }

    /// The ids of `video`'s local alternate versions, in their link order
    /// (`LibraryManager.GetLocalAlternateVersionIds`).
    #[must_use]
    pub fn local_version_ids(&self, video: &BaseItemEntity) -> Vec<Uuid> {
        self.local_alternates(video, video)
            .into_iter()
            .map(row_id)
            .collect()
    }

    /// `video`'s linked (merged) alternate versions — `LibraryManager.
    /// GetLinkedAlternateVersions` (`LibraryManager.cs:2311-2325`), ordered
    /// by `SortName`.
    fn linked_alternates<'a>(
        &'a self,
        video: &'a BaseItemEntity,
        queried: &'a BaseItemEntity,
    ) -> Vec<&'a BaseItemEntity> {
        let id = row_id(video);
        let mut linked: Vec<&BaseItemEntity> = self
            .versions_of(video, queried)
            .filter(|row| !is_local_version_of(row, id))
            .collect();
        linked.sort_by(|a, b| a.sort_name.cmp(&b.sort_name));
        linked
    }

    /// The items `queried`'s media sources come from, each once — port of
    /// `Video.GetAllItemsForMediaSources` (`Video.cs:845-885`): the video
    /// itself, its linked versions and — when it is itself a version — its
    /// primary and the primary's linked versions, then the local versions of
    /// every one of those. A linked version is a `Grouping` (user-merged,
    /// splittable) source, and so is the primary when `queried` is linked
    /// onto it; everything else is a `Default` source.
    #[must_use]
    pub fn items_for_media_sources<'a>(
        &'a self,
        queried: &'a BaseItemEntity,
    ) -> Vec<(&'a BaseItemEntity, MediaSourceType)> {
        let id = row_id(queried);
        let primary = parse_id(queried.primary_version_id.as_deref())
            .filter(|p| *p != id)
            .and_then(|p| self.row(p, queried));
        let primary_linked = primary.map_or_else(Vec::new, |p| self.linked_alternates(p, queried));
        let primary_type = if primary_linked.iter().any(|row| row_id(row) == id) {
            MediaSourceType::Grouping
        } else {
            MediaSourceType::Default
        };
        let mut grouped: Vec<(&BaseItemEntity, MediaSourceType)> =
            vec![(queried, MediaSourceType::Default)];
        grouped.extend(
            self.linked_alternates(queried, queried)
                .into_iter()
                .map(|row| (row, MediaSourceType::Grouping)),
        );
        if let Some(primary) = primary {
            grouped.push((primary, primary_type));
            grouped.extend(
                primary_linked
                    .into_iter()
                    .map(|row| (row, MediaSourceType::Grouping)),
            );
        }
        let locals: Vec<(&BaseItemEntity, MediaSourceType)> = grouped
            .iter()
            .flat_map(|(row, _)| self.local_alternates(row, queried))
            .map(|row| (row, MediaSourceType::Default))
            .collect();
        let mut seen = HashSet::new();
        grouped
            .into_iter()
            .chain(locals)
            .filter(|(row, _)| seen.insert(row_id(row)))
            .collect()
    }

    /// The ids of every version of `queried`, itself included — `Video.
    /// GetAllVersions` (`Video.cs:416-422`).
    #[must_use]
    pub fn all_version_ids(&self, queried: &BaseItemEntity) -> Vec<Uuid> {
        self.items_for_media_sources(queried)
            .into_iter()
            .map(|(row, _)| row_id(row))
            .collect()
    }

    /// The version of `queried` a media source id names, or `None` when it
    /// is not one of its versions — `Video.GetAlternateVersion`
    /// (`Video.cs:429-432`).
    #[must_use]
    pub fn alternate_version<'a>(
        &'a self,
        queried: &'a BaseItemEntity,
        item_id: Uuid,
    ) -> Option<&'a BaseItemEntity> {
        self.items_for_media_sources(queried)
            .into_iter()
            .map(|(row, _)| row)
            .find(|row| row_id(row) == item_id)
    }

    /// The media sources `queried` lists, ready to serve: one per item of
    /// [`Self::items_for_media_sources`], each built by `build`, named
    /// ([`media_source_name`], over the group's common prefix) and typed,
    /// then ordered as `BaseItem.GetMediaSources` orders them
    /// ([`order_media_sources`]).
    pub fn media_sources(
        &self,
        queried: &BaseItemEntity,
        mut build: impl FnMut(&BaseItemEntity) -> MediaSourceInfo,
    ) -> Vec<MediaSourceInfo> {
        let group = self.items_for_media_sources(queried);
        if group.len() < 2 {
            // No versions: its own source, named as `build` named it.
            return group.into_iter().map(|(row, _)| build(row)).collect();
        }
        let rows: Vec<&BaseItemEntity> = group.iter().map(|(row, _)| *row).collect();
        let prefix = common_name_prefix(&rows);
        // `HasLocalAlternateVersions` → the folder-name fallback.
        let folder = (!self.local_alternates(queried, queried).is_empty())
            .then(|| containing_folder_name(queried));
        let mut sources: Vec<MediaSourceInfo> = group
            .iter()
            .map(|(row, source_type)| {
                let mut source = build(row);
                source.name = Some(media_source_name(folder.as_deref(), row, prefix.as_deref()));
                source.type_ = *source_type;
                source
            })
            .collect();
        order_media_sources(&mut sources, row_id(queried));
        sources
    }
}

/// Puts the version the user is part-way through first — port of
/// `MediaSourceManager.SetAlternateVersionResumeStates` (`MediaSourceManager.cs:
/// 436-473`) over `VersionPlaybackSelector.SelectMostRecentlyPlayed`: among
/// the sources whose user data has a resume point, the one played last (the
/// first on a tie) leads, so resuming a primary without naming a source
/// plays the version last watched. A completed version has no resume point
/// and is not moved; a queried version (`PrimaryVersionId` set) keeps its own
/// source first, as does a video with one source.
pub fn put_resumed_version_first<'a>(
    queried: &BaseItemEntity,
    sources: &mut [MediaSourceInfo],
    user_data_of: impl Fn(Uuid) -> Option<&'a ferrofin_model::dto::UserItemDataDto>,
) {
    if sources.len() < 2 || queried.primary_version_id.is_some() {
        return;
    }
    let mut winner: Option<(usize, chrono::DateTime<chrono::Utc>)> = None;
    for (index, source) in sources.iter().enumerate() {
        let Some(user_data) = source
            .id
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok())
            .and_then(&user_data_of)
        else {
            continue;
        };
        if user_data.playback_position_ticks <= 0 {
            continue;
        }
        let date = user_data
            .last_played_date
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC);
        if winner.is_none_or(|(_, best)| date > best) {
            winner = Some((index, date));
        }
    }
    if let Some((index, _)) = winner {
        sources[..=index].rotate_right(1);
    }
}

/// Orders the sources `BaseItem.GetMediaSources` returns (`BaseItem.cs:
/// 1177-1188`): the queried item's own source first, so it is the default a
/// client plays, then video files before discs and 2D before 3D.
///
/// The last key, `ThenByDescending(i => i, new MediaSourceWidthComparator())`,
/// compares widths only between two sources with the same path
/// (`MediaSourceWidthComparator.cs:31-52`) and calls every other pair equal.
/// Every source here is a different row, and two rows with one path are
/// never in one group, so it orders nothing; it is left out because it is
/// not a total order, which a Rust sort requires.
pub fn order_media_sources(sources: &mut [MediaSourceInfo], queried: Uuid) {
    let own = queried.simple().to_string();
    sources.sort_by_key(|source| {
        (
            !source
                .id
                .as_deref()
                .is_some_and(|id| id.eq_ignore_ascii_case(&own)),
            source.video_type != Some(VideoType::VideoFile),
            source.video3d_format.is_some(),
        )
    });
}

/// The file name without its extension (`Path.GetFileNameWithoutExtension`).
fn file_stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The folder a video lives in (`Video.ContainingFolderPath`, `Video.cs:
/// 201-219`): the parent directory of a file — of the first part for a
/// stacked one — and the path itself for a disc folder.
fn containing_folder_path(video: &BaseItemEntity) -> String {
    let path = video.path.as_deref().unwrap_or_default();
    let data = crate::item_data::parse_data(video.data.as_deref());
    let stacked = data
        .get(ADDITIONAL_PARTS)
        .and_then(serde_json::Value::as_array)
        .is_some_and(|parts| !parts.is_empty());
    let disc = matches!(
        crate::item_data::read_data_string(&data, "VideoType").as_deref(),
        Some("Dvd" | "BluRay")
    );
    if !stacked && (disc || video.is_folder) {
        return path.to_owned();
    }
    std::path::Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The name of the folder a video lives in (`Path.GetFileName(
/// ContainingFolderPath)`).
fn containing_folder_name(video: &BaseItemEntity) -> String {
    std::path::Path::new(&containing_folder_path(video))
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Whether a video's `Data` lists local alternate versions — read off the
/// text first, so a blob without the key (or with `[]`) is not parsed.
fn lists_local_versions(data: Option<&str>) -> bool {
    data.is_some_and(|text| text.contains("\"LocalAlternateVersions\":[\""))
        || (data.is_some_and(|text| text.contains("\"LocalAlternateVersions\""))
            && !data_paths(data, LOCAL_ALTERNATE_VERSIONS).is_empty())
}

/// .NET `char.ToUpperInvariant` for the simple one-to-one mappings.
fn upper(c: char) -> char {
    let mut up = c.to_uppercase();
    match (up.next(), up.next()) {
        (Some(u), None) => u,
        _ => c,
    }
}

/// `text.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)`, on chars.
fn starts_with_ignore_case(text: &[char], prefix: &[char]) -> bool {
    text.len() >= prefix.len() && text.iter().zip(prefix).all(|(a, b)| upper(*a) == upper(*b))
}

/// The label left after `prefix`: `displayName.AsSpan(prefix.Length)
/// .TrimStart([' ', .. VersionDelimiters])`, unless it is blank.
fn label_after(display: &[char], prefix: &[char]) -> Option<String> {
    if display.len() <= prefix.len() || !starts_with_ignore_case(display, prefix) {
        return None;
    }
    let label: String = display[prefix.len()..]
        .iter()
        .skip_while(|c| **c == ' ' || VERSION_DELIMITERS.contains(c))
        .collect();
    (!label.trim().is_empty()).then_some(label)
}

/// The name a version's media source shows — port of `BaseItem.
/// GetMediaSourceName` (`BaseItem.cs:1289-1362`), called on the queried
/// video for `item`, one of its sources. `queried_folder` is the name of the
/// queried video's folder when it has local versions
/// (`HasLocalAlternateVersions`), else `None`.
///
/// A file names its source after the part of its file name that differs
/// from the group's (`common_prefix`); failing that, when the queried video
/// has local versions, after the part that follows its folder's name; failing that,
/// after the whole file name. A source with no file is named after the
/// item.
///
/// The `3D`/`Bluray`/`DVD`/`ISO` terms upstream appends read
/// `Video3DFormat`, `VideoType` and `IsoType`; every source here is built as
/// a 2D `VideoFile` ([`crate::media_source_manager`]), for which upstream
/// appends none either.
#[must_use]
pub fn media_source_name(
    queried_folder: Option<&str>,
    item: &BaseItemEntity,
    common_prefix: Option<&str>,
) -> String {
    let path = item.path.as_deref().filter(|p| !p.is_empty());
    let Some(path) = path.filter(|p| crate::media_info_resolver::is_file_protocol(p)) else {
        return item.name.clone().unwrap_or_default();
    };
    let display: Vec<char> = file_stem(path).chars().collect();
    if let Some(prefix) = common_prefix.filter(|p| !p.is_empty()) {
        let prefix: Vec<char> = prefix.chars().collect();
        if let Some(label) = label_after(&display, &prefix) {
            return label;
        }
    }
    if let Some(folder) = queried_folder {
        let folder: Vec<char> = folder.chars().collect();
        if let Some(label) = label_after(&display, &folder) {
            return label;
        }
    }
    display.into_iter().collect()
}

/// The prefix the media source items' file names share, or `None` when
/// fewer than two of them are files or they share none — port of
/// `BaseItem.GetCommonNamePrefix` (`BaseItem.cs:1386-1405`).
#[must_use]
pub fn common_name_prefix(items: &[&BaseItemEntity]) -> Option<String> {
    let names: Vec<String> = items
        .iter()
        .filter_map(|item| item.path.as_deref())
        .filter(|p| !p.is_empty() && crate::media_info_resolver::is_file_protocol(p))
        .map(file_stem)
        .collect();
    if names.len() < 2 {
        return None;
    }
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    Some(common_version_prefix(&refs)).filter(|p| !p.is_empty())
}

/// The case-insensitive longest common prefix of the version file names,
/// retreated to the last delimiter boundary — port of `BaseItem.
/// GetCommonVersionPrefix` (`BaseItem.cs:1419-1468`).
///
/// It retreats to a structural delimiter (`-`, `_`, `.`), skipping a dot
/// that is a decimal point, and only when there is none to a space; it does
/// not retreat when the prefix is itself one of the names (that version is
/// the unlabelled base name). `file_names` must not be empty.
#[must_use]
pub fn common_version_prefix(file_names: &[&str]) -> String {
    let names: Vec<Vec<char>> = file_names.iter().map(|n| n.chars().collect()).collect();
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut prefix: Vec<char> = first.clone();
    for name in &names[1..] {
        if prefix.is_empty() {
            break;
        }
        let common = prefix
            .iter()
            .zip(name)
            .take_while(|(a, b)| upper(**a) == upper(**b))
            .count();
        prefix.truncate(common);
    }
    let whole_name = names.iter().any(|n| n.len() == prefix.len());
    if !whole_name {
        let mut cut = prefix.len();
        while cut > 0
            && (!VERSION_DELIMITERS.contains(&prefix[cut - 1])
                || is_decimal_point(&prefix, cut - 1, &names))
        {
            cut -= 1;
        }
        if cut == 0 {
            cut = prefix.len();
            while cut > 0 && prefix[cut - 1] != ' ' {
                cut -= 1;
            }
        }
        prefix.truncate(cut);
    }
    prefix.into_iter().collect()
}

/// Whether the dot at `index` is a decimal point inside a number rather
/// than a delimiter (`BaseItem.IsDecimalPoint`, `BaseItem.cs:1473-1495`).
/// .NET's `char.IsDigit` is the Unicode `Nd` category; version labels are
/// ASCII digits.
fn is_decimal_point(prefix: &[char], index: usize, names: &[Vec<char>]) -> bool {
    if index == 0 || prefix[index] != '.' || !prefix[index - 1].is_ascii_digit() {
        return false;
    }
    if index + 1 < prefix.len() {
        return prefix[index + 1].is_ascii_digit();
    }
    names
        .iter()
        .all(|name| name.get(index + 1).is_some_and(char::is_ascii_digit))
}

/// Gives an owned item its owner's production year and premiere date where
/// the owner has them, returning whether anything changed — port of
/// `BaseItem.InheritDatesFromOwner` (`BaseItem.cs:2776-2794`).
pub fn inherit_dates_from_owner(owner: &BaseItemEntity, item: &mut BaseItemEntity) -> bool {
    let mut changed = false;
    if owner.production_year.is_some() && item.production_year != owner.production_year {
        item.production_year = owner.production_year;
        changed = true;
    }
    if owner.premiere_date.is_some() && item.premiere_date != owner.premiere_date {
        item.premiere_date = owner.premiere_date;
        changed = true;
    }
    changed
}

/// Gives an owned item "some data from the main item, for querying
/// purposes", returning whether anything changed — the `copyTitleMetadata`
/// branch of `BaseItem.RefreshMetadataForOwnedItem` (`BaseItem.cs:
/// 2805-2858`), which a primary's refresh runs for each of its stacked parts
/// (`Video.RefreshedOwnedItems`, `Video.cs:549-555`): genres, studios,
/// production locations, community and critic rating, overview, official and
/// custom rating, then the owner's dates ([`inherit_dates_from_owner`]).
/// The lists are stored `|`-joined, so comparing the text compares them in
/// order, as `SequenceEqual` does.
pub fn copy_title_metadata(owner: &BaseItemEntity, item: &mut BaseItemEntity) -> bool {
    let mut changed = false;
    for (from, to) in [
        (&owner.genres, &mut item.genres),
        (&owner.studios, &mut item.studios),
        (&owner.production_locations, &mut item.production_locations),
        (&owner.overview, &mut item.overview),
        (&owner.official_rating, &mut item.official_rating),
        (&owner.custom_rating, &mut item.custom_rating),
    ] {
        if to != from {
            to.clone_from(from);
            changed = true;
        }
    }
    for (from, to) in [
        (owner.community_rating, &mut item.community_rating),
        (owner.critic_rating, &mut item.critic_rating),
    ] {
        if *to != from {
            *to = from;
            changed = true;
        }
    }
    inherit_dates_from_owner(owner, item) || changed
}

/// Gives a local alternate version its primary's metadata, returning whether
/// any column changed — the column half of `Video.UpdateToRepositoryAsync`
/// (`Video.cs:704-726`), which every save of a primary runs for each of its
/// local versions: overview, production year, premiere date, community
/// rating, official rating and genres, taken as they are (an empty value
/// included). Its images and provider ids are copied too; those are rows of
/// their own tables, which the persistence service compares and copies.
pub fn copy_version_metadata(primary: &BaseItemEntity, version: &mut BaseItemEntity) -> bool {
    let mut changed = false;
    for (from, to) in [
        (&primary.overview, &mut version.overview),
        (&primary.official_rating, &mut version.official_rating),
        (&primary.genres, &mut version.genres),
    ] {
        if to != from {
            to.clone_from(from);
            changed = true;
        }
    }
    if version.production_year != primary.production_year {
        version.production_year = primary.production_year;
        changed = true;
    }
    if version.premiere_date != primary.premiere_date {
        version.premiere_date = primary.premiere_date;
        changed = true;
    }
    if version.community_rating != primary.community_rating {
        version.community_rating = primary.community_rating;
        changed = true;
    }
    changed
}

#[cfg(test)]
mod tests;
